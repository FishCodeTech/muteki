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

import asyncio
import copy
import hashlib
import json
import re
import os
import sys
from pathlib import Path
from typing import TYPE_CHECKING, Any, Awaitable, Callable

from apps.web.dispatch_parse import parse_dispatch
from apps.web.llm_credentials import LlmCredentialStore
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
    """Preserve the actionable probe error after the probe redacts credentials."""
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
) -> tuple[dict[str, bool], list[dict[str, Any]]]:
    """Send one real minimal model request for every participating profile."""
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


# Allowlisted module prefixes for the optional `swarm_class` /start knob. The
# endpoint loads a class by dotted path, so without this list it would be an
# arbitrary-import (RCE) surface.
_SWARM_CLASS_PREFIXES: tuple[str, ...] = (
    "muteki.swarm.",
    "muteki.solver.",
    "muteki.frameworks.",
)


def _resolve_swarm_class(spec: Any) -> type:
    """Resolve the optional `swarm_class` body knob (``module.path:ClassName``).

    Product default is ``muteki.swarm.swarm.Swarm`` (coordinator_loop).
    Empty / omitted spec must stay on that class. Experimental arms
    (ChainForce, PEX, DualRush, ReapClose, F01–F11) load only when a
    caller writes an explicit allowlisted spec. Web UI does not send
    this field. See AGENTS.md.
    """
    from muteki.swarm.swarm import Swarm

    text = str(spec or "").strip()
    if not text:
        return Swarm
    module_name, sep, class_name = text.partition(":")
    if not sep or not module_name or not class_name:
        raise RuntimeError(
            f"swarm_class must be 'module.path:ClassName', got {text!r}")
    if not module_name.startswith(_SWARM_CLASS_PREFIXES):
        raise RuntimeError(
            f"swarm_class module {module_name!r} not in allowlist "
            f"{_SWARM_CLASS_PREFIXES}")
    import importlib

    cls = getattr(importlib.import_module(module_name), class_name, None)
    if not (isinstance(cls, type) and issubclass(cls, Swarm)):
        raise RuntimeError(
            f"swarm_class {text!r} is not a muteki.swarm.swarm.Swarm subclass")
    return cls


def build_driver(body: dict[str, Any], mgr: RunManager | None = None) -> Driver:
    # Real solving is the DEFAULT now — the deck launches the CLI executor swarm.
    # "mock" is opt-in (UI dev / e2e only).
    kind = (body or {}).get("kind", "swarm")
    if kind == "mock":
        return _mock_driver(body)
    if kind == "pentest_demo":
        return _pentest_demo_driver(body, mgr=mgr)
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

# Stop inferred URLs at fullwidth / CJK wrappers. ASCII rstrip alone leaves
# `http://127.0.0.1:4280）做黑盒渗透。账号` which later blows up urllib port parse.
_INFERRED_URL_CUT = re.compile(r"[）】」』，。；、（【「『<>\"']")
_INFERRED_URL_KEEP = re.compile(
    r"(https?://(?:\[[0-9A-Fa-f:]+\]|[^/:?#\s]+)"
    r"(?::\d{1,5})?(?:[/?#][^\s]*)?)"
)


def _inferred_http_target(prompt: str) -> str:
    m = re.search(r"https?://[^\s\"'<>]+", prompt or "")
    if not m:
        return ""
    raw = m.group(0)
    cut = _INFERRED_URL_CUT.search(raw)
    if cut:
        raw = raw[:cut.start()]
    raw = raw.rstrip(".,;)/]\\")
    kept = _INFERRED_URL_KEEP.match(raw)
    return (kept.group(1) if kept else raw).rstrip(".,;)/]\\")


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

    inferred: list[str] = []
    if not ch.get("description"):
        ch["description"] = prompt
        inferred.append("description")
    if not ch.get("category"):
        ch["category"] = next(
            (cat for cat, kws in _CATEGORY_HINTS if any(k in low for k in kws)),
            "misc",
        )
        inferred.append("category")
    if not ch.get("target"):
        target = _inferred_http_target(prompt)
        if target:
            ch["target"] = target
            inferred.append("target")
    if (body.get("mode") or ch.get("mode")) == "pentest":
        ch["mode"] = "pentest"
    if not ch.get("name"):
        # first few words, slugified — a readable thread-rail label
        words = re.findall(r"[A-Za-z0-9]+", prompt)[:4]
        ch["name"] = "-".join(w.lower() for w in words) or "challenge"
        inferred.append("name")
    body["challenge"] = ch
    body["_inferred_fields"] = inferred
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


def _pentest_demo_driver(body: dict[str, Any], mgr: RunManager | None = None) -> Driver:
    """Scripts a full pentest run whose sole purpose is to populate every
    runtime-asset / review panel on the deck (findings, vuln reports, PoCs,
    routes/branches, directives, credentials). UI/demo ONLY — kind:"pentest_demo".

    Credentials read the run's persisted shared_graph.db, so when a RunManager is
    available we point the demo at ``sessions/{id}/workspace/graph`` and it writes
    the verified credential facts there (matching what /api/runs/{id}/credentials
    reads). Without a manager it degrades to counting-only (no db)."""
    async def drive(run: Run) -> None:
        from examples.pentest_demo_solver import run_pentest_demo_solve

        # pace the canned stream so the panels visibly fill in the browser.
        tick = float(body.get("tick", 0.5))
        graph_dir = None
        if mgr is not None:
            graph_dir = mgr.workspace_dir(run.run_id) / "graph"
        await run_pentest_demo_solve(
            run.bus, run.cost, run_id=run.run_id, tick=tick, graph_dir=graph_dir)

    return drive


async def _open_planner_llm(
    *,
    llm_profiles: dict[str, Any],
    run: Run,
    mgr: RunManager | None,
) -> tuple[Any, Any]:
    """Return (context manager, client). Never raises; (None, None) on failure."""
    try:
        planner_profile = llm_profiles.get("planner") or {}
        planner_base = str(planner_profile.get("base_url") or "").strip()
        llm_kwargs: dict[str, Any] = {
            "cost": run.cost,
            "bus": run.bus,
            **llm_temperature_kwargs(planner_profile),
        }
        if planner_base:
            llm_kwargs["base_url"] = planner_base
        if mgr is not None:
            planner_key = LlmCredentialStore(mgr.sessions_root).resolve("planner")
            if planner_key:
                llm_kwargs["api_key"] = planner_key
        llm_cm = LLMClient(**llm_kwargs)
        llm = await llm_cm.__aenter__()
        return llm_cm, llm
    except Exception:
        return None, None


def _llm_may_override(field: str, inferred: set[str], current: Any) -> bool:
    if field in inferred:
        return True
    return not str(current or "").strip()


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
      swarm_class: "module.path:ClassName" (default muteki.swarm.swarm:Swarm)
                                                — product Coordinator. Omit this
                                                field for CTF and pentest.
                                                Experimental arms are eval-only
                                                and require an explicit spec.
    """
    try:
        declared_protocol = int(body.get("protocol", 1) or 1)
    except (TypeError, ValueError) as exc:
        raise RuntimeError("protocol must be 1 or 2") from exc

    async def drive(run: Run) -> None:
        import os
        import tempfile
        from pathlib import Path

        from muteki.models.solve_graph import (
            Challenge, apply_expected_findings, parse_engagement_goal,
        )
        from muteki.sandbox.manager import SandboxManager
        from muteki.solver.result import ArtifactStore
        from muteki.solver.types import SolverConfig
        from muteki.swarm.models import default_lineup

        # Empty spec → production Swarm. Explicit spec is eval-only.
        swarm_cls = _resolve_swarm_class(body.get("swarm_class"))

        ch = body.get("challenge", {})
        inferred_fields = set(body.get("_inferred_fields") or [])
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
        prompt_text = (body.get("prompt") or ch.get("description") or "").strip()
        goal_text = (ch.get("goal") or body.get("goal") or "")
        if mode == "pentest" and not str(goal_text).strip():
            goal_text = prompt_text
        scope_text = (ch.get("scope") or body.get("scope") or "")
        if mode == "pentest" and not scope_text and ch.get("target"):
            scope_text = str(ch.get("target") or "")
        coordinator = bool(body.get("coordinator", True))
        llm_profiles = dict(body.get("llm_profiles") or {})
        if not llm_profiles and mgr is not None:
            try:
                llm_profiles = dict(
                    mgr.worker_config.get().get("llm_profiles") or {})
            except Exception:
                llm_profiles = {}
        llm_cm = None
        llm = None
        parse_audit: dict[str, Any] = {"source": "regex", "model": ""}
        planner_model = str(
            (llm_profiles.get("planner") or {}).get("model") or "deepseek-v4-pro")
        parsed: dict[str, Any] = {}
        if coordinator:
            llm_cm, llm = await _open_planner_llm(
                llm_profiles=llm_profiles, run=run, mgr=mgr)
            if llm is not None:
                try:
                    parsed = await asyncio.wait_for(
                        parse_dispatch(
                            prompt_text, goal_text, mode,
                            llm=llm, model=planner_model),
                        timeout=15.0,
                    )
                except Exception:
                    parsed = {}
                if parsed:
                    parse_audit = {"source": "llm", "model": planner_model}
                    if (_llm_may_override("category", inferred_fields, ch.get("category"))
                            and parsed.get("category")):
                        ch["category"] = parsed["category"]
                        inferred_fields.discard("category")
                    if (_llm_may_override("target", inferred_fields, ch.get("target"))
                            and parsed.get("target")):
                        ch["target"] = parsed["target"]
                        inferred_fields.discard("target")
                    if (_llm_may_override("name", inferred_fields, ch.get("name"))
                            and parsed.get("name")):
                        ch["name"] = parsed["name"]
                        inferred_fields.discard("name")
                    if not str(scope_text or "").strip() and parsed.get("scope"):
                        scope_text = parsed["scope"]
        name_autogen = "name" in inferred_fields
        engagement = parse_engagement_goal(goal_text) if mode == "pentest" else None
        if engagement is not None and parsed:
            updates: dict[str, Any] = {}
            if parsed.get("finding_class"):
                updates["finding_class"] = parsed["finding_class"]
            if parsed.get("quantity"):
                updates["quantity"] = parsed["quantity"]
            if parsed.get("expected_findings") is not None:
                updates["expected_findings"] = parsed["expected_findings"]
            if parsed.get("collect_until_coverage") is not None:
                updates["collect_until_coverage"] = parsed["collect_until_coverage"]
            if parsed.get("expected_findings") and parsed.get("quantity") != "recon":
                updates.setdefault("quantity", "collect")
                updates["collect_until_coverage"] = False
            if updates:
                engagement = engagement.model_copy(update=updates)
        expected_findings_raw = (
            body.get("expected_findings")
            if body.get("expected_findings") is not None
            else ch.get("expected_findings")
        )
        if engagement is not None and expected_findings_raw not in (None, ""):
            try:
                want = int(expected_findings_raw)
            except (TypeError, ValueError):
                want = None
            if want is not None:
                engagement = apply_expected_findings(engagement, want)
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
            goal=goal_text,
            scope=scope_text,
            engagement=engagement,
            pentest_flag_required=bool(body.get("pentest_flag_required")
                                       if body.get("pentest_flag_required") is not None
                                       else ch.get("pentest_flag_required", False)),
        )
        executor = body.get("executor", "cli")
        try:
            protocol_version = int(body.get("protocol", 1) or 1)
        except (TypeError, ValueError) as exc:
            raise RuntimeError("protocol must be 1 or 2") from exc
        if protocol_version not in {1, 2}:
            raise RuntimeError("protocol must be 1 or 2")
        cli_race = bool(body.get("cli_race", False))
        cli_engine = body.get("cli_engine", "claude")
        # Default-off cognitive cluster planner (intent ranking + engine match).
        # Env MUTEKI_COGNITIVE_CLUSTER_PLANNER=1 also enables (Swarm ctor).
        cognitive_cluster_planner = bool(
            body.get("cognitive_cluster_planner", False)
        )
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
        # Offline capability is checked per selected profile below. Cursor uses its
        # ACP permission channel in this mode; the other engines use their native
        # deny flags or local-only tool allowlists.
        wc = mgr.worker_config.resolve(challenge.category) if mgr is not None else {}
        engines = body.get("engines") or wc.get("engines") or ["cursor", "claude", "codex", "pi", "omp"]
        worker_profiles = body.get("worker_profiles") or wc.get("worker_profiles") or []
        worker_network = str(
            body.get("worker_network") or wc.get("worker_network") or "bridge"
        ).strip()
        if worker_network not in {"bridge", "host", "none"}:
            raise RuntimeError("worker_network must be bridge, host, or none")
        if offline:
            worker_network = "none"
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
        token_budget = int(body.get("token_budget", 0) or 0)
        tool_call_budget = int(body.get("tool_call_budget", 0) or 0)
        max_barren_attempts = int(body.get("max_barren_attempts", 1) or 1)
        llm_profiles = body.get("llm_profiles") or wc.get("llm_profiles") or {}
        if protocol_version == 2:
            if mgr is None or mgr.protocol2 is None:
                raise RuntimeError(
                    "Protocol2Unavailable: "
                    + (mgr.protocol2_error if mgr is not None else "no Web composition root"))
            selected = _selected_profiles(engines, worker_profiles)
            if not offline or kb:
                raise RuntimeError(
                    "Protocol2CanaryRejected: live canary requires offline=true and kb=false")
            if (coordinator or bool(body.get("race_scout", True))
                    or len(selected) != 1):
                raise RuntimeError(
                    "Protocol2CanaryRejected: use one profile, coordinator=false, race_scout=false")
            if start_workers != 1 or max_workers != 1 or max_total_workers != 1:
                raise RuntimeError(
                    "Protocol2CanaryRejected: minimal canary requires exactly one worker/attempt")
            if (wall_clock_budget == float("inf") or not cost_budget_usd
                    or token_budget <= 0 or tool_call_budget <= 0):
                raise RuntimeError(
                    "Protocol2CanaryRejected: finite wall/cost/token/tool budgets are required")
        if "stage_policy" in body:
            stage_policy = copy.deepcopy(body.get("stage_policy") or {})
        elif wc.get("stage_policy"):
            stage_policy = copy.deepcopy(wc["stage_policy"])
        else:
            stage_policy = {
                "race": {
                    "enabled": bool(body["race_scout"]) if "race_scout" in body else (
                        False if mode == "pentest" else bool(wc.get("race_scout", True))),
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
        stage_policy.setdefault("coordinator", {})
        stage_policy["coordinator"]["token_budget"] = token_budget
        stage_policy["coordinator"]["tool_call_budget"] = tool_call_budget
        # race-scout layer (DESIGN_race_scout_layer.md): one parallel single-shot
        # round in front of the main coordinator loop. Operator-configurable from the request:
        #   race_scout (bool, default on) — whole-layer toggle
        #   race_engines (list, default = engines) — which engines race (worker switch)
        #   race_timeout (int, default 720s) — short per-worker recon timeout
        race_scout = (
            bool(body["race_scout"]) if "race_scout" in body
            else (False if mode == "pentest" else bool(wc.get("race_scout", True)))
        )
        race_engines = body.get("race_engines") or wc.get("race_engines") or None  # None → defaults to the roster
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
        if protocol_version == 2 and worker_backend != "local":
            raise RuntimeError(
                "Protocol2CanaryRejected: current live-local egress enforcer "
                "requires worker_backend=local")
        startup_health_snapshot: dict[str, bool] | None = None
        if mgr is not None and protocol_version != 2:
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
                    "challenge": challenge.model_dump(mode="json"),
                    "parse": parse_audit,
                    "name_autogen": bool(name_autogen),
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
                    sessions_root=mgr.sessions_root,
                    cached_results=run.profile_readiness,
                )
                mgr.persist_profile_readiness(run)
            if preflight_failures:
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
            root = mgr.workspace_dir(run.run_id)
        else:
            root = Path(tempfile.mkdtemp(prefix="muteki-web-"))
        # sbx is the sandbox root — sandbox.shutdown_all() rmtree's it at run end,
        # so NOTHING durable may live under it. arts + graph are SIBLINGS of sbx so
        # they persist (the shared_graph.db is the run's queryable fact graph).
        sandbox = SandboxManager(bus=run.bus, root=root / "sbx")
        arts = ArtifactStore(root=root / "arts")
        protocol2_session = None
        if protocol_version == 2:
            protocol2_session = mgr.protocol2.prepare_live_session(
                run_id=run.run_id,
                challenge_id=challenge.id,
                attachments=attachments,
                profiles=_selected_profiles(engines, worker_profiles),
                artifacts=arts,
                max_attempts=int(max_total_workers or 0),
                max_barren_attempts=max_barren_attempts,
                wall_ms=int(wall_clock_budget * 1000),
                token_budget=token_budget,
                cost_micro_usd=int(float(cost_budget_usd or 0.0) * 1_000_000),
                tool_call_budget=tool_call_budget,
                expected_goal_units=max(1, expected_flags),
            )
        graph_dir = root / "graph"
        # worker_root is a SIBLING of sbx (NOT under it) so each CLI worker's cwd —
        # staged attachments, agent-extracted files, PoCs — lives under the run's
        # sessions/{id}/workspace/ and survives sandbox.shutdown_all()'s rmtree of
        # sbx. It's cleaned up with the run (RunManager.delete drops sessions/{id}).
        worker_root = root / "workers"

        # Planner LLMClient was opened before Challenge construction (dispatch
        # parse). A missing key leaves llm=None and Reason no-ops.

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
        if mgr is not None and protocol_version != 2:
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
            hitl_inbox=(None if protocol_version == 2 else run.hitl),
            worker_cmds=(None if protocol_version == 2 else run.worker_cmds),
            executor=executor, cli_engine=cli_engine, cli_race=cli_race,
            engines=engines, start_workers=start_workers, max_workers=max_workers,
            web_access=web_access, kb=kb, coordinator=coordinator,
            cognitive_cluster_planner=cognitive_cluster_planner,
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
            worker_profiles=worker_profiles,
            startup_health_snapshot=startup_health_snapshot,
            credential_accounts_root=(
                account_store_root(mgr.sessions_root) if mgr is not None else None
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
            protocol2_session=protocol2_session,
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
            if protocol2_session is not None:
                await mgr.protocol2.complete_live_session(
                    run_id=run.run_id, session=protocol2_session,
                    solved=bool(out.solved))
            else:
                # Protocol 1 keeps its direct outcome projection. Protocol 2's
                # private outcome is only a canonical-finalization handoff; accepted
                # values become public later through typed flag.accepted recovery.
                run.flag = out.flag
        except BaseException as exc:
            from muteki.swarm.swarm_support import ControlShutdownIncomplete
            if not isinstance(exc, ControlShutdownIncomplete):
                if protocol2_session is not None:
                    try:
                        await mgr.protocol2.abort_live_session(
                            run_id=run.run_id, session=protocol2_session)
                    except BaseException as cleanup_exc:
                        # Never let best-effort cleanup erase the original owner
                        # failure/cancellation. Protocol2WebAdapter retains its live
                        # owner and store until canonical finalization itself lands.
                        if not isinstance(exc, asyncio.CancelledError):
                            exc.add_note(
                                "Protocol 2 abort cleanup failed without replacing "
                                "the original exception; "
                                f"cleanup_error_class={type(cleanup_exc).__name__}"
                            )
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
                    if protocol2_session is not None:
                        await mgr.protocol2.complete_live_session(
                            run_id=run.run_id, session=protocol2_session,
                            solved=bool(run.solved))
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

    drive.protocol_version = declared_protocol  # type: ignore[attr-defined]
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
    if engine in {"pi", "omp", "opencode", "dsh"}:
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

        from muteki.models.solve_graph import Challenge, EngagementGoal, parse_engagement_goal
        from muteki.solver.cli_driver import driver_for
        from muteki.solver.cli_solver import CliSolver
        from muteki.solver.credential_accounts import account_store_root
        from muteki.solver.result import ArtifactStore
        from muteki.solver.types import SolverConfig
        from muteki.swarm.shared_graph import SQLiteSharedGraph

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
            root = mgr.workspace_dir(run.run_id)
        else:
            return  # no workspace → nothing durable to resume from

        graph_dir = root / "graph"
        arts = ArtifactStore(root=root / "arts")
        worker_root = root / "workers"
        worker_root.mkdir(parents=True, exist_ok=True)

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
        engagement = None
        if mode == "pentest":
            raw_eg = ch.get("engagement")
            if isinstance(raw_eg, dict):
                try:
                    engagement = EngagementGoal.model_validate(raw_eg)
                except Exception:
                    engagement = parse_engagement_goal(ch.get("goal") or "")
            else:
                engagement = parse_engagement_goal(ch.get("goal") or "")
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
            # Carry the run's flag mode across a post-solve standby re-solve.
            expected_flags=int(ch.get("expected_flags") or 1),
            multi_flag=bool(ch.get("multi_flag", False)),
            verifier_rate_limited=bool(ch.get("verifier_rate_limited", False)),
            mode=mode,
            goal=ch.get("goal") or "",
            scope=ch.get("scope") or "",
            engagement=engagement,
            pentest_flag_required=bool(ch.get("pentest_flag_required", False)),
        )

        wc = mgr.worker_config.resolve(challenge.category) if mgr is not None else {}
        worker_profiles = wc.get("worker_profiles") or []
        worker_network = str(wc.get("worker_network") or "bridge")
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
            request_backend=None,
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
        container = None
        setup_cancel_boundary = None
        setup_exit_query = None
        account_root = account_store_root(mgr.sessions_root) if mgr is not None else None
        if backend == "container":
            from muteki.solver.container_exec import ensure_container
            setup_container_active = True

            async def _cancel_setup_container() -> None:
                nonlocal setup_container_active
                if not setup_container_active:
                    return
                from muteki.solver.container_exec import teardown_container
                removed = await asyncio.to_thread(
                    teardown_container, run.run_id, remove=True)
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
                    str(root),
                    network=worker_network,
                    account_root=(str(account_root) if account_root is not None else None),
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
            hitl_cmd={**runtime_cmd, "flag": flag},
            found_flags=prior_flags,
            solver_label=solver_label,
            container=container,
            worker_env=worker_env,
            identity=worker_identity_fields(profile),
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
