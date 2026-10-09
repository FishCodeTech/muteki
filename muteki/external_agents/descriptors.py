"""One typed descriptor per Agent engine.

The descriptor is the single static source of engine knowledge outside the
adapter implementations: identity and adapter ids, the capabilities each
integration implements, login guidance, model discovery, native home layout,
attachment wire format, CLI argv conventions, Runtime settings fields and the
Muteki command catalog.  Values mirror the current adapter and driver code;
implementations stay where they are and are selected through the typed keys
declared here.

Probe results remain the runtime truth for a concrete installation.
``resolve_capabilities`` overlays a probe on the declared baseline and labels
every field with its source.

This module must stay import-light: ``muteki.solver`` and Web modules import
it lazily, and it must not import adapter implementations.
"""

from __future__ import annotations

from typing import Any, Literal, Optional

from pydantic import ConfigDict, Field

from muteki.external_agents.capabilities import BOOL_CAPABILITY_FIELDS, SOURCE_STATIC
from muteki.platform.contracts.base import ContractModel
from muteki.platform.contracts.external_agents import (
    ACCESS_MODE_VALUES,
    AccessMode,
    AgentCapabilities,
)
from muteki.solver.engine_registry import ENGINE_DESCRIPTOR_BY_ID


DESCRIPTOR_SCHEMA_VERSION = 1
GROK_MCP_READY_WAIT_SECONDS = 8.0

_PROBE_LIST_FIELDS: tuple[str, ...] = (
    "supported_models", "supported_efforts", "permission_modes", "sandbox_modes",
)
_PROBE_SCALAR_FIELDS: tuple[str, ...] = ("protocol_version", "runtime_version", "plan_mode_per_turn", "resume_continues_turn")


class UnknownProviderError(ValueError):
    """An engine or adapter id has no provider descriptor."""

    code = "provider.descriptor_unknown"

    def __init__(self, value: str, *, kind: str = "engine") -> None:
        self.value = str(value or "")
        self.kind = kind
        super().__init__(f"no provider descriptor for {kind} {self.value!r}")


class _Frozen(ContractModel):
    model_config = ConfigDict(extra="forbid", frozen=True)


TransportKind = Literal["rpc", "sdk", "acp", "http", "http+ws", "cli"]


class TransportSetting(_Frozen):
    """One field of an adapter's ``transport`` settings object."""

    name: str
    type: Literal["boolean", "string", "number"]
    title: str = ""
    description: str = ""
    default: Any = None
    minimum: Optional[float] = None
    maximum: Optional[float] = None


class AdapterDescriptor(_Frozen):
    """One transport (adapter id) that runs this engine."""

    adapter_id: str
    role: Literal["default", "variant", "cli"]
    transport_kind: TransportKind
    legacy_aliases: tuple[str, ...] = ()
    # Integration-level baseline: what the adapter code implements natively.
    # A field stays False when the code has no path for it, even if the
    # engine itself might support it elsewhere.
    capabilities: AgentCapabilities
    # Native rewind of the engine session (not Muteki's history rebuild).
    native_rewind: bool = False
    # Adapter accepts Muteki capability-gateway / plugin injection.
    capability_gateway: bool = True
    # Why each access mode outside ``capabilities.access_modes`` is not
    # offered: rejected at session start or accepted with weaker semantics.
    access_mode_notes: dict[str, str] = Field(default_factory=dict)
    # ``engine_bin``: factory resolves the engine binary for this adapter.
    # ``adapter_owned``: the adapter resolves its own bridge executable.
    binary_resolution: Literal["engine_bin", "adapter_owned"] = "engine_bin"
    # Credential-scoped model catalog probing in a temporary native home.
    scoped_model_catalog_probe: bool = False
    # Optional top-level Runtime config fields beyond the common ones.
    accepts_adapter_endpoint: bool = False
    accepts_launch_args: bool = True
    transport_settings: tuple[TransportSetting, ...] = ()
    notes: str = ""


class ProviderIdentity(_Frozen):
    engine: str
    display_name: str
    support_status: str
    default_adapter_id: str
    cli_adapter_id: str
    # Short label of the default conversation transport.
    transport_label: str


HostLoginImport = Literal[
    "none", "claude_settings_env", "kimi_home_copy", "grok_home_copy",
    "codex_auth_json",
]
LoginStatusProbe = Literal[
    "claude_oauth", "codex_auth_json", "cursor_session", "pi_agent_files",
    "omp_agent_dir", "kimi_credentials", "grok_auth_json", "opencode_auth",
    "devin_cli", "droid_login",
]


class LoginSpec(_Frozen):
    guidance_command: str
    guidance_note: str = ""
    # Argv the operator runs to log in. Empty when login happens inside the
    # interactive TUI (``guidance_command`` explains it).
    login_argv: tuple[str, ...] = ()
    # Read-only host login detector implemented in credential_accounts.
    status_probe: LoginStatusProbe
    # Native CLI status command used by that detector, when one exists.
    status_argv: tuple[str, ...] = ()
    host_login_import: HostLoginImport = "none"
    # Only the host CLI login is usable (no stored credential accounts).
    system_login_only: bool = False


CredentialEnvResolver = Literal[
    "claude", "codex", "cursor", "kimi", "grok", "opencode", "pi", "omp",
    "devin_host_only", "droid",
]


class CredentialSpec(_Frozen):
    # Resolver in credential_accounts.runtime_env_for_engine.
    env_resolver: CredentialEnvResolver
    # Variables shadowed before a stored account is applied.
    env_keys: tuple[str, ...] = ()
    # Account layout written by ``upsert_secret`` for an official account:
    # a named secret file, the generic ``API_KEY`` layout, or a Codex auth home.
    # Empty means stored accounts are not supported (host login only).
    secret_file: Literal[
        "", "CLAUDE_CODE_OAUTH_TOKEN", "CURSOR_API_KEY", "API_KEY", "CODEX_AUTH_HOME",
    ] = ""
    # The resolver writes per-run agent state (generated provider config or
    # XDG directories) when given an ``agent_state_dir``.
    agent_state_dir: bool = False


ModelParser = Literal[
    "openai_models", "cursor_models", "pi_models", "omp_models", "kimi_models",
    "grok_models", "opencode_models", "devin_models", "droid_models",
]


class ModelDiscoverySpec(_Frozen):
    # ``cli``: non-interactive CLI catalog; ``reference_catalog``: no stable
    # catalog command, Muteki shows a reference list.
    method: Literal["cli", "reference_catalog"]
    # Arguments after the binary.
    argv: tuple[str, ...] = ()
    # Retried when the primary catalog fails or is empty.
    fallback_argv: tuple[str, ...] = ()
    fallback_source: str = ""
    parser: Optional[ModelParser] = None
    # Richer metadata source tried before ``argv``.
    metadata_probe: Literal["", "grok_acp_session", "pi_node_catalog"] = ""
    # Catalog rows carry a provider namespace that a profile may pin.
    provider_scoped: bool = False
    # Wire protocol of custom endpoint model discovery.
    endpoint_protocol: Literal["openai", "anthropic"] = "openai"
    # Custom-endpoint model tests run in a throwaway native config dir and
    # check the model reported in the CLI's JSON result.
    endpoint_test_isolated_config: bool = False
    # ``system:<engine>`` credentials list models from the live CLI.
    system_credential_live_catalog: bool = False


class EnvironmentImport(_Frozen):
    """Extra host path imported into the private chat home."""

    host_relative: str
    # Path relative to the private environment root.
    target: str


class EnvironmentSpec(_Frozen):
    home_env_var: str
    home_relative: str
    # The variable names a parent directory; the engine home is ``$VAR/<engine>``.
    home_env_is_parent: bool = False
    # Muteki prepares a private per-identity native home for chat.
    managed_home: bool = True
    mcp_files: tuple[str, ...] = ("mcp.json",)
    # Extra MCP config files relative to the user home.
    extra_mcp_home_files: tuple[str, ...] = ()
    extra_configuration_names: tuple[str, ...] = ()
    native_extension_assets: bool = False
    extra_imports: tuple[EnvironmentImport, ...] = ()
    # Extra variables pointing into the private home:
    # ``home_target`` = engine home, ``private_data`` = <root>/data.
    private_env: dict[str, Literal["home_target", "private_data"]] = Field(default_factory=dict)
    # Credential variables that, when supplied, require an in-memory store.
    memory_credential_store_when: tuple[str, ...] = ()
    memory_credential_store_env: str = ""
    user_skill_roots: tuple[str, ...] = ()
    plugin_manifest: Literal["", "codex_config_toml", "claude_installed_plugins"] = ""
    # Worker-home variable for native extensions/agents and its subdir.
    worker_component_home_env: str = ""
    worker_component_home_subdir: str = ""


class ComponentSpec(_Frozen):
    """Native ABIs for chat plugin components."""

    native_agents: Literal["none", "plugin_package", "home_agents_dir"] = "none"
    native_hooks: Literal["none", "all", "command_only"] = "none"
    native_plugin_packages: Literal["none", "claude_local_plugins", "codex_marketplace"] = "none"
    # Skills marked ``requires_native`` use this engine's own skill semantics.
    native_skill_semantics: bool = False
    extension_abi: bool = False
    extension_dir: str = ""
    extension_export: Literal["", "default", "star"] = ""


class AttachmentSpec(_Frozen):
    # Native image wire format; ``none`` = workspace paths only.
    native_image_wire: Literal[
        "none", "codex_user_input", "claude_content_blocks", "pi_prompt_images",
    ] = "none"


EffortStyle = Literal[
    "none", "claude_settings_or_flag", "codex_config", "cursor_model_variant",
    "thinking_flag", "reasoning_effort_flag", "environment", "variant_flag",
]
RuntimeArgvHook = Literal[
    "opencode_config_merge", "kimi_env_provider_model", "cursor_endpoint",
    "codex_provider_flags", "pi_like_provider",
]


class CliSpec(_Frozen):
    """Worker CLI argv conventions (muteki.solver.cli_engines.argv)."""

    reasoning_efforts: tuple[str, ...] = ()
    effort_style: EffortStyle = "none"
    # Extra options must precede ``-p/--prompt/--single``.
    options_before_prompt_flag: bool = False
    runtime_argv: tuple[RuntimeArgvHook, ...] = ()
    # Model selected through this variable instead of ``--model``.
    model_env_var: str = ""
    # Endpoint profiles inject provider/model flags in the driver itself.
    endpoint_driver_sets_model: bool = False
    # Prefix of MUTEKI_<X>_PROVIDER / _MODEL / _SYSTEM_PROMPT variables.
    provider_env_prefix: str = ""
    # JSON event type that ends a print-mode turn; a turn that ends with it
    # but no assistant message is classified as ``model_no_reply``.
    turn_settled_event: str = ""


class WorkerProfileSpec(_Frozen):
    """Flat Worker profile values (muteki.solver.worker_profiles)."""

    # Profile ``transport`` id; an alias of this engine in TRANSPORT_TO_ENGINE.
    transport: str = ""
    # ``wire_api`` of a custom-endpoint profile; the same default the profile
    # normalizer and model-test identity apply when the field is empty.
    endpoint_wire_api: Literal["", "responses", "chat_completions"] = ""
    # Display label of the protocol a Worker credential for this engine speaks.
    protocol_label: str = ""
    # ``credential_mode`` of a profile bound to an official (non-endpoint)
    # credential; ``api_key`` engines need a stored key unless host login works.
    official_credential_mode: Literal["subscription", "api_key"] = "subscription"


class CommandCatalogSpec(_Frozen):
    # Command name -> Muteki UI action.
    client_aliases: dict[str, str] = Field(default_factory=dict)
    # Terminal-only commands that must never be sent to the model. ``None``
    # means the engine has no Muteki command catalog (the list is not known).
    terminal_only: Optional[tuple[str, ...]] = None

    @property
    def defined(self) -> bool:
        return self.terminal_only is not None


class ProviderDescriptor(_Frozen):
    descriptor_version: int = DESCRIPTOR_SCHEMA_VERSION
    identity: ProviderIdentity
    adapters: tuple[AdapterDescriptor, ...]
    login: LoginSpec
    credentials: CredentialSpec
    models: ModelDiscoverySpec
    environment: EnvironmentSpec
    components: ComponentSpec = ComponentSpec()
    attachments: AttachmentSpec = AttachmentSpec()
    cli: CliSpec = CliSpec()
    worker: WorkerProfileSpec = WorkerProfileSpec()
    commands: CommandCatalogSpec = CommandCatalogSpec()
    session_import: Literal["none", "claude_projects", "codex_sessions"] = "none"

    @property
    def engine(self) -> str:
        return self.identity.engine

    def adapter(self, adapter_id: str = "") -> AdapterDescriptor:
        wanted = adapter_id or self.identity.default_adapter_id
        for item in self.adapters:
            if item.adapter_id == wanted or wanted in item.legacy_aliases:
                return item
        raise UnknownProviderError(wanted, kind="adapter")

    @property
    def default_adapter(self) -> AdapterDescriptor:
        return self.adapter(self.identity.default_adapter_id)

    @property
    def capability_gateway(self) -> bool:
        return self.default_adapter.capability_gateway


# ---------------------------------------------------------------------------
# Declared capabilities
# ---------------------------------------------------------------------------

_ALL_ACCESS = tuple(ACCESS_MODE_VALUES)
_NO_AUTO = (
    AccessMode.SUPERVISED.value,
    AccessMode.AUTO_ACCEPT_EDITS.value,
    AccessMode.FULL_ACCESS.value,
)


def _caps(transport_kind: str, *, access_modes: tuple[str, ...] = (), plan_mode_per_turn: bool = True, **flags: bool) -> AgentCapabilities:
    unknown = set(flags) - set(BOOL_CAPABILITY_FIELDS)
    if unknown:
        raise ValueError(f"unknown capability fields: {sorted(unknown)}")
    return AgentCapabilities(
        transport_kind=transport_kind,
        access_modes=list(access_modes),
        plan_mode_per_turn=plan_mode_per_turn,
        capability_source=SOURCE_STATIC,
        **flags,
    )


def _acp_caps(*, access_modes: tuple[str, ...], **flags: bool) -> AgentCapabilities:
    """BaseAcpAdapter baseline: streaming, tool events, permission requests,
    cancel, usage, plan updates, session load/resume and mcpServers."""
    base = dict(
        streaming=True, tool_events=True, approval=True, interrupt=True,
        usage_events=True, plan=True, resume=True, session_persistence=True,
        acp_mcp_config=True,
    )
    base.update(flags)
    return _caps("acp", access_modes=access_modes, **base)


def _cli_caps(*, usage_events: bool) -> AgentCapabilities:
    """CLI compatibility path (probe_cli_driver): stream parsing, resume argv
    and process-tree interrupt; everything structured is unavailable."""
    return _caps(
        "cli", streaming=True, tool_events=True, resume=True,
        session_persistence=True, interrupt=True, usage_events=usage_events,
    )



_COMMON_ALIASES: dict[str, str] = {
    "undo": "rewind", "rewind": "rewind",
    "help": "help", "model": "model", "rename": "rename",
    "resume": "sessions", "fork": "fork", "export": "export",
    "status": "status", "permissions": "permissions",
    "skills": "skills", "plugins": "plugins", "mcp": "mcp",
}

_FULL_EFFORTS = ("none", "minimal", "low", "medium", "high", "xhigh", "max")
_UPPER_EFFORTS = ("low", "medium", "high", "xhigh", "max")


def _cli_adapter(engine: str, *, usage_events: bool) -> AdapterDescriptor:
    return AdapterDescriptor(
        adapter_id=f"cli.{engine}", role="cli", transport_kind="cli",
        capabilities=_cli_caps(usage_events=usage_events),
        accepts_launch_args=False,
        notes="Worker compatibility transport; one process per turn.",
    )


def _display_name(engine: str) -> str:
    return ENGINE_DESCRIPTOR_BY_ID[engine].display_name


def _support_status(engine: str) -> str:
    return ENGINE_DESCRIPTOR_BY_ID[engine].support_status


def _identity(engine: str, default_adapter_id: str, transport_label: str) -> ProviderIdentity:
    return ProviderIdentity(
        engine=engine,
        display_name=_display_name(engine),
        support_status=_support_status(engine),
        default_adapter_id=default_adapter_id,
        cli_adapter_id=f"cli.{engine}",
        transport_label=transport_label,
    )


_SESSION_DIR = TransportSetting(name="session_dir", type="string", title="会话目录")

_OPENAI_ENV_KEYS = ("OPENAI_API_KEY", "OPENAI_API_KEY_FILE", "OPENAI_BASE_URL")


_DESCRIPTORS: tuple[ProviderDescriptor, ...] = (
    ProviderDescriptor(
        identity=_identity("codex", "codex.app_server", "App Server"),
        adapters=(
            AdapterDescriptor(
                adapter_id="codex.app_server", role="default", transport_kind="rpc",
                capabilities=_caps(
                    "rpc", access_modes=_ALL_ACCESS,
                    streaming=True, tool_events=True, subagents=True, resume=True,
                    session_persistence=True, steer=True, interrupt=True,
                    approval=True, user_input=True, fork=True, mcp=True,
                    usage_events=True, plan=True, image_input=True, compaction=True,
                ),
                native_rewind=True,
                scoped_model_catalog_probe=True,
                transport_settings=(
                    TransportSetting(
                        name="experimental_api", type="boolean",
                        title="启用 experimentalApi（结构化提问）", default=False,
                        description=(
                            "初始化 App Server 时声明 experimentalApi，启用结构化"
                            "提问（requestUserInput）与 apps 等实验接口。默认关闭。"
                        ),
                    ),
                ),
                notes=(
                    "user_input requires experimentalApi at runtime. The backend "
                    "transport schema rejects unknown fields such as codex_home."
                ),
            ),
            _cli_adapter("codex", usage_events=False),
        ),
        login=LoginSpec(
            guidance_command="codex login",
            guidance_note="登录后可在统一凭据中心从宿主 ~/.codex/auth.json 导入。",
            login_argv=("codex", "login"),
            status_probe="codex_auth_json",
            host_login_import="codex_auth_json",
        ),
        credentials=CredentialSpec(
            env_resolver="codex",
            env_keys=(*_OPENAI_ENV_KEYS, "CODEX_HOME"),
            secret_file="CODEX_AUTH_HOME",
        ),
        models=ModelDiscoverySpec(
            method="cli", argv=("debug", "models"),
            fallback_argv=("debug", "models", "--bundled"),
            fallback_source="codex_cli_bundled",
            parser="openai_models",
        ),
        environment=EnvironmentSpec(
            home_env_var="CODEX_HOME", home_relative=".codex",
            mcp_files=("config.toml",),
            user_skill_roots=(".codex/skills", ".agents/skills"),
            plugin_manifest="codex_config_toml",
        ),
        components=ComponentSpec(
            native_hooks="command_only", native_plugin_packages="codex_marketplace",
        ),
        attachments=AttachmentSpec(native_image_wire="codex_user_input"),
        cli=CliSpec(
            reasoning_efforts=_FULL_EFFORTS, effort_style="codex_config",
            runtime_argv=("codex_provider_flags",), endpoint_driver_sets_model=True,
        ),
        worker=WorkerProfileSpec(transport="codex_cli", endpoint_wire_api="responses", protocol_label="OpenAI Responses"),
        commands=CommandCatalogSpec(
            client_aliases={**_COMMON_ALIASES, "approvals": "permissions", "diff": "diff", "undo": "rewind"},
            terminal_only=("theme", "statusline", "terminal-setup", "quit", "exit", "logout", "login"),
        ),
        session_import="codex_sessions",
    ),
    ProviderDescriptor(
        identity=_identity("claude", "claude.sdk", "Agent SDK"),
        adapters=(
            AdapterDescriptor(
                adapter_id="claude.sdk", role="default", transport_kind="sdk",
                legacy_aliases=("claude.agent_sdk",),
                capabilities=_caps(
                    "sdk", access_modes=_ALL_ACCESS,
                    streaming=True, resume=True, session_persistence=True,
                    steer=True, interrupt=True, approval=True, user_input=True,
                    fork=True, mcp=True, native_tool_binding=True,
                    structured_output=True, subagents=True, tool_events=True,
                    usage_events=True, image_input=True, plan=True, plan_mode=True,
                ),
                native_rewind=True,
                transport_settings=(
                    TransportSetting(name="permission_mode", type="string", title="权限模式", default="default"),
                ),
                notes=(
                    "No structured compact operation: /compact is a native slash "
                    "command sent as a prompt. plan_mode maps to permission mode "
                    "plan per turn; ExitPlanMode is surfaced as a plan_exit "
                    "approval carrying the full plan (allow leaves plan mode, "
                    "deny keeps planning). No task-list plan events. Native "
                    "rewind needs the turn log of this process (resumed "
                    "sessions rebuild instead)."
                ),
            ),
            _cli_adapter("claude", usage_events=True),
        ),
        login=LoginSpec(
            guidance_command="claude（进入后执行 /login）",
            guidance_note="macOS 登录态保存在 Keychain；也可在统一凭据中心导入宿主登录。",
            status_probe="claude_oauth",
            host_login_import="claude_settings_env",
        ),
        credentials=CredentialSpec(
            env_resolver="claude",
            env_keys=(
                "CLAUDE_CODE_OAUTH_TOKEN", "CLAUDE_CODE_OAUTH_TOKEN_FILE",
                "ANTHROPIC_API_KEY", "ANTHROPIC_API_KEY_FILE",
                "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_AUTH_TOKEN_FILE",
                "ANTHROPIC_BASE_URL",
            ),
            secret_file="CLAUDE_CODE_OAUTH_TOKEN",
        ),
        models=ModelDiscoverySpec(
            method="reference_catalog", endpoint_protocol="anthropic",
            endpoint_test_isolated_config=True,
        ),
        environment=EnvironmentSpec(
            home_env_var="CLAUDE_CONFIG_DIR", home_relative=".claude",
            mcp_files=("settings.json", "mcp.json"),
            extra_mcp_home_files=(".claude.json",),
            extra_imports=(EnvironmentImport(host_relative=".claude.json", target="home/.claude.json"),),
            user_skill_roots=(".claude/skills", ".agents/skills"),
            plugin_manifest="claude_installed_plugins",
        ),
        components=ComponentSpec(
            native_agents="plugin_package", native_hooks="all",
            native_plugin_packages="claude_local_plugins", native_skill_semantics=True,
        ),
        attachments=AttachmentSpec(native_image_wire="claude_content_blocks"),
        cli=CliSpec(
            reasoning_efforts=_UPPER_EFFORTS, effort_style="claude_settings_or_flag",
            model_env_var="ANTHROPIC_MODEL",
        ),
        worker=WorkerProfileSpec(transport="claude_code", protocol_label="Anthropic Messages"),
        commands=CommandCatalogSpec(
            client_aliases={**_COMMON_ALIASES, "effort": "effort", "cost": "status", "copy": "copy", "rewind": "rewind"},
            terminal_only=("vim", "terminal-setup", "theme", "keybindings", "voice", "exit", "quit", "login", "logout"),
        ),
        session_import="claude_projects",
    ),
    ProviderDescriptor(
        identity=_identity("cursor", "cursor.sdk", "SDK"),
        adapters=(
            AdapterDescriptor(
                adapter_id="cursor.sdk", role="default", transport_kind="sdk",
                capabilities=_caps(
                    "sdk", access_modes=_ALL_ACCESS,
                    streaming=True, resume=True, steer=True, interrupt=True,
                    usage_events=True, subagents=True, tool_events=True,
                    session_persistence=True, mcp=True, plan=True, plan_mode=True,
                    image_input=True, approval=False, user_input=False,
                ),
                binary_resolution="adapter_owned",
                scoped_model_catalog_probe=True,
                access_mode_notes={
                    AccessMode.SUPERVISED.value: (
                        "supervised: mapped to autoReview=true plus sandbox; "
                        "weaker than interactive approval"
                    ),
                    AccessMode.AUTO_ACCEPT_EDITS.value: (
                        "auto-accept-edits: mapped to autoReview=true plus sandbox, "
                        "the same as supervised; weaker than interactive approval"
                    ),
                },
            ),
            AdapterDescriptor(
                adapter_id="cursor.acp", role="variant", transport_kind="acp",
                capabilities=_acp_caps(access_modes=_NO_AUTO, subagents=True),
                binary_resolution="adapter_owned",
                access_mode_notes={
                    AccessMode.AUTO.value: (
                        "auto: rejected at session start; Cursor's Smart Auto "
                        "(--auto-review) is accepted on the command line, but the "
                        "ACP server still requests approval for read-only "
                        "commands (measured on 2026.10.01)"
                    ),
                },
            ),
            _cli_adapter("cursor", usage_events=True),
        ),
        login=LoginSpec(
            guidance_command="cursor-agent login 或设置 CURSOR_API_KEY",
            guidance_note="headless 模式只读取 CURSOR_API_KEY。",
            login_argv=("cursor-agent", "login"),
            status_probe="cursor_session",
        ),
        credentials=CredentialSpec(
            env_resolver="cursor",
            env_keys=(
                "CURSOR_API_KEY", "CURSOR_API_KEY_FILE", "CURSOR_AUTH_TOKEN",
                "CURSOR_AUTH_TOKEN_FILE", "CURSOR_ENDPOINT",
            ),
            secret_file="CURSOR_API_KEY",
        ),
        models=ModelDiscoverySpec(method="cli", argv=("models",), parser="cursor_models"),
        environment=EnvironmentSpec(
            home_env_var="CURSOR_CONFIG_DIR", home_relative=".cursor",
            mcp_files=("mcp.json", "cli-config.json"),
            private_env={"CURSOR_DATA_DIR": "private_data"},
            memory_credential_store_when=("CURSOR_API_KEY", "CURSOR_AUTH_TOKEN"),
            memory_credential_store_env="AGENT_CLI_CREDENTIAL_STORE",
            user_skill_roots=(".cursor/skills", ".cursor/skills-cursor", ".agents/skills"),
        ),
        cli=CliSpec(
            reasoning_efforts=_UPPER_EFFORTS, effort_style="cursor_model_variant",
            runtime_argv=("cursor_endpoint",),
        ),
        worker=WorkerProfileSpec(transport="cursor_agent", protocol_label="Cursor CLI 接口", official_credential_mode="api_key"),
        commands=CommandCatalogSpec(
            client_aliases={**_COMMON_ALIASES, "new-chat": "new", "newchat": "new", "rewind": "rewind", "about": "status", "copy": "copy"},
            terminal_only=("vim", "line-numbers", "show-thinking", "status-indicators", "setup-terminal", "quit", "exit", "open", "cursor", "update", "login", "logout", "summarize", "compress", "shell", "sh", "run", "feedback", "config", "sandbox", "max-mode", "goal", "debug"),
        ),
    ),
    ProviderDescriptor(
        identity=_identity("grok", "grok.acp", "ACP"),
        adapters=(
            AdapterDescriptor(
                adapter_id="grok.acp", role="default", transport_kind="acp",
                capabilities=_acp_caps(access_modes=_ALL_ACCESS, subagents=True),
                transport_settings=(
                    TransportSetting(
                        name="mcp_ready_wait_seconds", type="number",
                        title="HTTP MCP 启动等待（秒）", default=GROK_MCP_READY_WAIT_SECONDS, minimum=0, maximum=30,
                    ),
                ),
            ),
            _cli_adapter("grok", usage_events=True),
        ),
        login=LoginSpec(
            guidance_command="grok login",
            login_argv=("grok", "login"),
            status_probe="grok_auth_json",
            host_login_import="grok_home_copy",
        ),
        credentials=CredentialSpec(
            env_resolver="grok",
            env_keys=("XAI_API_KEY", "XAI_API_KEY_FILE", "GROK_MODELS_BASE_URL", "GROK_HOME"),
            secret_file="API_KEY",
        ),
        models=ModelDiscoverySpec(
            method="cli", argv=("models",), parser="grok_models",
            metadata_probe="grok_acp_session",
        ),
        environment=EnvironmentSpec(
            home_env_var="GROK_HOME", home_relative=".grok",
            mcp_files=("mcp.json", "config.toml"),
            user_skill_roots=(".grok/skills", ".agents/skills"),
        ),
        cli=CliSpec(
            reasoning_efforts=("low", "medium", "high", "xhigh"),
            effort_style="reasoning_effort_flag", options_before_prompt_flag=True,
        ),
        worker=WorkerProfileSpec(transport="grok_build", protocol_label="Grok Build CLI"),
        commands=CommandCatalogSpec(
            client_aliases={**_COMMON_ALIASES, "always-approve": "permissions"},
            terminal_only=("login", "logout", "quit", "exit", "theme"),
        ),
    ),
    ProviderDescriptor(
        identity=_identity("opencode", "opencode.server", "HTTP API"),
        adapters=(
            AdapterDescriptor(
                adapter_id="opencode.server", role="default", transport_kind="http",
                capabilities=_caps(
                    "http", access_modes=_NO_AUTO,
                    streaming=True, tool_events=True, usage_events=True,
                    interrupt=True, approval=True, user_input=True, resume=True,
                    session_persistence=True, mcp=True, subagents=True,
                    compaction=True, steer=True, plan=True, plan_mode=True,
                ),
                accepts_adapter_endpoint=True,
                access_mode_notes={
                    AccessMode.SUPERVISED.value: (
                        "supervised: shell commands and edits request approval. "
                        "OpenCode 1.x managed servers install descendant rules "
                        "through inline config; restricted subagent tools on "
                        "attached 1.x servers are denied. OpenCode 2.x uses "
                        "native session rules and routes subagent requests "
                        "through the parent conversation"
                    ),
                },
            ),
            _cli_adapter("opencode", usage_events=False),
        ),
        login=LoginSpec(
            guidance_command="opencode auth login",
            login_argv=("opencode", "auth", "login"),
            status_probe="opencode_auth",
        ),
        credentials=CredentialSpec(
            env_resolver="opencode",
            env_keys=(
                *_OPENAI_ENV_KEYS, "OPENCODE_API_KEY", "OPENCODE_API_KEY_FILE",
                "OPENCODE_CONFIG_CONTENT", "MUTEKI_OPENCODE_PROVIDER",
            ),
            secret_file="API_KEY", agent_state_dir=True,
        ),
        models=ModelDiscoverySpec(
            method="cli", argv=("models", "--verbose"), parser="opencode_models",
            provider_scoped=True,
        ),
        environment=EnvironmentSpec(
            home_env_var="XDG_CONFIG_HOME", home_relative=".config/opencode",
            home_env_is_parent=True,
            mcp_files=("opencode.json",),
            native_extension_assets=True,
            extra_imports=(EnvironmentImport(
                host_relative=".local/share/opencode/auth.json",
                target="data/opencode/auth.json",
            ),),
            private_env={"OPENCODE_CONFIG_DIR": "home_target"},
            user_skill_roots=(".config/opencode/skills", ".opencode/skills", ".agents/skills"),
            worker_component_home_env="XDG_CONFIG_HOME",
            worker_component_home_subdir="opencode",
        ),
        components=ComponentSpec(
            native_agents="home_agents_dir", extension_abi=True,
            extension_dir="plugins", extension_export="star",
        ),
        cli=CliSpec(
            reasoning_efforts=_FULL_EFFORTS, effort_style="variant_flag",
            runtime_argv=("opencode_config_merge",),
        ),
        worker=WorkerProfileSpec(transport="opencode_cli", endpoint_wire_api="chat_completions", protocol_label="OpenAI 兼容接口", official_credential_mode="api_key"),
        commands=CommandCatalogSpec(
            client_aliases={**_COMMON_ALIASES, "models": "model", "sessions": "sessions", "continue": "sessions", "undo": "rewind", "details": "details"},
            terminal_only=("connect", "editor", "exit", "quit", "q", "theme", "themes"),
        ),
    ),
    ProviderDescriptor(
        identity=_identity("pi", "pi.rpc", "RPC"),
        adapters=(
            AdapterDescriptor(
                adapter_id="pi.rpc", role="default", transport_kind="rpc",
                capabilities=_caps(
                    "rpc", streaming=True, tool_events=True, usage_events=True,
                    steer=True, interrupt=True, resume=True,
                    session_persistence=True, skills=True, agent_plugin=True,
                    image_input=True, compaction=True,
                ),
                transport_settings=(_SESSION_DIR,),
                notes="No in-band approval: tool scope is fixed by launch flags.",
            ),
            AdapterDescriptor(
                adapter_id="pi.acp", role="variant", transport_kind="acp",
                capabilities=_acp_caps(access_modes=_NO_AUTO),
                binary_resolution="adapter_owned",
                access_mode_notes={
                    AccessMode.AUTO.value: (
                        "auto: rejected at session start; the pi-acp bridge asks "
                        "before every tool call and Pi has no native auto policy"
                    ),
                },
            ),
            _cli_adapter("pi", usage_events=True),
        ),
        login=LoginSpec(
            guidance_command="pi（完成登录后写入 ~/.pi/agent）",
            status_probe="pi_agent_files",
        ),
        credentials=CredentialSpec(
            env_resolver="pi",
            env_keys=(*_OPENAI_ENV_KEYS, "PI_CODING_AGENT_DIR", "MUTEKI_PI_PROVIDER", "MUTEKI_PI_MODEL"),
            secret_file="API_KEY", agent_state_dir=True,
        ),
        models=ModelDiscoverySpec(
            method="cli", argv=("--list-models",), parser="pi_models",
            metadata_probe="pi_node_catalog", provider_scoped=True,
        ),
        environment=EnvironmentSpec(
            home_env_var="PI_CODING_AGENT_DIR", home_relative=".pi/agent",
            native_extension_assets=True,
            user_skill_roots=(".pi/agent/skills", ".pi/skills", ".agents/skills"),
            worker_component_home_env="PI_CODING_AGENT_DIR",
        ),
        components=ComponentSpec(
            extension_abi=True, extension_dir="extensions", extension_export="default",
        ),
        attachments=AttachmentSpec(native_image_wire="pi_prompt_images"),
        cli=CliSpec(
            reasoning_efforts=_FULL_EFFORTS, effort_style="thinking_flag",
            runtime_argv=("pi_like_provider",), provider_env_prefix="MUTEKI_PI",
            turn_settled_event="agent_settled",
        ),
        worker=WorkerProfileSpec(transport="pi", protocol_label="OpenAI 兼容接口", official_credential_mode="api_key"),
        commands=CommandCatalogSpec(
            client_aliases={**_COMMON_ALIASES, "name": "rename", "thinking": "effort", "clone": "fork", "copy": "copy"},
            terminal_only=("settings", "hotkeys", "scoped-models", "trust", "login", "logout", "quit", "share", "bug", "reload", "changelog", "import"),
        ),
    ),
    ProviderDescriptor(
        identity=_identity("kimi", "kimi.acp", "ACP / Wire"),
        adapters=(
            AdapterDescriptor(
                adapter_id="kimi.acp", role="default", transport_kind="acp",
                # auto-accept-edits keeps Kimi's native ``default`` mode and the
                # shared ACP permission callback allows edit-kind tool calls.
                capabilities=_acp_caps(access_modes=_ALL_ACCESS),
            ),
            AdapterDescriptor(
                adapter_id="kimi.local_server", role="variant", transport_kind="http+ws",
                capabilities=_caps(
                    "http+ws", access_modes=_ALL_ACCESS,
                    resume=True, session_persistence=True,
                    interrupt=True, streaming=True, tool_events=True,
                    usage_events=True, approval=True, user_input=True,
                    steer=True, subagents=True,
                ),
                accepts_adapter_endpoint=True,
                notes=(
                    "Experimental local server. Access modes map to the per-prompt "
                    "permission_mode (manual/auto/yolo) only when the instance "
                    "OpenAPI declares it; supervised is refused inside a Git worktree."
                ),
            ),
            _cli_adapter("kimi", usage_events=False),
        ),
        login=LoginSpec(
            guidance_command="kimi（进入后执行 /login，写入 ~/.kimi-code）",
            status_probe="kimi_credentials",
            host_login_import="kimi_home_copy",
        ),
        credentials=CredentialSpec(
            env_resolver="kimi",
            env_keys=(
                "KIMI_MODEL_API_KEY", "KIMI_MODEL_API_KEY_FILE",
                "KIMI_MODEL_BASE_URL", "KIMI_MODEL_NAME",
                "KIMI_MODEL_PROVIDER_TYPE", "KIMI_MODEL_MAX_CONTEXT_SIZE",
                "KIMI_MODEL_MAX_OUTPUT_SIZE", "KIMI_CODE_HOME",
            ),
            secret_file="API_KEY",
        ),
        models=ModelDiscoverySpec(
            method="cli", argv=("provider", "list", "--json"), parser="kimi_models",
            provider_scoped=True,
        ),
        environment=EnvironmentSpec(
            home_env_var="KIMI_CODE_HOME", home_relative=".kimi-code",
            mcp_files=("mcp.json", "config.toml"),
            user_skill_roots=(".kimi-code/skills", ".kimi/skills", ".agents/skills"),
        ),
        cli=CliSpec(
            reasoning_efforts=_UPPER_EFFORTS, effort_style="environment",
            options_before_prompt_flag=True, runtime_argv=("kimi_env_provider_model",),
        ),
        worker=WorkerProfileSpec(transport="kimi_code", protocol_label="Kimi Code CLI"),
        commands=CommandCatalogSpec(
            client_aliases={**_COMMON_ALIASES, "sessions": "sessions", "branch": "fork", "reset": "new", "yolo": "permissions"},
            terminal_only=("login", "logout", "provider", "theme", "editor", "quit", "exit", "update", "setup"),
        ),
    ),
    ProviderDescriptor(
        identity=_identity("omp", "omp.acp", "ACP"),
        adapters=(
            AdapterDescriptor(
                adapter_id="omp.acp", role="default", transport_kind="acp",
                capabilities=_acp_caps(access_modes=_NO_AUTO, subagents=True),
                access_mode_notes={
                    AccessMode.AUTO.value: (
                        "auto: rejected at session start; OMP --approval-mode "
                        "offers only always-ask, write and yolo"
                    ),
                },
            ),
            AdapterDescriptor(
                adapter_id="omp.rpc_v2", role="variant", transport_kind="rpc",
                legacy_aliases=("omp.rpc",),
                capabilities=_caps(
                    "rpc", streaming=True, tool_events=True, usage_events=True,
                    interrupt=True, steer=True, resume=True,
                    session_persistence=True, structured_http_rpc=True,
                    subagents=True, compaction=True,
                ),
                transport_settings=(_SESSION_DIR,),
                notes=(
                    "Variant. This adapter does not pass --approval-mode and "
                    "answers extension_ui_request as cancelled, so Conversation "
                    "access modes are not honored (omp.rpc.access_mode_not_honored). "
                    "omp 17.2.12 can emit an Approve/Deny select when launched with "
                    "--approval-mode always-ask; the RPC bash command still skips that gate."
                ),
            ),
            _cli_adapter("omp", usage_events=True),
        ),
        login=LoginSpec(
            guidance_command="omp（完成登录后写入 ~/.omp/agent）",
            status_probe="omp_agent_dir",
        ),
        credentials=CredentialSpec(
            env_resolver="omp",
            env_keys=(*_OPENAI_ENV_KEYS, "PI_CODING_AGENT_DIR", "MUTEKI_OMP_PROVIDER", "MUTEKI_OMP_MODEL"),
            secret_file="API_KEY", agent_state_dir=True,
        ),
        models=ModelDiscoverySpec(
            method="cli", argv=("models", "--json"), parser="omp_models",
            provider_scoped=True,
        ),
        environment=EnvironmentSpec(
            home_env_var="PI_CODING_AGENT_DIR", home_relative=".omp/agent",
            mcp_files=("mcp.json", "settings.json"),
            native_extension_assets=True,
            user_skill_roots=(".omp/agent/skills", ".omp/skills", ".agents/skills"),
            worker_component_home_env="PI_CODING_AGENT_DIR",
        ),
        components=ComponentSpec(
            native_agents="home_agents_dir", extension_abi=True,
            extension_dir="extensions", extension_export="default",
        ),
        cli=CliSpec(
            reasoning_efforts=_FULL_EFFORTS, effort_style="thinking_flag",
            runtime_argv=("pi_like_provider",), provider_env_prefix="MUTEKI_OMP",
        ),
        worker=WorkerProfileSpec(transport="omp", protocol_label="OpenAI 兼容接口", official_credential_mode="api_key"),
        commands=CommandCatalogSpec(
            client_aliases={**_COMMON_ALIASES, "models": "model", "thinking": "effort", "branch": "fork", "copy": "copy"},
            terminal_only=("settings", "hotkeys", "login", "logout", "quit", "theme"),
        ),
    ),
    ProviderDescriptor(
        identity=_identity("devin", "devin.acp", "ACP"),
        adapters=(
            AdapterDescriptor(
                adapter_id="devin.acp", role="default", transport_kind="acp",
                capabilities=_acp_caps(
                    access_modes=("auto-accept-edits", "auto", "full-access"), subagents=True,
                ),
                access_mode_notes={
                    "supervised": (
                        "不可用：Devin 3000.11.3 的 Code/Smart/Bypass 不逐次询问文件修改；"
                        "Ask 只读，Plan 批准后切换到 Code/Bypass。请明确选择其他权限模式。"
                    ),
                    "auto-accept-edits": (
                        "Selects Devin's native accept-edits mode: workspace edits "
                        "are auto-approved by Devin; shell commands run through "
                        "Muteki's client terminal and ask for approval."
                    ),
                },
                notes=(
                    "Conversation-only; Gateway delivery uses ACP session mcpServers "
                    "with the native HTTP capability probe. Effort is a model variant."
                ),
            ),
            _cli_adapter("devin", usage_events=False),
        ),
        login=LoginSpec(
            guidance_command="devin auth login",
            guidance_note="聊天使用本机 Devin CLI 登录；也支持 WINDSURF_API_KEY。",
            login_argv=("devin", "auth", "login"),
            status_probe="devin_cli",
            status_argv=("devin", "auth", "status"),
            system_login_only=True,
        ),
        credentials=CredentialSpec(env_resolver="devin_host_only"),
        models=ModelDiscoverySpec(
            method="cli", argv=("models", "list", "--format", "json"),
            parser="devin_models", system_credential_live_catalog=True,
        ),
        environment=EnvironmentSpec(
            home_env_var="XDG_CONFIG_HOME", home_relative=".config/devin",
            home_env_is_parent=True, managed_home=False,
            mcp_files=("mcp_config.json",),
            extra_configuration_names=("config.json", "mcp_config.json"),
            user_skill_roots=(".config/devin/skills", ".agents/skills"),
        ),
        cli=CliSpec(options_before_prompt_flag=True),
        worker=WorkerProfileSpec(transport="devin_cli", protocol_label="Devin CLI"),
        # No Muteki command catalog: Devin's terminal-only command list is not
        # known, and its composer relies on the native ACP command list only.
        commands=CommandCatalogSpec(),
    ),
    ProviderDescriptor(
        identity=_identity("droid", "droid.rpc", "RPC"),
        adapters=(
            AdapterDescriptor(
                adapter_id="droid.rpc", role="default", transport_kind="rpc",
                capabilities=_caps(
                    "rpc", access_modes=_ALL_ACCESS,
                    streaming=True, tool_events=True, approval=True, user_input=True,
                    interrupt=True, resume=True, session_persistence=True, mcp=True,
                    usage_events=True,
                ),
                access_mode_notes={
                    "full-access": (
                        "Droid autonomy high, not --skip-permissions-unsafe; requests "
                        "Droid still raises are allowed once."
                    ),
                },
                notes=(
                    "Official droid-sdk stream-jsonrpc over a process_supervisor "
                    "transport. Access modes map to autonomy off/low/medium/high and "
                    "are applied on every open, resume included. Approvals offer "
                    "proceed_once and cancel only. Steer, Mission and plan mode are "
                    "not enabled. Muteki Control is injected as stdio."
                ),
            ),
            AdapterDescriptor(
                adapter_id="droid.acp", role="variant", transport_kind="acp",
                capabilities=_acp_caps(
                    access_modes=_ALL_ACCESS, steer=False, subagents=False,
                    usage_events=False, acp_mcp_config=False, mcp=True,
                ),
                notes=(
                    "droid exec --output-format acp. Sessions start at auto-high, so "
                    "autonomy_level (normal/auto-low/auto-medium/auto-high) is set from "
                    "the access mode on every open and an unconfirmed change fails the "
                    "start. Empty session/set_config_option results wait for "
                    "config_option_update. The wire carries no token usage. HTTP MCP is "
                    "not injected; Muteki Control uses the stdio bridge."
                ),
            ),
            _cli_adapter("droid", usage_events=True),
        ),
        login=LoginSpec(
            guidance_command="droid（浏览器登录）或保存 FACTORY_API_KEY",
            guidance_note=(
                "宿主登录写在 ~/.factory，不能据此认为容器已认证。"
                "容器使用账号里的 FACTORY_API_KEY。未验证会话目录隔离，不声明多账号并发。"
            ),
            status_probe="droid_login",
        ),
        credentials=CredentialSpec(
            env_resolver="droid",
            env_keys=("FACTORY_API_KEY", "FACTORY_API_KEY_FILE"),
            secret_file="API_KEY",
        ),
        models=ModelDiscoverySpec(
            method="cli", argv=("exec", "--help"), parser="droid_models",
            system_credential_live_catalog=True,
        ),
        environment=EnvironmentSpec(
            home_env_var="", home_relative=".factory", managed_home=False,
        ),
        cli=CliSpec(
            reasoning_efforts=_FULL_EFFORTS,
            effort_style="reasoning_effort_flag",
        ),
        worker=WorkerProfileSpec(
            transport="droid",
            protocol_label="Factory Droid CLI",
            official_credential_mode="api_key",
        ),
        commands=CommandCatalogSpec(client_aliases=dict(_COMMON_ALIASES)),
    ),
)


_BY_ENGINE: dict[str, ProviderDescriptor] = {
    item.identity.engine: item for item in _DESCRIPTORS
}
_BY_ADAPTER: dict[str, tuple[ProviderDescriptor, AdapterDescriptor]] = {}
for _descriptor in _DESCRIPTORS:
    for _adapter in _descriptor.adapters:
        for _key in (_adapter.adapter_id, *_adapter.legacy_aliases):
            if _key in _BY_ADAPTER:
                raise RuntimeError(f"duplicate adapter id in descriptors: {_key}")
            _BY_ADAPTER[_key] = (_descriptor, _adapter)
del _descriptor, _adapter, _key


def all_descriptors() -> tuple[ProviderDescriptor, ...]:
    return _DESCRIPTORS


def engine_ids() -> tuple[str, ...]:
    return tuple(_BY_ENGINE)


def find_descriptor(engine: str) -> Optional[ProviderDescriptor]:
    return _BY_ENGINE.get(str(engine or "").strip().lower())


def get_descriptor(engine: str) -> ProviderDescriptor:
    found = find_descriptor(engine)
    if found is None:
        raise UnknownProviderError(engine)
    return found


def _adapter_entry(adapter_id: str) -> Optional[tuple[ProviderDescriptor, AdapterDescriptor]]:
    value = str(adapter_id or "").strip()
    entry = _BY_ADAPTER.get(value)
    if entry is None and value in _BY_ENGINE:
        descriptor = _BY_ENGINE[value]
        entry = (descriptor, descriptor.default_adapter)
    return entry


def find_descriptor_for_adapter(adapter_id: str) -> Optional[ProviderDescriptor]:
    entry = _adapter_entry(adapter_id)
    return entry[0] if entry is not None else None


def descriptor_for_adapter(adapter_id: str) -> ProviderDescriptor:
    entry = _adapter_entry(adapter_id)
    if entry is None:
        raise UnknownProviderError(adapter_id, kind="adapter")
    return entry[0]


def find_adapter_descriptor(adapter_id: str) -> Optional[AdapterDescriptor]:
    entry = _adapter_entry(adapter_id)
    return entry[1] if entry is not None else None


def adapter_descriptor(adapter_id: str) -> AdapterDescriptor:
    entry = _adapter_entry(adapter_id)
    if entry is None:
        raise UnknownProviderError(adapter_id, kind="adapter")
    return entry[1]


def engines_where(predicate: Any) -> tuple[str, ...]:
    return tuple(item.identity.engine for item in _DESCRIPTORS if predicate(item))


def resolve_capabilities(
    descriptor: ProviderDescriptor,
    probe: AgentCapabilities | None,
    *,
    adapter_id: str = "",
    probe_sources: dict[str, str] | None = None,
) -> tuple[AgentCapabilities, dict[str, str]]:
    """Overlay a probe on the declared baseline.

    Every boolean field starts from the adapter's declared value with source
    ``static``. A probe that measured anything (``capability_source`` other
    than ``static``) overrides each field it measured; ``probe_sources`` (the
    probe report's per-field labels) marks fields the probe left at its own
    conservative default as unmeasured, so they keep the declaration. The
    overlaid source is the probe's label (``probe`` or ``adapter_reported``).

    ``access_modes`` is the intersection of the declared honored modes and the
    probe's modes: adapters accept modes they do not honor natively, and a
    probe can only narrow the declaration for one installation.
    """
    adapter = descriptor.adapter(adapter_id)
    declared = adapter.capabilities
    caps = declared.model_copy(deep=True)
    sources: dict[str, str] = {
        name: SOURCE_STATIC
        for name in (*BOOL_CAPABILITY_FIELDS, "access_modes", *_PROBE_LIST_FIELDS)
    }
    caps.capability_source = SOURCE_STATIC
    if probe is None:
        return caps, sources
    for name in _PROBE_SCALAR_FIELDS:
        setattr(caps, name, getattr(probe, name))
    overall = str(probe.capability_source or SOURCE_STATIC)
    if overall == SOURCE_STATIC:
        return caps, sources
    caps.capability_source = overall
    labels = dict(probe_sources or {})

    def measured(name: str) -> str:
        label = str(labels.get(name) or overall)
        return "" if label == SOURCE_STATIC else label

    for name in BOOL_CAPABILITY_FIELDS:
        label = measured(name)
        if label:
            setattr(caps, name, bool(getattr(probe, name)))
            sources[name] = label
    for name in _PROBE_LIST_FIELDS:
        label = measured(name)
        if label:
            setattr(caps, name, list(getattr(probe, name)))
            sources[name] = label
    label = measured("access_modes")
    if label:
        offered = set(probe.access_modes)
        caps.access_modes = [mode for mode in declared.access_modes if mode in offered]
        sources["access_modes"] = label
    return caps, sources


def public_descriptor(descriptor: ProviderDescriptor) -> dict[str, Any]:
    """JSON payload of one descriptor (static data only, no secrets)."""
    return descriptor.model_dump(mode="json")


__all__ = [
    "AdapterDescriptor",
    "AttachmentSpec",
    "CliSpec",
    "CommandCatalogSpec",
    "ComponentSpec",
    "CredentialEnvResolver",
    "CredentialSpec",
    "DESCRIPTOR_SCHEMA_VERSION",
    "EnvironmentImport",
    "EnvironmentSpec",
    "LoginSpec",
    "LoginStatusProbe",
    "ModelDiscoverySpec",
    "ModelParser",
    "ProviderDescriptor",
    "ProviderIdentity",
    "TransportSetting",
    "UnknownProviderError",
    "WorkerProfileSpec",
    "adapter_descriptor",
    "all_descriptors",
    "descriptor_for_adapter",
    "engine_ids",
    "engines_where",
    "find_adapter_descriptor",
    "find_descriptor",
    "find_descriptor_for_adapter",
    "get_descriptor",
    "public_descriptor",
    "resolve_capabilities",
]
