"""CompetitionAdvisor：经 ExternalAgentAdapter 运行的比赛顾问（任务书 10.5 /
设计 11.4、15、17.1，COMP-06）。

定位（设计 11.4 逐字）：CompetitionAdvisor 可以读取比赛 projection 和
Run 摘要，提出结构化建议；它没有平台写权限，Scheduler 在没有 Advisor 时
仍能完整运行。

实现口径：

- Advisor 必须通过 ``ExternalAgentSessionExecutor`` 运行，后者只依赖
  RUNTIME-01 ``BaseExternalAgentAdapter`` 的 start → send → close 会话面；
  执行器不持有任何平台 Adapter、SubmissionService 或 CapabilityBinding —
  构造时要求 adapter 不带 ``binding_service``（不签发 grant，无平台写
  权限），Advisor 自身只读 ``store.snapshot()`` 投影。
- 输出为结构化 ``AdvisorAdvice``：选题（select）/ 预算（budget）/
  停止（stop）/ 挂起（hold）四类建议，逐条带 rationale 与 confidence；
  模型回复要求为 JSON，解析失败时 advice 状态落 ``unparseable`` 且不
  产生建议。
- Advisor 停用（``enabled=False`` / 无 executor）或失败（adapter 异常、
  会话不支持 send、超时）时返回 ``disabled`` / ``error`` 状态的空建议；
  确定性 Scheduler 在任何 Advisor 状态下都独立完整工作 — 建议只被记录
  进 ``AdmissionDecision.advisor`` 与 ``competition.advisor.advice``
  事件，不改变确定规则的准入结果。
"""

from __future__ import annotations

import asyncio
import json
from datetime import datetime
from typing import Any, Callable, Optional

from pydantic import Field

from muteki.competition import events as ev
from muteki.competition.store import CompetitionStore, NotFoundError
from muteki.competition.models import Competition
from muteki.external_agents.base import BaseExternalAgentAdapter
from muteki.platform.contracts.base import ContractModel, new_id, utcnow
from muteki.platform.contracts.external_agents import (
    AgentEventType,
    AgentInput,
    AgentSessionRef,
    SessionStart,
)

#: Advisor 建议落库事件（append-only，供 Operator / 前端审阅）。
ADVISOR_ADVICE = "competition.advisor.advice"

#: 建议种类：选题 / 预算 / 停止 / 挂起。
RECOMMEND_KINDS: frozenset[str] = frozenset({
    "select", "budget", "stop", "hold",
})


class AdvisorRecommendation(ContractModel):
    """一条结构化建议（Advisor 输出契约，任务书 10.5「选题/预算/停止」）。"""

    kind: str = "select"                    # select / budget / stop / hold
    challenge_id: str = ""                  # 本地 challenge_id（可空 = 全局建议）
    external_challenge_id: str = ""         # 平台侧题目 id（可空）
    priority: float = 0.0                   # 建议优先级（仅记录，不改确定排序）
    rationale: str = ""
    confidence: float = 0.0


class AdvisorAdvice(ContractModel):
    """一轮顾问结果。``status``：ok / disabled / error / unparseable。"""

    advice_id: str = Field(default_factory=lambda: new_id("adv"))
    competition_id: str = ""
    status: str = "ok"
    summary: str = ""
    recommendations: list[AdvisorRecommendation] = Field(default_factory=list)
    error: str = ""
    session_id: str = ""
    created_at: datetime = Field(default_factory=utcnow)

    def to_dict(self) -> dict[str, Any]:
        return self.model_dump(mode="json")


class ExecutorResult(ContractModel):
    """一次 Advisor 会话的执行结果（最终文本 + 事件计数 + 错误）。"""

    text: str = ""
    events: int = 0
    session_id: str = ""
    error: str = ""


class ExternalAgentSessionExecutor:
    """一次性外部 Agent 会话执行器（ExternalAgentAdapter 形态）。

    流程固定为 ``adapter.start`` → ``adapter.send`` 收集统一 AgentEvent →
    ``adapter.close``（finally 保证关闭并撤销任何 grant）。不 probe 平台、
    不调用任何比赛写路径；adapter 必须是无 ``binding_service`` 的
    ``BaseExternalAgentAdapter``（Advisor 无平台写权限的结构性保证）。
    """

    def __init__(
        self,
        adapter: BaseExternalAgentAdapter,
        *,
        timeout_seconds: float = 120.0,
        session_options: Optional[dict[str, Any]] = None,
    ) -> None:
        if not isinstance(adapter, BaseExternalAgentAdapter):
            raise TypeError(
                "ExternalAgentSessionExecutor requires a "
                "BaseExternalAgentAdapter (RUNTIME-01)"
            )
        if getattr(adapter, "_binding_service", None) is not None:
            raise ValueError(
                "advisor adapter must not carry a binding_service: "
                "the advisor has no platform write capability"
            )
        self._adapter = adapter
        self._timeout = float(timeout_seconds)
        self._session_options = dict(session_options or {})

    async def run_prompt(
        self, text: str, *, payload: Optional[dict[str, Any]] = None
    ) -> ExecutorResult:
        """跑一轮提示并收集最终文本；失败不抛出，错误落在结果里。"""
        session: Optional[AgentSessionRef] = None
        try:
            session = await self._adapter.start(SessionStart(
                options={
                    "principal_id": "competition.advisor",
                    **self._session_options,
                },
            ))
            return await asyncio.wait_for(
                self._collect(session, text, payload or {}),
                timeout=self._timeout,
            )
        except Exception as exc:
            return ExecutorResult(
                session_id=session.agent_session_id if session else "",
                error=f"{type(exc).__name__}: {exc}",
            )
        finally:
            if session is not None:
                try:
                    await self._adapter.close(session)
                except Exception:
                    pass  # 关闭失败不掩盖会话结果

    async def _collect(
        self,
        session: AgentSessionRef,
        text: str,
        payload: dict[str, Any],
    ) -> ExecutorResult:
        chunks: list[str] = []
        events = 0
        error = ""
        stream = self._adapter.send(session, AgentInput(
            kind="message", text=text, payload=payload))
        async for event in stream:
            events += 1
            if event.event_type is AgentEventType.MESSAGE_COMPLETED:
                chunk = str(
                    event.payload.get("text")
                    or event.payload.get("content") or "")
                if chunk:
                    chunks.append(chunk)
            elif event.event_type is AgentEventType.MESSAGE_DELTA:
                # 只有 delta 的 Runtime：拼接增量作为最终文本。
                chunks.append(str(
                    event.payload.get("text")
                    or event.payload.get("delta") or ""))
            elif event.event_type in (
                AgentEventType.RUNTIME_ERROR, AgentEventType.TURN_FAILED,
            ):
                error = str(event.payload.get("code") or "") or "turn_failed"
        return ExecutorResult(
            text="".join(chunks) if chunks and len(chunks) > 1 else
                 (chunks[-1] if chunks else ""),
            events=events,
            session_id=session.agent_session_id,
            error=error,
        )


class CompetitionAdvisor:
    """比赛顾问：读比赛 projection 与 Run 摘要，输出结构化建议。

    - ``store``：只读使用（snapshot / list），从不经 Advisor 写比赛状态；
    - ``executor``：ExternalAgentSessionExecutor；为 None 即停用；
    - ``run_summary_provider``：``(run_id) -> dict``，提供 Run 内摘要
      （Worker 状态、近期路线、Review 结论等）；缺省时用 competition.db
      内可得信号（binding 状态、候选数、判错数）。
    """

    def __init__(
        self,
        store: CompetitionStore,
        executor: Optional[ExternalAgentSessionExecutor] = None,
        *,
        enabled: bool = True,
        run_summary_provider: Optional[Callable[[str], dict[str, Any]]] = None,
        persist_events: bool = True,
    ) -> None:
        self._store = store
        self._executor = executor
        self.enabled = bool(enabled) and executor is not None
        self._run_summary_provider = run_summary_provider
        self._persist_events = persist_events

    # -- 主入口 ----------------------------------------------------------------

    async def advise(self, competition_id: str) -> AdvisorAdvice:
        """产出一轮结构化建议；任何失败都落状态，不向上抛出。"""
        if self._store.get(Competition, competition_id) is None:
            raise NotFoundError(f"competition not found: {competition_id}")
        if not self.enabled or self._executor is None:
            return AdvisorAdvice(
                competition_id=competition_id, status="disabled")
        try:
            projection = self.build_projection(competition_id)
        except Exception as exc:
            return AdvisorAdvice(
                competition_id=competition_id, status="error",
                error=f"projection: {exc}")
        result = await self._executor.run_prompt(
            self._prompt(projection),
            payload={"projection": projection},
        )
        if result.error and not result.text:
            advice = AdvisorAdvice(
                competition_id=competition_id, status="error",
                error=result.error, session_id=result.session_id)
            self._persist(advice)
            return advice
        advice = self._parse(competition_id, result)
        self._persist(advice)
        return advice

    # -- 输入投影（只读） -----------------------------------------------------------

    def build_projection(self, competition_id: str) -> dict[str, Any]:
        """Advisor 的只读输入：比赛 projection + 活动 Run 摘要。"""
        snapshot = self._store.snapshot(competition_id)
        challenges: list[dict[str, Any]] = []
        for item in snapshot.challenges:
            challenge = item.challenge
            revision = item.current_revision
            entry: dict[str, Any] = {
                "challenge_id": challenge.challenge_id,
                "external_challenge_id": challenge.external_challenge_id,
                "name": challenge.name,
                "category": challenge.category,
                "state": challenge.state,
                "remote_state": challenge.remote_state,
                "points": revision.points if revision else 0.0,
                "hints": len(revision.hints) if revision else 0,
                "prerequisites": (
                    list(revision.prerequisites) if revision else []),
                "candidates": len(item.candidates),
                "submissions": len(item.submissions),
            }
            if item.queue_entry is not None:
                entry["queue"] = {
                    "state": item.queue_entry.state,
                    "priority": item.queue_entry.priority,
                    "score": item.queue_entry.score,
                }
            if item.active_binding is not None:
                binding = item.active_binding
                entry["run"] = {
                    "run_id": binding.run_id,
                    "binding_state": binding.state,
                    "execution_generation": binding.execution_generation,
                    "summary": self._run_summary(binding.run_id),
                }
            challenges.append(entry)
        competition = snapshot.competition
        return {
            "competition_id": competition_id,
            "title": competition.title if competition else "",
            "scheduler_state": (
                competition.scheduler_state if competition else ""),
            "automation_mode": (
                snapshot.policy.automation_mode if snapshot.policy else ""),
            "ends_at": (
                competition.ends_at.isoformat()
                if competition and competition.ends_at else None),
            "budgets": {
                b.kind: {"limit": b.limit, "used": b.used}
                for b in snapshot.budgets
            },
            "challenges": challenges,
        }

    def _run_summary(self, run_id: str) -> dict[str, Any]:
        if self._run_summary_provider is None or not run_id:
            return {}
        try:
            return dict(self._run_summary_provider(run_id) or {})
        except Exception as exc:
            return {"error": str(exc)}

    # -- 提示与解析 ---------------------------------------------------------------

    @staticmethod
    def _prompt(projection: dict[str, Any]) -> str:
        return (
            "你是 CTF 比赛调度顾问。根据下面的比赛投影 JSON，输出一个 JSON "
            "对象（不要输出其他内容）：\n"
            '{"summary": "...", "recommendations": ['
            '{"kind": "select|budget|stop|hold", '
            '"challenge_id": "<本地 challenge_id 或空>", '
            '"external_challenge_id": "<平台题目 id 或空>", '
            '"priority": <number>, "rationale": "...", '
            '"confidence": <0..1>}]}\n'
            "规则：select=建议选题，budget=建议调整预算，stop=建议停止某题或"
            "全场，hold=建议挂起。不要建议任何平台写操作；你没有平台写权限。\n"
            f"比赛投影：\n{json.dumps(projection, ensure_ascii=False)}"
        )

    def _parse(self, competition_id: str, result: ExecutorResult) -> AdvisorAdvice:
        raw = self._extract_json(result.text)
        if raw is None:
            return AdvisorAdvice(
                competition_id=competition_id,
                status="unparseable",
                error="advisor reply is not a JSON object",
                session_id=result.session_id,
            )
        recommendations: list[AdvisorRecommendation] = []
        dropped = 0
        for item in raw.get("recommendations") or []:
            if not isinstance(item, dict):
                dropped += 1
                continue
            kind = str(item.get("kind") or "select")
            if kind not in RECOMMEND_KINDS:
                dropped += 1
                continue
            try:
                recommendations.append(AdvisorRecommendation(
                    kind=kind,
                    challenge_id=str(item.get("challenge_id") or ""),
                    external_challenge_id=str(
                        item.get("external_challenge_id") or ""),
                    priority=float(item.get("priority") or 0.0),
                    rationale=str(item.get("rationale") or ""),
                    confidence=max(0.0, min(1.0, float(
                        item.get("confidence") or 0.0))),
                ))
            except (TypeError, ValueError):
                dropped += 1
        status = "ok" if not result.error else "error"
        error = result.error
        if dropped:
            error = (error + "; " if error else "") + (
                f"dropped {dropped} malformed recommendation(s)")
        return AdvisorAdvice(
            competition_id=competition_id,
            status=status,
            summary=str(raw.get("summary") or ""),
            recommendations=recommendations,
            error=error,
            session_id=result.session_id,
        )

    @staticmethod
    def _extract_json(text: str) -> Optional[dict[str, Any]]:
        """从回复中提取首个完整 JSON 对象（容忍 ```json 围栏与前后缀）。"""
        stripped = text.strip()
        if not stripped:
            return None
        candidates = [stripped]
        if "```" in stripped:
            for block in stripped.split("```"):
                block = block.strip()
                if block.startswith("json"):
                    block = block[4:].strip()
                if block.startswith("{"):
                    candidates.append(block)
        start = stripped.find("{")
        if start >= 0:
            candidates.append(stripped[start:])
        for candidate in candidates:
            depth = 0
            for end in range(len(candidate)):
                ch = candidate[end]
                if ch == "{":
                    depth += 1
                elif ch == "}":
                    depth -= 1
                    if depth == 0:
                        try:
                            parsed = json.loads(candidate[: end + 1])
                        except ValueError:
                            break
                        return parsed if isinstance(parsed, dict) else None
        return None

    # -- 持久化（append-only 事件） -------------------------------------------------

    def _persist(self, advice: AdvisorAdvice) -> None:
        if not self._persist_events:
            return
        self._store.append_events([ev.make_event(
            competition_id=advice.competition_id,
            aggregate_type=ev.AGG_COMPETITION,
            aggregate_id=advice.competition_id,
            event_type=ADVISOR_ADVICE,
            payload=advice.model_dump(mode="json"),
        )])


__all__ = [
    "ADVISOR_ADVICE",
    "AdvisorAdvice",
    "AdvisorRecommendation",
    "CompetitionAdvisor",
    "ExecutorResult",
    "ExternalAgentSessionExecutor",
    "RECOMMEND_KINDS",
]
