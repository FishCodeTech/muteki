"""WorkerProfile normalization shared by the web config and swarm scheduler.

Profiles are the scheduling unit.  ``profile["name"]`` is what the coordinator
selects; ``profile["engine"]`` is the concrete CLI transport family.
"""

from __future__ import annotations

from typing import Any
from urllib.parse import urlparse

from muteki.solver.engine_registry import (
    KNOWN_ENGINE_IDS,
    SUPPORTED_ENGINE_IDS,
    EngineTemporarilyUnsupportedError,
    canonical_engine_id,
)
from muteki.solver.credential_accounts import (
    account_credential_id,
    account_id_from_credential_id,
    canonical_credential_id,
    system_credential_id,
)


KNOWN_BASE_ENGINES = KNOWN_ENGINE_IDS
VALID_BASE_ENGINES = SUPPORTED_ENGINE_IDS
CONTAINER_BASE_ENGINES = SUPPORTED_ENGINE_IDS
VALID_REASONING_EFFORTS = (
    "default", "none", "minimal", "low", "medium", "high", "xhigh", "max",
)
TRANSPORT_TO_ENGINE = {
    "claude": "claude",
    "claude_code": "claude",
    "codex": "codex",
    "codex_cli": "codex",
    "cursor": "cursor",
    "cursor_agent": "cursor",
    "pi": "pi",
    "omp": "omp",
    "oh_my_pi": "omp",
    "oh-my-pi": "omp",
    "ohmypi": "omp",
    "kimi": "kimi",
    "kimi_code": "kimi",
    "kimi-code": "kimi",
    "grok": "grok",
    "grok_build": "grok",
    "grok-build": "grok",
    "opencode": "opencode",
    "opencode_cli": "opencode",
    "opencode-cli": "opencode",
    "devin": "devin",
    "devin_cli": "devin",
    "dsh": "dsh",
    "deepseek_harness": "dsh",
    "deepseek-harness": "dsh",
    "dsh_sdk_worker": "dsh",
    "dsh-sdk-worker": "dsh",
}
DEFAULT_ROLES = ["race", "bootstrap", "explore", "respond", "review", "verifier"]


def coerce_nonneg_int(value: Any, default: int) -> int:
    try:
        n = int(value)
    except (TypeError, ValueError):
        return default
    return n if n >= 0 else default


def coerce_pos_int(value: Any, default: int) -> int:
    try:
        n = int(value)
    except (TypeError, ValueError):
        return default
    return n if n > 0 else default


def normalize_reasoning_effort(value: Any, default: str = "default") -> str:
    """Normalize the persisted cross-engine reasoning-effort value."""
    effort = str(value or "").strip().lower()
    return effort if effort in VALID_REASONING_EFFORTS else default


def base_engine_for_profile(profile_or_name: Any) -> str:
    """Resolve a profile dict OR a bare string to one of the nine base engines.

    A bare string may be a base engine ("codex"), a transport ("codex_cli"), or a
    PROFILE ID ("codex-sub-container"). Profile ids are "<base>-<suffix>", so when a
    string is neither a known base nor transport we recover the base from its segments
    (the first segment that is a valid base engine). This is what keeps a profile id
    from being passed straight to DRIVERS[...] (→ KeyError) downstream. The original
    string is returned only when nothing resolves, so callers can still error clearly.
    """
    if isinstance(profile_or_name, dict):
        transport = str(profile_or_name.get("transport") or "").strip()
        engine = str(profile_or_name.get("engine") or "").strip()
        return TRANSPORT_TO_ENGINE.get(transport, engine)
    s = str(profile_or_name or "").strip()
    if s in TRANSPORT_TO_ENGINE:
        return TRANSPORT_TO_ENGINE[s]
    if s in KNOWN_BASE_ENGINES:
        return s
    # profile id like "codex-sub-container" / "cursor-api-container" → recover base.
    for seg in s.split("-"):
        if seg in KNOWN_BASE_ENGINES:
            return seg
        if seg in TRANSPORT_TO_ENGINE:
            return TRANSPORT_TO_ENGINE[seg]
    return s


def normalize_worker_profile(item: dict[str, Any], *, reject_invalid: bool = False) -> dict[str, Any] | None:
    if not isinstance(item, dict):
        if reject_invalid:
            raise ValueError("worker profile must be an object")
        return None
    if not item.get("enabled", True):
        return None
    transport = str(item.get("transport") or item.get("engine") or "").strip()
    engine = TRANSPORT_TO_ENGINE.get(transport, str(item.get("engine") or "").strip())
    if canonical_engine_id(engine) == "dsh":
        if reject_invalid:
            raise EngineTemporarilyUnsupportedError("dsh")
        return None
    if engine not in VALID_BASE_ENGINES:
        if reject_invalid:
            raise ValueError("worker profile requires valid transport/engine")
        return None
    pid = str(item.get("name") or item.get("id") or "").strip()
    if not pid:
        if reject_invalid:
            raise ValueError("worker profile requires name or id")
        return None
    raw_roles = item.get("roles")
    roles = [
        str(r).strip()
        for r in raw_roles
        if isinstance(r, str) and str(r).strip()
    ] if isinstance(raw_roles, list) else []
    # review/verifier capability must be EXPLICIT. Roles that only land on an
    # ordinary profile through the default list or this auto-extension are
    # recorded in implicit_roles; the scheduler serves them only when the
    # operator opted into the review/verifier fallback policy flag.
    implicit_roles: list[str] = []
    if not roles:
        roles = list(DEFAULT_ROLES)
        implicit_roles = ["review", "verifier"]
    elif any(r in roles for r in ("race", "bootstrap", "explore", "respond")):
        if "review" not in roles:
            roles = [*roles, "review"]
            implicit_roles.append("review")
        if "verifier" not in roles:
            roles = [*roles, "verifier"]
            implicit_roles.append("verifier")
    credential_mode = str(
        item.get("credential_mode") or item.get("auth") or "subscription"
    ).strip() or "subscription"
    raw_credential_id = str(item.get("credential_id") or "").strip()
    if "credential_account" in item:
        raw_account = item.get("credential_account")
    elif "credential_account_ref" in item:
        raw_account = item.get("credential_account_ref")
    elif account_id_from_credential_id(raw_credential_id):
        raw_account = account_id_from_credential_id(raw_credential_id)
    elif raw_credential_id.startswith("system:"):
        raw_account = ""
    else:
        raw_account = f"{engine}-main"
    credential_account = str(raw_account or "").strip()
    try:
        credential_id = (
            canonical_credential_id(raw_credential_id, engine=engine)
            if raw_credential_id
            else account_credential_id(credential_account)
            if credential_account
            else system_credential_id(engine)
            if str(item.get("credential_kind") or "") == "system_inherit"
            else ""
        )
    except ValueError:
        if reject_invalid:
            raise ValueError("worker profile credential_id is invalid")
        credential_id = ""
    normalized = {
        "id": pid,
        "name": pid,
        # human-readable display name, carried through so a seat-id-based pid (post
        # identity migration) still renders a friendly name in the UI. Defaults to
        # the pid when no explicit label is given.
        "label": str(item.get("label") or pid).strip(),
        "engine": engine,
        "transport": transport or engine,
        "credential_mode": credential_mode,
        "auth": credential_mode,
        "credential_account": credential_account,
        "credential_id": credential_id,
        "api_key_ref": str(item.get("api_key_ref") or "").strip(),
        "base_url": str(item.get("base_url") or "").strip(),
        "wire_api": str(item.get("wire_api") or ("responses" if engine == "codex" else "")).strip(),
        "roles": roles,
        "implicit_roles": implicit_roles,
        "race": bool(item.get("race", "race" in roles)),
        "max_running": coerce_pos_int(item.get("max_running"), 1),
        # 0 means "inherit the global review.max_concurrent"; review capacity is
        # intentionally separate from max_running, which now only gates ordinary
        # race/bootstrap/explore/respond workers.
        "max_review_running": coerce_nonneg_int(item.get("max_review_running"), 0),
        "max_verifier_running": coerce_nonneg_int(item.get("max_verifier_running"), 0),
        "priority": coerce_nonneg_int(item.get("priority"), 100),
        "model": str(item.get("model") or "").strip(),
        "reasoning_effort": normalize_reasoning_effort(
            item.get("reasoning_effort"), "default"),
        # RUNTIME-05（任务书 7.3）：Profile 引用 Runtime instance 或 credential
        # seat 的确定映射。只保存引用（"adapter_id:instance_id" / seat id /
        # secret:// 句柄），不复制真实 Secret。空串表示未显式绑定，由
        # resolve_profile_runtime_instance 按 engine+账户推导。
        "runtime_instance_ref": str(item.get("runtime_instance_ref") or "").strip(),
        "credential_seat_ref": str(item.get("credential_seat_ref") or "").strip(),
        "enabled": True,
    }
    return normalized


def normalize_worker_profiles(value: Any, *, defaults: list[dict[str, Any]] | None = None,
                              reject_invalid: bool = False) -> list[dict[str, Any]]:
    if value is None:
        return [dict(p) for p in (defaults or [])]
    if not isinstance(value, list):
        if reject_invalid:
            raise ValueError("worker_profiles must be a list")
        return [dict(p) for p in (defaults or [])]
    out: list[dict[str, Any]] = []
    seen: set[str] = set()
    for item in value:
        profile = normalize_worker_profile(item, reject_invalid=reject_invalid)
        if profile is None:
            continue
        if profile["name"] in seen:
            if reject_invalid:
                raise ValueError("worker profile names must be unique")
            continue
        seen.add(profile["name"])
        out.append(profile)
    return out or [dict(p) for p in (defaults or [])]


def profile_names(profiles: list[dict[str, Any]]) -> list[str]:
    return [str(p["name"]) for p in profiles if p.get("enabled", True)]


def normalize_profile_roster(values: Any, profiles: list[dict[str, Any]]) -> list[str]:
    """Map profile names and legacy base-engine names to profile-name roster.

    Unknown names are ignored. A legacy base engine expands to every matching
    profile in priority/name order.
    """

    if not isinstance(values, (list, tuple)):
        return []
    by_name = {str(p["name"]): p for p in profiles}
    by_engine: dict[str, list[str]] = {}
    # coerce_nonneg_int (NOT `priority or 100`): preserve a legal priority 0
    # (highest precedence) instead of silently demoting it to the default.
    for p in sorted(profiles, key=lambda p: (coerce_nonneg_int(p.get("priority"), 100), str(p["name"]))):
        by_engine.setdefault(str(p["engine"]), []).append(str(p["name"]))
    out: list[str] = []
    seen: set[str] = set()
    for raw in values:
        if not isinstance(raw, str):
            continue
        names = [raw] if raw in by_name else by_engine.get(raw, [])
        for name in names:
            if name not in seen:
                seen.add(name)
                out.append(name)
    return out


def profile_uses_endpoint(profile: dict[str, Any] | None) -> bool:
    if not profile:
        return False
    return bool(profile.get("base_url"))


_IDENTITY_ENV_KEYS: tuple[tuple[str, str], ...] = (
    ("profile_id", "MUTEKI_WORKER_PROFILE_ID"),
    ("profile_label", "MUTEKI_WORKER_PROFILE_LABEL"),
    ("model", "MUTEKI_WORKER_MODEL"),
    ("account_id", "MUTEKI_CREDENTIAL_ACCOUNT_ID"),
    ("endpoint_host", "MUTEKI_WORKER_ENDPOINT_HOST"),
    ("connection", "MUTEKI_WORKER_CONNECTION"),
    ("provider", "MUTEKI_WORKER_PROVIDER"),
)


def endpoint_host_from_url(url: Any) -> str:
    raw = str(url or "").strip()
    if not raw:
        return ""
    parsed = urlparse(raw if "://" in raw else f"https://{raw}")
    return str(parsed.hostname or "").strip()


def profile_connection_kind(profile: dict[str, Any] | None) -> str:
    if not profile:
        return ""
    if str(profile.get("base_url") or "").strip():
        return "custom_endpoint"
    if str(profile.get("credential_account") or "").strip():
        return "official"
    return "system"


def worker_identity_fields(profile: dict[str, Any] | None) -> dict[str, str]:
    """Operator-facing identity for a spawned worker. Never includes secrets."""
    if not isinstance(profile, dict):
        return {}
    pid = str(profile.get("id") or profile.get("name") or "").strip()
    label = str(profile.get("label") or "").strip() or pid
    model = str(profile.get("model") or "").strip()
    account_id = str(profile.get("credential_account") or "").strip()
    host = endpoint_host_from_url(profile.get("base_url"))
    provider = str(profile.get("provider") or "").strip()
    connection = profile_connection_kind(profile)
    out: dict[str, str] = {}
    if pid:
        out["profile_id"] = pid
    if label:
        out["profile_label"] = label
    if model:
        out["model"] = model
    if account_id:
        out["account_id"] = account_id
    if host:
        out["endpoint_host"] = host
    if connection:
        out["connection"] = connection
    if provider:
        out["provider"] = provider
    return out


def apply_worker_identity_env(
    env: dict[str, str], profile: dict[str, Any] | None,
) -> dict[str, str]:
    identity = worker_identity_fields(profile)
    for field, key in _IDENTITY_ENV_KEYS:
        value = identity.get(field)
        if value:
            env[key] = value
    return env


def worker_identity_from_env(env: dict[str, str] | None) -> dict[str, str]:
    mapping = {key: field for field, key in _IDENTITY_ENV_KEYS}
    out: dict[str, str] = {}
    for key, field in mapping.items():
        value = str((env or {}).get(key) or "").strip()
        if value:
            out[field] = value
    return out


def worker_identity_event_fields(worker: Any) -> dict[str, str]:
    identity = getattr(worker, "identity", None)
    if not isinstance(identity, dict):
        return {}
    return {str(key): str(value) for key, value in identity.items() if value}


def parse_runtime_instance_ref(ref: Any) -> tuple[str, str] | None:
    """解析 ``"adapter_id:instance_id"`` 引用；非法输入返回 None。

    instance 身份为 ``adapter_id + instance_id``（任务书 7.3）。bare
    adapter id（无冒号）不算 instance 引用，调用方应走 engine 推导。
    """
    text = str(ref or "").strip()
    if not text or ":" not in text:
        return None
    adapter_id, _, instance_id = text.partition(":")
    adapter_id = adapter_id.strip()
    instance_id = instance_id.strip()
    if not adapter_id or not instance_id:
        return None
    return adapter_id, instance_id


def resolve_profile_runtime_instance(
    profile: dict[str, Any] | None,
    instances: list[dict[str, Any]],
) -> dict[str, Any] | None:
    """Worker Profile → CLI instance 的唯一确定映射。

    ``instances`` 为 instance 配置视图（dict，至少含 ``adapter_id`` /
    ``instance_id`` / ``engine`` / ``credential_account`` / ``enabled``）。
    判定顺序固定，任何调用方结果一致：

    1. Profile 显式 ``runtime_instance_ref``（``adapter:instance``）精确匹配；
    2. Profile ``credential_seat_ref`` 匹配 instance 的 seat 引用；
    3. engine 相同且 instance 的 ``credential_account`` 等于 Profile 的
       ``credential_account``；
    4. 该 engine 有唯一 ``default_for_engine`` CLI 实例时取它；
    5. 该 engine 只有一个启用 instance 时取它；
    6. 其余（无匹配 / 多实例歧义）返回 None，由调用方显式报错或回退，
       不静默挑选。
    """
    if not isinstance(profile, dict):
        return None
    # Worker 只允许已注册的 CLI。Conversation 的 ACP、App Server、SDK instance
    # 即使出现在同一个设置存储中，也不进入这里的候选集合。
    rows = [
        i for i in instances
        if isinstance(i, dict) and i.get("adapter_id") and i.get("instance_id")
        and str(i.get("adapter_id") or "").startswith("cli.")
    ]
    if not rows:
        return None

    def _key(item: dict[str, Any]) -> str:
        return f"{item['adapter_id']}:{item['instance_id']}"

    def _healthy_for_default(item: dict[str, Any]) -> bool:
        # 显式引用仍返回具体实例并由调用方显示故障；自动默认只能使用已
        # 完成最低 probe 的实例。旧调用方未提供 health 字段时保持兼容。
        if "health" not in item:
            return True
        health = item.get("health")
        return isinstance(health, dict) and bool(health.get("healthy"))

    # 1. 显式引用优先（含指向已停用 instance：返回给调用方自行判定）。
    ref = parse_runtime_instance_ref(profile.get("runtime_instance_ref"))
    if ref is not None:
        want = f"{ref[0]}:{ref[1]}"
        for item in rows:
            if _key(item) == want:
                return item
        return None

    # 2. credential seat 引用。
    seat_ref = str(profile.get("credential_seat_ref") or "").strip()
    if seat_ref:
        for item in rows:
            if str(item.get("credential_seat_ref") or "").strip() == seat_ref:
                return item
        return None

    engine = base_engine_for_profile(profile)
    engine_rows = [
        i for i in rows
        if str(i.get("engine") or "").strip() == engine
        or str(i.get("adapter_id") or "").strip() in (engine, f"cli.{engine}")
    ]

    # 3. engine + 账户引用精确匹配。
    account = str(profile.get("credential_account") or "").strip()
    if account:
        matched = [
            i for i in engine_rows
            if str(i.get("credential_account") or "").strip() == account
        ]
        if len(matched) == 1:
            return matched[0]
        if len(matched) > 1:
            return None  # 同账户多实例歧义，不静默挑选

    # 4. 产品明确指定的 CLI 默认实例。
    defaults = [
        i for i in engine_rows
        if i.get("enabled", True) and i.get("default_for_engine", False)
        and _healthy_for_default(i)
    ]
    if len(defaults) == 1:
        return defaults[0]
    if len(defaults) > 1:
        return None

    # 5. 该 engine 唯一启用 instance。
    enabled = [
        i for i in engine_rows
        if i.get("enabled", True) and _healthy_for_default(i)
    ]
    if len(enabled) == 1:
        return enabled[0]
    return None


def resolve_seat_ref(
    ref: Any,
    *,
    seats: list[dict[str, Any]],
    alias_table: dict[str, str] | None = None,
) -> str | None:
    """THE single seat-reference resolver (plan §5.0(b)).

    A foreign key in config (engines[]/review.engine/race_engines/...) may name a
    seat THREE ways:
      - a new seat id (`seat_claude_ab12cd`),
      - a legacy profile name (`claude-local`),
      - a legacy hyphen "canonical" alias (`claude-api-local`, from the old
        worker_config._canonical_profile_id), OR a bare base engine (`claude`).
    All four must resolve to the new seat id. Shared by worker_config / drivers /
    server / swarm so they can never disagree.

    Returns the matched seat id; None when nothing matches (caller decides the
    fallback — NEVER silently swallowed) or when a bare engine is ambiguous across
    multiple seats (None + the caller can expand via the engine fan-out instead).
    """
    if not isinstance(ref, str) or not ref.strip():
        return None
    ref = ref.strip()
    by_id = {str(s.get("id")): s for s in seats if isinstance(s, dict) and s.get("id")}
    if ref in by_id:
        return ref
    alias_table = alias_table or {}
    if ref in alias_table and alias_table[ref] in by_id:
        return alias_table[ref]
    # label match (legacy name kept as the seat label).
    by_label = {str(s.get("label")): str(s.get("id")) for s in seats
                if isinstance(s, dict) and s.get("label")}
    if ref in by_label:
        return by_label[ref]
    # bare base engine → resolve ONLY if exactly one seat for that engine (else
    # ambiguous: the caller should fan out across the engine's seats instead).
    if ref in VALID_BASE_ENGINES:
        matches = [str(s["id"]) for s in seats
                   if isinstance(s, dict) and str(s.get("engine")) == ref and s.get("id")]
        return matches[0] if len(matches) == 1 else None
    return None
