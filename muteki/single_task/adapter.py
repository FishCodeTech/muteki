"""单题模块的运行时接线（任务书 3.3、9.4，SINGLE-01）。

本模块只做适配与校验，不复制任何业务逻辑：

- 能力工具 → Handler 的映射从 ``capability_catalog.DEFAULT_CATALOG`` 的
  single_task 工具集派生，工具目录变更时这里随之漂移，不维护第二份工具清单；
- run.*/task.* 命令与 run.snapshot/task.get 查询由 COMMAND-01 的
  ``register_builtin_handlers`` 注册；graph.shared.read 等能力查询由 CAP-01 的
  ``register_capability_query_handlers`` 注册，本模块只提供真实
  SharedGraph 的 ``graph_resolver``；
- 创建合同：模块创建入口产出的旧版 start 请求体绝不携带 ``swarm_class``
  （空 spec 由 ``apps.web.drivers._resolve_swarm_class`` 解析为标准
  ``muteki.swarm.swarm.Swarm``；实验臂只能走显式评测入口）。
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Optional

from muteki.platform.capability_catalog import (
    DEFAULT_CATALOG,
    ToolTargetKind,
    default_tool_set,
)
from muteki.platform.capability_gateway import (
    GraphResolver,
    register_capability_query_handlers,
)
from muteki.platform.contracts.capabilities import ThreadMode
from muteki.platform.contracts.commands import ActorRef, CommandEnvelope
from muteki.platform.contracts.receipts import CommandReceipt

# ---------------------------------------------------------------------------
# 能力工具 → Command/Query Handler 一一映射
# ---------------------------------------------------------------------------

#: command_type / query_type → 属主 Handler 标签（与 builtin_modules.py 的
#: ModuleSpec command_handlers 取值同口径）。
_HANDLER_OWNERS: dict[str, str] = {
    "project.list": "platform.capability_gateway",
    "thread.list": "platform.capability_gateway",
    "task.create": "platform.command_handlers.task",
    "task.get": "platform.command_handlers.task",
    "task.list": "platform.command_handlers.task",
    "run.create": "platform.command_handlers.run",
    "run.start": "platform.command_handlers.run",
    "run.resolve": "platform.command_handlers.run",
    "run.pause": "platform.command_handlers.run",
    "run.resume": "platform.command_handlers.run",
    "run.stop": "platform.command_handlers.run",
    "run.add_context": "platform.command_handlers.run",
    "run.spawn_worker": "platform.command_handlers.run",
    "run.cancel_worker": "platform.command_handlers.run",
    "run.list": "platform.command_handlers.run",
    "run.operator_directive": "platform.command_handlers.run",
    "run.snapshot": "platform.command_handlers.run",
    "graph.shared.read": "platform.capability_gateway",
}

#: read_events / wait / receipt 不经 HandlerRegistry，是 Command API 的内建流式
#: 能力（数据源为 RunGateway.events 的 SessionStore JSONL 回放，与 SSE 同源）。
_STREAM_TARGETS = {
    ToolTargetKind.READ_EVENTS: "command_api.read_events",
    ToolTargetKind.WAIT: "command_api.wait",
    ToolTargetKind.RECEIPT: "command_api.get_receipt",
}


@dataclass(frozen=True)
class ToolHandlerBinding:
    """一个 single_task 能力工具到 Handler 的绑定记录。"""

    tool: str
    target_kind: ToolTargetKind
    # COMMAND → command_type；QUERY → query_type；流式目标为空串。
    target_type: str = ""
    handler: str = ""


class SingleTaskWiringError(RuntimeError):
    """单题模块 Handler 接线不完整（缺注册或缺属主声明）。"""


def _build_tool_handlers() -> tuple[ToolHandlerBinding, ...]:
    """从能力目录的 single_task 工具集派生映射（目录变更即漂移）。"""
    bindings: list[ToolHandlerBinding] = []
    for name in default_tool_set(ThreadMode.SINGLE_TASK):
        spec = DEFAULT_CATALOG.require(name)
        if spec.target_kind in _STREAM_TARGETS:
            bindings.append(ToolHandlerBinding(
                tool=name,
                target_kind=spec.target_kind,
                handler=_STREAM_TARGETS[spec.target_kind],
            ))
            continue
        target_type = str(spec.command_type or spec.query_type or "")
        owner = _HANDLER_OWNERS.get(target_type)
        if owner is None:
            raise SingleTaskWiringError(
                f"single_task tool {name!r} targets {target_type!r} "
                "without a declared handler owner")
        bindings.append(ToolHandlerBinding(
            tool=name,
            target_kind=spec.target_kind,
            target_type=target_type,
            handler=owner,
        ))
    return tuple(bindings)


#: single_task 工具 → Handler 的一一映射（模块加载时从目录派生；缺属主声明
#: 会直接抛 SingleTaskWiringError，fail-fast 而不是运行时才发现）。
SINGLE_TASK_TOOL_HANDLERS: tuple[ToolHandlerBinding, ...] = _build_tool_handlers()


def verify_single_task_handlers(api: Any) -> dict[str, str]:
    """校验 single_task 全部工具在 Command API 上有真实 Handler。

    返回 ``tool -> handler`` 报告；任一命令 / 查询类型未注册时抛
    ``SingleTaskWiringError`` 并列出缺失项。流式目标（read_events / wait /
    receipt）是 Command API 内建能力，恒为已接线。
    """
    known_commands = api.handlers.known_command_types()
    known_queries = api.handlers.known_query_types()
    report: dict[str, str] = {}
    missing: list[str] = []
    for binding in SINGLE_TASK_TOOL_HANDLERS:
        if binding.target_kind is ToolTargetKind.COMMAND:
            ok = binding.target_type in known_commands
        elif binding.target_kind is ToolTargetKind.QUERY:
            ok = binding.target_type in known_queries
        else:
            ok = True
        if ok:
            report[binding.tool] = binding.handler
        else:
            missing.append(
                f"{binding.tool} -> {binding.target_type} ({binding.handler})")
    if missing:
        raise SingleTaskWiringError(
            "single_task handlers not registered: " + ", ".join(missing))
    return report


# ---------------------------------------------------------------------------
# 兼容 Adapter：模块创建入口 → 旧 /api/runs/{run_id}/start 请求体
# ---------------------------------------------------------------------------

def build_start_body(
    *,
    task_kind: str,
    kind: str = "swarm",
    challenge: Optional[dict[str, Any]] = None,
    prompt: str = "",
    extra: Optional[dict[str, Any]] = None,
) -> dict[str, Any]:
    """把模块级创建请求归一化为旧 ``/api/runs/{run_id}/start`` 请求体。

    ``task_kind`` 为 ``ctf*`` / ``pentest*``（模块 task_kinds）；pentest 归一化
    为 ``challenge.mode = "pentest"``，其余字段原样透传给
    ``apps.web.drivers.build_driver``。产物绝不携带 ``swarm_class``——即使
    调用方在 ``extra`` 里塞入也会被剥掉（创建合同，drivers 空 spec → Swarm）。
    """
    mode = "pentest" if str(task_kind).strip().startswith("pentest") else "ctf"
    body = dict(extra or {})
    body["kind"] = kind
    ch = dict(challenge or {})
    ch.setdefault("mode", mode)
    text = str(prompt or "").strip()
    if text and not ch.get("description"):
        ch["description"] = text
    body["challenge"] = ch
    body.pop("swarm_class", None)
    return body


# ---------------------------------------------------------------------------
# SharedGraph 只读视图的真实数据源
# ---------------------------------------------------------------------------

def make_graph_resolver(run_manager: Any) -> GraphResolver:
    """构造 run_id → GraphService 的解析器（ctf.shared_graph.v1）。

    图数据库位于 RunManager 的 coordinator-owned graph root。标准 Coordinator 的 Driver 在
    ``run.start`` 回执之后异步执行预检，随后才构造 Swarm；因此聊天在刚启动
    Run 后立即读取 SharedGraph 时，DB 可能尚未由 Driver 创建。

    对已经启动、尚未结束的 Run，解析器会在该空窗期惰性打开同一
    canonical DB。随后 Coordinator 复用该文件，不会产生第二份图；草稿和已
    结束但没有历史图的 Run 仍返回 None，由
    SharedGraphReadQueryHandler 归一化为 ``graph.unavailable``。每次调用都
    新开 GraphService 句柄，查询 Handler 不缓存写入能力。
    """
    def resolve(run_id: str) -> Any:
        run_id = str(run_id or "").strip()
        if not run_id:
            return None
        run = run_manager.get(run_id)
        if run is None:
            return None
        db_path = run_manager.storage.run_graph(run_id) / "shared_graph.db"
        if (not db_path.exists()
                and (not bool(getattr(run, "started", False))
                     or bool(getattr(run, "finished", False)))):
            return None
        from muteki.models.solve_graph import Challenge
        from muteki.swarm.shared_graph import open_graph_service

        return open_graph_service(
            db_path=db_path,
            challenge=Challenge(
                id=run_id,
                name=run.name or run_id,
                category=run.category or "web",
            ),
        )

    return resolve


def register_single_task_handlers(
    api: Any,
    *,
    run_manager: Any = None,
    graph_resolver: Optional[GraphResolver] = None,
) -> dict[str, str]:
    """注册单题模块的只读查询 Handler 并校验整链接线。

    run.*/task.* 由 COMMAND-01 的 ``register_builtin_handlers`` 提供
    （``MutekiCommandApiImpl.with_builtin_handlers``）；这里补上 CAP-01 的
    能力查询（project.list / thread.list / graph.shared.read），并返回
    ``verify_single_task_handlers`` 的接线报告。``graph_resolver`` 缺省时
    由 ``run_manager`` 派生真实 SharedGraph 解析器。
    """
    resolver = graph_resolver
    if resolver is None and run_manager is not None:
        resolver = make_graph_resolver(run_manager)
    register_capability_query_handlers(api, graph_resolver=resolver)
    return verify_single_task_handlers(api)


# ---------------------------------------------------------------------------
# 模块创建入口：task.create + run.create + run.start（经 Command API）
# ---------------------------------------------------------------------------

@dataclass
class SingleTaskRunHandle:
    """模块创建入口的返回：异步回执与聚合 id（与 Web 同一 receipt）。"""

    task_id: str
    run_id: str
    receipts: list[CommandReceipt] = field(default_factory=list)


async def create_task_and_run(
    api: Any,
    *,
    task_kind: str,
    title: str = "",
    binding_key: str,
    start_body: Optional[dict[str, Any]] = None,
    actor: Optional[ActorRef] = None,
    task_id: str = "",
) -> SingleTaskRunHandle:
    """从模块入口创建并启动一个单题 Run。

    三步全部经 Command API（与 Web / Gateway 同一 receipt、同一幂等与
    RunManager 路径）：``task.create``（任务聚合）→ ``run.create``（幂等
    binding key 绑定 Run）→ ``run.start``（启动执行代，请求体经
    ``build_start_body`` 归一化，不发送 ``swarm_class``）。任一步骤的回执
    带 error 即停止并返回已收集的回执。
    """
    actor = actor or ActorRef(kind="operator", id="local-user")
    receipts: list[CommandReceipt] = []

    task_payload: dict[str, Any] = {"kind": task_kind, "title": title}
    if task_id:
        task_payload["task_id"] = task_id
    task_receipt = await api.dispatch(CommandEnvelope(
        command_type="task.create",
        aggregate_type="task",
        aggregate_id=task_id,
        actor=actor,
        payload=task_payload,
    ))
    receipts.append(task_receipt)
    if task_receipt.error is not None:
        return SingleTaskRunHandle(
            task_id=task_id, run_id="", receipts=receipts)
    resolved_task_id = (
        task_receipt.aggregate.id if task_receipt.aggregate else ""
    ) or task_id

    run_receipt = await api.dispatch(CommandEnvelope(
        command_type="run.create",
        aggregate_type="run",
        actor=actor,
        payload={
            "binding_key": binding_key,
            "task_id": resolved_task_id or None,
            "task_kind": task_kind,
        },
    ))
    receipts.append(run_receipt)
    if run_receipt.error is not None or not run_receipt.run_id:
        return SingleTaskRunHandle(
            task_id=resolved_task_id, run_id="", receipts=receipts)
    run_id = run_receipt.run_id

    body = build_start_body(
        task_kind=task_kind, **(dict(start_body or {})))
    start_receipt = await api.dispatch(CommandEnvelope(
        command_type="run.start",
        aggregate_type="run",
        aggregate_id=run_id,
        actor=actor,
        payload=body,
    ))
    receipts.append(start_receipt)
    return SingleTaskRunHandle(
        task_id=resolved_task_id, run_id=run_id, receipts=receipts)


__all__ = [
    "SINGLE_TASK_TOOL_HANDLERS",
    "SingleTaskRunHandle",
    "SingleTaskWiringError",
    "ToolHandlerBinding",
    "build_start_body",
    "create_task_and_run",
    "make_graph_resolver",
    "register_single_task_handlers",
    "verify_single_task_handlers",
]
