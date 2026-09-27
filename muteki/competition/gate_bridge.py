"""把标准 Run Gate 的确认结果投影为比赛提交候选。"""

from __future__ import annotations

import logging
from pathlib import Path
from typing import Any, Iterable, Optional

from muteki.competition.models import RunBinding, SubmissionCandidate, flag_digest
from muteki.competition.submission import (
    CandidateRejectedError,
    SOURCE_ARTIFACT,
    SOURCE_EXECUTION_OUTPUT,
    SOURCE_RUN_EVENT,
    SubmissionService,
)
from muteki.competition.store import CompetitionStore
from muteki.core.events import Event, EventType


LOG = logging.getLogger(__name__)


class CompetitionRunWitnessResolver:
    """从 Run 的持久事件或工作区 Artifact 解析规范来源证据。"""

    _EVENT_KINDS = {
        EventType.TERMINAL_OUTPUT,
        EventType.TOOL_CALL_RESULT,
        EventType.AGENT_RUNTIME_EVENT,
    }

    def __init__(self, store: CompetitionStore, run_manager: Any) -> None:
        self.store = store
        self.run_manager = run_manager

    def __call__(
        self,
        *,
        challenge_id: str,
        value: str,
        source_run_id: str,
        source_kind: str,
        witness: str,
        source_execution_generation: int = 0,
        source_worker_id: str = "",
        source_session_id: str = "",
        witness_artifact_path: str = "",
        **_unused: Any,
    ) -> str:
        bindings = self.store.list(
            RunBinding,
            run_id=source_run_id,
            competition_challenge_id=challenge_id,
        )
        if not bindings:
            return ""
        if source_execution_generation:
            oldest = min(int(item.execution_generation) for item in bindings)
            if int(source_execution_generation) < oldest:
                return ""
        if source_kind == SOURCE_ARTIFACT:
            return self._artifact_witness(
                source_run_id, witness_artifact_path, value)
        if source_kind not in {SOURCE_EXECUTION_OUTPUT, SOURCE_RUN_EVENT}:
            return ""
        run = self.run_manager.get(source_run_id)
        if run is None:
            return ""
        for raw in reversed(run.store.load_all(source_run_id)):
            try:
                event = Event.model_validate(raw)
            except (TypeError, ValueError):
                continue
            if event.event_type not in self._EVENT_KINDS:
                continue
            generation = int(
                (event.payload or {}).get("execution_generation") or 0)
            if (
                source_execution_generation
                and generation
                and generation < int(source_execution_generation)
            ):
                continue
            if source_worker_id and event.solver_id not in {
                None, "", source_worker_id,
            }:
                continue
            session_id = str(
                (event.payload or {}).get("session_id")
                or (event.payload or {}).get("session")
                or ""
            )
            if source_session_id and session_id and session_id != source_session_id:
                continue
            serialized = event.model_dump_json()
            if value in serialized:
                return serialized
        return ""

    def _artifact_witness(
        self, run_id: str, artifact_path: str, value: str
    ) -> str:
        raw = str(artifact_path or "").strip()
        if not raw:
            return ""
        root = Path(self.run_manager.workspace_dir(run_id)).resolve()
        candidate = Path(raw)
        if not candidate.is_absolute():
            candidate = root / candidate
        try:
            resolved = candidate.resolve(strict=True)
            resolved.relative_to(root)
        except (OSError, ValueError):
            return ""
        if not resolved.is_file() or resolved.stat().st_size > 2 * 1024 * 1024:
            return ""
        try:
            content = resolved.read_text(encoding="utf-8", errors="replace")
        except OSError:
            return ""
        return content if value in content else ""


class CompetitionGateBridge:
    """把模型通过 Blackboard Skill 提交的 Flag 登记为比赛候选。

    Skill 仍是唯一的 Flag 接收入口。本投影只接受 ``flag.accepted``
    或 ``blackboard.delta(kind=flag_found)``，并且要求来源 Run 已由
    ``RunBinding`` 绑定到比赛题目。重复事件依靠候选的题目/槽位/摘要唯一键
    幂等收敛。
    """

    def __init__(
        self,
        store: CompetitionStore,
        submissions: SubmissionService,
        run_manager: Any,
    ) -> None:
        self.store = store
        self.submissions = submissions
        self.run_manager = run_manager
        self.projected = 0
        self.rejected = 0
        self.last_error = ""

    @staticmethod
    def _flag_from(event: Event) -> Optional[str]:
        payload = dict(event.payload or {})
        if event.event_type is EventType.FLAG_ACCEPTED:
            value = payload.get("flag")
            return value if type(value) is str else None
        if (
            event.event_type is EventType.BLACKBOARD_DELTA
            and payload.get("kind") == "flag_found"
        ):
            value = payload.get("flag")
            return value if type(value) is str else None
        return None

    def _binding(self, run_id: str) -> RunBinding | None:
        matches = self.store.list(RunBinding, run_id=run_id)
        if not matches:
            return None
        return max(
            matches,
            key=lambda item: (
                int(item.execution_generation), item.updated_at, item.binding_id
            ),
        )

    @staticmethod
    def _event_generation(event: Event) -> int:
        payload = dict(event.payload or {})
        for raw in (
            payload.get("execution_generation"),
            getattr(event, "execution_generation", None),
        ):
            try:
                value = int(raw or 0)
            except (TypeError, ValueError):
                continue
            if value > 0:
                return value
        return 0

    def _follow_run_generation(self, run_id: str, generation: int) -> bool:
        """把绑定代数追到 Run 当前代；paused 也跟，不挡 Flag 投影。"""
        binding = self._binding(run_id)
        if binding is None:
            return False
        target = max(0, int(generation or 0))
        current = int(binding.execution_generation or 0)
        if target <= current:
            return False
        self.store.save(binding.model_copy(update={
            "execution_generation": target,
        }))
        return True

    async def consume(self, event: Event) -> None:
        payload = dict(event.payload or {})
        event_generation = self._event_generation(event)
        if event_generation:
            self._follow_run_generation(event.run_id, event_generation)
        flag = self._flag_from(event)
        if flag is None:
            return
        binding = self._binding(event.run_id)
        if binding is None:
            return
        generation = event_generation or int(binding.execution_generation or 0)
        # 只丢掉比当前绑定更旧的一代。续跑后的更新一代要投影。
        if generation and generation < int(binding.execution_generation):
            return
        if self.store.list(
            SubmissionCandidate,
            competition_challenge_id=binding.competition_challenge_id,
            answer_slot=1,
            digest=flag_digest(flag),
        ):
            return
        try:
            self.submissions.register_candidate(
                binding.competition_challenge_id,
                flag,
                source_run_id=event.run_id,
                source_kind=SOURCE_RUN_EVENT,
                witness=flag,
                gate_verdict=(
                    f"model-submitted:{event.event_type.value}:"
                    f"seq:{int(event.seq or 0)}"
                ),
                source_execution_generation=generation,
                source_worker_id=str(
                    payload.get("actor") or event.solver_id or ""
                ),
                source_session_id=str(
                    payload.get("session_id") or payload.get("session") or ""
                ),
                shared_graph_fact_id=str(
                    payload.get("fact_id") or payload.get("shared_graph_fact_id") or ""
                ),
            )
        except CandidateRejectedError as exc:
            self.rejected += 1
            self.last_error = f"{exc.reason}: {exc}"
            LOG.warning(
                "competition Gate projection rejected run=%s seq=%s reason=%s",
                event.run_id,
                event.seq,
                exc.reason,
            )
            return
        except Exception as exc:
            self.last_error = f"{type(exc).__name__}: {exc}"
            LOG.exception(
                "competition Gate projection failed run=%s seq=%s",
                event.run_id,
                event.seq,
            )
            return
        self.projected += 1
        self.last_error = ""

    async def recover(self, runs: Iterable[Any]) -> dict[str, Any]:
        """重放已绑定 Run 的持久事件；候选唯一键保证恢复幂等。"""
        before = self.projected
        scanned = 0
        aligned = 0
        bound_ids = {item.run_id for item in self.store.list(RunBinding)}
        for run in runs:
            run_id = str(getattr(run, "run_id", "") or "")
            try:
                generation = int(getattr(run, "execution_generation", 0) or 0)
            except (TypeError, ValueError):
                generation = 0
            if run_id and generation and self._follow_run_generation(
                    run_id, generation):
                aligned += 1
        for run in runs:
            if str(getattr(run, "run_id", "")) not in bound_ids:
                continue
            store = getattr(run, "store", None)
            if store is None:
                continue
            for event in store.iter_matching_events(
                run.run_id,
                event_types=(EventType.FLAG_ACCEPTED.value,),
                payload_kinds=("flag_found",),
            ):
                scanned += 1
                await self.consume(event)
        return {
            "scanned_events": scanned,
            "projected": self.projected - before,
            "rejected": self.rejected,
            "generations_aligned": aligned,
            "last_error": self.last_error,
        }

    def health(self) -> dict[str, Any]:
        return {
            "state": "degraded" if self.last_error else "ready",
            "projected": self.projected,
            "rejected": self.rejected,
            "last_error": self.last_error,
        }


__all__ = ["CompetitionGateBridge", "CompetitionRunWitnessResolver"]
