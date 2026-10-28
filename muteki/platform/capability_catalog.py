"""能力工具注册目录（任务书 6.6、10.11，CAP-01）。

数据驱动的工具目录：每个 ``CapabilityToolSpec`` 声明一个 ``muteki_*`` 工具的
名称、说明、入参 JSON Schema，以及它归一化到 MutekiCommandAPI 的映射
（command / query / read_events / wait / receipt）。CAP-02 的各协议入口
（MCP / Native Tool / ACP / HTTP-JSONRPC / Agent Plugin）从这里动态生成工具
schema，不维护第二份命令目录。

比赛类工具声明 ``competition.*`` 的 command_type / query_type；对应
Handler 由 COMP-09 注册在绑定 CompetitionStore 的 Command API 上
（``muteki.competition.commands``）。平台侧 Command API 未注册这些
Handler 时由 HandlerRegistry 返回统一 ``command.unsupported`` /
``query.unsupported`` 错误，本目录不伪造任何比赛业务逻辑。
"""

from __future__ import annotations

from enum import Enum
from typing import Any, Optional

from pydantic import Field
from jsonschema import Draft202012Validator

from muteki.platform.contracts.base import ContractModel
from muteki.platform.contracts.capabilities import ThreadMode, ToolDescription


class ToolTargetKind(str, Enum):
    """工具到 MutekiCommandAPI 的映射种类。"""

    COMMAND = "command"          # dispatch(command_type)
    QUERY = "query"              # query(query_type)
    READ_EVENTS = "read_events"  # read_events(run 流)
    WAIT = "wait"                # 有界 wait(run 流)
    RECEIPT = "receipt"          # get_receipt(command_id)


class CapabilityToolSpec(ContractModel):
    """一个能力工具的静态声明（不含任何运行时状态）。"""

    name: str
    description: str = ""
    target_kind: ToolTargetKind
    # COMMAND 映射的 command_type；QUERY 映射的 query_type。
    command_type: Optional[str] = None
    query_type: Optional[str] = None
    # 命令 / 事件流作用的聚合类型（run / task / competition / connection 等）。
    aggregate_type: str = ""
    # 入参中提供 aggregate_id 的字段名（如 run_id）；为空则聚合 id 由
    # Handler 自行决定（如 task.create）。
    aggregate_arg: str = ""
    # 注入给 Runtime 的入参 JSON Schema。
    input_schema: dict[str, Any] = Field(default_factory=dict)
    # 归一化为命令时注入的默认 payload 键（调用方显式传值优先）。用于声明
    # 语义固定的工具，例如 muteki_dispatch_challenge 固定创建 ctf.challenge。
    payload_defaults: dict[str, Any] = Field(default_factory=dict)
    # The aggregate is the Thread owning the calling Binding, never a model
    # supplied id; such tools only ever act on the caller's own Thread scope.
    aggregate_from_caller_thread: bool = False

    def describe(self) -> ToolDescription:
        """协议入口注入用的工具描述。"""
        return ToolDescription(
            name=self.name,
            description=self.description,
            input_schema=dict(self.input_schema),
        )


def _props(**kwargs: Any) -> dict[str, Any]:
    """构造简单的 object JSON Schema。"""

    def _string(desc: str) -> dict[str, Any]:
        return {"type": "string", "description": desc}

    properties: dict[str, Any] = {}
    required: list[str] = []
    for key, (desc, req) in kwargs.items():
        properties[key] = _string(desc)
        if req:
            required.append(key)
    schema: dict[str, Any] = {"type": "object", "properties": properties}
    if required:
        schema["required"] = required
    return schema


# ---------------------------------------------------------------------------
# 工具目录（10.11 清单 + 管理模式的全局运行管理工具）
# ---------------------------------------------------------------------------

_TOOL_SPECS: tuple[CapabilityToolSpec, ...] = (
    # -- 通用（对话 / 任务追踪） -------------------------------------------
    CapabilityToolSpec(
        name="muteki_list_projects",
        description="列出当前可见的 Project。",
        target_kind=ToolTargetKind.QUERY,
        query_type="project.list",
        input_schema=_props(),
    ),
    CapabilityToolSpec(
        name="muteki_list_threads",
        description=(
            "列出当前 Principal 可见的 Thread。支持按项目、模式和标题/ID "
            "筛选并限制返回条数，避免结果过大后 Runtime 回退读取临时文件。"
        ),
        target_kind=ToolTargetKind.QUERY,
        query_type="thread.list",
        input_schema={
            "type": "object",
            "properties": {
                "project_id": {
                    "type": "string",
                    "description": "可选：限定 Project id",
                },
                "mode": {
                    "type": "string",
                    "enum": ["conversation", "single_task", "competition", "management"],
                    "description": "可选：限定 Thread 模式",
                },
                "query": {
                    "type": "string",
                    "description": "可选：按 Thread id 或标题搜索",
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 200,
                    "default": 50,
                    "description": "可选：最多返回条数，默认 50，最大 200",
                },
                "cursor": {"type": "string", "maxLength": 8192,
                           "description": "续读时传上一页 next_cursor，并沿用原筛选条件。"},
            },
        },
    ),
    CapabilityToolSpec(
        name="muteki_list_tasks",
        description=(
            "列出 Muteki 已登记的任务历史。用户询问历史 CTF 题目、任务清单或"
            "题目来源时优先使用本工具，不需要读取 sessions、目录或数据库。"
        ),
        target_kind=ToolTargetKind.QUERY,
        query_type="task.list",
        aggregate_type="task",
        input_schema={
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "description": "可选：任务种类精确值，例如 ctf.challenge",
                },
                "kind_prefix": {
                    "type": "string",
                    "description": "可选：任务种类前缀，例如 ctf.",
                },
                "project_id": {
                    "type": "string",
                    "description": "可选：限定 Project id",
                },
                "thread_id": {
                    "type": "string",
                    "description": "可选：限定 Thread id",
                },
                "query": {
                    "type": "string",
                    "description": "可选：按任务 id、标题或种类搜索",
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 2000,
                    "default": 50,
                    "description": "每页条数，默认 50，最大 2000；仅在需要批量数据时显式增大。",
                },
                "cursor": {"type": "string", "maxLength": 8192,
                           "description": "续读时传上一页 next_cursor，并沿用原筛选条件。"},
            },
        },
    ),
    CapabilityToolSpec(
        name="muteki_get_task",
        description=(
            "读取一个 Muteki Task 的完整领域输入。先用 muteki_list_tasks 定位"
            "task_id，再用本工具读取题目描述和结构化输入。"
        ),
        target_kind=ToolTargetKind.QUERY,
        query_type="task.get",
        aggregate_type="task",
        aggregate_arg="task_id",
        input_schema=_props(task_id=("任务 id", True)),
    ),
    CapabilityToolSpec(
        name="muteki_list_runs",
        description=(
            "列出 Muteki 已执行的 Run 历史和解题结果。用户询问历史 CTF 题目、"
            "运行状态、是否解出或 Flag 情况时优先使用本工具，不需要读取"
            "sessions、目录或数据库。"
        ),
        target_kind=ToolTargetKind.QUERY,
        query_type="run.list",
        aggregate_type="run",
        input_schema={
            "type": "object",
            "properties": {
                "category": {
                    "type": "string",
                    "description": "可选：题目分类精确值",
                },
                "status": {
                    "type": "string",
                    "description": "可选：running / paused / solved / finished",
                },
                "solved": {
                    "type": "boolean",
                    "description": "可选：只返回已解出或未解出的 Run",
                },
                "include_archived": {
                    "type": "boolean",
                    "description": "可选：是否包含已归档 Run，默认包含",
                },
                "query": {
                    "type": "string",
                    "description": "可选：按 Run id、题目名或分类搜索",
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 2000,
                    "default": 50,
                    "description": "每页条数，默认 50，最大 2000；仅在需要批量数据时显式增大。",
                },
                "cursor": {"type": "string", "maxLength": 8192,
                           "description": "续读时传上一页 next_cursor，并沿用原筛选条件。"},
            },
        },
    ),
    CapabilityToolSpec(
        name="muteki_create_task",
        description=(
            "创建一个普通任务（task.create），可在 input 中提交目标、题目描述、"
            "范围和其他结构化任务输入，返回异步 CommandReceipt。"
        ),
        target_kind=ToolTargetKind.COMMAND,
        command_type="task.create",
        aggregate_type="task",
        aggregate_arg="task_id",
        input_schema={
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "description": "任务种类，例如 ctf.challenge / pentest.target",
                },
                "title": {"type": "string", "description": "任务标题"},
                "task_id": {
                    "type": "string",
                    "description": "可选：预先指定 Task id",
                },
                "input": {
                    "type": "object",
                    "description": "任务的完整结构化输入，例如描述、目标、范围和限制",
                    "additionalProperties": True,
                },
            },
            "required": ["kind"],
        },
    ),
    CapabilityToolSpec(
        name="muteki_get_command_receipt",
        description="按 command_id 查询命令回执（与 Web 同一 receipt）。",
        target_kind=ToolTargetKind.RECEIPT,
        input_schema=_props(command_id=("命令 id", True)),
    ),
    # -- 单题模式 -----------------------------------------------------------
    CapabilityToolSpec(
        name="muteki_dispatch_challenge",
        description=(
            "登记一道 CTF 题目并返回 Task 回执。随后使用回执里的 task_id 调用"
            "muteki_create_run，再调用 muteki_start_swarm。"
        ),
        target_kind=ToolTargetKind.COMMAND,
        command_type="task.create",
        aggregate_type="task",
        aggregate_arg="task_id",
        # task.create 要求 kind；该工具语义固定为 CTF 题目（SINGLE-01 接线后
        # 经 Gateway invoke 必须能直达真实 Handler）。
        payload_defaults={"kind": "ctf.challenge"},
        input_schema={
            "type": "object",
            "properties": {
                "title": {"type": "string", "description": "题目标题"},
                "task_id": {
                    "type": "string",
                    "description": "可选：预先指定 Task id",
                },
                "input": {
                    "type": "object",
                    "description": "CTF 题目的完整结构化输入，例如描述、分类和附件引用",
                    "additionalProperties": True,
                },
            },
            "required": ["title"],
        },
    ),
    CapabilityToolSpec(
        name="muteki_create_run",
        description=(
            "为 Task 创建或幂等取得一个 Run。binding_key 建议使用 task_id；"
            "返回回执中的 run_id，随后可启动、查询、等待或控制该 Run。"
        ),
        target_kind=ToolTargetKind.COMMAND,
        command_type="run.create",
        aggregate_type="run",
        aggregate_arg="run_id",
        payload_defaults={
            "task_kind": "ctf.challenge",
            "executor_id": "swarm.coordinator",
        },
        input_schema={
            "type": "object",
            "properties": {
                "binding_key": {
                    "type": "string",
                    "description": "幂等绑定键；通常直接使用 task_id",
                },
                "task_id": {"type": "string", "description": "关联 Task id"},
                "task_kind": {
                    "type": "string",
                    "description": "任务种类，默认 ctf.challenge",
                },
                "task_revision": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Task revision，默认 1",
                },
                "run_id": {
                    "type": "string",
                    "description": "可选：预先指定 Run id",
                },
                "executor_id": {
                    "type": "string",
                    "description": "执行器 id，默认 swarm.coordinator",
                },
            },
            "required": ["binding_key"],
        },
    ),
    CapabilityToolSpec(
        name="muteki_start_swarm",
        description=(
            "启动 Run 的标准 Coordinator Swarm，立即返回 receipt、run_id 和事件"
            "游标。prompt 或 challenge 至少提供一种；不得传实验 swarm_class。"
            "渗透功能正在全面重写，当前不能启动 pentest 模式。"
        ),
        target_kind=ToolTargetKind.COMMAND,
        command_type="run.start",
        aggregate_type="run",
        aggregate_arg="run_id",
        payload_defaults={"kind": "swarm"},
        input_schema={
            "type": "object",
            "properties": {
                "run_id": {"type": "string", "description": "目标 Run id"},
                "prompt": {
                    "type": "string",
                    "description": "题目描述；未提供 challenge 时由产品解析",
                },
                "challenge": {
                    "type": "object",
                    "description": "结构化 CTF 题目。category 使用小写标准值；pentest 模式目前不可启动。",
                    "properties": {
                        "name": {"type": "string", "description": "题目或目标名称"},
                        "category": {
                            "type": "string",
                            "enum": ["web", "pwn", "reverse", "crypto", "forensics", "misc"],
                            "description": "小写题目分类",
                        },
                        "description": {"type": "string", "description": "题目或测试目标描述"},
                        "target": {"type": "string", "description": "执行目标"},
                        "scope": {"type": "string", "description": "Pentest 授权范围；省略时使用 target"},
                        "goal": {"type": "string", "description": "Pentest 完成目标；省略时使用 description"},
                        "flag_format": {
                            "type": "string",
                            "description": (
                                "Flag 校验合同：可传 token（裸令牌模式）、"
                                "flag{...}/WMCTF{...} 这类格式示例，或可编译的 Python "
                                "正则表达式；格式示例会在启动时转换为有界正则"
                            ),
                            "examples": ["flag{...}", "token", r"WMCTF\{[^}]+\}"],
                        },
                        "flag_format_wrapper": {
                            "type": "string",
                            "description": (
                                "自定义包装示例，如 WMCTF{...}。当 flag_format 为空或"
                                "为 custom 时，它会成为校验合同；若同时提供显式"
                                "flag_format，则显式格式优先（token 保持裸令牌模式）"
                            ),
                        },
                    },
                    "additionalProperties": True,
                },
                "expected_flags": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "预期不同 Flag 数量，默认 1",
                },
                "coordinator": {
                    "type": "boolean",
                    "description": "是否启用产品 Coordinator，默认 true",
                },
                "cli_race": {
                    "type": "boolean",
                    "description": "是否启用直接 CLI race，默认 false",
                },
            },
            "required": ["run_id"],
        },
    ),
    CapabilityToolSpec(
        name="muteki_resolve_run",
        description=(
            "对已结束但尚未完成的 Run 发起下一执行代，复用原工作区继续求解。"
            "可附加 prompt 或 challenge 更新。"
        ),
        target_kind=ToolTargetKind.COMMAND,
        command_type="run.resolve",
        aggregate_type="run",
        aggregate_arg="run_id",
        input_schema={
            "type": "object",
            "properties": {
                "run_id": {"type": "string", "description": "目标 Run id"},
                "prompt": {"type": "string", "description": "可选：追加求解要求"},
                "challenge": {
                    "type": "object",
                    "description": "可选：更新后的题目信息",
                    "additionalProperties": True,
                },
            },
            "required": ["run_id"],
        },
    ),
    CapabilityToolSpec(
        name="muteki_get_run_snapshot",
        description="读取 Run 进度快照（run.snapshot）。",
        target_kind=ToolTargetKind.QUERY,
        query_type="run.snapshot",
        aggregate_type="run",
        aggregate_arg="run_id",
        input_schema=_props(run_id=("目标 Run id", True)),
    ),
    CapabilityToolSpec(
        name="muteki_read_run_events",
        description="按事件游标读取 Run 事件流的一页。",
        target_kind=ToolTargetKind.READ_EVENTS,
        aggregate_type="run",
        aggregate_arg="run_id",
        input_schema={
            "type": "object",
            "properties": {
                "run_id": {"type": "string", "description": "目标 Run id"},
                "after_cursor": {"type": "string", "description": "可选：续读游标"},
                "limit": {
                    "type": "integer", "minimum": 1, "maximum": 500,
                    "description": "单页事件数，默认 100，最大 500",
                },
            },
            "required": ["run_id"],
        },
    ),
    CapabilityToolSpec(
        name="muteki_wait_run",
        description="有界等待 Run 新事件；超时返回最新游标。",
        target_kind=ToolTargetKind.WAIT,
        aggregate_type="run",
        aggregate_arg="run_id",
        input_schema={
            "type": "object",
            "properties": {
                "run_id": {"type": "string", "description": "目标 Run id"},
                "after_cursor": {"type": "string", "description": "可选：续读游标"},
                "timeout_seconds": {
                    "type": "number", "minimum": 0, "maximum": 30,
                    "description": "有界等待秒数，默认 30，最大 30",
                },
                "limit": {
                    "type": "integer", "minimum": 1, "maximum": 500,
                    "description": "最多返回事件数，默认 100，最大 500",
                },
            },
            "required": ["run_id"],
        },
    ),
    CapabilityToolSpec(
        name="muteki_read_shared_graph",
        description="读取 Run 的 SharedGraph 只读状态视图。可用 sections 选择需要的完整区块；返回 available_sections 与 watermark。",
        target_kind=ToolTargetKind.QUERY,
        query_type="graph.shared.read",
        aggregate_type="run",
        aggregate_arg="run_id",
        input_schema={
            "type": "object", "required": ["run_id"],
            "properties": {
                "run_id": {"type": "string", "description": "目标 Run id"},
                "sections": {"type": "array", "minItems": 1, "uniqueItems": True,
                             "items": {"type": "string", "minLength": 1},
                             "description": "可选：按 available_sections 中的名称选择完整区块；省略时读取完整状态。"},
            },
        },
    ),
    CapabilityToolSpec(
        name="muteki_pause_run",
        description="暂停 Run（run.pause）。",
        target_kind=ToolTargetKind.COMMAND,
        command_type="run.pause",
        aggregate_type="run",
        aggregate_arg="run_id",
        input_schema=_props(run_id=("目标 Run id", True)),
    ),
    CapabilityToolSpec(
        name="muteki_resume_run",
        description="恢复 Run（run.resume）。",
        target_kind=ToolTargetKind.COMMAND,
        command_type="run.resume",
        aggregate_type="run",
        aggregate_arg="run_id",
        input_schema=_props(run_id=("目标 Run id", True)),
    ),
    CapabilityToolSpec(
        name="muteki_stop_run",
        description="停止 Run（run.stop）。",
        target_kind=ToolTargetKind.COMMAND,
        command_type="run.stop",
        aggregate_type="run",
        aggregate_arg="run_id",
        input_schema=_props(run_id=("目标 Run id", True)),
    ),
    CapabilityToolSpec(
        name="muteki_send_operator_directive",
        description="向 Run 发送 Operator 指令（run.operator_directive）。",
        target_kind=ToolTargetKind.COMMAND,
        command_type="run.operator_directive",
        aggregate_type="run",
        aggregate_arg="run_id",
        input_schema=_props(
            run_id=("目标 Run id", True),
            text=("指令内容", True),
        ),
    ),
    CapabilityToolSpec(
        name="muteki_add_run_context",
        description=(
            "向活动 Run 注入结构化上下文，供 Coordinator 和后续 Worker 使用。"
            "context 可包含 content、kind、standing 和 max_bindings。"
        ),
        target_kind=ToolTargetKind.COMMAND,
        command_type="run.add_context",
        aggregate_type="run",
        aggregate_arg="run_id",
        input_schema={
            "type": "object",
            "properties": {
                "run_id": {"type": "string", "description": "目标 Run id"},
                "context": {
                    "type": "object",
                    "description": "上下文对象，至少包含 content；kind 只能使用列出的标准值",
                    "properties": {
                        "content": {
                            "type": "string",
                            "minLength": 1,
                            "description": "要注入的上下文正文",
                        },
                        "kind": {
                            "type": "string",
                            "enum": [
                                "clue", "constraint", "endpoint", "objective",
                                "secret_ref", "operator_note",
                            ],
                            "description": "上下文种类；默认 clue",
                        },
                        "standing": {
                            "type": "boolean",
                            "description": "是否作为持续上下文保留",
                        },
                        "max_bindings": {
                            "type": "integer",
                            "minimum": 1,
                            "description": "最多交付次数；standing=true 时可省略",
                        },
                        "metadata": {
                            "type": "object",
                            "description": "可选的结构化元数据",
                            "additionalProperties": True,
                        },
                    },
                    "required": ["content"],
                    "additionalProperties": True,
                },
                "target": {
                    "type": "string",
                    "description": "可选：global / worker:<id> / role:<name>",
                },
            },
            "required": ["run_id", "context"],
        },
    ),
    CapabilityToolSpec(
        name="muteki_spawn_run_worker",
        description="为活动 Coordinator Run 增加一个 Worker；可指定 engine。",
        target_kind=ToolTargetKind.COMMAND,
        command_type="run.spawn_worker",
        aggregate_type="run",
        aggregate_arg="run_id",
        input_schema=_props(
            run_id=("目标 Run id", True),
            engine=("可选：Worker 引擎；省略时由 Coordinator 选择", False),
        ),
    ),
    CapabilityToolSpec(
        name="muteki_cancel_run_worker",
        description="停止活动 Run 中指定的 Worker。",
        target_kind=ToolTargetKind.COMMAND,
        command_type="run.cancel_worker",
        aggregate_type="run",
        aggregate_arg="run_id",
        input_schema=_props(
            run_id=("目标 Run id", True),
            worker_id=("Worker id", True),
        ),
    ),
    # -- 对话子 Agent（Muteki 自管；子对话是独立 Thread） -------------------------
    CapabilityToolSpec(
        name="muteki_subagent_runtimes",
        description=(
            "查询聊天子代理可选的真实 provider/Runtime、凭据引用、模型、支持的 effort 和权限。"
            "可按 engine 筛选；不返回秘密，不猜测模型能力或价格，不触发付费推理。"
            "跨 provider 委派前先查询，使用 available=true 的接入；目录缺失时请用户在设置中刷新。"
        ),
        target_kind=ToolTargetKind.QUERY,
        query_type="conversation.subagent.runtimes",
        aggregate_type="thread",
        aggregate_from_caller_thread=True,
        input_schema={
            "type": "object",
            "properties": {"engine": {"type": "string", "description": "可选：provider id，如 codex / claude"}},
            "additionalProperties": False,
        },
    ),
    CapabilityToolSpec(
        name="muteki_spawn_subagent",
        description=(
            "创建一个由 Muteki 管理的子 Agent：新建一个与当前对话关联的子对话"
            "并把 prompt 作为它的第一条消息发出。子 Agent 看不到当前对话历史，"
            "prompt 必须自包含目标、已知事实、精确范围、约束和报告格式；长任务填写"
            "acceptance_criteria 与 routing_reason。completed 仅代表执行结束，主代理仍需独立验收。默认继承当前"
            "对话的引擎、凭据、模型、effort 与访问模式；访问模式不能高于当前"
            "对话。isolation=worktree 时在 Muteki 托管目录为子 Agent 新建 Git "
            "worktree（基于当前 HEAD，不含未提交改动）。wait=true 时最多等待 "
            "timeout_seconds，返回最终回复全文；超时返回 wait.outcome=pending，"
            "之后用 muteki_subagent_status 查询。层级上限与每个根对话的并发上限"
            "超出时返回带稳定 code 的错误。返回的 conversation_url 是可直接打开的子会话链接，不要猜域名或端口。"
        ),
        target_kind=ToolTargetKind.COMMAND,
        command_type="conversation.subagent.spawn",
        aggregate_type="thread",
        aggregate_from_caller_thread=True,
        input_schema={
            "type": "object",
            "properties": {
                "prompt": {"type": "string", "minLength": 1,
                           "description": "子 Agent 的完整任务说明（自包含）"},
                "title": {"type": "string", "description": "可选：子对话标题"},
                "acceptance_criteria": {"type": "string", "minLength": 1,
                                        "description": "验收条件：检查命令、预期退出码及人工检查项；会完整交付子代理并在 UI 展示"},
                "routing_reason": {"type": "string", "minLength": 1,
                                   "description": "为何该 provider/模型/effort 适合这项任务；不要臆测价格或能力排名"},
                "engine": {"type": "string",
                           "description": "可选：引擎 id（如 codex / claude）；省略时继承当前对话"},
                "adapter_id": {"type": "string",
                               "description": "可选：精确 Runtime adapter id；与 engine 二选一"},
                "instance_id": {"type": "string", "description": "可选：Runtime instance id"},
                "credential_id": {"type": "string",
                                  "description": "可选：全局凭据 id；换引擎时省略则使用该引擎的系统登录"},
                "model": {"type": "string", "description": "可选：模型；换引擎时必须给出"},
                "effort": {"type": "string", "description": "可选：推理强度"},
                "access_mode": {
                    "type": "string",
                    "enum": ["supervised", "auto-accept-edits", "auto", "full-access"],
                    "description": "可选：访问模式，不能高于当前对话",
                },
                "isolation": {"type": "string", "enum": ["shared", "worktree"],
                              "default": "shared",
                              "description": "shared 共用当前工作区；worktree 使用独立 Git worktree"},
                "wait": {"type": "boolean", "default": False,
                         "description": "是否等待子 Agent 完成"},
                "timeout_seconds": {"type": "number", "exclusiveMinimum": 0,
                                    "description": "wait=true 时的最长等待秒数；省略用服务默认值"},
                "idempotency_key": {"type": "string", "minLength": 1, "maxLength": 200,
                                    "description": "可选：重试同一次创建时复用，避免重复创建"},
            },
            "required": ["prompt"],
            "additionalProperties": False,
        },
    ),
    CapabilityToolSpec(
        name="muteki_subagent_status",
        description=(
            "查询当前对话创建的子 Agent。给出 subagent_id 时返回该子 Agent 的"
            "状态与最终回复全文（可用 wait_seconds 有界等待其结束）；省略时列出"
            "当前对话的全部直接子 Agent，最终回复以 message_id 引用给出。"
        ),
        target_kind=ToolTargetKind.QUERY,
        query_type="conversation.subagent.status",
        aggregate_type="thread",
        aggregate_from_caller_thread=True,
        input_schema={
            "type": "object",
            "properties": {
                "subagent_id": {"type": "string", "minLength": 1,
                                "description": "可选：muteki_spawn_subagent 返回的 subagent_id"},
                "wait_seconds": {"type": "number", "minimum": 0,
                                 "description": "可选：与 subagent_id 同用，最多等待其结束的秒数"},
            },
            "additionalProperties": False,
        },
    ),
    CapabilityToolSpec(
        name="muteki_subagent_send",
        description=(
            "在当前对话的一个已结束的直接子代理会话中继续发送任务或修正说明，沿用其会话上下文，"
            "不新建子代理、不更换 provider/model/effort。prompt 写明聚焦的反馈和验收要求。"
            "仍在执行、已取消、已归档或队列待处理时明确报错。wait=true 可有界等待，"
            "pending 不代表完成；重试相同投递时复用 idempotency_key。"
        ),
        target_kind=ToolTargetKind.COMMAND,
        command_type="conversation.subagent.send",
        aggregate_type="thread",
        aggregate_from_caller_thread=True,
        input_schema={
            "type": "object",
            "properties": {
                "subagent_id": {"type": "string", "minLength": 1},
                "prompt": {"type": "string", "minLength": 1},
                "wait": {"type": "boolean", "default": False},
                "timeout_seconds": {"type": "number", "exclusiveMinimum": 0},
                "idempotency_key": {"type": "string", "minLength": 1, "maxLength": 200},
            },
            "required": ["subagent_id", "prompt"],
            "additionalProperties": False,
        },
    ),
    CapabilityToolSpec(
        name="muteki_subagent_cancel",
        description="取消当前对话创建的一个子 Agent，并级联取消它创建的所有后代子 Agent。",
        target_kind=ToolTargetKind.COMMAND,
        command_type="conversation.subagent.cancel",
        aggregate_type="thread",
        aggregate_from_caller_thread=True,
        input_schema={
            "type": "object",
            "properties": {
                "subagent_id": {"type": "string", "minLength": 1,
                                "description": "要取消的 subagent_id"},
                "reason": {"type": "string", "description": "可选：取消原因"},
            },
            "required": ["subagent_id"],
            "additionalProperties": False,
        },
    ),
    # -- 比赛模式（command_type 的 Handler 由 COMP-09 注册） --------------------
    CapabilityToolSpec(
        name="muteki_list_competitions",
        description=(
            "列出 Muteki 已登记的比赛。查询比赛题目历史时先使用本工具取得"
            "competition_id，再调用 muteki_list_competition_challenges。"
        ),
        target_kind=ToolTargetKind.QUERY,
        query_type="competition.list",
        aggregate_type="competition",
        input_schema=_props(),
    ),
    CapabilityToolSpec(
        name="muteki_list_connections",
        description=(
            "列出比赛平台连接及当前状态。需要同步或排查比赛前，先用本工具"
            "取得 connection_id；不返回凭据原文。"
        ),
        target_kind=ToolTargetKind.QUERY,
        query_type="connection.list",
        aggregate_type="connection",
        input_schema=_props(),
    ),
    CapabilityToolSpec(
        name="muteki_list_competition_challenges",
        description=(
            "列出指定比赛的全部题目和同步状态；直接读取比赛投影，不需要访问"
            "数据库或比赛目录。"
        ),
        target_kind=ToolTargetKind.QUERY,
        query_type="competition.challenges",
        aggregate_type="competition",
        aggregate_arg="competition_id",
        input_schema=_props(competition_id=("比赛 id", True)),
    ),
    CapabilityToolSpec(
        name="muteki_create_connection",
        description="创建比赛平台连接（competition.connection.create）。",
        target_kind=ToolTargetKind.COMMAND,
        command_type="competition.connection.create",
        aggregate_type="connection",
        input_schema=_props(
            platform_kind=("平台标识（ctfd / rctf / gzctf / generic_browser …）", True),
            base_url=("平台地址", True),
            account_key=("账户标识（用户名 / token 指纹等）", True),
            credential_ref=("可选：凭据的 secret:// 引用（绝不传原文）", False),
        ),
    ),
    CapabilityToolSpec(
        name="muteki_test_connection",
        description="探测指定比赛平台连接并更新连接状态，返回异步回执。",
        target_kind=ToolTargetKind.COMMAND,
        command_type="connection.test",
        aggregate_type="connection",
        aggregate_arg="connection_id",
        input_schema=_props(connection_id=("连接 id", True)),
    ),
    CapabilityToolSpec(
        name="muteki_sync_competition",
        description="同步比赛题目与远端状态（competition.sync），立即返回 receipt + event cursor。",
        target_kind=ToolTargetKind.COMMAND,
        command_type="competition.sync",
        aggregate_type="competition",
        aggregate_arg="competition_id",
        input_schema=_props(
            connection_id=("平台连接 id", True),
            external_competition_id=("平台侧比赛 id", True),
            competition_id=("可选：已知比赛 id（重复同步归一）", False),
            title=("可选：比赛标题", False),
            description=("可选：比赛说明", False),
            starts_at=("可选：ISO 8601 开始时间", False),
            ends_at=("可选：ISO 8601 结束时间", False),
        ),
    ),
    CapabilityToolSpec(
        name="muteki_update_policy",
        description="更新比赛调度策略（competition.policy.update）。",
        target_kind=ToolTargetKind.COMMAND,
        command_type="competition.policy.update",
        aggregate_type="competition",
        aggregate_arg="competition_id",
        input_schema={
            "type": "object",
            "properties": {
                "competition_id": {"type": "string", "description": "比赛 id"},
                "automation_mode": {
                    "type": "string",
                    "enum": ["observe", "assisted", "autonomous"],
                    "description": "自动化模式",
                },
                "max_concurrent_runs": {
                    "type": "integer", "minimum": 0,
                    "description": "最大并发 Run 数",
                },
                "max_instances": {
                    "type": "integer", "minimum": 0,
                    "description": "最大动态实例数",
                },
                "submission_cooldown_seconds": {
                    "type": "number", "minimum": 0,
                    "description": "提交冷却秒数",
                },
                "category_allow": {
                    "type": "array", "items": {"type": "string"},
                    "description": "允许自动调度的分类",
                },
                "category_deny": {
                    "type": "array", "items": {"type": "string"},
                    "description": "禁止自动调度的分类",
                },
                "budget_limits": {
                    "type": "object",
                    "additionalProperties": {"type": "number"},
                    "description": "按预算种类设置上限",
                },
            },
            "required": ["competition_id"],
        },
    ),
    CapabilityToolSpec(
        name="muteki_queue_challenge",
        description="把题目加入调度队列（competition.challenge.queue）。",
        target_kind=ToolTargetKind.COMMAND,
        command_type="competition.challenge.queue",
        aggregate_type="competition",
        aggregate_arg="competition_id",
        input_schema=_props(
            competition_id=("比赛 id", True),
            challenge_id=("题目 id", True),
            priority=("可选：Operator 固定优先级", False),
        ),
    ),
    CapabilityToolSpec(
        name="muteki_select_challenge",
        description="把比赛题目标记为已选择，保留人工确认后再排队。",
        target_kind=ToolTargetKind.COMMAND,
        command_type="challenge.select",
        aggregate_type="competition",
        aggregate_arg="competition_id",
        input_schema=_props(
            competition_id=("比赛 id", True),
            challenge_id=("题目 id", True),
        ),
    ),
    CapabilityToolSpec(
        name="muteki_skip_challenge",
        description="跳过指定比赛题目并记录状态变化。",
        target_kind=ToolTargetKind.COMMAND,
        command_type="challenge.skip",
        aggregate_type="competition",
        aggregate_arg="competition_id",
        input_schema=_props(
            competition_id=("比赛 id", True),
            challenge_id=("题目 id", True),
        ),
    ),
    CapabilityToolSpec(
        name="muteki_start_scheduler",
        description="启动指定比赛的自动调度器。",
        target_kind=ToolTargetKind.COMMAND,
        command_type="scheduler.start",
        aggregate_type="competition",
        aggregate_arg="competition_id",
        input_schema=_props(competition_id=("比赛 id", True)),
    ),
    CapabilityToolSpec(
        name="muteki_pause_scheduler",
        description="暂停指定比赛的自动调度器。",
        target_kind=ToolTargetKind.COMMAND,
        command_type="scheduler.pause",
        aggregate_type="competition",
        aggregate_arg="competition_id",
        input_schema=_props(competition_id=("比赛 id", True)),
    ),
    CapabilityToolSpec(
        name="muteki_resume_scheduler",
        description="恢复指定比赛的自动调度器。",
        target_kind=ToolTargetKind.COMMAND,
        command_type="scheduler.resume",
        aggregate_type="competition",
        aggregate_arg="competition_id",
        input_schema=_props(competition_id=("比赛 id", True)),
    ),
    CapabilityToolSpec(
        name="muteki_ensure_challenge_instance",
        description="为指定比赛题目申请或确保一个动态实例租约。",
        target_kind=ToolTargetKind.COMMAND,
        command_type="instance.ensure",
        aggregate_type="competition",
        aggregate_arg="competition_id",
        input_schema={
            "type": "object",
            "properties": {
                "competition_id": {"type": "string", "description": "比赛 id"},
                "challenge_id": {"type": "string", "description": "题目 id"},
                "owner": {"type": "string", "description": "可选：租约 owner"},
                "ttl_seconds": {
                    "type": "integer", "minimum": 0,
                    "description": "可选：租约 TTL 秒数",
                },
            },
            "required": ["competition_id", "challenge_id"],
        },
    ),
    CapabilityToolSpec(
        name="muteki_stop_challenge_instance",
        description="释放指定 lease，或释放题目当前的活动实例租约。",
        target_kind=ToolTargetKind.COMMAND,
        command_type="instance.stop",
        aggregate_type="competition",
        aggregate_arg="competition_id",
        input_schema={
            "type": "object",
            "properties": {
                "competition_id": {"type": "string", "description": "比赛 id"},
                "challenge_id": {"type": "string", "description": "可选：题目 id"},
                "lease_id": {"type": "string", "description": "可选：租约 id"},
            },
            "required": ["competition_id"],
        },
    ),
    CapabilityToolSpec(
        name="muteki_get_competition_snapshot",
        description="读取比赛快照（competition.snapshot）。",
        target_kind=ToolTargetKind.QUERY,
        query_type="competition.snapshot",
        aggregate_type="competition",
        aggregate_arg="competition_id",
        input_schema=_props(competition_id=("比赛 id", True)),
    ),
    CapabilityToolSpec(
        name="muteki_list_competition_submissions",
        description=(
            "列出指定比赛的提交记录和候选答案，包括状态、判定和来源引用。"
        ),
        target_kind=ToolTargetKind.QUERY,
        query_type="competition.submissions",
        aggregate_type="competition",
        aggregate_arg="competition_id",
        input_schema=_props(competition_id=("比赛 id", True)),
    ),
    CapabilityToolSpec(
        name="muteki_list_competition_leases",
        description="列出指定比赛的动态实例租约及当前状态。",
        target_kind=ToolTargetKind.QUERY,
        query_type="competition.leases",
        aggregate_type="competition",
        aggregate_arg="competition_id",
        input_schema=_props(competition_id=("比赛 id", True)),
    ),
    CapabilityToolSpec(
        name="muteki_submit_competition_answer",
        description="提交比赛答案（competition.submission.submit），经统一提交管线。",
        target_kind=ToolTargetKind.COMMAND,
        command_type="competition.submission.submit",
        aggregate_type="competition",
        aggregate_arg="competition_id",
        input_schema=_props(
            competition_id=("比赛 id", True),
            challenge_id=("题目 id", True),
            answer=("提交的 Flag / 答案", True),
            answer_slot=("可选：答案槽位（多 Flag 题）", False),
            witness=("Gate witness（真实命令输出 / 产物内容）", True),
            source_run_id=("产生候选的 Run id", True),
            source_kind=(
                "执行来源类别：execution_output、artifact 或 run_event", True),
            execution_generation=("可选：来源执行代", False),
            worker_id=("可选：来源 Worker id", False),
            session_id=("可选：来源 Agent 会话 id", False),
            artifact_path=("可选：来源产物路径", False),
        ),
    ),
    # -- SRC 赏金工作台（Task 身份从调用方 Binding 的 Thread 反解） -------------
    CapabilityToolSpec(
        name="muteki_list_src_tasks",
        description="列出当前可信身份可读的 SRC 长期任务及其主 Thread、Run 数量和来源水位。",
        target_kind=ToolTargetKind.QUERY, query_type="src.task.list",
        input_schema=_props(),
    ),
    CapabilityToolSpec(
        name="muteki_get_src_task",
        description="读取当前 SRC Task 的目标、授权输入、主 Thread 和投影水位。",
        target_kind=ToolTargetKind.QUERY, query_type="src.task.get",
        aggregate_type="thread", aggregate_from_caller_thread=True,
        input_schema=_props(),
    ),
    CapabilityToolSpec(
        name="muteki_list_src_runs",
        description="列出当前 SRC Task 绑定的全部 Pentest Run 及双向绑定、目标和快照。",
        target_kind=ToolTargetKind.QUERY, query_type="src.run.list",
        aggregate_type="thread", aggregate_from_caller_thread=True,
        input_schema=_props(),
    ),
    CapabilityToolSpec(
        name="muteki_ensure_src_run",
        description="为当前 SRC Task 幂等取得或创建一个具体 URL 的深入 Run；公司名或搜索候选不是有效目标。",
        target_kind=ToolTargetKind.COMMAND, command_type="src.run.ensure",
        aggregate_type="thread", aggregate_from_caller_thread=True,
        input_schema={
            "type": "object",
            "properties": {
                "target_url": {"type": "string", "description": "完整 http(s) 目标 URL"},
                "question": {"type": "string", "description": "此 Run 要回答的具体研究问题"},
                "binding_key": {"type": "string", "description": "稳定深入目标键；续跑复用"},
                "binding_revision": {"type": "integer", "minimum": 1},
                "plan_ref": {"type": "string"}, "authorization_ref": {"type": "string"},
                "evidence_refs": {"type": "array", "items": {"type": "string"}},
                "identity": {"type": "object"}, "object_state": {"type": "object"},
                "budget": {"type": "object"}, "target_version": {"type": "string"},
                "source_message_id": {"type": "string"},
                "idempotency_key": {"type": "string"},
            },
            "required": ["target_url", "question"], "additionalProperties": True,
        },
    ),
    CapabilityToolSpec(
        name="muteki_list_src_records",
        description="按 kind 读取当前 SRC Task 的持久记录；控制面记录仍受服务端权限限制。",
        target_kind=ToolTargetKind.QUERY, query_type="src.record.list",
        aggregate_type="thread", aggregate_from_caller_thread=True,
        input_schema={
            "type": "object",
            "properties": {
                "kind": {"type": "string"}, "parent_id": {"type": "string"},
                "limit": {"type": "integer", "minimum": 1, "maximum": 1000, "default": 100},
                "cursor": {"type": "string", "description": "上一页 next_cursor；必须沿用原筛选"},
                "after_cursor": {"type": "string", "description": "cursor 的兼容别名"},
            },
        },
    ),
    CapabilityToolSpec(
        name="muteki_get_src_record",
        description="按 kind 和 record_id 读取当前 SRC Task 的一条完整持久记录。",
        target_kind=ToolTargetKind.QUERY, query_type="src.record.get",
        aggregate_type="thread", aggregate_from_caller_thread=True,
        input_schema=_props(kind=("记录类型", True), record_id=("记录 ID", True)),
    ),
    CapabilityToolSpec(
        name="muteki_start_src_run",
        description="启动当前 SRC Task 已绑定 Run 的标准 Pentest 执行器；目标默认使用冻结契约，续跑使用原 Run。",
        target_kind=ToolTargetKind.COMMAND, command_type="src.run.start",
        aggregate_type="thread", aggregate_from_caller_thread=True,
        input_schema={
            "type": "object",
            "properties": {
                "run_id": {"type": "string", "description": "muteki_ensure_src_run 返回的 Run ID"},
                "target_url": {"type": "string", "description": "可选，必须与冻结契约一致"},
                "prompt": {"type": "string"}, "goal": {"type": "string"},
                "scope": {"type": "string"}, "idempotency_key": {"type": "string"},
            },
            "required": ["run_id"], "additionalProperties": True,
        },
    ),
    CapabilityToolSpec(
        name="muteki_get_src_policy",
        description="读取当前 SRC Task 的有效执行策略、授权目标、排除项、规则与版本。",
        target_kind=ToolTargetKind.QUERY, query_type="src.policy.current",
        aggregate_type="thread", aggregate_from_caller_thread=True,
        input_schema=_props(),
    ),
    CapabilityToolSpec(
        name="muteki_get_src_execution_capabilities",
        description="读取当前 Task 各执行通道的真实环境 generation 与覆盖能力，不把声明当强制保护。",
        target_kind=ToolTargetKind.QUERY, query_type="src.execution.capabilities",
        aggregate_type="thread", aggregate_from_caller_thread=True,
        input_schema=_props(),
    ),
    CapabilityToolSpec(
        name="muteki_get_src_execution_receipt",
        description="按 invocation_id 读取策略决定、审批等待、真实下发、完成或 outcome_unknown 回执。",
        target_kind=ToolTargetKind.QUERY, query_type="src.execution.receipt",
        aggregate_type="thread", aggregate_from_caller_thread=True,
        input_schema=_props(invocation_id=("执行调用 ID", True)),
    ),
    CapabilityToolSpec(
        name="muteki_workbench_suggestion_propose",
        description="向工作台提出一项变更建议（src.workbench.suggestion.propose）。只记录建议数据，由操作者决定是否采纳；不要自行应用变更。",
        target_kind=ToolTargetKind.COMMAND,
        command_type="src.workbench.suggestion.propose",
        aggregate_type="task",
        aggregate_arg="task_id",
        input_schema={
            "type": "object", "required": ["kind", "proposal"],
            "properties": {
                "kind": {"type": "string", "enum": ["classify", "new_finding", "merge", "conflict_resolve", "plan_update", "report_draft", "retest_record"],
                         "description": "建议类型"},
                "proposal": {"type": "object", "description": "建议内容（按 kind 的结构化字段，如 finding_id/asset_id/report_id/primary_id/member_ids/chosen_value/summary/sections/verdict）"},
                "rationale": {"type": "string", "description": "给出建议的理由"},
                "turn_id": {"type": "string", "description": "关联的对话轮次 id（可选）"},
                "source_message_id": {"type": "string", "description": "关联的消息 id（可选）"},
            },
        },
    ),
    # -- 管理模式（Runtime / Profile / 全局运行管理） -------------------------
    CapabilityToolSpec(
        name="muteki_list_runtime_instances",
        description="列出外部 Agent Runtime instance（runtime.instance.list）。",
        target_kind=ToolTargetKind.QUERY,
        query_type="runtime.instance.list",
        aggregate_type="runtime",
        input_schema=_props(),
    ),
    CapabilityToolSpec(
        name="muteki_list_worker_profiles",
        description="列出 Worker Profile（worker_profile.list）。",
        target_kind=ToolTargetKind.QUERY,
        query_type="worker_profile.list",
        aggregate_type="profile",
        input_schema=_props(),
    ),
)


def _src_agent_command_spec(operation: str) -> CapabilityToolSpec:
    name = "muteki_" + operation.removeprefix("src.").replace(".", "_")
    return CapabilityToolSpec(
        name=name,
        description=(
            f"在当前可信 SRC Task 执行 {operation}。输入使用该领域版本化契约；"
            "服务端重新校验身份、参数、授权与执行策略，不允许跨 Task 或修改控制面。"
        ),
        target_kind=ToolTargetKind.COMMAND, command_type=operation,
        aggregate_type="thread", aggregate_from_caller_thread=True,
        input_schema={
            "type": "object",
            "properties": {
                "idempotency_key": {"type": "string", "description": "可选：同一次逻辑操作的稳定幂等键"},
            },
            "additionalProperties": True,
        },
    )


_SRC_AGENT_DOMAIN_OPERATIONS = (
    "src.exchange.record", "src.exchange.draft.save", "src.exchange.draft.execute",
    "src.research.plan.save", "src.research.skill.register",
    "src.research.openapi.ingest", "src.research.javascript.ingest",
    "src.research.runtime.register", "src.research.dataflow.register",
    "src.research.page_action.register", "src.research.business.save",
    "src.research.permission_question.add", "src.research.experiment.record",
    "src.research.capability_chain.record", "src.research.precondition_trial.record",
    "src.asset.observe", "src.asset.relate", "src.asset.derive",
    "src.ingestion.ingest", "src.query.save", "src.vocabulary.save",
    "src.tag.save", "src.tag.assign", "src.note.save",
    "src.connector.import", "src.connector.query", "src.connector.repository",
    "src.report.freeze", "src.report.reproduction.record", "src.submission.export",
    "src.report.render", "src.review.record",
    "src.retest.request", "src.retest.dispatch", "src.retest.complete",
    "src.finding.claim.create", "src.finding.review.record", "src.finding.invalidate",
    "src.finding.deduplicate", "src.browser.ensure", "src.browser.context.ensure",
    "src.browser.tab.open", "src.browser.tab.release", "src.browser.screenshot.capture",
    "src.browser.image.review", "src.browser.execution.record", "src.browser.run.finish",
    "src.identity.verify", "src.identity.recover", "src.object.lease.acquire",
    "src.object.lease.release", "src.object.register", "src.object.experiment.begin",
    "src.object.experiment.after", "src.object.experiment.restore",
    "src.oast.allocate", "src.oast.poll", "src.oast.expire", "src.oast.health",
    "src.oast.local.start", "src.stream.websocket.connect", "src.stream.websocket.send",
    "src.stream.websocket.receive", "src.stream.websocket.acknowledge",
    "src.stream.websocket.close", "src.stream.sse.connect", "src.stream.sse.read",
    "src.stream.sse.close",
)
_SRC_AGENT_DOMAIN_SPECS = tuple(
    _src_agent_command_spec(operation) for operation in _SRC_AGENT_DOMAIN_OPERATIONS
)
_TOOL_SPECS = (*_TOOL_SPECS, *_SRC_AGENT_DOMAIN_SPECS)


def _src_agent_query_spec(operation: str) -> CapabilityToolSpec:
    return CapabilityToolSpec(
        name="muteki_" + operation.removeprefix("src.").replace(".", "_"),
        description=f"读取当前可信 SRC Task 的 {operation} 投影；包含真实来源、版本、完整性与错误状态。",
        target_kind=ToolTargetKind.QUERY, query_type=operation,
        aggregate_type="thread", aggregate_from_caller_thread=True,
        input_schema={"type": "object", "additionalProperties": True},
    )


_SRC_AGENT_QUERY_OPERATIONS = (
    "src.exchange.query", "src.exchange.read", "src.exchange.draft.read",
    "src.exchange.compare", "src.exchange.export", "src.research.records",
    "src.research.record.get", "src.research.plan.current", "src.research.skill.read",
    "src.research.openapi.read", "src.research.openapi.compare",
    "src.research.javascript.read", "src.research.backlog", "src.research.history",
    "src.research.precondition.summary", "src.asset.detail", "src.asset.published",
    "src.asset.query", "src.asset.saved_query.read", "src.asset.full_text",
    "src.asset.pivot", "src.asset.export", "src.report.get", "src.report.list",
    "src.report.templates", "src.artifact.read",
    "src.feedback.diff", "src.feedback.platform_status", "src.feedback.history",
    "src.finding.claims", "src.finding.claim.get", "src.finding.reviews",
    "src.finding.status", "src.finding.deduplications", "src.connector.manifests",
    "src.connector.manifest", "src.connector.config", "src.monitor.get",
    "src.monitor.list", "src.monitor.changes", "src.monitor.notifications",
    "src.retest.recover", "src.identity.get", "src.identity.verifications",
    "src.identity.compare", "src.object.history", "src.oast.events",
    "src.stream.export", "src.knowledge.search", "src.knowledge.read",
    "src.knowledge.source.read", "src.knowledge.history", "src.browser.service",
    "src.browser.context", "src.browser.generation", "src.browser.image.read",
    "src.inspection.requests",
)
_SRC_AGENT_QUERY_SPECS = tuple(
    _src_agent_query_spec(operation) for operation in _SRC_AGENT_QUERY_OPERATIONS
)
_TOOL_SPECS = (*_TOOL_SPECS, *_SRC_AGENT_QUERY_SPECS)


class CapabilityCatalog:
    """工具目录的只读视图；按名称索引，供 Gateway 与 Binding 服务共用。"""

    def __init__(self, specs: tuple[CapabilityToolSpec, ...] = _TOOL_SPECS) -> None:
        self._by_name: dict[str, CapabilityToolSpec] = {}
        self._validators: dict[str, Draft202012Validator] = {}
        for spec in specs:
            if spec.name in self._by_name:
                raise ValueError(f"duplicate capability tool: {spec.name}")
            self._by_name[spec.name] = spec
            Draft202012Validator.check_schema(spec.input_schema)
            self._validators[spec.name] = Draft202012Validator(spec.input_schema)

    def validation_errors(self, name: str, arguments: dict[str, Any]) -> list[dict[str, Any]]:
        """Report structural errors without echoing argument values or secrets."""
        return [{"path": ["arguments", *error.absolute_path], "rule": error.validator,
                 "expected": error.validator_value}
                for error in self._validators[name].iter_errors(arguments)]

    def get(self, name: str) -> Optional[CapabilityToolSpec]:
        return self._by_name.get(name)

    def require(self, name: str) -> CapabilityToolSpec:
        spec = self.get(name)
        if spec is None:
            raise KeyError(f"unknown capability tool: {name}")
        return spec

    def names(self) -> list[str]:
        return sorted(self._by_name)

    def filter(self, tool_set: list[str]) -> list[CapabilityToolSpec]:
        """按 Binding 的 tool_set 过滤（保持 tool_set 顺序，忽略未知名称）。"""
        return [self._by_name[n] for n in tool_set if n in self._by_name]

    def command_types_of(self, tool_set: list[str]) -> list[str]:
        """该工具集合涉及的 command_type 清单（写入 Binding.allowed_commands）。"""
        return sorted({
            spec.command_type for spec in self.filter(tool_set)
            if spec.command_type
        })

    def query_types_of(self, tool_set: list[str]) -> list[str]:
        """该工具集合涉及的 query_type 清单（写入 Binding.allowed_queries）。"""
        return sorted({
            spec.query_type for spec in self.filter(tool_set)
            if spec.query_type
        })


#: 产品默认目录（单例语义；CAP-02 动态生成协议工具时复用）。
DEFAULT_CATALOG = CapabilityCatalog()

# ---------------------------------------------------------------------------
# 四类 Thread 模式的默认工具范围（任务书 6.6 表格与 10.11 清单）
# ---------------------------------------------------------------------------

#: 所有模式共享的只读产品历史与异步回执入口。
_HISTORY_TOOLS: tuple[str, ...] = (
    "muteki_list_projects",
    "muteki_list_threads",
    "muteki_list_tasks",
    "muteki_get_task",
    "muteki_list_runs",
    "muteki_get_command_receipt",
)

#: single_task：完整 Task → Run → Swarm 生命周期、进度、SharedGraph、控制
#: 和 Worker 调整。比赛域能力不在本模式中混入。
_SINGLE_TASK_TOOLS: tuple[str, ...] = (
    *_HISTORY_TOOLS,
    "muteki_create_task",
    "muteki_dispatch_challenge",
    "muteki_create_run",
    "muteki_start_swarm",
    "muteki_resolve_run",
    "muteki_get_run_snapshot",
    "muteki_read_run_events",
    "muteki_wait_run",
    "muteki_read_shared_graph",
    "muteki_pause_run",
    "muteki_resume_run",
    "muteki_stop_run",
    "muteki_send_operator_directive",
    "muteki_add_run_context",
    "muteki_spawn_run_worker",
    "muteki_cancel_run_worker",
)

#: Muteki 自管子 Agent：子 Agent 本身是 conversation Thread，所以只在对话模式提供。
SUBAGENT_TOOLS: tuple[str, ...] = (
    "muteki_subagent_runtimes",
    "muteki_spawn_subagent",
    "muteki_subagent_send",
    "muteki_subagent_status",
    "muteki_subagent_cancel",
)

#: conversation：普通聊天也可以直接下发和控制一个单题任务。这样 Agent 不必
#: 切换到单题页面、读取 sessions 或绕过能力网关；在 single_task 范围之外只
#: 增加子 Agent 工具，故意不带任何比赛平台或全局管理工具。
_CONVERSATION_TOOLS: tuple[str, ...] = (*_SINGLE_TASK_TOOLS, *SUBAGENT_TOOLS)

#: 2026-08-29 之前 conversation 的产品默认工具。仅用于把当时由产品自动
#: 签发的 Binding 迁移到新的对话控制集合；它不代表用户手工自定义的工具集。
_PREVIOUS_CONVERSATION_TOOLS: tuple[str, ...] = (
    *_HISTORY_TOOLS,
    "muteki_list_competitions",
    "muteki_list_connections",
    "muteki_list_competition_challenges",
    "muteki_list_competition_submissions",
    "muteki_list_competition_leases",
    "muteki_create_task",
)

#: competition：平台连接、同步、题目队列、批量下发、Scheduler、子 Run、
#: 动态实例、提交（10.11 全量清单）。
_COMPETITION_TOOLS: tuple[str, ...] = (
    *_SINGLE_TASK_TOOLS,
    "muteki_list_competitions",
    "muteki_list_connections",
    "muteki_list_competition_challenges",
    "muteki_list_competition_submissions",
    "muteki_list_competition_leases",
    "muteki_create_connection",
    "muteki_test_connection",
    "muteki_sync_competition",
    "muteki_update_policy",
    "muteki_select_challenge",
    "muteki_queue_challenge",
    "muteki_skip_challenge",
    "muteki_start_scheduler",
    "muteki_pause_scheduler",
    "muteki_resume_scheduler",
    "muteki_ensure_challenge_instance",
    "muteki_stop_challenge_instance",
    "muteki_get_competition_snapshot",
    "muteki_submit_competition_answer",
)

#: management：Runtime、Profile、连接和全局运行管理（需要管理授权）。
_MANAGEMENT_TOOLS: tuple[str, ...] = (
    *_COMPETITION_TOOLS,
    "muteki_list_runtime_instances",
    "muteki_list_worker_profiles",
)

_SRC_TOOLS: tuple[str, ...] = (
    *_CONVERSATION_TOOLS,
    "muteki_list_src_tasks",
    "muteki_get_src_task",
    "muteki_list_src_runs",
    "muteki_ensure_src_run",
    "muteki_list_src_records",
    "muteki_get_src_record",
    "muteki_start_src_run",
    "muteki_get_src_policy",
    "muteki_get_src_execution_capabilities",
    "muteki_get_src_execution_receipt",
    *(spec.name for spec in _SRC_AGENT_DOMAIN_SPECS),
    *(spec.name for spec in _SRC_AGENT_QUERY_SPECS),
)

#: 模式 → 默认工具集合。
MODE_TOOL_TEMPLATES: dict[ThreadMode, tuple[str, ...]] = {
    ThreadMode.CONVERSATION: _CONVERSATION_TOOLS,
    ThreadMode.SRC: _SRC_TOOLS,
    ThreadMode.SINGLE_TASK: _SINGLE_TASK_TOOLS,
    ThreadMode.COMPETITION: _COMPETITION_TOOLS,
    ThreadMode.MANAGEMENT: _MANAGEMENT_TOOLS,
}

# 旧版产品默认集合仅用于无损升级。用户自定义过的 tool_set 不匹配这些
# 精确元组，因此继续按原集合执行；历史默认 Binding 会在下一次 Session
# 签发时轮换到当前目录和相应 scope，不必让 Agent 回退读取目录或数据库。
_LEGACY_MODE_TOOL_TEMPLATES: dict[ThreadMode, tuple[tuple[str, ...], ...]] = {
    ThreadMode.SRC: (),
    ThreadMode.CONVERSATION: (
        (
            "muteki_list_projects",
            "muteki_list_threads",
            "muteki_create_task",
            "muteki_get_command_receipt",
        ),
        _PREVIOUS_CONVERSATION_TOOLS,
        # Product default before subagent tools existed.
        _SINGLE_TASK_TOOLS,
        # Product default before built-in orchestration and continuations.
        (*_SINGLE_TASK_TOOLS, "muteki_spawn_subagent", "muteki_subagent_status", "muteki_subagent_cancel"),
    ),
    ThreadMode.SINGLE_TASK: ((
        "muteki_list_projects",
        "muteki_list_threads",
        "muteki_create_task",
        "muteki_get_command_receipt",
        "muteki_dispatch_challenge",
        "muteki_start_swarm",
        "muteki_get_run_snapshot",
        "muteki_read_run_events",
        "muteki_wait_run",
        "muteki_read_shared_graph",
        "muteki_pause_run",
        "muteki_resume_run",
        "muteki_send_operator_directive",
    ),),
    ThreadMode.COMPETITION: ((
        "muteki_list_projects",
        "muteki_list_threads",
        "muteki_create_task",
        "muteki_get_command_receipt",
        "muteki_dispatch_challenge",
        "muteki_start_swarm",
        "muteki_get_run_snapshot",
        "muteki_read_run_events",
        "muteki_wait_run",
        "muteki_read_shared_graph",
        "muteki_pause_run",
        "muteki_resume_run",
        "muteki_send_operator_directive",
        "muteki_list_competitions",
        "muteki_create_connection",
        "muteki_sync_competition",
        "muteki_update_policy",
        "muteki_queue_challenge",
        "muteki_get_competition_snapshot",
        "muteki_submit_competition_answer",
    ),),
    ThreadMode.MANAGEMENT: ((
        "muteki_list_projects",
        "muteki_list_threads",
        "muteki_get_command_receipt",
        "muteki_get_run_snapshot",
        "muteki_read_run_events",
        "muteki_wait_run",
        "muteki_pause_run",
        "muteki_resume_run",
        "muteki_stop_run",
        "muteki_send_operator_directive",
        "muteki_list_competitions",
        "muteki_get_competition_snapshot",
        "muteki_create_connection",
        "muteki_update_policy",
        "muteki_list_runtime_instances",
        "muteki_list_worker_profiles",
    ),),
}


def default_tool_set(mode: ThreadMode) -> list[str]:
    """该模式的默认工具清单（新列表，调用方可安全修改）。"""
    return list(MODE_TOOL_TEMPLATES[mode])


def effective_tool_set(
    mode: ThreadMode | str,
    tool_set: list[str],
) -> list[str]:
    """返回 Binding 当前应使用的工具集合。

    精确匹配旧版产品默认集合时升级到当前默认；任何自定义集合原样保留。
    这样现有 MCP/ACP Session 可以在下一次 tools/list 时取得新工具，同时不
    扩大用户手工收窄过的授权。
    """
    resolved_mode = mode if isinstance(mode, ThreadMode) else ThreadMode(str(mode))
    if tuple(tool_set) in _LEGACY_MODE_TOOL_TEMPLATES[resolved_mode]:
        return default_tool_set(resolved_mode)
    return list(tool_set)


def default_resource_scopes(mode: ThreadMode, thread_id: str) -> list[str]:
    """该模式的默认 resource scope。

    所有模式都覆盖自身 Thread；任务 / Run / 比赛聚合按模式放宽。空清单在
    CommandPolicy 里不视为通配，所以这里显式给出。
    """
    thread_scope = f"thread:{thread_id}" if thread_id else "thread:*"
    if mode in {ThreadMode.CONVERSATION, ThreadMode.SRC}:
        return [thread_scope, "task:*", "project:*", "run:*"]
    if mode is ThreadMode.SINGLE_TASK:
        return [thread_scope, "task:*", "project:*", "run:*"]
    if mode is ThreadMode.COMPETITION:
        return [thread_scope, "task:*", "project:*", "run:*",
                "competition:*", "connection:*"]
    # management：全局运行管理，显式全量 scope（管理授权的体现）。
    return ["*:*"]


def resource_scopes_for_tool_set(
    mode: ThreadMode | str,
    thread_id: str,
    tool_set: list[str],
    *,
    catalog: CapabilityCatalog = DEFAULT_CATALOG,
) -> list[str]:
    """生成与自定义工具集合一致的资源范围。

    能力管理页允许在通用对话里手工加入单题或比赛工具。仅替换 tool_set 而
    继续保留模式默认 scope，会出现工具可见、调用却必然被拒绝的配置。这里
    保留模式的基础范围，并为已选择工具声明的聚合类型补齐显式通配范围。
    """
    resolved_mode = mode if isinstance(mode, ThreadMode) else ThreadMode(str(mode))
    scopes = default_resource_scopes(resolved_mode, thread_id)
    seen = set(scopes)
    for name in tool_set:
        spec = catalog.get(str(name or "").strip())
        if spec is not None and spec.aggregate_from_caller_thread:
            # Covered by the Thread's own scope; a wildcard would widen it.
            continue
        aggregate_type = str(spec.aggregate_type if spec is not None else "").strip()
        if not aggregate_type:
            continue
        scope = f"{aggregate_type}:*"
        if scope not in seen and "*:*" not in seen:
            scopes.append(scope)
            seen.add(scope)
    return scopes


__all__ = [
    "CapabilityCatalog",
    "CapabilityToolSpec",
    "DEFAULT_CATALOG",
    "MODE_TOOL_TEMPLATES",
    "SUBAGENT_TOOLS",
    "ToolTargetKind",
    "default_resource_scopes",
    "resource_scopes_for_tool_set",
    "default_tool_set",
    "effective_tool_set",
]
