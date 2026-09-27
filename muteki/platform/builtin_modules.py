"""内置 DomainModule 描述（任务书 6.8、9.4、11.1，CORE-03）。

三个产品内置模块的描述集中在此注册，不散落 ``if mode == ...`` 接线：

- ``builtin.conversation``：通用对话工作区，executor ``external-agent.single``。
  CONV-01（``muteki.conversation.store``）与 RUNTIME-01
  （``muteki.external_agents.base``）已落地，模块处于 ready。
- ``builtin.single-security-task``：当前 CTF/Pentest 单题，executor
  ``swarm.coordinator``，workspace kind ``single-security-task``，API routes
  引用现有 ``/api/runs`` 外形；依赖组件真实存在，处于 ready。命令/查询
  Handler 接线与创建合同见 ``muteki/single_task/``（SINGLE-01）。
- ``builtin.competition``：比赛工作区。COMP-01～COMP-09 已落地 store /
  scheduler / platforms，模块处于 ready。

本文件只放描述与注册逻辑；``muteki/conversation/``、``muteki/competition/``
的实现属于 CONV / COMP 工作包。
"""

from __future__ import annotations

from .contracts.modules import DomainModuleDescriptor
from .registry import (
    DomainModuleRegistry,
    ModuleComponent,
    ModuleRegistration,
    default_registry,
)


def conversation_descriptor() -> DomainModuleDescriptor:
    """通用对话模块（任务书第 9 章）。

    Thread/Session 生命周期事件归入 ``core.*`` 命名空间（契约 events.py
    未定义独立的 conversation 事件命名空间）。
    """
    return DomainModuleDescriptor(
        id="builtin.conversation",
        version="1.0.0",
        task_kinds=["conversation", "coding"],
        required_capabilities=[],
        default_executor="external-agent.single",
        command_handlers={
            # CONV-01 落地 muteki/conversation/commands.py 后由该 handler 承接
            "conversation.project.create": "conversation.commands",
            "conversation.thread.create": "conversation.commands",
            "conversation.turn.send": "conversation.commands",
            "conversation.turn.retry": "conversation.commands",
            "conversation.turn.steer": "conversation.commands",
            "conversation.turn.interrupt": "conversation.commands",
            "conversation.turn.resume": "conversation.commands",
            "conversation.thread.resume": "conversation.commands",
            "conversation.thread.fork": "conversation.commands",
            "conversation.thread.archive": "conversation.commands",
            "conversation.thread.unarchive": "conversation.commands",
            "conversation.approval.resolve": "conversation.commands",
            "conversation.user_input.resolve": "conversation.commands",
            "conversation.user_input.inject": "conversation.commands",
        },
        event_namespaces=["core."],
        api_routes=[
            "/api/projects",
            "/api/threads",
            "/api/threads/{thread_id}",
            "/api/threads/{thread_id}/events",
            "/api/threads/{thread_id}/commands",
            "/api/agent-runtimes",
        ],
        workspace_kind="conversation",
        ui_contributions={
            "title": "对话",
            "description": "与外部 Agent Runtime 的通用多轮对话工作区",
            "icon": "chat",
            "route": "/chat",
            "create_entry": "/chat",
            "aggregate_type": "thread",
        },
        artifact_types=["conversation.upload", "conversation.artifact"],
        graph_binding=None,
        gate_binding=None,
    )


def conversation_components() -> list[ModuleComponent]:
    return [
        ModuleComponent(
            name="conversation.store",
            module_path="muteki.conversation.store",
            detail="Thread/Project 持久化（CONV-01 已落地）",
        ),
        ModuleComponent(
            name="executor.external-agent.single",
            module_path="muteki.external_agents.base",
            detail="外部 Agent Runtime Adapter 基础层（RUNTIME-01）",
        ),
    ]


def single_security_task_descriptor() -> DomainModuleDescriptor:
    """CTF/Pentest 单题模块（任务书 3.3、9.4，SINGLE-01）。

    对现有单题页面和 API 做注册式接线：``/api/runs`` 与 ``/run/[id]`` 保持
    兼容，单题模块不发送 ``swarm_class``（空 spec 由 drivers 解析为标准
    ``Swarm``）。command_handlers 与 single_task 能力工具一一对应
    （``muteki/single_task/adapter.py`` 的 SINGLE_TASK_TOOL_HANDLERS 是同
    一份映射的运行时校验）；只读查询 run.snapshot / graph.shared.read 由
    COMMAND-01 / CAP-01 注册到同一 Command API。
    """
    return DomainModuleDescriptor(
        id="builtin.single-security-task",
        version="1.0.0",
        task_kinds=["ctf", "pentest"],
        required_capabilities=["run.flag_gate@1", "graph.shared@1", "run.coordinator@1"],
        default_executor="swarm.coordinator",
        command_handlers={
            # create task / dispatch challenge（muteki_create_task /
            # muteki_dispatch_challenge 共用 task.create 聚合）
            "task.create": "platform.command_handlers.task",
            # 绑定并启动 Swarm（run.create + run.start；muteki_start_swarm）
            "run.create": "platform.command_handlers.run",
            "run.start": "platform.command_handlers.run",
            "run.resolve": "platform.command_handlers.run",
            # Run 控制（muteki_pause_run / muteki_resume_run /
            # muteki_send_operator_directive 及既有 HITL 控制面）
            "run.pause": "platform.command_handlers.run",
            "run.resume": "platform.command_handlers.run",
            "run.stop": "platform.command_handlers.run",
            "run.operator_directive": "platform.command_handlers.run",
            "run.hint": "platform.command_handlers.run",
            "run.answer_decision": "platform.command_handlers.run",
            "run.add_context": "platform.command_handlers.run",
            "run.spawn_worker": "platform.command_handlers.run",
            "run.cancel_worker": "platform.command_handlers.run",
        },
        event_namespaces=["run.", "ctf.", "pentest."],
        # 与 apps/web/server.py 现有单题路由外形一致（兼容 Adapter，
        # 行为完全不变）；查询类能力 run.snapshot / graph.shared.read /
        # read_events / wait 经 Command API，不走 HTTP。
        api_routes=[
            "/api/runs",
            "/api/runs/{run_id}",
            "/api/runs/{run_id}/start",
            "/api/runs/{run_id}/events",
            "/api/runs/{run_id}/control",
            "/api/runs/{run_id}/control/{command_id}",
            "/api/runs/{run_id}/resolve",
            "/api/runs/{run_id}/workers",
            "/api/runs/{run_id}/archive",
            "/api/runs/{run_id}/purge",
            "/api/runs/{run_id}/uploads",
            "/api/runs/{run_id}/credentials",
            "/api/runs/{run_id}/btw",
            "/api/runs/{run_id}/hitl",
            "/api/runs/{run_id}/terminal",
        ],
        workspace_kind="single-security-task",
        ui_contributions={
            "title": "单题任务",
            "description": (
                "CTF / Pentest 单题求解工作区：创建表单（类型、目标、附件、"
                "期望 Flag 数）、Swarm Coordinator、SharedGraph、Review、"
                "Operator、Terminal 与 Worker 管理（现有 Conversation.tsx /"
                " RunInspector / WorkerOrchestration / SharedGraphPanel /"
                " Terminal 全部保留）"
            ),
            "icon": "flag",
            "route": "/task",
            "create_entry": "/task",
            "aggregate_type": "run",
        },
        artifact_types=["run.artifact", "worker.log", "flag.evidence"],
        graph_binding="ctf.shared_graph.v1",
        gate_binding="ctf.flag_gate",
    )


def single_security_task_components() -> list[ModuleComponent]:
    return [
        ModuleComponent(
            name="executor.swarm.coordinator",
            module_path="muteki.swarm.swarm",
            detail="标准 Swarm Coordinator（现有实现）",
        ),
        ModuleComponent(
            name="gate.ctf.flag_gate",
            module_path="muteki.solver.gate",
            detail="Flag 校验入口（现有实现）",
        ),
        ModuleComponent(
            name="graph.ctf.shared_graph",
            module_path="muteki.swarm.shared_graph",
            detail="单题共享图（现有实现，GraphService 适配见 muteki.graphs）",
        ),
        ModuleComponent(
            name="command_handlers.run",
            module_path="muteki.platform.command_handlers.run_handlers",
            detail="run.* 命令与 run.snapshot 查询 Handler（COMMAND-01 已注册）",
        ),
        ModuleComponent(
            name="command_handlers.task",
            module_path="muteki.platform.command_handlers.task_handlers",
            detail="task.create / task.get Handler（COMMAND-01 已注册）",
        ),
        ModuleComponent(
            name="capability.single_task",
            module_path="muteki.platform.capability_gateway",
            detail="single_task 能力工具与 graph.shared.read 只读视图（CAP-01 已注册）",
        ),
        ModuleComponent(
            name="adapter.single_task",
            module_path="muteki.single_task.adapter",
            detail="创建合同 / 兼容 Adapter / Handler 接线校验（SINGLE-01）",
        ),
        ModuleComponent(
            name="api.runs",
            module_path="apps.web.server",
            detail="现有 /api/runs 路由外形",
        ),
    ]


def competition_descriptor() -> DomainModuleDescriptor:
    """比赛工作区模块（任务书第 10 章）。

    CompetitionAdvisor 必须经 ExternalAgentAdapter 运行；组件的实时状态由
    WebPlatformStack 健康报告提供，本描述只声明产品能力和路由。
    """
    return DomainModuleDescriptor(
        id="builtin.competition",
        version="1.0.0",
        task_kinds=["competition"],
        required_capabilities=[],
        default_executor="competition.scheduler",
        command_handlers={
            "competition.sync": "competition.commands",
            "competition.challenge.queue": "competition.commands",
            "competition.submission.submit": "competition.commands",
            "competition.instance.lease": "competition.commands",
        },
        event_namespaces=["competition."],
        api_routes=["/api/competitions", "/api/competitions/{competition_id}"],
        workspace_kind="competition",
        ui_contributions={
            "title": "比赛",
            "description": "CTFd/rCTF/GZCTF 比赛工作区与自动调度",
            "icon": "trophy",
            "route": "/competitions",
            "create_entry": "/competitions",
            "aggregate_type": "competition",
        },
        artifact_types=["challenge.artifact", "submission.receipt", "scoreboard.snapshot"],
        # 比赛子题 Run 复用 ctf.shared_graph.v1（由单题模块持有）；比赛模块本身不建图
        graph_binding=None,
        gate_binding=None,
    )


def competition_components() -> list[ModuleComponent]:
    return [
        ModuleComponent(
            name="competition.store",
            module_path="muteki.competition.store",
            detail="比赛状态机与持久化（COMP-01 已落地）",
        ),
        ModuleComponent(
            name="competition.scheduler",
            module_path="muteki.competition.scheduler",
            detail="确定性调度器；长驻循环状态见 /api/health",
        ),
        ModuleComponent(
            name="competition.platforms",
            module_path="muteki.competition.platforms",
            detail="CTFd/rCTF/GZCTF/Generic Browser/本地模拟 Adapter 工厂",
        ),
        ModuleComponent(
            name="executor.external-agent.single",
            module_path="muteki.external_agents.base",
            detail="CompetitionAdvisor 使用统一结构化 Runtime 工厂",
        ),
    ]


def register_builtin_modules(
    registry: DomainModuleRegistry | None = None,
) -> DomainModuleRegistry:
    """在服务启动时注册全部内置模块；单个失败不影响其他模块。"""
    if registry is None:
        registry = default_registry()
    registry.register(
        conversation_descriptor(), components=conversation_components(),
    )
    registry.register(
        single_security_task_descriptor(), components=single_security_task_components(),
    )
    registry.register(
        competition_descriptor(), components=competition_components(),
    )
    return registry


def builtin_registrations(
    registry: DomainModuleRegistry,
) -> list[ModuleRegistration]:
    """按内置顺序返回三个内置模块的注册记录。"""
    return [
        registry.get(module_id)
        for module_id in (
            "builtin.conversation",
            "builtin.single-security-task",
            "builtin.competition",
        )
        if registry.get(module_id) is not None
    ]
