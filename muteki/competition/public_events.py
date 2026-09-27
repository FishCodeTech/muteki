"""CompetitionPublicEventAdapter：内部领域事件 → Public Event。"""

from __future__ import annotations

from typing import Any, Optional

from muteki.platform.contracts.events import EventEnvelope

class CompetitionPublicEventAdapter:
    """内部领域事件 → Public Event 的纯函数适配（无状态）。"""

    def to_public(
        self, seq: int, event: EventEnvelope
    ) -> Optional[dict[str, Any]]:
        """转换一条内部事件；非比赛命名空间的事件不对外暴露（None）。"""
        if not event.event_type.startswith("competition."):
            return None
        payload = dict(event.payload or {})
        public: dict[str, Any] = {
            "seq": int(seq),
            "event_type": event.event_type,
            "occurred_at": event.occurred_at.isoformat(),
            "competition_id": str(
                event.payload.get("competition_id") or ""),
            "aggregate_type": event.aggregate_type,
            "aggregate_id": event.aggregate_id,
            "payload": payload,
        }
        if event.command_id:
            public["command_id"] = event.command_id
        if event.correlation_id:
            public["correlation_id"] = event.correlation_id
        # 子 Run 链接（run_id）提升到顶层，前端直接构造 /run/{id} 跳转。
        run_id = str(payload.get("run_id") or "").strip()
        if run_id:
            public["run_id"] = run_id
        return public

    def to_public_many(
        self, rows: list[tuple[int, EventEnvelope]]
    ) -> list[dict[str, Any]]:
        """批量转换 ``store.read_competition_events`` 的输出。"""
        return [
            public
            for seq, event in rows
            if (public := self.to_public(seq, event)) is not None
        ]


__all__ = ["CompetitionPublicEventAdapter"]
