"""Process ownership, environment, signalling, and exit fencing for CliSolver."""
from __future__ import annotations

import asyncio
import os
import re
import signal
from pathlib import Path
from typing import Any, Optional

from muteki.solver.cli_driver import CliResult
from muteki.solver.cli_prompts import _CTF_WORKER_SYSTEM
from muteki.solver.cli_workspace import _stable_worker_path
from muteki.solver.worker_profiles import profile_uses_endpoint
from muteki.solver.worker_skills import (
    RoleContractStage,
    instruction_file_gitignored,
    role_contract_filename,
    stage_blackboard_skill,
    stage_role_contract,
)

_SAFE_HOST_ENV = {
    "HOME", "USER", "LOGNAME", "SHELL", "TMPDIR", "LANG", "LC_ALL", "TERM",
    "SSH_AUTH_SOCK", "SSL_CERT_FILE", "SSL_CERT_DIR", "REQUESTS_CA_BUNDLE",
    "CURL_CA_BUNDLE", "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY",
    "http_proxy", "https_proxy", "all_proxy", "no_proxy",
}
_CONTROL_SECRET_ENV = {
    "MUTEKI_WEB_PASSWORD", "MUTEKI_CONTROL_TOKEN", "TSEC_AGENT_TOKEN",
    "MUTEKI_PLATFORM_TOKEN", "MUTEKI_SESSION_TOKEN",
}
_SCOPED_WORKER_SECRET_ENV = {
    "MUTEKI_CAPABILITY_TOKEN", "MUTEKI_CAPABILITY_BEARER_TOKEN",
}


def _claude_uses_bare(driver: Any) -> bool:
    """File-channel CLAUDE.md is unread when the launch argv includes --bare."""
    if str(getattr(driver, "name", "") or "").strip().lower() != "claude":
        return False
    if type(driver).__name__ == "EndpointDriver":
        return True
    profile = getattr(driver, "profile", None)
    if not isinstance(profile, dict):
        return False
    credential_kind = str(
        profile.get("credential_kind")
        or ("engine_key" if profile.get("credential_account") else "system_inherit")
    ).strip()
    return credential_kind != "system_inherit"


def _ensure_role_contract(self, cwd: Optional[str] = None) -> RoleContractStage:
    workdir = cwd or getattr(self, "_workdir", None) or ""
    engine = str(getattr(getattr(self, "driver", None), "name", "") or "")
    challenge_mode = str(getattr(self.challenge, "mode", "ctf") or "ctf")
    worker_mode = str(getattr(self, "mode", "") or "")
    cache_key = (str(workdir), engine, challenge_mode, worker_mode)
    cached = getattr(self, "_role_contract_cache", None)
    if cached is not None and getattr(self, "_role_contract_cache_key", None) == cache_key:
        return cached
    name = engine.strip().lower()
    filename = role_contract_filename(name)
    result = stage_role_contract(
        workdir,
        engine=name,
        challenge_mode=challenge_mode,
        worker_mode=worker_mode,
        bare=_claude_uses_bare(getattr(self, "driver", None)),
        gitignore_blocked=(
            name == "grok"
            and bool(workdir)
            and instruction_file_gitignored(workdir, filename)
        ),
    )
    self._role_contract_cache = result
    self._role_contract_cache_key = cache_key
    return result


def _worker_env(self, cwd: Optional[str] = None) -> dict:
    """Env vars handed to the worker subprocess.

    The initial scoped board projection is injected into the prompt. Live reads
    and every ordinary Worker mutation use the host-drained Blackboard Skill
    ingress, with no raw graph access and no assistant-output protocol.

    Ordinary solve/review roles deliberately do not receive the raw graph path:
    their role-scoped prompt plus the Skill's ``context`` operation are the
    complete model-visible context. The host validates every requested mutation
    against the Worker's assigned role.
    Framework coordination and post-solve respond are the only modes that receive
    the DB path."""
    # The host-owned invocation runner consumes this environment directly.
    # Merge driver-level model variables here so custom Claude endpoints use
    # the same model selection in both standard and host-owned execution.
    skill_workdir = cwd or self._workdir
    if skill_workdir:
        stage_blackboard_skill(
            skill_workdir,
            engine=self.driver.name,
            container=self.container is not None,
            allow_operator_input=bool(
                getattr(self.challenge, "allow_operator_input", True)
            ),
        )
        self._ensure_role_contract(skill_workdir)

    env_extra = getattr(self.driver, "env_extra", None)
    driver_env = env_extra() if callable(env_extra) else {}
    env = {
        key: value for key, value in os.environ.items()
        if key in _SAFE_HOST_ENV
    }
    env.update(driver_env)
    env.update(self._extra_worker_env)
    for secret_name in _CONTROL_SECRET_ENV:
        env.pop(secret_name, None)
    for name in list(env):
        if (name not in _SCOPED_WORKER_SECRET_ENV
                and name.startswith(("MUTEKI_", "TSEC_"))
                and re.search(r"(?:TOKEN|PASSWORD|SECRET|API_KEY|COOKIE|AUTH)", name)):
            env.pop(name, None)
    # These capabilities are host-owned.  The subprocess runner overlays this
    # mapping on os.environ, so removing a driver/profile value is insufficient:
    # a same-named host variable would otherwise reappear in a local Worker.
    # Empty values are deliberate revocations and also override image ENV for
    # container Workers.  The role checks below grant back only the capabilities
    # assigned to this Worker.
    for controlled_name in (
        "MUTEKI_BLACKBOARD_DB",
        "MUTEKI_BLACKBOARD_INGRESS_DIR", "MUTEKI_BLACKBOARD_PROFILE",
        "MUTEKI_BLACKBOARD_ROLE",
    ):
        env[controlled_name] = ""
    if not bool(getattr(self.challenge, "allow_operator_input", True)):
        env["MUTEKI_BLACKBOARD_PROFILE"] = "autonomous"
    state_root = Path(cwd or self._workdir).resolve() if (cwd or self._workdir) else None
    if state_root is not None:
        temp_host = state_root / ".tmp"
        temp_host.mkdir(parents=True, exist_ok=True)
        if self.container is not None:
            from muteki.solver.container_exec import _chown_tree_to_worker

            _chown_tree_to_worker(str(temp_host), image=self.container.image)
        temp_path = str(temp_host)
        mapper = getattr(self.container, "to_container_path", None)
        if callable(mapper):
            try:
                temp_path = mapper(temp_path)
            except Exception:
                pass
        env["TMPDIR"] = temp_path
        env["TMP"] = temp_path
        env["TEMP"] = temp_path
    if self.driver.name == "opencode" and state_root is not None:
        opencode_root = state_root / ".muteki-opencode"
        for dirname in ("data", "config", "cache"):
            (opencode_root / dirname).mkdir(parents=True, exist_ok=True)
        runtime_root = str(opencode_root)
        mapper = getattr(self.container, "to_container_path", None)
        if callable(mapper):
            runtime_root = mapper(runtime_root)
        env.setdefault("XDG_DATA_HOME", f"{runtime_root}/data")
        env.setdefault("XDG_CONFIG_HOME", f"{runtime_root}/config")
        env.setdefault("XDG_CACHE_HOME", f"{runtime_root}/cache")
    profile = getattr(self.driver, "profile", None)
    if (
        self.driver.name == "claude"
        and isinstance(profile, dict)
        and profile_uses_endpoint(profile)
    ):
        # Claude Code loads ~/.claude/settings.json after process launch. A
        # host-level ANTHROPIC_BASE_URL / ANTHROPIC_MODEL there can replace
        # the selected Worker's custom endpoint even though the subprocess
        # received the correct environment. Keep endpoint-backed Workers in a
        # per-worker config directory; subscription Workers continue to use
        # the operator's normal Claude configuration.
        config_root = cwd or self._workdir
        if config_root:
            config_host = Path(config_root).resolve() / ".muteki-claude-config"
            config_host.mkdir(parents=True, exist_ok=True)
            if self.container is not None:
                # Claude creates `session-env/` below CLAUDE_CONFIG_DIR before
                # its Bash/Write tools run.  This directory is also created
                # after the run-level workspace chown, so make the late path
                # writable by the container worker as well.
                from muteki.solver.container_exec import _chown_tree_to_worker

                _chown_tree_to_worker(
                    str(config_host), image=self.container.image
                )
            config_path = str(config_host)
            mapper = getattr(self.container, "to_container_path", None)
            if callable(mapper):
                try:
                    config_path = mapper(config_path)
                except Exception:
                    pass
            env["CLAUDE_CONFIG_DIR"] = config_path
    env["PATH"] = _stable_worker_path(env.get("PATH") or os.environ.get("PATH", ""))
    env["MUTEKI_WORKER_ID"] = self.solver_id
    env["MUTEKI_CHALLENGE_MODE"] = str(
        getattr(self.challenge, "mode", "ctf") or "ctf"
    )
    if (
        self.driver.name == "pi"
        and env["MUTEKI_CHALLENGE_MODE"] == "ctf"
        and self.mode == "explore"
    ):
        env["MUTEKI_PI_SYSTEM_PROMPT"] = _CTF_WORKER_SYSTEM
    if (
        self.driver.name == "omp"
        and env["MUTEKI_CHALLENGE_MODE"] == "ctf"
        and self.mode == "explore"
    ):
        env["MUTEKI_OMP_SYSTEM_PROMPT"] = _CTF_WORKER_SYSTEM
    env["MUTEKI_BLACKBOARD_ROLE"] = (
        "review" if self.mode == "review"
        else "verifier" if self.mode == "fact_verifier"
        else "reproducer" if self.mode == "report_reproducer"
        else "solve" if (
            self.mode == "respond"
            and str(self.hitl_cmd.get("action") or "") == "mark_false"
        )
        else "respond" if self.mode == "respond"
        else "solve"
    )
    intent_id = getattr(self, "intent_id_assigned", "") or getattr(self, "_intent_id", "") or ""
    if intent_id:
        env["MUTEKI_INTENT_ID"] = intent_id
    env["MUTEKI_TARGET_EPOCH"] = str(
        getattr(self, "_target_epoch", "") or "1")
    env["MUTEKI_TOOLBOX_DIR"] = "./toolbox"
    # Explicit empty values override the host environment even when a local or
    # container runner merges its own environment beneath this mapping.
    for proxy_name in (
        "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY",
        "http_proxy", "https_proxy", "all_proxy",
    ):
        env[proxy_name] = ""
    env["NO_PROXY"] = "*"
    env["no_proxy"] = "*"
    env["MUTEKI_BLACKBOARD_SCRIPT"] = self._blackboard_script_path()
    if cwd and (self.mode != "respond"
                or str(self.hitl_cmd.get("action") or "") == "mark_false"):
        mapper = getattr(self.container, "to_container_path", None)
        ingress_host = Path(cwd).resolve() / ".muteki-blackboard-ingress"
        ingress_host.mkdir(parents=True, exist_ok=True)
        if self.container is not None:
            from muteki.solver.container_exec import _chown_tree_to_worker

            _chown_tree_to_worker(
                str(ingress_host), image=self.container.image
            )
        self._blackboard_ingress_dir = ingress_host
        ingress_path = str(ingress_host)
        if callable(mapper):
            try:
                ingress_path = mapper(ingress_path)
            except Exception:
                pass
        env["MUTEKI_BLACKBOARD_INGRESS_DIR"] = ingress_path
    # Workers receive role-scoped context and host-drained mutation ingress.
    # Only post-solve respond receives read-only raw graph access.
    db = getattr(self.shared_graph, "db_path", None)
    if db and self.mode == "respond":
        # ABSOLUTE path: the worker subprocess runs with cwd=<its own workdir>,
        # so a relative db_path would resolve against the wrong dir and the
        # blackboard skill / raw sqlite would hit "unable to open database file"
        # (observed run-7352). abspath resolves against OUR cwd, which is correct.
        db_path = os.path.abspath(str(db))
        mapper = getattr(self.container, "to_container_path", None)
        if callable(mapper):
            try:
                db_path = mapper(db_path)
            except Exception:
                pass
        env["MUTEKI_BLACKBOARD_DB"] = db_path
    return env


@staticmethod
def _signal_proc(proc: Any, sig: int) -> bool:
    """Send `sig` to a worker's whole PROCESS GROUP (the CLI agent spawns
    curl/python/sh helpers; signalling only the parent leaves them running —
    the deeper form of bug #2). The subprocess is a group leader because the
    runner starts it with start_new_session=True. Falls back to the bare pid /
    proc.kill() when the group send isn't available (e.g. test fakes)."""
    # Container backend: the worker runs via `docker exec`, so its real pgid is
    # INSIDE the container — a host-side os.killpg on the docker-exec client pid
    # would not reach it. A _ContainerProc exposes _container_signal to route the
    # signal in via `docker exec kill`. Prefer it when present.
    cont_sig = getattr(proc, "_container_signal", None)
    if callable(cont_sig):
        try:
            # RCP returns an explicit delivery/verification bool. Legacy
            # container wrappers predate that contract and return None after a
            # successful synchronous docker call, which remains accepted.
            return cont_sig(sig) is not False
        except Exception:
            return False
    local_owner = getattr(proc, "_muteki_process_owner", None)
    owner_signal = getattr(local_owner, "signal", None)
    if callable(owner_signal):
        try:
            return owner_signal(sig) is not False
        except Exception:
            return False
    pid = getattr(proc, "pid", None)
    if pid is not None:
        try:
            os.killpg(os.getpgid(pid), sig)
            return True
        except Exception:
            pass
        try:
            os.kill(pid, sig)
            return True
        except Exception:
            pass
    # last resort for SIGKILL: Popen.kill() (covers test fakes with no pid)
    if sig == getattr(signal, "SIGKILL", -9):
        try:
            proc.kill()
            return True
        except Exception:
            pass
    return False

@staticmethod
def _proc_is_alive(proc: Any) -> bool:
    """Conservatively decide whether a registered child may still be alive.

    Real ``Popen`` and container handles expose ``poll``.  Unknown/fake handles
    are deliberately retained: dropping an unprovably-dead handle is precisely
    how a cancelled ``asyncio.to_thread`` runner loses its last kill boundary.
    """
    local_owner = getattr(proc, "_muteki_process_owner", None)
    owner_has_live = getattr(local_owner, "has_live_processes", None)
    if callable(owner_has_live):
        try:
            if owner_has_live():
                return True
        except Exception:
            return True
    if hasattr(proc, "_exit_confirmed"):
        # RCP logical handles have a dedicated wire-level fence. A returned or
        # failed transport wrapper is not equivalent to the supervisor's exit
        # frame and must never prune an unknown remote owner.
        return not bool(getattr(proc, "_exit_confirmed", False))
    poll = getattr(proc, "poll", None)
    # The legacy docker-exec wrapper exposes its host Popen as _client_proc.
    # Prefer that real poll fence rather than treating the wrapper as immortal.
    if not callable(poll):
        poll = getattr(getattr(proc, "_client_proc", None), "poll", None)
    if not callable(poll):
        # Unknown logical handles without either poll, a dedicated RCP fence,
        # or this legacy transport-return proof remain conservatively live.
        return not bool(getattr(proc, "_muteki_runner_exited", False))
    try:
        return poll() is None
    except Exception:
        return True


def _prune_finished_procs(self) -> None:
    with self._procs_lock:
        self._live_procs = {
            proc for proc in self._live_procs if self._proc_is_alive(proc)
        }


def runtime_exit_confirmed(self) -> bool:
    """Query the hard runtime termination fence.

    True means every to_thread runner has returned AND every registered process
    handle is provably exited (``poll() is not None`` or the equivalent RCP
    transport-return proof). It deliberately remains False when only the outer
    asyncio worker task has finished.
    """
    self._prune_finished_procs()
    self._runner_tasks = {
        task for task in self._runner_tasks if not task.done()
    }
    with self._procs_lock:
        has_live_process = bool(self._live_procs)
    return not self._runner_tasks and not has_live_process


async def wait_runtime_exit(self, timeout: Optional[float] = None) -> bool:
    """Await runtime termination without cancelling runner Tasks.

    ``timeout=None`` waits until proof exists. A finite timeout returns False;
    it never turns a kill request or an already-done wrapper into proof.
    Multiple waiters (driver cleanup plus RunManager receipt fencing) are safe.
    """
    loop = asyncio.get_running_loop()
    deadline = None if timeout is None else loop.time() + max(0.0, float(timeout))
    while True:
        if self.runtime_exit_confirmed():
            return True
        if deadline is not None:
            remaining = deadline - loop.time()
            if remaining <= 0:
                return False
        else:
            remaining = 0.05

        pending = tuple(
            task for task in self._runner_tasks if not task.done()
        )
        wait_for = min(0.05, remaining) if deadline is not None else 0.05
        if pending:
            # asyncio.wait observes completion without propagating this waiter's
            # cancellation into the shielded runner tasks.
            await asyncio.wait(pending, timeout=wait_for)
        else:
            # A process may expose its terminal poll code shortly after runner
            # return; keep polling conservatively until the caller's deadline.
            await asyncio.sleep(wait_for)


@staticmethod
def _thread_cancel_cleanup_timeout() -> float:
    try:
        return max(0.01, float(os.environ.get(
            "MUTEKI_CLI_CANCEL_CLEANUP_TIMEOUT", "2")))
    except (TypeError, ValueError):
        return 2.0


async def _to_thread_with_cancel_cleanup(
    self, func: Any, /, *args: Any, **kwargs: Any
) -> Any:
    """Run a blocking CLI call without orphaning it on asyncio cancellation.

    ``asyncio.to_thread`` keeps running after its awaiting task is cancelled.
    Shield the runner task, signal the real subprocess tree via ``cancel()``,
    then give the runner a bounded window to reap.  If that window expires the
    thread may still finish later, so live process handles remain registered and
    the done callback prunes them only after ``poll`` proves they exited.
    """
    runner_procs: set[Any] = set()

    def _blocking_call() -> Any:
        self._runner_proc_local.procs = runner_procs
        try:
            return func(*args, **kwargs)
        finally:
            # RCP logical handles do not expose poll(); transport return is their
            # own explicit exit-frame proof. Unknown legacy logical handles use
            # transport return, while RCP handles must never be laundered by it.
            # Handles with a real poll still require the poll result.
            for proc in tuple(runner_procs):
                if hasattr(proc, "_exit_confirmed"):
                    continue
                try:
                    setattr(proc, "_muteki_runner_exited", True)
                except Exception:
                    pass
            try:
                del self._runner_proc_local.procs
            except AttributeError:
                pass

    self._stream_cost_flushed = False
    self._stream_tool_starts = 0
    self._stream_reasoning_chars = 0
    runner_task = asyncio.create_task(asyncio.to_thread(_blocking_call))
    self._runner_tasks.add(runner_task)

    def _runner_done(done: "asyncio.Task[Any]") -> None:
        # Retrieve a late exception when the outer coroutine already left after
        # its bounded cancellation window; otherwise asyncio logs an unrelated
        # "Task exception was never retrieved" warning during shutdown.
        try:
            done.exception()
        except (asyncio.CancelledError, Exception):
            pass
        self._prune_finished_procs()
        self._runner_tasks.discard(done)

    runner_task.add_done_callback(_runner_done)
    try:
        result = await asyncio.shield(runner_task)
    except asyncio.CancelledError:
        # The outer task was cancelled, but shield kept the blocking runner
        # alive.  Kill its real child first, then wait briefly for thread/proc
        # teardown before propagating cancellation to the caller.
        self.cancel()
        salvaged: Any = None
        try:
            salvaged = await asyncio.wait_for(
                asyncio.shield(runner_task),
                timeout=self._thread_cancel_cleanup_timeout(),
            )
        except asyncio.TimeoutError:
            # A second signal is idempotent and catches a process that registered
            # just as the first snapshot was taken.  Do NOT clear its handle.
            self.cancel()
        except asyncio.CancelledError:
            # A second cancellation of this coroutine must still re-signal the
            # child; the runner remains shielded and its handle remains retained.
            self.cancel()
        except Exception:
            # The blocking runner failed while unwinding.  The original asyncio
            # cancellation remains the externally meaningful terminal cause.
            pass
        finally:
            self._prune_finished_procs()
        # NYU-AB metering gap: asyncio cancel used to discard a completed
        # CliResult, so COST_UPDATE never fired despite real tool activity.
        # Salvage tokens/cost before re-raising so receipts stay measurable.
        # If the runner outlives the cleanup window, price a floor from live
        # stream activity so zero-dollar cells cannot erase a real run.
        if salvaged is None and runner_task.done() and not runner_task.cancelled():
            try:
                salvaged = runner_task.result()
            except Exception:
                salvaged = None
        if isinstance(salvaged, CliResult):
            try:
                await self._stream_cost(salvaged)
            except Exception:
                pass
        else:
            try:
                await self._flush_stream_activity_cost()
            except Exception:
                pass
        raise
    finally:
        # Normal completion/exception may leave a short-lived dead Popen handle.
        # Poll before removal; never use an unconditional clear here.
        self._prune_finished_procs()
    return result
