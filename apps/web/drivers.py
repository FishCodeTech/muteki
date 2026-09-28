"""Run drivers — turn a /start request body into a coroutine that emits onto the
run's bus. Keeps the HTTP layer (server.py) ignorant of solving internals.

Kinds:
  - "swarm" (DEFAULT): races the REAL solver swarm (shelled claude+codex CLI
    executor) against a challenge spec. Needs a live target (URL in the prompt /
    challenge.target) and the claude (and optionally codex) CLI on PATH — no
    DeepSeek key (the CLI executor doesn't use the code-driven kernel).
"""

from __future__ import annotations

import asyncio
import copy
import hashlib
import json
import re
import os
import sys
from pathlib import Path
from typing import TYPE_CHECKING, Any, Awaitable, Callable

from apps.web.dispatch_parse import explicit_category
from apps.web.llm_credentials import resolve_llm_profile_credential
from apps.web.run_manager import Run, RunManager
from apps.web.worker_config import (
    backend_for_profile,
    resolve_worker_backend,
)
from muteki.solver.credential_accounts import account_store_root
from muteki.core.runtime_env import is_web_container
from muteki.solver.worker_profiles import (
    apply_worker_identity_env,
    base_engine_for_profile,
    normalize_profile_roster,
    profile_uses_endpoint,
    worker_identity_fields,
)
from muteki.solver.cli_driver import driver_for
from muteki.core.llm import LLMClient, llm_temperature_kwargs
from muteki.solver.gate import (
    DEFAULT_BRACE_FLAG_FORMAT,
    normalize_flag_contract,
    normalize_flag_wrapper,
)

if TYPE_CHECKING:
    from muteki.solver.profile_health import ProfileHealth

Driver = Callable[[Run], Awaitable[None]]



def _format_missing(p: dict, h: "ProfileHealth") -> str:
    """Reconstruct the historical `missing` string from a kernel verdict so any
    log/operator reading it sees the same tokens as before the unification:
      - binding failure → `<id>:<account_id or '<missing>'>`
      - endpoint-profile probe failure → `<name>:endpoint:<detail>`
      - other probe failure → `<name>:probe:<detail>`
    The string is display-only (never parsed), but its readability is the point.
    """
    if h.layer == "binding":
        account_id = str(p.get("credential_account") or "")
        return f"{p.get('id') or p.get('engine')}:{account_id or '<missing>'}"
    probe = "endpoint" if profile_uses_endpoint(p) else "probe"
    name = p.get("name") or p.get("id") or p.get("engine")
    return f"{name}:{probe}:{h.detail or 'unhealthy'}"


def _missing_profile_accounts(
    *,
    worker_profiles: list[dict],
    worker_backend: str,
    sessions_root: Path,
) -> list[str]:
    """Dispatch precheck — now a thin wrapper over the profile_health kernel so it
    can never disagree with the settings self-check. The kernel does cheap binding
    inline, then every enabled profile executes a real CLI hello; probes fan out so
    the dispatch path pays max(timeout), not sum(timeout)."""
    from concurrent.futures import ThreadPoolExecutor

    from muteki.solver.profile_health import evaluate_profile_health

    enabled = [
        p for p in worker_profiles if isinstance(p, dict) and p.get("enabled", True)
    ]
    if not enabled:
        return []

    def _ev(p: dict) -> "tuple[dict, ProfileHealth]":
        backend = backend_for_profile(
            worker_backend=worker_backend,
            in_web_container=is_web_container(),
        )
        return p, evaluate_profile_health(
            p, backend=backend, sessions_root=sessions_root, depth="auth"
        )

    if len(enabled) == 1:
        verdicts = [_ev(enabled[0])]
    else:
        with ThreadPoolExecutor(max_workers=len(enabled)) as pool:
            verdicts = list(pool.map(_ev, enabled))
    return [_format_missing(p, h) for p, h in verdicts if not h.ok]


def _selected_profiles(engines: list[str], worker_profiles: list[dict]) -> list[dict]:
    names = normalize_profile_roster(engines, worker_profiles)
    by_name = {str(p.get("name") or p.get("id")): p for p in worker_profiles if isinstance(p, dict)}
    return [by_name[n] for n in names if n in by_name]


def _startup_profiles(
    *,
    engines: list[str],
    race_engines: list[str] | None,
    worker_profiles: list[dict],
    stage_policy: dict[str, Any],
    coordinator: bool,
) -> tuple[list[dict], list[str]]:
    """Return the task roster that Swarm can actually dispatch.

    ``Swarm.engines`` is built from the main roster. Race and review settings can
    only select a subset of that roster; references outside it are filtered by
    the scheduler and therefore must not spend a readiness request or block the
    task. Keeping those parameters explicit documents that this matches the
    current call chain rather than the shape of the settings document.
    """
    del race_engines, stage_policy, coordinator
    refs = list(engines)
    selected = _selected_profiles(refs, worker_profiles)
    unknown_refs = [] if not worker_profiles else [
        str(ref)
        for ref in refs
        if str(ref).strip()
        and not normalize_profile_roster([str(ref)], worker_profiles)
    ]
    out: list[dict] = []
    seen: set[str] = set()
    for profile in selected:
        profile_id = str(
            profile.get("id") or profile.get("name") or profile.get("engine") or ""
        )
        if profile_id and profile_id not in seen and profile.get("enabled", True):
            seen.add(profile_id)
            out.append(profile)
    return out, list(dict.fromkeys(unknown_refs))


def _safe_preflight_detail(value: Any, *, layer: str) -> str:
    """Preserve the actionable probe error."""
    text = str(value or "预检失败").replace("\x00", "").strip()
    text = "".join(ch for ch in text if ch in "\n\t" or ord(ch) >= 32)
    return text[:2000] or f"{layer or 'model'} preflight failed"


def _preflight_error_id(*parts: Any) -> str:
    material = "\x1f".join(str(part or "") for part in parts)
    digest = hashlib.sha256(material.encode("utf-8", "replace")).hexdigest()
    return f"PF-{digest[:10].upper()}"


def _profile_readiness_key(
    profile: dict,
    *,
    runtime: dict,
    backend: str,
) -> str:
    """Identify the exact runnable profile configuration for task-level reuse."""
    material = {
        # v2 只接受真实 CLI model probe；使旧版由 structured Runtime 健康
        # 状态写入的绿色缓存立即失效。
        "readiness_contract": "worker-cli-v2",
        "execution_transport": "cli",
        "profile_id": str(
            profile.get("id") or profile.get("name") or profile.get("engine") or ""
        ),
        "engine": base_engine_for_profile(profile),
        "model": str(profile.get("model") or ""),
        "reasoning_effort": str(profile.get("reasoning_effort") or "default"),
        "credential_account": str(profile.get("credential_account") or ""),
        "backend": backend,
        "runtime": runtime,
    }
    encoded = json.dumps(
        material, ensure_ascii=False, sort_keys=True, separators=(",", ":"),
        default=str,
    )
    return hashlib.sha256(encoded.encode("utf-8", "replace")).hexdigest()


async def _startup_readiness(
    *,
    profiles: list[dict],
    worker_network: str,
    worker_backend: str,
    sessions_root: Path,
    cached_results: dict[str, tuple[bool, dict[str, Any] | None]],
    effective_network: str = "",
) -> tuple[dict[str, bool], list[dict[str, Any]]]:
    """Send one real CLI model request for every participating Worker profile."""
    from apps.web.worker_models import ProbeProcessOwner, probe_worker_model
    from muteki.solver.profile_health import evaluate_profile_health

    runtime = {"network": worker_network}
    owner = ProbeProcessOwner()

    async def _probe(profile: dict) -> tuple[str, bool, dict[str, Any] | None]:
        profile_id = str(
            profile.get("id") or profile.get("name") or profile.get("engine")
        )
        backend = backend_for_profile(
            worker_backend=worker_backend,
            in_web_container=is_web_container(),
        )
        cache_key = _profile_readiness_key(
            profile, runtime=runtime, backend=backend)
        cached = cached_results.get(cache_key)
        if cached is not None:
            cached_ok, cached_failure = cached
            return profile_id, cached_ok, copy.deepcopy(cached_failure)
        binding = await asyncio.to_thread(
            evaluate_profile_health,
            profile,
            backend=backend,
            sessions_root=sessions_root,
            depth="binding",
        )
        if not binding.ok:
            result: dict[str, Any] = {
                "ok": False,
                "layer": binding.layer or "binding",
                "detail": binding.detail or binding.blocker or "凭据未配置",
            }
        else:
            result = await asyncio.to_thread(
                probe_worker_model,
                profile=profile,
                model=str(profile.get("model") or ""),
                reasoning_effort=str(
                    profile.get("reasoning_effort") or "default"),
                sessions_root=sessions_root,
                backend=backend,
                runtime=runtime,
                owner=owner,
            )
        if result.get("ok"):
            cached_results[cache_key] = (True, None)
            return profile_id, True, None
        layer = str(result.get("layer") or "model")
        detail = _safe_preflight_detail(result.get("detail"), layer=layer)
        code = f"preflight_{layer}_failed"
        failure = {
            "error_id": _preflight_error_id(
                profile_id, base_engine_for_profile(profile), layer, code, detail),
            "profile_id": profile_id,
            "engine": base_engine_for_profile(profile),
            "model": str(profile.get("model") or ""),
            "backend": backend,
            "network": worker_network if backend == "container" else "",
            "effective_network": (
                (effective_network or worker_network) if backend == "container" else ""
            ),
            "stage": "preflight",
            "layer": layer,
            "code": code,
            "detail": detail,
        }
        cached_results[cache_key] = (False, copy.deepcopy(failure))
        return profile_id, False, failure

    tasks = [asyncio.create_task(_probe(profile)) for profile in profiles]
    try:
        rows = await asyncio.gather(*tasks)
    except asyncio.CancelledError:
        await asyncio.to_thread(owner.cancel)
        await asyncio.to_thread(owner.wait, 15.0)
        await asyncio.gather(*tasks, return_exceptions=True)
        raise
    snapshot = {profile_id: ok for profile_id, ok, _failure in rows}
    failures = [failure for _profile_id, _ok, failure in rows if failure is not None]
    return snapshot, failures


def _resolve_swarm_class(spec: Any) -> type:
    """Accept only the shipped Coordinator, including legacy explicit specs."""
    from muteki.swarm.swarm import Swarm

    text = str(spec or "").strip()
    if text in ("", "muteki.swarm.swarm:Swarm"):
        return Swarm
    raise RuntimeError(f"swarm_class {text!r} is unavailable; use the standard Swarm")


def build_driver(body: dict[str, Any], mgr: RunManager | None = None) -> Driver:
    # Only shipped product drivers are accepted.
    body = _normalize_start_flag_contract(body)
    kind = body.get("kind", "swarm")
    if kind == "idle":
        return _idle_driver(body)
    if kind != "swarm":
        raise ValueError(f"unsupported run kind: {kind}")
    return _swarm_driver(_infer_challenge(body), mgr=mgr)


_DEFAULT_BRACE_FLAG_FORMAT = DEFAULT_BRACE_FLAG_FORMAT


def _clean_flag_wrapper(raw: Any) -> str:
    return normalize_flag_wrapper(raw)


def _flag_format_fields(ch: dict[str, Any], body: dict[str, Any]) -> tuple[str, str, str]:
    raw_format = (
        ch.get("flag_format")
        or ch.get("flagFormat")
        or body.get("flag_format")
        or body.get("flagFormat")
        or ""
    )
    wrapper = (
        ch.get("flag_format_wrapper")
        or ch.get("flagWrapper")
        or body.get("flag_format_wrapper")
        or body.get("flagWrapper")
        or ""
    )
    hint = str(ch.get("flag_format_hint") or ch.get("flagFormatHint") or "").strip()
    cleaned_wrapper = _clean_flag_wrapper(wrapper)
    # The helper owns the selector/wrapper precedence for every start path:
    # custom-or-empty format may use the wrapper as a contract, while an explicit
    # regex or token mode remains authoritative.
    contract = normalize_flag_contract(raw_format, cleaned_wrapper)
    flag_format = contract.flag_format
    cleaned_wrapper = contract.flag_format_wrapper
    if flag_format == "token":
        return flag_format, hint, ""

    if cleaned_wrapper:
        return flag_format, cleaned_wrapper, cleaned_wrapper

    return flag_format, hint, ""


def _normalize_start_flag_contract(body: dict[str, Any] | None) -> dict[str, Any]:
    """Copy and normalize the start payload's flag contract synchronously.

    ``build_driver`` is shared by HTTP, Command API and RunGateway starts.  Doing
    this before a Driver coroutine is returned makes an invalid regex an admission
    error, rather than a delayed runtime failure after Workers have started.
    """
    normalized = dict(body or {})
    challenge = dict(normalized.get("challenge") or {})
    flag_format, hint, wrapper = _flag_format_fields(challenge, normalized)
    challenge["flag_format"] = flag_format
    if hint:
        challenge["flag_format_hint"] = hint
    if wrapper:
        challenge["flag_format_wrapper"] = wrapper
    normalized["challenge"] = challenge
    return normalized


def _infer_challenge(body: dict[str, Any]) -> dict[str, Any]:
    """Copy the conversational prompt onto ``challenge.description`` when missing.

    Title and category are not guessed here. The left rail gets them from a
    later Planner label; Swarm keeps the original instruction.
    """
    body = dict(body or {})
    ch = dict(body.get("challenge") or {})
    roster_category = explicit_category(ch.get("category"))
    if roster_category:
        ch["category"] = roster_category
    else:
        ch.pop("category", None)
    prompt = (body.get("prompt") or ch.get("description") or "").strip()
    if prompt and not ch.get("description"):
        ch["description"] = prompt
    if (body.get("mode") or ch.get("mode")) == "pentest":
        ch["mode"] = "pentest"
    body["challenge"] = ch
    return body


def _public_challenge_payload(
    challenge: Any, *, operator_name: str, roster_category: str,
) -> dict[str, Any]:
    """Drop default name/category so the rail stays empty until RUN_TITLED."""
    payload = challenge.model_dump(mode="json")
    if not operator_name:
        payload.pop("name", None)
    if not roster_category:
        payload.pop("category", None)
    return payload


def _idle_driver(body: dict[str, Any]) -> Driver:
    """Keeps a run's bus open without solving — used to drive HITL/manual flows
    (and as a smoke target). Stays alive until cancelled."""
    async def drive(run: Run) -> None:
        import asyncio

        while True:
            await asyncio.sleep(3600)

    return drive


async def _open_planner_llm(
    *,
    llm_profiles: dict[str, Any],
    run: Run,
    mgr: RunManager | None,
) -> tuple[Any, Any]:
    """Open the configured Ark client for a Pentest's final report."""
    planner_profile = llm_profiles.get("planner") or {}
    llm_kwargs: dict[str, Any] = {
        "cost": run.cost,
        "bus": run.bus,
        **llm_temperature_kwargs(planner_profile),
    }
    if mgr is not None:
        credential = resolve_llm_profile_credential(
            "planner", planner_profile, sessions_root=mgr.state_root)
        if credential.base_url:
            llm_kwargs["base_url"] = credential.base_url
        if credential.api_key:
            llm_kwargs["api_key"] = credential.api_key
    elif str(planner_profile.get("base_url") or "").strip():
        llm_kwargs["base_url"] = str(planner_profile["base_url"]).strip()
    llm_cm = LLMClient(**llm_kwargs)
    if not llm_cm.api_key:
        raise ValueError("planner API key is missing")
    llm = await llm_cm.__aenter__()
    llm.usage_generation = run.execution_generation
    return llm_cm, llm


def _swarm_driver(body: dict[str, Any], mgr: RunManager | None = None) -> Driver:
    """The REAL solver: a shelled-CLI swarm (claude + codex race) against the
    challenge. CliSolver runs subscription CLIs directly; a Flag is recorded only
    when the model calls the Blackboard Skill's submit command.

    Knobs from the request body (all optional):
      challenge.{name,category,target,description,flag_format}
      cli_race: bool (default True)           — race claude + codex
      cli_engine: "claude" | "codex"          — single engine when not racing
      race_scout: bool (default True)         — one parallel single-shot recon round
                                                in front of the main coordinator loop
                                                (fast path on flag, else hands facts
                                                to the coordinator loop)
      race_engines: list (default = engines)  — which engines race (worker switch)
      race_timeout: int (default 300)         — short per-worker recon timeout (s)
      offline: bool (default False)           — deny worker web tools (clean eval);
                                                also denies the KB unless `kb` is set.
                                                Does NOT force Docker --network none
                                                (#171 / MNT-09.04); egress is separate.
      kb: bool (default: True online / False offline) — let the worker query the KB
      n_solvers: int (default 2)              — bootstrap lineup size
      engines: list[str] (default [cursor,claude,codex]) — engine roster; offline
                                                drops cursor (can't go offline cleanly)
      start_workers: int (default len(engines)) — bootstrap workers (one per engine)
      swarm_class: omitted or muteki.swarm.swarm:Swarm — standard Coordinator
    """
    async def drive(run: Run) -> None:
        import os
        import tempfile
        from pathlib import Path

        from muteki.models.solve_graph import (
            Challenge, TaskContract,
        )
        from muteki.sandbox.manager import SandboxManager
        from muteki.solver.result import ArtifactStore
        from muteki.solver.types import SolverConfig
        from muteki.swarm.models import default_lineup

        # Legacy explicit standard spec is accepted; research implementations
        # are no longer shipped.
        swarm_cls = _resolve_swarm_class(body.get("swarm_class"))

        ch = dict(body.get("challenge") or {})
        task_contract = None
        contract_payload = body.get("task_contract") or ch.get("task_contract")
        if isinstance(contract_payload, dict):
            task_contract = TaskContract.model_validate(contract_payload)
        # attachments: local file paths for FILE-based tracks (crypto/rev/forensics
        # /misc). The worker stages them into its cwd. Keep only paths that exist so
        # a stray entry can't crash the run.
        attachments = [a for a in (ch.get("attachments") or []) if Path(a).exists()]
        # engagement mode: "ctf" (default, flag-driven) or "pentest" (goal-driven —
        # find + prove vulnerabilities in scope). Body may carry it at top level or
        # under challenge.* ; default keeps every CTF dispatch byte-identical.
        mode = (ch.get("mode") or body.get("mode") or "ctf")
        if mode not in ("ctf", "pentest"):
            raise ValueError(f"unknown dispatch mode {mode!r}")
        prompt_text = (body.get("prompt") or ch.get("description") or "").strip()
        goal_text = (ch.get("goal") or body.get("goal") or "")
        if mode == "pentest" and not str(goal_text).strip():
            goal_text = prompt_text
        scope_text = (ch.get("scope") or body.get("scope") or "")
        coordinator = bool(body.get("coordinator", True))
        llm_profiles = dict(body.get("llm_profiles") or {})
        if not llm_profiles and mgr is not None:
            llm_profiles = dict(
                mgr.worker_config.get().get("llm_profiles") or {})
        llm_cm = None
        llm = None
        if task_contract is not None:
            completion = task_contract.completion_contract
            ch["description"] = task_contract.raw_instruction
            if task_contract.title:
                ch["name"] = task_contract.title
            else:
                ch.pop("name", None)
            if task_contract.category:
                ch["category"] = task_contract.category
            else:
                ch.pop("category", None)
            if task_contract.execution_target:
                ch["target"] = task_contract.execution_target
            else:
                ch.pop("target", None)
            ch["scope"] = task_contract.authorization_scope
            mode = task_contract.mode
            goal_text = completion.goal if mode == "pentest" else ""
            scope_text = task_contract.authorization_scope
            attachments = [item.path for item in task_contract.attachments]
        # Pentest shares the current Pi Decide execution kernel with CTF.
        # The retired HTTP/JSON Reason planner is never opened for this mode.
        pentest_contract = task_contract.pentest_contract if task_contract else None
        if mode == "pentest" and pentest_contract is None:
            from muteki.pentest.contract import compile_pentest_prompt
            pentest_contract = compile_pentest_prompt(
                prompt_text, target=str(ch.get("target") or ""), scope=scope_text,
            )
        if mode == "pentest":
            llm_cm, llm = await _open_planner_llm(
                llm_profiles=llm_profiles, run=run, mgr=mgr,
            )
        expected_flags = int(body.get("expected_flags")
                             or ch.get("expected_flags") or 1)
        multi_flag = bool(body.get("multi_flag")
                          if body.get("multi_flag") is not None
                          else ch.get("multi_flag", False))
        flag_format, flag_format_hint, flag_format_wrapper = _flag_format_fields(ch, body)
        operator_name = str(ch.get("name") or "").strip()
        roster_category = explicit_category(ch.get("category"))
        challenge = Challenge(
            id=run.run_id,
            name=operator_name or run.run_id,
            category=roster_category or "misc",
            points=ch.get("points", 0),
            description=ch.get("description", ""),
            target=ch.get("target"),
            attachments=attachments,
            flag_format=flag_format,
            flag_format_hint=flag_format_hint,
            flag_format_wrapper=flag_format_wrapper,
            expected_flags=max(1, expected_flags),
            initial_flags=list(
                body.get("initial_flags") or ch.get("initial_flags") or []),
            platform_confirmation_required=bool(
                body.get("platform_confirmation_required")
                if body.get("platform_confirmation_required") is not None
                else ch.get("platform_confirmation_required", False)
            ),
            multi_flag=multi_flag,
            allow_operator_input=bool(
                body.get("allow_operator_input")
                if body.get("allow_operator_input") is not None
                else ch.get("allow_operator_input", True)
            ),
            verifier_rate_limited=bool(body.get("verifier_rate_limited")
                                       if body.get("verifier_rate_limited") is not None
                                       else ch.get("verifier_rate_limited", False)),
            mode=mode,
            goal=goal_text,
            scope=scope_text,
            task_contract=task_contract,
            pentest_contract=pentest_contract,
        )
        executor = body.get("executor", "cli")
        cli_race = bool(body.get("cli_race", False))
        cli_engine = body.get("cli_engine", "claude")
        offline = bool(body.get("offline", False))
        web_access = not offline
        # offline implies NO KB (a clean black-box eval denies every external
        # dependency, KB included) — but `kb` can still be set explicitly to
        # override either way. Default KB on only when online.
        kb = bool(body.get("kb", not offline))
        n = int(body.get("n_solvers", 2))
        coordinator = bool(body.get("coordinator", True))
        # engine roster: three-engine race by default (cursor + claude + codex).
        # Resolution order: explicit body.engines > the operator's per-category
        # worker-config default (apps/web/worker_config.py) > the hardcoded roster.
        # Offline capability is checked per selected profile below. Cursor 当前无
        # 可核验的原生离线开关，因此明确拒绝；其他引擎使用 CLI 原生 deny
        # 参数、只读配置覆盖或本地工具 allowlist。
        wc = mgr.worker_config.resolve(roster_category or None) if mgr is not None else {}
        engines = body.get("engines") or wc.get("engines") or ["cursor", "claude", "codex", "pi", "omp"]
        worker_profiles = body.get("worker_profiles") or wc.get("worker_profiles") or []
        worker_network = str(
            body.get("worker_network") or wc.get("worker_network") or "bridge"
        ).strip()
        if worker_network not in {"bridge", "host", "none"}:
            raise RuntimeError("worker_network must be bridge, host, or none")
        # #171 / MNT-09.04: offline / web_access=false denies WebSearch/WebFetch
        # only. It must NOT silently force Docker --network none, and must not
        # report "cannot reach network". Container egress is a separate knob.
        if offline:
            incompatible_profiles = [
                p for p in _selected_profiles(engines, worker_profiles)
                if not bool(getattr(
                    driver_for(p), "offline_web_isolation", False))
            ]
            if incompatible_profiles:
                names = ", ".join(
                    str(p.get("name") or p.get("id"))
                    for p in incompatible_profiles
                )
                raise RuntimeError(
                    "profile_incompatible offline eval cannot isolate web tools for profile(s): "
                    + names
                )
        worker_container_scope = str(
            body.get("worker_container_scope")
            or wc.get("worker_container_scope") or "run"
        ).strip()
        if mode == "pentest":
            worker_container_scope = "run"
        if worker_container_scope not in {"run", "shared"}:
            raise RuntimeError("worker_container_scope must be run or shared")
        worker_privilege = str(
            body.get("worker_privilege")
            or wc.get("worker_privilege") or "default"
        ).strip().lower()
        if mode == "pentest":
            worker_privilege = "default"
        if worker_privilege not in {"default", "elevated"}:
            raise RuntimeError("worker_privilege must be default or elevated")
        # MNT-09.03 / #170 — resolve cgroup + stream/workdir budgets (not TSec defaults).
        from muteki.solver.worker_resource_limits import resolve_worker_resource_limits
        _limits = resolve_worker_resource_limits(
            memory=body.get("worker_memory", wc.get("worker_memory")),
            cpus=body.get("worker_cpus", wc.get("worker_cpus")),
            pids_limit=body.get("worker_pids_limit", wc.get("worker_pids_limit")),
            output_limit=body.get(
                "worker_output_limit", wc.get("worker_output_limit")),
            disk_limit=body.get("worker_disk_limit", wc.get("worker_disk_limit")),
            config=wc,
        )
        worker_memory = _limits.memory
        worker_cpus = _limits.cpus
        worker_pids_limit = _limits.pids_limit
        worker_output_limit = _limits.output_limit
        worker_disk_limit = _limits.disk_limit
        worker_vpn_config = None
        if bool(wc.get("worker_vpn_enabled")) and mgr is not None:
            candidate = (
                mgr.state_root
                / "_secrets" / "runtime" / "openvpn" / "client.ovpn"
            )
            if not candidate.is_file():
                raise RuntimeError("OpenVPN 已启用，但尚未上传配置文件")
            worker_vpn_config = candidate
        # bootstrap worker count: explicit body wins, else the config default, else
        # one per engine (heterogeneous rush). max_workers likewise from config.
        default_sw = wc.get("start_workers") or len(engines)
        start_workers = int(body.get("start_workers", default_sw))
        max_workers = int(body.get("max_workers", wc.get("max_workers", 10)))
        # wall-clock cap. ABSENT → the Swarm default (infinite: the interactive deck
        # never gives up on its own; only solve / operator-stop ends it). A batch
        # eval, which is unattended, MUST pass a finite budget so a hard challenge
        # can't run forever. `0`/None/negative are treated as "no cap" too.
        _wcb = body.get("wall_clock_budget")
        if _wcb is None:
            _wcb = body.get("visit_timebox_s")
        if _wcb is None and wc:
            _wcb = wc.get("wall_clock_budget")
        wall_clock_budget = float(_wcb) if (_wcb and float(_wcb) > 0) else float("inf")
        max_total_workers = int(body.get("max_total_workers", wc.get("max_total_workers", 0)) or 0) or None
        cost_budget_usd = float(body.get("cost_budget_usd", wc.get("cost_budget_usd", 0.0)) or 0.0) or None
        token_budget = int(body.get("token_budget", 0) or 0)
        tool_call_budget = int(body.get("tool_call_budget", 0) or 0)
        llm_profiles = body.get("llm_profiles") or wc.get("llm_profiles") or {}
        if "stage_policy" in body:
            stage_policy = copy.deepcopy(body.get("stage_policy") or {})
        elif wc.get("stage_policy"):
            stage_policy = copy.deepcopy(wc["stage_policy"])
        else:
            stage_policy = {
                "race": {
                    "enabled": bool(body["race_scout"]) if "race_scout" in body else (
                        False if mode == "pentest" else bool(wc.get("race_scout", True))),
                    "timeout": int(body.get("race_timeout", wc.get("race_timeout", 300))),
                    "engines": body.get("race_engines") or wc.get("race_engines") or [],
                },
                "coordinator": {"wall_clock_budget": 0 if wall_clock_budget == float("inf") else int(wall_clock_budget)},
                "budgets": {"max_total_workers": max_total_workers or 0,
                            "cost_budget_usd": cost_budget_usd or 0.0},
            }
        if "race_timeout" in body:
            stage_policy.setdefault("race", {})["timeout"] = int(body["race_timeout"])
        if "wall_clock_budget" in body or "visit_timebox_s" in body:
            v = float(
                body.get("wall_clock_budget")
                or body.get("visit_timebox_s")
                or 0
            )
            stage_policy.setdefault("coordinator", {})["wall_clock_budget"] = (
                int(v) if v > 0 else 0)
        if "max_total_workers" in body:
            stage_policy.setdefault("budgets", {})["max_total_workers"] = int(
                body["max_total_workers"] or 0)
        if "cost_budget_usd" in body:
            stage_policy.setdefault("budgets", {})["cost_budget_usd"] = float(
                body["cost_budget_usd"] or 0.0)
        stage_policy.setdefault("coordinator", {})
        stage_policy["coordinator"]["token_budget"] = token_budget
        stage_policy["coordinator"]["tool_call_budget"] = tool_call_budget
        # Web exposes two scheduling modes: auto (Decide-led, no first-round
        # Race) and fixed (all selected ordinary seats join the first Race).
        # A legacy race_scout/race_engines request cannot create fixed-without-
        # Race or auto-with-Race as a hidden third mode.
        dispatch_mode = str(
            stage_policy.setdefault("coordinator", {}).get("dispatch_mode")
            or "fixed"
        ).strip().lower()
        if dispatch_mode not in {"auto", "fixed"}:
            dispatch_mode = "fixed"
        if mode == "pentest":
            dispatch_mode = "auto"
        stage_policy["coordinator"]["dispatch_mode"] = dispatch_mode
        race_scout = dispatch_mode == "fixed"
        race_engines = list(engines) if race_scout else []
        race_timeout = int(body.get("race_timeout", wc.get("race_timeout", 300)))
        stage_policy.setdefault("race", {})["enabled"] = race_scout
        stage_policy["race"]["engines"] = list(race_engines)
        if race_scout:
            worker_profiles = copy.deepcopy(worker_profiles)
            selected = set(race_engines)
            for profile in worker_profiles:
                if not isinstance(profile, dict):
                    continue
                if str(profile.get("name") or profile.get("id") or "") not in selected:
                    continue
                profile["race"] = True
                roles = list(profile.get("roles") or [])
                if "race" not in roles:
                    profile["roles"] = [*roles, "race"]
        # cold_start (run-75379 BUG④): "继续做题"/standby relaunch sets this False so the
        # coordinator skips the race-scout warmup and continues on the existing graph.
        # Default True = a fresh run. The Swarm ALSO has a graph-state backstop, so a
        # caller that omits this is still protected on a populated graph.
        cold_start = bool(body["cold_start"]) if "cold_start" in body else True

        # worker execution backend: "local" (host subprocess) or "container" (each
        # worker in the run's Kali tool container). Request body wins, else config,
        # else env, else default — with the container_dockerexec alias, invalid
        # fallback, and the web-container override all owned by the single resolver
        # so the settings health endpoints resolve the SAME effective backend.
        worker_backend = resolve_worker_backend(
            request_backend=("container" if mode == "pentest" else body.get("worker_backend")),
            config_backend=wc.get("worker_backend"),
            env_backend=os.environ.get("MUTEKI_WORKER_BACKEND"),
            in_web_container=is_web_container(),
        )
        effective_worker_network = ""
        if worker_backend == "container":
            from muteki.solver.container_exec import (
                WorkerNetworkConfigError,
                project_worker_network,
                resolve_worker_run_network,
            )
            try:
                _net_proj = project_worker_network(worker_network)
                resolve_worker_run_network(worker_network, needs_egress=True)
            except WorkerNetworkConfigError as exc:
                raise RuntimeError(str(exc)) from exc
            effective_worker_network = _net_proj["effective"]
        startup_health_snapshot: dict[str, bool] | None = None
        if mgr is not None:
            from muteki.core.events import Event, EventType

            precheck_profiles, unknown_profile_refs = _startup_profiles(
                engines=list(engines),
                race_engines=list(race_engines) if race_engines else None,
                worker_profiles=worker_profiles,
                stage_policy=stage_policy,
                coordinator=coordinator,
            )
            if not precheck_profiles and not worker_profiles:
                precheck_profiles = [
                    {
                        "id": str(engine),
                        "name": str(engine),
                        "engine": str(engine),
                        "model": "",
                        "credential_account": "",
                        "enabled": True,
                    }
                    for engine in engines
                ]
            await run.bus.emit(Event(
                event_type=EventType.RUN_PREPARING,
                run_id=run.run_id,
                challenge_id=challenge.id,
                payload={
                    "phase": "preflight",
                    "challenge": _public_challenge_payload(
                        challenge,
                        operator_name=operator_name,
                        roster_category=roster_category,
                    ),
                    "profiles": [
                        {
                            "profile_id": str(
                                profile.get("id") or profile.get("name")
                                or profile.get("engine") or ""),
                            "engine": base_engine_for_profile(profile),
                            "model": str(profile.get("model") or ""),
                            "reused": _profile_readiness_key(
                                profile,
                                runtime={"network": worker_network},
                                backend=backend_for_profile(
                                    worker_backend=worker_backend,
                                    in_web_container=is_web_container(),
                                ),
                            ) in run.profile_readiness,
                        }
                        for profile in precheck_profiles
                    ],
                },
            ))
            if unknown_profile_refs:
                startup_health_snapshot = {}
                preflight_failures = [
                    {
                        "profile_id": ref,
                        "engine": "",
                        "model": "",
                        "backend": worker_backend,
                        "network": worker_network if worker_backend == "container" else "",
                        "effective_network": (
                            (effective_worker_network or worker_network)
                            if worker_backend == "container" else ""
                        ),
                        "stage": "preflight",
                        "layer": "binding",
                        "code": "unknown_profile_ref",
                        "detail": "任务引用了不存在的 Worker Profile",
                        "error_id": _preflight_error_id(
                            ref, "binding", "unknown_profile_ref"),
                    }
                    for ref in unknown_profile_refs
                ]
            else:
                startup_health_snapshot, preflight_failures = await _startup_readiness(
                    profiles=precheck_profiles,
                    worker_network=worker_network,
                    worker_backend=worker_backend,
                    sessions_root=mgr.state_root,
                    cached_results=run.profile_readiness,
                    effective_network=effective_worker_network,
                )
                mgr.persist_profile_readiness(run)
            # A stale optional profile must not make a task unusable when another
            # selected profile has passed its real model preflight.  The Swarm
            # consumes the same snapshot and excludes the failed profile from its
            # roster.  Only fail the whole task when there is no runnable profile.
            healthy_profile_ids = {
                profile_id
                for profile_id, ok in (startup_health_snapshot or {}).items()
                if ok
            }
            if preflight_failures and healthy_profile_ids:
                for failure in preflight_failures:
                    await run.bus.emit(Event(
                        event_type=EventType.BLACKBOARD_DELTA,
                        run_id=run.run_id,
                        challenge_id=challenge.id,
                        payload={
                            "kind": "engine_degraded",
                            "actor": "preflight",
                            "engine": str(failure.get("engine") or failure.get("profile_id") or ""),
                            "profile_id": str(failure.get("profile_id") or ""),
                            "status": "degraded",
                            "reason": str(failure.get("detail") or "preflight failed"),
                            "code": str(failure.get("code") or "preflight_failed"),
                        },
                    ))
            elif preflight_failures:
                await run.bus.emit(Event(
                    event_type=EventType.RUN_FINISHED,
                    run_id=run.run_id,
                    challenge_id=challenge.id,
                    payload={
                        "flag": None,
                        "flags": [],
                        "expected_flags": challenge.expected_flags,
                        "multi_flag": challenge.multi_flag,
                        "solved": False,
                        "reason": "preflight_failed",
                        "failure_code": "profile_unhealthy",
                        "failure_phase": "preflight",
                        "error_id": _preflight_error_id(
                            run.run_id,
                            *(failure.get("error_id", "")
                              for failure in preflight_failures),
                        ),
                        "detail": (
                            f"Worker 预检失败（{len(preflight_failures)} 个 Profile）"
                        ),
                        "profile_failures": preflight_failures,
                    },
                ))
                if llm_cm is not None:
                    try:
                        await llm_cm.__aexit__(None, None, None)
                    except Exception:
                        pass
                    llm_cm = None
                    llm = None
                return

        if mgr is not None:
            if worker_backend == "container" and worker_container_scope == "shared":
                mgr.prepare_shared_workspace(run.run_id)
            root = mgr.workspace_dir(run.run_id)
        else:
            root = Path(tempfile.mkdtemp(prefix="muteki-web-"))
        # sbx is the sandbox root — sandbox.shutdown_all() rmtree's it at run end,
        # so NOTHING durable may live under it. arts + graph are SIBLINGS of sbx so
        # they persist (the shared_graph.db is the run's queryable fact graph).
        sandbox = SandboxManager(bus=run.bus, root=root / "sbx")
        arts = ArtifactStore(root=root / "arts")
        graph_dir = (
            mgr.graph_dir(run.run_id) if mgr is not None else root / "graph"
        )
        # Every backend and container scope writes into the same logical Run
        # workspace. Shared scope redirects only enrolled Run workspaces into a
        # dedicated pool mount; Coordinator state stays outside that mount.
        worker_root = root / "workers"

        # Pi Decide remains the planner. The HTTP client is only used for the
        # evidence-grounded report at terminal finalization.

        # §16 flywheel store (optional; recall prior + distill on solve)
        from muteki.learning.distill import TemplateStore
        knowledge = TemplateStore(root=os.environ.get("MUTEKI_KNOWLEDGE_DIR", "knowledge"))

        # Initialise the run-local control boundary before workers are built so
        # every spawn registers against the same registry and secret:// values can
        # be materialised only at the final in-memory worker injection boundary.
        secret_resolver = None
        context_provider = None
        context_binder = None
        context_reserver = None
        context_committer = None
        context_releaser = None
        context_delivery_unknown_marker = None
        context_status_provider = None
        context_expirer = None
        standing_clear_provider = None
        control_state_provider = None
        worker_registry = getattr(run, "worker_registry", None)
        if mgr is not None:
            try:
                _actor, control_journal, secret_store = mgr._ensure_control(run)
                secret_resolver = secret_store.resolve
                context_provider = control_journal.context_resources
                context_binder = control_journal.bind_context
                context_reserver = control_journal.reserve_context
                context_committer = control_journal.commit_context_binding
                context_releaser = control_journal.release_context_reservation
                context_delivery_unknown_marker = (
                    control_journal.mark_context_delivery_unknown)
                context_status_provider = control_journal.context_delivery_status
                context_expirer = control_journal.expire_context
                standing_clear_provider = (
                    control_journal.standing_clear_operations)
                control_state_provider = control_journal.current_state
            except Exception:
                # Control is additive: a journal/storage failure must not prevent
                # an otherwise valid solve from starting.
                secret_resolver = None

        swarm = swarm_cls(
            challenge, default_lineup(n), llm=llm, sandbox=sandbox,
            bus=run.bus, cost=run.cost, artifacts=arts,
            config=SolverConfig(), run_id=run.run_id, knowledge=knowledge,
            execution_generation=int(getattr(run, "execution_generation", 1) or 1),
            hitl_inbox=run.hitl,
            worker_cmds=run.worker_cmds,
            control_ready=run.control_ready,
            worker_control_ready=run.worker_control_ready,
            executor=executor, cli_engine=cli_engine, cli_race=cli_race,
            engines=engines, start_workers=start_workers, max_workers=max_workers,
            web_access=web_access, kb=kb, coordinator=coordinator,
            graph_dir=graph_dir, worker_root=worker_root,
            wall_clock_budget=wall_clock_budget,
            race_scout=race_scout, race_engines=race_engines,
            race_timeout=race_timeout, cold_start=cold_start,
            max_total_workers=max_total_workers,
            cost_budget_usd=cost_budget_usd,
            stage_policy=stage_policy,
            llm_profiles=llm_profiles,
            reason_model=(llm_profiles.get("planner") or {}).get("model", "deepseek-v4-pro"),
            worker_backend=worker_backend,
            worker_network=worker_network,
            worker_container_scope=worker_container_scope,
            worker_privilege=worker_privilege,
            worker_memory=worker_memory,
            worker_cpus=worker_cpus,
            worker_pids_limit=worker_pids_limit,
            worker_output_limit=worker_output_limit,
            worker_disk_limit=worker_disk_limit,
            shared_mount_root=(
                mgr.storage.shared_worker_mount() if mgr is not None else None
            ),
            account_projection_root=(
                mgr.account_projection_root if mgr is not None else None
            ),
            container_bootstrap_root=(
                mgr.container_bootstrap_root if mgr is not None else None
            ),
            worker_vpn_config=worker_vpn_config,
            worker_profiles=worker_profiles,
            startup_health_snapshot=startup_health_snapshot,
            credential_accounts_root=(
                account_store_root(mgr.state_root) if mgr is not None else None
            ),
            worker_registry=worker_registry,
            secret_resolver=secret_resolver,
            context_provider=context_provider,
            context_binder=context_binder,
            context_reserver=context_reserver,
            context_committer=context_committer,
            context_releaser=context_releaser,
            context_delivery_unknown_marker=context_delivery_unknown_marker,
            context_status_provider=context_status_provider,
            context_expirer=context_expirer,
            standing_clear_provider=standing_clear_provider,
            control_state_provider=control_state_provider,
            # 做题模式只使用已注册的无交互 CLI Worker。Conversation 的 ACP、
            # App Server、SDK 注册表不得进入 Swarm。
            adapter_registry=None,
        )
        if mgr is not None:
            # Swarm owns the trusted winning outcome; RunManager owns storage that
            # Worker containers cannot modify. Keep this hook process-local so
            # experimental Swarm classes do not need a constructor API change.
            swarm._winner_continuation_writer = (  # type: ignore[attr-defined]
                lambda payload: mgr.persist_winner_continuation(
                    run.run_id, payload)
            )
        deferred_cleanup = False
        try:
            out = await swarm.run()
            run.flag = out.flag
        except BaseException as exc:
            from muteki.swarm.swarm_support import ControlShutdownIncomplete
            if not isinstance(exc, ControlShutdownIncomplete):
                raise
            deferred_cleanup = True
            run.runtime_incomplete = True
            run.runtime_owner = swarm
            run.runtime_error = (
                f"control shutdown incomplete ({type(exc).__name__})")
            cleanup_state = {"sandbox": False, "llm": False}

            async def _settle_incomplete_runtime() -> None:
                try:
                    await swarm.settle_control_shutdown()
                    if not cleanup_state["sandbox"]:
                        await sandbox.shutdown_all()
                        cleanup_state["sandbox"] = True
                    if llm_cm is not None and not cleanup_state["llm"]:
                        await llm_cm.__aexit__(None, None, None)
                        cleanup_state["llm"] = True
                    # settle_control_shutdown emits the delayed truthful terminal
                    # event only after the orphan owner has left and graph/container
                    # teardown is safe.
                    run.finished = True
                    if run.progress_publisher is not None:
                        await run.progress_publisher.publish_pending(
                            trigger="terminal")
                    await run.bus.close()
                except BaseException as cleanup_exc:
                    run.runtime_error = (
                        "runtime cleanup failed "
                        f"({type(cleanup_exc).__name__})")
                    raise
                else:
                    run.runtime_incomplete = False
                    run.runtime_owner = None
                    run.runtime_error = ""
                    run.runtime_settle = None

            run.runtime_settle = _settle_incomplete_runtime
            run.runtime_cleanup_task = asyncio.create_task(
                _settle_incomplete_runtime(),
                name=f"runtime-owner-settle-{run.run_id}",
            )
            raise
        finally:
            if not deferred_cleanup:
                original = sys.exception()
                cleanup_failures: list[BaseException] = []
                try:
                    await sandbox.shutdown_all()
                except BaseException as cleanup_exc:
                    cleanup_failures.append(cleanup_exc)
                if llm_cm is not None:
                    try:
                        await llm_cm.__aexit__(None, None, None)
                    except BaseException as cleanup_exc:
                        cleanup_failures.append(cleanup_exc)
                if cleanup_failures:
                    if original is None:
                        raise cleanup_failures[0]
                    if not isinstance(original, asyncio.CancelledError):
                        classes = ", ".join(
                            type(failure).__name__ for failure in cleanup_failures
                        )
                        original.add_note(
                            "Driver final cleanup failed without replacing the "
                            f"original exception; cleanup_error_classes={classes}"
                        )

    return drive


# ---- standby (post-solve HITL) ----------------------------------------------
# After a run finishes (or the server restarted), a human follow-up no longer has
# a live swarm to reach. The standby driver COLD-STARTS a single worker from disk:
# it reads coordinator-owned continuation state + the persisted shared_graph,
# resumes that SAME session, and serves one command — answer a
# question, mark the flag a false-positive and keep solving, or write a writeup.
# Everything it needs is durable, so this works identically before and after a
# server restart. Older runs without private continuation metadata recover only
# non-sensitive identity from durable events and start a fresh session if needed.

def _standby_profile_for(
    engine: str,
    worker_profiles: list[dict[str, Any]],
) -> dict[str, Any] | None:
    """Pick the profile that should serve a post-solve standby command."""
    if not worker_profiles:
        return None
    engine = (engine or "").strip()
    by_name = {
        str(p.get("name") or p.get("id")): p
        for p in worker_profiles
        if isinstance(p, dict) and p.get("enabled", True)
    }
    if engine in by_name:
        return by_name[engine]
    for p in by_name.values():
        if base_engine_for_profile(p) == engine:
            return p
    return None


def _standby_worker_env(
    *,
    root: Path,
    label: str,
    engine: str,
    profile: dict[str, Any] | None,
    account_root: Path | None,
    container: object | None,
) -> dict[str, str]:
    from muteki.solver.credential_accounts import runtime_env_for_engine

    agent_state_dir: Path | None = None
    agent_state_container_path: str | None = None
    if engine in {"pi", "omp", "opencode"}:
        agent_state_dir = root / ".muteki-agent-state" / label
        agent_state_dir.mkdir(parents=True, exist_ok=True)
        if container is not None:
            mapper = getattr(container, "to_container_path", None)
            if callable(mapper):
                agent_state_container_path = mapper(str(agent_state_dir))

    env = runtime_env_for_engine(
        engine,
        account_root=account_root,
        account_id=(profile.get("credential_account") if profile else None),
        container=container is not None,
        agent_state_dir=agent_state_dir,
        agent_state_container_path=agent_state_container_path,
        model=str((profile or {}).get("model") or ""),
    ).env
    if profile:
        apply_worker_identity_env(env, profile)
        env["MUTEKI_WORKER_REASONING_EFFORT"] = str(
            profile.get("reasoning_effort") or "default")
    if container is not None:
        from muteki.swarm.swarm import _ensure_blackboard_skill_links
        from muteki.solver.container_exec import _chown_tree_to_worker
        if agent_state_dir is not None:
            _chown_tree_to_worker(str(agent_state_dir))
        home_host = root / "homes" / label
        home_host.mkdir(parents=True, exist_ok=True)
        _ensure_blackboard_skill_links(home_host)
        _chown_tree_to_worker(str(home_host))
        mapper = getattr(container, "to_container_path", None)
        env["HOME"] = mapper(str(home_host)) if callable(mapper) else str(home_host)
    return env


def _standby_home_label(root: Path, engine: str, session: str) -> str:
    """Best-effort reuse of the winner worker's HOME for CLI session resume."""
    fallback = f"cli-{engine}-standby"
    homes = root / "homes"
    if not homes.exists():
        return fallback
    candidates = sorted(
        p for p in homes.glob(f"cli-{engine}*")
        if p.is_dir()
    )
    needle = (session or "").strip()
    if needle:
        for home in candidates:
            try:
                for p in home.rglob("*"):
                    if needle in str(p):
                        return home.name
                    if not p.is_file():
                        continue
                    try:
                        if p.stat().st_size > 2_000_000:
                            continue
                        if needle in p.read_text(encoding="utf-8", errors="ignore"):
                            return home.name
                    except OSError:
                        continue
            except OSError:
                continue
    primary = homes / f"cli-{engine}"
    if primary.exists():
        return primary.name
    return candidates[0].name if candidates else fallback


def build_standby_driver(cmd: dict[str, Any], mgr: "RunManager | None" = None) -> Driver:
    """A driver that serves ONE post-solve HITL command via a resumed worker."""
    async def drive(run: Run) -> None:
        import asyncio
        import inspect
        import json
        from pathlib import Path

        from muteki.models.solve_graph import Challenge
        from muteki.pentest.contract import PentestContract
        from muteki.solver.cli_driver import driver_for
        from muteki.solver.cli_solver import CliSolver
        from muteki.solver.credential_accounts import account_store_root
        from muteki.solver.result import ArtifactStore
        from muteki.solver.types import SolverConfig
        from muteki.swarm.shared_graph import SQLiteSharedGraph
        from muteki.swarm.worker_session import WorkerSessionSupervisor

        mark_false_already_applied = bool(
            cmd.get("_control_mark_false_applied", False))
        safe_cmd = {
            key: value for key, value in dict(cmd).items()
            if not str(key).startswith("_control_")
            and key != "_standby_delivery_ack"
        }
        context_reservations = list(
            cmd.get("_control_context_reservations") or [])
        context_owner = str(cmd.get("_control_context_owner") or "")
        if mgr is not None:
            _actor, control_journal, control_secrets = mgr._ensure_control(run)
        else:
            control_journal = None
            control_secrets = None
        runtime_cmd = dict(safe_cmd)
        materialized_secret_values: list[str] = []
        if context_reservations:
            if control_journal is None or control_secrets is None:
                raise RuntimeError("standby context journal is unavailable")
            context_id = str(context_reservations[0][0])
            resource = next(
                (row for row in control_journal.context_resources(active_only=False)
                 if str(getattr(row, "context_id", "")) == context_id),
                None,
            )
            if resource is None:
                raise RuntimeError("standby context resource is unavailable")
            content = str(getattr(resource, "content", "") or "")
            if content.startswith("secret://"):
                try:
                    content = str(control_secrets.resolve(content) or "")
                except Exception:
                    raise RuntimeError("standby secret material is unavailable") from None
                if not content or content.startswith("secret://"):
                    raise RuntimeError("standby secret material is unavailable")
                materialized_secret_values.append(content)
            # One reserved resource authorises exactly one prompt value. Never
            # recursively decrypt the rest of the envelope/metadata.
            runtime_cmd = {
                key: safe_cmd[key]
                for key in (
                    "action", "target", "command_id", "request_id",
                    "standing", "preempt_policy", "preemption", "flag",
                    "followup_id",
                )
                if key in safe_cmd
            }
            kind = getattr(resource, "kind", "")
            kind_value = str(getattr(kind, "value", kind) or "")
            if kind_value == "endpoint":
                runtime_cmd["url"] = content
            else:
                runtime_cmd["text"] = content
        action = (runtime_cmd.get("action") or "ask").lower()

        if mgr is not None:
            root = mgr.storage.workspace(run.run_id)
        else:
            return  # no workspace → nothing durable to resume from

        winner = mgr.load_winner_continuation(run.run_id)

        # Rebuild the Challenge from coordinator-owned state. Older runs recover
        # the launch payload and winning Worker identity from durable server events.
        # Worker-writable workspace files never select profiles, credentials,
        # backends, sessions or host paths.
        ch = winner.get("challenge") or {}
        legacy_workers: dict[str, dict[str, str]] = {}
        legacy_winner_actor = ""
        if not ch or not winner.get("engine") or not winner.get("profile_id"):
            try:
                from muteki.core.events import EventType
                async for ev in run.store.replay(run.run_id):
                    payload = ev.payload or {}
                    if not ch and ev.event_type in {
                        EventType.RUN_PREPARING, EventType.RUN_STARTED,
                    }:
                        ch = payload.get("challenge") or {}

                    solver_id = str(ev.solver_id or "").strip()
                    if solver_id and ev.event_type in {
                        EventType.WORKER_STATUS, EventType.WORKER_LIFECYCLE,
                    }:
                        current = legacy_workers.setdefault(solver_id, {})
                        for source, target in (
                            ("engine", "engine"),
                            ("profile_id", "profile_id"),
                            ("session", "session"),
                        ):
                            value = str(payload.get(source) or "").strip()
                            if value:
                                current[target] = value

                    kind = str(payload.get("kind") or "")
                    actor = ""
                    if (ev.event_type is EventType.BLACKBOARD_DELTA
                            and kind == "flag_found"):
                        actor = str(payload.get("actor") or solver_id).strip()
                    elif (ev.event_type is EventType.SOLVE_GRAPH_DELTA
                          and kind == "flag"):
                        actor = solver_id
                    elif (ev.event_type is EventType.INSIGHT_BUS_EVENT
                          and kind == "FlagFound"):
                        actor = str(payload.get("by") or solver_id).strip()
                    elif ev.event_type is EventType.FLAG_ACCEPTED:
                        actor = str(payload.get("actor") or solver_id).strip()
                    if actor and actor != "coordinator":
                        legacy_winner_actor = actor

            except Exception:
                pass
        if legacy_winner_actor:
            legacy_worker = legacy_workers.get(legacy_winner_actor) or {}
            winner.setdefault("worker_id", legacy_winner_actor)
            if legacy_worker.get("engine"):
                winner.setdefault("engine", legacy_worker["engine"])
            if legacy_worker.get("profile_id"):
                winner.setdefault("profile_id", legacy_worker["profile_id"])
            if legacy_worker.get("session"):
                winner.setdefault("session", legacy_worker["session"])
        mode = ch.get("mode") or "ctf"
        if mode not in ("ctf", "pentest"):
            mode = "ctf"
        pentest_contract = (
            PentestContract.model_validate(ch["pentest_contract"])
            if mode == "pentest" and isinstance(ch.get("pentest_contract"), dict)
            else None
        )
        standby_flag_contract = normalize_flag_contract(
            ch.get("flag_format", _DEFAULT_BRACE_FLAG_FORMAT),
            ch.get("flag_format_wrapper", ""),
        )
        challenge = Challenge(
            id=run.run_id,
            name=ch.get("name", run.name or run.run_id),
            category=ch.get("category", run.category or "web"),
            points=ch.get("points", 0),
            description=ch.get("description", ""),
            target=ch.get("target"),
            attachments=[],
            flag_format=standby_flag_contract.flag_format,
            flag_format_hint=(
                ch.get("flag_format_hint", "")
                or standby_flag_contract.flag_format_wrapper
            ),
            flag_format_wrapper=standby_flag_contract.flag_format_wrapper,
            # Carry the run's flag mode across a post-solve standby re-solve.
            expected_flags=int(ch.get("expected_flags") or 1),
            initial_flags=list(ch.get("initial_flags") or []),
            platform_confirmation_required=bool(
                ch.get("platform_confirmation_required", False)),
            multi_flag=bool(ch.get("multi_flag", False)),
            allow_operator_input=bool(ch.get("allow_operator_input", True)),
            verifier_rate_limited=bool(ch.get("verifier_rate_limited", False)),
            mode=mode,
            goal=ch.get("goal") or "",
            scope=ch.get("scope") or "",
            pentest_contract=pentest_contract,
        )

        wc = mgr.worker_config.resolve(challenge.category) if mgr is not None else {}
        worker_profiles = wc.get("worker_profiles") or []
        worker_network = str(wc.get("worker_network") or "bridge")
        worker_container_scope = str(wc.get("worker_container_scope") or "run")
        if mode == "pentest":
            worker_container_scope = "run"
        if root.is_symlink():
            if root.resolve() != mgr.storage.shared_workspace(run.run_id).resolve():
                raise RuntimeError("Run workspace points outside its shared pool slot")
            worker_container_scope = "shared"
        from muteki.solver.worker_resource_limits import resolve_worker_resource_limits
        _standby_limits = resolve_worker_resource_limits(config=wc)
        worker_memory = _standby_limits.memory
        worker_cpus = _standby_limits.cpus
        worker_pids_limit = _standby_limits.pids_limit
        worker_output_limit = _standby_limits.output_limit
        worker_disk_limit = _standby_limits.disk_limit
        worker_vpn_config = None
        if bool(wc.get("worker_vpn_enabled")) and mgr is not None:
            candidate = (
                mgr.state_root
                / "_secrets" / "runtime" / "openvpn" / "client.ovpn"
            )
            if candidate.is_file():
                worker_vpn_config = candidate
        winner_engine = str(winner.get("engine") or "claude")
        winner_profile_ref = str(
            winner.get("profile_id") or winner_engine
        ).strip()
        profile = _standby_profile_for(winner_profile_ref, worker_profiles)
        if winner.get("profile_id") and profile is None:
            raise RuntimeError(
                "winning Worker profile is unavailable in current configuration"
            )
        transport = base_engine_for_profile(profile or winner_engine)
        worker_backend = resolve_worker_backend(
            request_backend="container" if mode == "pentest" else None,
            config_backend=wc.get("worker_backend"),
            env_backend=os.environ.get("MUTEKI_WORKER_BACKEND"),
            in_web_container=is_web_container(),
        )
        backend = (
            backend_for_profile(
                worker_backend=worker_backend,
                in_web_container=is_web_container(),
            )
            if profile else worker_backend
        )
        if backend == "container" and worker_container_scope == "shared":
            mgr.prepare_shared_workspace(run.run_id)
        root = mgr.workspace_dir(run.run_id)
        graph_dir = mgr.graph_dir(run.run_id)
        arts = ArtifactStore(root=root / "arts")
        worker_root = root / "workers"
        worker_root.mkdir(parents=True, exist_ok=True)
        container = None
        setup_cancel_boundary = None
        setup_exit_query = None
        account_root = account_store_root(mgr.state_root) if mgr is not None else None
        if backend == "container":
            from muteki.solver.container_exec import ensure_container
            setup_container_active = True

            async def _cancel_setup_container() -> None:
                nonlocal setup_container_active
                if not setup_container_active:
                    return
                from muteki.solver.container_exec import teardown_container
                removed = await asyncio.to_thread(
                    teardown_container, run.run_id, remove=True,
                    container_scope=worker_container_scope,
                    bootstrap_root=str(mgr.container_bootstrap_root))
                if removed is not True:
                    raise RuntimeError("container teardown could not be proven")
                setup_container_active = False

            def _setup_runtime_exited() -> bool:
                return not setup_container_active

            async def _wait_setup_exit(_timeout=None) -> bool:
                return not setup_container_active

            setup_cancel_boundary = _cancel_setup_container
            setup_exit_query = _setup_runtime_exited

            def _clear_setup_owner(setup_task: asyncio.Task[Any]) -> None:
                if run.standby_cancel is _cancel_setup_container:
                    run.standby_cancel = None
                if run.standby_runtime_exited is _setup_runtime_exited:
                    run.standby_runtime_exited = None
                if run.standby_wait_runtime_exit is _wait_setup_exit:
                    run.standby_wait_runtime_exit = None
                if run.standby_setup_task is setup_task:
                    run.standby_setup_task = None
                try:
                    current = asyncio.current_task()
                except RuntimeError:
                    current = None
                if run.standby_runtime_cleanup_task is current:
                    run.standby_runtime_cleanup_task = None

            async def _reap_failed_setup(setup_task: asyncio.Task[Any]) -> None:
                try:
                    while setup_container_active:
                        try:
                            await _cancel_setup_container()
                        except asyncio.CancelledError:
                            raise
                        except Exception:
                            await asyncio.sleep(0.1)
                finally:
                    if not setup_container_active:
                        _clear_setup_owner(setup_task)

            def _retain_failed_setup(setup_task: asyncio.Task[Any]) -> None:
                run.standby_cancel = _cancel_setup_container
                run.standby_runtime_exited = _setup_runtime_exited
                run.standby_wait_runtime_exit = _wait_setup_exit
                cleanup = run.standby_runtime_cleanup_task
                if cleanup is None or cleanup.done():
                    run.standby_runtime_cleanup_task = asyncio.create_task(
                        _reap_failed_setup(setup_task),
                        name=f"standby-setup-reap:{run.run_id}",
                    )

            setup_task = asyncio.create_task(asyncio.to_thread(
                    ensure_container,
                    run.run_id,
                    str(worker_root.parent),
                    network=worker_network,
                    memory=worker_memory,
                    cpus=worker_cpus,
                    pids_limit=worker_pids_limit,
                    output_limit=worker_output_limit,
                    disk_limit=worker_disk_limit,
                    account_root=(str(account_root) if account_root is not None else None),
                    account_ids=sorted({
                        str(p.get("credential_account") or "").strip()
                        for p in (worker_profiles or [])
                        if str(p.get("credential_account") or "").strip()
                    }),
                    container_scope=worker_container_scope,
                    shared_mount_root=(
                        str(mgr.storage.shared_worker_mount()) if mgr is not None else None
                    ),
                    account_projection_root=(
                        str(mgr.account_projection_root) if mgr is not None else None
                    ),
                    bootstrap_root=(
                        str(mgr.container_bootstrap_root) if mgr is not None else None
                    ),
                    vpn_config=(str(worker_vpn_config) if worker_vpn_config else None),
                    worker_privilege=str(wc.get("worker_privilege") or "default"),
                ), name=f"standby-runtime-setup:{run.run_id}")
            run.standby_setup_task = setup_task
            try:
                container = await asyncio.shield(setup_task)
            except asyncio.CancelledError:
                # to_thread acquisition is not cancellable. Retain ownership until
                # it lands, then tear down the possibly-created container before the
                # wrapper is allowed to finish cancellation.
                try:
                    container = await asyncio.shield(setup_task)
                except Exception:
                    container = None
                _retain_failed_setup(setup_task)
                try:
                    await _cancel_setup_container()
                except Exception:
                    # The autonomous reaper retains the only cleanup owner.
                    pass
                if not setup_container_active:
                    _clear_setup_owner(setup_task)
                raise
            except Exception:
                # ensure_container can create the container successfully and fail
                # later while awaiting its supervisor. Treat every ordinary setup
                # exception as a potential acquired owner and prove rollback.
                _retain_failed_setup(setup_task)
                try:
                    await _cancel_setup_container()
                except Exception:
                    pass
                if not setup_container_active:
                    _clear_setup_owner(setup_task)
                raise
            else:
                if run.standby_setup_task is setup_task:
                    run.standby_setup_task = None
                run.standby_cancel = _cancel_setup_container
                run.standby_runtime_exited = _setup_runtime_exited
                run.standby_wait_runtime_exit = _wait_setup_exit

        # re-open the persisted shared graph (verified facts / dead-ends / flag).
        shared_graph = None
        try:
            graph_dir.mkdir(parents=True, exist_ok=True)
            shared_graph = SQLiteSharedGraph.open(
                db_path=graph_dir / "shared_graph.db", challenge=challenge,
                artifacts=arts)
        except Exception:
            shared_graph = None

        canonical_graph_flags: list[str] = []
        graph_flags_authoritative = False
        if shared_graph is not None:
            try:
                canonical_graph_flags = list(shared_graph.snapshot().flags)
                graph_flags_authoritative = any(
                    row.get("kind") in {"flag_found", "flag_invalidated"}
                    for row in shared_graph.events()
                )
            except Exception:
                canonical_graph_flags = []
                graph_flags_authoritative = False
        stored_flag = (
            run.flag
            or (canonical_graph_flags[0]
                if graph_flags_authoritative and canonical_graph_flags else "")
            or winner.get("flag")
            or ""
        )

        def _flag_from_operator_cmd() -> str:
            explicit = str(runtime_cmd.get("flag") or "").strip()
            if explicit:
                return explicit
            raw = str(runtime_cmd.get("text") or "").strip()
            if not raw:
                return ""
            m = re.search(r"[A-Za-z0-9_]{0,15}\{[^}]{1,200}\}", raw)
            if m:
                return m.group(0)
            # Allows advanced/API callers to pass a bare token as the command text.
            return raw if " " not in raw and len(raw) <= 240 else ""

        flag = (_flag_from_operator_cmd() if action == "mark_false" else "") or stored_flag
        # Multi-flag: the flags already collected, minus the one
        # the operator is marking false — so a mark_false re-solve worker is seeded
        # with the SURVIVING flags and re-finds only the missing one, not the rest.
        prior_flags = list(
            canonical_graph_flags
            if graph_flags_authoritative
            else run.flags
            if (run.flags or run.invalidated_flags)
            else winner.get("flags")
            or ([stored_flag] if stored_flag else [])
        )
        if action == "mark_false":
            prior_flags = [f for f in prior_flags if f != flag]

        async def _emit_bb(kind: str, **fields: Any) -> None:
            from muteki.core.events import (
                Event, EventType, blackboard_delta_payload)
            await run.bus.emit(Event(
                event_type=EventType.BLACKBOARD_DELTA, run_id=run.run_id,
                challenge_id=challenge.id,
                payload=blackboard_delta_payload(kind, actor="operator", **fields)))

        # mark_false: re-open the solve BEFORE the worker runs, so the board shows a
        # dead-end + reopened intents (fact-graph + blackboard grow the dead-end
        # node), and the rail flips back to running (RUN_REOPENED).
        if (action == "mark_false" and not mark_false_already_applied
                and shared_graph is not None and flag):
            try:
                info = shared_graph.reopen_after_false_positive(
                    actor="operator", flag=flag)
                await _emit_bb("dead_end", reason=info["dead_end_reason"])
                for iid in info.get("reopened", []):
                    await _emit_bb("intent_reopened", intent_id=iid)
                await _emit_bb("flag_invalidated", flag=flag)
                from muteki.core.events import Event, EventType
                # tell the rail this run is solving again (status → running)
                await run.bus.emit(Event(
                    event_type=EventType.RUN_REOPENED, run_id=run.run_id,
                    challenge_id=challenge.id, payload={"flag": flag}))
            except Exception:
                pass

        workdir = ""
        workdir_rel = str(winner.get("workdir_rel") or "").strip()
        if workdir_rel:
            try:
                candidate = (root / workdir_rel).resolve()
                candidate.relative_to(worker_root.resolve())
                if candidate.exists():
                    workdir = str(candidate)
            except (OSError, ValueError):
                workdir = ""
        if not workdir:
            workdir = str(worker_root / f"standby-{transport}")
        Path(workdir).mkdir(parents=True, exist_ok=True)
        if container is not None:
            from muteki.solver.container_exec import _chown_tree_to_worker
            _chown_tree_to_worker(workdir)
        solver_label = f"cli-{transport}-standby"
        home_label = ""
        agent_state_rel = str(winner.get("agent_state_rel") or "").strip()
        if agent_state_rel:
            try:
                state_root = (root / ".muteki-agent-state").resolve()
                state_dir = (root / agent_state_rel).resolve()
                state_dir.relative_to(state_root)
                if state_dir.is_dir():
                    home_label = state_dir.name
            except (OSError, ValueError):
                home_label = ""
        if not home_label:
            home_label = _standby_home_label(
                root, transport, str(winner.get("session") or ""))
        worker_env = _standby_worker_env(
            root=root,
            label=home_label,
            engine=transport,
            profile=profile,
            account_root=account_root,
            container=container,
        )

        session_supervisor = WorkerSessionSupervisor(
            run_id=run.run_id,
            execution_generation=max(
                1, int(getattr(run, "execution_generation", 1) or 1)),
            shared_graph=shared_graph,
        )
        worker = CliSolver(
            None, challenge, bus=run.bus, cost=run.cost, artifacts=arts,
            config=SolverConfig(), run_id=run.run_id, shared_graph=shared_graph,
            engine=transport,
            driver=driver_for(profile or transport),
            workdir=workdir,
            web_access=True, kb=False,
            mode="respond",
            resume_session=winner.get("session") or None,
            hitl_cmd={**runtime_cmd, "flag": flag},
            found_flags=prior_flags,
            solver_label=solver_label,
            container=container,
            worker_env=worker_env,
            identity=worker_identity_fields(profile),
            target_epoch=max(
                1, int(getattr(run, "execution_generation", 1) or 1)),
            session_supervisor=session_supervisor,
            worker_profile=profile or {"engine": transport},
        )
        worker._control_secret_values = list(materialized_secret_values)
        if context_reservations and control_journal is not None:
            worker._pending_control_context_reservations = context_reservations
            worker._context_committer = control_journal.commit_context_binding
            worker._context_releaser = control_journal.release_context_reservation
            worker._context_delivery_unknown_marker = (
                control_journal.mark_context_delivery_unknown)
            worker._context_binding_worker_id = context_owner
        delivery_ack = cmd.get("_standby_delivery_ack")
        delivery_loop = asyncio.get_running_loop()

        def _confirm_delivery(ok: bool) -> None:
            if not isinstance(delivery_ack, asyncio.Future):
                return

            def _set() -> None:
                if not delivery_ack.done():
                    delivery_ack.set_result(bool(ok))

            delivery_loop.call_soon_threadsafe(_set)

        worker._context_delivery_callback = _confirm_delivery
        # Publish the REAL worker cancellation boundary before awaiting it.  A
        # RunManager STOP can then kill the shelled CLI process tree first instead
        # of merely cancelling this Python coroutine and leaking the child.
        worker_cancel = getattr(worker, "cancel", None)
        worker_runtime_exited = getattr(worker, "runtime_exit_confirmed", None)
        worker_wait_runtime_exit = getattr(worker, "wait_runtime_exit", None)

        async def _cancel_runtime_owner() -> None:
            if callable(worker_cancel):
                result = worker_cancel()
                if inspect.isawaitable(result):
                    await result
            if callable(setup_cancel_boundary):
                result = setup_cancel_boundary()
                if inspect.isawaitable(result):
                    await result

        def _runtime_owner_exited() -> bool:
            worker_done = True
            if callable(worker_runtime_exited):
                try:
                    worker_done = bool(worker_runtime_exited())
                except Exception:
                    worker_done = False
            container_done = True
            if callable(setup_exit_query):
                try:
                    container_done = bool(setup_exit_query())
                except Exception:
                    container_done = False
            return worker_done and container_done

        async def _wait_runtime_owner_exit(timeout=None) -> bool:
            loop = asyncio.get_running_loop()
            deadline = None if timeout is None else (
                loop.time() + max(0.0, float(timeout)))
            if callable(worker_wait_runtime_exit):
                remaining = None if deadline is None else max(
                    0.0, deadline - loop.time())
                result = worker_wait_runtime_exit(remaining)
                worker_done = bool(
                    await result if inspect.isawaitable(result) else result)
                if not worker_done:
                    return False
            if callable(setup_cancel_boundary) and not bool(setup_exit_query()):
                try:
                    result = setup_cancel_boundary()
                    if inspect.isawaitable(result):
                        await result
                except Exception:
                    return False
            return _runtime_owner_exited()

        run.standby_cancel = _cancel_runtime_owner
        run.standby_runtime_exited = _runtime_owner_exited
        run.standby_wait_runtime_exit = _wait_runtime_owner_exit

        def _clear_runtime_registration() -> None:
            if shared_graph is not None:
                try:
                    shared_graph.close()
                except Exception:
                    pass
            if run.standby_cancel is _cancel_runtime_owner:
                run.standby_cancel = None
            if run.standby_runtime_exited is _runtime_owner_exited:
                run.standby_runtime_exited = None
            if run.standby_wait_runtime_exit is _wait_runtime_owner_exit:
                run.standby_wait_runtime_exit = None
            try:
                current = asyncio.current_task()
            except RuntimeError:
                current = None
            if run.standby_runtime_cleanup_task is current:
                run.standby_runtime_cleanup_task = None

        async def _reap_runtime_until_exit() -> None:
            """Keep the real kill boundary alive after the wrapper task exits.

            A PARTIAL control receipt is an audit fact, not permission to orphan the
            child. Re-signal and poll until CliSolver proves every runner/process is
            gone; later STOP/FORCE_CANCEL commands can use the same retained callbacks.
            """
            confirmed = False
            try:
                while not confirmed:
                    try:
                        await _cancel_runtime_owner()
                        confirmed = await _wait_runtime_owner_exit(0.5)
                        if not confirmed:
                            await asyncio.sleep(0.1)
                    except asyncio.CancelledError:
                        raise
                    except Exception:
                        # A transient signal/poll failure must not abandon the only
                        # remaining process handle. Keep this watcher and retry.
                        await asyncio.sleep(0.1)
            except asyncio.CancelledError:
                # Preserve the callbacks when server shutdown interrupts the watcher;
                # clearing them would turn an unproved runtime into a fake clean exit.
                raise
            finally:
                if confirmed:
                    _clear_runtime_registration()
        try:
            out = await worker.run()
            # writeup: persist the body to sessions/{id}/writeup.md (and it already
            # streamed to the chat as the worker's reply).
            artifact_path = ""
            if action == "writeup" and getattr(out, "reply", ""):
                try:
                    writeup_path = root / "writeup.md"
                    writeup_path.write_text(out.reply)
                    artifact_path = str(writeup_path)
                except Exception as exc:
                    raise RuntimeError("writeup artifact could not be persisted") from exc
            if action in {"ask", "writeup"}:
                from muteki.core.events import Event, EventType
                await run.bus.emit(Event(
                    event_type=EventType.FOLLOWUP_COMPLETED,
                    run_id=run.run_id,
                    solver_id=solver_label,
                    payload={
                        "followup_id": runtime_cmd.get("followup_id") or "",
                        "kind": action,
                        "text": getattr(out, "reply", "") or "",
                        "artifact_path": artifact_path,
                    },
                ))
            # A successful false-positive re-solve becomes the next trusted
            # continuation owner. Persist identifiers in coordinator-only storage;
            # the workspace JSON remains a compatibility artifact.
            if action == "mark_false" and out.solved and out.flag:
                refound = list(getattr(out, "flags", None) or [out.flag])
                run.merge_flags(refound)
                try:
                    persisted = {
                        "engine": out.engine, "worker_id": solver_label,
                        "session": out.session,
                        "workdir": out.workdir, "flag": run.flag,
                        "flags": list(run.flags),
                        "challenge": challenge.model_dump(),
                        "profile_id": str(
                            (profile or {}).get("id")
                            or (profile or {}).get("name")
                            or ""
                        ),
                        "backend": backend,
                    }
                    mgr.persist_winner_continuation(run.run_id, persisted)
                    (root / "winner.json").write_text(json.dumps({
                        key: persisted[key]
                        for key in (
                            "engine", "worker_id", "session", "workdir",
                            "flag", "flags", "challenge", "profile_id",
                        )
                    }, ensure_ascii=False, indent=2))
                except Exception:
                    pass
        finally:
            # A secure transport can reject before any process boundary (for
            # example Cursor exact-secret delivery is intentionally unsupported).
            # Return those reservations immediately so the same context remains
            # retryable; only a crossed/uncertain start may consume or strand them.
            if (context_reservations and control_journal is not None
                    and not bool(getattr(worker, "_runtime_process_started", False))
                    and not bool(getattr(
                        worker, "_control_context_delivery_committed", False))
                    and not bool(getattr(
                        worker, "_control_context_delivery_unknown", False))):
                released: set[tuple[str, str]] = set()
                for context_id, reservation_id in context_reservations:
                    try:
                        if control_journal.release_context_reservation(
                            str(context_id), worker_id=context_owner,
                            reservation_id=str(reservation_id),
                        ):
                            released.add((str(context_id), str(reservation_id)))
                    except Exception:
                        pass
                lock = getattr(worker, "_context_delivery_lock", None)
                if lock is not None:
                    with lock:
                        worker._pending_control_context_reservations = [
                            item for item in
                            worker._pending_control_context_reservations
                            if (str(item[0]), str(item[1])) not in released
                        ]
                worker._notify_context_delivery(False)
            # Always cross the worker boundary, including normal completion: a
            # cancelled asyncio.to_thread await may leave its runner thread alive
            # briefly, and CliSolver retains the process handles specifically so a
            # final idempotent cancel can still reap them.
            try:
                await _cancel_runtime_owner()
            except Exception:
                pass
            runtime_confirmed = _runtime_owner_exited()
            if runtime_confirmed:
                _clear_runtime_registration()
            else:
                cleanup_task = asyncio.create_task(
                    _reap_runtime_until_exit(),
                    name=f"standby-runtime-reap:{run.run_id}",
                )
                run.standby_runtime_cleanup_task = cleanup_task

    return drive
