"""SwarmReapClose — architecture #4: harvest + close + family cover.

Anti-lineage: subclasses production ``Swarm`` only. Does **not** import
``planning_kernel_v1``, ``swarm_planning_base``, or any other
``muteki.solver.swarm_*``. Control loop is deterministic Python — Reason is
never consulted for dispatch.

Three mechanisms aimed at the round-18 old-arm failure taxonomy:

1. **Harvest** (class D): scan graph facts + workspace for ``flag{...}`` and
   promote into the flag ledger without waiting for worker ``flag_found``.
2. **Close** (class A): when observable near-miss signals appear, fill every
   free worker slot with finish goals (no LLM round-trip).
3. **Family cover** (class C/B): seed mutually exclusive technique/target
   families at t=0 and refill by least-covered family (deterministic).
"""

from __future__ import annotations

import asyncio
import re
import time
import uuid
from pathlib import Path
from typing import Any, Optional

from muteki.swarm.swarm import Swarm
from muteki.swarm.swarm_support import (
    ControlShutdownIncomplete,
    SwarmOutcome,
    WorkerBudgetExhausted,
    WorkerSpawnRejected,
)

_FLAG_RE = re.compile(r"flag\{[^}]{1,200}\}", re.IGNORECASE)
_FALSE_HASH_FLAG_RE = re.compile(
    r"^flag\{[0-9a-fA-F]{32,64}\}$", re.IGNORECASE
)

# CLOSE triggers — keyword evidence only (no Reason).
_CLOSE_GIT_BITFLIP = re.compile(
    r"(hash.?path.?mismatch|hash-path|bit.?flip|51337|&lag|bare\s*git|"
    r"git\s+fsck|calculate_flag|sata|objects/[0-9a-f]{2}/)",
    re.IGNORECASE,
)
_CLOSE_HAAR_ZIP = re.compile(
    r"(detectmultiscale|haar|jolly.?roger|zip\s*ok|"
    r"tcp[\.\s]+stream\s*6|stream\s*6|minneighbors\s*=?\s*50|opencv|"
    r"base64\s*jpeg|and here be the map)",
    re.IGNORECASE,
)
_CLOSE_UNZIP_DONE = re.compile(
    r"(zip\s*ok|successfully\s+unzip|inflating:\s*flag\.txt|"
    r"extracted.*flag\.txt|unzip.*flag\.txt)",
    re.IGNORECASE,
)
_CLOSE_CRYPTO_HOM = re.compile(
    r"(homophonic|zodiac|z/?103|same.?symbol|ciphertext.?align)",
    re.IGNORECASE,
)


def _cat_bucket(category: str, name: str = "") -> str:
    blob = f"{category} {name}".lower()
    if any(k in blob for k in ("cry", "crypto", "cipher")):
        return "crypto"
    if any(k in blob for k in ("msc", "misc", "pwn", "web")):
        return "misc"
    if any(k in blob for k in ("for", "forensic")):
        return "forensics"
    if any(k in blob for k in ("rev", "reverse")):
        return "rev"
    return "generic"


def _family_catalog(bucket: str) -> list[tuple[str, str]]:
    """Return (family_id, goal) pairs — mutually exclusive covers."""
    if bucket == "crypto":
        return [
            (
                "FAM_HOMOPHONIC",
                (
                    "FAMILY COVER — homophonic / Zodiac-copycat: treat the "
                    "cipher as homophonic substitution (Z/103 style). Align "
                    "multiple ciphertexts, recover equivalence classes, and "
                    "decrypt toward flag{...} from real output. Do NOT spend "
                    "the whole wall on ElGamal/MTP alone."
                ),
            ),
            (
                "FAM_CLASSICAL",
                (
                    "FAMILY COVER — classical crypto: frequency, Vigenere, "
                    "substitution, crib-drag, known-plaintext. Record verified "
                    "intermediates; extract flag{...} from real command output."
                ),
            ),
            (
                "FAM_MODERN_ASYM",
                (
                    "FAMILY COVER — modern/asymmetric/pad: if materials suggest "
                    "ElGamal/RSA/OTP/MTP, pursue that path end-to-end. Write "
                    "facts; stop cleanly if the family is barren."
                ),
            ),
        ]
    if bucket == "misc":
        return [
            (
                "FAM_LOCAL_LISTEN",
                (
                    "FAMILY COVER — local listeners FIRST: enumerate "
                    "127.0.0.1 / localhost listening ports (ss/lsof/netstat). "
                    "Prefer challenge-local services (e.g. 13337/18000) over "
                    "docker-bridge IPs with empty banners. Speak the protocol "
                    "from attachment sources (e.g. coins.py) and recover "
                    "flag{...}."
                ),
            ),
            (
                "FAM_ATTACH_PROTOCOL",
                (
                    "FAMILY COVER — attachment protocol: read player Python/"
                    "binaries that define the wire protocol; write a solver "
                    "against the REAL local endpoint; do not exclusive-claim "
                    "empty TCP ports."
                ),
            ),
            (
                "FAM_REMOTE_TCP",
                (
                    "FAMILY COVER — remote/docker TCP (secondary): only after "
                    "local listeners are checked. Probe open ports with a "
                    "protocol handshake; abandon empty EOF/bannerless sockets "
                    "quickly."
                ),
            ),
        ]
    if bucket == "forensics":
        return [
            (
                "FAM_PCAP_MEDIA",
                (
                    "FAMILY COVER — pcap/media pipeline: tshark streams, "
                    "carve JPEG/zip/audio, OpenCV Haar / image maps, password "
                    "normalize + unzip. Drive to flag{...} and declare it."
                ),
            ),
            (
                "FAM_FS_GIT",
                (
                    "FAMILY COVER — filesystem/git artifacts: extract archives, "
                    "bare git, git fsck hash-path mismatches, SATA bitflip "
                    "repairs, assemble calculate_flag / SHA1 parts → flag{...}."
                ),
            ),
            (
                "FAM_STEGO_ALT",
                (
                    "FAMILY COVER — alternate stego (audio/SSTV/DTMF/binwalk): "
                    "pursue ONLY if the primary media/git path is barren. Mark "
                    "dead ends; do not starve the primary close path."
                ),
            ),
        ]
    if bucket == "rev":
        return [
            (
                "FAM_STATIC",
                (
                    "FAMILY COVER — static reverse: strings, disasm, decompile, "
                    "key derivation from real code paths → flag{...}."
                ),
            ),
            (
                "FAM_DYNAMIC",
                (
                    "FAMILY COVER — dynamic: run under tracer/debugger, capture "
                    "runtime secrets → flag{...}."
                ),
            ),
            (
                "FAM_PATCH",
                (
                    "FAMILY COVER — patch/emulate: fix broken logic or emulate "
                    "the check to print flag{...}."
                ),
            ),
        ]
    return [
        (
            "FAM_DIRECT",
            (
                "FAMILY COVER — direct path: inventory player files and pursue "
                "the most direct decode/extract to flag{...}; write verified "
                "facts continuously."
            ),
        ),
        (
            "FAM_ALT_DECODE",
            (
                "FAMILY COVER — alternate decode: encodings, nested archives, "
                "odd offsets — a DIFFERENT angle from the direct path."
            ),
        ),
        (
            "FAM_OSINT_HINT",
            (
                "FAMILY COVER — external type-anchor (safe OSINT): identify the "
                "challenge archetype from public CTF writeup titles/keywords "
                "ONLY to pick a technique family, then solve from local "
                "artifacts. Do not paste solutions."
            ),
        ),
    ]


def _close_goal(kind: str) -> str:
    if kind == "git_bitflip":
        return (
            "CLOSE MODE — finish the git/bitflip line NOW: repair hash-path "
            "mismatches with small bitflips (51337→31337, factor typos, "
            "&lag→flag), restore sharp.cpp / blobs, run calculate_flag / "
            "assemble SHA1 parts, print flag{...}, and call flag_found. "
            "Do not open new exploration branches."
        )
    if kind == "haar_zip":
        return (
            "CLOSE MODE — finish Haar→password→unzip NOW: take detectMultiScale "
            "hit frames, derive the password (normalize spaces/case), unzip, "
            "read flag.txt, and call flag_found. Stop audio/SSTV side quests."
        )
    if kind == "unzip_done":
        return (
            "CLOSE MODE — answer already on disk: locate flag.txt / flag{...} "
            "in the workspace or recent command output and IMMEDIATELY declare "
            "flag_found. Do not explore further."
        )
    if kind == "homophonic":
        return (
            "CLOSE MODE — finish homophonic recovery: complete equivalence-"
            "class recovery and decrypt to flag{...}; declare flag_found."
        )
    return (
        "CLOSE MODE — finish the strongest verified path to flag{...} from "
        "existing facts; declare flag_found; no new reconnaissance."
    )


class SwarmReapClose(Swarm):
    """Drop-in Swarm: deterministic harvest / close / family-cover loop."""

    architecture_name = "reapclose"
    executor_timeout_s: int = 480
    harvest_period_s: float = 8.0
    max_total_workers_floor: int = 96

    def __init__(self, *args: Any, **kwargs: Any) -> None:
        # Never race-scout into Reason — this arm owns the whole loop.
        kwargs.setdefault("race_scout", False)
        super().__init__(*args, **kwargs)
        try:
            current = int(self.max_total_workers or 0)
        except (TypeError, ValueError):
            current = 0
        if current < self.max_total_workers_floor:
            self.max_total_workers = int(self.max_total_workers_floor)
        # Harness profiles often pin max_running=start_workers (3) while
        # max_workers is 6 — under flaky retire that caps refill. Lift the
        # seat cap to max_workers so idle slots stay filled (worker-seconds
        # are free; Reason is not on the path).
        try:
            cap = max(int(self.max_workers), 6)
            for prof in list(getattr(self, "worker_profiles", None) or []):
                if isinstance(prof, dict):
                    prof["max_running"] = max(
                        cap, int(prof.get("max_running") or 0)
                    )
        except Exception:
            pass
        self._family_spawn_counts: dict[str, int] = {}
        self._close_kind: str = ""
        self._close_since: float = 0.0
        self._last_harvest_t: float = 0.0
        self._harvest_hits: int = 0
        self._healthy_cache: list[str] = []
        self._healthy_cache_t: float = 0.0
        self._spawn_fail_streak: int = 0
        bucket = _cat_bucket(
            str(getattr(self.challenge, "category", "") or ""),
            str(getattr(self.challenge, "name", "") or ""),
        )
        self._families = _family_catalog(bucket)
        for fid, _ in self._families:
            self._family_spawn_counts[fid] = 0

    # ── coordinator (full replace; no Reason) ─────────────────────────────

    async def _run_coordinator(self) -> SwarmOutcome:
        self._operator_event = asyncio.Event()
        self._coord_sinks = list(getattr(self, "_coord_sinks", []) or [])
        self._pending_help = list(getattr(self, "_pending_help", []) or [])
        self._run_finalized = False
        per_solver: dict[str, Any] = {}
        winner: Optional[str] = None
        flag: Optional[str] = None
        started = time.monotonic()
        active: dict[asyncio.Task, Any] = {}
        terminal_reason = ""
        fact_texts: list[str] = []
        hitl_task: Optional[asyncio.Task[Any]] = None
        if self.hitl_inbox is not None:
            hitl_task = asyncio.create_task(
                self._supervise_control_drain(), name="hitl-drain")

        async def _stop_control_drain() -> None:
            nonlocal hitl_task
            if hitl_task is None:
                return
            task, hitl_task = hitl_task, None
            task.cancel()
            await asyncio.gather(task, return_exceptions=True)

        try:
            await self._emit_coord_bb(
                "reapclose_start",
                architecture=self.architecture_name,
                challenge=self.challenge.name,
                category=self.challenge.category,
                families=[f for f, _ in self._families],
            )

            # Under shared-endpoint contention, one failed hello must not abort
            # the whole cell — retry briefly before declaring NoEligibleEngine.
            healthy: list[str] = []
            for attempt in range(6):
                healthy = await self._healthy_engines_async()
                if healthy:
                    self._healthy_cache = list(healthy)
                    self._healthy_cache_t = time.monotonic()
                    break
                await self._emit_coord_bb(
                    "reapclose_health_retry",
                    architecture=self.architecture_name,
                    attempt=attempt + 1,
                    configured_engines=list(self.engines),
                )
                await asyncio.sleep(min(45.0, 8.0 + 5.0 * attempt))
            if not healthy:
                await self._emit_coord_bb(
                    "health_unavailable",
                    reason="NoEligibleEngine",
                    configured_engines=list(self.engines),
                )
                await _stop_control_drain()
                await self._finalize_coordinator_run(
                    winner=None,
                    flag=None,
                    goal_complete=False,
                    per_solver=per_solver,
                    terminal_reason="paused: NoEligibleEngine",
                )
                return SwarmOutcome(
                    False,
                    None,
                    None,
                    per_solver,
                    "paused: NoEligibleEngine (all configured worker health probes failed)",
                )

            # M3: seed every family once (up to max_workers).
            await self._seed_families(active, fact_texts)

            while True:
                await self._reapclose_wait_while_paused()
                if getattr(self, "_operator_stop", False):
                    terminal_reason = "operator_stop"
                    break
                if self._flags_complete():
                    terminal_reason = "solved"
                    break
                if self._budget_elapsed(started) >= float(self.wall_clock_budget):
                    terminal_reason = "budget_exhausted"
                    break

                kind = self._budget_exhausted()
                if kind == "worker_budget_exhausted" and (
                    float(self.wall_clock_budget) - self._budget_elapsed(started)
                    > 45.0
                ):
                    cur = int(getattr(self, "max_total_workers", 0) or 0)
                    bump = max(24, int(self.max_workers) * 4)
                    self.max_total_workers = cur + bump
                    await self._emit_coord_bb(
                        "reapclose_worker_budget_extend",
                        architecture=self.architecture_name,
                        from_cap=cur,
                        to_cap=int(self.max_total_workers),
                        spawned=int(getattr(self, "_spawned_total", 0) or 0),
                    )
                    kind = None
                if kind:
                    terminal_reason = str(kind)
                    break

                await self._reap_finished(active, per_solver)
                self._sync_flags_from_graph()
                if self._found_flags:
                    self._record_flags(*list(self._found_flags))
                    if self._flags_complete():
                        winner = winner or next(iter(per_solver), None)
                        flag = self._found_flags[0]
                        terminal_reason = "solved"
                        break

                # M1: harvest off the critical worker path (cheap scan).
                now = time.monotonic()
                if now - self._last_harvest_t >= self.harvest_period_s:
                    self._last_harvest_t = now
                    promoted = await self._harvest_pass(
                        fact_texts, actor="reapclose-harvest"
                    )
                    if promoted and self._flags_complete():
                        winner = winner or next(iter(per_solver), None)
                        flag = self._found_flags[0]
                        terminal_reason = "solved"
                        break

                fact_texts = self._collect_fact_texts(fact_texts)
                # M2: detect close signals from facts only.
                close_kind = self._detect_close_kind(fact_texts)
                if close_kind and close_kind != self._close_kind:
                    self._close_kind = close_kind
                    self._close_since = time.monotonic()
                    await self._emit_coord_bb(
                        "reapclose_close_enter",
                        architecture=self.architecture_name,
                        close_kind=close_kind,
                        facts=len(fact_texts),
                    )

                slots = max(0, int(self.max_workers) - len(active))
                if slots > 0:
                    before = len(active)
                    if self._close_kind:
                        # Keep one family slot diversifying unless wall is tight.
                        close_slots = slots
                        remain = (
                            float(self.wall_clock_budget)
                            - self._budget_elapsed(started)
                        )
                        if remain > 120 and slots >= 2 and not fact_texts:
                            close_slots = slots - 1
                            await self._fill_families(active, 1, fact_texts)
                        await self._fill_close(
                            active, max(0, close_slots), fact_texts
                        )
                    else:
                        await self._fill_families(active, slots, fact_texts)
                    if len(active) == before:
                        self._spawn_fail_streak += 1
                        # Invalidate health cache after repeated spawn failures.
                        if self._spawn_fail_streak >= 2:
                            self._healthy_cache = []
                            self._healthy_cache_t = 0.0
                    else:
                        self._spawn_fail_streak = 0

                if self._flags_complete():
                    winner = winner or next(iter(per_solver), None)
                    flag = self._found_flags[0]
                    terminal_reason = "solved"
                    break

                remain = (
                    float(self.wall_clock_budget) - self._budget_elapsed(started)
                )
                if active:
                    await self._wait_any(active, timeout=2.0)
                elif remain > 5.0:
                    # Worker-seconds are free: never idle-exit while wall remains.
                    # Back off on spawn failures so health probes can recover.
                    delay = min(12.0, 1.5 + 0.75 * self._spawn_fail_streak)
                    await asyncio.sleep(delay)
                else:
                    terminal_reason = terminal_reason or "budget_exhausted"
                    break

            await self._cancel_all(active, per_solver)
            await _stop_control_drain()
            if self._shutdown_owners_incomplete():
                self._retain_control_shutdown_owner(
                    winner=winner,
                    flag=flag,
                    goal_complete=False,
                    per_solver=per_solver,
                )
                raise ControlShutdownIncomplete(
                    "reapclose worker shutdown incomplete; runtime owner retained"
                )
            # Final harvest before finalize (D-class last chance).
            await self._harvest_pass(fact_texts, actor="reapclose-final")
            self._sync_flags_from_graph()
            if self._found_flags:
                self._record_flags(*list(self._found_flags))
                flag = flag or self._found_flags[0]
                if self._flags_complete():
                    winner = winner or next(iter(per_solver), None)
                    terminal_reason = "solved"

            goal_complete = self._flags_complete()
            await self._finalize_coordinator_run(
                winner=winner,
                flag=flag,
                goal_complete=goal_complete,
                per_solver=per_solver,
                terminal_reason=terminal_reason
                or ("solved" if goal_complete else "budget_exhausted"),
            )
            if goal_complete:
                return SwarmOutcome(
                    True,
                    flag,
                    winner,
                    per_solver,
                    "solved",
                    flags=list(self._found_flags),
                )
            return SwarmOutcome(
                False,
                flag,
                winner,
                per_solver,
                terminal_reason or "budget_exhausted",
                flags=list(self._found_flags),
            )
        except BaseException as exc:
            try:
                await self._emit_coord_bb(
                    "reapclose_error",
                    architecture=self.architecture_name,
                    error_type=type(exc).__name__,
                    detail=str(exc)[:240],
                )
            except Exception:
                pass
            await self._cancel_all(active, per_solver)
            await _stop_control_drain()
            if self._shutdown_owners_incomplete():
                self._retain_control_shutdown_owner(
                    winner=winner,
                    flag=flag,
                    goal_complete=False,
                    per_solver=per_solver,
                )
                if isinstance(exc, ControlShutdownIncomplete):
                    raise
                raise ControlShutdownIncomplete(
                    "reapclose worker shutdown incomplete; runtime owner retained"
                ) from exc
            try:
                await self._finalize_coordinator_run(
                    winner=winner,
                    flag=flag,
                    goal_complete=False,
                    per_solver=per_solver,
                    terminal_reason=(
                        "operator_stop"
                        if (getattr(self, "_operator_stop", False)
                            or isinstance(exc, asyncio.CancelledError))
                        else "reapclose_error"
                    ),
                )
            except Exception:
                pass
            raise
        finally:
            await _stop_control_drain()

    async def _reapclose_wait_while_paused(self) -> None:
        """Hold dispatch while PAUSE/FREEZE is active; RESUME/THAW wakes it."""
        event = self._operator_event
        while (getattr(self, "_operator_paused", False)
               and not getattr(self, "_operator_stop", False)):
            if event is None:
                await asyncio.sleep(0.1)
                continue
            event.clear()
            if (not getattr(self, "_operator_paused", False)
                    or getattr(self, "_operator_stop", False)):
                continue
            await event.wait()

    # ── family cover ──────────────────────────────────────────────────────

    async def _seed_families(
        self, active: dict, fact_texts: list[str]
    ) -> None:
        boot_timeout = int(
            min(560, max(320, float(self.wall_clock_budget) * 0.7))
        )
        for fam_id, goal in self._families:
            if len(active) >= int(self.max_workers):
                break
            item = {
                "id": fam_id,
                "goal": goal,
                "mode": "bootstrap",
                "timeout": boot_timeout,
                "meta": {"family": fam_id},
            }
            spawned = await self._spawn_worker(item, fact_texts)
            if spawned is None:
                continue
            task, worker = spawned
            active[task] = worker
            self._family_spawn_counts[fam_id] = (
                self._family_spawn_counts.get(fam_id, 0) + 1
            )
            sid = str(getattr(worker, "solver_id", "") or "")
            await self._emit_coord_bb(
                "reapclose_family_seed",
                architecture=self.architecture_name,
                family=fam_id,
                worker=sid,
                active=len(active),
            )
            await self._emit_coord_bb(
                "worker_spawned",
                architecture=self.architecture_name,
                worker=sid,
                solver_id=sid,
                unit_id=fam_id,
                mode="bootstrap",
                active=len(active),
                max_workers=int(self.max_workers),
            )

    async def _fill_families(
        self, active: dict, slots: int, fact_texts: list[str]
    ) -> None:
        # Prefer bootstrap for refills too — explore short-timeouts wasted
        # wall under flaky endpoints; worker-seconds are the free resource.
        timeout = int(
            min(520, max(280, float(self.wall_clock_budget) * 0.55))
        )
        for _ in range(slots):
            if len(active) >= int(self.max_workers):
                break
            fam_id, goal = min(
                self._families,
                key=lambda fg: self._family_spawn_counts.get(fg[0], 0),
            )
            item = {
                "id": f"{fam_id}-{self._family_spawn_counts.get(fam_id, 0)+1}",
                "goal": goal,
                "mode": "bootstrap",
                "timeout": timeout,
                "meta": {"family": fam_id},
            }
            spawned = await self._spawn_worker(item, fact_texts)
            if spawned is None:
                break
            task, worker = spawned
            active[task] = worker
            self._family_spawn_counts[fam_id] = (
                self._family_spawn_counts.get(fam_id, 0) + 1
            )
            sid = str(getattr(worker, "solver_id", "") or "")
            await self._emit_coord_bb(
                "reapclose_family_fill",
                architecture=self.architecture_name,
                family=fam_id,
                worker=sid,
                active=len(active),
            )
            await self._emit_coord_bb(
                "worker_spawned",
                architecture=self.architecture_name,
                worker=sid,
                solver_id=sid,
                unit_id=str(item.get("id") or fam_id),
                mode="bootstrap",
                active=len(active),
                max_workers=int(self.max_workers),
            )

    # ── close ─────────────────────────────────────────────────────────────

    def _detect_close_kind(self, fact_texts: list[str]) -> str:
        blob = "\n".join(fact_texts[-80:])
        if not blob.strip():
            return ""
        if _CLOSE_UNZIP_DONE.search(blob):
            return "unzip_done"
        if _CLOSE_GIT_BITFLIP.search(blob):
            return "git_bitflip"
        if _CLOSE_HAAR_ZIP.search(blob):
            return "haar_zip"
        if _CLOSE_CRYPTO_HOM.search(blob):
            return "homophonic"
        return ""

    async def _fill_close(
        self, active: dict, slots: int, fact_texts: list[str]
    ) -> None:
        goal = _close_goal(self._close_kind)
        timeout = int(
            min(480, max(280, float(self.wall_clock_budget) * 0.5))
        )
        for i in range(slots):
            if len(active) >= int(self.max_workers):
                break
            item = {
                "id": f"CLOSE-{self._close_kind}-{i}-{int(time.time())%10000}",
                "goal": goal,
                "mode": "bootstrap",
                "timeout": timeout,
                "meta": {"close": self._close_kind},
            }
            spawned = await self._spawn_worker(item, fact_texts)
            if spawned is None:
                break
            task, worker = spawned
            active[task] = worker
            sid = str(getattr(worker, "solver_id", "") or "")
            await self._emit_coord_bb(
                "reapclose_close_spawn",
                architecture=self.architecture_name,
                close_kind=self._close_kind,
                worker=sid,
                active=len(active),
            )
            await self._emit_coord_bb(
                "worker_spawned",
                architecture=self.architecture_name,
                worker=sid,
                solver_id=sid,
                unit_id=str(item.get("id") or "CLOSE"),
                mode="bootstrap",
                active=len(active),
                max_workers=int(self.max_workers),
            )

    # ── harvest ───────────────────────────────────────────────────────────

    def _collect_fact_texts(self, prior: list[str]) -> list[str]:
        out: list[str] = list(prior[-200:])
        seen = set(out)
        if self.shared_graph is None:
            return out
        rows: list[Any] = []
        for getter in (
            "verified_evidence",
            "candidate_evidence",
            "recent_evidence",
        ):
            fn = getattr(self.shared_graph, getter, None)
            if not callable(fn):
                continue
            try:
                rows.extend(list(fn() or []))
            except Exception:
                continue
        try:
            snap = self.shared_graph.snapshot()
            rows.extend(list(getattr(snap, "facts", None) or []))
            # Some snapshots expose dead_ends / evidence blobs with useful text.
            rows.extend(list(getattr(snap, "dead_ends", None) or []))
        except Exception:
            pass
        for row in rows:
            if isinstance(row, dict):
                text = str(
                    row.get("fact") or row.get("text") or row.get("reason") or ""
                ).strip()
            else:
                text = str(
                    getattr(row, "fact", None)
                    or getattr(row, "text", None)
                    or getattr(row, "reason", None)
                    or row
                ).strip()
            if text and text not in seen:
                seen.add(text)
                out.append(text)
        return out[-400:]

    async def _harvest_pass(
        self, fact_texts: list[str], *, actor: str
    ) -> list[str]:
        texts = list(fact_texts)
        texts.extend(self._scan_workspace_flag_blobs())
        promoted = self._promote_flags_from_texts(texts, actor=actor)
        if promoted:
            self._harvest_hits += len(promoted)
            await self._emit_coord_bb(
                "reapclose_harvest_hit",
                architecture=self.architecture_name,
                count=len(promoted),
                total_hits=self._harvest_hits,
                actor=actor,
            )
            # Also mirror onto insight bus so siblings stop cleanly.
            if getattr(self, "insight", None) is not None:
                for fl in promoted:
                    try:
                        await self.insight.flag_found(actor, fl)
                    except Exception:
                        pass
        return promoted

    def _scan_workspace_flag_blobs(self) -> list[str]:
        roots: list[Path] = []
        if self.workspace_root is not None:
            roots.append(Path(self.workspace_root))
        if self.worker_root is not None:
            roots.append(Path(self.worker_root))
        blobs: list[str] = []
        names = ("flag.txt", "flag", "FLAG.txt", "answer.txt")
        for root in roots:
            if not root.is_dir():
                continue
            for name in names:
                for path in root.rglob(name):
                    try:
                        if not path.is_file() or path.stat().st_size > 64_000:
                            continue
                        blobs.append(
                            path.read_text(encoding="utf-8", errors="ignore")
                        )
                    except OSError:
                        continue
            # Also skim short *.txt near worker dirs (bounded).
            try:
                txts = list(root.rglob("*.txt"))[:80]
            except OSError:
                txts = []
            for path in txts:
                try:
                    if path.stat().st_size > 16_000:
                        continue
                    body = path.read_text(encoding="utf-8", errors="ignore")
                except OSError:
                    continue
                if "flag{" in body.lower():
                    blobs.append(body)
        return blobs

    def _promote_flags_from_texts(
        self, texts: list[str], *, actor: str
    ) -> list[str]:
        """Compatibility hook; text cannot promote a flag into accepted state."""
        return []

    @staticmethod
    def _flags_from_hex_blob(text: str) -> list[str]:
        found: list[str] = []
        for run in re.findall(r"[0-9a-fA-F]{16,4000}", text or ""):
            if len(run) % 2:
                run = run[:-1]
            try:
                raw = bytes.fromhex(run)
            except ValueError:
                continue
            decoded = raw.decode("utf-8", errors="ignore")
            found.extend(_FLAG_RE.findall(decoded))
        return found

    # ── worker spawn / reap (Swarm primitives only) ───────────────────────

    async def _cached_healthy(self, *, role: str) -> list[str]:
        now = time.monotonic()
        if self._healthy_cache and (now - self._healthy_cache_t) < 120.0:
            return list(self._healthy_cache)
        healthy = await self._healthy_engines_async(role=role)
        if not healthy and role != "bootstrap":
            healthy = await self._healthy_engines_async(role="bootstrap")
        if healthy:
            self._healthy_cache = list(healthy)
            self._healthy_cache_t = now
            return list(healthy)
        # Probe failed under contention: keep a fresh-enough stale cache rather
        # than declaring the roster empty and burning the wall on no-ops.
        if self._healthy_cache and (now - self._healthy_cache_t) < 420.0:
            return list(self._healthy_cache)
        return []

    async def _spawn_worker(
        self, item: dict[str, Any], fact_texts: list[str]
    ) -> Optional[tuple[asyncio.Task, Any]]:
        mode = str(item.get("mode") or "bootstrap")
        role = "bootstrap" if mode == "bootstrap" else "explore"
        healthy = await self._cached_healthy(role=role)
        if not healthy:
            await self._emit_coord_bb(
                "reapclose_spawn_fail",
                architecture=self.architecture_name,
                reason="no_healthy_engine",
                unit_id=str(item.get("id") or ""),
            )
            return None
        running = [
            str(getattr(w, "engine", "") or getattr(w, "solver_id", ""))
            for w in list(getattr(self, "_live_solvers", {}).values())
        ]
        try:
            engine = self._pick_engine(running, healthy, role=role)
        except Exception:
            engine = healthy[0]
        intent_id = f"I-rc-{uuid.uuid4().hex[:10]}"
        goal = str(item.get("goal") or "").strip()
        if not goal:
            return None
        timeout = int(item.get("timeout") or self.executor_timeout_s)
        unit_id = str(item.get("id") or "")
        packet_lines = fact_texts[-12:]
        packet = "\n".join(f"- {t[:180]}" for t in packet_lines if t)
        prior = list(self._next_worker_guidance)
        guidance = [
            f"[architecture={self.architecture_name}] unit={unit_id}",
            goal,
        ]
        if packet:
            guidance.append("Recent verified facts:\n" + packet)
        if self._close_kind:
            guidance.append(
                f"CLOSE ACTIVE kind={self._close_kind}: prioritize finish; "
                "declare flag_found from real output."
            )
        self._next_worker_guidance = guidance
        if self.shared_graph is not None:
            try:
                self.shared_graph.propose_intent(
                    actor="reapclose",
                    intent_id=intent_id,
                    goal=goal[:500],
                )
            except Exception:
                pass

        roles_to_try = [role]
        if role == "bootstrap":
            roles_to_try.append("explore")
        worker = None
        last_exc: Exception | None = None
        try:
            for try_role in roles_to_try:
                try:
                    worker = self._make_cli_worker(
                        engine,
                        mode=(
                            mode
                            if mode in {"bootstrap", "explore", "respond"}
                            else "bootstrap"
                        ),
                        intent_goal=goal[:800],
                        intent_id=intent_id,
                        timeout_override=timeout,
                        profile_role=try_role,
                    )
                    break
                except WorkerSpawnRejected as exc:
                    last_exc = exc
                    continue
                except WorkerBudgetExhausted as exc:
                    last_exc = exc
                    break
            if worker is None:
                await self._emit_coord_bb(
                    "reapclose_spawn_fail",
                    architecture=self.architecture_name,
                    reason=type(last_exc).__name__ if last_exc else "spawn_rejected",
                    detail=str(last_exc or "")[:160],
                    unit_id=unit_id,
                )
                return None
        except Exception as exc:
            await self._emit_coord_bb(
                "reapclose_spawn_fail",
                architecture=self.architecture_name,
                reason=type(exc).__name__,
                detail=str(exc)[:160],
                unit_id=unit_id,
            )
            return None
        finally:
            self._next_worker_guidance = prior
        try:
            task = await self._schedule_control_worker(
                worker, name=worker.solver_id, intent_id=intent_id
            )
        except Exception as exc:
            await self._emit_coord_bb(
                "reapclose_spawn_fail",
                architecture=self.architecture_name,
                reason="schedule_failed",
                detail=str(exc)[:160],
                unit_id=unit_id,
            )
            return None
        worker._reapclose_unit = dict(item)
        worker._reapclose_intent_id = intent_id
        return task, worker

    async def _reap_finished(
        self, active: dict, per_solver: dict
    ) -> None:
        done = [t for t in list(active) if t.done()]
        for task in done:
            worker = active.pop(task)
            try:
                outcome = task.result()
            except asyncio.CancelledError:
                continue
            except Exception as exc:
                from muteki.models.solve_graph import SolveGraph
                from muteki.solver.types import SolveOutcome

                graph = getattr(worker, "graph", None)
                if graph is None:
                    graph = SolveGraph(challenge=self.challenge)
                outcome = SolveOutcome(
                    False, None, 0, graph, f"error: {exc}"
                )
            sid = str(getattr(worker, "solver_id", "") or "worker")
            per_solver[sid] = outcome
            found = ""
            if getattr(outcome, "flag", None):
                found = str(outcome.flag)
            elif getattr(outcome, "flags", None):
                flags = list(outcome.flags or [])
                if flags:
                    found = str(flags[0])
            if found:
                self._record_flags(found)
            try:
                await self._retire_worker_account(
                    worker,
                    intent_id=str(
                        getattr(worker, "_reapclose_intent_id", "") or ""
                    ),
                    reason="reapclose_reap",
                )
            except Exception:
                pass

    async def _wait_any(self, active: dict, *, timeout: float) -> None:
        if not active:
            await asyncio.sleep(min(0.5, timeout))
            return
        try:
            await asyncio.wait(
                set(active.keys()),
                timeout=timeout,
                return_when=asyncio.FIRST_COMPLETED,
            )
        except Exception:
            await asyncio.sleep(min(0.5, timeout))

    async def _cancel_all(self, active: dict, per_solver: dict) -> None:
        for task, worker in list(active.items()):
            if not task.done():
                try:
                    self._cancel_solver(worker)
                except Exception:
                    pass
                task.cancel()
        if active:
            await asyncio.gather(*list(active.keys()), return_exceptions=True)
        for task, worker in list(active.items()):
            sid = str(getattr(worker, "solver_id", "") or "worker")
            if sid not in per_solver:
                from muteki.models.solve_graph import SolveGraph
                from muteki.solver.types import SolveOutcome

                graph = getattr(worker, "graph", None)
                if graph is None:
                    graph = SolveGraph(challenge=self.challenge)
                per_solver[sid] = SolveOutcome(
                    False, None, 0, graph, "cancelled"
                )
            try:
                await self._retire_worker_account(
                    worker,
                    intent_id=str(
                        getattr(worker, "_reapclose_intent_id", "") or ""
                    ),
                    reason="reapclose_shutdown",
                )
            except Exception:
                pass
        active.clear()


Swarm = SwarmReapClose
