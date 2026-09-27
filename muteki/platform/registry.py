"""DomainModuleRegistry：DomainModule 与 workspace kind 注册（任务书 6.8、11.1，CORE-03）。

服务启动时加载内置模块（``builtin.conversation``、``builtin.single-security-task``、
``builtin.competition``），校验 id 唯一性、版本兼容、事件/命令命名空间冲突、
workspace kind 唯一性以及声明的 RunExecutor / Graph / Gate / UI 字段合法性。

注册失败的模块不进入 ready 状态：失败原因以统一错误 envelope
（``muteki.platform.contracts.errors.ErrorEnvelope``）记录在注册结果里，
不影响其他模块。Operator 可以 disable 模块；禁用只隐藏 workspace kind 入口，
模块记录与历史数据查询 API 保持可读。
"""

from __future__ import annotations

import importlib.util
import re
from enum import Enum
from typing import Any, Iterable, Mapping, Optional

from pydantic import Field

from .contracts.base import ContractModel
from .contracts.errors import ErrorCategory, ErrorEnvelope
from .contracts.events import BUILTIN_NAMESPACES, NS_EXT_PREFIX
from .contracts.modules import DomainModuleDescriptor

#: 当前平台支持的 DomainModule 描述主版本号。描述 version 的主版本号必须等于它，
#: 否则视为版本不兼容（次版本/补丁版本向后兼容）。
MODULE_API_MAJOR = 1

_SEMVER_RE = re.compile(r"^(\d+)\.(\d+)\.(\d+)$")
#: capability 需求写法："name"（任意版本）或 "name@N"（主版本精确匹配）。
_CAPABILITY_RE = re.compile(r"^([a-z0-9_.\-]+)(?:@(\d+))?$")


class ModuleState(str, Enum):
    """模块生命周期状态。

    - ``registered``：描述校验通过，但声明的依赖组件未全部就绪。
    - ``ready``：校验通过且依赖齐全，可以提供 workspace kind 入口。
    - ``unavailable``：注册校验失败，``error`` 中记录明确原因。
    - ``disabled``：Operator 禁用；入口隐藏，记录与历史数据仍可读。
    """

    REGISTERED = "registered"
    READY = "ready"
    UNAVAILABLE = "unavailable"
    DISABLED = "disabled"


class ComponentStatus(ContractModel):
    """模块声明的一个依赖组件的就绪状态（如实探测，不伪造 ready）。"""

    name: str = ""
    # 以 importlib 探测的 Python 模块路径，例如 muteki.swarm.swarm
    module_path: str = ""
    required: bool = True
    available: bool = False
    detail: str = ""


class ModuleRegistration(ContractModel):
    """一次模块注册的结果记录（含失败记录）。"""

    descriptor: DomainModuleDescriptor = Field(default_factory=DomainModuleDescriptor)
    state: ModuleState = ModuleState.REGISTERED
    # 注册失败原因；成功时为 None
    error: Optional[ErrorEnvelope] = None
    components: list[ComponentStatus] = Field(default_factory=list)
    # 校验通过但未 ready 时的待就绪组件名列表
    pending_components: list[str] = Field(default_factory=list)
    # 模块来源，例如 builtin
    origin: str = "builtin"


class WorkspaceKindInfo(ContractModel):
    """对外公开的 workspace kind 描述（由模块描述与 ui_contributions 派生）。"""

    id: str = ""
    module_id: str = ""
    title: str = ""
    description: str = ""
    icon: str = ""
    # 工作区路由，例如 /task、/chat、/competitions
    route: str = ""
    # 首页创建入口路由
    create_entry: str = ""
    # 后端聚合类型，例如 run / thread / competition
    aggregate_type: str = ""
    state: ModuleState = ModuleState.REGISTERED
    task_kinds: list[str] = Field(default_factory=list)


class ModuleComponent(ContractModel):
    """注册时随描述一起提交的依赖组件声明（见 builtin_modules.py）。"""

    name: str
    module_path: str = ""
    required: bool = True
    detail: str = ""


#: workspace kind 公开描述允许使用的 ui_contributions 键。
_UI_KEYS = frozenset({
    "title", "description", "icon", "route", "create_entry", "aggregate_type",
})


def _probe_module(module_path: str) -> bool:
    """探测 Python 模块是否真实存在（仅有 __pycache__ 的目录不算就绪）。

    不执行模块代码，只解析 spec；命名空间包（origin 为 None）视为未实现。
    """
    path = str(module_path or "").strip()
    if not path:
        return False
    try:
        spec = importlib.util.find_spec(path)
    except (ImportError, AttributeError, ValueError):
        return False
    if spec is None:
        return False
    return spec.origin is not None


def _error(code: str, message: str, category: ErrorCategory, **detail: Any) -> ErrorEnvelope:
    return ErrorEnvelope(code=code, message=message, category=category, detail=detail)


class DomainModuleRegistry:
    """DomainModule 注册表。

    构造时注入平台能力目录（capability 名 -> 主版本号）、已知 RunExecutor、
    Graph 绑定和 Gate 绑定；默认目录由 ``default_capability_catalog`` 等
    函数按当前代码库真实探测。
    """

    def __init__(
        self,
        *,
        capabilities: Optional[Mapping[str, int]] = None,
        executors: Optional[Mapping[str, str]] = None,
        graph_bindings: Optional[Mapping[str, str]] = None,
        gate_bindings: Optional[Mapping[str, str]] = None,
        module_api_major: int = MODULE_API_MAJOR,
    ) -> None:
        self._capabilities = dict(capabilities or {})
        self._executors = dict(executors or {})
        self._graph_bindings = dict(graph_bindings or {})
        self._gate_bindings = dict(gate_bindings or {})
        self._module_api_major = int(module_api_major)
        self._modules: dict[str, ModuleRegistration] = {}
        # id 冲突等无法按键归档的失败注册，仍保留记录供设置页展示
        self._failed: list[ModuleRegistration] = []

    # ------------------------------------------------------------------
    # 查询
    # ------------------------------------------------------------------

    def get(self, module_id: str) -> Optional[ModuleRegistration]:
        return self._modules.get(module_id)

    def list_modules(self, *, include_disabled: bool = True) -> list[ModuleRegistration]:
        """全部模块注册记录（含 unavailable）；历史数据查询不受 disable 影响。"""
        records = list(self._modules.values()) + list(self._failed)
        if include_disabled:
            return records
        return [r for r in records if r.state != ModuleState.DISABLED]

    def list_workspace_kinds(self) -> list[WorkspaceKindInfo]:
        """已注册 workspace kind 入口；禁用与校验失败的模块不返回入口。"""
        kinds: list[WorkspaceKindInfo] = []
        for record in self._modules.values():
            if record.state in (ModuleState.DISABLED, ModuleState.UNAVAILABLE):
                continue
            kinds.append(self._workspace_kind_of(record))
        return kinds

    def workspace_kind_ids(self) -> list[str]:
        return [kind.id for kind in self.list_workspace_kinds()]

    # ------------------------------------------------------------------
    # 注册
    # ------------------------------------------------------------------

    def register(
        self,
        descriptor: DomainModuleDescriptor,
        *,
        components: Iterable[ModuleComponent] = (),
        origin: str = "builtin",
    ) -> ModuleRegistration:
        """注册一个 DomainModule。

        校验失败不抛异常：返回 state=unavailable 且携带错误 envelope 的记录，
        其他模块不受影响。
        """
        record = ModuleRegistration(
            descriptor=descriptor, state=ModuleState.REGISTERED, origin=origin,
            components=[self._probe_component(c) for c in components],
        )
        error = self._validate(descriptor)
        if error is not None:
            record.state = ModuleState.UNAVAILABLE
            record.error = error
            # id 冲突时不能覆盖已有记录，归档到失败列表
            if error.code == "module.duplicate_id":
                self._failed.append(record)
            else:
                self._modules[descriptor.id] = record
            return record
        pending = [c.name for c in record.components if c.required and not c.available]
        record.pending_components = pending
        record.state = ModuleState.READY if not pending else ModuleState.REGISTERED
        self._modules[descriptor.id] = record
        return record

    def _probe_component(self, component: ModuleComponent) -> ComponentStatus:
        available = _probe_module(component.module_path) if component.module_path else False
        return ComponentStatus(
            name=component.name,
            module_path=component.module_path,
            required=component.required,
            available=available,
            detail=component.detail,
        )

    # ------------------------------------------------------------------
    # 校验
    # ------------------------------------------------------------------

    def _validate(self, descriptor: DomainModuleDescriptor) -> Optional[ErrorEnvelope]:
        """按任务书 11.1 校验模块描述；返回 None 表示通过。"""
        module_id = descriptor.id.strip()
        if not module_id:
            return _error(
                "module.invalid_id", "module id cannot be empty",
                ErrorCategory.VALIDATION,
            )
        existing = self._modules.get(module_id)
        if (existing is not None and existing.state != ModuleState.UNAVAILABLE) or any(
            r.descriptor.id == module_id for r in self._failed
        ):
            return _error(
                "module.duplicate_id",
                f"module id already registered: {module_id}",
                ErrorCategory.CONFLICT, module_id=module_id,
            )
        error = self._validate_version(descriptor)
        if error is not None:
            return error
        error = self._validate_capabilities(descriptor)
        if error is not None:
            return error
        error = self._validate_executor(descriptor)
        if error is not None:
            return error
        error = self._validate_namespaces(descriptor)
        if error is not None:
            return error
        error = self._validate_bindings(descriptor)
        if error is not None:
            return error
        return self._validate_workspace_kind(descriptor)

    def _validate_version(self, descriptor: DomainModuleDescriptor) -> Optional[ErrorEnvelope]:
        match = _SEMVER_RE.match(descriptor.version.strip())
        if not match:
            return _error(
                "module.invalid_version",
                f"module version must be semver X.Y.Z: {descriptor.version!r}",
                ErrorCategory.VALIDATION, module_id=descriptor.id,
                version=descriptor.version,
            )
        if int(match.group(1)) != self._module_api_major:
            return _error(
                "module.version_incompatible",
                f"module API major {match.group(1)} is not supported "
                f"(supported major: {self._module_api_major})",
                ErrorCategory.VALIDATION, module_id=descriptor.id,
                version=descriptor.version,
                supported_major=self._module_api_major,
            )
        return None

    def _validate_capabilities(self, descriptor: DomainModuleDescriptor) -> Optional[ErrorEnvelope]:
        for requirement in descriptor.required_capabilities:
            parsed = _CAPABILITY_RE.match(str(requirement).strip())
            if not parsed:
                return _error(
                    "module.invalid_capability",
                    f"invalid capability requirement: {requirement!r}",
                    ErrorCategory.VALIDATION, module_id=descriptor.id,
                    capability=requirement,
                )
            name, major = parsed.group(1), parsed.group(2)
            available = self._capabilities.get(name)
            if available is None or (major is not None and available != int(major)):
                return _error(
                    "module.missing_capability",
                    f"required capability is not available: {requirement}",
                    ErrorCategory.VALIDATION, module_id=descriptor.id,
                    capability=requirement,
                    available_capabilities=sorted(self._capabilities),
                )
        return None

    def _validate_executor(self, descriptor: DomainModuleDescriptor) -> Optional[ErrorEnvelope]:
        executor = descriptor.default_executor.strip()
        if not executor:
            return _error(
                "module.missing_executor",
                "default_executor cannot be empty",
                ErrorCategory.VALIDATION, module_id=descriptor.id,
            )
        if executor not in self._executors:
            return _error(
                "module.unknown_executor",
                f"unknown RunExecutor: {executor}",
                ErrorCategory.VALIDATION, module_id=descriptor.id,
                executor=executor, known_executors=sorted(self._executors),
            )
        return None

    def _validate_namespaces(self, descriptor: DomainModuleDescriptor) -> Optional[ErrorEnvelope]:
        # 事件命名空间：必须落在已知前缀内，且模块间不得重叠
        claimed_events: dict[str, str] = {}
        for record in self._modules.values():
            if record.state == ModuleState.UNAVAILABLE:
                continue
            for ns in record.descriptor.event_namespaces:
                claimed_events[ns] = record.descriptor.id
        for namespace in descriptor.event_namespaces:
            ns = str(namespace).strip()
            if not self._is_known_event_namespace(ns):
                return _error(
                    "module.invalid_event_namespace",
                    f"event namespace must use a known prefix "
                    f"(core./run./competition./ctf./pentest./ext.<id>.): {ns!r}",
                    ErrorCategory.VALIDATION, module_id=descriptor.id, namespace=ns,
                )
            owner = claimed_events.get(ns)
            if owner is not None:
                return _error(
                    "module.event_namespace_conflict",
                    f"event namespace {ns!r} already claimed by {owner}",
                    ErrorCategory.CONFLICT, module_id=descriptor.id,
                    namespace=ns, owner=owner,
                )
        # 命令命名空间：command_handlers 键的点分前缀模块间不得重叠
        claimed_commands: dict[str, str] = {}
        for record in self._modules.values():
            if record.state == ModuleState.UNAVAILABLE:
                continue
            for command_type in record.descriptor.command_handlers:
                claimed_commands[command_type.split(".", 1)[0]] = record.descriptor.id
        for command_type, handler in descriptor.command_handlers.items():
            text = str(command_type).strip()
            if "." not in text or not str(handler).strip():
                return _error(
                    "module.invalid_command_handler",
                    f"command handler key must be a namespaced command type with "
                    f"a non-empty handler: {command_type!r}",
                    ErrorCategory.VALIDATION, module_id=descriptor.id,
                    command_type=command_type,
                )
            namespace = text.split(".", 1)[0]
            owner = claimed_commands.get(namespace)
            if owner is not None:
                return _error(
                    "module.command_namespace_conflict",
                    f"command namespace {namespace!r} already claimed by {owner}",
                    ErrorCategory.CONFLICT, module_id=descriptor.id,
                    namespace=namespace, owner=owner,
                )
        return None

    @staticmethod
    def _is_known_event_namespace(namespace: str) -> bool:
        if namespace.startswith(NS_EXT_PREFIX):
            return True
        return any(namespace.startswith(prefix) for prefix in BUILTIN_NAMESPACES)

    def _validate_bindings(self, descriptor: DomainModuleDescriptor) -> Optional[ErrorEnvelope]:
        if descriptor.graph_binding is not None:
            binding = descriptor.graph_binding.strip()
            if not binding or binding not in self._graph_bindings:
                return _error(
                    "module.unknown_graph_binding",
                    f"unknown GraphService binding: {descriptor.graph_binding!r}",
                    ErrorCategory.VALIDATION, module_id=descriptor.id,
                    graph_binding=descriptor.graph_binding,
                    known_bindings=sorted(self._graph_bindings),
                )
        if descriptor.gate_binding is not None:
            binding = descriptor.gate_binding.strip()
            if not binding or binding not in self._gate_bindings:
                return _error(
                    "module.unknown_gate_binding",
                    f"unknown policy/gate binding: {descriptor.gate_binding!r}",
                    ErrorCategory.VALIDATION, module_id=descriptor.id,
                    gate_binding=descriptor.gate_binding,
                    known_bindings=sorted(self._gate_bindings),
                )
        return None

    def _validate_workspace_kind(self, descriptor: DomainModuleDescriptor) -> Optional[ErrorEnvelope]:
        kind = descriptor.workspace_kind.strip()
        if not kind:
            return _error(
                "module.missing_workspace_kind",
                "workspace_kind cannot be empty",
                ErrorCategory.VALIDATION, module_id=descriptor.id,
            )
        for record in self._modules.values():
            if record.state == ModuleState.UNAVAILABLE:
                continue
            if record.descriptor.workspace_kind == kind:
                return _error(
                    "module.workspace_kind_conflict",
                    f"workspace kind {kind!r} already claimed by {record.descriptor.id}",
                    ErrorCategory.CONFLICT, module_id=descriptor.id,
                    workspace_kind=kind, owner=record.descriptor.id,
                )
        for route in descriptor.api_routes:
            text = str(route).strip()
            if not text.startswith("/api/"):
                return _error(
                    "module.invalid_api_route",
                    f"api route must start with /api/: {route!r}",
                    ErrorCategory.VALIDATION, module_id=descriptor.id, route=route,
                )
        unknown_keys = sorted(set(descriptor.ui_contributions) - _UI_KEYS)
        if unknown_keys:
            return _error(
                "module.invalid_ui_contribution",
                f"unknown ui_contributions keys: {', '.join(unknown_keys)}",
                ErrorCategory.VALIDATION, module_id=descriptor.id,
                unknown_keys=unknown_keys, known_keys=sorted(_UI_KEYS),
            )
        # workspace kind 入口必须能渲染：标题、路由与创建入口齐全
        ui = descriptor.ui_contributions
        missing = [key for key in ("title", "route", "create_entry") if not str(ui.get(key) or "").strip()]
        if missing:
            return _error(
                "module.invalid_ui_contribution",
                f"ui_contributions missing required keys: {', '.join(missing)}",
                ErrorCategory.VALIDATION, module_id=descriptor.id,
                missing_keys=missing,
            )
        return None

    # ------------------------------------------------------------------
    # 启用 / 禁用
    # ------------------------------------------------------------------

    def disable(self, module_id: str) -> ModuleRegistration:
        """禁用模块：workspace kind 入口隐藏，记录与历史数据 API 保持可读。"""
        record = self._require(module_id)
        if record.state == ModuleState.UNAVAILABLE:
            return record
        record.state = ModuleState.DISABLED
        return record

    def enable(self, module_id: str) -> ModuleRegistration:
        """重新启用模块：按当前依赖探测结果恢复 ready / registered。"""
        record = self._require(module_id)
        if record.state != ModuleState.DISABLED:
            return record
        record.state = ModuleState.READY if not record.pending_components else ModuleState.REGISTERED
        return record

    def unregister(self, module_id: str) -> Optional[ModuleRegistration]:
        """注销动态模块；内置模块的调用方应继续使用 disable。"""
        return self._modules.pop(module_id, None)

    def _require(self, module_id: str) -> ModuleRegistration:
        record = self._modules.get(module_id)
        if record is None:
            raise KeyError(f"unknown module: {module_id}")
        return record

    # ------------------------------------------------------------------
    # workspace kind 派生
    # ------------------------------------------------------------------

    @staticmethod
    def _workspace_kind_of(record: ModuleRegistration) -> WorkspaceKindInfo:
        descriptor = record.descriptor
        ui = descriptor.ui_contributions
        return WorkspaceKindInfo(
            id=descriptor.workspace_kind,
            module_id=descriptor.id,
            title=str(ui.get("title") or descriptor.workspace_kind),
            description=str(ui.get("description") or ""),
            icon=str(ui.get("icon") or ""),
            route=str(ui.get("route") or ""),
            create_entry=str(ui.get("create_entry") or ""),
            aggregate_type=str(ui.get("aggregate_type") or ""),
            state=record.state,
            task_kinds=list(descriptor.task_kinds),
        )


# ----------------------------------------------------------------------
# 默认目录：按当前代码库真实探测，探测不到的 capability / 组件如实缺席
# ----------------------------------------------------------------------

def default_capability_catalog() -> dict[str, int]:
    """平台当前真实提供的 capability 目录（名字 -> 主版本号）。

    条目只有在对应实现真实存在时才出现；CONV-01 / COMP-01 等工作包落地后
    在此补充对应条目。
    """
    catalog: dict[str, int] = {}
    if _probe_module("muteki.solver.gate"):
        catalog["run.flag_gate"] = 1
    if _probe_module("muteki.swarm.shared_graph"):
        catalog["graph.shared"] = 1
    if _probe_module("muteki.swarm.swarm"):
        catalog["run.coordinator"] = 1
    return catalog


def default_executor_catalog() -> dict[str, str]:
    """已知 RunExecutor id -> 实现模块路径（任务书 6.4）。"""
    return {
        "swarm.coordinator": "muteki.swarm.swarm",
        "external-agent.single": "muteki.external_agents.base",
        "competition.scheduler": "muteki.competition.scheduler",
    }


def default_graph_bindings() -> dict[str, str]:
    """已知 GraphService 绑定 id -> 实现模块路径。"""
    return {"ctf.shared_graph.v1": "muteki.swarm.shared_graph"}


def default_gate_bindings() -> dict[str, str]:
    """已知结果门禁绑定 id -> 实现模块路径。"""
    return {"ctf.flag_gate": "muteki.solver.gate"}


def default_registry() -> DomainModuleRegistry:
    """构造带默认目录的注册表（不注册任何模块）。"""
    return DomainModuleRegistry(
        capabilities=default_capability_catalog(),
        executors=default_executor_catalog(),
        graph_bindings=default_graph_bindings(),
        gate_bindings=default_gate_bindings(),
    )
