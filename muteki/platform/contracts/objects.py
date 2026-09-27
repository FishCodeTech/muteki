"""通用产品对象契约（任务书 5.1）。

Project / Workspace / Thread / Task / RunRef / ExecutionGeneration /
AgentSession / Artifact / ResourceLease。这里只冻结字段与身份键，
持久化（platform.db 等）由 CORE-02 实现。
"""

from __future__ import annotations

from datetime import datetime
from typing import Any, Optional

from pydantic import Field

from .base import ContractModel, new_id, utcnow


class Project(ContractModel):
    """相关 Thread、Task、Workspace、设置和知识的长期作用域。"""

    project_id: str = Field(default_factory=lambda: new_id("proj"))
    name: str = ""
    description: str = ""
    settings: dict[str, Any] = Field(default_factory=dict)
    created_at: datetime = Field(default_factory=utcnow)
    updated_at: datetime = Field(default_factory=utcnow)


class Workspace(ContractModel):
    """文件、命令、容器和 Artifact 的执行边界。"""

    workspace_id: str = Field(default_factory=lambda: new_id("ws"))
    project_id: Optional[str] = None
    # 执行环境形态，例如 local / container / git / isolated
    kind: str = "local"
    root_path: str = ""
    # Thread 工作区模式与 worktree 元数据（base_ref / branch / worktree_of 等）。
    settings: dict[str, Any] = Field(default_factory=dict)
    created_at: datetime = Field(default_factory=utcnow)


class Thread(ContractModel):
    """用户与 Muteki 的长期交互流。"""

    thread_id: str = Field(default_factory=lambda: new_id("thr"))
    project_id: Optional[str] = None
    workspace_id: Optional[str] = None
    title: str = ""
    # 标题来源：fallback（创建时占位）| model（模型生成）| user（用户改名）。
    # 后台标题任务只允许覆盖 fallback，用户改名始终拥有最高优先级。
    title_source: str = "user"
    # 面向列表与置顶栏的一句话会话摘要。仅保存模型明确返回的摘要；
    # Runtime 原始推理、工具输出和隐藏思考不会进入该字段。
    summary: str = ""
    # 异步元数据生成的乐观并发版本与依据回合。每次生成请求或用户改名
    # 都递增 revision，用于丢弃迟到的模型响应。
    metadata_revision: int = 0
    metadata_turn_id: Optional[str] = None
    # 对话模式：conversation | single_task | competition | management
    mode: str = "conversation"
    created_at: datetime = Field(default_factory=utcnow)
    updated_at: datetime = Field(default_factory=utcnow)


class Task(ContractModel):
    """需要完成的目标和领域输入。revision 随输入变化递增。"""

    task_id: str = Field(default_factory=lambda: new_id("task"))
    thread_id: Optional[str] = None
    project_id: Optional[str] = None
    # 领域任务种类，例如 ctf.challenge / pentest.target / conversation.message
    kind: str = ""
    title: str = ""
    input: dict[str, Any] = Field(default_factory=dict)
    revision: int = 1
    created_at: datetime = Field(default_factory=utcnow)


class RunRef(ContractModel):
    """Run 的稳定引用。Run 是 Task 的稳定工作单元，身份不随重试变化。"""

    run_id: str
    task_id: Optional[str] = None
    thread_id: Optional[str] = None
    # 实际执行器 id，例如 swarm.coordinator / external-agent.single
    executor_id: Optional[str] = None
    # ensure_bound_run 的幂等键；相同 key 永远返回同一 Run
    binding_key: Optional[str] = None
    created_at: datetime = Field(default_factory=utcnow)


class ExecutionGeneration(ContractModel):
    """Run 内一次实际执行尝试，身份为 (run_id, generation)。"""

    run_id: str
    generation: int
    executor_id: Optional[str] = None
    agent_session_ids: list[str] = Field(default_factory=list)
    started_at: datetime = Field(default_factory=utcnow)
    ended_at: Optional[datetime] = None


class AgentSession(ContractModel):
    """一个外部 Runtime 的可恢复会话映射。"""

    agent_session_id: str = Field(default_factory=lambda: new_id("asess"))
    # 外部 Runtime 自己的 session id，启动成功后回填
    external_session_id: Optional[str] = None
    adapter_id: str = ""
    runtime_instance_id: Optional[str] = None
    thread_id: Optional[str] = None
    run_id: Optional[str] = None
    execution_generation: Optional[int] = None
    # 可恢复句柄（如 transcript 路径、resume token 的引用），不含凭据本体
    resume_handle: Optional[str] = None
    created_at: datetime = Field(default_factory=utcnow)
    closed_at: Optional[datetime] = None


class Artifact(ContractModel):
    """内容寻址的不可变输入或输出，身份为 sha256。"""

    sha256: str
    name: str = ""
    # Artifact 种类，例如 challenge.file / run.output / report
    kind: str = ""
    media_type: str = ""
    size: int = 0
    run_id: Optional[str] = None
    created_at: datetime = Field(default_factory=utcnow)


class ResourceLease(ContractModel):
    """带 owner、scope、TTL 和 fencing token 的资源占用。

    身份为 (resource_kind, resource_key)。fencing token 单调递增，
    持有方写共享资源时必须携带，防止过期 lease 的写穿透。
    """

    resource_kind: str
    resource_key: str
    owner: str = ""
    scope: str = ""
    ttl_seconds: int = 0
    fencing_token: int = 0
    acquired_at: datetime = Field(default_factory=utcnow)
    expires_at: Optional[datetime] = None
    released_at: Optional[datetime] = None
