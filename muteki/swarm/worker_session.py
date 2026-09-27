"""Worker Session Supervisor（EXEC-01，任务书 7.4 / 8.2，设计 11.1）。

CliSolver 收敛后从 Worker 执行循环析出的会话监督层。职责边界：

- Worker Profile → 对应八类 ``cli.<engine>`` Adapter；做题模式不解析
  Conversation 的 ACP、App Server 或 SDK Runtime；
- 每个 Worker 的内部 AgentSession identity：``(run_id, execution_generation,
  solver_id)`` 三元组锚定，external session id 出现后回填；
- 统一 AgentEvent：每条事件带 ``execution_generation`` 与 ``solver_id``，
  经 ``EventProjector`` 投影（generation fencing——旧 generation 事件拒绝
  写入新 generation 投影）；resolve 继续 Run 时新 Swarm 携带新
  ``execution_generation`` 构造新 supervisor；
- Runtime 退出分类：``interrupted / failed / resumable / closed``，只做
  Session 收尾，**不产生** turn 完成事件（不伪造完成事件）。

不负责：进程 spawn、流式 stdout 解析、Prompt 构建、Flag/Finding 提取与
gate——这些继续由 ``CliSolver`` / ``CliDriver`` / ``gate.py`` 承担。
Operator steer/interrupt、Review、Flag 结束条件保持现状。
"""

from __future__ import annotations

import threading
from dataclasses import dataclass, field
from typing import Any, Optional

from muteki.external_agents.events import EventSequencer, build_event
from muteki.external_agents.factory import (
    RuntimeAdapterConfig,
    RuntimeAdapterFactory,
)
from muteki.external_agents.registry import AdapterRegistry
from muteki.external_agents.sessions import (
    EventProjector,
    SessionTracker,
    classify_exit,
)
from muteki.platform.contracts.external_agents import (
    AgentEvent,
    AgentEventType,
    AgentSessionRef,
    SessionStart,
)
#: SharedGraph 审计事件 kind（graph_views 只投影已知领域 kind，新 kind 为纯
#: 追加，不影响现有投影与黑板协议）。
EV_WORKER_AGENT_SESSION = "worker_agent_session"

KIND_CLI_COMPAT = "cli_compat"

def _build_cli_compat_adapter(
    config: RuntimeAdapterConfig, common: dict[str, Any]
) -> Any:
    """solver 层提供给基础 Runtime Factory 的 CLI 产品构造器。"""
    from muteki.solver.cli_driver import cli_adapter_for

    profile: dict[str, Any] = {
        "engine": str(config.adapter_id).removeprefix("cli."),
    }
    return cli_adapter_for(profile, **common)


@dataclass
class AdapterResolution:
    """一次 Worker Profile → Adapter instance 的解析结果。"""

    adapter: Any
    adapter_key: str  # "adapter_id:instance_id"
    kind: str  # Worker 固定为 KIND_CLI_COMPAT
    engine: str


@dataclass
class _WorkerSessionState:
    """一个 Worker 的会话监督状态（进程内，配合 SessionTracker 记录）。"""

    solver_id: str
    agent_session_id: str
    resolution: AdapterResolution
    sequencer: EventSequencer = field(default_factory=EventSequencer)
    external_session_id: Optional[str] = None
    exit_classification: str = ""


class WorkerSessionSupervisor:
    """标准 Swarm 的 Worker 会话监督器（每 Run 一个）。

    构造参数：

    - ``run_id`` / ``execution_generation``：Run 身份与执行代次；
      generation 作为 fencing 水位写入每条事件与该 Run 全部 Worker 的
      AgentSession 记录；
    - ``registry``：历史兼容参数，做题模式不会接收 Conversation 的注册表；
      supervisor 始终持有私有 CLI 注册表；
    - ``shared_graph``：可选 SharedGraph，用于 Session 生命周期审计追加；
    - ``store``：可选 PlatformStore，持久化内部 AgentSession 记录。
    """

    def __init__(
        self,
        *,
        run_id: str,
        execution_generation: int = 1,
        registry: Optional[AdapterRegistry] = None,
        shared_graph: Any = None,
        store: Any = None,
        factory: Optional[RuntimeAdapterFactory] = None,
    ) -> None:
        self.run_id = str(run_id or "")
        self.execution_generation = max(1, int(execution_generation or 1))
        # 做题模式与 Conversation Runtime 完全隔离。即使旧调用方仍传入共享
        # 注册表，也不能让其中的 ACP/App Server/SDK 实例进入 Worker 调度。
        del registry
        self.registry = AdapterRegistry()
        self.factory = factory or RuntimeAdapterFactory(
            store=store, cli_builder=_build_cli_compat_adapter)
        if self.factory.cli_builder is None:
            self.factory.cli_builder = _build_cli_compat_adapter
        # 只注册八类 CLI compatibility adapter。
        self.factory.populate(
            self.registry,
            include_structured_defaults=False,
            include_cli_compatibility=True,
        )
        self._shared_graph = shared_graph
        self._tracker = SessionTracker(store)
        self._projector = EventProjector(self._tracker)
        self._lock = threading.RLock()
        self._workers: dict[str, _WorkerSessionState] = {}
        self._events: list[AgentEvent] = []

    @property
    def shared_graph(self) -> Any:
        """本 supervisor 审计追加写入的 SharedGraph（可为 None）。"""
        return self._shared_graph

    # ------------------------------------------------------------------
    # Profile → Adapter instance 解析
    # ------------------------------------------------------------------

    def _cli_compat_adapter(
        self,
        engine: str,
        profile: Optional[dict[str, Any]],
        *,
        driver: Any = None,
    ) -> Any:
        """CLI 兼容路径实例：按 ``cli.<engine>:default`` 注册一次后复用。"""
        adapter_id = f"cli.{engine}"
        existing = self.registry.get(adapter_id, "default")
        if existing is not None:
            return existing
        from muteki.solver.cli_driver import CliDriverAdapter, driver_for

        if driver is None:
            driver = driver_for(profile or engine)
        adapter = CliDriverAdapter(driver)
        try:
            self.registry.register(
                adapter, instance_id="default",
                metadata={"engine": engine, "enabled": True})
        except ValueError:
            adapter = self.registry.get(adapter_id, "default")
        return adapter

    def resolve(
        self,
        profile: Optional[dict[str, Any]],
        engine: str,
        *,
        driver: Any = None,
        solver_id: str = "",
    ) -> AdapterResolution:
        """把 Worker Profile 固定解析到对应的无交互 CLI Adapter。

        ``runtime_instance_ref`` 属于 Conversation Runtime 选择，做题模式不再
        消费它。Worker 的引擎、模型、凭据账户和运行环境仍由 Profile 传给
        ``CliDriver``，执行失败由真实 CLI 预检和子进程结果报告。
        """
        profile = profile if isinstance(profile, dict) else None
        engine = str(engine or "")
        if not engine:
            raise RuntimeError("Worker Profile 缺少 CLI engine")
        adapter_id = f"cli.{engine}"
        adapter = self._cli_compat_adapter(engine, profile, driver=driver)
        if adapter is None:
            raise RuntimeError(f"engine={engine!r} 没有可用的 CLI Worker")
        return AdapterResolution(
            adapter=adapter,
            adapter_key=f"{adapter_id}:default",
            kind=KIND_CLI_COMPAT,
            engine=engine,
        )

    # ------------------------------------------------------------------
    # AgentSession identity 与统一事件
    # ------------------------------------------------------------------

    def open_session(
        self, worker: Any, resolution: AdapterResolution
    ) -> AgentSessionRef:
        """为一个 Worker 建内部 AgentSession（preparing）并签发 SESSION_STARTED。

        identity 三元组：``(run_id, execution_generation, solver_id)``；
        external session id 由 ``note_external_session`` 在 Runtime 真实分配
        后回填（状态转 active）。
        """
        solver_id = str(getattr(worker, "solver_id", "") or "")
        adapter_id = str(getattr(resolution.adapter, "id", "") or "")
        instance_id = str(
            getattr(getattr(resolution.adapter, "identity", None),
                    "instance_id", "") or "default")
        request = SessionStart(
            run_id=self.run_id,
            execution_generation=self.execution_generation,
            options={
                "solver_id": solver_id,
                "worker_mode": str(getattr(worker, "mode", "") or ""),
                "engine": resolution.engine,
            },
        )
        record = self._tracker.create(
            request, adapter_id=adapter_id, runtime_instance_id=instance_id)
        self._tracker.set_generation(
            record.agent_session_id, self.execution_generation)
        state = _WorkerSessionState(
            solver_id=solver_id,
            agent_session_id=record.agent_session_id,
            resolution=resolution,
        )
        with self._lock:
            self._workers[solver_id] = state
        self._graph_append(EV_WORKER_AGENT_SESSION, solver_id, {
            "phase": "preparing",
            "agent_session_id": record.agent_session_id,
            "adapter": resolution.adapter_key,
            "transport": resolution.kind,
            "engine": resolution.engine,
            "execution_generation": self.execution_generation,
        })
        ref = AgentSessionRef(
            agent_session_id=record.agent_session_id,
            adapter_id=adapter_id,
            runtime_instance_id=instance_id,
        )
        return ref

    def note_external_session(self, solver_id: str, external_session_id: str) -> None:
        """Runtime 真实分配的 external session id 回填（状态转 active）。

        与 ``(run_id, execution_generation, solver_id)`` 一起保存在内部
        AgentSession 记录上（任务书 7.4）。幂等：同值重复上报不重复回填。
        """
        solver_id = str(solver_id or "")
        external_session_id = str(external_session_id or "")
        if not solver_id or not external_session_id:
            return
        with self._lock:
            state = self._workers.get(solver_id)
        if state is None or state.external_session_id == external_session_id:
            return
        state.external_session_id = external_session_id
        try:
            self._tracker.activate(
                state.agent_session_id,
                external_session_id=external_session_id,
                resume_handle=external_session_id)
        except KeyError:
            return
        self._graph_append(EV_WORKER_AGENT_SESSION, solver_id, {
            "phase": "external_session_bound",
            "agent_session_id": state.agent_session_id,
            "external_session_id": external_session_id,
            "execution_generation": self.execution_generation,
        })

    def apply_event(self, event: AgentEvent) -> "tuple[bool, str]":
        """注入一条外部 AgentEvent（经 generation fencing 投影）。

        旧 generation / 乱序事件被拒绝（``(False, reason)``），不写入投影；
        这是 RUNTIME-01 EventProjector fencing 在 Worker 层的复用入口。
        """
        accepted, _snapshot, reason = self._projector.apply(event)
        if accepted:
            with self._lock:
                self._events.append(event)
        return accepted, reason

    def close_session(
        self,
        solver_id: str,
        *,
        stop_reason: str = "",
        cancelled: bool = False,
        steered: bool = False,
    ) -> str:
        """Worker 退出收尾：分类为 interrupted/failed/resumable/closed。

        只做 Session 状态收尾与 RUNTIME_EXITED/SESSION_CLOSED 事件，不产生
        turn 完成事件——完成只能来自 Runtime 的真实输出（任务书 7.4）。
        幂等：重复关闭返回已记录分类。
        """
        solver_id = str(solver_id or "")
        with self._lock:
            state = self._workers.get(solver_id)
        if state is None:
            return ""
        if state.exit_classification:
            return state.exit_classification
        reason = str(stop_reason or "").strip().lower()
        record = self._tracker.get(state.agent_session_id)
        external_id = (state.external_session_id
                       or (record.external_session_id if record else None))
        classification = classify_exit(
            cancelled=cancelled or reason == "cancelled",
            steered=steered or reason == "steered",
            timed_out=reason.startswith("timeout"),
            resume_handle=external_id or None,
            error="worker error" if reason == "error" else "",
        )
        state.exit_classification = classification
        common = {
            "solver_id": solver_id,
            "adapter": state.resolution.adapter_key,
            "classification": classification,
        }
        # Runtime 退出分类事件（不含任何完成语义）。
        self._emit(state, AgentEventType.RUNTIME_EXITED,
                   native_type="swarm.worker.runtime_exited",
                   payload=dict(common, stop_reason=reason))
        self._emit(state, AgentEventType.SESSION_CLOSED,
                   native_type="swarm.worker.session_closed",
                   payload=common)
        self._tracker.close(state.agent_session_id, classification)
        self._graph_append(EV_WORKER_AGENT_SESSION, solver_id, {
            "phase": "closed",
            "agent_session_id": state.agent_session_id,
            "external_session_id": external_id or "",
            "classification": classification,
            "stop_reason": reason,
            "execution_generation": self.execution_generation,
        })
        return classification

    # ------------------------------------------------------------------
    # 查询（测试 / 运行证据 / 前端透传）
    # ------------------------------------------------------------------

    def events(self, solver_id: Optional[str] = None) -> list[AgentEvent]:
        """已接受（通过 fencing）的统一 AgentEvent 列表。"""
        with self._lock:
            events = list(self._events)
        if solver_id is not None:
            events = [e for e in events
                      if e.payload.get("solver_id") == solver_id]
        return events

    def snapshot(self, solver_id: str):
        """该 Worker 的 AgentSessionSnapshot（fencing 后的投影）。"""
        with self._lock:
            state = self._workers.get(str(solver_id or ""))
        if state is None:
            return None
        snapshot = self._projector.snapshot(state.agent_session_id)
        return snapshot.model_copy(update={
            "adapter_id": str(
                getattr(state.resolution.adapter, "id", "") or ""),
            "external_session_id": (
                state.external_session_id or snapshot.external_session_id),
            "state": self._tracker.state(state.agent_session_id) or snapshot.state,
        })

    def session_state(self, solver_id: str) -> str:
        """内部 AgentSession 状态机值（preparing/active/closed/error）。"""
        with self._lock:
            state = self._workers.get(str(solver_id or ""))
        if state is None:
            return ""
        return self._tracker.state(state.agent_session_id)

    def exit_classification(self, solver_id: str) -> str:
        with self._lock:
            state = self._workers.get(str(solver_id or ""))
        return state.exit_classification if state is not None else ""

    # ------------------------------------------------------------------
    # 内部
    # ------------------------------------------------------------------

    def _emit(
        self,
        state: _WorkerSessionState,
        event_type: AgentEventType,
        *,
        native_type: str,
        payload: dict[str, Any],
    ) -> AgentEvent:
        """构造统一事件（带 generation 与 solver_id）并经 fencing 投影。"""
        payload = {"solver_id": state.solver_id, **payload}
        event = build_event(
            event_type, state.sequencer,
            agent_session_id=state.agent_session_id,
            external_session_id=state.external_session_id,
            run_id=self.run_id,
            execution_generation=self.execution_generation,
            native_type=native_type,
            payload=payload,
        )
        self.apply_event(event)
        return event

    def _graph_append(self, kind: str, actor: str, payload: dict) -> None:
        """SharedGraph 审计追加（best-effort：图不可用不影响求解）。"""
        graph = self._shared_graph
        append = getattr(graph, "_append", None)
        if append is None:
            return
        try:
            append(kind, actor, payload)
        except Exception:
            pass


__all__ = [
    "AdapterResolution",
    "EV_WORKER_AGENT_SESSION",
    "KIND_CLI_COMPAT",
    "WorkerSessionSupervisor",
]
