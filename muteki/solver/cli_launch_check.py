"""Final process-launch contract: reject illegal argv/env/cwd/stdin.

This check runs after profile, model, system prompt, and path mapping have been
applied. It never rewrites required text or switches transport. A violation is a
deterministic start failure, not a target-side UNANSWERED result.
"""
from __future__ import annotations

import os
from typing import Any, Mapping, Optional


LAUNCH_CODE_ILLEGAL = "process_input_illegal"
LAUNCH_CODE_ARGV_SIZE = "process_argv_too_large"
LAUNCH_CODE_CONTEXT = "shared_context_unready"
LAUNCH_CODE_ENVIRONMENT = "worker_environment_unavailable"


class LaunchContractError(RuntimeError):
    """The reconstructed process invocation cannot be started."""

    def __init__(
        self,
        detail: str,
        *,
        code: str,
        field: str = "",
        source: str = "",
    ) -> None:
        super().__init__(detail)
        self.code = code
        self.field = field
        self.source = source


def _nul_field(label: str, value: Any) -> str:
    if "\x00" in str(value or ""):
        return label
    return ""


def _arg_max_bytes() -> int:
    try:
        return int(os.sysconf("SC_ARG_MAX"))
    except (AttributeError, OSError, ValueError):
        return 256 * 1024


def _utf8_size(value: Any) -> int:
    return len(str(value or "").encode("utf-8", errors="surrogateescape"))


def check_process_launch(
    argv: list[str],
    *,
    cwd: str = "",
    env: Optional[Mapping[str, Any]] = None,
    stdin_text: Optional[str] = None,
    source: str = "process",
) -> None:
    """Raise ``LaunchContractError`` when the invocation cannot cross exec/stdin."""
    for index, item in enumerate(argv):
        field = _nul_field(f"argv[{index}]", item)
        if field:
            raise LaunchContractError(
                f"{source}: {field} contains a NUL byte",
                code=LAUNCH_CODE_ILLEGAL,
                field=field,
                source=source,
            )
        # Linux rejects an individual argv/env string at MAX_ARG_STRLEN
        # (128 KiB) even when the aggregate ARG_MAX budget still has room.
        if _utf8_size(item) + 1 > 128 * 1024:
            raise LaunchContractError(
                f"{source}: argv[{index}] is {_utf8_size(item)} bytes; "
                "exceeds the per-argument launch limit",
                code=LAUNCH_CODE_ARGV_SIZE,
                field=f"argv[{index}]",
                source=source,
            )
    cwd_field = _nul_field("cwd", cwd)
    if cwd_field:
        raise LaunchContractError(
            f"{source}: cwd contains a NUL byte",
            code=LAUNCH_CODE_ILLEGAL,
            field=cwd_field,
            source=source,
        )
    if env is not None:
        for key, value in env.items():
            field = _nul_field(f"env.{key}", key) or _nul_field(f"env.{key}", value)
            if field:
                raise LaunchContractError(
                    f"{source}: {field} contains a NUL byte",
                    code=LAUNCH_CODE_ILLEGAL,
                    field=field,
                    source=source,
                )
            if _utf8_size(key) + _utf8_size(value) + 2 > 128 * 1024:
                raise LaunchContractError(
                    f"{source}: env.{key} exceeds the per-string launch limit",
                    code=LAUNCH_CODE_ARGV_SIZE,
                    field=f"env.{key}",
                    source=source,
                )
    if stdin_text is not None:
        field = _nul_field("stdin", stdin_text)
        if field:
            raise LaunchContractError(
                f"{source}: stdin contains a NUL byte",
                code=LAUNCH_CODE_ILLEGAL,
                field=field,
                source=source,
            )
    argv_bytes = sum(_utf8_size(item) + 1 for item in argv)
    env_bytes = 0
    if env is not None:
        env_bytes = sum(_utf8_size(key) + _utf8_size(value) + 2 for key, value in env.items())
    budget = max(4096, _arg_max_bytes() - 4096)
    if argv_bytes + env_bytes > budget:
        raise LaunchContractError(
            f"{source}: argv+env is {argv_bytes + env_bytes} bytes; "
            f"exceeds launch budget {budget}",
            code=LAUNCH_CODE_ARGV_SIZE,
            field="argv",
            source=source,
        )


def launch_failure_code(exc: BaseException) -> str:
    """Classify a deterministic launch/input failure, or return empty."""
    if isinstance(exc, LaunchContractError):
        return str(exc.code or LAUNCH_CODE_ILLEGAL)
    code = str(getattr(exc, "code", "") or "")
    if code in {
        LAUNCH_CODE_ILLEGAL,
        LAUNCH_CODE_ARGV_SIZE,
        LAUNCH_CODE_CONTEXT,
        LAUNCH_CODE_ENVIRONMENT,
        "provider_config_missing",
        "model_catalog_missing",
        "worker_spawn_rejected",
    }:
        return code
    return ""
