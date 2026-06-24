"""Cross-solver Insight Bus (§5.3).

When N solvers race the same challenge, they share VERIFIED OBJECTIVE FACTS only
— never guesses or plans (sharing speculation would bias the whole swarm toward
one solver's wrong idea). Three message kinds:

    FactDiscovered  — a confirmed fact (leaked cred, service version/CVE, a
                      decoded intermediate, a recovered offset). Other solvers
                      fold it into their evidence.
    DeadEndMarked   — a refuted path. Other solvers MUST prune it (it lands in
                      their solve-graph's dead-end list -> shown as "do NOT retry").
    FlagFound       — the first valid flag. The swarm cancels the rest.

This is a per-challenge fan-out hub, separate from the global EventBus (which is
for the frontend). A solver publishes here; the bus pushes to every OTHER
solver's inbox. The publishing solver does not receive its own messages back.
"""

from __future__ import annotations

import asyncio
from collections import deque
from dataclasses import dataclass, field
from enum import Enum
from typing import Optional

_HISTORY_CAP = 1000  # bound the backlog so long runs don't grow memory unbounded


class InsightKind(str, Enum):
    FACT = "FactDiscovered"
    DEAD_END = "DeadEndMarked"
    FLAG = "FlagFound"  # one distinct flag landed — siblings note it, keep solving
    ALL_FLAGS_FOUND = "AllFlagsFound"  # the run has collected expected_flags — siblings STOP
    GUIDANCE = "HumanGuidance"  # HITL: a human command/hint injected into the run
    # ── submission gate (rate-limited verifier coordination) ──────────────────
    # Only used when Challenge.verifier_rate_limited is set (chained-submit CTFs
    # like BreachLab Specter, whose verifier has a per-player burn-lockout). They
    # serialize "submit to the target's verifier" across the swarm so concurrent
    # workers don't each burn the shared per-player attempt budget.
    SUBMIT_LOCKED = "SubmitLocked"  # one worker holds the submit lock — others must
    #   NOT run the verifier now (they keep working, just hold off submitting).
    SUBMIT_UNLOCKED = "SubmitUnlocked"  # the holder released the lock; `text` carries
    #   the result summary (accepted / rejected-with-reason) so siblings improve.
    VERIFIER_LOCKED = "VerifierLocked"  # the verifier hit a cooldown/burn-lockout;
    #   `text` carries the seconds remaining. NOBODY submits until it elapses.


@dataclass
class Insight:
    kind: InsightKind
    by: str  # solver_id of the producer ("human" for HITL guidance)
    text: str  # the fact / dead-end reason / flag / human command
    artifact_id: Optional[str] = None  # peekable evidence backing a fact
    action: str = ""  # for GUIDANCE: hint | redirect | pause | resume | focus
    target: str = ""  # for GUIDANCE: "global" or "solver:<id>" (scoping)
    url: str = ""  # for GUIDANCE redirect: a NEW target URL to retarget workers at
    standing: bool = False  # for GUIDANCE: persistent background guidance (VPS/SSH
    #   creds, global constraints) injected into EVERY future worker's prompt, not a
    #   one-shot steer. Distinct from `target` scoping.


@dataclass
class InsightBus:
    """Per-challenge fan-out of verified facts to all participating solvers.

    Each solver registers an asyncio.Queue inbox. publish() pushes to every
    inbox EXCEPT the producer's own. A late subscriber still gets the backlog of
    facts already broadcast (so a solver that joins/cold-starts later is caught
    up), which is why we keep `history`.
    """

    challenge_id: str
    _inboxes: dict[str, asyncio.Queue[Insight]] = field(default_factory=dict)
    # bounded backlog: a late/cold-started subscriber gets the most recent
    # _HISTORY_CAP insights (verified facts/dead-ends rarely exceed this in a run)
    history: deque[Insight] = field(default_factory=lambda: deque(maxlen=_HISTORY_CAP))
    # every distinct flag broadcast so far (dedup, discovery order). Multi-flag:
    # a worker reads this to skip flags teammates already found. `flag` property
    # returns the first for back-compat.
    _flags: list[str] = field(default_factory=list)

    def subscribe(self, solver_id: str) -> asyncio.Queue[Insight]:
        q: asyncio.Queue[Insight] = asyncio.Queue()
        self._inboxes[solver_id] = q
        # catch the newcomer up on everything broadcast so far
        for ins in self.history:
            if ins.by != solver_id:
                q.put_nowait(ins)
        return q

    def unsubscribe(self, solver_id: str) -> None:
        self._inboxes.pop(solver_id, None)

    async def publish(self, ins: Insight) -> None:
        # M1: don't let an operator hint storm (11 identical hints in run-0011)
        # flood the bounded history — each copy evicts a genuinely useful
        # VERIFIED_FACT / DEAD_END off the front of the deque, AND every copy is
        # replayed to every cold-started subscriber. A GUIDANCE identical to the most
        # recent one already in history is dropped (FLAG already dedups via _flags;
        # facts/dead-ends are content-distinct and kept). Live signals
        # (FLAG/SUBMIT_*/VERIFIER_*) are never deduped.
        if ins.kind is InsightKind.GUIDANCE and self._is_duplicate_guidance(ins):
            return
        self.history.append(ins)
        if ins.kind is InsightKind.FLAG and ins.text and ins.text not in self._flags:
            self._flags.append(ins.text)
        for sid, q in self._inboxes.items():
            if sid != ins.by:
                await q.put(ins)

    def _is_duplicate_guidance(self, ins: "Insight") -> bool:
        """True if an identical GUIDANCE (same text/action/target/url/standing) is
        already the most recent GUIDANCE in history — i.e. a re-send with nothing
        new. Only scans back over trailing GUIDANCE so a hint that genuinely changed
        is published, and a hint repeated after other activity still re-broadcasts."""
        for prev in reversed(self.history):
            if prev.kind is not InsightKind.GUIDANCE:
                return False  # most recent guidance is older than other activity
            if (prev.text == ins.text and prev.action == ins.action
                    and prev.target == ins.target and prev.url == ins.url
                    and prev.standing == ins.standing):
                return True
        return False

    # convenience producers ------------------------------------------------
    async def fact(self, by: str, text: str, artifact_id: Optional[str] = None) -> None:
        await self.publish(Insight(InsightKind.FACT, by, text, artifact_id))

    async def dead_end(self, by: str, reason: str) -> None:
        await self.publish(Insight(InsightKind.DEAD_END, by, reason))

    async def flag_found(self, by: str, flag: str) -> None:
        await self.publish(Insight(InsightKind.FLAG, by, flag))

    async def all_flags_found(self, by: str, *, count: int = 0) -> None:
        """The run has collected every expected flag — tell siblings to STOP. A
        single-flag run (expected_flags=1) fires this immediately after the first
        FLAG, so sibling behaviour is identical to the old 'first flag wins'."""
        await self.publish(Insight(InsightKind.ALL_FLAGS_FOUND, by, str(count)))

    async def submit_locked(self, by: str) -> None:
        """A worker has acquired the global verifier-submit lock and is about to
        run the target's verifier. Tell every other worker to HOLD their own
        submission (they keep working — recon/refine — they just don't submit)."""
        await self.publish(Insight(InsightKind.SUBMIT_LOCKED, by, ""))

    async def submit_unlocked(self, by: str, result: str = "") -> None:
        """The submit-lock holder released it. `result` summarizes the verifier's
        verdict (accepted, or rejected-with-reason + the exact answer it tried) so
        siblings fold it in and don't re-submit the same losing answer."""
        await self.publish(Insight(InsightKind.SUBMIT_UNLOCKED, by, result))

    async def verifier_locked(self, by: str, seconds: float) -> None:
        """The verifier hit a cooldown / burn-lockout. Broadcast the remaining
        seconds; coordinator records a global `verifier_locked_until` and refuses
        to grant the submit-lock until it elapses, so the swarm stops burning the
        per-player attempt budget and spends the cooldown refining the answer."""
        await self.publish(Insight(InsightKind.VERIFIER_LOCKED, by, str(int(max(0, seconds)))))

    async def guidance(self, text: str, *, action: str = "hint",
                       target: str = "global", by: str = "human",
                       url: str = "", standing: bool = False) -> None:
        """HITL: broadcast a human command/hint to every solver. `by="human"`
        means publish() fans it out to ALL solvers (none are skipped, since the
        producer-skip check is by solver_id).

        `url`: a redirect can carry a NEW target URL (the operator says "the
        challenge moved here"). `standing`: persistent background guidance (e.g.
        "use this VPS: ssh root@…") that every FUTURE worker must see — the bus
        replays it to late/cold-started subscribers via `history`, and the
        coordinator folds it into each new worker's prompt."""
        await self.publish(Insight(InsightKind.GUIDANCE, by, text,
                                   action=action, target=target, url=url,
                                   standing=standing))

    @property
    def flag(self) -> Optional[str]:
        return self._flags[0] if self._flags else None

    @property
    def flags(self) -> list[str]:
        return list(self._flags)
