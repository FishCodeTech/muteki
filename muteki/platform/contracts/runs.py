"""RunGateway / RunExecutor 相关契约（任务书 6.3、6.4、8.1）。

Run 生命周期与事件的历史存储（SessionStore JSONL、``muteki.core.events``）
保持不变；这里的模型是平台层与领域模块之间的新契约。
"""

from __future__ import annotations

from datetime import datetime
from typing import Any, Optional

from pydantic import Field

from .base import ContractModel, new_id, utcnow


class BoundRunRequest(ContractModel):
    """``RunGateway.ensure_bound_run`` 的幂等请求。

    - 相同 ``binding_key`` 返回同一 Run；
    - 相同 ``binding_key`` 对应不同 ``task_revision`` 时返回冲突；
    - 调用方可以通过 ``run_id`` 预先确定 Run 身份；
    - 重试不会生成第二个 workspace、SessionStore 或 SharedGraph。
    """

    binding_key: str
    task_id: Optional[str] = None
    task_kind: str = ""
    task_revision: int = 1
    run_id: Optional[str] = None
    executor_id: Optional[str] = None
    payload: dict[str, Any] = Field(default_factory=dict)


class RunCommand(ContractModel):
    """下发给单个 Run 的控制命令（start/pause/resume/resolve/stop 等）。

    Run 控制 journal 与接收/效果回执继续复用 ``muteki.control``，
    这里只是经 RunGateway 进入时的统一信封。
    """

    command_type: str
    command_id: str = Field(default_factory=lambda: new_id("cmd"))
    actor_id: str = ""
    expected_generation: Optional[int] = None
    payload: dict[str, Any] = Field(default_factory=dict)


class RunEvent(ContractModel):
    """经 RunGateway 暴露的公开 Run 事件（由现有 Run Event 适配而来）。

    不替代 ``muteki.core.events.Event`` 的持久格式，只是领域模块读取
    Run 进度时使用的完整视图。
    """

    run_id: str
    seq: int = 0
    event_type: str = ""
    occurred_at: datetime = Field(default_factory=utcnow)
    payload: dict[str, Any] = Field(default_factory=dict)


class RunSnapshot(ContractModel):
    """Run 当前状态快照，用于查询与恢复展示。"""

    run_id: str
    state: str = ""
    generation: int = 0
    task_kind: str = ""
    executor_id: Optional[str] = None
    created_at: Optional[datetime] = None
    updated_at: Optional[datetime] = None
    detail: dict[str, Any] = Field(default_factory=dict)


class RunExecutionContext(ContractModel):
    """交给 ``RunExecutor.execute`` 的执行上下文。"""

    run_id: str
    task_kind: str = ""
    generation: int = 1
    task_id: Optional[str] = None
    workspace_id: Optional[str] = None
    execution_binding_id: Optional[str] = None
    payload: dict[str, Any] = Field(default_factory=dict)


class RunResult(ContractModel):
    """``RunExecutor.execute`` 的终态结果。"""

    run_id: str
    # succeeded | failed | stopped
    state: str = ""
    detail: dict[str, Any] = Field(default_factory=dict)
