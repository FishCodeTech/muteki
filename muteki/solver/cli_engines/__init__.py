"""Split implementation of muteki.solver.cli_driver.

``cli_driver`` remains the public import path and re-exports these names.
"""
from muteki.solver.cli_engines.adapter import CliDriverAdapter, cli_adapter_for
from muteki.solver.cli_engines.argv import (
    _claude_endpoint_model_env,
    _endpoint_api_model,
    _insert_before_prompt,
    _insert_model_arg,
    apply_reasoning_effort,
    apply_runtime_argv,
)
from muteki.solver.cli_engines.base import (
    CliDriver,
    _SECURE_HELP_CACHE,
    _SECURE_HELP_LOCK,
    _secure_help_preflight,
)
from muteki.solver.cli_engines.bins import (
    KB_MCP_NAME,
    _BAD_REALPATH_MARKERS,
    _BIN_NAME,
    _ENV_OVERRIDE,
    _KNOWN_GOOD,
    _looks_bad,
    _runs_ok,
    _which_all,
    resolve_engine_bin,
    resolve_engine_bin_source,
)
from muteki.solver.cli_engines.engines.claude import ClaudeCodeDriver
from muteki.solver.cli_engines.engines.codex import CodexDriver
from muteki.solver.cli_engines.engines.cursor import CursorDriver
from muteki.solver.cli_engines.engines.devin import DevinDriver
from muteki.solver.cli_engines.engines.grok import GrokDriver
from muteki.solver.cli_engines.engines.kimi import KimiCodeDriver
from muteki.solver.cli_engines.engines.omp import OhMyPiDriver
from muteki.solver.cli_engines.engines.pi import PiDriver, PiLikeDriver
from muteki.solver.cli_engines.engines.opencode import OpenCodeDriver
from muteki.solver.cli_engines.health import (
    _CONTAINER_ENGINE_BIN,
    _HEALTH_TTL,
    _claude_oauth,
    _cursor_session_cookie,
    _engine_health_container,
    _health_cache,
    _patched_env,
    _probe_health_with_creds,
    engine_health,
    engine_liveness,
    engine_status,
)
from muteki.solver.cli_engines.process import (
    _LOCAL_PROCESS_OBSERVER,
    _LOCAL_PROCESS_OWNERS,
    _LOCAL_PROCESS_OWNERS_LOCK,
    _LocalProcessOwner,
    _descendant_pids,
    _fully_owned_process_groups,
    _host_protected_pids,
    _kill_proc_tree,
    _local_process_table,
    _observe_local_process_owners,
    _register_local_process_owner,
    _unregister_local_process_owner,
    run_cli,
    run_cli_streaming,
)
from muteki.solver.cli_engines.registry import (
    DRIVERS,
    EndpointDriver,
    ProfileDriver,
    driver_for,
    get_driver,
)
from muteki.solver.cli_engines.types import (
    CliResult,
    LaunchContext,
    LaunchPurpose,
    SecurePromptUnsupported,
    StreamStep,
    WORKER_LAUNCH,
    _cli_stderr_failure_detail,
    _structured_cli_error,
    finalize_cli_result,
)
from muteki.solver.cli_engines.adapter import (
    _agent_plugin_instructions,
)

__all__ = [
    "KB_MCP_NAME",
    "LaunchPurpose",
    "LaunchContext",
    "WORKER_LAUNCH",
    "CliResult",
    "StreamStep",
    "SecurePromptUnsupported",
    "finalize_cli_result",
    "CliDriver",
    "ClaudeCodeDriver",
    "CodexDriver",
    "CursorDriver",
    "DevinDriver",
    "PiLikeDriver",
    "PiDriver",
    "OhMyPiDriver",
    "KimiCodeDriver",
    "GrokDriver",
    "OpenCodeDriver",
    "ProfileDriver",
    "EndpointDriver",
    "DRIVERS",
    "get_driver",
    "driver_for",
    "apply_runtime_argv",
    "apply_reasoning_effort",
    "resolve_engine_bin",
    "resolve_engine_bin_source",
    "engine_health",
    "engine_liveness",
    "engine_status",
    "run_cli",
    "run_cli_streaming",
    "CliDriverAdapter",
    "cli_adapter_for",
    "_ENV_OVERRIDE",
    "_BIN_NAME",
    "_KNOWN_GOOD",
    "_BAD_REALPATH_MARKERS",
    "_looks_bad",
    "_runs_ok",
    "_which_all",
    "_structured_cli_error",
    "_cli_stderr_failure_detail",
    "_SECURE_HELP_CACHE",
    "_SECURE_HELP_LOCK",
    "_secure_help_preflight",
    "_insert_before_prompt",
    "_insert_model_arg",
    "_claude_endpoint_model_env",
    "_endpoint_api_model",
    "_HEALTH_TTL",
    "_health_cache",
    "_patched_env",
    "_probe_health_with_creds",
    "_claude_oauth",
    "_cursor_session_cookie",
    "_CONTAINER_ENGINE_BIN",
    "_engine_health_container",
    "_local_process_table",
    "_LOCAL_PROCESS_OWNERS_LOCK",
    "_LOCAL_PROCESS_OWNERS",
    "_LOCAL_PROCESS_OBSERVER",
    "_host_protected_pids",
    "_fully_owned_process_groups",
    "_observe_local_process_owners",
    "_register_local_process_owner",
    "_unregister_local_process_owner",
    "_LocalProcessOwner",
    "_descendant_pids",
    "_kill_proc_tree",
    "_agent_plugin_instructions",
]
