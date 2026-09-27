"""DomainModule 与 PlatformAdapter 的契约模型（任务书 6.8、6.10）。

DomainModule 声明自身的任务种类、能力需求、默认执行器、命令 Handler、
事件命名空间、API 路由、workspace kind、UI 贡献、Artifact 类型和图/门禁绑定。
PlatformAdapter 的传输模型在此冻结字段；CTFd/rCTF/GZCTF 等具体实现
由 COMP 工作包落地。
"""

from __future__ import annotations

from datetime import datetime
from typing import Any, Optional

from pydantic import Field

from .base import ContractModel, new_id, utcnow


class DomainModuleDescriptor(ContractModel):
    """领域模块声明（任务书 6.8 字段清单）。"""

    id: str = ""
    version: str = ""
    task_kinds: list[str] = Field(default_factory=list)
    required_capabilities: list[str] = Field(default_factory=list)
    # 默认 RunExecutor id，例如 swarm.coordinator
    default_executor: str = ""
    # command_type -> handler 标识
    command_handlers: dict[str, str] = Field(default_factory=dict)
    event_namespaces: list[str] = Field(default_factory=list)
    api_routes: list[str] = Field(default_factory=list)
    # workspace kind 注册键，例如 conversation / single_task / competition
    workspace_kind: str = ""
    ui_contributions: dict[str, Any] = Field(default_factory=dict)
    artifact_types: list[str] = Field(default_factory=list)
    # GraphService 绑定，例如 ctf.shared_graph.v1；None 表示不建图
    graph_binding: Optional[str] = None
    # 结果门禁绑定，例如 ctf.flag_gate
    gate_binding: Optional[str] = None


class PlatformConnectionRef(ContractModel):
    """一个比赛平台连接的引用；真实凭据在 SecretStore，不进入本模型。"""

    connection_id: str = Field(default_factory=lambda: new_id("pconn"))
    # ctfd / rctf / gzctf / generic_browser
    platform_kind: str = ""
    endpoint: str = ""
    competition_id: Optional[str] = None


class PlatformCapabilities(ContractModel):
    """平台能力探测结果。"""

    platform_kind: str = ""
    sync: bool = False
    artifacts: bool = False
    dynamic_instances: bool = False
    submit: bool = False
    scoreboard: bool = False
    detail: dict[str, Any] = Field(default_factory=dict)


class SyncRequest(ContractModel):
    connection_id: str = ""
    competition_id: Optional[str] = None
    # 增量同步游标；None 表示全量
    cursor: Optional[str] = None


class SyncResult(ContractModel):
    connection_id: str = ""
    cursor: Optional[str] = None
    synced_challenges: int = 0
    detail: dict[str, Any] = Field(default_factory=dict)


class RemoteArtifactRef(ContractModel):
    """远端题目附件引用。"""

    connection_id: str = ""
    remote_id: str = ""
    url: str = ""
    sha256: Optional[str] = None


class ArtifactObject(ContractModel):
    """抓取回来的 Artifact 内容（content 为原始字节）。"""

    sha256: str = ""
    media_type: str = ""
    content: bytes = b""


class PlatformChallengeRef(ContractModel):
    connection_id: str = ""
    challenge_key: str = ""
    revision: int = 1


class InstanceLeaseRef(ContractModel):
    """动态实例租约引用。"""

    lease_id: str = Field(default_factory=lambda: new_id("ilease"))
    connection_id: str = ""
    challenge_key: str = ""
    fencing_token: int = 0
    expires_at: Optional[datetime] = None


class InstanceResult(ContractModel):
    lease: InstanceLeaseRef = Field(default_factory=InstanceLeaseRef)
    # 实例访问入口，例如 {"http": "http://host:port"}
    endpoints: dict[str, str] = Field(default_factory=dict)
    acquired_at: datetime = Field(default_factory=utcnow)


class SubmissionRequest(ContractModel):
    connection_id: str = ""
    challenge_key: str = ""
    flag: Optional[str] = None
    artifact_sha256: Optional[str] = None
    idempotency_key: Optional[str] = None
    payload: dict[str, Any] = Field(default_factory=dict)


class SubmissionResult(ContractModel):
    submission_id: Optional[str] = None
    # correct | incorrect | pending | unknown
    status: str = "pending"
    detail: dict[str, Any] = Field(default_factory=dict)
