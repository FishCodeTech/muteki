"""ExternalAgentAdapter 层（RUNTIME-01/02/03，任务书 6.5 / 6.7 / 7.1–7.6）。

- ``base``：Adapter 抽象骨架（instance identity、注入计划选择、
  SessionStart 的 Binding 七步交付、typed unsupported receipt）。
- ``registry``：Adapter Registry（``adapter_id + instance_id`` 多实例、
  probe 缓存与健康快照）。
- ``capabilities``：capability probe 框架与 MCP 优先的注入方式选择。
- ``events``：统一 AgentEvent 构造、会话内单调序号与完整 payload。
- ``sessions``：Session/Turn identity、generation fencing、退出分类。
- ``rpc``：共享 stdio JSONL / JSON-RPC 子进程 transport（严格 LF 分帧、
  请求关联、反向请求应答）。
- ``acp``：ACP v1 transport 与 ``BaseAcpAdapter``（replay/live 区分、
  mcpServers 注入、request_permission 审批策略）。
- ``cursor`` / ``grok`` / ``pi_acp``：RUNTIME-03 的结构化 Adapter
  （Cursor、Grok 默认走 ACP；Pi ACP 仅保留为显式兼容入口）。
- ``codex``：Codex app-server stdio JSON-RPC 结构化 Adapter（RUNTIME-02）。
- ``claude``：Claude Agent SDK 结构化 Adapter（RUNTIME-02）。
- ``opencode`` / ``kimi`` / ``omp``：RUNTIME-04
  的结构化 Adapter（OpenCode Server HTTP/SSE；Kimi ACP + Local Server；
  Pi、OMP 默认使用能够公布完整命令集的原生 RPC，OMP ACP 保留为可选传输）。

CLI 兼容路径见
``muteki.solver.cli_driver.CliDriverAdapter``（本包不 import solver 层）。
"""

from .acp import (
    ACP_PROTOCOL_VERSION,
    AcpError,
    AcpHello,
    AcpTransport,
    BaseAcpAdapter,
    materialize_mcp_servers,
    normalize_session_update,
)
from .base import (
    AdapterIdentity,
    BaseExternalAgentAdapter,
    UNSUPPORTED_CAPABILITY_CODE,
)
from .capabilities import (
    CapabilityProbeReport,
    probe_cli_driver,
    select_injection_kind,
)
from .claude import ClaudeSDKAdapter
from .codex import CodexAppServerAdapter
from .cursor import CursorAcpAdapter
from .devin import DevinAcpAdapter
from .events import EventSequencer, build_event
from .factory import (
    DEFAULT_ADAPTER_BY_ENGINE,
    DEFAULT_CLI_ADAPTER_BY_ENGINE,
    DEFAULT_STRUCTURED_ADAPTER_BY_ENGINE,
    RuntimeAdapterConfig,
    RuntimeAdapterFactory,
    STRUCTURED_ADAPTER_ENGINES,
    canonical_adapter_id,
    engine_for_adapter,
    runtime_config_schema,
)
from .grok import GrokAcpAdapter
from .kimi import KimiAcpAdapter, KimiLocalServerAdapter
from .omp import OmpAcpAdapter, OmpRpcAdapter
from .opencode import OpenCodeServerAdapter
from .pi import PiAdapter
from .pi_acp import PI_ACP_PACKAGE, PiAcpAdapter
from .registry import AdapterInstanceRecord, AdapterRegistry
from .rpc import JsonLineFramer, PeerClosedError, StdioJsonlPeer
from .sessions import (
    EXIT_CLOSED,
    EXIT_FAILED,
    EXIT_INTERRUPTED,
    EXIT_RESUMABLE,
    EventProjector,
    SessionTracker,
    classify_exit,
)

__all__ = [
    "ACP_PROTOCOL_VERSION",
    "AdapterIdentity",
    "AdapterInstanceRecord",
    "AdapterRegistry",
    "AcpError",
    "AcpHello",
    "AcpTransport",
    "BaseAcpAdapter",
    "BaseExternalAgentAdapter",
    "CapabilityProbeReport",
    "ClaudeSDKAdapter",
    "CodexAppServerAdapter",
    "CursorAcpAdapter",
    "DevinAcpAdapter",
    "EXIT_CLOSED",
    "EXIT_FAILED",
    "EXIT_INTERRUPTED",
    "EXIT_RESUMABLE",
    "EventProjector",
    "EventSequencer",
    "DEFAULT_ADAPTER_BY_ENGINE",
    "DEFAULT_CLI_ADAPTER_BY_ENGINE",
    "DEFAULT_STRUCTURED_ADAPTER_BY_ENGINE",
    "GrokAcpAdapter",
    "JsonLineFramer",
    "KimiAcpAdapter",
    "KimiLocalServerAdapter",
    "OmpAcpAdapter",
    "OmpRpcAdapter",
    "OpenCodeServerAdapter",
    "PeerClosedError",
    "PiAdapter",
    "PiAcpAdapter",
    "PI_ACP_PACKAGE",
    "RuntimeAdapterConfig",
    "RuntimeAdapterFactory",
    "SessionTracker",
    "StdioJsonlPeer",
    "STRUCTURED_ADAPTER_ENGINES",
    "UNSUPPORTED_CAPABILITY_CODE",
    "build_event",
    "canonical_adapter_id",
    "classify_exit",
    "engine_for_adapter",
    "materialize_mcp_servers",
    "normalize_session_update",
    "probe_cli_driver",
    "runtime_config_schema",
    "select_injection_kind",
]
