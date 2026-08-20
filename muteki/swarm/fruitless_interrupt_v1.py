"""Env-gated mid-flight fruitless worker interrupt for the coordinator.

Round-1 NYU A/B (DeepSeek) showed ``MUTEKI_CHAIN_COMPLETION`` never fired on
burn failures: bootstrap workers tool-spin until budget death and never emit
``worker_finished``, so the post-reap Reason force has no trigger.

Round-2 proved interrupt fires in the real loop, but PASS trajectories were
also killed (``life`` 222s→412s) and post-interrupt Reason only saw a thin
brief.

Round-3 (still default OFF):
  * interrupt only while the board has **zero verified facts and zero flags**
    (once any fact/flag exists, keep the worker — protect PASS / fact-growth);
  * bootstrap is always eligible under that gate; explore requires
    ``MUTEKI_FRUITLESS_INTERRUPT_EXPLORE=1`` (harness new arm enables it so
    zero-fact explore spins can still be reaped);
  * after interrupt, inject a short **working packet** into standing guidance
    so Reason sees attempted goals / dead-ends / do-not-repeat, not a bare force.

Round-4 (same env gate): after an interrupt-forced Reason cycle, if the planner
fails/times out (or otherwise leaves the queue empty), **do not** enter
``collect_idle`` / ``needs_new_information`` — spawn one bounded re-bootstrap
worker so the run keeps a live actor.

Round-5 (same env gate): defer interrupt while tools are still progressing,
especially for a sole ordinary worker (PASS ``life`` was killed at 150s while
still tooling).  Burns that tool forever are still cut by a hard cap.

Round-6: longer retire settle + soft-continue so hard-cap cancel reaches
``worker_finished`` / Reason.

Round-7: strengthen the post-interrupt working packet + a MUST coordinator
directive so Reason proposes small discriminating experiments, not another
whole-challenge bootstrap paraphrase.

Round-8: inject **named attachment / workspace / brief artifacts** into the
packet + MUST constraint so Reason must pick one concrete file/blob for a
single check (file/strings/xxd/decode) instead of pwd/ls/README meta.

Round-9: (a) empty post-interrupt Reason (``proposed=0``) must retry / inject
a synthetic artifact-chain intent — never silent idle while siblings burn;
(b) defer hard-cap when the live worker is already tooling a Named artifact,
and require file→strings→xxd in ONE intent/worker.

Round-10: on fruitless interrupt, **harvest** file/strings/xxd tool outputs
touching Named artifacts into shared-graph facts. Round-9 showed tools ran
extensively but the CLI never emitted ``VERIFIED_FACT=`` markers, so the
graph stayed at facts=0; ``runtime_failure`` is the generic unsolved finish
label, not a separate wipe of already-committed facts.

Round-11: after harvest, working packet / MUST constraint cite concrete
ELF/strings/.rodata clues and demand a **falsifiable crypto hypothesis**
(xor / LFSR / known-plaintext / constant decode) — forbid redoing the
file→strings→xxd suite.

Round-13: **domain-aware** packets — filter hex/prose noise from
``named_artifacts``; enable crypto-hypothesis only for crypto category or
binary+cipher evidence; forensics/pcap packets demand tshark/tcpdump/strings
and forbid XOR on pcap headers.

No promotion authority.
"""

from __future__ import annotations

import os
import re
from pathlib import Path
from typing import Any, Final

ENV_FLAG: Final = "MUTEKI_FRUITLESS_INTERRUPT"
ENV_THRESHOLD_S: Final = "MUTEKI_FRUITLESS_INTERRUPT_S"
ENV_MAX: Final = "MUTEKI_FRUITLESS_INTERRUPT_MAX"
ENV_EXPLORE: Final = "MUTEKI_FRUITLESS_INTERRUPT_EXPLORE"
ENV_REBOOTSTRAP_MAX: Final = "MUTEKI_FRUITLESS_INTERRUPT_REBOOTSTRAP_MAX"
ENV_TOOL_STALL_S: Final = "MUTEKI_FRUITLESS_INTERRUPT_TOOL_STALL_S"
ENV_SOLE_EXTRA_S: Final = "MUTEKI_FRUITLESS_INTERRUPT_SOLE_EXTRA_S"
ENV_HARD_CAP_S: Final = "MUTEKI_FRUITLESS_INTERRUPT_HARD_CAP_S"
ENV_SETTLE_S: Final = "MUTEKI_FRUITLESS_INTERRUPT_SETTLE_S"
ENV_ARTIFACT_EXTRA_S: Final = "MUTEKI_FRUITLESS_INTERRUPT_ARTIFACT_EXTRA_S"
ENV_EMPTY_REASON_RETRIES: Final = "MUTEKI_FRUITLESS_INTERRUPT_EMPTY_REASON_RETRIES"
ENV_HARVEST: Final = "MUTEKI_FRUITLESS_INTERRUPT_HARVEST"

DEFAULT_THRESHOLD_S: Final = 180.0
DEFAULT_MAX_INTERRUPTS: Final = 4
DEFAULT_MAX_REBOOTSTRAPS: Final = 2
DEFAULT_TOOL_STALL_S: Final = 60.0
DEFAULT_SOLE_EXTRA_S: Final = 120.0
DEFAULT_HARD_CAP_S: Final = 300.0
# Hard-cap kills often need >2s for CLI process exit proof; the default
# MUTEKI_CLI_CANCEL_CLEANUP_TIMEOUT=2 was the round-5 hang (retire miss →
# ControlShutdownIncomplete before worker_finished / Reason).
DEFAULT_SETTLE_S: Final = 20.0
# Extra hard-cap budget while a worker is actively tooling a Named artifact
# (file→strings→xxd chain should finish in one worker, not reset each step).
DEFAULT_ARTIFACT_EXTRA_S: Final = 180.0
DEFAULT_EMPTY_REASON_RETRIES: Final = 1

PACKET_PREFIX: Final = "[fruitless-interrupt working packet]"

# PlannerFailureKind string values (and loose substrings) that mean Reason did
# not produce usable work after an interrupt — must not idle-pause.
_REASON_FAILURE_MARKERS: Final = (
    "planner_exception",
    "planner_unavailable",
    "exception",
    "unavailable",
    "timeout",
    "connecttimeout",
    "empty_plan",
    "invalid_plan",
    "needs_new_information",
)


def enabled() -> bool:
    return os.environ.get(ENV_FLAG, "").strip() in {"1", "true", "TRUE", "yes"}


def explore_interrupt_enabled() -> bool:
    """When off, only bootstrap (no claimed explore intent) may be interrupted."""
    return os.environ.get(ENV_EXPLORE, "").strip() in {"1", "true", "TRUE", "yes"}


def threshold_seconds() -> float:
    raw = os.environ.get(ENV_THRESHOLD_S, "").strip()
    if not raw:
        return DEFAULT_THRESHOLD_S
    try:
        value = float(raw)
    except ValueError:
        return DEFAULT_THRESHOLD_S
    # Floor keeps accidental 0/negative from instantly killing every spawn.
    return max(30.0, value)


def max_interrupts() -> int:
    raw = os.environ.get(ENV_MAX, "").strip()
    if not raw:
        return DEFAULT_MAX_INTERRUPTS
    try:
        value = int(raw)
    except ValueError:
        return DEFAULT_MAX_INTERRUPTS
    return max(1, value)


def max_reboots() -> int:
    raw = os.environ.get(ENV_REBOOTSTRAP_MAX, "").strip()
    if not raw:
        return DEFAULT_MAX_REBOOTSTRAPS
    try:
        value = int(raw)
    except ValueError:
        return DEFAULT_MAX_REBOOTSTRAPS
    return max(1, value)


def _env_float(name: str, default: float, *, floor: float = 0.0) -> float:
    raw = os.environ.get(name, "").strip()
    if not raw:
        return default
    try:
        value = float(raw)
    except ValueError:
        return default
    return max(floor, value)


def tool_stall_seconds() -> float:
    return _env_float(ENV_TOOL_STALL_S, DEFAULT_TOOL_STALL_S, floor=5.0)


def sole_extra_seconds() -> float:
    return _env_float(ENV_SOLE_EXTRA_S, DEFAULT_SOLE_EXTRA_S, floor=0.0)


def hard_cap_seconds() -> float:
    # 0 disables the hard cap (not recommended for burns).
    return _env_float(ENV_HARD_CAP_S, DEFAULT_HARD_CAP_S, floor=0.0)


def settle_seconds() -> float:
    """How long to wait for runtime exit proof after an interrupt cancel."""
    return _env_float(ENV_SETTLE_S, DEFAULT_SETTLE_S, floor=2.0)


def artifact_extra_seconds() -> float:
    """Extra hard-cap seconds when the worker is tooling a Named artifact."""
    return _env_float(ENV_ARTIFACT_EXTRA_S, DEFAULT_ARTIFACT_EXTRA_S, floor=0.0)


def max_empty_reason_retries() -> int:
    raw = os.environ.get(ENV_EMPTY_REASON_RETRIES, "").strip()
    if not raw:
        return DEFAULT_EMPTY_REASON_RETRIES
    try:
        value = int(raw)
    except ValueError:
        return DEFAULT_EMPTY_REASON_RETRIES
    return max(0, value)


def harvest_enabled() -> bool:
    """Tool→fact harvest at interrupt. Default ON when interrupt is ON.

    Set ``MUTEKI_FRUITLESS_INTERRUPT_HARVEST=0`` to disable without turning off
    the rest of the interrupt stack.
    """
    if not enabled():
        return False
    raw = os.environ.get(ENV_HARVEST, "").strip()
    if not raw:
        return True
    return raw in {"1", "true", "TRUE", "yes"}


def should_soft_continue_after_retire_miss(
    *,
    was_fruitless_interrupt: bool,
    interrupt_enabled: bool | None = None,
) -> bool:
    """True → emit worker_finished + replan; do NOT raise ControlShutdownIncomplete.

    Round-5 stfu: hard-cap cancel left the asyncio task done, but
    ``_retire_worker_account`` timed out at 2s and the coordinator raised
    before ``worker_finished`` / Reason could run.  Soft-continue keeps the
    autonomous reaper as owner while the swarm replans.
    """
    if not was_fruitless_interrupt:
        return False
    return bool(enabled() if interrupt_enabled is None else interrupt_enabled)


def worker_tool_count(solver: Any) -> int:
    """Best-effort tool-progress counter from a live CliSolver."""
    if solver is None:
        return 0
    for attr in ("_raw_tool_commands", "_raw_tool_outputs", "_pending_tool_calls"):
        rows = getattr(solver, attr, None)
        if isinstance(rows, list):
            return len(rows)
    return 0


_ARTIFACT_CHECK_RE: Final = re.compile(
    r"(?i)\b(file|strings|xxd|hexdump|objdump|readelf|binwalk|openssl|base64)\b"
)


def worker_artifact_progress(
    solver: Any,
    named_artifacts: list[str] | None,
) -> bool:
    """True when live tool commands inspect a Named artifact.

    Intent goal alone does NOT count — bootstrap goals like ``Solve stfu`` would
    otherwise defer hard-cap forever without real file/strings/xxd progress.
    """
    names = [
        str(a).strip().lower()
        for a in (named_artifacts or [])
        if str(a or "").strip()
    ]
    if not names or solver is None:
        return False
    cmds = getattr(solver, "_raw_tool_commands", None)
    if not isinstance(cmds, list) or not cmds:
        return False
    for raw in cmds:
        text = str(raw or "").strip()
        if not text:
            continue
        low = text.lower()
        if not _ARTIFACT_CHECK_RE.search(low):
            continue
        if any(name in low for name in names):
            return True
    return False


_HARVEST_NOISE_RE: Final = re.compile(
    r"(?i)(command not found|no such file or directory|permission denied|"
    r"usage:\s|could not open|moved to the background|timeout)"
)
_HARVEST_CHECK_KIND_RE: Final = re.compile(
    r"(?i)\b(file|strings|xxd|hexdump|readelf|objdump|binwalk)\b"
)


def _clip_witness(text: str, n: int = 280) -> str:
    t = " ".join(str(text or "").split())
    if len(t) <= n:
        return t
    return t[: n - 1] + "…"


_PRINTABLE_RUN_RE: Final = re.compile(r"[\x20-\x7e]{4,64}")
_ELF_LINE_RE: Final = re.compile(
    r"(?i)\bELF\s+[\w\s,-]+?(?:executable|shared object)[^\n]{0,120}"
)
_HEX_CONST_RE: Final = re.compile(
    r"(?i)\b(?:0x)?([0-9a-f]{6,16})\b"
)
_RODATA_HINT_RE: Final = re.compile(
    r"(?i)(\.rodata|extra data|buildid|sha256|LFSR|xor|cipher|key)"
)


def _high_signal_snippets(kind: str, body: str, *, limit: int = 4) -> list[str]:
    """Pull short, planner-usable clues from a harvest witness body."""
    text = str(body or "")
    if not text.strip():
        return []
    out: list[str] = []
    kind_l = (kind or "").lower()

    if kind_l == "file" or "elf" in text.lower():
        for m in _ELF_LINE_RE.finditer(text):
            clip = _clip_witness(m.group(0), 140)
            if clip and clip not in out:
                out.append(clip)
            if len(out) >= limit:
                return out

    # Prefer readable ASCII runs (strings / .rodata dumps).
    runs = _PRINTABLE_RUN_RE.findall(text)
    # Rank: letter-heavy, not pure hex, not path noise.
    ranked: list[tuple[int, str]] = []
    for run in runs:
        s = run.strip()
        if len(s) < 4:
            continue
        low = s.lower()
        if low.startswith("/lib") or low.startswith("/usr"):
            continue
        if re.fullmatch(r"[0-9a-fA-F]+", s):
            continue
        letters = sum(ch.isalpha() for ch in s)
        score = letters * 2 + (5 if " " in s else 0)
        if _RODATA_HINT_RE.search(s):
            score += 8
        if any(tok in low for tok in ("stfu", "flag", "usage", "file", "error")):
            score += 4
        ranked.append((score, s))
    ranked.sort(key=lambda x: (-x[0], -len(x[1])))
    for _, s in ranked:
        clip = _clip_witness(s, 80)
        if clip and clip not in out:
            out.append(clip)
        if len(out) >= limit:
            return out

    # Hex / LE constants as last resort (rodata heads).
    for m in _HEX_CONST_RE.finditer(text):
        hx = m.group(1)
        if len(hx) < 6:
            continue
        clip = f"const=0x{hx.lower()}"
        if clip not in out:
            out.append(clip)
        if len(out) >= limit:
            break
    return out[:limit]


def extract_crypto_clues(
    harvest_rows: list[dict[str, str]] | None = None,
    shared_graph: Any = None,
    *,
    limit: int = 6,
) -> list[str]:
    """Concrete ELF/strings/.rodata clues for post-harvest Reason packets."""
    clues: list[str] = []

    def _add(raw: str) -> None:
        text = _clip_witness(str(raw or "").strip(), 120)
        if not text or text in clues:
            return
        clues.append(text)

    for row in harvest_rows or []:
        kind = str(row.get("check") or "")
        art = str(row.get("artifact") or "")
        for snip in row.get("signals") or []:
            label = f"{kind}:{art}" if art else kind
            _add(f"[{label}] {snip}" if label else str(snip))
            if len(clues) >= limit:
                return clues
        # Fall back to witness snippets when signals missing.
        for snip in _high_signal_snippets(kind, str(row.get("witness") or "")):
            label = f"{kind}:{art}" if art else kind
            _add(f"[{label}] {snip}")
            if len(clues) >= limit:
                return clues

    # Also mine recent graph facts (harvest already committed).
    for fact in _recent_fact_texts(shared_graph, limit=8):
        if "Tool harvest" not in fact and "ELF" not in fact and ".rodata" not in fact:
            # Still allow high-signal printable from any fact text.
            for snip in _high_signal_snippets("", fact, limit=2):
                _add(snip)
                if len(clues) >= limit:
                    return clues
            continue
        kind = "harvest"
        m = re.search(r"Tool harvest \((\w+)\) on `([^`]+)`", fact)
        if m:
            kind = f"{m.group(1)}:{m.group(2)}"
        for snip in _high_signal_snippets(kind.split(":", 1)[0], fact, limit=3):
            _add(f"[{kind}] {snip}")
            if len(clues) >= limit:
                return clues
    return clues[:limit]


def _tool_pairs(solver: Any) -> list[tuple[str, str]]:
    cmds = getattr(solver, "_raw_tool_commands", None) or []
    outs = getattr(solver, "_raw_tool_outputs", None) or []
    if not isinstance(cmds, list) or not isinstance(outs, list):
        return []
    n = min(len(cmds), len(outs))
    return [(str(cmds[i] or ""), str(outs[i] or "")) for i in range(n)]


def harvest_artifact_tool_facts(
    solver: Any,
    named_artifacts: list[str] | None,
    *,
    limit: int = 6,
) -> list[dict[str, str]]:
    """Compress Named-artifact check tool outputs into graph-ready fact rows.

    Round-10: CLI workers often run ``file``/``strings``/``xxd`` but never emit
    ``VERIFIED_FACT=`` markers, so the shared graph stays empty until harvest.
    Round-11: also attach ``signals`` (high-signal string/ELF/rodata snippets).
    """
    if not harvest_enabled() or solver is None:
        return []
    names = [
        str(a).strip()
        for a in (named_artifacts or [])
        if str(a or "").strip()
    ]
    if not names:
        return []
    names_l = [n.lower() for n in names]
    out: list[dict[str, str]] = []
    seen: set[str] = set()
    # Prefer latest observations (end of the burn is usually richest).
    for cmd, raw_out in reversed(_tool_pairs(solver)):
        if len(out) >= max(1, int(limit)):
            break
        low_cmd = cmd.lower()
        kind_m = _HARVEST_CHECK_KIND_RE.search(low_cmd)
        if not kind_m:
            continue
        if not any(n in low_cmd for n in names_l):
            continue
        body = raw_out.strip()
        if not body:
            continue
        # Skip pure noise, but keep short actionable errors (missing tool/file).
        if _HARVEST_NOISE_RE.search(body) and len(body) > 220:
            continue
        # Skip harness/report JSON accidentally slurped into a Bash output.
        head = body.lstrip()[:120]
        if head.startswith("{") and (
            '"schema"' in head or "nyu-ab" in head or "muteki.eval" in head
        ):
            continue
        kind = kind_m.group(1).lower()
        hit = next((n for n in names if n.lower() in low_cmd), names[0])
        cmd_short = _clip(cmd.replace("\n", " "), 80)
        signals = _high_signal_snippets(kind, body, limit=4)
        # Prefer signal-rich witness for the fact line when available.
        if signals:
            witness = _clip_witness(" | ".join(signals), 280)
        else:
            witness = _clip_witness(body, 280)
        fact = (
            f"Tool harvest ({kind}) on `{hit}`: {witness} "
            f"[via `{cmd_short}`]"
        )
        key = f"{kind}::{hit.lower()}::{witness[:80].lower()}"
        if key in seen:
            continue
        seen.add(key)
        out.append({
            "fact": fact[:500],
            "witness": witness,
            "command": cmd_short,
            "artifact": hit,
            "check": kind,
            "signals": signals,
        })
    # Restore chronological order for readability on the board.
    out.reverse()
    return out


def commit_harvested_facts(
    shared_graph: Any,
    *,
    actor: str,
    rows: list[dict[str, str]],
) -> list[int]:
    """Write harvested rows into ``shared_graph`` as verified evidence (witnessed)."""
    if shared_graph is None or not rows:
        return []
    add = getattr(shared_graph, "add_evidence", None)
    if not callable(add):
        return []
    seqs: list[int] = []
    who = (actor or "coordinator").strip() or "coordinator"
    for row in rows:
        fact = str(row.get("fact") or "").strip()
        if not fact:
            continue
        try:
            seq = int(
                add(
                    actor=who,
                    source="fruitless_interrupt_harvest",
                    fact=fact,
                    artifact_id=(
                        f"harvest:{row.get('check') or 'tool'}:"
                        f"{row.get('artifact') or 'file'}"
                    ),
                    verified=True,
                    confidence=0.75,
                    witness=str(row.get("witness") or "")[:500] or None,
                    verifier="fruitless_interrupt_harvest",
                )
                or 0
            )
        except Exception:
            seq = 0
        if seq > 0:
            seqs.append(seq)
    return seqs


def should_retry_empty_reason(
    *,
    pending_recovery: bool,
    reason_proposed: int,
    retry_count: int = 0,
    max_retries: int | None = None,
    interrupt_enabled: bool | None = None,
) -> bool:
    """True when interrupt-forced Reason returned zero intents — retry once."""
    if not (enabled() if interrupt_enabled is None else interrupt_enabled):
        return False
    if not pending_recovery:
        return False
    if int(reason_proposed) > 0:
        return False
    limit = (
        max_empty_reason_retries()
        if max_retries is None
        else int(max_retries)
    )
    return int(retry_count) < max(0, limit)


def should_inject_artifact_chain_intent(
    *,
    pending_recovery: bool,
    reason_proposed: int,
    named_artifacts: list[str] | None,
    already_injected: bool = False,
    interrupt_enabled: bool | None = None,
) -> bool:
    """True when empty Reason exhausted retries — inject a concrete chain intent."""
    if not (enabled() if interrupt_enabled is None else interrupt_enabled):
        return False
    if not pending_recovery or already_injected:
        return False
    if int(reason_proposed) > 0:
        return False
    return bool(named_artifacts)


def build_artifact_chain_intent_goal(
    named_artifacts: list[str] | None,
    *,
    domain: str | None = None,
) -> str:
    """Single-intent goal tailored to replan domain (crypto vs pcap vs generic)."""
    names = [
        _artifact_basename(a)
        for a in (named_artifacts or [])
        if _is_usable_artifact_name(_artifact_basename(a))
    ]
    primary = names[0] if names else "ARTIFACT"
    dom = (domain or "").strip().lower()
    if not dom:
        dom = (
            "forensics_pcap"
            if any(_looks_like_pcap_name(n) for n in names)
            else "bootstrap"
        )
    if dom == "forensics_pcap":
        return (
            f"On `{primary}` in one worker: run file, then tshark or tcpdump to "
            f"list conversations/streams, then strings/carve transferred "
            f"payloads; record findings before any other exploration."
        )
    return (
        f"On `{primary}` in one worker: run file, then strings, then xxd/hexdump "
        f"of the first 1–4KiB; record type/strings/header findings before any "
        f"other exploration."
    )


def reason_failure_kind(planner_failure: Any) -> str:
    """Normalize ``PlannerFailure.kind`` (enum or str) to a lowercase token."""
    if planner_failure is None:
        return ""
    kind = getattr(planner_failure, "kind", planner_failure)
    if hasattr(kind, "value"):
        kind = kind.value
    return str(kind or "").strip().lower()


def should_rebootstrap_after_reason(
    *,
    pending_recovery: bool,
    tasks_empty: bool,
    open_intents: int,
    reason_proposed: int,
    planner_failure_kind: str = "",
    rebootstrap_count: int = 0,
    max_reboots_n: int | None = None,
    interrupt_enabled: bool | None = None,
) -> bool:
    """True when post-interrupt Reason left no work — re-bootstrap, don't idle.

    Gated by ``MUTEKI_FRUITLESS_INTERRUPT``.  Covers ConnectTimeout / EXCEPTION
    and empty plans so the coordinator never parks in ``collect_idle`` after
    killing the only live worker.
    """
    if not (enabled() if interrupt_enabled is None else interrupt_enabled):
        return False
    if not pending_recovery:
        return False
    if not tasks_empty or int(open_intents) > 0:
        return False
    limit = max_reboots() if max_reboots_n is None else int(max_reboots_n)
    if int(rebootstrap_count) >= limit:
        return False
    if int(reason_proposed) > 0:
        # Intents were accepted into the board but somehow not open — still
        # avoid idle; a bootstrap keeps the loop alive.
        return True
    kind = str(planner_failure_kind or "").strip().lower()
    if kind:
        if any(marker in kind for marker in _REASON_FAILURE_MARKERS):
            return True
        # Unknown failure token — still recover rather than idle-pause.
        return True
    # No typed failure but zero proposals after interrupt-forced Reason.
    return True


def should_interrupt_worker(
    *,
    running_for_s: float,
    threshold_s: float,
    facts_at_start: int,
    flags_at_start: int,
    facts_now: int,
    flags_now: int,
    is_review: bool = False,
    worker_mode: str = "bootstrap",
    allow_explore: bool | None = None,
    ordinary_worker_count: int = 1,
    seconds_since_last_tool: float | None = None,
    tool_stall_s: float | None = None,
    sole_extra_s: float | None = None,
    hard_cap_s: float | None = None,
    artifact_progress: bool = False,
    artifact_extra_s: float | None = None,
) -> bool:
    """Return True when this live worker is a mid-flight fruitless interrupt target.

    Round-3 gates:
      1. never interrupt review;
      2. never interrupt once the board has any verified fact or flag;
      3. per-worker clock past threshold with no growth since THAT start;
      4. explore workers only when ``allow_explore`` (env) is on.

    Round-5 gates:
      5. if tools progressed recently (``seconds_since_last_tool < tool_stall_s``),
         defer — sole workers get ``threshold + sole_extra`` and otherwise wait
         for ``hard_cap_s`` so continuous tooling on burns still gets cut;
      6. if tools have stalled past ``tool_stall_s`` and base threshold elapsed,
         interrupt (empty spinning / hung CLI).

    Round-9: when ``artifact_progress`` (tools/goal touch a Named artifact),
    hard-cap is deferred by ``artifact_extra_s`` so file→strings→xxd can finish
    in the same worker instead of resetting at the base hard-cap.
    """
    if is_review:
        return False
    mode = (worker_mode or "bootstrap").strip().lower()
    if mode == "review":
        return False
    # Board already has signal — do not mid-flight kill (round-2 life regression).
    if facts_now > 0 or flags_now > 0:
        return False
    if mode == "explore":
        explore_ok = (
            explore_interrupt_enabled()
            if allow_explore is None
            else bool(allow_explore)
        )
        if not explore_ok:
            return False
    if running_for_s < threshold_s:
        return False
    # Still require per-worker fruitlessness (defensive; board is already zero).
    if facts_now > facts_at_start:
        return False
    if flags_now > flags_at_start:
        return False

    stall_s = tool_stall_seconds() if tool_stall_s is None else float(tool_stall_s)
    sole_s = sole_extra_seconds() if sole_extra_s is None else float(sole_extra_s)
    cap_s = hard_cap_seconds() if hard_cap_s is None else float(hard_cap_s)
    extra_s = (
        artifact_extra_seconds()
        if artifact_extra_s is None
        else float(artifact_extra_s)
    )
    if artifact_progress and cap_s > 0:
        cap_s = cap_s + max(0.0, extra_s)

    # Hard cap: zero-fact burn that keeps tooling forever must still be cut.
    if cap_s > 0 and running_for_s >= cap_s:
        return True

    # Unknown tool clock (None) → treat as stalled for the full runtime so a
    # worker that never emits tool telemetry can still be reaped at threshold.
    since_tool = (
        float(running_for_s)
        if seconds_since_last_tool is None
        else float(seconds_since_last_tool)
    )
    recent_tools = since_tool < max(0.0, stall_s)
    if recent_tools:
        # Round-9: tooling a Named artifact → defer until (extended) hard-cap.
        if artifact_progress:
            return False
        sole = int(ordinary_worker_count) <= 1
        if sole:
            # Sole worker still tooling: give threshold+sole_extra, then wait
            # for hard_cap (already checked above) rather than mid-kill.
            if running_for_s < threshold_s + max(0.0, sole_s):
                return False
            return False
        # Multi-worker: still defer while tools are hot; hard_cap is the escape.
        return False

    # Tools stalled and base threshold elapsed → interrupt.
    # Exception: Named-artifact progress with a recent stall still waits for
    # the extended hard-cap (already handled above when running_for_s >= cap).
    if artifact_progress and cap_s > 0 and running_for_s < cap_s:
        return False
    return True


def _clip(text: str, n: int = 120) -> str:
    t = " ".join(str(text or "").split())
    if len(t) <= n:
        return t
    return t[: n - 1] + "…"


def _recent_dead_ends(shared_graph: Any, *, limit: int = 3) -> list[str]:
    if shared_graph is None:
        return []
    try:
        snap = shared_graph.snapshot()
    except Exception:
        return []
    dead = getattr(snap, "dead_ends", None) or []
    out: list[str] = []
    for item in dead:
        if isinstance(item, dict):
            reason = str(item.get("reason") or item.get("text") or "").strip()
        else:
            reason = str(item or "").strip()
        if reason and reason not in out:
            out.append(_clip(reason))
        if len(out) >= limit:
            break
    # Prefer most recent.
    return out[-limit:]


def _recent_fact_texts(shared_graph: Any, *, limit: int = 3) -> list[str]:
    if shared_graph is None:
        return []
    try:
        snap = shared_graph.snapshot()
    except Exception:
        return []
    if isinstance(snap, dict):
        facts = snap.get("facts") or snap.get("evidence") or []
    else:
        facts = getattr(snap, "facts", None) or getattr(snap, "evidence", None) or []
    out: list[str] = []
    for item in facts:
        if isinstance(item, dict):
            text = str(
                item.get("text") or item.get("fact") or item.get("content") or ""
            ).strip()
        else:
            text = str(getattr(item, "text", None) or item or "").strip()
        if text and text not in out:
            out.append(_clip(text))
        if len(out) >= limit:
            break
    return out[-limit:]


def recent_attempted_goals(shared_graph: Any, *, limit: int = 4) -> list[str]:
    """Goals already tried — barren concludes first, then any intent goals."""
    if shared_graph is None:
        return []
    out: list[str] = []
    try:
        rows = shared_graph.barren_concluded_goal_texts()
    except Exception:
        rows = []
    for goal in rows or []:
        text = _clip(str(goal or "").strip(), 160)
        if text and text not in out:
            out.append(text)
        if len(out) >= limit:
            return out
    try:
        intents = shared_graph.list_intents()  # type: ignore[attr-defined]
    except Exception:
        try:
            snap = shared_graph.snapshot()
            intents = snap.get("intents") if isinstance(snap, dict) else []
        except Exception:
            intents = []
    for intent in intents or []:
        if not isinstance(intent, dict):
            continue
        text = _clip(str(intent.get("goal") or "").strip(), 160)
        if text and text not in out:
            out.append(text)
        if len(out) >= limit:
            break
    return out


CONSTRAINT_PREFIX: Final = "[fruitless-interrupt MUST constraint]"

# Stable phrases unit tests / telemetry assert on.
REQUIRED_PACKET_MARKERS: Final = (
    "do NOT paraphrase",
    "FORBIDDEN",
    "REQUIRED",
    "whole-challenge bootstrap",
    "Named artifacts",
    "falsifiable",
)

REQUIRED_PACKET_MARKERS_BOOTSTRAP: Final = (
    "do NOT paraphrase",
    "FORBIDDEN",
    "REQUIRED",
    "SAME worker",
    "whole-challenge bootstrap",
    "Named artifacts",
    "file, then strings, then xxd",
)

REQUIRED_PACKET_MARKERS_FORENSICS: Final = (
    "do NOT paraphrase",
    "FORBIDDEN",
    "REQUIRED",
    "whole-challenge bootstrap",
    "Named artifacts",
    "tshark",
)

# Operator / infra names that must not be pushed as player artifacts.
_SKIP_ARTIFACT_NAMES: Final = frozenset({
    "challenge.json",
    "dockerfile",
    "docker-compose.yml",
    "docker-compose.yaml",
    "manifest.json",
    "winner.json",
    ".gitignore",
    ".ds_store",
})
_BLOCKED_ARTIFACT_FRAGMENTS: Final = (
    "flag",
    "solution",
    "solver",
    "writeup",
    "exploit",
    "answer",
    "secret",
    "private",
    "organizer",
    "fruitless_interrupt",
)
# Prose / harvest-noise tokens that leaked into R12 named_artifacts.
_ARTIFACT_STOPWORDS: Final = frozenset({
    "fact", "facts", "harvest", "tool", "tools", "source", "on", "via",
    "the", "and", "with", "from", "for", "this", "that", "type", "name",
    "size", "data", "idx", "bash", "completed", "output", "witness",
    "actor", "coordinator", "signals", "checks", "artifacts", "artifact",
    "worker", "intent", "goal", "header", "hex", "const", "none", "null",
    "true", "false", "file", "strings", "xxd", "objdump", "readelf",
})
_CRYPTO_CATEGORIES: Final = frozenset({"crypto", "cryptography"})
_FORENSICS_CATEGORIES: Final = frozenset({"forensics", "forensic"})
_PCAP_SUFFIXES: Final = (".pcap", ".pcapng", ".cap")
_BINARY_CIPHER_MARKERS: Final = (
    "elf ",
    "elf-",
    "ciphertext",
    "cipher text",
    " stfu",
    "header magic",
    ".rodata",
    "tap values",
    "lfsr",
    "berlekamp",
    "known-plaintext",
    "encrypted",
    "xor key",
)
# Filenames mentioned in brief: extensioned names or `backtick` / "quoted" tokens.
_BRIEF_FILE_RE: Final = re.compile(
    r"(?i)(?:"
    r"`([a-z0-9][\w.-]{1,64})`"
    r"|\"([a-z0-9][\w.-]{1,64})\""
    r"|'([a-z0-9][\w.-]{1,64})'"
    r"|\b([a-z0-9][\w.-]{0,64}\."
    r"(?:bin|txt|py|rb|c|cpp|h|zip|gz|tgz|pcap|pcapng|cap|img|elf|so|key|dat|csv|"
    r"json|md|pdf|exe|enc|cipher|out|in|db))"
    r")"
)
# Fact mining: ONLY backtick names or extensioned basenames (not bare hex words).
_FACT_BACKTICK_RE: Final = re.compile(r"`([^`\n]{1,80})`")
_FACT_EXT_FILE_RE: Final = re.compile(
    r"(?i)\b([a-z0-9][\w.-]{0,64}\."
    r"(?:bin|txt|py|rb|c|cpp|h|zip|gz|tgz|pcap|pcapng|cap|img|elf|so|key|dat|csv|"
    r"json|md|pdf|exe|enc|cipher|out|in|db))\b"
)
_HEX_ONLY_RE: Final = re.compile(r"^[0-9a-fA-F]{2,16}$")
_META_DIR_WORDS: Final = frozenset({
    "cwd", "pwd", "home", "tmp", "temp", "root", "usr", "etc", "var",
    "opt", "bin", "lib", "dev", "proc", "sys", "challenge", "ctf",
    "files", "file", "directory", "dir", "path", "readme", "license",
})


def _artifact_basename(raw: str) -> str:
    text = str(raw or "").strip().strip("'\"`")
    if not text:
        return ""
    name = Path(text).name.strip()
    return name


def _looks_like_hex_fragment(name: str) -> bool:
    """True for xxd-style short hex tokens (08af, 0e00, 5e6ee)."""
    n = (name or "").strip()
    if not n:
        return True
    if _HEX_ONLY_RE.fullmatch(n):
        return True
    # Mostly-hex short tokens without a real extension.
    if "." not in n and len(n) <= 10:
        hexish = sum(1 for ch in n if ch in "0123456789abcdefABCDEF")
        if hexish >= max(3, len(n) - 1) and hexish / max(1, len(n)) >= 0.8:
            return True
    return False


def _looks_like_pcap_name(name: str) -> bool:
    low = _artifact_basename(name).lower()
    return any(low.endswith(suf) for suf in _PCAP_SUFFIXES)


def _is_usable_artifact_name(name: str) -> bool:
    n = (name or "").strip()
    if not n or len(n) > 80:
        return False
    low = n.lower()
    if low in _SKIP_ARTIFACT_NAMES or low in _ARTIFACT_STOPWORDS:
        return False
    if low in _META_DIR_WORDS:
        return False
    if any(frag in low for frag in _BLOCKED_ARTIFACT_FRAGMENTS):
        return False
    if _looks_like_hex_fragment(n):
        return False
    # Reject pure punctuation / digits.
    if not re.search(r"[a-zA-Z]", n):
        return False
    # Reject tempfile / absolute path debris basenames.
    if low.startswith("nyu-ab-") or low.endswith(".events.jsonl"):
        return False
    return True


def _rank_artifact_name(name: str) -> tuple[int, str]:
    """Prefer real challenge blobs over README/license meta."""
    low = name.lower()
    penalty = 0
    if low in {"readme", "readme.md", "license", "license.md", "changelog"}:
        penalty = 20
    elif low.endswith(".md"):
        penalty = 10
    elif _looks_like_pcap_name(low) or low.endswith(
        (".bin", ".enc", ".elf", ".img", ".dat", ".key")
    ):
        penalty = -8
    elif "." not in low:
        # Bare binary / blob names (e.g. stfu, key) rank highest.
        penalty = -5
    return (penalty, low)


def _normalize_category(category: str) -> str:
    return str(category or "").strip().lower()


def _evidence_blob(
    harvest_rows: list[dict[str, str]] | None = None,
    crypto_clues: list[str] | None = None,
    fact_texts: list[str] | None = None,
) -> str:
    parts: list[str] = []
    for row in harvest_rows or []:
        parts.append(str(row.get("witness") or ""))
        parts.append(str(row.get("fact") or ""))
        for snip in row.get("signals") or []:
            parts.append(str(snip))
    for clue in crypto_clues or []:
        parts.append(str(clue))
    for fact in fact_texts or []:
        parts.append(str(fact))
    return " ".join(parts).lower()


def has_binary_cipher_evidence(
    harvest_rows: list[dict[str, str]] | None = None,
    crypto_clues: list[str] | None = None,
    fact_texts: list[str] | None = None,
) -> bool:
    blob = _evidence_blob(harvest_rows, crypto_clues, fact_texts)
    if not blob.strip():
        return False
    return any(marker in blob for marker in _BINARY_CIPHER_MARKERS)


def crypto_hypothesis_eligible(
    *,
    category: str = "",
    named_artifacts: list[str] | None = None,
    harvest_rows: list[dict[str, str]] | None = None,
    crypto_clues: list[str] | None = None,
    fact_texts: list[str] | None = None,
    fact_count: int = 0,
) -> bool:
    """Crypto-hypothesis packet only for crypto cat or binary+cipher evidence."""
    arts = [
        _artifact_basename(a)
        for a in (named_artifacts or [])
        if _is_usable_artifact_name(_artifact_basename(a))
    ]
    if any(_looks_like_pcap_name(a) for a in arts):
        return False
    cat = _normalize_category(category)
    if cat in _FORENSICS_CATEGORIES and any(_looks_like_pcap_name(a) for a in arts):
        return False
    evidence = has_binary_cipher_evidence(harvest_rows, crypto_clues, fact_texts)
    if cat in _CRYPTO_CATEGORIES:
        return int(fact_count) > 0 or evidence or bool(crypto_clues)
    return evidence


def infer_replan_domain(
    *,
    category: str = "",
    named_artifacts: list[str] | None = None,
    harvest_rows: list[dict[str, str]] | None = None,
    crypto_clues: list[str] | None = None,
    fact_texts: list[str] | None = None,
    fact_count: int = 0,
) -> str:
    """Return bootstrap | crypto | forensics_pcap | generic."""
    arts = [
        _artifact_basename(a)
        for a in (named_artifacts or [])
        if _is_usable_artifact_name(_artifact_basename(a))
    ]
    cat = _normalize_category(category)
    blob = _evidence_blob(harvest_rows, crypto_clues, fact_texts)
    pcapish = any(_looks_like_pcap_name(a) for a in arts) or (
        "pcap capture file" in blob or "pcapng" in blob
    )
    if pcapish or (cat in _FORENSICS_CATEGORIES and pcapish):
        return "forensics_pcap"
    if cat in _FORENSICS_CATEGORIES and any(
        a.lower().endswith((".img", ".raw", ".dd", ".e01")) for a in arts
    ):
        # Non-pcap forensics still must not get crypto-XOR pressure.
        if int(fact_count) <= 0 and not blob.strip():
            return "bootstrap"
        return "generic"
    if crypto_hypothesis_eligible(
        category=category,
        named_artifacts=arts,
        harvest_rows=harvest_rows,
        crypto_clues=crypto_clues,
        fact_texts=fact_texts,
        fact_count=fact_count,
    ):
        return "crypto"
    if int(fact_count) <= 0 and not (crypto_clues or []):
        return "bootstrap"
    return "generic"


def collect_named_artifacts(
    shared_graph: Any = None,
    *,
    attachments: list[str] | None = None,
    workspace_root: str | Path | None = None,
    challenge: Any = None,
    description: str = "",
    limit: int = 8,
) -> list[str]:
    """Gather player-facing basenames for the post-interrupt working packet.

    Sources (in order, de-duped): explicit attachments, challenge.attachments,
    shared_graph.challenge.attachments, workspace ``inputs/by-name``, filenames
    mentioned in the challenge brief, backtick / extensioned names in facts.
    Round-13: hex fragments and harvest prose are rejected.
    """
    out: list[str] = []

    def _add(raw: str) -> None:
        name = _artifact_basename(raw)
        if not _is_usable_artifact_name(name):
            return
        if name not in out and name.lower() not in {x.lower() for x in out}:
            out.append(name)

    for item in attachments or []:
        _add(str(item))

    chal = challenge
    if chal is None and shared_graph is not None:
        chal = getattr(shared_graph, "challenge", None)
    if chal is not None:
        for item in getattr(chal, "attachments", None) or []:
            _add(str(item))
        if not description:
            description = str(getattr(chal, "description", "") or "")

    if workspace_root is not None:
        by_name = Path(workspace_root) / "inputs" / "by-name"
        try:
            if by_name.is_dir():
                for link in sorted(by_name.iterdir(), key=lambda p: p.name.lower()):
                    if link.is_file() or link.is_symlink():
                        _add(link.name)
        except OSError:
            pass

    if description:
        for groups in _BRIEF_FILE_RE.findall(description):
            for match in groups:
                if match:
                    _add(match)

    # Graph facts: backticks + extensioned filenames only (not bare hex words).
    for fact in _recent_fact_texts(shared_graph, limit=6):
        for match in _FACT_BACKTICK_RE.findall(fact):
            _add(match)
        for match in _FACT_EXT_FILE_RE.findall(fact):
            _add(match)

    out.sort(key=_rank_artifact_name)
    return out[: max(1, int(limit))]


def _format_named_artifacts(names: list[str]) -> str:
    if not names:
        return "(none provisioned)"
    return ", ".join(f"`{n}`" for n in names[:8])


def _filter_clues(clues: list[str] | None, *, limit: int = 6) -> list[str]:
    """Drop tempfile-path / empty-bash noise that poisoned R12 pcap packets."""
    out: list[str] = []
    for raw in clues or []:
        text = str(raw or "").strip()
        if not text:
            continue
        low = text.lower()
        if "bash completed with no output" in low:
            continue
        if "/private/var/folders/" in low or "/tmp/nyu-ab-" in low:
            continue
        if "nyu-ab-" in low and "pcap" not in low and "elf" not in low:
            continue
        if text not in out:
            out.append(text)
        if len(out) >= limit:
            break
    return out


def build_working_packet(
    shared_graph: Any = None,
    *,
    fact_count: int,
    flag_count: int,
    fruitless_workers: int = 0,
    open_intents: int = 0,
    interrupted_worker: str = "",
    interrupted_goal: str = "",
    running_for_s: float = 0.0,
    last_goals: list[str] | None = None,
    named_artifacts: list[str] | None = None,
    attachments: list[str] | None = None,
    workspace_root: str | Path | None = None,
    crypto_clues: list[str] | None = None,
    harvest_rows: list[dict[str, str]] | None = None,
    category: str = "",
) -> str:
    """Compressed Reason working packet after a fruitless interrupt.

    Lands in ``standing_guidance`` so ``to_reason_summary`` surfaces it without
    dumping full worker transcripts (context-rot countermeasure).

    Round-11/13: post-harvest domain gate — crypto hypothesis only when
    eligible; forensics/pcap gets tshark/tcpdump guidance (no header XOR).
    """
    goals = [
        g for g in (
            last_goals if last_goals is not None
            else recent_attempted_goals(shared_graph)
        )
        if g
    ]
    if interrupted_goal:
        ig = _clip(interrupted_goal, 140)
        if ig and ig not in goals:
            goals = [ig, *goals][:4]
    tried = "; ".join(goals[:4]) if goals else "(none recorded)"
    dead = _recent_dead_ends(shared_graph, limit=3)
    dead_s = "; ".join(dead) if dead else "(none)"
    facts = _recent_fact_texts(shared_graph, limit=4)
    facts_s = "; ".join(facts) if facts else "(none yet)"
    who = interrupted_worker or "worker"
    runtime = f"{running_for_s:.0f}s" if running_for_s else "?"
    artifacts = (
        list(named_artifacts)
        if named_artifacts is not None
        else collect_named_artifacts(
            shared_graph,
            attachments=attachments,
            workspace_root=workspace_root,
        )
    )
    # Keep only usable basenames; tests pass explicit lists.
    artifacts = [
        _artifact_basename(a) for a in artifacts
        if _is_usable_artifact_name(_artifact_basename(a))
    ]
    artifacts_s = _format_named_artifacts(artifacts)
    target = f"`{artifacts[0]}`" if artifacts else "the Named artifact"
    raw_clues = list(crypto_clues) if crypto_clues is not None else extract_crypto_clues(
        harvest_rows, shared_graph, limit=8
    )
    clues = _filter_clues(raw_clues, limit=6)
    domain = infer_replan_domain(
        category=category,
        named_artifacts=artifacts,
        harvest_rows=harvest_rows,
        crypto_clues=clues,
        fact_texts=facts,
        fact_count=fact_count,
    )
    header = (
        f"{PACKET_PREFIX} interrupted={who} after={runtime} "
        f"verified_facts={fact_count} flags={flag_count} "
        f"fruitless_workers={fruitless_workers} open_intents={open_intents}. "
        f"Attempted goals (do NOT paraphrase): {tried}. "
        f"Dead-ends: {dead_s}. "
        f"Known facts: {facts_s}. "
        f"Named artifacts (MUST pick ONE): {artifacts_s}. "
    )
    if domain == "crypto":
        clues_s = "; ".join(clues[:6]) if clues else "(none yet — inspect artifact first)"
        return (
            f"{header}"
            f"Crypto clues (cite ≥1): {clues_s}. "
            "FORBIDDEN: whole-challenge bootstrap; 'solve the challenge'; "
            "'find the flag'; pwd/ls-cwd meta; redoing file→strings→xxd on the "
            "same artifact; any paraphrase of attempted goals. "
            "REQUIRED: ONE NEW falsifiable crypto hypothesis intent that "
            f"names {target} AND quotes one Crypto clue, then tests it "
            "(xor/LFSR/known-plaintext/decode a named constant or string). "
            "State the expected observable. Keep the goal ≤35 words."
        )
    if domain == "forensics_pcap":
        clues_s = "; ".join(clues[:6]) if clues else "(pcap type/stream notes sparse)"
        return (
            f"{header}"
            f"Forensics clues: {clues_s}. "
            "FORBIDDEN: whole-challenge bootstrap; 'solve the challenge'; "
            "'find the flag'; pwd/ls-cwd meta; XOR/LFSR/crypto-hypothesis on "
            "pcap global headers or filename-hash bytes; treating path fragments "
            "as keys; any paraphrase of attempted goals. "
            "REQUIRED: ONE NEW concrete forensics intent that names "
            f"{target} and uses tshark/tcpdump/strings (or tcp stream follow / "
            "file carve) on the capture — recover transferred payloads on the "
            "noted port. State the expected observable. Keep the goal ≤35 words."
        )
    if domain == "generic":
        return (
            f"{header}"
            "FORBIDDEN: whole-challenge bootstrap; 'solve the challenge'; "
            "'find the flag'; pwd/ls-cwd meta; inventing XOR/crypto tests "
            "without binary/cipher evidence; any paraphrase of attempted goals. "
            "REQUIRED: ONE NEW concrete discriminating check that names "
            f"{target} and cites one Known fact, advancing toward a recoverable "
            "artifact or credential. State the expected observable. "
            "Keep the goal ≤35 words."
        )
    # bootstrap
    if any(_looks_like_pcap_name(a) for a in artifacts):
        return (
            f"{header}"
            "FORBIDDEN: whole-challenge bootstrap; 'solve the challenge'; "
            "'find the flag'; pwd/ls-cwd/print-path meta; XOR on pcap headers; "
            "splitting inspection across workers; any paraphrase of attempted goals. "
            "REQUIRED: ONE NEW concrete intent that in the SAME worker runs "
            f"file, then tshark/tcpdump, then strings on {target} before any other "
            "exploration. Do NOT split those checks into separate intents. "
            "Keep the goal ≤30 words."
        )
    return (
        f"{header}"
        "FORBIDDEN: whole-challenge bootstrap; 'solve the challenge'; "
        "'find the flag'; pwd/ls-cwd/print-path meta; generic README hunt "
        "that ignores Named artifacts; splitting file/strings/xxd across "
        "workers; any paraphrase of attempted goals. "
        "REQUIRED: ONE NEW concrete intent that in the SAME worker runs "
        f"file, then strings, then xxd/hexdump on {target} before any other "
        "exploration. Do NOT split those checks into separate intents. "
        "Keep the goal ≤30 words."
    )


def build_discriminating_constraint(
    *,
    interrupted_goal: str = "",
    fact_count: int = 0,
    named_artifacts: list[str] | None = None,
    crypto_clues: list[str] | None = None,
    harvest_rows: list[dict[str, str]] | None = None,
    category: str = "",
    domain: str | None = None,
) -> str:
    """MUST-priority coordinator directive injected beside the working packet."""
    bad = _clip(interrupted_goal, 100) if interrupted_goal else "solve the whole challenge"
    artifacts = [
        _artifact_basename(a) for a in (named_artifacts or [])
        if _is_usable_artifact_name(_artifact_basename(a))
    ]
    artifacts_s = _format_named_artifacts(artifacts)
    primary = artifacts[0] if artifacts else "a Named artifact"
    clues = _filter_clues(crypto_clues, limit=6)
    dom = (domain or "").strip().lower() or infer_replan_domain(
        category=category,
        named_artifacts=artifacts,
        harvest_rows=harvest_rows,
        crypto_clues=clues,
        fact_count=fact_count,
    )
    clue_hint = (
        f" Cite clue like '{_clip(clues[0], 60)}'."
        if clues
        else ""
    )
    if dom == "crypto":
        return (
            f"{CONSTRAINT_PREFIX} Harvested observations already exist. "
            f"Next intent MUST be a falsifiable crypto test on `{primary}` "
            f"(xor/LFSR/known-plaintext/decode), NOT another file→strings→xxd "
            f"pass, NOT '{bad}'.{clue_hint} "
            "Quote one Crypto clue in the goal; name the expected observable. "
            "Reject vague re-inspection goals."
        )
    if dom == "forensics_pcap":
        return (
            f"{CONSTRAINT_PREFIX} Capture evidence already exists. "
            f"Next intent MUST use tshark/tcpdump/strings (or stream follow) on "
            f"`{primary}` to recover transferred files — NOT XOR/LFSR on the "
            f"pcap global header, NOT filename-hash-as-key, NOT '{bad}'.{clue_hint} "
            "Name the expected observable. Reject crypto-hypothesis goals."
        )
    if dom == "generic":
        return (
            f"{CONSTRAINT_PREFIX} Observations exist but this is not a crypto "
            f"binary. Next intent MUST be a concrete check on `{primary}` citing "
            f"a Known fact — NOT invented XOR/crypto, NOT '{bad}'. "
            "Name the expected observable."
        )
    facts_hint = (
        f"Board still has zero verified facts — run file/strings/xxd on "
        f"`{primary}` first."
        if artifacts and not any(_looks_like_pcap_name(a) for a in artifacts)
        else (
            f"Board still has zero verified facts — run file/tshark/strings on "
            f"`{primary}` first."
            if artifacts
            else "Board still has zero verified facts — pick the smallest check that could create the first one."
        )
    )
    chain = (
        "file→tshark/tcpdump→strings"
        if any(_looks_like_pcap_name(a) for a in artifacts)
        else "file→strings→xxd"
    )
    return (
        f"{CONSTRAINT_PREFIX} After a fruitless mid-flight interrupt, the next "
        f"intent MUST name one of Named artifacts [{artifacts_s}] and in the "
        f"SAME worker run {chain} (do not split). "
        f"NOT '{bad}', NOT pwd/ls-cwd/README-meta, NOT whole-challenge "
        "bootstrap paraphrase. "
        f"{facts_hint} "
        "Reject vague goals that omit the basename."
    )


def packet_has_named_artifact(text: str, *, name: str | None = None) -> bool:
    """True when the packet lists Named artifacts (optionally a specific one)."""
    body = str(text or "")
    start = body.find("Named artifacts (MUST pick ONE):")
    if start < 0:
        return False
    rest = body[start:]
    # Filenames may contain dots (*.pcap); end at the next section, not first '.'.
    end_m = re.search(
        r"\.\s*(?:Crypto clues|Forensics clues|FORBIDDEN)\b",
        rest,
    )
    listed = rest[: end_m.start()] if end_m else rest[:240]
    if "(none provisioned)" in listed:
        return False
    names = re.findall(r"`([^`]+)`", listed)
    if not names:
        return False
    if name is None:
        return True
    want = _artifact_basename(name).lower()
    return any(n.lower() == want for n in names)


def packet_meets_replan_quality(text: str) -> bool:
    """Unit-test / telemetry helper: packet carries replan + named-artifact constraints."""
    body = str(text or "")
    if not body.startswith(PACKET_PREFIX):
        return False
    if "falsifiable" in body or "Crypto clues" in body:
        markers = REQUIRED_PACKET_MARKERS
    elif "Forensics clues" in body or "tshark" in body:
        markers = REQUIRED_PACKET_MARKERS_FORENSICS
    elif "without binary/cipher evidence" in body:
        markers = (
            "do NOT paraphrase",
            "FORBIDDEN",
            "REQUIRED",
            "whole-challenge bootstrap",
            "Named artifacts",
        )
    else:
        markers = REQUIRED_PACKET_MARKERS_BOOTSTRAP
    if not all(marker in body for marker in markers):
        return False
    return packet_has_named_artifact(body)


__all__ = [
    "DEFAULT_HARD_CAP_S",
    "DEFAULT_MAX_INTERRUPTS",
    "DEFAULT_MAX_REBOOTSTRAPS",
    "DEFAULT_SETTLE_S",
    "DEFAULT_SOLE_EXTRA_S",
    "DEFAULT_THRESHOLD_S",
    "DEFAULT_TOOL_STALL_S",
    "DEFAULT_ARTIFACT_EXTRA_S",
    "DEFAULT_EMPTY_REASON_RETRIES",
    "ENV_ARTIFACT_EXTRA_S",
    "ENV_EMPTY_REASON_RETRIES",
    "ENV_EXPLORE",
    "ENV_FLAG",
    "ENV_HARD_CAP_S",
    "ENV_MAX",
    "ENV_REBOOTSTRAP_MAX",
    "ENV_SETTLE_S",
    "ENV_SOLE_EXTRA_S",
    "ENV_THRESHOLD_S",
    "ENV_TOOL_STALL_S",
    "CONSTRAINT_PREFIX",
    "PACKET_PREFIX",
    "REQUIRED_PACKET_MARKERS",
    "REQUIRED_PACKET_MARKERS_BOOTSTRAP",
    "REQUIRED_PACKET_MARKERS_FORENSICS",
    "ENV_HARVEST",
    "artifact_extra_seconds",
    "build_artifact_chain_intent_goal",
    "build_discriminating_constraint",
    "build_working_packet",
    "collect_named_artifacts",
    "commit_harvested_facts",
    "crypto_hypothesis_eligible",
    "enabled",
    "explore_interrupt_enabled",
    "extract_crypto_clues",
    "hard_cap_seconds",
    "has_binary_cipher_evidence",
    "harvest_artifact_tool_facts",
    "harvest_enabled",
    "infer_replan_domain",
    "max_empty_reason_retries",
    "max_interrupts",
    "max_reboots",
    "packet_has_named_artifact",
    "packet_meets_replan_quality",
    "reason_failure_kind",
    "recent_attempted_goals",
    "settle_seconds",
    "should_inject_artifact_chain_intent",
    "should_interrupt_worker",
    "should_rebootstrap_after_reason",
    "should_retry_empty_reason",
    "should_soft_continue_after_retire_miss",
    "sole_extra_seconds",
    "threshold_seconds",
    "tool_stall_seconds",
    "worker_artifact_progress",
    "worker_tool_count",
]
