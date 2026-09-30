"""RUNTIME-05：Agent Runtime 设置、多账户与能力面板后端（任务书 7.3 / 3.5 / 9.2）。

本模块提供：

- ``RuntimeInstanceConfig`` / ``RuntimeInstanceConfigStore``：Runtime instance
  配置实体与持久化。instance 由 ``adapter_id + instance_id`` 唯一标识，绑定
  binary/endpoint、账户引用（``secret://`` 句柄，不复制真实 Secret）、环境
  引用、模型集合与健康/能力快照缓存。
- ``AgentRuntimeService``：发现（九类 Runtime 默认实例）、认证状态、登录
  指引、probe 刷新与健康缓存的唯一业务入口；单个实例 probe 失败不影响其他
  实例。
- Command/Query Handler：``runtime.instance.upsert/remove/probe/refresh``、
  ``worker_profile.upsert/remove`` 与查询 ``runtime.instance.list`` /
  ``worker_profile.list``，注册到共享 ``MutekiCommandAPI``（CAP-01
  management 模式的 ``muteki_list_runtime_instances`` /
  ``muteki_list_worker_profiles`` 工具走同一查询 Handler）。
- ``create_agent_runtime_router``：FastAPI router 工厂，供 INTEG-01 挂载到
  ``apps/web/server.py``。设置 API 不直接写配置 Store——所有状态修改经
  Command API dispatch，返回 CommandReceipt。

凭据安全：配置与事件只携带 ``secret://`` 引用与账户 id，真实 Secret 永不进入
持久化配置、领域事件或日志。
"""

from __future__ import annotations

import asyncio
import json
import threading
import time
import tempfile
from dataclasses import dataclass, field, replace
from pathlib import Path
from typing import Any, Optional

from muteki.platform.command_handlers.base import (
    CommandFailed,
    CommandPlan,
    HandlerContext,
    SideEffectResult,
    correlation_id_of,
    make_error,
)
from muteki.platform.contracts.base import utcnow
from muteki.platform.contracts.commands import (
    ActorRef,
    CommandEnvelope,
    QueryResult,
)
from muteki.platform.contracts.errors import ErrorCategory, ErrorEnvelope
from muteki.platform.contracts.events import EventEnvelope
from muteki.platform.contracts.receipts import (
    AggregateRef,
    CommandReceipt,
    ReceiptState,
)
from muteki.external_agents.factory import (
    DEFAULT_ADAPTER_BY_ENGINE,
    DEFAULT_CLI_ADAPTER_BY_ENGINE,
    DEFAULT_STRUCTURED_ADAPTER_BY_ENGINE,
    canonical_adapter_id as _canonical_adapter_id,
    engine_for_adapter,
    runtime_config_schema,
)
from muteki.platform.contracts.external_agents import ACCESS_MODE_VALUES, ProbeRequest
from muteki.solver.worker_profiles import (
    VALID_BASE_ENGINES,
    normalize_worker_profile,
    parse_runtime_instance_ref,
)
from muteki.solver.credential_accounts import host_discovery_enabled, canonical_credential_id
from muteki.solver.engine_registry import (
    ENGINE_TEMPORARILY_UNSUPPORTED_CODE,
    EngineTemporarilyUnsupportedError,
    ensure_engine_supported,
    temporarily_disabled_engine_in_payload,
)

#: 事件生产者标识（builtin 领域模块命名风格）。
PRODUCER = "builtin.agent-runtime"

#: Web 传输入口的本地操作员身份（单操作员产品默认，与 CommandPolicy 一致）。
OPERATOR = ActorRef(kind="operator", id="local-user")


def _ensure_command_engine_supported(command: CommandEnvelope, value: Any) -> None:
    try:
        ensure_engine_supported(value)
    except EngineTemporarilyUnsupportedError as exc:
        raise CommandFailed(make_error(
            ENGINE_TEMPORARILY_UNSUPPORTED_CODE,
            exc.reason,
            ErrorCategory.STATE,
            correlation_id=correlation_id_of(command),
        )) from exc

#: CLI 兼容路径的 adapter_id 约定（与 CliDriverAdapter 一致）：cli.<engine>。
def canonical_adapter_id(adapter_id: str) -> str:
    """归一为产品稳定 Adapter ID；bare engine 选择 Conversation 默认 transport。"""
    return _canonical_adapter_id(adapter_id)


def engine_of_adapter(adapter_id: str) -> str:
    """adapter_id → 九类基础引擎名；无法识别时返回空串。"""
    engine = engine_for_adapter(adapter_id)
    return engine if engine in {*VALID_BASE_ENGINES, "devin"} else ""


#: 九类 Runtime 的登录指引（任务书 3.1.6：缺失/未认证显示准确诊断与登录命令）。
#: 与 muteki.solver.credential_accounts.detect_system_login 的检测口径一一对应。
LOGIN_GUIDANCE: dict[str, dict[str, str]] = {
    "devin": {
        "command": "devin auth login",
        "note": "聊天使用本机 Devin CLI 登录；也支持 WINDSURF_API_KEY。",
    },
    "claude": {
        "command": "claude（进入后执行 /login）",
        "note": "macOS 登录态保存在 Keychain；也可在统一凭据中心导入宿主登录。",
    },
    "codex": {
        "command": "codex login",
        "note": "登录后可在统一凭据中心从宿主 ~/.codex/auth.json 导入。",
    },
    "cursor": {
        "command": "cursor-agent login 或设置 CURSOR_API_KEY",
        "note": "headless 模式只读取 CURSOR_API_KEY。",
    },
    "pi": {
        "command": "pi（完成登录后写入 ~/.pi/agent）",
        "note": "",
    },
    "omp": {
        "command": "omp（完成登录后写入 ~/.omp/agent）",
        "note": "",
    },
    "kimi": {
        "command": "kimi（进入后执行 /login，写入 ~/.kimi-code）",
        "note": "",
    },
    "grok": {
        "command": "grok login",
        "note": "",
    },
    "opencode": {
        "command": "opencode auth login",
        "note": "",
    },
}

#: 环境引用值的合法前缀：只允许引用，不允许内联真实值。
_ENV_REF_PREFIXES = ("secret://", "env:")


def _secret_ref_for_account(account_id: str) -> str:
    """账户 id → SecretStore 引用句柄（句柄即账户 id，本体不出 SecretStore）。"""
    return f"secret://credential-accounts/{account_id}"


def _account_id_of_ref(credential_ref: str) -> str:
    """``secret://credential-accounts/<account_id>`` → 账户 id；否则空串。"""
    text = str(credential_ref or "").strip()
    prefix = "secret://credential-accounts/"
    if text.startswith(prefix):
        return text[len(prefix):].strip()
    return ""


def _normalize_credential_ref(value: Any) -> str:
    """账户引用归一化：bare 账户 id 升级为 secret:// 句柄；拒绝内联凭据。

    合法输入：空串、``secret://...`` 句柄、账户 id（字母数字与 ``._-``）。
    其余一律拒绝——宁可报错也不让疑似真实凭据进入持久化配置。
    """
    text = str(value or "").strip()
    if not text:
        return ""
    if text.startswith("secret://"):
        return text
    # bare 账户 id：字符白名单，含空白/等号/换行的输入视为疑似凭据本体。
    if all(ch.isalnum() or ch in "._-" for ch in text):
        return _secret_ref_for_account(text)
    raise ValueError(
        "credential_ref 只接受 secret:// 引用或账户 id，不接受内联凭据")


def _normalize_env_refs(value: Any) -> dict[str, str]:
    """环境引用归一化：``{NAME: "secret://..." | "env:HOST_VAR"}``，拒绝内联值。"""
    if value in (None, ""):
        return {}
    if not isinstance(value, dict):
        raise ValueError("env_refs 必须是对象 {变量名: 引用}")
    out: dict[str, str] = {}
    for key, raw in value.items():
        name = str(key or "").strip()
        ref = str(raw or "").strip()
        if not name:
            continue
        if not name.replace("_", "").isalnum() or not name.isupper():
            raise ValueError(f"env_refs 变量名须为大写环境变量名：{name!r}")
        if ref and not ref.startswith(_ENV_REF_PREFIXES):
            raise ValueError(
                f"env_refs[{name}] 只接受 secret:// 或 env: 引用，不接受内联值")
        out[name] = ref
    return out


def _normalize_models(value: Any) -> list[str]:
    if not isinstance(value, (list, tuple)):
        return []
    out: list[str] = []
    seen: set[str] = set()
    for item in value:
        model = str(item or "").strip()
        if model and model not in seen:
            seen.add(model)
            out.append(model)
    return out


# ---------------------------------------------------------------------------
# Runtime instance 配置实体
# ---------------------------------------------------------------------------


@dataclass
class RuntimeInstanceConfig:
    """一个 Runtime instance 的适配器技术配置（任务书 7.3）。

    身份为 ``adapter_id + instance_id``。凭据和模型由 Worker / Thread 的
    稳定引用选择；下面三个 legacy 字段只用于读取已经落盘的旧配置，新写入
    不再接受它们。
    """

    adapter_id: str
    instance_id: str = "default"
    label: str = ""
    binary_path: str = ""
    # Adapter 自身的 attach/service 地址。模型服务地址由 CredentialAccount
    # 的 custom_endpoint 元数据持有。
    endpoint: str = ""
    credential_ref: str = ""
    env_refs: dict[str, str] = field(default_factory=dict)
    models: list[str] = field(default_factory=list)
    default_model: str = ""
    enabled: bool = True
    transport: dict[str, Any] = field(default_factory=dict)
    created_at: str = ""
    updated_at: str = ""

    @property
    def key(self) -> str:
        return f"{self.adapter_id}:{self.instance_id}"

    @property
    def engine(self) -> str:
        return engine_of_adapter(self.adapter_id)

    @property
    def credential_account(self) -> str:
        return _account_id_of_ref(self.credential_ref)

    def to_dict(self) -> dict[str, Any]:
        data = {
            "adapter_id": self.adapter_id,
            "instance_id": self.instance_id,
            "key": self.key,
            "engine": self.engine,
            "label": self.label or self.key,
            "binary_path": self.binary_path,
            "adapter_endpoint": self.endpoint,
            # Published compatibility alias. It has adapter-service semantics.
            "endpoint": self.endpoint,
            "env_refs": dict(self.env_refs),
            "enabled": self.enabled,
            "transport": dict(self.transport),
            "config_schema": runtime_config_schema(self.adapter_id),
            "created_at": self.created_at,
            "updated_at": self.updated_at,
        }
        if self.credential_ref:
            data["credential_ref"] = self.credential_ref
            data["credential_account"] = self.credential_account
        if self.models:
            data["models"] = list(self.models)
        if self.default_model:
            data["default_model"] = self.default_model
        return data

    @classmethod
    def from_payload(cls, payload: dict[str, Any], *, existing: Optional["RuntimeInstanceConfig"] = None) -> "RuntimeInstanceConfig":
        """从命令 / API 负载构造配置（校验失败抛 ValueError）。

        给出 ``existing`` 时做增量合并（未提供字段保持原值），供 upsert。
        """
        base = existing.to_dict() if existing is not None else {}

        def _pick(name: str, default: Any = "") -> Any:
            if name in payload:
                return payload[name]
            return base.get(name, default)

        adapter_id = canonical_adapter_id(str(_pick("adapter_id") or ""))
        engine = engine_of_adapter(adapter_id)
        if not engine:
            raise ValueError(
                f"未知 Runtime adapter：{_pick('adapter_id')!r}，"
                "需要结构化 Adapter ID 或 cli.<engine> compatibility transport")
        instance_id = str(_pick("instance_id", "default") or "default").strip()
        if not instance_id or any(ch in instance_id for ch in ":/ \t\n"):
            raise ValueError(f"非法 instance_id：{instance_id!r}")
        # Credential/model fields were owned by Runtime in the legacy schema.
        # Every current write removes them: Worker/Thread ``credential_id`` and
        # model selection are now the only launch-time source of truth.
        credential_ref = ""
        env_refs = _normalize_env_refs(_pick("env_refs"))
        models: list[str] = []
        default_model = ""
        now = utcnow().isoformat()
        return cls(
            adapter_id=adapter_id,
            instance_id=instance_id,
            label=str(_pick("label") or "").strip(),
            binary_path=str(_pick("binary_path") or "").strip(),
            endpoint=str(
                payload.get("adapter_endpoint")
                if "adapter_endpoint" in payload
                else _pick("endpoint")
                or ""
            ).strip(),
            credential_ref=credential_ref,
            env_refs=env_refs,
            models=models,
            default_model=default_model,
            enabled=bool(_pick("enabled", True)),
            transport=dict(_pick("transport", {}) or {}),
            created_at=str(base.get("created_at") or now),
            updated_at=now,
        )

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> "RuntimeInstanceConfig":
        return cls(
            adapter_id=canonical_adapter_id(str(data.get("adapter_id") or "")),
            instance_id=str(data.get("instance_id") or "default"),
            label=str(data.get("label") or ""),
            binary_path=str(data.get("binary_path") or ""),
            endpoint=str(data.get("adapter_endpoint") or data.get("endpoint") or ""),
            credential_ref=str(data.get("credential_ref") or ""),
            env_refs={str(k): str(v) for k, v in dict(data.get("env_refs") or {}).items()},
            models=[str(m) for m in data.get("models") or []],
            default_model=str(data.get("default_model") or ""),
            enabled=bool(data.get("enabled", True)),
            transport=dict(data.get("transport") or {}),
            created_at=str(data.get("created_at") or ""),
            updated_at=str(data.get("updated_at") or ""),
        )


class RuntimeInstanceConfigStore:
    """instance 配置与健康快照的 JSON 持久化（风格对齐 WorkerConfigStore）。

    单个 JSON 文件（默认 ``state/_agent_runtimes.json``），原子写入；
    损坏文件不阻断启动（回落空配置）。只保存引用，不保存真实凭据。
    """

    def __init__(self, root: str | Path = "state") -> None:
        self._root = Path(root)
        self.path = self._root / "_agent_runtimes.json"
        self._lock = threading.RLock()
        self._instances: dict[str, RuntimeInstanceConfig] = {}
        self._health: dict[str, dict[str, Any]] = {}
        self._load()

    def _load(self) -> None:
        if not self.path.exists():
            return
        try:
            raw = json.loads(self.path.read_text(encoding="utf-8"))
        except (json.JSONDecodeError, OSError):
            return
        for item in raw.get("instances") or []:
            if not isinstance(item, dict):
                continue
            try:
                cfg = RuntimeInstanceConfig.from_dict(item)
            except Exception:  # noqa: BLE001 — 单条损坏不拖垮其余
                continue
            if cfg.adapter_id and cfg.instance_id:
                self._instances[cfg.key] = cfg
        health = raw.get("health")
        if isinstance(health, dict):
            self._health = {
                str(k): v for k, v in health.items() if isinstance(v, dict)
            }

    def _flush(self) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        tmp = self.path.with_suffix(".json.tmp")
        tmp.write_text(
            json.dumps(self.to_dict(), ensure_ascii=False, indent=2),
            encoding="utf-8")
        tmp.replace(self.path)  # atomic on POSIX

    def to_dict(self) -> dict[str, Any]:
        return {
            "instances": [cfg.to_dict() for cfg in self.list()],
            "health": dict(self._health),
        }

    # -- instance CRUD ---------------------------------------------------------

    def list(self, adapter_id: Optional[str] = None) -> list[RuntimeInstanceConfig]:
        with self._lock:
            rows = list(self._instances.values())
        if adapter_id is not None:
            rows = [r for r in rows if r.adapter_id == adapter_id]
        return sorted(rows, key=lambda r: (r.adapter_id, r.instance_id))

    def get(self, adapter_id: str, instance_id: str = "default") -> Optional[RuntimeInstanceConfig]:
        return self._instances.get(f"{adapter_id}:{instance_id}")

    def upsert(self, cfg: RuntimeInstanceConfig, *, expected_revision: str | None = None) -> RuntimeInstanceConfig:
        with self._lock:
            previous = self._instances.get(cfg.key)
            if expected_revision is not None and (previous.updated_at if previous else "") != expected_revision:
                raise ValueError("runtime.instance.revision_conflict")
            self._instances[cfg.key] = cfg
            try:
                self._flush()
            except BaseException:
                if previous is None:
                    self._instances.pop(cfg.key, None)
                else:
                    self._instances[cfg.key] = previous
                raise
        return cfg

    def remove(self, adapter_id: str, instance_id: str = "default") -> bool:
        key = f"{adapter_id}:{instance_id}"
        with self._lock:
            existed = self._instances.pop(key, None) is not None
            self._health.pop(key, None)
            if existed:
                self._flush()
        return existed

    def purge_engine(self, engine: str) -> dict[str, int]:
        """Remove configured instances and health snapshots for one engine."""
        selected = str(engine or "").strip().lower()
        removed_instances = 0
        removed_health = 0
        with self._lock:
            for key, cfg in list(self._instances.items()):
                if cfg.engine == selected:
                    self._instances.pop(key, None)
                    removed_instances += 1
            for key in list(self._health):
                adapter_id = key.rsplit(":", 1)[0]
                if engine_of_adapter(adapter_id) == selected:
                    self._health.pop(key, None)
                    removed_health += 1
            if removed_instances or removed_health:
                self._flush()
        return {
            "runtime_instances": removed_instances,
            "runtime_health": removed_health,
        }

    def clear_legacy_identity_fields(self) -> int:
        """Remove Runtime-owned credential/model fields after Thread migration.

        Loading remains backward compatible long enough for ``PlatformStack`` to
        copy an old Runtime credential into every legacy Thread. The fields are
        then removed atomically before Adapter registration, so they can never
        become Adapter defaults or compete with a Thread selection.
        """
        changed = 0
        with self._lock:
            for key, cfg in list(self._instances.items()):
                if not (cfg.credential_ref or cfg.models or cfg.default_model):
                    continue
                self._instances[key] = replace(
                    cfg,
                    credential_ref="",
                    models=[],
                    default_model="",
                    updated_at=utcnow().isoformat(),
                )
                changed += 1
            if changed:
                self._flush()
        return changed

    # -- 健康 / 能力快照缓存 ------------------------------------------------------

    def save_health(self, key: str, health: dict[str, Any]) -> None:
        with self._lock:
            self._health[key] = dict(health)
            self._flush()

    def health(self, key: str) -> Optional[dict[str, Any]]:
        cached = self._health.get(key)
        return dict(cached) if isinstance(cached, dict) else None


# ---------------------------------------------------------------------------
# AgentRuntimeService：发现 / 认证 / probe / Profile 映射的唯一业务入口
# ---------------------------------------------------------------------------


class RuntimeProbeScopeChangedError(RuntimeError):
    code = "runtime.instance.probe_scope_changed"


class AgentRuntimeService:
    """Runtime 设置面板的业务服务（Handler 与 HTTP router 共用）。

    依赖均为可选：``registry``（RUNTIME-01 AdapterRegistry）提供已注册
    实例的真实 probe；缺省时对 CLI 兼容路径做本机实测
    （``probe_cli_driver``）。``worker_config``（WorkerConfigStore）提供
    Worker Profile 读写；兼容参数 ``sessions_root`` 实际指向私有状态根，用于
    定位凭据账户库做认证检测。
    """

    def __init__(
        self,
        instance_store: RuntimeInstanceConfigStore,
        *,
        registry: Any = None,
        worker_config: Any = None,
        sessions_root: str | Path = "state",
        factory: Any = None,
        probe_timeout_s: float = 90.0,
    ) -> None:
        self.store = instance_store
        self.registry = registry
        self.worker_config = worker_config
        self.sessions_root = Path(sessions_root)
        self.factory = factory
        self._probe_tasks: dict[str, asyncio.Task[dict[str, Any]]] = {}
        self._probe_locks: dict[str, asyncio.Lock] = {}
        self._version_tasks: dict[str, asyncio.Task[dict[str, Any]]] = {}
        self._version_cache: dict[str, tuple[float, dict[str, Any]]] = {}
        self._version_cache_ttl_s = 300.0
        self._probe_timeout_s = max(1.0, float(probe_timeout_s))

    def apply_live_config(self, cfg: RuntimeInstanceConfig) -> None:
        """把设置页变更同步到当前进程的共享 AdapterRegistry。"""
        if self.registry is None or self.factory is None:
            return
        self.registry.unregister(cfg.adapter_id, cfg.instance_id)
        if cfg.enabled:
            self.factory.register(self.registry, cfg)

    def remove_live_config(self, adapter_id: str, instance_id: str) -> None:
        if self.registry is not None:
            self.registry.unregister(adapter_id, instance_id)
            if (
                self.factory is not None
                and instance_id == "default"
                and DEFAULT_ADAPTER_BY_ENGINE.get(
                    engine_of_adapter(adapter_id)) == adapter_id
            ):
                from muteki.external_agents.factory import RuntimeAdapterConfig

                self.factory.register(self.registry, RuntimeAdapterConfig(
                    adapter_id=adapter_id, instance_id=instance_id))

    # -- 发现 -----------------------------------------------------------------

    def _discovered_default(self, adapter_id: str) -> dict[str, Any]:
        """未配置 instance 的发现视图（CLI 产品默认或结构化兼容入口）。"""
        adapter_id = canonical_adapter_id(adapter_id)
        engine = engine_of_adapter(adapter_id)
        binary = ""
        try:
            from muteki.solver.cli_driver import DRIVERS

            driver = DRIVERS.get(engine)
            binary = str(driver.bin) if driver is not None else ""
        except Exception:  # noqa: BLE001 — binary 解析失败只影响展示
            binary = ""
        if adapter_id == "devin.acp":
            from muteki.external_agents.devin import default_devin_binary

            binary = default_devin_binary()
        structured = not adapter_id.startswith("cli.")
        return {
            "adapter_id": adapter_id,
            "instance_id": "default",
            "key": f"{adapter_id}:default",
            "engine": engine,
            "label": (
                f"{engine}（结构化兼容）" if structured
                else f"{engine}（CLI 默认）"
            ),
            "binary_path": binary,
            "adapter_endpoint": "",
            "endpoint": "",
            "env_refs": {},
            "enabled": True,
            "transport": {},
            "transport_kind": "structured" if structured else "cli",
            "access_modes": list(ACCESS_MODE_VALUES) if structured else [],
            "default_for_engine": (
                DEFAULT_ADAPTER_BY_ENGINE.get(engine) == adapter_id
            ),
            "config_schema": runtime_config_schema(adapter_id),
            "discovered": True,
            "configured": False,
        }

    def list_instances(
        self,
        adapter_id: Optional[str] = None,
        *,
        include_discovered: bool = True,
        transport_kind: str = "",
    ) -> list[dict[str, Any]]:
        """全部 instance 视图：已配置实例 + 未配置引擎的默认发现行。

        每行附认证状态与缓存的健康快照；发现/probe 失败只影响本行，
        不阻断其他 Runtime。
        """
        transport_kind = str(transport_kind or "").strip().lower()
        if transport_kind not in {"", "cli", "structured"}:
            raise ValueError(f"未知 Runtime transport：{transport_kind!r}")
        out: list[dict[str, Any]] = []
        adapter_id = canonical_adapter_id(adapter_id or "") or None
        configured = self.store.list(adapter_id)
        if not adapter_id and transport_kind:
            configured = [
                cfg for cfg in configured
                if (cfg.adapter_id.startswith("cli.")) == (transport_kind == "cli")
            ]
        seen_keys: set[str] = set()
        for cfg in configured:
            seen_keys.add(cfg.key)
            out.append(self._instance_view(cfg, discovered=False))
        if include_discovered:
            if adapter_id:
                adapter_ids = [canonical_adapter_id(adapter_id)]
            elif transport_kind == "cli":
                adapter_ids = [
                    DEFAULT_CLI_ADAPTER_BY_ENGINE[engine]
                    for engine in VALID_BASE_ENGINES
                    if engine in DEFAULT_CLI_ADAPTER_BY_ENGINE
                ]
            elif transport_kind == "structured":
                adapter_ids = list(DEFAULT_STRUCTURED_ADAPTER_BY_ENGINE.values())
            else:
                adapter_ids = list(DEFAULT_STRUCTURED_ADAPTER_BY_ENGINE.values()) + [
                    f"cli.{engine}" for engine in VALID_BASE_ENGINES]
            for discovered_adapter_id in adapter_ids:
                key = f"{discovered_adapter_id}:default"
                if engine_of_adapter(discovered_adapter_id) and key not in seen_keys:
                    view = self._discovered_default(discovered_adapter_id)
                    health = self.store.health(view["key"])
                    view["health"] = health
                    cached_auth = (
                        health.get("auth")
                        if discovered_adapter_id.startswith("cli.")
                        and host_discovery_enabled()
                        and isinstance(health, dict)
                        and isinstance(health.get("auth"), dict)
                        else None
                    )
                    view["auth"] = cached_auth or self._auth_view(
                        engine_of_adapter(discovered_adapter_id), "")
                    out.append(view)
        for row in out:
            health = row.get("health") or self.store.health(row["key"])
            row.setdefault("health", health)
            cached_auth = (
                health.get("auth")
                if str(row.get("adapter_id") or "").startswith("cli.")
                and (bool(_account_id_of_ref(str(row.get("credential_ref") or ""))) or host_discovery_enabled())
                and isinstance(health, dict)
                and isinstance(health.get("auth"), dict)
                else None
            )
            row.setdefault("auth", cached_auth or self._auth_view(
                row["engine"], str(row.get("credential_ref") or "")))
        return out

    def _instance_view(self, cfg: RuntimeInstanceConfig, *, discovered: bool) -> dict[str, Any]:
        view = cfg.to_dict()
        view["discovered"] = discovered
        view["configured"] = not discovered
        view["transport_kind"] = (
            "cli" if cfg.adapter_id.startswith("cli.") else "structured"
        )
        view["access_modes"] = (
            [] if cfg.adapter_id.startswith("cli.") else list(ACCESS_MODE_VALUES)
        )
        view["default_for_engine"] = (
            cfg.instance_id == "default"
            and DEFAULT_ADAPTER_BY_ENGINE.get(cfg.engine)
            == cfg.adapter_id
        )
        return view

    def get_instance(self, adapter_id: str, instance_id: str) -> Optional[dict[str, Any]]:
        adapter_id = canonical_adapter_id(adapter_id)
        cfg = self.store.get(adapter_id, instance_id)
        if cfg is not None:
            view = self._instance_view(cfg, discovered=False)
        elif instance_id == "default" and engine_of_adapter(adapter_id):
            view = self._discovered_default(adapter_id)
        else:
            return None
        health = self.store.health(view["key"])
        view["health"] = health
        cached_auth = (
            health.get("auth")
            if adapter_id.startswith("cli.")
            and (bool(_account_id_of_ref(str(view.get("credential_ref") or ""))) or host_discovery_enabled())
            and isinstance(health, dict)
            and isinstance(health.get("auth"), dict)
            else None
        )
        view["auth"] = cached_auth or self._auth_view(
            view["engine"], str(view.get("credential_ref") or ""))
        return view

    # -- 认证状态与登录指引 --------------------------------------------------------

    def _auth_view(
        self,
        engine: str,
        credential_ref: str,
        *,
        detect_host: bool = False,
    ) -> dict[str, Any]:
        """实例认证状态：ok / missing / unknown + 诊断与登录命令。不抛异常。"""
        guidance = LOGIN_GUIDANCE.get(engine, {})
        login_command = str(guidance.get("command") or "")
        note = str(guidance.get("note") or "")
        account_id = _account_id_of_ref(credential_ref)
        if not credential_ref and not detect_host:
            return {
                "status": "not_applicable",
                "detail": "凭据由 Worker 或 Thread 的 credential_id 在启动时解析",
                "login_command": "",
                "account_id": "",
            }
        try:
            if account_id:
                from muteki.solver.credential_accounts import (
                    CredentialAccountStore,
                    account_store_root,
                )

                store = CredentialAccountStore(
                    account_store_root(self.sessions_root))
                row = next(
                    (a for a in store.list()
                     if str(a.get("account_id") or "") == account_id),
                    None,
                )
                if row is not None and row.get("present"):
                    return {
                        "status": "ok",
                        "detail": f"账户 {account_id} 已登记凭据",
                        "login_command": "",
                        "account_id": account_id,
                    }
                detail = (f"账户 {account_id} 已登记但凭据缺失"
                          if row is not None else f"账户 {account_id} 尚未登记凭据")
                return {
                    "status": "missing",
                    "detail": detail,
                    "login_command": login_command,
                    "note": note or "登录后在统一凭据中心登记该账户。",
                    "account_id": account_id,
                }
            from muteki.solver.credential_accounts import detect_system_login

            host = detect_system_login(engine)
            if host == "disabled":
                return {
                    "status": "unavailable",
                    "code": "host_discovery_disabled",
                    "detail": "此服务未提供宿主登录发现；请选择已登记凭据",
                    "login_command": "",
                    "account_id": "",
                }
            if host == "present":
                return {
                    "status": "ok",
                    "detail": "检测到宿主登录态",
                    "login_command": "",
                    "account_id": "",
                }
            if host == "absent":
                return {
                    "status": "missing",
                    "detail": "未检测到宿主登录态",
                    "login_command": login_command,
                    "note": note,
                    "account_id": "",
                }
            return {
                "status": "unknown",
                "detail": "认证状态检测失败",
                "login_command": login_command,
                "note": note,
                "account_id": "",
            }
        except Exception as exc:  # noqa: BLE001 — 检测失败不阻断其他 Runtime
            return {
                "status": "unknown",
                "detail": f"认证检测异常：{str(exc)}",
                "login_command": login_command,
                "note": note,
                "account_id": account_id,
            }

    # -- probe 与健康缓存 ----------------------------------------------------------

    async def probe_one(
        self,
        adapter_id: str,
        instance_id: str,
        *,
        force_version_check: bool = False,
        credential_id: str = "",
        environment: str = "local",
    ) -> dict[str, Any]:
        """Share only probes with the same Runtime configuration and owner."""
        adapter_id = canonical_adapter_id(adapter_id)
        key = f"{adapter_id}:{instance_id}"
        if environment != "local":
            raise ValueError("Runtime probes execute in the local conversation environment")
        credential_id = canonical_credential_id(credential_id, engine=engine_of_adapter(adapter_id))
        if credential_id and adapter_id != "codex.app_server":
            raise ValueError("Scoped model catalog probing is supported by codex.app_server")
        cfg = self.store.get(adapter_id, instance_id)
        configuration = json.loads(json.dumps(cfg.to_dict())) if cfg is not None else None
        task_key = json.dumps([key, credential_id, environment, configuration], sort_keys=True)
        existing = self._probe_tasks.get(task_key)
        if existing is not None and not existing.done():
            return await asyncio.shield(existing)

        async def _perform() -> dict[str, Any]:
            try:
                # Registered adapters keep their last report on the instance.
                # Serialize different credential probes so that report cannot
                # be read back as the other request's result.
                async with self._probe_locks.setdefault(key, asyncio.Lock()):
                    return await self._probe_one_unlocked(
                        adapter_id, instance_id,
                        force_version_check=force_version_check,
                        credential_id=credential_id, environment=environment,
                        configuration=configuration,
                    )
            except Exception as exc:
                if credential_id or isinstance(exc, RuntimeProbeScopeChangedError):
                    raise
                engine = engine_of_adapter(adapter_id)
                previous = self.store.health(key) or {}
                self.store.save_health(key, {
                    "healthy": False,
                    "detail": f"{type(exc).__name__}: {exc}",
                    "binary_path": "",
                    "runtime_version": "",
                    "probed_at": utcnow().isoformat(),
                    "capabilities": {},
                    "capability_injection": {},
                    "field_sources": {},
                    "degradations": [],
                    "auth": self._auth_view(engine, "") if engine else {},
                    "source": "probe_error",
                    "version_check": dict(previous.get("version_check") or {}),
                })
                raise

        task = asyncio.create_task(_perform())
        self._probe_tasks[task_key] = task
        try:
            return await asyncio.wait_for(
                asyncio.shield(task), timeout=self._probe_timeout_s,
            )
        except asyncio.CancelledError:
            task.cancel()
            try:
                await task
            except asyncio.CancelledError:
                pass
            raise
        except asyncio.TimeoutError as exc:
            task.cancel()
            try:
                await task
            except asyncio.CancelledError:
                pass
            engine = engine_of_adapter(adapter_id)
            previous = self.store.health(key) or {}
            detail = f"Runtime 探测超过 {self._probe_timeout_s:g} 秒"
            if credential_id:
                raise TimeoutError(detail) from exc
            self.store.save_health(key, {
                "healthy": False,
                "detail": detail,
                "binary_path": "",
                "runtime_version": "",
                "probed_at": utcnow().isoformat(),
                "capabilities": {},
                "capability_injection": {},
                "field_sources": {},
                "degradations": [detail],
                "auth": self._auth_view(engine, "") if engine else {},
                "source": "probe_timeout",
                "version_check": dict(previous.get("version_check") or {}),
            })
            raise TimeoutError(detail) from exc
        finally:
            if task.done() and self._probe_tasks.get(task_key) is task:
                self._probe_tasks.pop(task_key, None)

    async def _probe_credential_catalog(
        self, adapter_id: str, instance_id: str, credential_id: str,
        configuration: dict[str, Any] | None,
    ) -> tuple[Any, str]:
        """Prepare a small temporary native home for the selected credential."""
        from muteki.conversation.chat_plugins import ChatPluginService
        from muteki.external_agents.factory import RuntimeAdapterConfig
        from muteki.external_agents.probe_environment import PROBE_ENVIRONMENT
        from muteki.solver.credential_accounts import resolve_credential_env
        from muteki.solver.credential_accounts import (
            CredentialAccountStore, account_id_from_credential_id, account_store_root,
        )

        accounts = CredentialAccountStore(account_store_root(self.sessions_root))
        account_id = account_id_from_credential_id(credential_id)
        revision = accounts.revision(account_id) if account_id else ""

        base = self.sessions_root / "_runtime_probe_environments"
        base.mkdir(parents=True, exist_ok=True, mode=0o700)
        with tempfile.TemporaryDirectory(prefix="probe-", dir=base) as directory:
            root = Path(directory)

            def prepare() -> dict[str, str]:
                resolved = resolve_credential_env(
                    credential_id, engine="codex", sessions_root=self.sessions_root,
                    container=False, agent_state_dir=root / "credentials",
                )
                cfg = RuntimeAdapterConfig.from_value(configuration or {
                    "adapter_id": adapter_id, "instance_id": instance_id})
                configured = self.factory._env(cfg) if self.factory is not None else {}
                return ChatPluginService(root / "native").prepare_environment(
                    "codex", f"probe:{adapter_id}:{instance_id}:{credential_id}",
                    {**configured, **resolved.env}, include_assets=False,
                )

            preparation = asyncio.create_task(asyncio.to_thread(prepare))
            try:
                env = await asyncio.shield(preparation)
            except asyncio.CancelledError:
                # to_thread cannot be stopped. Wait for its writes to finish
                # before removing the temporary credential/home tree.
                await preparation
                raise
            token = PROBE_ENVIRONMENT.set(env)
            try:
                report = await self.registry.probe(adapter_id, instance_id, request=ProbeRequest(
                    runtime_instance_id=instance_id, include_models=True))
                if account_id and accounts.revision(account_id) != revision:
                    raise RuntimeProbeScopeChangedError("Credential configuration changed during model catalog probing")
                return report, revision
            finally:
                PROBE_ENVIRONMENT.reset(token)

    async def _check_version(
        self,
        *,
        key: str,
        engine: str,
        binary_path: str,
        runtime_version: str,
        force: bool,
    ) -> dict[str, Any]:
        """读取发布版本并做五分钟缓存；失败时保留同版本的上次成功结果。"""
        cache_key = f"{engine}\0{binary_path}\0{runtime_version}"
        cached = self._version_cache.get(cache_key)
        now = time.monotonic()
        if cached and not force and now - cached[0] < self._version_cache_ttl_s:
            return dict(cached[1])
        existing = self._version_tasks.get(cache_key)
        if existing is not None and not existing.done():
            return await asyncio.shield(existing)

        async def _perform() -> dict[str, Any]:
            from muteki.external_agents.version_check import check_cli_version

            result = await check_cli_version(engine, binary_path, runtime_version)
            if result.get("status") == "unknown":
                previous = dict((self.store.health(key) or {}).get("version_check") or {})
                same_install = (
                    previous.get("installed_version")
                    and previous.get("installed_version") == result.get("installed_version")
                )
                if same_install and previous.get("latest_version"):
                    result = {
                        **previous,
                        "stale": True,
                        "attempted_at": result.get("checked_at"),
                        "detail": "版本源暂时不可用，沿用上次检查结果",
                        "error": result.get("error", ""),
                    }
            self._version_cache[cache_key] = (time.monotonic(), dict(result))
            return result

        task = asyncio.create_task(_perform())
        self._version_tasks[cache_key] = task
        try:
            return await asyncio.shield(task)
        finally:
            if task.done() and self._version_tasks.get(cache_key) is task:
                self._version_tasks.pop(cache_key, None)

    async def _probe_one_unlocked(
        self,
        adapter_id: str,
        instance_id: str,
        *,
        force_version_check: bool = False,
        credential_id: str = "",
        environment: str = "local",
        configuration: dict[str, Any] | None = None,
    ) -> dict[str, Any]:
        """对一个 instance 做 capability probe 并写健康缓存。

        优先使用 AdapterRegistry 中已注册的实例（RUNTIME-01）；否则对该
        引擎的 CLI 兼容路径做本机实测（binary + --version + 覆写检测）。
        probe 失败抛出异常，由调用方归类为该实例的失败事件——不影响其他
        实例。
        """
        engine = engine_of_adapter(adapter_id)
        if not engine:
            raise ValueError(f"未知 Runtime adapter：{adapter_id!r}")
        cfg = self.store.get(adapter_id, instance_id)
        key = f"{adapter_id}:{instance_id}"
        def ensure_configuration() -> None:
            current = self.store.get(adapter_id, instance_id)
            if (current.to_dict() if current is not None else None) != configuration:
                raise RuntimeProbeScopeChangedError("Runtime configuration changed during model catalog probing")
        if credential_id:
            ensure_configuration()

        caps: Any = None
        report: Any = None
        credential_revision = ""
        source = "cli_driver"
        if self.registry is not None and self.registry.record(adapter_id, instance_id) is not None:
            if credential_id:
                report, credential_revision = await self._probe_credential_catalog(
                    adapter_id, instance_id, credential_id, configuration)
            else:
                report = await self.registry.probe(adapter_id, instance_id)
            caps = report.capabilities
            source = "registry"
        elif adapter_id.startswith("cli."):
            from muteki.external_agents.capabilities import probe_cli_driver
            from muteki.solver.cli_driver import DRIVERS

            driver = DRIVERS.get(engine)
            if driver is None:
                raise ValueError(f"引擎 {engine!r} 没有可用的 CLI Driver")
            report = await asyncio.to_thread(
                probe_cli_driver,
                driver,
                adapter_id=adapter_id,
                instance_id=instance_id,
                include_models=True,
            )
            caps = report.capabilities
        else:
            raise LookupError(
                f"结构化 Runtime instance 未注册：{adapter_id}:{instance_id}")

        # 实例显式覆盖 binary 时单独验证该 binary 可运行（不改动 Driver）。
        detail = str(getattr(report, "detail", "") or "")
        binary_path = str(getattr(report, "binary_path", "") or "")
        healthy = bool(getattr(report, "healthy", False))
        overridden = str(cfg.binary_path if cfg else "").strip()
        if overridden:
            binary_path = overridden
            from muteki.external_agents.capabilities import _probe_version

            version = _probe_version(overridden)
            if not version:
                healthy = False
                detail = f"实例配置的 binary 不可运行：{overridden}"
            elif not caps.runtime_version:
                caps = caps.model_copy(update={"runtime_version": version})

        auth = await asyncio.to_thread(
            self._auth_view,
            engine,
            str(cfg.credential_ref if cfg else ""),
            detect_host=adapter_id.startswith("cli.") or adapter_id == "devin.acp",
        )
        from muteki.external_agents.capabilities import describe_injection_path
        binary_source = ""
        binary_env = ""
        if adapter_id.startswith("cli."):
            try:
                from muteki.solver.cli_driver import (
                    _ENV_OVERRIDE,
                    resolve_engine_bin_source,
                )

                binary_source = resolve_engine_bin_source(engine)
                binary_env = str(_ENV_OVERRIDE.get(engine) or "")
            except Exception:  # noqa: BLE001 — 仅影响诊断元数据
                pass

        runtime_version = str(getattr(caps, "runtime_version", "") or "")
        version_check = await self._check_version(
            key=key,
            engine=engine,
            binary_path=binary_path,
            runtime_version=runtime_version,
            force=force_version_check,
        )
        health = {
            "healthy": healthy,
            "detail": detail,
            "binary_path": binary_path,
            "binary_source": binary_source,
            "binary_env": binary_env,
            "runtime_version": runtime_version,
            "version_check": version_check,
            "probed_at": utcnow().isoformat(),
            "capabilities": caps.model_dump(mode="json") if hasattr(caps, "model_dump") else {},
            "capability_injection": describe_injection_path(caps),
            "field_sources": dict(getattr(report, "field_sources", {}) or {}),
            "degradations": list(getattr(report, "degradations", []) or []),
            "auth": auth,
            "source": source,
        }
        if credential_id:
            ensure_configuration()
            from muteki.solver.credential_accounts import (
                CredentialAccountStore, account_id_from_credential_id, account_store_root,
            )
            account_id = account_id_from_credential_id(credential_id)
            if account_id and CredentialAccountStore(account_store_root(self.sessions_root)).revision(account_id) != credential_revision:
                raise RuntimeProbeScopeChangedError("Credential configuration changed before model catalog publication")
            from apps.web.worker_models import CredentialModelCatalogStore
            result = getattr(report, "model_catalog", None)
            if not isinstance(result, dict):
                result = {"ok": False, "models": [], "source": "codex.model/list",
                          "error_code": "codex.model_catalog.unavailable",
                          "detail": "Native Runtime did not return a model catalog"}
            catalogs = CredentialModelCatalogStore(self.sessions_root)
            previous = catalogs.get(credential_id, engine, environment, key) or {}
            health["model_catalog"] = catalogs.save_discovery(
                credential_id=credential_id, engine=engine, environment=environment,
                runtime_instance=key, result=result,
                configured_models=list(previous.get("configured_models") or []),
                default_model=str(previous.get("default_model") or result.get("default_model") or ""),
            )
            # Retain the complete native diagnostic in the probe response too.
            health["model_catalog"]["detail"] = str(result.get("detail") or "")
        if not credential_id:
            self.store.save_health(key, health)
        return health

    async def probe_all(
        self,
        adapter_id: Optional[str] = None,
        *,
        transport_kind: str = "",
        force_version_check: bool = False,
    ) -> dict[str, Any]:
        """批量 probe（配置实例 + 该范围引擎的默认发现实例）。

        单实例失败被隔离进结果条目，不中断其余实例。
        """
        views = self.list_instances(
            adapter_id,
            include_discovered=True,
            transport_kind=transport_kind,
        )
        semaphore = asyncio.Semaphore(4)

        async def probe(view: dict[str, Any]) -> dict[str, Any]:
            entry = {
                "key": view["key"],
                "adapter_id": view["adapter_id"],
                "instance_id": view["instance_id"],
                "engine": view["engine"],
            }
            async with semaphore:
                try:
                    health = await self.probe_one(
                        view["adapter_id"],
                        view["instance_id"],
                        force_version_check=force_version_check,
                    )
                    entry["ok"] = True
                    entry["healthy"] = bool(health.get("healthy"))
                    entry["detail"] = str(health.get("detail") or "")
                    entry["version_check"] = dict(
                        health.get("version_check") or {})
                except Exception as exc:  # noqa: BLE001 — 故障隔离
                    entry["ok"] = False
                    entry["healthy"] = False
                    entry["detail"] = f"{type(exc).__name__}: {str(exc)[:160]}"
            return entry

        results = list(await asyncio.gather(*(probe(view) for view in views)))
        failed = [r for r in results if not r["ok"]]
        return {
            "probed_at": utcnow().isoformat(),
            "total": len(results),
            "failed": len(failed),
            "results": results,
        }

    # -- Worker Profile 映射 --------------------------------------------------------

    def _raw_profiles(self) -> list[dict[str, Any]]:
        if self.worker_config is None:
            return []
        try:
            cfg = self.worker_config.get()
        except Exception:  # noqa: BLE001
            return []
        return [p for p in cfg.get("worker_profiles") or [] if isinstance(p, dict)]

    def list_worker_profiles(self) -> list[dict[str, Any]]:
        """Worker Profile 清单；执行后端固定为同引擎的默认 CLI。"""
        instances = self.list_instances(
            include_discovered=True, transport_kind="cli")
        by_key = {str(item.get("key") or ""): item for item in instances}
        out: list[dict[str, Any]] = []
        for profile in self._raw_profiles():
            row = dict(profile)
            explicit = parse_runtime_instance_ref(profile.get("runtime_instance_ref"))
            engine = str(profile.get("engine") or "")
            expected_key = f"cli.{engine}:default"
            binding = by_key.get(expected_key)
            if binding is not None:
                row["runtime_binding"] = {
                    "key": f"{binding['adapter_id']}:{binding['instance_id']}",
                    "adapter_id": binding["adapter_id"],
                    "instance_id": binding["instance_id"],
                    "enabled": bool(binding.get("enabled", True)),
                    "source": "worker_cli_default",
                    "transport_kind": "cli",
                    "health": binding.get("health"),
                    "auth": binding.get("auth"),
                    "capabilities": dict(
                        (binding.get("health") or {}).get("capabilities") or {}),
                    "capability_injection": dict(
                        (binding.get("health") or {}).get(
                            "capability_injection") or {}),
                }
                row["runtime_binding_reason"] = (
                    "旧 runtime_instance_ref 已忽略；做题模式固定使用同引擎 CLI"
                    if explicit and f"{explicit[0]}:{explicit[1]}" != expected_key
                    else ""
                )
            else:
                row["runtime_binding"] = None
                row["runtime_binding_reason"] = f"缺少 {expected_key} CLI Worker"
            row["runtime_resolution"] = {
                "requested": expected_key,
                "selected": str((row.get("runtime_binding") or {}).get("key") or ""),
                "reason": str(row.get("runtime_binding_reason") or ""),
                "fallback_order": ["worker_cli_default"],
            }
            out.append(row)
        return out

    def upsert_worker_profile(self, payload: dict[str, Any]) -> dict[str, Any]:
        """经 WorkerConfigStore 规范化并持久化一个 Profile（upsert 语义）。"""
        if self.worker_config is None:
            raise ValueError("worker profile store 不可用")
        profile = normalize_worker_profile(payload, reject_invalid=True)
        assert profile is not None  # reject_invalid=True 时不会静默返回 None
        # Worker 的执行身份由 engine/model/credential_account 决定；
        # Conversation Runtime instance 选择不进入做题 Profile。
        profile["runtime_instance_ref"] = ""
        profiles = self._raw_profiles()
        replaced = False
        new_list: list[dict[str, Any]] = []
        for item in profiles:
            if str(item.get("name") or item.get("id")) == profile["name"]:
                new_list.append(profile)
                replaced = True
            else:
                new_list.append(item)
        if not replaced:
            new_list.append(profile)
        self.worker_config.set(worker_profiles=new_list)
        return profile

    def remove_worker_profile(self, name: str) -> None:
        if self.worker_config is None:
            raise ValueError("worker profile store 不可用")
        profiles = self._raw_profiles()
        kept = [
            p for p in profiles
            if str(p.get("name") or p.get("id")) != name
        ]
        if len(kept) == len(profiles):
            raise KeyError(f"unknown worker profile: {name}")
        if not kept:
            # 清空会触发 normalize 的默认回填，静默恢复默认 Profile 不可接受。
            raise ValueError("不能删除最后一个 Worker Profile")
        self.worker_config.set(worker_profiles=kept)


# ---------------------------------------------------------------------------
# Command / Query Handler（注册到共享 MutekiCommandAPI）
# ---------------------------------------------------------------------------


def _event(
    command: CommandEnvelope,
    event_type: str,
    aggregate_id: str,
    payload: dict[str, Any],
) -> EventEnvelope:
    """统一 envelope 的领域事件（aggregate_type 固定为 runtime/profile）。"""
    return EventEnvelope(
        aggregate_type=command.aggregate_type or "runtime",
        aggregate_id=aggregate_id,
        event_type=event_type,
        producer=PRODUCER,
        actor_id=command.actor.id or "system",
        command_id=command.command_id,
        causation_id=command.command_id,
        correlation_id=correlation_id_of(command),
        idempotency_key=command.idempotency_key,
        payload=payload,
    )


def _instance_key_of(command: CommandEnvelope) -> tuple[str, str]:
    """从 aggregate_id（``adapter:instance``）或 payload 解析实例身份。"""
    ref = parse_runtime_instance_ref(command.aggregate_id)
    if ref is not None:
        return ref
    adapter_id = canonical_adapter_id(str(command.payload.get("adapter_id") or ""))
    instance_id = str(command.payload.get("instance_id") or "default").strip() or "default"
    return adapter_id, instance_id


class RuntimeInstanceCommandHandler:
    """``runtime.instance.*`` 命令命名空间的属主（RUNTIME-05）。

    状态修改只经 Command API：plan 阶段校验并产生领域事件 + accepted 回执，
    副作用阶段写配置 Store / 执行 probe 并写健康缓存，回执落终态。
    """

    command_types = {
        "runtime.instance.upsert",
        "runtime.instance.remove",
        "runtime.instance.probe",
        "runtime.instance.refresh",
        "runtime.instance.login",
        "runtime.instance.logout",
        "runtime.instance.auth.refresh",
        "runtime.instance.capability.refresh",
    }

    def __init__(self, service: AgentRuntimeService) -> None:
        self._service = service

    async def plan(self, command: CommandEnvelope, ctx: HandlerContext) -> CommandPlan:
        ct = command.command_type
        if ct == "runtime.instance.upsert":
            return self._plan_upsert(command)
        if ct == "runtime.instance.remove":
            return self._plan_remove(command)
        if ct == "runtime.instance.probe":
            return self._plan_probe(command)
        if ct == "runtime.instance.capability.refresh":
            return self._plan_probe(command)
        if ct == "runtime.instance.refresh":
            return self._plan_refresh(command)
        return self._plan_auth(command)

    def _plan_upsert(self, command: CommandEnvelope) -> CommandPlan:
        _ensure_command_engine_supported(
            command,
            command.payload.get("adapter_id") or command.aggregate_id,
        )
        try:
            adapter_id, instance_id = _instance_key_of(command)
            existing = self._service.store.get(adapter_id, instance_id)
            cfg = RuntimeInstanceConfig.from_payload(
                {**dict(command.payload),
                 "adapter_id": adapter_id or command.payload.get("adapter_id"),
                 "instance_id": instance_id},
                existing=existing)
        except ValueError as exc:
            raise CommandFailed(make_error(
                "runtime.instance.invalid", str(exc), ErrorCategory.VALIDATION,
                correlation_id=correlation_id_of(command))) from exc
        receipt = CommandReceipt(
            command_id=command.command_id,
            state=ReceiptState.ACCEPTED,
            aggregate=AggregateRef(type="runtime", id=cfg.key),
        )
        store = self._service.store

        async def _save() -> SideEffectResult:
            try:
                store.upsert(cfg, expected_revision=command.payload.get("expected_revision"))
            except ValueError as exc:
                raise CommandFailed(make_error("runtime.instance.revision_conflict", "运行配置已被其它视图修改，请刷新后重试。", ErrorCategory.CONFLICT, correlation_id=correlation_id_of(command))) from exc
            self._service.apply_live_config(cfg)
            return SideEffectResult(output={"instance": cfg.to_dict()})

        return CommandPlan(
            events=[_event(command, "core.runtime.instance.upserted", cfg.key, {
                "key": cfg.key,
                "adapter_id": cfg.adapter_id,
                "instance_id": cfg.instance_id,
                "engine": cfg.engine,
                "binary_path": cfg.binary_path,
                "adapter_endpoint": cfg.endpoint,
                "enabled": cfg.enabled,
            })],
            receipt=receipt,
            side_effect=_save,
        )

    def _plan_remove(self, command: CommandEnvelope) -> CommandPlan:
        adapter_id, instance_id = _instance_key_of(command)
        _ensure_command_engine_supported(command, adapter_id)
        key = f"{adapter_id}:{instance_id}"
        if self._service.store.get(adapter_id, instance_id) is None:
            raise CommandFailed(make_error(
                "runtime.instance.not_found",
                f"unknown runtime instance: {key}",
                ErrorCategory.NOT_FOUND,
                correlation_id=correlation_id_of(command)))
        store = self._service.store

        async def _remove() -> SideEffectResult:
            store.remove(adapter_id, instance_id)
            self._service.remove_live_config(adapter_id, instance_id)
            return SideEffectResult()

        return CommandPlan(
            events=[_event(command, "core.runtime.instance.removed", key, {
                "key": key})],
            receipt=CommandReceipt(
                command_id=command.command_id,
                state=ReceiptState.ACCEPTED,
                aggregate=AggregateRef(type="runtime", id=key)),
            side_effect=_remove,
        )

    def _plan_probe(self, command: CommandEnvelope) -> CommandPlan:
        adapter_id, instance_id = _instance_key_of(command)
        _ensure_command_engine_supported(command, adapter_id)
        if not engine_of_adapter(adapter_id):
            raise CommandFailed(make_error(
                "runtime.instance.invalid",
                f"未知 Runtime adapter：{adapter_id!r}",
                ErrorCategory.VALIDATION,
                correlation_id=correlation_id_of(command)))
        key = f"{adapter_id}:{instance_id}"
        if self._service.get_instance(adapter_id, instance_id) is None:
            raise CommandFailed(make_error(
                "runtime.instance.not_found",
                f"unknown runtime instance: {key}",
                ErrorCategory.NOT_FOUND,
                correlation_id=correlation_id_of(command)))
        service = self._service
        raw_credential = command.payload.get("credential_id", "")
        environment = command.payload.get("environment", "local")
        try:
            if not isinstance(raw_credential, str) or environment != "local":
                raise ValueError("credential_id must be a string and environment must be local")
            credential_id = canonical_credential_id(raw_credential, engine=engine_of_adapter(adapter_id))
            if credential_id and adapter_id != "codex.app_server":
                raise ValueError("Scoped model catalog probing is supported by codex.app_server")
        except ValueError as exc:
            raise CommandFailed(make_error(
                "runtime.instance.probe_scope_invalid", str(exc), ErrorCategory.VALIDATION,
                correlation_id=correlation_id_of(command))) from exc

        async def _probe() -> SideEffectResult:
            try:
                health = await service.probe_one(
                    adapter_id,
                    instance_id,
                    force_version_check=True,
                    credential_id=credential_id,
                    environment=environment,
                )
            except Exception as exc:  # noqa: BLE001 — 归类为该实例的失败
                return SideEffectResult(
                    events=[_event(command, "core.runtime.instance.probe_failed", key, {
                        "key": key,
                        "error": f"{type(exc).__name__}: {exc}",
                    })],
                    error=make_error(
                        str(getattr(exc, "code", "core.runtime.instance.probe_failed")),
                        f"{type(exc).__name__}: {exc}",
                        ErrorCategory.STATE if isinstance(exc, RuntimeProbeScopeChangedError) else ErrorCategory.INTERNAL,
                        correlation_id=correlation_id_of(command),
                        retryable=True),
                    state=ReceiptState.FAILED,
                )
            catalog = health.get("model_catalog") or {}
            if credential_id and catalog.get("refresh_status") != "fresh":
                return SideEffectResult(
                    events=[_event(command, "core.runtime.instance.probe_failed", key, {
                        "key": key, "credential_id": credential_id, "environment": environment,
                        "error": str(catalog.get("detail") or catalog.get("last_error") or "Native catalog unavailable"),
                    })],
                    error=make_error(
                        str(catalog.get("error_code") or "codex.model_catalog.request_failed"),
                        str(catalog.get("detail") or catalog.get("last_error") or "Native catalog unavailable"),
                        ErrorCategory.RUNTIME, correlation_id=correlation_id_of(command), retryable=True),
                    state=ReceiptState.FAILED,
                )
            return SideEffectResult(events=[_event(
                command, "core.runtime.instance.probed", key, {
                    "key": key,
                    "healthy": bool(health.get("healthy")),
                    "detail": str(health.get("detail") or ""),
                    "runtime_version": str(health.get("runtime_version") or ""),
                    "version_check": dict(health.get("version_check") or {}),
                    "auth_status": str((health.get("auth") or {}).get("status") or ""),
                    "probed_at": str(health.get("probed_at") or ""),
                    "capabilities": dict(health.get("capabilities") or {}),
                    "capability_injection": dict(
                        health.get("capability_injection") or {}),
                    "degradations": list(health.get("degradations") or []),
                    **({"credential_id": credential_id, "environment": environment,
                        "model_catalog": catalog} if credential_id else {}),
                })])

        return CommandPlan(
            events=[_event(command, "core.runtime.instance.probe_requested", key, {
                "key": key})],
            receipt=CommandReceipt(
                command_id=command.command_id,
                state=ReceiptState.ACCEPTED,
                aggregate=AggregateRef(type="runtime", id=key)),
            side_effect=_probe,
        )

    def _plan_refresh(self, command: CommandEnvelope) -> CommandPlan:
        adapter_id = canonical_adapter_id(str(command.payload.get("adapter_id") or ""))
        if adapter_id:
            _ensure_command_engine_supported(command, adapter_id)
        transport_kind = str(
            command.payload.get("transport_kind")
            or command.payload.get("transport")
            or ""
        ).strip().lower()
        if adapter_id and not engine_of_adapter(adapter_id):
            raise CommandFailed(make_error(
                "runtime.instance.invalid",
                f"未知 Runtime adapter：{adapter_id!r}",
                ErrorCategory.VALIDATION,
                correlation_id=correlation_id_of(command)))
        if transport_kind not in {"", "cli", "structured"}:
            raise CommandFailed(make_error(
                "runtime.instance.invalid",
                f"未知 Runtime transport：{transport_kind!r}",
                ErrorCategory.VALIDATION,
                correlation_id=correlation_id_of(command)))
        scope = adapter_id or transport_kind or "all"
        service = self._service

        async def _refresh() -> SideEffectResult:
            summary = await service.probe_all(
                adapter_id or None,
                transport_kind=transport_kind,
                force_version_check=True,
            )
            failed = [r for r in summary["results"] if not r["ok"]]
            return SideEffectResult(
                events=[_event(command, "core.runtime.instance.refresh_completed", scope, {
                    "scope": scope,
                    "transport_kind": transport_kind,
                    "total": summary["total"],
                    "failed": summary["failed"],
                    "results": summary["results"],
                })],
                # 部分实例失败不回失败回执：故障隔离是产品语义（任务书 3.1.6）。
                state=ReceiptState.COMPLETED,
                error=None if not failed or len(failed) < summary["total"] else make_error(
                    "runtime.instance.refresh_failed",
                    "全部实例 probe 失败",
                    ErrorCategory.INTERNAL,
                    correlation_id=correlation_id_of(command),
                    retryable=True),
            )

        return CommandPlan(
            events=[_event(command, "core.runtime.instance.refresh_requested", scope, {
                "scope": scope,
                "transport_kind": transport_kind,
            })],
            receipt=CommandReceipt(
                command_id=command.command_id,
                state=ReceiptState.ACCEPTED,
                aggregate=AggregateRef(type="runtime", id=scope)),
            side_effect=_refresh,
        )

    def _plan_auth(self, command: CommandEnvelope) -> CommandPlan:
        adapter_id, instance_id = _instance_key_of(command)
        _ensure_command_engine_supported(command, adapter_id)
        key = f"{adapter_id}:{instance_id}"
        view = self._service.get_instance(adapter_id, instance_id)
        if view is None:
            raise CommandFailed(make_error(
                "runtime.instance.not_found",
                f"unknown runtime instance: {key}",
                ErrorCategory.NOT_FOUND,
                correlation_id=correlation_id_of(command),
            ))
        action = command.command_type.rsplit(".", 1)[-1]
        service = self._service

        async def _apply_auth() -> SideEffectResult:
            cfg = service.store.get(adapter_id, instance_id)
            credential_ref = str(command.payload.get("credential_ref") or "")
            if action == "login" and credential_ref:
                return SideEffectResult(
                    state=ReceiptState.FAILED,
                    error=make_error(
                        "runtime.instance.credential_moved",
                        "Runtime 不再保存凭据；请在统一凭据中心登记账号，并让 "
                        "Worker 或 Thread 保存 credential_id",
                        ErrorCategory.VALIDATION,
                        correlation_id=correlation_id_of(command),
                    ),
                )
            elif action == "logout" and cfg is not None:
                cfg = cfg.__class__(**{
                    **cfg.__dict__,
                    "credential_ref": "",
                    "updated_at": utcnow().isoformat(),
                })
                service.store.upsert(cfg)
                service.apply_live_config(cfg)
            auth = service._auth_view(
                engine_of_adapter(adapter_id),
                str(cfg.credential_ref if cfg is not None else ""),
            )
            health = service.store.health(key)
            if health is not None:
                service.store.save_health(key, {**health, "auth": auth})
            output = {
                "instance": key,
                "auth": auth,
                "credential_detached": action == "logout",
            }
            if action == "login" and auth.get("status") != "ok":
                error = make_error(
                    "runtime.instance.login_interaction_required",
                    str(auth.get("detail") or "Runtime login is required"),
                    ErrorCategory.STATE,
                    correlation_id=correlation_id_of(command),
                    recovery_hint=str(auth.get("login_command") or "complete Runtime login"),
                ).model_copy(update={"detail": {"auth": auth}})
                return SideEffectResult(
                    state=ReceiptState.FAILED, error=error, output=output)
            return SideEffectResult(output=output)

        return CommandPlan(
            events=[_event(
                command,
                f"core.runtime.instance.{action.replace('.', '_')}_requested",
                key,
                {"key": key, "action": action},
            )],
            receipt=CommandReceipt(
                command_id=command.command_id,
                state=ReceiptState.ACCEPTED,
                aggregate=AggregateRef(type="runtime", id=key),
            ),
            side_effect=_apply_auth,
        )


class WorkerProfileCommandHandler:
    """``worker_profile.*`` 命令命名空间的属主（RUNTIME-05）。"""

    command_types = {"worker_profile.upsert", "worker_profile.remove"}

    def __init__(self, service: AgentRuntimeService) -> None:
        self._service = service

    async def plan(self, command: CommandEnvelope, ctx: HandlerContext) -> CommandPlan:
        ct = command.command_type
        if ct == "worker_profile.upsert":
            return self._plan_upsert(command)
        return self._plan_remove(command)

    def _plan_upsert(self, command: CommandEnvelope) -> CommandPlan:
        payload = dict(command.payload.get("profile") or command.payload)
        paused_engine = temporarily_disabled_engine_in_payload(payload)
        if paused_engine:
            _ensure_command_engine_supported(command, paused_engine)
        # 计划阶段只做校验（normalize + instance 引用存在性），不接触 Store。
        try:
            profile = normalize_worker_profile(payload, reject_invalid=True)
            if profile is not None:
                profile["runtime_instance_ref"] = ""
        except ValueError as exc:
            raise CommandFailed(make_error(
                "worker_profile.invalid", str(exc), ErrorCategory.VALIDATION,
                correlation_id=correlation_id_of(command))) from exc
        assert profile is not None
        name = profile["name"]
        service = self._service

        async def _save() -> SideEffectResult:
            try:
                service.upsert_worker_profile(payload)
            except ValueError as exc:
                return SideEffectResult(
                    error=make_error(
                        "worker_profile.invalid", str(exc),
                        ErrorCategory.VALIDATION,
                        correlation_id=correlation_id_of(command)),
                    state=ReceiptState.FAILED)
            return SideEffectResult()

        return CommandPlan(
            events=[_event(command, "core.worker_profile.upserted", name, {
                "name": name,
                "engine": profile["engine"],
                "runtime_instance_ref": profile.get("runtime_instance_ref") or "",
                "credential_seat_ref": profile.get("credential_seat_ref") or "",
                "credential_account": profile.get("credential_account") or "",
            })],
            receipt=CommandReceipt(
                command_id=command.command_id,
                state=ReceiptState.ACCEPTED,
                aggregate=AggregateRef(type="profile", id=name)),
            side_effect=_save,
        )

    def _plan_remove(self, command: CommandEnvelope) -> CommandPlan:
        name = str(
            command.payload.get("name") or command.aggregate_id or "").strip()
        if not name:
            raise CommandFailed(make_error(
                "worker_profile.invalid",
                "worker_profile.remove requires payload.name",
                ErrorCategory.VALIDATION,
                correlation_id=correlation_id_of(command)))
        service = self._service

        async def _remove() -> SideEffectResult:
            try:
                service.remove_worker_profile(name)
            except KeyError as exc:
                return SideEffectResult(
                    error=make_error(
                        "worker_profile.not_found", str(exc),
                        ErrorCategory.NOT_FOUND,
                        correlation_id=correlation_id_of(command)),
                    state=ReceiptState.FAILED)
            except ValueError as exc:
                return SideEffectResult(
                    error=make_error(
                        "worker_profile.invalid", str(exc),
                        ErrorCategory.VALIDATION,
                        correlation_id=correlation_id_of(command)),
                    state=ReceiptState.FAILED)
            return SideEffectResult()

        return CommandPlan(
            events=[_event(command, "core.worker_profile.removed", name, {"name": name})],
            receipt=CommandReceipt(
                command_id=command.command_id,
                state=ReceiptState.ACCEPTED,
                aggregate=AggregateRef(type="profile", id=name)),
            side_effect=_remove,
        )


class WorkerSettingsCommandHandler:
    """旧 Worker 设置表单的持久化入口，同样经过 Command API。"""

    command_types = {"worker_settings.update", "worker_identity.update"}

    def __init__(self, service: AgentRuntimeService) -> None:
        self._service = service

    async def plan(self, command: CommandEnvelope, ctx: HandlerContext) -> CommandPlan:
        config_store = self._service.worker_config
        if config_store is None:
            raise CommandFailed(make_error(
                "worker_settings.unavailable", "worker settings store 不可用",
                ErrorCategory.INTERNAL,
                correlation_id=correlation_id_of(command)))
        payload = dict(command.payload)
        paused_engine = temporarily_disabled_engine_in_payload(payload)
        if paused_engine:
            _ensure_command_engine_supported(command, paused_engine)
        command_type = command.command_type

        async def _save() -> SideEffectResult:
            try:
                if command_type == "worker_identity.update":
                    config = config_store.set_identity_model(
                        seats=payload.get("seats"),
                        credentials=payload.get("credentials"),
                        worker_backend=payload.get("worker_backend"),
                        worker_network=payload.get("worker_network"))
                else:
                    allowed = {
                        "engines", "start_workers", "max_workers",
                        "worker_backend", "worker_network", "worker_container_scope", "worker_privilege",
                        "worker_memory", "worker_cpus", "worker_pids_limit",
                        "worker_output_limit", "worker_disk_limit",
                        "worker_vpn_enabled", "race_scout",
                        "race_timeout", "wall_clock_budget", "race_engines",
                        "max_total_workers", "cost_budget_usd", "stage_policy",
                        "llm_profiles", "worker_profiles", "overrides",
                    }
                    settings = {
                        key: value for key, value in payload.items()
                        if key in allowed
                    }
                    if "seats" in payload or "credentials" in payload:
                        config = config_store.set_configuration(
                            seats=payload.get("seats"),
                            credentials=payload.get("credentials"),
                            **settings,
                        )
                    else:
                        config = config_store.set(**settings)
            except ValueError as exc:
                return SideEffectResult(
                    error=make_error(
                        "worker_settings.invalid", str(exc),
                        ErrorCategory.VALIDATION,
                        correlation_id=correlation_id_of(command)),
                    state=ReceiptState.FAILED)
            return SideEffectResult(output={"config": config})

        return CommandPlan(
            events=[_event(
                command,
                ("core.worker.identity.updated"
                 if command_type == "worker_identity.update"
                 else "core.worker.settings.updated"),
                "global",
                {"fields": sorted(payload)},
            )],
            receipt=CommandReceipt(
                command_id=command.command_id,
                state=ReceiptState.ACCEPTED,
                aggregate=AggregateRef(type="worker_settings", id="global")),
            side_effect=_save,
        )


class RuntimeInstanceQueryHandler:
    """Runtime instance 只读查询（management 工具与设置页共用）。"""

    query_types = {"runtime.instance.list"}

    def __init__(self, service: AgentRuntimeService) -> None:
        self._service = service

    async def handle(self, query: Any, ctx: HandlerContext) -> QueryResult:
        adapter_id = str(query.params.get("adapter_id") or "").strip()
        include_discovered = bool(query.params.get("include_discovered", True))
        transport_kind = str(
            query.params.get("transport_kind")
            or query.params.get("transport")
            or ""
        ).strip().lower()
        instances = self._service.list_instances(
            canonical_adapter_id(adapter_id) or None,
            include_discovered=include_discovered,
            transport_kind=transport_kind,
        )
        probed_at = max(
            (
                str((item.get("health") or {}).get("probed_at") or "")
                for item in instances
                if isinstance(item.get("health"), dict)
            ),
            default="",
        )
        return QueryResult(
            query_id=query.query_id,
            query_type=query.query_type,
            result={
                "instances": instances,
                "count": len(instances),
                "probed_at": probed_at,
                "transport_kind": transport_kind,
            },
        )


class WorkerProfileQueryHandler:
    """Worker Profile 只读查询（含 Runtime instance 确定映射）。"""

    query_types = {"worker_profile.list"}

    def __init__(self, service: AgentRuntimeService) -> None:
        self._service = service

    async def handle(self, query: Any, ctx: HandlerContext) -> QueryResult:
        profiles = self._service.list_worker_profiles()
        return QueryResult(
            query_id=query.query_id,
            query_type=query.query_type,
            result={"profiles": profiles, "count": len(profiles)},
        )


def register_agent_runtime_handlers(
    api: Any,
    service: AgentRuntimeService,
) -> None:
    """把 RUNTIME-05 的 Command/Query Handler 注册到共享 Command API。

    幂等：已注册过同名命名空间时跳过（INTEG-01 与 router 工厂可能都调用）。
    """
    known_commands = api.handlers.known_command_types()
    known_queries = api.handlers.known_query_types()
    if "runtime.instance.upsert" not in known_commands:
        api.register_command(RuntimeInstanceCommandHandler(service))
    if "worker_profile.upsert" not in known_commands:
        api.register_command(WorkerProfileCommandHandler(service))
    if "worker_settings.update" not in known_commands:
        api.register_command(WorkerSettingsCommandHandler(service))
    if "runtime.instance.list" not in known_queries:
        api.register_query(RuntimeInstanceQueryHandler(service))
    if "worker_profile.list" not in known_queries:
        api.register_query(WorkerProfileQueryHandler(service))


# ---------------------------------------------------------------------------
# FastAPI router 工厂（供 INTEG-01 挂载到 apps/web/server.py）
# ---------------------------------------------------------------------------


def _receipt_status(receipt: CommandReceipt) -> int:
    """回执终态 → HTTP 状态码。"""
    if receipt.state is ReceiptState.COMPLETED:
        return 200
    if receipt.state is ReceiptState.CONFLICT:
        return 409
    category = getattr(getattr(receipt, "error", None), "category", None)
    if category is ErrorCategory.NOT_FOUND:
        return 404
    if category is ErrorCategory.VALIDATION:
        return 400
    if category is ErrorCategory.STATE:
        return 409
    return 502


def _receipt_body(receipt: CommandReceipt) -> dict[str, Any]:
    return {"receipt": receipt.model_dump(mode="json")}


def create_agent_runtime_router(
    *,
    command_api: Any,
    service: AgentRuntimeService,
    actor: ActorRef = OPERATOR,
    register_handlers: bool = True,
) -> Any:
    """构造 Agent Runtime 设置 API router。

    所有状态修改经 ``command_api.dispatch``（产生 CommandReceipt），查询经
    ``command_api.query``；本模块不直接写配置 Store。``register_handlers``
    为 True 时把 RUNTIME-05 Handler 注册到该 Command API（幂等）。
    """
    from fastapi import APIRouter, Body
    from fastapi.responses import JSONResponse

    if register_handlers:
        register_agent_runtime_handlers(command_api, service)

    router = APIRouter(tags=["agent-runtimes"])

    def _command(
        command_type: str,
        aggregate_type: str,
        aggregate_id: str,
        payload: dict[str, Any],
        *,
        command_id: str = "",
        idempotency_key: str = "",
    ) -> CommandEnvelope:
        fields: dict[str, Any] = {
            "command_type": command_type,
            "aggregate_type": aggregate_type,
            "aggregate_id": aggregate_id,
            "actor": actor,
            "payload": payload,
        }
        if command_id:
            fields["command_id"] = command_id
        if idempotency_key:
            fields["idempotency_key"] = idempotency_key
        return CommandEnvelope(**fields)

    async def _dispatch(envelope: CommandEnvelope) -> JSONResponse:
        receipt = await command_api.dispatch(envelope)
        return JSONResponse(_receipt_body(receipt), status_code=_receipt_status(receipt))

    def _parse_key(instance_key: str) -> tuple[str, str]:
        """``adapter:instance`` 或 bare instance_id（需唯一匹配已配置实例）。"""
        ref = parse_runtime_instance_ref(instance_key)
        if ref is not None:
            return ref
        instance_id = str(instance_key or "").strip()
        matches = [
            cfg for cfg in service.store.list() if cfg.instance_id == instance_id
        ]
        if len(matches) == 1:
            return matches[0].adapter_id, matches[0].instance_id
        return "", instance_id

    # -- 查询 ---------------------------------------------------------------

    @router.get("/api/agent-runtimes")
    async def list_runtimes(
        adapter_id: str = "",
        discovered: bool = True,
        transport: str = "",
    ) -> Any:
        result = await command_api.query(_query(
            "runtime.instance.list",
            adapter_id=adapter_id,
            include_discovered=discovered,
            transport_kind=transport,
        ))
        return result.result

    @router.get("/api/agent-runtimes/profiles")
    async def list_worker_profiles() -> Any:
        result = await command_api.query(_query("worker_profile.list"))
        return result.result

    @router.get("/api/agent-runtimes/{instance_key}")
    async def get_runtime(instance_key: str) -> Any:
        adapter_id, instance_id = _parse_key(instance_key)
        view = service.get_instance(adapter_id, instance_id) if adapter_id else None
        if view is None:
            error = ErrorEnvelope(
                code="runtime.instance.not_found",
                message=f"unknown runtime instance: {instance_key}",
                category=ErrorCategory.NOT_FOUND,
            )
            return JSONResponse(
                {"error": error.model_dump(mode="json")},
                status_code=404)
        return view

    # -- 状态修改（全部经 Command API） ----------------------------------------

    @router.post("/api/agent-runtimes")
    async def upsert_runtime(body: dict[str, Any] = Body(...)) -> Any:
        command_id = str(body.pop("command_id", "") or "")
        idem = str(body.pop("idempotency_key", "") or "")
        adapter_id = canonical_adapter_id(str(body.get("adapter_id") or ""))
        instance_id = str(body.get("instance_id") or "default").strip() or "default"
        return await _dispatch(_command(
            "runtime.instance.upsert", "runtime", f"{adapter_id}:{instance_id}",
            dict(body), command_id=command_id, idempotency_key=idem))

    @router.delete("/api/agent-runtimes/{instance_key}")
    async def remove_runtime(instance_key: str) -> Any:
        adapter_id, instance_id = _parse_key(instance_key)
        return await _dispatch(_command(
            "runtime.instance.remove", "runtime", f"{adapter_id}:{instance_id}",
            {"adapter_id": adapter_id, "instance_id": instance_id}))

    @router.post("/api/agent-runtimes/{instance_key}/probe")
    async def probe_runtime(instance_key: str, body: dict[str, Any] = Body(default={})) -> Any:
        adapter_id, instance_id = _parse_key(instance_key)
        key = f"{adapter_id}:{instance_id}"
        response = await _dispatch(_command(
            "runtime.instance.probe", "runtime", key,
            {"adapter_id": adapter_id, "instance_id": instance_id,
             "credential_id": body.get("credential_id", ""), "environment": body.get("environment", "local")}))
        body = json.loads(response.body)
        # probe 完成后把最新健康快照一并返回，前端无需二次请求。
        view = service.get_instance(adapter_id, instance_id) if adapter_id else None
        if view is not None:
            body["instance"] = view
        return JSONResponse(body, status_code=response.status_code)

    @router.post("/api/agent-runtimes/{instance_key}/login")
    async def login_runtime(
        instance_key: str, body: dict[str, Any] = Body(default={})
    ) -> Any:
        adapter_id, instance_id = _parse_key(instance_key)
        return await _dispatch(_command(
            "runtime.instance.login", "runtime",
            f"{adapter_id}:{instance_id}",
            {
                "adapter_id": adapter_id,
                "instance_id": instance_id,
                "credential_ref": str((body or {}).get("credential_ref") or ""),
            },
        ))

    @router.post("/api/agent-runtimes/{instance_key}/logout")
    async def logout_runtime(instance_key: str) -> Any:
        adapter_id, instance_id = _parse_key(instance_key)
        return await _dispatch(_command(
            "runtime.instance.logout", "runtime",
            f"{adapter_id}:{instance_id}",
            {"adapter_id": adapter_id, "instance_id": instance_id},
        ))

    @router.post("/api/agent-runtimes/{instance_key}/auth-refresh")
    async def refresh_runtime_auth(instance_key: str) -> Any:
        adapter_id, instance_id = _parse_key(instance_key)
        return await _dispatch(_command(
            "runtime.instance.auth.refresh", "runtime",
            f"{adapter_id}:{instance_id}",
            {"adapter_id": adapter_id, "instance_id": instance_id},
        ))

    @router.post("/api/agent-runtimes/{instance_key}/capability-refresh")
    async def refresh_runtime_capabilities(instance_key: str) -> Any:
        adapter_id, instance_id = _parse_key(instance_key)
        return await _dispatch(_command(
            "runtime.instance.capability.refresh", "runtime",
            f"{adapter_id}:{instance_id}",
            {"adapter_id": adapter_id, "instance_id": instance_id},
        ))

    @router.post("/api/agent-runtimes/refresh")
    async def refresh_runtimes(body: dict[str, Any] = Body(default={})) -> Any:
        adapter_id = canonical_adapter_id(str((body or {}).get("adapter_id") or ""))
        transport_kind = str(
            (body or {}).get("transport_kind")
            or (body or {}).get("transport")
            or ""
        ).strip().lower()
        return await _dispatch(_command(
            "runtime.instance.refresh", "runtime",
            adapter_id or transport_kind or "all",
            {
                "adapter_id": adapter_id,
                "transport_kind": transport_kind,
            }))

    # -- Worker Profile 管理（经 Command API） --------------------------------

    @router.post("/api/agent-runtimes/profiles")
    async def upsert_profile(body: dict[str, Any] = Body(...)) -> Any:
        command_id = str(body.pop("command_id", "") or "")
        idem = str(body.pop("idempotency_key", "") or "")
        name = str(body.get("name") or body.get("id") or "").strip()
        return await _dispatch(_command(
            "worker_profile.upsert", "profile", name, {"profile": dict(body)},
            command_id=command_id, idempotency_key=idem))

    @router.delete("/api/agent-runtimes/profiles/{name}")
    async def remove_profile(name: str) -> Any:
        return await _dispatch(_command(
            "worker_profile.remove", "profile", name, {"name": name}))

    def _query(query_type: str, **params: Any) -> Any:
        from muteki.platform.contracts.commands import QueryEnvelope

        return QueryEnvelope(
            query_type=query_type,
            aggregate_type="runtime" if query_type.startswith("runtime.") else "profile",
            actor=actor,
            params={k: v for k, v in params.items() if v not in (None, "")},
        )

    return router


__all__ = [
    "AgentRuntimeService",
    "LOGIN_GUIDANCE",
    "OPERATOR",
    "PRODUCER",
    "RuntimeInstanceCommandHandler",
    "RuntimeInstanceConfig",
    "RuntimeInstanceConfigStore",
    "RuntimeInstanceQueryHandler",
    "WorkerProfileCommandHandler",
    "WorkerProfileQueryHandler",
    "canonical_adapter_id",
    "create_agent_runtime_router",
    "engine_of_adapter",
    "register_agent_runtime_handlers",
]
