"""Run drivers — turn a /start request body into a coroutine that emits onto the
run's bus. Keeps the HTTP layer (server.py) ignorant of solving internals.

Kinds:
  - "swarm" (DEFAULT): races the REAL solver swarm (shelled claude+codex CLI
    executor) against a challenge spec. Needs a live target (URL in the prompt /
    challenge.target) and the claude (and optionally codex) CLI on PATH — no
    DeepSeek key (the CLI executor doesn't use the code-driven kernel).
  - "mock": scripts the canned event stream (no model, no target) — UI dev / e2e
    ONLY. Must be asked for explicitly (kind:"mock"); it is no longer the default.
"""

from __future__ import annotations

import copy
import re
import os
from pathlib import Path
from typing import TYPE_CHECKING, Any, Awaitable, Callable

from apps.web.run_manager import Run, RunManager
from apps.web.worker_config import (
    DEFAULT_WORKER_BACKEND,
    backend_for_profile,
    resolve_worker_backend,
)
from muteki.solver.credential_accounts import account_store_root
from muteki.core.runtime_env import is_web_container
from muteki.solver.worker_profiles import (
    base_engine_for_profile,
    normalize_profile_roster,
    profile_uses_endpoint,
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
    runtime_profiles: list[dict],
    sessions_root: Path,
) -> list[str]:
    """Dispatch precheck — now a thin wrapper over the profile_health kernel so it
    can never disagree with the settings self-check. Same two-pass cost profile:
    the kernel does cheap binding inline and only fires the slow CLI hello when a
    profile `needs_auth_probe`; we fan out across profiles so the dispatch path
    pays max(timeout), not sum(timeout) (the "/start freezes" symptom)."""
    from concurrent.futures import ThreadPoolExecutor

    from muteki.solver.profile_health import evaluate_profile_health

    enabled = [
        p for p in worker_profiles if isinstance(p, dict) and p.get("enabled", True)
    ]
    if not enabled:
        return []

    def _ev(p: dict) -> "tuple[dict, ProfileHealth]":
        backend = backend_for_profile(
            p,
            runtime_profiles=runtime_profiles,
            worker_backend=DEFAULT_WORKER_BACKEND,
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


def _drop_cursor_profiles(engines: list[str], worker_profiles: list[dict]) -> list[str]:
    if not worker_profiles:
        return [e for e in engines if e != "cursor"]
    by_name = {str(p.get("name") or p.get("id")): p for p in worker_profiles if isinstance(p, dict)}
    return [
        name for name in normalize_profile_roster(engines, worker_profiles)
        if base_engine_for_profile(by_name.get(name) or name) != "cursor"
    ]


def build_driver(body: dict[str, Any], mgr: RunManager | None = None) -> Driver:
    # Real solving is the DEFAULT now — the deck launches the CLI executor swarm.
    # "mock" is opt-in (UI dev / e2e only).
    kind = (body or {}).get("kind", "swarm")
    if kind == "mock":
        return _mock_driver(body)
    if kind == "idle":
        return _idle_driver(body)
    return _swarm_driver(_infer_challenge(body), mgr=mgr)


# ---- conversational dispatch ------------------------------------------------
# The conversation-first deck lets the operator DESCRIBE a challenge in prose
# instead of filling a form: "Flag's behind layers of encoding at
# http://host/secret". The swarm infers category/target/name from that prompt.
# This is a deliberately small heuristic — the real planner refines it; this just
# seeds the Challenge so a run can start from one sentence.

_CATEGORY_HINTS: list[tuple[str, tuple[str, ...]]] = [
    ("crypto", ("rsa", "aes", "cipher", "encrypt", "decrypt", "xor", "crypto", "modulus", "ecc")),
    ("pwn", ("overflow", "ret2", "rop", "shellcode", "pwn", "gets(", "libc", "canary", "heap")),
    ("reverse", ("reverse", "disassemble", "binary", "decompile", "ghidra", "ida", "rev", ".exe", "elf")),
    ("forensics", ("pcap", "wireshark", "memory dump", "stego", "forensic", "carve", "volatility")),
    ("web", ("http", "https", "url", "cookie", "jwt", "sqli", "xss", "endpoint", "/admin", "/secret", "web")),
]

_DEFAULT_BRACE_FLAG_FORMAT = r"[A-Za-z0-9_]{0,15}\{[^}]{1,200}\}"


def _clean_flag_wrapper(raw: Any) -> str:
    wrapper = str(raw or "").strip()
    if not wrapper:
        return ""
    return "".join(wrapper.split())[:80]


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
    if raw_format == "token":
        return "token", hint, ""

    cleaned_wrapper = _clean_flag_wrapper(wrapper)
    if cleaned_wrapper:
        flag_format = str(raw_format) if raw_format and raw_format not in ("brace", "custom") else _DEFAULT_BRACE_FLAG_FORMAT
        return flag_format, cleaned_wrapper, cleaned_wrapper

    if raw_format in ("", "brace", "custom"):
        return _DEFAULT_BRACE_FLAG_FORMAT, hint, ""
    return str(raw_format), hint, ""


def _infer_challenge(body: dict[str, Any]) -> dict[str, Any]:
    """Fill a `challenge` block from a conversational `prompt` when the caller
    didn't pass structured fields. Caller-provided fields always win."""
    body = dict(body or {})
    ch = dict(body.get("challenge") or {})
    prompt = (body.get("prompt") or ch.get("description") or "").strip()
    if not prompt:
        body["challenge"] = ch
        return body
    low = prompt.lower()

    if not ch.get("description"):
        ch["description"] = prompt
    if not ch.get("category"):
        ch["category"] = next(
            (cat for cat, kws in _CATEGORY_HINTS if any(k in low for k in kws)),
            "misc",
        )
    if not ch.get("target"):
        m = re.search(r"https?://[^\s\"'<>]+", prompt)
        if m:
            ch["target"] = m.group(0).rstrip(".,;)")
    if not ch.get("name"):
        # first few words, slugified — a readable thread-rail label
        words = re.findall(r"[A-Za-z0-9]+", prompt)[:4]
        ch["name"] = "-".join(w.lower() for w in words) or "challenge"
    body["challenge"] = ch
    return body


def _idle_driver(body: dict[str, Any]) -> Driver:
    """Keeps a run's bus open without solving — used to drive HITL/manual flows
    (and as a smoke target). Stays alive until cancelled."""
    async def drive(run: Run) -> None:
        import asyncio

        while True:
            await asyncio.sleep(3600)

    return drive


def _mock_driver(body: dict[str, Any]) -> Driver:
    async def drive(run: Run) -> None:
        from examples.mock_solver import run_mock_solve

        # pace the canned stream so the evolving graph + chat animate in the
        # browser and a human has a window to inject HITL commands mid-run.
        tick = float(body.get("tick", 0.6))
        # optional multi-flag demo: body.expected_flags (or challenge.expected_flags)
        ef = int(body.get("expected_flags")
                 or (body.get("challenge") or {}).get("expected_flags") or 1)
        await run_mock_solve(run.bus, run.cost, run_id=run.run_id, tick=tick,
                             expected_flags=ef)

    return drive


def _swarm_driver(body: dict[str, Any], mgr: RunManager | None = None) -> Driver:
    """The REAL solver: a shelled-CLI swarm (claude + codex race) against the
    challenge. No DeepSeek key — CliSolver runs the subscription CLIs directly and
    still gates every flag through the real provenance check.

    Knobs from the request body (all optional):
      challenge.{name,category,target,description,flag_format}  (inferred from prompt)
      cli_race: bool (default True)           — race claude + codex
      cli_engine: "claude" | "codex"          — single engine when not racing
      race_scout: bool (default True)         — one parallel single-shot recon round
                                                in front of the main coordinator loop
                                                (fast path on flag, else hands facts
                                                to the coordinator loop)
      race_engines: list (default = engines)  — which engines race (worker switch)
      race_timeout: int (default 720)         — short per-worker recon timeout (s)
      offline: bool (default False)           — deny worker web tools (clean eval);
                                                also denies the KB unless `kb` is set
      kb: bool (default: True online / False offline) — let the worker query the KB
      n_solvers: int (default 2)              — bootstrap lineup size
      engines: list[str] (default [cursor,claude,codex]) — engine roster; offline
                                                drops cursor (can't go offline cleanly)
      start_workers: int (default len(engines)) — bootstrap workers (one per engine)
    """
    async def drive(run: Run) -> None:
        import os
        import tempfile
        from pathlib import Path

        from muteki.models.solve_graph import Challenge
        from muteki.sandbox.manager import SandboxManager
        from muteki.solver.result import ArtifactStore
        from muteki.solver.types import SolverConfig
        from muteki.swarm.models import default_lineup
        from muteki.swarm.swarm import Swarm

        ch = body.get("challenge", {})
        # attachments: local file paths for FILE-based tracks (crypto/rev/forensics
        # /misc). The worker stages them into its cwd. Keep only paths that exist so
        # a stray entry can't crash the run.
        attachments = [a for a in (ch.get("attachments") or []) if Path(a).exists()]
        # engagement mode: "ctf" (default, flag-driven) or "pentest" (goal-driven —
        # find + prove vulnerabilities in scope). Body may carry it at top level or
        # under challenge.* ; default keeps every CTF dispatch byte-identical.
        mode = (ch.get("mode") or body.get("mode") or "ctf")
        if mode not in ("ctf", "pentest"):
            mode = "ctf"
        # multi-flag: thread expected_flags + multi_flag so a ladder/collection
        # challenge SAVES every flag without finishing on the first (run-10070's
        # 22-level ladder otherwise registered as single-flag). multi_flag is the
        # mode bit; expected_flags is the optional count (<=1 in multi-flag mode →
        # collect until operator STOP / no-progress pause). body.* wins over ch.*.
        expected_flags = int(body.get("expected_flags")
                             or ch.get("expected_flags") or 1)
        multi_flag = bool(body.get("multi_flag")
                          if body.get("multi_flag") is not None
                          else ch.get("multi_flag", False))
        flag_format, flag_format_hint, flag_format_wrapper = _flag_format_fields(ch, body)
        challenge = Challenge(
            id=run.run_id,
            name=ch.get("name", run.run_id),
            category=ch.get("category", "web"),
            points=ch.get("points", 0),
            description=ch.get("description", ""),
            target=ch.get("target"),
            attachments=attachments,
            flag_format=flag_format,
            flag_format_hint=flag_format_hint,
            flag_format_wrapper=flag_format_wrapper,
            expected_flags=max(1, expected_flags),
            multi_flag=multi_flag,
            verifier_rate_limited=bool(body.get("verifier_rate_limited")
                                       if body.get("verifier_rate_limited") is not None
                                       else ch.get("verifier_rate_limited", False)),
            mode=mode,
            goal=(ch.get("goal") or body.get("goal") or ""),
            scope=(ch.get("scope") or body.get("scope") or ""),
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
        # OFFLINE drops cursor — Cursor's headless CLI has no --disallowed-tools to
        # deny web tools and doesn't inherit the optional KB MCP, so it can't run a
        # clean offline bench eval (protects the AGENTS.md offline rule).
        wc = mgr.worker_config.resolve(challenge.category) if mgr is not None else {}
        engines = body.get("engines") or wc.get("engines") or ["cursor", "claude", "codex"]
        runtime_profiles = body.get("runtime_profiles") or wc.get("runtime_profiles") or []
        worker_profiles = body.get("worker_profiles") or wc.get("worker_profiles") or []
        # OFFLINE normally drops cursor (no --disallowed-tools to deny web tools →
        # can't guarantee a clean black-box). `allow_cursor_offline:true` overrides
        # that — the operator accepts that cursor MIGHT web-search a writeup, and the
        # anti-cheat audit (scripts/audit_retest.py) is relied on to flag any case
        # where a flag comes from a fetched writeup rather than a real exploit.
        allow_cursor_offline = bool(body.get("allow_cursor_offline", False))
        if offline and not allow_cursor_offline:
            engines = _drop_cursor_profiles(engines, worker_profiles) or ["claude", "codex"]
        if offline:
            runtime_profiles = [
                {**r, "network": "none"} if isinstance(r, dict)
                and str(r.get("backend") or "") == "container" else r
                for r in runtime_profiles
            ]
            endpoint_profiles = [
                p for p in _selected_profiles(engines, worker_profiles)
                if profile_uses_endpoint(p)
            ]
            if endpoint_profiles:
                names = ", ".join(str(p.get("name") or p.get("id")) for p in endpoint_profiles)
                raise RuntimeError(
                    "profile_incompatible offline eval cannot use custom endpoint profile(s): "
                    + names
                )
        # bootstrap worker count: explicit body wins, else the config default, else
        # one per engine (heterogeneous rush). max_workers likewise from config.
        default_sw = wc.get("start_workers") or len(engines)
        start_workers = int(body.get("start_workers", default_sw))
        max_workers = int(body.get("max_workers", wc.get("max_workers", 10)))
        # wall-clock cap. ABSENT → the Swarm default (infinite: the interactive deck
        # never gives up on its own; only solve / operator-stop ends it). A batch
        # eval, which is unattended, MUST pass a finite budget so a hard challenge
        # can't run forever. `0`/None/negative are treated as "no cap" too.
        _wcb = body.get("wall_clock_budget", wc.get("wall_clock_budget") if wc else None)
        wall_clock_budget = float(_wcb) if (_wcb and float(_wcb) > 0) else float("inf")
        max_total_workers = int(body.get("max_total_workers", wc.get("max_total_workers", 0)) or 0) or None
        cost_budget_usd = float(body.get("cost_budget_usd", wc.get("cost_budget_usd", 0.0)) or 0.0) or None
        llm_profiles = body.get("llm_profiles") or wc.get("llm_profiles") or {}
        if "stage_policy" in body:
            stage_policy = copy.deepcopy(body.get("stage_policy") or {})
        elif wc.get("stage_policy"):
            stage_policy = copy.deepcopy(wc["stage_policy"])
        else:
            stage_policy = {
                "race": {
                    "enabled": bool(body["race_scout"]) if "race_scout" in body else bool(wc.get("race_scout", True)),
                    "timeout": int(body.get("race_timeout", wc.get("race_timeout", 720))),
                    "engines": body.get("race_engines") or wc.get("race_engines") or [],
                },
                "coordinator": {"wall_clock_budget": 0 if wall_clock_budget == float("inf") else int(wall_clock_budget)},
                "budgets": {"max_total_workers": max_total_workers or 0,
                            "cost_budget_usd": cost_budget_usd or 0.0},
            }
        if "race_scout" in body:
            stage_policy.setdefault("race", {})["enabled"] = bool(body["race_scout"])
        if "race_timeout" in body:
            stage_policy.setdefault("race", {})["timeout"] = int(body["race_timeout"])
        if "race_engines" in body:
            stage_policy.setdefault("race", {})["engines"] = list(body.get("race_engines") or [])
        if "wall_clock_budget" in body:
            v = float(body["wall_clock_budget"] or 0)
            stage_policy.setdefault("coordinator", {})["wall_clock_budget"] = (
                int(v) if v > 0 else 0)
        if "max_total_workers" in body:
            stage_policy.setdefault("budgets", {})["max_total_workers"] = int(
                body["max_total_workers"] or 0)
        if "cost_budget_usd" in body:
            stage_policy.setdefault("budgets", {})["cost_budget_usd"] = float(
                body["cost_budget_usd"] or 0.0)
        # race-scout layer (DESIGN_race_scout_layer.md): one parallel single-shot
        # round in front of the main coordinator loop. Operator-configurable from the request:
        #   race_scout (bool, default on) — whole-layer toggle
        #   race_engines (list, default = engines) — which engines race (worker switch)
        #   race_timeout (int, default 720s) — short per-worker recon timeout
        race_scout = bool(body["race_scout"]) if "race_scout" in body else bool(wc.get("race_scout", True))
        race_engines = body.get("race_engines") or wc.get("race_engines") or None  # None → defaults to the roster
        if offline and not allow_cursor_offline and race_engines:
            race_engines = _drop_cursor_profiles(race_engines, worker_profiles) or None
        race_timeout = int(body.get("race_timeout", wc.get("race_timeout", 720)))
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
            request_backend=body.get("worker_backend"),
            config_backend=wc.get("worker_backend"),
            env_backend=os.environ.get("MUTEKI_WORKER_BACKEND"),
            in_web_container=is_web_container(),
        )
        if mgr is not None and worker_profiles:
            # The precheck runs a real per-profile health probe (a synchronous
            # `subprocess.run` that shells the CLI for a one-turn hello) for any
            # profile that needs an account/endpoint. That can take seconds per
            # engine, so it MUST run off the event loop — otherwise a relaunch
            # (`/resolve`) freezes the whole single-threaded uvicorn loop while it
            # probes (the "resolve → backend hangs" symptom). to_thread it.
            import asyncio
            precheck_profiles = _selected_profiles(engines, worker_profiles) or worker_profiles
            missing_accounts = await asyncio.to_thread(
                _missing_profile_accounts,
                worker_profiles=precheck_profiles,
                runtime_profiles=runtime_profiles,
                sessions_root=mgr.sessions_root,
            )
            if missing_accounts:
                raise RuntimeError(
                    "profile_unhealthy missing credential account(s): "
                    + ", ".join(missing_accounts)
                )

        if mgr is not None:
            root = mgr.workspace_dir(run.run_id)
        else:
            root = Path(tempfile.mkdtemp(prefix="muteki-web-"))
        # sbx is the sandbox root — sandbox.shutdown_all() rmtree's it at run end,
        # so NOTHING durable may live under it. arts + graph are SIBLINGS of sbx so
        # they persist (the shared_graph.db is the run's queryable fact graph).
        sandbox = SandboxManager(bus=run.bus, root=root / "sbx")
        arts = ArtifactStore(root=root / "arts")
        graph_dir = root / "graph"
        # worker_root is a SIBLING of sbx (NOT under it) so each CLI worker's cwd —
        # staged attachments, agent-extracted files, PoCs — lives under the run's
        # sessions/{id}/workspace/ and survives sandbox.shutdown_all()'s rmtree of
        # sbx. It's cleaned up with the run (RunManager.delete drops sessions/{id}).
        worker_root = root / "workers"

        # LLMClient: the coordinator needs it for the Reason planner. A plain CLI
        # race needs none.
        llm_cm = None
        llm = None
        if coordinator:
            from muteki.core.llm import LLMClient
            try:
                # planner endpoint override (DESIGN §2.2): base_url from the
                # planner llm_profile (empty = default DeepSeek). Key stays in env.
                planner_base = str((llm_profiles.get("planner") or {}).get("base_url") or "").strip()
                llm_kwargs: dict[str, Any] = {"cost": run.cost, "bus": run.bus}
                if planner_base:
                    llm_kwargs["base_url"] = planner_base
                llm_cm = LLMClient(**llm_kwargs)
                llm = await llm_cm.__aenter__()
            except Exception:
                # no key / client unavailable → coordinator Reason will no-op,
                # bootstrap workers still run. Never block the run on this.
                llm_cm = None
                llm = None

        # §16 flywheel store (optional; recall prior + distill on solve)
        from muteki.learning.distill import TemplateStore
        knowledge = TemplateStore(root=os.environ.get("MUTEKI_KNOWLEDGE_DIR", "knowledge"))

        swarm = Swarm(
            challenge, default_lineup(n), llm=llm, sandbox=sandbox,
            bus=run.bus, cost=run.cost, artifacts=arts,
            config=SolverConfig(), run_id=run.run_id, knowledge=knowledge,
            hitl_inbox=run.hitl,  # HITL: human commands reach the solvers
            worker_cmds=run.worker_cmds,  # operator spawn/kill of specific engines
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
            runtime_profiles=runtime_profiles,
            worker_profiles=worker_profiles,
            credential_accounts_root=(
                account_store_root(mgr.sessions_root) if mgr is not None else None
            ),
        )
        try:
            out = await swarm.run()
            run.flag = out.flag
        finally:
            await sandbox.shutdown_all()
            if llm_cm is not None:
                await llm_cm.__aexit__(None, None, None)

    return drive


# ---- standby (post-solve HITL) ----------------------------------------------
# After a run finishes (or the server restarted), a human follow-up no longer has
# a live swarm to reach. The standby driver COLD-STARTS a single worker from disk:
# it reads winner.json (the winning worker's CLI session) + the persisted
# shared_graph, resumes that SAME session, and serves one command — answer a
# question, mark the flag a false-positive and keep solving, or write a writeup.
# Everything it needs is durable, so this works identically before and after a
# server restart. No winner.json (old run) → degrade to a fresh worker seeded with
# the board context.

def _standby_profile_for(engine: str, worker_profiles: list[dict[str, Any]]) -> dict[str, Any] | None:
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


def _runtime_for_profile(
    profile: dict[str, Any] | None,
    runtime_profiles: list[dict[str, Any]],
) -> dict[str, Any]:
    if not profile:
        return {}
    rid = str(profile.get("runtime") or "")
    for rt in runtime_profiles:
        if isinstance(rt, dict) and str(rt.get("id") or "") == rid:
            return rt
    return {}


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

    env = runtime_env_for_engine(
        engine,
        account_root=account_root,
        account_id=(profile.get("credential_account") if profile else None),
        container=container is not None,
    ).env
    if profile:
        env["MUTEKI_WORKER_PROFILE_ID"] = str(profile.get("id") or "")
        env["MUTEKI_CREDENTIAL_ACCOUNT_ID"] = str(profile.get("credential_account") or "")
        if profile.get("model"):
            env["MUTEKI_WORKER_MODEL"] = str(profile["model"])
    if container is not None:
        from muteki.swarm.swarm import _ensure_blackboard_skill_links
        from muteki.solver.container_exec import _chown_tree_to_worker
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
        import json
        from pathlib import Path

        from muteki.models.solve_graph import Challenge
        from muteki.solver.cli_driver import driver_for
        from muteki.solver.cli_solver import CliSolver
        from muteki.solver.credential_accounts import account_store_root
        from muteki.solver.result import ArtifactStore
        from muteki.solver.types import SolverConfig
        from muteki.swarm.shared_graph import SQLiteSharedGraph

        action = (cmd.get("action") or "ask").lower()

        if mgr is not None:
            root = mgr.workspace_dir(run.run_id)
        else:
            return  # no workspace → nothing durable to resume from

        graph_dir = root / "graph"
        winner_path = root / "winner.json"
        arts = ArtifactStore(root=root / "arts")
        worker_root = root / "workers"
        worker_root.mkdir(parents=True, exist_ok=True)

        winner: dict[str, Any] = {}
        if winner_path.exists():
            try:
                winner = json.loads(winner_path.read_text())
            except Exception:
                winner = {}

        # Rebuild the Challenge: prefer the snapshot stored in winner.json. Older
        # runs may not have winner.json, so recover the original launch payload from
        # the durable JSONL before degrading to rail metadata.
        ch = winner.get("challenge") or {}
        if not ch:
            try:
                from muteki.core.events import EventType
                async for ev in run.store.replay(run.run_id):
                    if ev.event_type == EventType.RUN_STARTED:
                        ch = (ev.payload or {}).get("challenge") or {}
                        if ch:
                            break
            except Exception:
                ch = {}
        challenge = Challenge(
            id=run.run_id,
            name=ch.get("name", run.name or run.run_id),
            category=ch.get("category", run.category or "web"),
            points=ch.get("points", 0),
            description=ch.get("description", ""),
            target=ch.get("target"),
            attachments=[],
            flag_format=ch.get("flag_format", _DEFAULT_BRACE_FLAG_FORMAT),
            flag_format_hint=ch.get("flag_format_hint", ""),
            flag_format_wrapper=ch.get("flag_format_wrapper", ""),
            # carry the run's flag mode across a post-solve standby re-solve so a
            # mark_false/resolve doesn't silently revert a collection run to single
            # flag (review #15). winner.json persists these in the challenge block.
            expected_flags=int(ch.get("expected_flags") or 1),
            multi_flag=bool(ch.get("multi_flag", False)),
            verifier_rate_limited=bool(ch.get("verifier_rate_limited", False)),
        )

        wc = mgr.worker_config.resolve(challenge.category) if mgr is not None else {}
        runtime_profiles = wc.get("runtime_profiles") or []
        worker_profiles = wc.get("worker_profiles") or []
        winner_engine = str(winner.get("engine") or "claude")
        profile = _standby_profile_for(winner_engine, worker_profiles)
        transport = base_engine_for_profile(profile or winner_engine)
        worker_backend = resolve_worker_backend(
            request_backend=None,
            config_backend=wc.get("worker_backend"),
            env_backend=os.environ.get("MUTEKI_WORKER_BACKEND"),
            in_web_container=is_web_container(),
        )
        backend = (
            backend_for_profile(
                profile,
                runtime_profiles=runtime_profiles,
                worker_backend=worker_backend,
                in_web_container=is_web_container(),
            )
            if profile else worker_backend
        )
        runtime = _runtime_for_profile(profile, runtime_profiles)
        container = None
        account_root = account_store_root(mgr.sessions_root) if mgr is not None else None
        if backend == "container":
            from muteki.solver.container_exec import ensure_container
            container = await asyncio.to_thread(
                ensure_container,
                run.run_id,
                str(root),
                network=str(runtime.get("network") or "bridge"),
                memory=str(runtime.get("memory") or "") or None,
                cpus=str(runtime.get("cpus") or "") or None,
                pids_limit=int(runtime.get("pids_limit") or 0) or None,
                account_root=(str(account_root) if account_root is not None else None),
            )

        # re-open the persisted shared graph (verified facts / dead-ends / flag).
        shared_graph = None
        try:
            graph_dir.mkdir(parents=True, exist_ok=True)
            shared_graph = SQLiteSharedGraph.open(
                db_path=graph_dir / "shared_graph.db", challenge=challenge,
                artifacts=arts)
        except Exception:
            shared_graph = None

        stored_flag = winner.get("flag") or run.flag or ""

        def _flag_from_operator_cmd() -> str:
            explicit = str(cmd.get("flag") or "").strip()
            if explicit:
                return explicit
            raw = str(cmd.get("text") or "").strip()
            if not raw:
                return ""
            m = re.search(r"[A-Za-z0-9_]{0,15}\{[^}]{1,200}\}", raw)
            if m:
                return m.group(0)
            # Allows advanced/API callers to pass a bare token as the command text.
            return raw if " " not in raw and len(raw) <= 240 else ""

        flag = (_flag_from_operator_cmd() if action == "mark_false" else "") or stored_flag
        # multi-flag: the flags already collected (from winner.json), minus the one
        # the operator is marking false — so a mark_false re-solve worker is seeded
        # with the SURVIVING flags and re-finds only the missing one, not the rest.
        prior_flags = list(winner.get("flags") or run.flags or ([stored_flag] if stored_flag else []))
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
        if action == "mark_false" and shared_graph is not None and flag:
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

        workdir = str(winner.get("workdir") or "")
        if not workdir or not Path(workdir).exists():
            workdir = str(worker_root / f"standby-{transport}")
        Path(workdir).mkdir(parents=True, exist_ok=True)
        if container is not None:
            from muteki.solver.container_exec import _chown_tree_to_worker
            _chown_tree_to_worker(workdir)
        solver_label = f"cli-{transport}-standby"
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

        worker = CliSolver(
            None, challenge, bus=run.bus, cost=run.cost, artifacts=arts,
            config=SolverConfig(), run_id=run.run_id, shared_graph=shared_graph,
            engine=transport,
            driver=driver_for(profile or transport),
            workdir=workdir,
            web_access=True, kb=False,
            mode="respond",
            resume_session=winner.get("session") or None,
            hitl_cmd={**cmd, "flag": flag},
            found_flags=prior_flags,
            solver_label=solver_label,
            container=container,
            worker_env=worker_env,
        )
        try:
            out = await worker.run()
            # writeup: persist the body to sessions/{id}/writeup.md (and it already
            # streamed to the chat as the worker's reply).
            if action == "writeup" and getattr(out, "reply", ""):
                try:
                    (root / "writeup.md").write_text(out.reply)
                except Exception:
                    pass
            # mark_false that re-solved: refresh winner.json + run flags. Multi-flag:
            # merge the re-found flag(s) into the run set (the invalidated one was
            # already removed via reopen_after_false_positive) and persist the full
            # list, mirroring Swarm._persist_winner.
            if action == "mark_false" and out.solved and out.flag:
                refound = list(getattr(out, "flags", None) or [out.flag])
                run.merge_flags(refound)
                try:
                    (root / "winner.json").write_text(json.dumps({
                        "engine": out.engine, "session": out.session,
                        "workdir": out.workdir, "flag": run.flag,
                        "flags": list(run.flags),
                        "challenge": challenge.model_dump(),
                    }, ensure_ascii=False, indent=2))
                except Exception:
                    pass
        finally:
            if shared_graph is not None:
                try:
                    shared_graph.close()
                except Exception:
                    pass
            if container is not None:
                try:
                    from muteki.solver.container_exec import teardown_container
                    await asyncio.to_thread(teardown_container, run.run_id, remove=True)
                except Exception:
                    pass

    return drive
