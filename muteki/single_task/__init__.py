"""单题 DomainModule 适配层（任务书 3.3、9.4，SINGLE-01）。

``builtin.single-security-task`` 的描述在 ``muteki/platform/builtin_modules.py``
（CORE-03）；本包提供该模块的运行时接线：

- ``SINGLE_TASK_TOOL_HANDLERS`` / ``verify_single_task_handlers``：single_task
  模式 ``muteki_*`` 能力工具与 Command/Query Handler 的一一映射及校验；
- ``build_start_body``：模块创建入口到旧 ``/api/runs/{run_id}/start`` 请求体的
  兼容 Adapter（创建合同不发送 ``swarm_class``）；
- ``make_graph_resolver`` / ``register_single_task_handlers``：把
  ``graph.shared.read`` 等只读查询 Handler 接到真实 Run 的 SharedGraph；
- ``create_task_and_run``：模块创建入口（task.create + run.create + run.start
  均经 Command API，与 Web 同一 receipt / 同一 RunManager 路径）。
"""

from muteki.single_task.adapter import (
    SINGLE_TASK_TOOL_HANDLERS,
    SingleTaskRunHandle,
    SingleTaskWiringError,
    ToolHandlerBinding,
    build_start_body,
    create_task_and_run,
    make_graph_resolver,
    register_single_task_handlers,
    verify_single_task_handlers,
)

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
