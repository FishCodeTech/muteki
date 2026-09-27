"""Durable, user-facing progress summaries derived from admitted run events."""

from __future__ import annotations

import asyncio
import hashlib
import json
import logging
import time
from collections import OrderedDict, deque
from typing import Any, Optional, TYPE_CHECKING

from muteki.core.events import Event, EventType

if TYPE_CHECKING:
    from apps.web.run_state import Run


LOG = logging.getLogger("apps.web.run_progress")

_PHASES = {
    "draft", "racing", "running", "collecting", "paused",
    "solved", "goal_met", "finished", "failed",
}


def _text(value: Any, limit: int = 260) -> str:
    cleaned = " ".join(str(value or "").split()).strip()
    return cleaned[:limit]


def _ref(kind: str, value: Any) -> dict[str, str]:
    return {"kind": kind, "id": str(value)}


def _item(
    text: Any = "", *, ref: Optional[dict[str, str]] = None,
    text_key: str = "",
) -> dict[str, Any]:
    row: dict[str, Any] = {}
    if text_key:
        row["text_key"] = text_key
    else:
        row["text"] = _text(text)
    if ref:
        row["ref"] = ref
    return row


class RunProgressPublisher:
    """Coalesce meaningful run changes into replay-safe progress events.

    The publisher is an EventBus sink, so it never emits recursively from the
    sink call itself. A short timer publishes after the current event has left
    the bus lock. Milestones bypass the normal interval; unchanged projections
    are skipped.
    """

    COALESCE_SECONDS = 12.0
    MIN_INTERVAL_SECONDS = 45.0
    MAX_DELAY_SECONDS = 120.0

    def __init__(self, run: "Run") -> None:
        self.run = run
        self.mode = "ctf"
        self.expected_flags: Optional[int] = None
        self.multi_flag = False
        self.platform_confirmation_required = False
        self.phase = "draft"
        self._confirmed: deque[dict[str, Any]] = deque(maxlen=16)
        self._blocked: deque[dict[str, Any]] = deque(maxlen=16)
        self._intents: "OrderedDict[str, dict[str, Any]]" = OrderedDict()
        self._candidates: "OrderedDict[str, dict[str, Any]]" = OrderedDict()
        self._facts: "OrderedDict[str, str]" = OrderedDict()
        self._dirty_since: Optional[float] = None
        self._last_event_seq = 0
        self._last_published_seq = 0
        self._last_published_at = 0.0
        self._last_source_hash = ""
        self._reason_summary = ""
        self._reason_sections: dict[str, list[str]] = {
            "confirmed": [], "active": [], "blocked": [], "next": [],
        }
        self._summary_error = ""
        self._timer: Optional[asyncio.TimerHandle] = None
        self._lock = asyncio.Lock()
        self._closed = False
        self._urgent_kind: Optional[str] = None

    def close(self) -> None:
        self._closed = True
        if self._timer is not None:
            self._timer.cancel()
            self._timer = None

    @staticmethod
    def _same_item(left: dict[str, Any], right: dict[str, Any]) -> bool:
        return (
            left.get("ref") == right.get("ref")
            and left.get("text") == right.get("text")
            and left.get("text_key") == right.get("text_key")
        )

    def _append_unique(self, rows: deque[dict[str, Any]], row: dict[str, Any]) -> bool:
        if not row.get("text") and not row.get("text_key"):
            return False
        existing = next((item for item in rows if self._same_item(item, row)), None)
        if existing is not None:
            return False
        rows.append(row)
        return True

    @staticmethod
    def _put_bounded(
        rows: "OrderedDict[str, dict[str, Any]]", key: str,
        row: dict[str, Any], limit: int = 32,
    ) -> bool:
        previous = rows.get(key)
        changed = previous != row
        rows[key] = row
        rows.move_to_end(key)
        while len(rows) > limit:
            rows.popitem(last=False)
        return changed

    def _remember_fact(self, fact_seq: Any, fact: Any) -> None:
        key = str(fact_seq or "")
        value = _text(fact)
        if not key or not value:
            return
        self._facts[key] = value
        self._facts.move_to_end(key)
        while len(self._facts) > 512:
            self._facts.popitem(last=False)

    def _fact_row(self, payload: dict[str, Any], fallback_seq: int) -> dict[str, Any]:
        fact_seq = payload.get("fact_seq") or fallback_seq
        fact = _text(payload.get("fact")) or self._facts.get(str(fact_seq), "")
        return _item(fact, ref=_ref("fact", fact_seq))

    def _set_phase(self, raw: Any) -> bool:
        token = _text(raw, 40).lower()
        aliases = {
            "race": "racing", "race_scout": "racing",
            "reason": "running", "explore": "running", "dispatch": "running",
            "collect": "collecting", "complete": "finished",
        }
        phase = aliases.get(token, token)
        if phase not in _PHASES or phase == self.phase:
            return False
        self.phase = phase
        return True

    def _observe_blackboard(self, ev: Event) -> tuple[bool, Optional[str]]:
        p = ev.payload
        kind = str(p.get("kind") or "")
        changed = False
        urgent: Optional[str] = None

        if kind == "phase_transition":
            changed |= self._set_phase(p.get("to"))
        elif kind in {"race_started", "race_runtime_window_started"}:
            changed |= self._set_phase("racing")
        elif kind == "race_concluded":
            changed |= self._set_phase("running")
        elif kind == "intent_proposed":
            intent_id = str(p.get("intent_id") or ev.seq)
            changed |= self._put_bounded(self._intents, intent_id, {
                "text": _text(p.get("goal")),
                "ref": _ref("intent", intent_id),
                "status": "proposed",
            })
        elif kind == "intent_claimed":
            intent_id = str(p.get("intent_id") or "")
            row = self._intents.get(intent_id, {
                "text": _text(p.get("goal")) or f"intent {intent_id}",
                "ref": _ref("intent", intent_id or ev.seq),
            })
            changed |= self._put_bounded(
                self._intents, intent_id or str(ev.seq), {**row, "status": "claimed"})
        elif kind in {"intent_concluded", "intent_state_changed"}:
            ids = str(p.get("intent_id") or "").split(",")
            closed = kind == "intent_concluded" or str(p.get("dispatch_state") or "") in {"closed", "retired"}
            for intent_id in (value.strip() for value in ids):
                if intent_id and intent_id in self._intents and closed:
                    row = self._intents[intent_id]
                    changed |= self._put_bounded(
                        self._intents, intent_id, {**row, "status": "done"})
        elif kind == "intent_reopened":
            intent_id = str(p.get("intent_id") or "")
            if intent_id in self._intents:
                row = self._intents[intent_id]
                changed |= self._put_bounded(
                    self._intents, intent_id, {**row, "status": "proposed"})
        elif kind == "fact_added":
            fact_seq = p.get("fact_seq") or ev.seq
            self._remember_fact(fact_seq, p.get("fact"))
            row = self._fact_row(p, ev.seq)
            key = f"fact:{fact_seq}"
            if bool(p.get("verified")):
                self._candidates.pop(key, None)
                changed |= self._append_unique(self._confirmed, row)
            else:
                changed |= self._put_bounded(self._candidates, key, row)
        elif kind in {"fact_promoted", "fact_revalidated"}:
            fact_seq = p.get("fact_seq") or ev.seq
            row = self._fact_row(p, ev.seq)
            self._candidates.pop(f"fact:{fact_seq}", None)
            changed |= self._append_unique(self._confirmed, row)
        elif kind in {"fact_rejected", "fact_challenged", "fact_superseded"}:
            fact_seq = p.get("fact_seq") or ev.seq
            candidate = self._candidates.pop(f"fact:{fact_seq}", None)
            detail = _text(p.get("reason")) or (candidate or {}).get("text") or f"fact {fact_seq}"
            changed |= self._append_unique(
                self._blocked, _item(detail, ref=_ref("fact", fact_seq)))
        elif kind == "dead_end":
            dead_id = p.get("dead_end_seq") or ev.seq
            changed |= self._append_unique(
                self._blocked,
                _item(p.get("reason"), ref=_ref("dead_end", dead_id)),
            )
        elif kind == "flag_found":
            flag = _text(p.get("flag"), 400)
            changed |= self._append_unique(
                self._confirmed, _item(flag, ref=_ref("flag", flag or ev.seq)))
            changed |= self._set_phase("collecting")
            urgent = "milestone"
        elif kind == "finding_found":
            summary = " · ".join(filter(None, [
                _text(p.get("finding_class"), 80),
                _text(p.get("resource_id"), 120),
            ]))
            changed |= self._append_unique(
                self._confirmed, _item(summary, ref=_ref("finding", ev.seq)))
            urgent = "milestone"
        elif kind == "report_submitted":
            report_id = str(p.get("report_id") or ev.seq)
            changed |= self._put_bounded(
                self._candidates, f"report:{report_id}",
                _item(p.get("title") or report_id, ref=_ref("report", report_id)),
            )
        elif kind in {"report_reproduced", "report_accepted"}:
            report_id = str(p.get("report_id") or ev.seq)
            row = self._candidates.pop(f"report:{report_id}", None) or _item(
                p.get("title") or report_id, ref=_ref("report", report_id))
            changed |= self._append_unique(self._confirmed, row)
            urgent = "milestone" if kind == "report_accepted" else None
        elif kind in {"report_rejected", "report_repro_failed", "report_value_rejected", "finding_rejected"}:
            report_id = str(p.get("report_id") or ev.seq)
            self._candidates.pop(f"report:{report_id}", None)
            changed |= self._append_unique(
                self._blocked,
                _item(p.get("reason") or p.get("detail") or kind,
                      ref=_ref("report", report_id)),
            )
        elif kind in {"awaiting_operator", "need_input"}:
            changed |= self._set_phase("paused")
            changed |= self._append_unique(
                self._blocked,
                _item(p.get("reason") or p.get("need"), ref=_ref("event", ev.seq)),
            )
            urgent = "blocker"
        elif kind == "planner_unavailable":
            detail = _text(p.get("detail") or "Planner unavailable", 500)
            changed |= self._set_phase("failed")
            changed |= self._append_unique(
                self._blocked,
                _item(
                    detail,
                    ref=_ref("event", ev.seq),
                ),
            )
            changed |= detail != self._summary_error
            self._summary_error = detail
            urgent = "blocker"
        elif kind in {"operator_resumed", "operator_thawed"}:
            changed |= self._set_phase("running")
        elif kind == "goal_complete":
            changed |= self._set_phase("goal_met")
            changed |= self._append_unique(
                self._confirmed,
                _item(p.get("why"), ref=_ref("event", ev.seq)),
            )
            urgent = "milestone"
        elif kind == "all_flags_found":
            changed |= self._set_phase("solved")
            urgent = "milestone"
        elif kind == "reason_done":
            summary = _text(p.get("progress_summary"), 1600)
            raw_sections = p.get("progress_sections")
            sections: dict[str, list[str]] = {}
            for key in ("confirmed", "active", "blocked", "next"):
                values = raw_sections.get(key) if isinstance(raw_sections, dict) else []
                rows: list[str] = []
                if isinstance(values, list):
                    for value in values:
                        row = _text(value, 500)
                        if row and row not in rows:
                            rows.append(row)
                        if len(rows) >= 3:
                            break
                sections[key] = rows
            if summary:
                changed |= summary != self._reason_summary
                changed |= sections != self._reason_sections
                changed |= bool(self._summary_error)
                self._reason_summary = summary
                self._reason_sections = sections
                self._summary_error = ""
            else:
                detail = _text(
                    p.get("error") or p.get("planner_failure_detail")
                    or "Reason 未返回可用的进展总结",
                    500,
                )
                changed |= detail != self._summary_error
                self._summary_error = detail

        return changed, urgent

    async def observe(self, ev: Event) -> None:
        if self._closed or ev.event_type is EventType.PROGRESS_BRIEF:
            return
        changed = False
        urgent: Optional[str] = None
        p = ev.payload

        if ev.event_type in {
            EventType.RUN_PREPARING, EventType.RUN_STARTED, EventType.RUN_FINISHED,
        }:
            contract = p if ev.event_type is EventType.RUN_FINISHED else (p.get("challenge") or {})
            expected = contract.get("expected_flags")
            if type(expected) is int and expected > 0:
                self.expected_flags = expected
            if isinstance(contract.get("multi_flag"), bool):
                self.multi_flag = contract["multi_flag"]
            if isinstance(contract.get("platform_confirmation_required"), bool):
                self.platform_confirmation_required = contract["platform_confirmation_required"]

        if ev.event_type in {EventType.RUN_PREPARING, EventType.RUN_STARTED}:
            challenge = p.get("challenge") or {}
            self.mode = "pentest" if challenge.get("mode") == "pentest" else "ctf"
            changed |= self._set_phase("running")
            changed = True
        elif ev.event_type is EventType.RUN_REOPENED:
            changed |= self._set_phase("running")
            changed = True
            urgent = "milestone"
        elif ev.event_type is EventType.REASON_INTENT:
            for raw in p.get("intents") or []:
                if not isinstance(raw, dict):
                    continue
                intent_id = str(raw.get("id") or raw.get("intent_id") or ev.seq)
                changed |= self._put_bounded(self._intents, intent_id, {
                    "text": _text(raw.get("goal")),
                    "ref": _ref("intent", intent_id),
                    "status": "proposed",
                })
            if p.get("goal_met"):
                changed |= self._set_phase("goal_met")
                urgent = "milestone"
        elif ev.event_type is EventType.BLACKBOARD_DELTA:
            changed, urgent = self._observe_blackboard(ev)
        elif ev.event_type is EventType.HITL_REQUEST:
            changed |= self._set_phase("paused")
            changed |= self._append_unique(
                self._blocked,
                _item(p.get("need") or p.get("text"), ref=_ref("event", ev.seq)),
            )
            urgent = "blocker"
        elif ev.event_type is EventType.STALLED:
            changed |= self._append_unique(
                self._blocked,
                _item(p.get("reason") or p.get("detail") or "stalled",
                      ref=_ref("event", ev.seq)),
            )
            urgent = "stalled"
        elif ev.event_type is EventType.RUN_FINISHED:
            flags = p.get("flags") or ([p.get("flag")] if p.get("flag") else [])
            final_flags: list[str] = []
            for flag in flags:
                value = _text(flag, 400)
                if value and value not in final_flags:
                    final_flags.append(value)
                changed |= self._append_unique(
                    self._confirmed,
                    _item(value, ref=_ref("flag", value or ev.seq)),
                )
            solved_phase = "goal_met" if self.mode == "pentest" else "solved"
            if p.get("solved"):
                final_phase = solved_phase
            elif p.get("reason") == "runtime_failure":
                final_phase = "failed"
            else:
                final_phase = "finished"
            changed |= self._set_phase(final_phase)

            # A run can finish immediately after its worker reports success, so
            # there may be no second Reason pass after the cold-start summary.
            # Build the terminal brief from admitted terminal/blackboard facts
            # instead of presenting that earlier summary as current state.
            self._reason_sections = {
                "confirmed": [
                    _text(row.get("text"), 500)
                    for row in list(self._confirmed)[-3:]
                    if _text(row.get("text"), 500)
                ],
                "active": [],
                "blocked": [
                    _text(row.get("text"), 500)
                    for row in list(self._blocked)[-3:]
                    if _text(row.get("text"), 500)
                ],
                "next": [],
            }
            if final_phase == "failed":
                self._reason_summary = ""
                self._summary_error = _text(
                    p.get("detail") or p.get("reason")
                    or "本轮执行结束前没有生成可用的 Reason 总结",
                    500,
                )
            else:
                if p.get("solved"):
                    if self.mode == "pentest":
                        summary = "已达到任务目标，本轮执行完成。"
                    elif final_flags:
                        summary = f"已找到并提交 {len(final_flags)} 个 Flag，本轮执行完成。"
                    else:
                        summary = "已完成题目目标，本轮执行完成。"
                elif self.mode == "ctf" and final_flags:
                    count_known = self.expected_flags is not None and (
                        not self.multi_flag or self.expected_flags > 1
                    )
                    summary = f"本轮执行已结束，已收集 {len(final_flags)} 个 Flag"
                    if self.platform_confirmation_required:
                        summary += "；需比赛平台确认后才能判定已解题。"
                    elif count_known and len(final_flags) < self.expected_flags:
                        summary += "，未达到预期数量。"
                    elif count_known:
                        summary += "，已达到预期数量，但运行未标记为已解题。"
                    else:
                        summary += "。"
                elif self.mode == "ctf":
                    summary = "本轮执行已结束，未收集到 Flag。"
                else:
                    summary = "本轮执行已结束，未达到任务目标。"
                self._reason_summary = summary
                self._summary_error = ""
            changed = True
            urgent = "final"

        if not changed:
            return
        self._last_event_seq = max(self._last_event_seq, int(ev.seq or 0))
        now = time.monotonic()
        if self._dirty_since is None:
            self._dirty_since = now
        if urgent:
            self._urgent_kind = urgent
        self._schedule(urgent=bool(urgent))

    def _schedule(self, *, urgent: bool) -> None:
        if self._closed:
            return
        try:
            loop = asyncio.get_running_loop()
        except RuntimeError:
            return
        now = loop.time()
        if urgent:
            due = now
        else:
            dirty_since = self._dirty_since or now
            due = min(
                max(now + self.COALESCE_SECONDS,
                    self._last_published_at + self.MIN_INTERVAL_SECONDS),
                dirty_since + self.MAX_DELAY_SECONDS,
            )
        if self._timer is not None and not self._timer.cancelled():
            if self._timer.when() <= due:
                return
            self._timer.cancel()
        self._timer = loop.call_at(due, self._timer_fired)

    def _timer_fired(self) -> None:
        self._timer = None
        task = asyncio.create_task(self.publish_pending(), name=f"progress-{self.run.run_id}")
        task.add_done_callback(self._publish_done)

    @staticmethod
    def _publish_done(task: asyncio.Task[Any]) -> None:
        try:
            task.result()
        except asyncio.CancelledError:
            pass
        except Exception:
            LOG.exception("progress brief publication failed")

    def _sections(self) -> dict[str, list[dict[str, Any]]]:
        return {
            key: [_item(row) for row in self._reason_sections[key]]
            for key in ("confirmed", "active", "blocked", "next")
        }

    def _projection(self) -> dict[str, Any]:
        sections = self._sections()
        terminal = self.phase in {"solved", "goal_met", "finished", "failed"}
        if self._summary_error:
            summary_status = "failed"
        elif self._reason_summary:
            summary_status = "ready"
        else:
            summary_status = "pending"
        projection = {
            "phase": self.phase,
            "mode": self.mode,
            "summary": self._reason_summary,
            "summary_source": (
                "reason" if self._reason_summary and not self._summary_error else ""
            ),
            "summary_status": summary_status,
            "summary_error": self._summary_error,
            "sections": sections,
            "counts": {
                "confirmed": len(self._confirmed),
                "active": len([row for row in self._intents.values()
                               if row.get("status") == "claimed"]) + len(self._candidates),
                "blocked": len(self._blocked),
                "next": len([row for row in self._intents.values()
                             if row.get("status") == "proposed"]),
            },
        }
        if terminal:
            projection["counts"]["active"] = 0
            projection["counts"]["next"] = 0
        return projection

    async def publish_pending(
        self, *, requested: bool = False, trigger: str = "automatic",
    ) -> Optional[Event]:
        async with self._lock:
            if self._closed:
                return None
            projection = self._projection()
            if (
                projection["summary_status"] == "pending"
                and not requested
                and self._urgent_kind not in {"final", "blocker", "stalled"}
            ):
                return None
            kind = self._urgent_kind or ("requested" if requested else "periodic")
            # Only fields rendered in the conversation should decide whether a
            # new brief exists. Phase and graph counts can change several times
            # during terminal event fan-out while the Reason summary stays
            # identical; treating those internal changes as new content floods
            # the conversation with duplicate cards. Kind remains material so a
            # final summary can replace an otherwise identical periodic update.
            publication = {
                "kind": kind,
                "summary": projection["summary"],
                "summary_source": projection["summary_source"],
                "summary_status": projection["summary_status"],
                "summary_error": projection["summary_error"],
                "sections": projection["sections"],
            }
            material = json.dumps(
                publication, ensure_ascii=False, sort_keys=True,
                separators=(",", ":"),
            ).encode("utf-8")
            source_hash = hashlib.sha256(material).hexdigest()
            if source_hash == self._last_source_hash:
                self._dirty_since = None
                self._urgent_kind = None
                return None
            if not requested and self._dirty_since is None:
                return None

            from_seq = self._last_published_seq + 1 if self._last_published_seq else 1
            to_seq = max(self._last_event_seq, from_seq - 1)
            brief_id = (
                f"pb-{self.run.execution_generation}-{to_seq}-{source_hash[:10]}"
            )
            event = await self.run.bus.emit(Event(
                event_type=EventType.PROGRESS_BRIEF,
                run_id=self.run.run_id,
                solver_id="coordinator",
                payload={
                    **projection,
                    "brief_id": brief_id,
                    "kind": kind,
                    "trigger": trigger,
                    "source_from_seq": from_seq,
                    "source_to_seq": to_seq,
                    "source_hash": source_hash,
                },
            ))
            self._last_source_hash = source_hash
            self._last_published_seq = to_seq
            self._last_published_at = time.monotonic()
            self._dirty_since = None
            self._urgent_kind = None
            return event

    async def request_now(self) -> Optional[Event]:
        if self._timer is not None:
            self._timer.cancel()
            self._timer = None
        if self._dirty_since is None and not self._last_source_hash:
            self._dirty_since = time.monotonic()
        return await self.publish_pending(requested=True, trigger="operator")
