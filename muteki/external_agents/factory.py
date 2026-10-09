"""Runtime Adapter 的唯一产品工厂。

Web Conversation、Runtime 设置、标准 Swarm 和 WorkerSessionSupervisor 都通过
本模块解释 ``adapter_id + instance_id``。Conversation 默认使用可双向交互的
结构化 Runtime；Worker 继续显式使用 ``cli.<engine>``。
"""

from __future__ import annotations

from dataclasses import dataclass, field, replace
from pathlib import Path
from typing import Any, Callable, Iterable, Mapping, Optional

from muteki.solver.engine_registry import (
    SUPPORTED_ENGINE_IDS,
    EngineTemporarilyUnsupportedError,
)

from .base import AdapterIdentity, BaseExternalAgentAdapter
from .registry import AdapterRegistry


STRUCTURED_ADAPTER_ENGINES: dict[str, str] = {
    "codex.app_server": "codex",
    "claude.sdk": "claude",
    "cursor.sdk": "cursor",
    "grok.acp": "grok",
    "pi.rpc": "pi",
    "opencode.server": "opencode",
    "kimi.acp": "kimi",
    "omp.acp": "omp",
    "devin.acp": "devin",
    "droid.rpc": "droid",
}

HISTORICAL_ADAPTER_ENGINES: dict[str, str] = {
    "deepseek.harness": "dsh",
}

# 已发布过的内部 ID 继续可读；新配置和 UI 一律返回左侧产品 ID。
LEGACY_ADAPTER_ALIASES: dict[str, str] = {
    "claude.agent_sdk": "claude.sdk",
    "dsh.sdk": "deepseek.harness",
    "omp.rpc": "omp.rpc_v2",
}

OPTIONAL_STRUCTURED_ADAPTER_ENGINES: dict[str, str] = {
    "kimi.local_server": "kimi",
    "pi.acp": "pi",
    "omp.rpc_v2": "omp",
    "cursor.acp": "cursor",
    "droid.acp": "droid",
}

DEFAULT_STRUCTURED_ADAPTER_BY_ENGINE: dict[str, str] = {
    engine: adapter_id for adapter_id, engine in STRUCTURED_ADAPTER_ENGINES.items()
}

# Worker 继续统一使用 CLI；Conversation 通过下方结构化默认值接收原生
# approval/user-input 反向请求。两套默认值必须保持独立。
DEFAULT_CLI_ADAPTER_BY_ENGINE: dict[str, str] = {
    engine: f"cli.{engine}"
    for engine in SUPPORTED_ENGINE_IDS
}
DEFAULT_ADAPTER_BY_ENGINE = dict(DEFAULT_STRUCTURED_ADAPTER_BY_ENGINE)


def canonical_adapter_id(adapter_id: str) -> str:
    """返回产品稳定 Adapter ID；bare engine 解析到 Conversation 默认 transport。"""
    value = str(adapter_id or "").strip()
    if value in LEGACY_ADAPTER_ALIASES:
        return LEGACY_ADAPTER_ALIASES[value]
    if value in DEFAULT_ADAPTER_BY_ENGINE:
        return DEFAULT_ADAPTER_BY_ENGINE[value]
    return value


def engine_for_adapter(adapter_id: str) -> str:
    """Adapter ID 对应的 Worker 基础引擎；无法识别时返回空串。"""
    value = canonical_adapter_id(adapter_id)
    if value.startswith("cli."):
        return value.removeprefix("cli.")
    return (
        STRUCTURED_ADAPTER_ENGINES.get(value)
        or HISTORICAL_ADAPTER_ENGINES.get(value)
        or OPTIONAL_STRUCTURED_ADAPTER_ENGINES.get(value)
        or ""
    )


MAX_LAUNCH_ARGS = 64
MAX_LAUNCH_ARG_LENGTH = 4096


def normalize_launch_args(value: Any, adapter_id: str) -> tuple[str, ...]:
    """Validate user CLI arguments for one Runtime instance (raises ValueError)."""
    if value in (None, "", [], ()):
        return ()
    if not isinstance(value, (list, tuple)):
        raise ValueError("launch_args 必须是字符串数组")
    adapter_id = canonical_adapter_id(adapter_id)
    if adapter_id.startswith("cli."):
        raise ValueError("CLI compatibility transport 不支持 launch_args")
    if len(value) > MAX_LAUNCH_ARGS:
        raise ValueError(f"launch_args 最多 {MAX_LAUNCH_ARGS} 项，当前 {len(value)} 项")
    out: list[str] = []
    for index, item in enumerate(value):
        if not isinstance(item, str):
            raise ValueError(f"launch_args[{index}] 必须是字符串")
        if not item.strip():
            raise ValueError(f"launch_args[{index}] 不能为空")
        if "\0" in item or "\n" in item:
            raise ValueError(f"launch_args[{index}] 不能包含换行或 NUL")
        if len(item) > MAX_LAUNCH_ARG_LENGTH:
            raise ValueError(f"launch_args[{index}] 超过 {MAX_LAUNCH_ARG_LENGTH} 字符")
        out.append(item)
    if adapter_id == "claude.sdk":
        from .claude import claude_extra_args

        claude_extra_args(out)
    return tuple(out)


def runtime_config_schema(adapter_id: str) -> dict[str, Any]:
    """返回设置页可直接生成表单的 Runtime 配置 schema。"""
    adapter_id = canonical_adapter_id(adapter_id)
    engine = engine_for_adapter(adapter_id)
    properties: dict[str, Any] = {
        "instance_id": {
            "type": "string", "title": "实例 ID", "default": "default",
        },
        "label": {"type": "string", "title": "显示名称"},
        "binary_path": {"type": "string", "title": "可执行文件"},
        "enabled": {"type": "boolean", "title": "启用", "default": True},
    }
    if not adapter_id.startswith("cli."):
        properties["launch_args"] = {
            "type": "array", "title": "启动参数", "items": {"type": "string"},
            "maxItems": MAX_LAUNCH_ARGS,
        }
    if adapter_id in {"opencode.server", "kimi.local_server"}:
        properties["adapter_endpoint"] = {
            "type": "string", "title": "Adapter 服务地址", "format": "uri",
        }
    transport_properties: dict[str, Any] = {}
    if adapter_id == "codex.app_server":
        transport_properties = {
            "experimental_api": {"type": "boolean", "default": False},
        }
    elif adapter_id == "claude.sdk":
        transport_properties = {
            "permission_mode": {
                "type": "string", "title": "权限模式", "default": "default",
            },
        }
    elif adapter_id == "grok.acp":
        transport_properties = {
            "mcp_ready_wait_seconds": {
                "type": "number",
                "title": "HTTP MCP 启动等待（秒）",
                "default": 3.0,
                "minimum": 0,
                "maximum": 30,
            },
        }
    elif adapter_id in {"pi.rpc", "omp.rpc_v2"}:
        transport_properties = {
            "session_dir": {"type": "string", "title": "会话目录"},
        }
    if transport_properties:
        properties["transport"] = {
            "type": "object",
            "title": "Transport 配置",
            "properties": transport_properties,
            "additionalProperties": False,
        }
    return {
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": f"{adapter_id} Runtime 配置",
        "type": "object",
        "required": ["instance_id"],
        "properties": properties,
        "additionalProperties": False,
        "x-muteki-adapter-id": adapter_id,
        "x-muteki-engine": engine,
        "x-muteki-transport": (
            "cli" if adapter_id.startswith("cli.") else "structured"
        ),
    }


@dataclass
class RuntimeAdapterConfig:
    adapter_id: str
    instance_id: str = "default"
    binary_path: str = ""
    endpoint: str = ""
    credential_ref: str = ""
    env_refs: dict[str, str] = field(default_factory=dict)
    default_model: str = ""
    enabled: bool = True
    transport: dict[str, Any] = field(default_factory=dict)
    launch_args: tuple[str, ...] = ()

    @classmethod
    def from_value(cls, value: Any) -> "RuntimeAdapterConfig":
        def read(name: str, default: Any = "") -> Any:
            if isinstance(value, Mapping):
                return value.get(name, default)
            return getattr(value, name, default)

        return cls(
            adapter_id=canonical_adapter_id(str(read("adapter_id") or "")),
            instance_id=str(read("instance_id", "default") or "default"),
            binary_path=str(read("binary_path") or ""),
            endpoint=str(read("adapter_endpoint") or read("endpoint") or ""),
            credential_ref=str(read("credential_ref") or ""),
            env_refs={
                str(key): str(item)
                for key, item in dict(read("env_refs", {}) or {}).items()
            },
            default_model=str(read("default_model") or ""),
            enabled=bool(read("enabled", True)),
            transport=dict(read("transport", {}) or {}),
            launch_args=tuple(str(item) for item in (read("launch_args", ()) or ())),
        )


class RuntimeAdapterFactory:
    """构造和注册产品 Runtime Adapter 的唯一入口。"""

    def __init__(
        self,
        *,
        store: Any = None,
        binding_service: Any = None,
        gateway_endpoint: str = "",
        descriptor_provider: Any = None,
        gateway: Any = None,
        sessions_root: str | Path = "state",
        env_ref_resolver: Optional[Callable[[Mapping[str, str]], dict[str, str]]] = None,
        cli_builder: Optional[Callable[[RuntimeAdapterConfig, dict[str, Any]], Any]] = None,
        probe_environment_factory: Optional[Callable[[str, str, dict[str, str]], dict[str, str]]] = None,
    ) -> None:
        self.store = store
        self.binding_service = binding_service
        self.gateway_endpoint = gateway_endpoint
        self.descriptor_provider = descriptor_provider
        self.gateway = gateway
        self.sessions_root = Path(sessions_root)
        self.env_ref_resolver = env_ref_resolver
        # Dependency inversion: external_agents 是基础层，不能反向导入 solver。
        # Web/WorkerSession 在装配处注入 CLI compatibility builder；结构化
        # Runtime 构造本身不需要 solver 层存在。
        self.cli_builder = cli_builder
        self.probe_environment_factory = probe_environment_factory

    @property
    def supported_adapter_ids(self) -> tuple[str, ...]:
        cli = tuple(f"cli.{engine}" for engine in sorted(
            SUPPORTED_ENGINE_IDS))
        return tuple(STRUCTURED_ADAPTER_ENGINES) + tuple(
            OPTIONAL_STRUCTURED_ADAPTER_ENGINES) + cli

    def _common(self, config: RuntimeAdapterConfig) -> dict[str, Any]:
        return {
            "instance_id": config.instance_id,
            "store": self.store,
            "binding_service": self.binding_service,
            "gateway_endpoint": self.gateway_endpoint,
            "descriptor_provider": self.descriptor_provider,
        }

    def _env(self, config: RuntimeAdapterConfig) -> dict[str, str]:
        resolved: dict[str, str] = {}
        if self.env_ref_resolver is not None:
            resolved.update(self.env_ref_resolver(config.env_refs))
        return resolved

    @staticmethod
    def _retag(adapter: Any, adapter_id: str, instance_id: str) -> Any:
        """兼容已有实现类的旧内部 ID，同时对外只暴露产品稳定 ID。"""
        adapter.id = adapter_id
        adapter.identity = AdapterIdentity(adapter_id, instance_id)
        return adapter

    def create(self, value: RuntimeAdapterConfig | Mapping[str, Any] | Any) -> BaseExternalAgentAdapter:
        config = value if isinstance(value, RuntimeAdapterConfig) else RuntimeAdapterConfig.from_value(value)
        adapter = self._create(config)
        if not isinstance(adapter, BaseExternalAgentAdapter):
            raise TypeError(
                f"runtime builder for {config.adapter_id!r} returned "
                f"{type(adapter).__name__}, not a BaseExternalAgentAdapter")
        if self.gateway is not None and self.descriptor_provider is None:
            adapter.bind_capability_gateway(self.gateway)
        if config.launch_args and not config.adapter_id.startswith("cli."):
            adapter.launch_args = normalize_launch_args(config.launch_args, config.adapter_id)
        if self.probe_environment_factory is not None:
            adapter.probe_environment_factory = lambda: self.probe_environment_factory(
                engine_for_adapter(config.adapter_id), f"probe:{config.instance_id}", self._env(config))
        return adapter

    def _create(self, value: RuntimeAdapterConfig | Mapping[str, Any] | Any) -> Any:
        config = (
            value if isinstance(value, RuntimeAdapterConfig)
            else RuntimeAdapterConfig.from_value(value)
        )
        adapter_id = canonical_adapter_id(config.adapter_id)
        engine = engine_for_adapter(adapter_id)
        if not engine:
            raise ValueError(f"unknown Runtime adapter: {config.adapter_id!r}")
        if engine == "dsh":
            raise EngineTemporarilyUnsupportedError("dsh")
        common = self._common(config)
        binary = config.binary_path or None
        transport = config.transport
        env = self._env(config)

        if adapter_id.startswith("cli."):
            if self.cli_builder is None:
                raise ValueError(
                    "CLI compatibility builder was not supplied by the product layer")
            # Older Runtime rows used these fields as model-service defaults.
            # Do not expose them even to a product-supplied compatibility builder;
            # Worker/Thread launch selections are the only identity/model source.
            return self.cli_builder(replace(
                config,
                endpoint="",
                credential_ref="",
                default_model="",
            ), common)

        from muteki import external_agents as ea

        # Health discovery and structured chat must select the same installation.
        # Bridge executables with their own resolution remain adapter-owned.
        if binary is None and adapter_id not in {"cursor.sdk", "cursor.acp", "pi.acp"}:
            from muteki.solver.cli_engines.bins import resolve_engine_bin

            binary = resolve_engine_bin(engine)

        if adapter_id == "devin.acp":
            from .devin import DevinAcpAdapter

            # Conversation-only scope is enforced by the adapter. It still
            # needs the same session-owned gateway bindings as other chats.
            return DevinAcpAdapter(
                binary=binary, env_extra=env, **common,
            )
        if adapter_id == "codex.app_server":
            adapter = ea.CodexAppServerAdapter(
                binary=binary,
                default_env=env,
                experimental_api=bool(transport.get("experimental_api", False)),
                **common,
            )
        elif adapter_id == "claude.sdk":
            adapter = ea.ClaudeSDKAdapter(
                cli_path=binary or "claude",
                default_env=env,
                permission_mode=str(transport.get("permission_mode") or "default"),
                gateway=self.gateway,
                **common,
            )
        elif adapter_id == "cursor.sdk":
            adapter = ea.CursorSdkAdapter(
                runtime_root=self.sessions_root / "_cursor_sdk_runtime",
                env_extra=env,
                **common,
            )
        elif adapter_id == "cursor.acp":
            adapter = ea.CursorAcpAdapter(binary=binary, env_extra=env, **common)
        elif adapter_id == "grok.acp":
            adapter = ea.GrokAcpAdapter(
                binary=binary,
                runtime_root=self.sessions_root / "_grok_acp_runtime",
                mcp_ready_wait_seconds=transport.get(
                    "mcp_ready_wait_seconds"),
                env_extra=env,
                **common,
            )
        elif adapter_id == "pi.acp":
            adapter = ea.PiAcpAdapter(
                binary=binary,
                runtime_root=self.sessions_root / "_pi_acp_runtime",
                env_extra=env,
                **common,
            )
        elif adapter_id == "pi.rpc":
            adapter = ea.PiAdapter(
                binary=binary,
                session_dir=str(transport.get("session_dir") or "") or None,
                runtime_root=self.sessions_root / "_pi_runtime",
                default_env=env,
                **common,
            )
        elif adapter_id == "opencode.server":
            adapter = ea.OpenCodeServerAdapter(
                binary=binary, base_url=config.endpoint or None,
                manage_server=not bool(config.endpoint), extra_env=env,
                log_root=self.sessions_root / "_logs" / "external_agents",
                **common,
            )
        elif adapter_id == "kimi.acp":
            adapter = ea.KimiAcpAdapter(
                binary=binary,
                env_extra=env,
                **common,
            )
        elif adapter_id == "kimi.local_server":
            adapter = ea.KimiLocalServerAdapter(
                binary=binary, base_url=config.endpoint or None,
                extra_env=env,
                log_root=self.sessions_root / "_logs" / "external_agents",
                **common,
            )
        elif adapter_id == "omp.rpc_v2":
            adapter = ea.OmpRpcAdapter(
                binary=binary,
                session_dir=str(transport.get("session_dir") or "") or None,
                default_env=env,
                **common,
            )
        elif adapter_id == "omp.acp":
            adapter = ea.OmpAcpAdapter(binary=binary, env_extra=env, **common)
        elif adapter_id == "droid.rpc":
            adapter = ea.DroidRpcAdapter(
                binary=binary, env_extra=env,
                log_root=self.sessions_root / "_logs" / "external_agents",
                **common,
            )
        elif adapter_id == "droid.acp":
            adapter = ea.DroidAcpAdapter(binary=binary, env_extra=env, **common)
        else:
            raise ValueError(f"unsupported Runtime adapter: {adapter_id!r}")
        return self._retag(adapter, adapter_id, config.instance_id)

    def register(
        self,
        registry: AdapterRegistry,
        value: RuntimeAdapterConfig | Mapping[str, Any] | Any,
        *,
        metadata: Optional[dict[str, Any]] = None,
    ) -> Any:
        config = (
            value if isinstance(value, RuntimeAdapterConfig)
            else RuntimeAdapterConfig.from_value(value)
        )
        if not config.enabled:
            return None
        existing = registry.get(config.adapter_id, config.instance_id)
        if existing is not None:
            return existing
        from . import ExternalAgentDependencyError

        try:
            adapter = self.create(config)
        except ExternalAgentDependencyError as exc:
            # Keep the exact dependency failure observable in health/start;
            # registering one unavailable engine must not break all chat.
            from .unavailable import DependencyUnavailableAdapter

            adapter = DependencyUnavailableAdapter(config.adapter_id, exc, **self._common(config))
        registry.register(adapter, instance_id=config.instance_id, metadata={
            "engine": engine_for_adapter(config.adapter_id),
            "enabled": config.enabled,
            "default_for_engine": (
                config.instance_id == "default"
                and DEFAULT_ADAPTER_BY_ENGINE.get(
                    engine_for_adapter(config.adapter_id))
                == canonical_adapter_id(config.adapter_id)
            ),
            "adapter_endpoint": config.endpoint,
            "endpoint": config.endpoint,
            **(metadata or {}),
        })
        return adapter

    def populate(
        self,
        registry: AdapterRegistry,
        configured: Iterable[Any] = (),
        *,
        include_structured_defaults: bool = False,
        include_cli_compatibility: bool = True,
    ) -> None:
        rows = [RuntimeAdapterConfig.from_value(value) for value in configured]
        configured_keys = {
            (row.adapter_id, row.instance_id) for row in rows if row.enabled
        }
        for row in rows:
            self.register(registry, row)
        if include_structured_defaults:
            for adapter_id in STRUCTURED_ADAPTER_ENGINES:
                key = (adapter_id, "default")
                if key not in configured_keys and registry.get(*key) is None:
                    self.register(registry, RuntimeAdapterConfig(adapter_id=adapter_id))
        if include_cli_compatibility:
            for engine in sorted(SUPPORTED_ENGINE_IDS):
                adapter_id = f"cli.{engine}"
                key = (adapter_id, "default")
                if key not in configured_keys and registry.get(*key) is None:
                    self.register(registry, RuntimeAdapterConfig(adapter_id=adapter_id))


__all__ = [
    "DEFAULT_ADAPTER_BY_ENGINE",
    "DEFAULT_CLI_ADAPTER_BY_ENGINE",
    "DEFAULT_STRUCTURED_ADAPTER_BY_ENGINE",
    "HISTORICAL_ADAPTER_ENGINES",
    "LEGACY_ADAPTER_ALIASES",
    "OPTIONAL_STRUCTURED_ADAPTER_ENGINES",
    "RuntimeAdapterConfig",
    "RuntimeAdapterFactory",
    "STRUCTURED_ADAPTER_ENGINES",
    "canonical_adapter_id",
    "engine_for_adapter",
    "runtime_config_schema",
]
