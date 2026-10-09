"""Codex app-server 结构化 Adapter（RUNTIME-02，任务书 7.2）。

传输与协议以 2026-08-21 核验结论为准
（docs/research/third_party_verification.md §CODEX，本机实测 codex-cli
0.147.0，schema 经 ``codex app-server generate-json-schema`` 导出复核）：

- 传输：stdio JSON-RPC（JSONL；服务器出站省略 ``"jsonrpc":"2.0"`` 头，
  入站携带该头已实测兼容）。websocket ``--listen`` 仍为 experimental，
  不作默认传输。底层分帧 / 请求关联复用共享 ``rpc.StdioJsonlPeer``。
- 握手：``initialize``（clientInfo{name,title,version}，可选
  ``capabilities.experimentalApi``）+ ``initialized`` 通知。
- Thread/Turn：``thread/start|resume|fork``、``turn/start|steer|interrupt``；
  turn 级 ``effort`` 覆盖、thread 级 ``model``。
- 审批与用户输入是**服务器→客户端的 JSON-RPC request**
 （``item/commandExecution/requestApproval``、``item/fileChange/requestApproval``、
  ``item/permissions/requestApproval``、``item/tool/requestUserInput``），
  Adapter 以 JSON-RPC response 应答；decision 取值
  ``accept / acceptForSession / decline / cancel``（schema 已核对）。
- Codex 0.154 的异步问题走 agentMessage(delivery=async, questions)，
  答案通过 turn/steer 或同 thread 的下一原生 turn 送回；
  委派活动走 collabAgentToolCall / subAgentActivity。
- token 用量走独立通知 ``thread/tokenUsage/updated``（``turn/completed``
  的 ``turn`` 里**没有** usage 字段，README 描述有误导，以源码为准）。
- MCP 注入按 T3 nightly：``thread/start|resume|fork`` 的 ``config.mcp_servers``
  携带每个目标线程的 URL 与内存 Authorization header；不在 app-server argv
  或全局环境中投递会话 token。``mcpServerStatus/list`` 按 threadId 分页查询。
- capability discovery：probe 用 ``generate-json-schema`` 导出当版
  schema，按 ClientRequest / ServerRequest / ServerNotification 的方法
  清单逐项判定能力，版本升级后方法面变化会如实反映；schema 导出失败时
  显式降级并在 ``degradations`` 说明。

CLI 兼容降级路径保持在 ``muteki.solver.cli_driver.CliDriverAdapter``
（``cli_adapter_for("codex")``，``codex exec --json``），本模块不 import
solver 层；structured transport 不可用时 probe 会在 degradations 里指向
该降级路径，不静默关闭审批 / 恢复 / 来源追踪。
"""

from __future__ import annotations

import asyncio
import hashlib
import json
import logging
import os
import subprocess
import tomllib

from .probe_environment import subprocess_environment
import tempfile
from pathlib import Path
from typing import Any, AsyncIterator, Mapping, Optional

from muteki.capability_bindings import acp_config
from muteki.platform.contracts.base import new_id
from muteki.platform.contracts.capabilities import CapabilityInjectionPlan, InjectionKind
from muteki.platform.contracts.external_agents import (
    ACCESS_MODE_VALUES,
    AccessMode,
    AgentCapabilities,
    AgentEvent,
    AgentEventType,
    AgentInput,
    ApprovalResponseInput,
    MessageInput,
    SteerInput,
    UserInputResponseInput,
    AgentSessionRef,
    ProbeRequest,
    SessionStart,
)
from muteki.platform.contracts.protocols import NativeRewindAdapter, RuntimeOperationAdapter
from muteki.platform.contracts.receipts import AggregateRef, CommandReceipt, ReceiptState

from .base import BaseExternalAgentAdapter, TurnLimits, TurnRunner
from .approvals import ApprovalDecision
from .attachment_input import codex_turn_input
from .command_providers import codex_review_target
from .capabilities import (
    BOOL_CAPABILITY_FIELDS,
    CapabilityProbeReport,
    SOURCE_PROBE,
    SOURCE_REPORTED,
    SOURCE_STATIC,
    conservative_capabilities,
)
from .events import build_event
from muteki.platform.contracts.agent_events import (
    AgentNodePayload,
    AgentUpdatedPayload,
    ApprovalRequestedPayload,
    ApprovalResolvedPayload,
    FailureCategory,
    MessageCompletedPayload,
    MessageDeltaPayload,
    PlanPayload,
    PlanTaskPayload,
    RateLimitState,
    ReasoningPayload,
    RuntimeErrorPayload,
    RuntimeExitedPayload,
    RuntimeWarningPayload,
    SessionPayload,
    ToolPayload,
    TurnCompletedPayload,
    TurnFailedPayload,
    TurnStartedPayload,
    UsagePayload,
    UserInputRequestedPayload,
    UserInputResolvedPayload,
    WorkspaceChangedPayload,
    WorkspaceFileChange,
    dump_payload,
)
from .rpc import PeerClosedError, StdioJsonlPeer
from .runtime_capabilities import (
    RuntimeCapabilityItem,
    RuntimeCapabilitySnapshot,
    dynamic_command_item,
)
from .sessions import EXIT_CLOSED, EXIT_FAILED, classify_exit
from .user_input_schema import (
    answers_for_codex_tool,
    content_for_elicitation,
    normalize_question,
    questions_from_codex_params,
)

#: 默认 binary 与 MCP server 名（与 CAP-02 acp_config 默认一致）。
DEFAULT_CODEX_BIN = "codex"
MCP_SERVER_NAME = acp_config.DEFAULT_SERVER_NAME

#: bearer token 经该环境变量进入 app-server 子进程（T3 同名机制核验；
#: 变量名进 ``-c mcp_servers.<name>.bearer_token_env_var``，token 本体不进 argv）。
MCP_TOKEN_ENV = "MUTEKI_CAPABILITY_TOKEN"

_LOG = logging.getLogger(__name__)

#: probe / 注入用的稳定 wire 方法名（实测 0.147.0 schema 复核）。
M_INITIALIZE = "initialize"
M_INITIALIZED = "initialized"
M_THREAD_START = "thread/start"
M_THREAD_RESUME = "thread/resume"
M_THREAD_UNSUBSCRIBE = "thread/unsubscribe"
M_THREAD_FORK = "thread/fork"
M_TURN_START = "turn/start"
M_TURN_STEER = "turn/steer"
M_TURN_INTERRUPT = "turn/interrupt"
M_MCP_RELOAD = "config/mcpServer/reload"
M_MODEL_LIST = "model/list"
M_MCP_STATUS = "mcpServerStatus/list"
M_SKILLS_LIST = "skills/list"
M_HOOKS_LIST = "hooks/list"
M_PLUGIN_LIST = "plugin/list"
M_APP_LIST = "app/list"
M_MCP_ELICITATION = "mcpServer/elicitation/request"

# Measured on codex-cli 0.160.1 with a temporary CODEX_HOME. The user's
# ~/.codex was not read or written.
# - `codex features list` reports collaboration_modes as removed/true.
# - `codex app-server generate-json-schema` writes v2/TurnStartParams.json.
#   Its properties omit collaborationMode; definitions still contain
#   CollaborationMode (mode plan|default, settings.model required).
# - stdio app-server: turn/start with collaborationMode and no experimentalApi
#   returns JSON-RPC -32600. The same request after
#   capabilities.experimentalApi is accepted and returns an inProgress turn.
# - `codex --strict-config -c tools.update_plan.enabled=true app-server`
#   accepts the key. `tools.definitely_not_real` is rejected as an unknown
#   configuration field. Thread config in the schema is additionalProperties.
_PLAN_MODE_DEGRADATION = (
    "codex-cli 0.160.1（临时 CODEX_HOME，未改 ~/.codex）："
    "features list 里 collaboration_modes 为 removed；"
    "generate-json-schema 的 v2/TurnStartParams.properties 省略 collaborationMode，"
    "definitions 仍有 CollaborationMode。"
    "stdio 实测未声明 experimentalApi 时 turn/start.collaborationMode 返回 JSON-RPC -32600，"
    "声明 experimentalApi 后请求被接受并得到 inProgress turn。"
    "tools.update_plan.enabled 被 --strict-config 接受，"
    "未知键 tools.definitely_not_real 被拒绝。"
)
_THREAD_UPDATE_PLAN_CONFIG = {"tools.update_plan.enabled": True}

#: 审批类 server-request（响应 ``{"decision": ...}``）。
APPROVAL_REQUEST_METHODS = {
    "item/commandExecution/requestApproval": "command_execution",
    "item/fileChange/requestApproval": "file_change",
    "item/permissions/requestApproval": "permissions",
    # v1 遗留名（旧版 app-server 仍可能发）。
    "execCommandApproval": "command_execution",
    "applyPatchApproval": "file_change",
}
#: 用户输入类 server-request（experimental；响应 ``{"answers": {...}}``）。
USER_INPUT_REQUEST_METHODS = {
    "item/tool/requestUserInput": "tool_user_input",
    M_MCP_ELICITATION: "mcp_elicitation",
}
#: 合法审批 decision（CommandExecution/FileChange schema 共有子集）。
APPROVAL_DECISIONS = ("accept", "acceptForSession", "decline", "cancel")

def _as_preview_str(value: Any) -> str:
    if value is None:
        return ""
    if isinstance(value, str):
        return value.strip()
    if isinstance(value, (list, tuple)):
        parts = [str(item).strip() for item in value if str(item).strip()]
        return " ".join(parts)
    return str(value).strip()


def _file_change_from_v1_entry(path: str, change: Any) -> dict[str, Any] | None:
    """Normalize applyPatchApproval file_changes map entry → {path,status,diff}."""
    path = _as_preview_str(path)
    if not path:
        return None
    row: dict[str, Any] = {"path": path}
    if not isinstance(change, Mapping):
        text = _as_preview_str(change)
        if text:
            row["diff"] = text
        return row
    kind = _as_preview_str(
        change.get("type") or change.get("kind") or change.get("status")
    ).lower()
    if kind:
        row["status"] = kind
    diff = _as_preview_str(
        change.get("diff")
        or change.get("unified_diff")
        or change.get("unifiedDiff")
        or change.get("patch")
        or change.get("content")
    )
    if diff:
        # Add/Delete content is full-file text; wrap when no unified header.
        if kind in {"add", "delete"} and not diff.lstrip().startswith(
            ("diff ", "@@", "--- ")
        ):
            if kind == "add":
                body = "".join(f"+{line}\n" for line in diff.splitlines())
                diff = f"--- /dev/null\n+++ b/{path}\n@@\n{body}"
            else:
                body = "".join(f"-{line}\n" for line in diff.splitlines())
                diff = f"--- a/{path}\n+++ /dev/null\n@@\n{body}"
        row["diff"] = diff
    move_to = _as_preview_str(change.get("move_path") or change.get("movePath"))
    if move_to:
        row["move_path"] = move_to
    return row


def _normalize_file_change_entries(raw: Any) -> list[dict[str, Any]]:
    """Accept v2 changes[] list or v1 file_changes {path: FileChange} map."""
    out: list[dict[str, Any]] = []
    if isinstance(raw, list):
        for item in raw:
            if not isinstance(item, Mapping):
                continue
            path = _as_preview_str(
                item.get("path") or item.get("filename") or item.get("file")
            )
            if not path:
                continue
            row: dict[str, Any] = {"path": path}
            status = _as_preview_str(
                item.get("kind") or item.get("status") or item.get("change_type")
            )
            if status:
                row["status"] = status
            diff = _as_preview_str(
                item.get("diff")
                or item.get("patch")
                or item.get("unified_diff")
                or item.get("unifiedDiff")
                or item.get("content")
            )
            if diff:
                row["diff"] = diff
            out.append(row)
        return out
    if isinstance(raw, Mapping):
        for path_key, change in raw.items():
            row = _file_change_from_v1_entry(str(path_key), change)
            if row:
                out.append(row)
    return out


def _compose_file_change_diff(files: list[Mapping[str, Any]]) -> str:
    chunks: list[str] = []
    for item in files:
        path = _as_preview_str(item.get("path"))
        diff = _as_preview_str(item.get("diff"))
        if not diff:
            continue
        if path and not diff.lstrip().startswith(("diff ", "--- ")):
            chunks.append(f"--- a/{path}\n+++ b/{path}\n{diff}")
        else:
            chunks.append(diff)
    return "\n".join(chunks)


def _mcp_result_text(result: Any) -> Any:
    """MCP tool result → display text; image blocks become a size marker.

    The model already received the image; tools that return one also return
    its saved path and SHA-256 in the text block, so the event log keeps the
    reference instead of a base64 copy.
    """
    if not isinstance(result, Mapping) or not isinstance(result.get("content"), list):
        return result
    parts: list[str] = []
    for block in result["content"]:
        if not isinstance(block, Mapping):
            continue
        if block.get("type") == "text":
            parts.append(str(block.get("text") or ""))
        elif block.get("type") == "image":
            size = len(str(block.get("data") or "")) * 3 // 4
            parts.append(f"[image {block.get('mimeType') or 'unknown'} ~{size} bytes]")
    return "\n".join(parts)


def _cache_file_change_item(ctx: dict[str, Any], item: Mapping[str, Any]) -> None:
    """Remember fileChange item.changes keyed by item id for later approval join.

    Codex ``item/fileChange/requestApproval`` params only carry itemId / reason /
    grantRoot — path + Diff live on the preceding ``item/started`` fileChange
    item (see app-server README "File change approvals").
    """
    item_id = _as_preview_str(item.get("id") or item.get("itemId"))
    if not item_id:
        return
    files = _normalize_file_change_entries(
        item.get("changes") or item.get("files") or item.get("file_changes")
    )
    if not files:
        return
    cache = ctx.setdefault("file_change_items", {})
    cache[item_id] = {
        "files": files,
        "diff": _compose_file_change_diff(files),
        "path": files[0]["path"] if len(files) == 1 else "",
        "paths": [row["path"] for row in files],
    }


def _file_change_preview_from_ctx(
    params: Mapping[str, Any],
    ctx: Mapping[str, Any],
) -> dict[str, Any]:
    """Join approval params with cached item/started fileChange preview."""
    item_id = _as_preview_str(
        params.get("itemId") or params.get("item_id") or params.get("call_id")
    )
    cached: dict[str, Any] = {}
    if item_id:
        cached = dict((ctx.get("file_change_items") or {}).get(item_id) or {})

    files = _normalize_file_change_entries(
        params.get("files")
        or params.get("changes")
        or params.get("file_changes")
        or params.get("fileChanges")
    )
    if not files:
        files = list(cached.get("files") or [])

    diff = _as_preview_str(
        params.get("diff")
        or params.get("patch")
        or params.get("unifiedDiff")
        or params.get("unified_diff")
        or cached.get("diff")
    )
    if not diff and files:
        diff = _compose_file_change_diff(files)

    path = _as_preview_str(
        params.get("path")
        or params.get("file")
        or params.get("filename")
        or cached.get("path")
    )
    if not path and len(files) == 1:
        path = files[0]["path"]

    cwd = _as_preview_str(
        params.get("cwd")
        or params.get("workdir")
        or params.get("working_directory")
        or params.get("grantRoot")
        or params.get("grant_root")
    )

    out: dict[str, Any] = {}
    if item_id:
        out["item_id"] = item_id
    if files:
        out["files"] = files
    if diff:
        out["diff"] = diff
    if path:
        out["path"] = path
    paths = [row["path"] for row in files] if files else list(cached.get("paths") or [])
    if paths:
        out["paths"] = paths
    if cwd:
        out["cwd"] = cwd
    return out



def _access_config(access_mode: str) -> dict[str, Any]:
    """T3 Code/Codex app-server 使用的 thread + turn 原生权限配置。"""
    try:
        mode = AccessMode(access_mode)
    except ValueError as exc:
        raise ValueError(f"codex unsupported access mode: {access_mode!r}") from exc
    if mode is AccessMode.SUPERVISED:
        return {
            "approvalPolicy": "untrusted",
            "sandbox": "read-only",
            "sandboxPolicy": {"type": "readOnly"},
            "approvalsReviewer": "user",
        }
    if mode is AccessMode.AUTO_ACCEPT_EDITS:
        return {
            "approvalPolicy": "on-request",
            "sandbox": "workspace-write",
            "sandboxPolicy": {"type": "workspaceWrite"},
            "approvalsReviewer": "user",
        }
    if mode is AccessMode.AUTO:
        return {
            "approvalPolicy": "on-request",
            "sandbox": "workspace-write",
            "sandboxPolicy": {"type": "workspaceWrite"},
            "approvalsReviewer": "auto_review",
        }
    return {
        "approvalPolicy": "never",
        "sandbox": "danger-full-access",
        "sandboxPolicy": {"type": "dangerFullAccess"},
        "approvalsReviewer": "user",
    }


def _codex_rate_limit(params: dict[str, Any]) -> dict[str, Any]:
    """Normalize ``account/rateLimits/updated`` (RateLimitSnapshot).

    ``rateLimitReachedType`` or a window at 100% means the account is
    limited; ``resets_at`` is the latest reset among the exhausted windows.
    """
    snapshot = params.get("rateLimits") if isinstance(
        params.get("rateLimits"), dict) else {}
    windows = [w for w in (snapshot.get("primary"), snapshot.get("secondary"))
               if isinstance(w, dict)]
    exhausted = [w for w in windows if int(w.get("usedPercent") or 0) >= 100]
    reached = snapshot.get("rateLimitReachedType")
    limited = bool(reached) or bool(exhausted)
    resets = [int(w["resetsAt"]) for w in (exhausted or windows)
              if isinstance(w.get("resetsAt"), (int, float))]
    peak = max((int(w.get("usedPercent") or 0) for w in windows), default=0)
    return {
        "limited": limited,
        "warning": not limited and peak >= 90,
        "resets_at": max(resets) if limited and resets else None,
        "kind": reached or snapshot.get("limitName") or None,
        "utilization": peak / 100 if windows else None,
    }


_FILE_CHANGE_KINDS = {
    "add": "add", "added": "add", "create": "add", "created": "add",
    "modify": "modify", "modified": "modify", "update": "modify",
    "updated": "modify", "edit": "modify",
    "delete": "delete", "deleted": "delete", "remove": "delete",
    "rename": "rename", "renamed": "rename", "move": "rename",
}


def _codex_approval_outcome(decision: str) -> str:
    """Codex native decision verbs → contract outcome literals."""
    return {
        "accept": "allow", "acceptForSession": "allow", "approve": "allow",
        "allow": "allow",
        "decline": "deny", "deny": "deny", "reject": "deny",
        "cancel": "cancelled", "cancelled": "cancelled",
    }.get(str(decision or ""), "deny")


def _codex_plan_payload(params: dict[str, Any], *, patch: bool) -> dict[str, Any]:
    """Normalize Codex turn/plan/updated and item/plan/delta into PLAN_UPDATED."""
    plan = params.get("plan")
    tasks: list[Any] = []
    title = ""
    if isinstance(plan, dict):
        title = str(plan.get("title") or plan.get("name") or "")
        for key in ("steps", "tasks", "entries", "items"):
            value = plan.get(key)
            if isinstance(value, list):
                tasks = value
                break
    elif isinstance(plan, list):
        tasks = plan
    item = params.get("item")
    if isinstance(item, dict) and not tasks:
        item_type = str(item.get("type") or "").lower()
        if "plan" in item_type or any(
            key in item for key in ("title", "content", "status", "steps")
        ):
            nested = item.get("steps") or item.get("tasks") or item.get("entries")
            if isinstance(nested, list):
                tasks = nested
            else:
                tasks = [item]
            title = title or str(item.get("title") or item.get("name") or "")
    if not tasks:
        for key in ("steps", "tasks", "entries", "items"):
            value = params.get(key)
            if isinstance(value, list):
                tasks = value
                break
    phase = "executing" if patch else "proposed"
    if any(
        str((row or {}).get("status") or "").lower() in (
            "in_progress", "running", "active",
        )
        for row in tasks
        if isinstance(row, dict)
    ):
        phase = "executing"
    def _task(row: Any, index: int) -> Optional[PlanTaskPayload]:
        if not isinstance(row, dict):
            if str(row or "").strip():
                return PlanTaskPayload(
                    task_id=f"task-{index}", title=str(row).strip())
            return None
        title = str(
            row.get("step") or row.get("title") or row.get("content")
            or row.get("name") or "").strip()
        if not title:
            return None
        status = {
            "inprogress": "in_progress", "in_progress": "in_progress",
            "running": "in_progress", "active": "in_progress",
            "completed": "completed", "complete": "completed",
            "done": "completed", "cancelled": "cancelled",
            "canceled": "cancelled", "blocked": "blocked",
        }.get(str(row.get("status") or "").strip().lower(), "pending")
        return PlanTaskPayload(
            task_id=str(row.get("id") or row.get("task_id") or f"task-{index}"),
            title=title, status=status)

    return dump_payload(PlanPayload(
        tasks=[task for index, row in enumerate(tasks)
               if (task := _task(row, index)) is not None],
        title=title or None,
        phase=phase,
        patch=patch,
        native=params,
    ))


#: capability discovery 需要的 schema 文件 → 方法清单键。
_SCHEMA_FILES = {
    "ClientRequest.json": "client_requests",
    "ServerRequest.json": "server_requests",
    "ServerNotification.json": "server_notifications",
}


class JsonRpcError(RuntimeError):
    """app-server 返回的 JSON-RPC error（code/message/data 保留）。"""

    def __init__(self, code: int, message: str, data: Any = None) -> None:
        super().__init__(f"json-rpc error {code}: {message}")
        self.code = code
        self.message = message
        self.data = data


class CodexPeerClosedError(JsonRpcError):
    """app-server 进程在请求未完成时退出；``reason``/``exit_code`` 来自 ``PeerClosedError``。"""

    def __init__(self, message: str, *, reason: str,
                 exit_code: Optional[int]) -> None:
        super().__init__(-32099, message)
        self.reason = reason
        self.exit_code = exit_code


class _PlanModeUnavailable(Exception):
    """Plan turn blocked before ``turn/start`` (no collaborationMode sent)."""


class ModelCatalogProtocolError(ValueError):
    code = "codex.model_catalog.protocol_invalid"

    def __init__(self, message: str, pages: list[Any]) -> None:
        super().__init__(message)
        self.catalog_pages = pages


async def read_model_catalog(conn: Any) -> dict[str, Any]:
    """Read the native picker catalog completely, including per-model efforts."""
    models: dict[str, dict[str, Any]] = {}
    cursor: Optional[str] = None
    seen_cursors: set[str] = set()
    pages: list[Any] = []
    default_model = ""
    while True:
        params: dict[str, Any] = {"limit": 50}
        if cursor is not None:
            params["cursor"] = cursor
        try:
            result = await conn.request(M_MODEL_LIST, params, timeout=30)
        except (JsonRpcError, asyncio.TimeoutError) as exc:
            exc.catalog_pages = pages
            raise
        pages.append(result)
        if not isinstance(result, dict) or not isinstance(result.get("data"), list):
            raise ModelCatalogProtocolError("model/list response must contain a data array", pages)
        for raw in result["data"]:
            if not isinstance(raw, dict):
                raise ModelCatalogProtocolError("model/list model must be an object", pages)
            model = raw.get("model") or raw.get("id")
            if not isinstance(model, str) or not model.strip():
                raise ModelCatalogProtocolError("model/list model identity must be a nonempty string", pages)
            model = model.strip()
            label = raw.get("displayName", model)
            levels = raw.get("supportedReasoningEfforts", [])
            if not isinstance(label, str) or not isinstance(levels, list):
                raise ModelCatalogProtocolError(f"model/list metadata is invalid for {model}", pages)
            efforts: list[str] = []
            for option in levels:
                effort = option.get("reasoningEffort") if isinstance(option, dict) else None
                if not isinstance(effort, str) or not effort:
                    raise ModelCatalogProtocolError(f"model/list reasoning option is invalid for {model}", pages)
                if effort not in efforts:
                    efforts.append(effort)
            default_effort = raw.get("defaultReasoningEffort", "")
            if not isinstance(default_effort, str):
                raise ModelCatalogProtocolError(f"model/list default reasoning is invalid for {model}", pages)
            raw_tiers = raw.get("serviceTiers") or []
            if not isinstance(raw_tiers, list):
                raise ModelCatalogProtocolError(f"model/list service tiers are invalid for {model}", pages)
            service_tiers: list[dict[str, str]] = []
            for tier in raw_tiers:
                tier_id = tier.get("id") if isinstance(tier, dict) else None
                if not isinstance(tier_id, str) or not tier_id.strip():
                    raise ModelCatalogProtocolError(f"model/list service tier is invalid for {model}", pages)
                if any(row["id"] == tier_id.strip() for row in service_tiers):
                    continue
                service_tiers.append({
                    "id": tier_id.strip(),
                    "name": str(tier.get("name") or tier_id).strip(),
                    "description": str(tier.get("description") or "").strip(),
                })
            default_tier = raw.get("defaultServiceTier")
            models[model] = {"id": model, "label": label or model, "reasoning": {
                "supported": bool(efforts), "levels": efforts,
                "default": default_effort if default_effort in efforts else "",
                "kind": "effort", "source": "codex.model/list",
            }, "service_tiers": service_tiers,
                "default_service_tier": (
                    default_tier if isinstance(default_tier, str)
                    and any(row["id"] == default_tier for row in service_tiers) else "")}
            if raw.get("isDefault") is True:
                default_model = model
        next_cursor = result.get("nextCursor")
        if next_cursor is None:
            break
        if not isinstance(next_cursor, str) or not next_cursor or next_cursor in seen_cursors:
            raise ModelCatalogProtocolError("model/list pagination cursor is invalid or repeated", pages)
        seen_cursors.add(next_cursor)
        cursor = next_cursor
    return {"ok": True, "models": list(models.values()),
            "default_model": default_model, "source": "codex.model/list",
            "evidence": {"method": M_MODEL_LIST, "format": "decoded_rpc_result_pages",
                         "complete": True, "pages": pages}}


class CodexPeer:
    """一条 codex app-server stdio JSON-RPC 连接（``StdioJsonlPeer`` 封装）。

    - ``request`` 把 error 响应转为 ``JsonRpcError``；
    - 入站非响应消息按是否带 ``id`` 分为服务器 request / 通知，统一进
      ``incoming`` 队列；reader EOF（进程退出）时放入 ``("eof", None)``；
    - stderr 由 ``StdioJsonlPeer`` 唯一的 reader 排空（app-server 日志走
      stderr，pipe 不消费会撑满阻塞）；退出详情读取其保留的尾部。
    """

    def __init__(self, peer: StdioJsonlPeer) -> None:
        self._peer = peer
        self._eof_watch: Optional[asyncio.Future] = None
        self.incoming: "asyncio.Queue[tuple[str, Any]]" = asyncio.Queue()
        self._thread_queues: dict[str, asyncio.Queue[tuple[str, Any]]] = {}
        self._thread_parents: dict[str, str] = {}
        self._claimed_threads: set[str] = set()
        self._released_threads: set[str] = set()
        self._closed = False
        self.launch_signature: Any = None

    def queue_for(self, thread_id: str) -> asyncio.Queue[tuple[str, Any]]:
        self._claimed_threads.add(thread_id)
        self._released_threads.discard(thread_id)
        return self._queue_for(thread_id)

    def _queue_for(self, thread_id: str) -> asyncio.Queue[tuple[str, Any]]:
        if thread_id not in self._thread_queues:
            queue: asyncio.Queue[tuple[str, Any]] = asyncio.Queue()
            self._thread_queues[thread_id] = queue
            if self._closed:
                queue.put_nowait(("eof", None))
        return self._thread_queues[thread_id]

    def _thread_owner(self, thread_id: str) -> Optional[str]:
        # A resumed/forked Muteki root owns its own stream. Native subagents
        # belong to their nearest loaded ancestor, as in T3's Codex adapter.
        seen: set[str] = set()
        while thread_id not in self._claimed_threads:
            if thread_id in seen or thread_id in self._released_threads:
                return None
            seen.add(thread_id)
            parent = self._thread_parents.get(thread_id)
            if not parent:
                return thread_id
            thread_id = parent
        return thread_id

    def release_thread(self, thread_id: str) -> None:
        owned = [key for key in self._thread_queues
                 if self._thread_owner(key) == thread_id]
        self._claimed_threads.discard(thread_id)
        self._released_threads.add(thread_id)
        for key in owned:
            self._thread_queues.pop(key, None)

    def bind_subagent(self, thread_id: str, parent: str) -> None:
        if not thread_id or not parent or thread_id == parent:
            return
        self._thread_parents[thread_id] = parent
        owner = self._thread_owner(thread_id)
        if owner and owner != thread_id:
            pending = self._thread_queues.pop(thread_id, None)
            if pending is not None:
                target = self._queue_for(owner)
                while not pending.empty():
                    target.put_nowait(pending.get_nowait())

    async def _dispatch(self, kind: str, msg: dict[str, Any]) -> None:
        params = msg.get("params") or {}
        thread = params.get("thread") or {}
        thread_id = str(params.get("threadId") or thread.get("id") or "")
        parent = str(thread.get("parentThreadId") or "")
        if msg.get("method") == "thread/started" and thread_id and parent:
            # The native 0.160.1 schema reserves parentThreadId for subagents;
            # thread/fork instead reports forkedFromId.
            self.bind_subagent(thread_id, parent)
        if thread_id:
            owner = self._thread_owner(thread_id)
            if owner is None:
                if kind == "request":
                    await self._peer.respond(msg["id"], error={"code": -32602,
                        "message": "codex.request_owner_closed: thread has been released"})
                return
            await self._queue_for(owner).put((kind, msg))
            return
        if kind == "request" and len(self._claimed_threads) > 1:
            await self._peer.respond(msg["id"], error={"code": -32602, "message": "codex.request_thread_missing: interactive request has no threadId"})
            return
        if self._thread_queues:
            for queue in self._thread_queues.values():
                await queue.put((kind, msg))
        else:
            await self.incoming.put((kind, msg))

    def _end_queues(self) -> None:
        self._closed = True
        self.incoming.put_nowait(("eof", None))
        for queue in self._thread_queues.values():
            queue.put_nowait(("eof", None))

    @classmethod
    async def spawn(
        cls,
        argv: list[str],
        *,
        env: Optional[dict[str, str]] = None,
        cwd: Optional[str] = None,
    ) -> "CodexPeer":
        holder: dict[str, CodexPeer] = {}

        async def on_message(msg: dict[str, Any]) -> None:
            conn = holder["self"]
            kind = "request" if "id" in msg else "notification"
            await conn._dispatch(kind, msg)

        peer = StdioJsonlPeer(
            argv, env=env, cwd=cwd, label="codex-app-server",
            on_message=on_message, stderr_tail_bytes=16384)
        conn = cls(peer)
        holder["self"] = conn
        await peer.start()
        # EOF 通知：stdout 关闭（含进程退出）时唤醒 turn 消费循环。
        eof_watch = asyncio.ensure_future(peer.wait_closed())
        eof_watch.add_done_callback(lambda _task: conn._end_queues())
        conn._eof_watch = eof_watch
        return conn

    def _exit_detail(self) -> str:
        returncode = self._peer.diagnostics()["returncode"]
        stderr = self._peer.stderr_text()
        details = [f"returncode={returncode}"]
        if stderr:
            details.append(f"stderr={stderr}")
        return "; ".join(details)

    async def request(
        self, method: str, params: Optional[dict[str, Any]] = None,
        *, timeout: float = 60.0,
    ) -> Any:
        try:
            response = await self._peer.request(method, params, timeout=timeout)
        except PeerClosedError as exc:
            raise CodexPeerClosedError(
                f"{exc}; {self._exit_detail()}", reason=exc.reason,
                exit_code=exc.exit_code) from exc
        if "error" in response:
            err = response.get("error") or {}
            raise JsonRpcError(
                int(err.get("code", -32000)),
                str(err.get("message", "unknown error")),
                err.get("data"))
        return response.get("result")

    async def notify(self, method: str, params: Optional[dict[str, Any]] = None) -> None:
        await self._peer.notify(method, params)

    async def respond(self, request_id: Any, result: Any) -> None:
        await self._peer.respond(request_id, result=result)

    @property
    def alive(self) -> bool:
        return self._peer.running

    async def close(self) -> int:
        return await self._peer.close()

    def stderr_text(self) -> str:
        """Complete app-server stderr for ``TurnRunner(diagnostics=...)``."""
        return self._peer.stderr_text()

    async def wait_exit(self) -> Optional[int]:
        """Process exit code for ``TurnRunner(exit_watch=...)``."""
        return await self._peer.wait_exit()


# ---------------------------------------------------------------------------
# capability discovery（schema 导出 → 方法面判定）
# ---------------------------------------------------------------------------


def _schema_methods(path: Path) -> set[str]:
    """从 generate-json-schema 导出的单个 schema 文件提取 method 名集合。"""
    try:
        doc = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return set()
    methods: set[str] = set()
    for key in ("anyOf", "oneOf"):
        for variant in doc.get(key, []) or []:
            if not isinstance(variant, dict):
                continue
            prop = (variant.get("properties") or {}).get("method") or {}
            if "const" in prop:
                methods.add(str(prop["const"]))
            for item in prop.get("enum", []) or []:
                methods.add(str(item))
    return methods


def export_protocol_methods(
    binary: str, out_dir: Path, *, timeout: float = 30.0
) -> dict[str, set[str]]:
    """运行 ``codex app-server generate-json-schema`` 并提取方法清单。

    返回 ``{"client_requests": {...}, "server_requests": {...},
    "server_notifications": {...}}``；导出失败返回空 dict（调用方降级）。
    """
    try:
        result = subprocess.run(
            [binary, "app-server", "generate-json-schema", "--out", str(out_dir)],
            capture_output=True, text=True, timeout=timeout, env=subprocess_environment(),
        )
    except (FileNotFoundError, subprocess.TimeoutExpired, OSError):
        return {}
    if result.returncode != 0:
        return {}
    methods: dict[str, set[str]] = {}
    for filename, key in _SCHEMA_FILES.items():
        methods[key] = _schema_methods(out_dir / filename)
    # Item discriminators are capabilities too; item/started alone does not
    # prove that the installed engine exposes delegation.
    try:
        schema = json.loads((out_dir / "ServerNotification.json").read_text())
        variants = schema.get("definitions", {}).get("ThreadItem", {}).get("oneOf", [])
        methods["item_types"] = {
            str(value) for variant in variants
            for value in variant.get("properties", {}).get("type", {}).get("enum", [])
        }
        methods["item_features"] = {
            "async_questions" for variant in variants
            if "agentMessage" in variant.get("properties", {}).get("type", {}).get("enum", [])
            and {"delivery", "questions"}.issubset(variant.get("properties", {}))
        }
    except (OSError, ValueError, TypeError):
        methods["item_types"] = set()
    # turn/start 参数字段也是能力面。0.160.1 把该文件放在 v2/ 下，且
    # properties 省略 collaborationMode（见 ``_PLAN_MODE_DEGRADATION``）。
    turn_doc = _turn_start_schema(out_dir)
    methods["turn_params"] = set((turn_doc.get("properties") or {}).keys())
    methods["schema_definitions"] = set((turn_doc.get("definitions") or {}).keys())
    return methods if any(methods.values()) else {}


def _turn_start_schema(out_dir: Path) -> dict[str, Any]:
    """Load TurnStartParams from the root or the v2 schema directory."""
    for relative in ("TurnStartParams.json", "v2/TurnStartParams.json"):
        path = out_dir / relative
        if not path.is_file():
            continue
        try:
            doc = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            continue
        if isinstance(doc, dict):
            return doc
    return {}


def _probe_version(binary: str, *, timeout: float = 15.0) -> str:
    try:
        result = subprocess.run(
            [binary, "--version"], capture_output=True, text=True,
            encoding="utf-8", errors="replace", timeout=timeout, env=subprocess_environment(),
        )
    except (FileNotFoundError, subprocess.TimeoutExpired, OSError):
        return ""
    lines = (result.stdout or result.stderr or "").strip().splitlines()
    return lines[0].strip() if result.returncode == 0 and lines else ""


# ---------------------------------------------------------------------------
# Adapter
# ---------------------------------------------------------------------------


class CodexAppServerAdapter(BaseExternalAgentAdapter, NativeRewindAdapter, RuntimeOperationAdapter):
    """Codex app-server（stdio JSON-RPC）的结构化 ExternalAgentAdapter。

    构造参数（除基类外）：

    - ``binary``：codex 可执行文件路径；
    - ``codex_home``：可选 CODEX_HOME 隔离目录（多账户 / 配置隔离；
      缺省沿用用户 ``~/.codex`` 以保留登录态）；
    - ``model`` / ``effort`` / ``approval_policy`` / ``sandbox``：
      thread/turn 默认值，可被 SessionStart 覆盖；
    - ``experimental_api``：``initialize`` 时声明 experimentalApi
      （开启 ``item/tool/requestUserInput`` 等实验面；默认关闭）；
    - ``approval_timeout_s``：审批等待超时，超时自动 ``decline``；
    - ``schema_probe``：probe 时是否导出 JSON schema 做能力发现。
    """

    context_compaction_events = True

    def __init__(
        self,
        *,
        binary: Optional[str] = None,
        instance_id: str = "default",
        store: Any = None,
        binding_service: Any = None,
        gateway_endpoint: str = "",
        descriptor_provider: Any = None,
        codex_home: Optional[str] = None,
        default_cwd: Optional[str] = None,
        default_env: Optional[dict[str, str]] = None,
        model: Optional[str] = None,
        effort: Optional[str] = None,
        approval_policy: Optional[str] = None,
        sandbox: Optional[str] = None,
        experimental_api: bool = False,
        turn_timeout_s: int = 900,
        approval_timeout_s: int = 300,
        schema_probe: bool = True,
    ) -> None:
        super().__init__(
            "codex.app_server",
            instance_id=instance_id,
            store=store,
            binding_service=binding_service,
            gateway_endpoint=gateway_endpoint,
            descriptor_provider=descriptor_provider,
        )
        from muteki.solver.cli_engines.bins import resolve_engine_bin

        self._binary = binary or resolve_engine_bin("codex")
        self._codex_home = codex_home
        self._default_cwd = default_cwd
        self._default_env = dict(default_env or {})
        self._model = model
        self._effort = effort
        self._approval_policy = approval_policy
        self._sandbox = sandbox
        self._experimental_api = bool(experimental_api)
        self._turn_timeout_s = int(turn_timeout_s)
        self._approval_timeout_s = int(approval_timeout_s)
        self._schema_probe = bool(schema_probe)
        # agent_session_id -> 运行上下文（conn/thread/queue 等）
        self._runs: dict[str, dict[str, Any]] = {}
        self._client_request_methods: set[str] = set()
        self._capability_revisions: dict[str, int] = {}
        self._fork_connections: dict[str, CodexPeer] = {}

    async def _ensure_protocol_methods(self) -> None:
        if self._client_request_methods or not self._schema_probe:
            return

        def load() -> set[str]:
            with tempfile.TemporaryDirectory(
                prefix="muteki-codex-schema-"
            ) as tmp:
                methods = export_protocol_methods(self._binary, Path(tmp))
            return set(methods.get("client_requests", set()))

        self._client_request_methods = await asyncio.to_thread(load)

    # -- capability probe ----------------------------------------------------

    async def probe(self, request: ProbeRequest) -> AgentCapabilities:
        """实测 probe：--version + schema 方法面 + initialize/model/list。"""
        field_sources: dict[str, str] = {}
        degradations: list[str] = []
        model_catalog: dict[str, Any] | None = None
        version = _probe_version(self._binary)
        probed = bool(version)

        caps = conservative_capabilities(
            transport_kind="rpc",
            capability_source=SOURCE_PROBE if probed else SOURCE_STATIC,
        )
        caps.runtime_version = version

        if not probed:
            for name in BOOL_CAPABILITY_FIELDS:
                field_sources[name] = SOURCE_STATIC
            degradations.append(
                "codex binary 不可用或 --version 失败：全部能力保守默认 False；"
                "CLI 兼容降级路径为 cli_adapter_for('codex')（codex exec --json）")
            self._probe_cache = CapabilityProbeReport(
                adapter_id=self.id, instance_id=self.identity.instance_id,
                capabilities=caps, binary_path=self._binary,
                field_sources=field_sources, degradations=degradations,
                detail="binary 不可用，使用保守默认",
                model_catalog={"ok": False, "models": [], "source": "codex.model/list",
                               "error_code": "codex.model_catalog.binary_unavailable",
                               "detail": "Codex binary unavailable"} if request.include_models else None)
            return caps

        # schema 方法面发现（版本变化探测的主信号）。
        methods: dict[str, set[str]] = {}
        if self._schema_probe:
            with tempfile.TemporaryDirectory(prefix="muteki-codex-schema-") as tmp:
                methods = export_protocol_methods(self._binary, Path(tmp))
        schema_ok = bool(methods)
        client_req = methods.get("client_requests", set())
        self._client_request_methods = set(client_req)
        server_req = methods.get("server_requests", set())
        server_ntf = methods.get("server_notifications", set())
        if not schema_ok:
            degradations.append(
                "generate-json-schema 导出失败：能力面无法实测，"
                "结构化传输相关字段回落保守默认")

        def mark(field_name: str, value: bool, probed_field: bool) -> bool:
            field_sources[field_name] = (
                SOURCE_PROBE if probed_field else SOURCE_STATIC)
            return bool(value and probed_field)

        caps.streaming = mark("streaming", "item/agentMessage/delta" in server_ntf, schema_ok)
        caps.tool_events = mark("tool_events", "item/started" in server_ntf, schema_ok)
        caps.subagents = mark("subagents", bool(
            methods.get("item_types", set()) & {"collabAgentToolCall", "subAgentActivity"}
        ), schema_ok)
        caps.resume = mark("resume", M_THREAD_RESUME in client_req, schema_ok)
        caps.session_persistence = mark(
            "session_persistence", M_THREAD_RESUME in client_req, schema_ok)
        caps.steer = mark("steer", M_TURN_STEER in client_req, schema_ok)
        caps.interrupt = mark("interrupt", M_TURN_INTERRUPT in client_req, schema_ok)
        caps.approval = mark(
            "approval",
            "item/commandExecution/requestApproval" in server_req
            or "execCommandApproval" in server_req,
            schema_ok)
        caps.access_modes = list(ACCESS_MODE_VALUES) if caps.approval else []
        field_sources["access_modes"] = (
            SOURCE_REPORTED if caps.approval else SOURCE_STATIC)
        caps.user_input = mark(
            "user_input", "item/tool/requestUserInput" in server_req
            or "async_questions" in methods.get("item_features", set()), schema_ok)
        caps.fork = mark("fork", M_THREAD_FORK in client_req, schema_ok)
        caps.mcp = mark("mcp", M_MCP_RELOAD in client_req, schema_ok)
        caps.usage_events = mark(
            "usage_events", "thread/tokenUsage/updated" in server_ntf, schema_ok)
        caps.plan = mark(
            "plan",
            "turn/plan/updated" in server_ntf or "item/plan/delta" in server_ntf,
            schema_ok,
        )
        # turn/start UserInput includes text | image | localImage (app-server).
        caps.image_input = mark(
            "image_input", M_TURN_START in client_req, schema_ok)
        # The /compact runtime operation calls thread/compact/start.
        caps.compaction = mark(
            "compaction", "thread/compact/start" in client_req, schema_ok)
        # Plan mode follows the 0.160.1 runtime gate, not the omitted schema
        # property. See ``_PLAN_MODE_DEGRADATION``. CollaborationMode in the
        # schema definitions is the installed binary's signal that the field
        # still exists; experimentalApi is what the server actually checks.
        collab_in_schema = "collaborationMode" in methods.get("turn_params", set())
        collab_known = collab_in_schema or (
            "CollaborationMode" in methods.get("schema_definitions", set()))
        caps.plan_mode = mark(
            "plan_mode", collab_known and self._experimental_api, schema_ok)
        if collab_known and not self._experimental_api:
            field_sources["plan_mode"] = SOURCE_REPORTED
            degradations.append(
                _PLAN_MODE_DEGRADATION
                + " 当前实例未开 experimentalApi，plan_mode 置 False。")
        elif caps.plan_mode and not collab_in_schema:
            degradations.append(
                _PLAN_MODE_DEGRADATION
                + " schema 未列出字段，按 experimentalApi 实测启用 plan_mode。")
        elif schema_ok and not collab_known:
            degradations.append(
                "generate-json-schema 未提供 collaborationMode 或 CollaborationMode，"
                "plan_mode 保持 False，不把规划模式当作可用。")

        if schema_ok and not self._experimental_api and caps.user_input:
            # requestUserInput 属 experimental 面：未开 experimentalApi 时
            # 协议存在但运行期会被门控，如实标注。
            field_sources["user_input"] = SOURCE_REPORTED
            degradations.append(
                "item/tool/requestUserInput 为 experimental 方法："
                "当前实例未开 experimentalApi，user_input 按 adapter_reported 标注")

        # initialize 握手实测（证明 stdio 传输真实可用）。
        handshake_ok = False
        conn: Optional[CodexPeer] = None
        try:
            spawn_env = self._spawn_env({})
            conn = await self._spawn_initialized_peer(
                argv=self._with_launch_args([
                    self._binary, "app-server", "--listen", "stdio://",
                    *self._provider_spawn_args(spawn_env)]),
                env=spawn_env, cwd=None,
                init_params={"clientInfo": {
                    "name": "muteki-probe", "title": "Muteki Probe",
                    "version": "0.1.0"}},
            )
            handshake_ok = True
            if request.include_models and M_MODEL_LIST in client_req:
                try:
                    model_catalog = await read_model_catalog(conn)
                    caps.supported_models = [row["id"] for row in model_catalog["models"]]
                    caps.supported_efforts = list(dict.fromkeys(
                        effort for row in model_catalog["models"]
                        for effort in row["reasoning"]["levels"]))
                    field_sources["supported_models"] = SOURCE_PROBE
                    field_sources["supported_efforts"] = SOURCE_PROBE
                except (JsonRpcError, asyncio.TimeoutError, ModelCatalogProtocolError) as exc:
                    detail = f"model/list 失败：{exc}"
                    model_catalog = {"ok": False, "models": [], "source": "codex.model/list",
                                     "error_code": getattr(exc, "code", "codex.model_catalog.request_failed")
                                     if isinstance(exc, ModelCatalogProtocolError) else "codex.model_catalog.request_failed",
                                     "detail": detail, "evidence": {
                                         "method": M_MODEL_LIST, "format": "decoded_rpc_result_pages",
                                         "complete": False, "pages": getattr(exc, "catalog_pages", []),
                                         "error": {"type": type(exc).__name__, "message": str(exc),
                                                   **({"code": exc.code, "data": exc.data}
                                                      if isinstance(exc, JsonRpcError) else {})},
                                     }}
                    degradations.append(detail)
            elif request.include_models:
                model_catalog = {"ok": False, "models": [], "source": "codex.model/list",
                                 "error_code": "codex.model_catalog.unsupported",
                                 "detail": "Runtime schema does not expose model/list"}
        except (OSError, JsonRpcError, asyncio.TimeoutError) as exc:
            detail = f"app-server initialize 握手失败：{exc}"
            degradations.append(detail)
            if request.include_models:
                model_catalog = {"ok": False, "models": [], "source": "codex.model/list",
                                 "error_code": "codex.model_catalog.initialize_failed", "detail": detail}
        finally:
            if conn is not None:
                await conn.close()
        if not handshake_ok:
            caps.streaming = caps.resume = caps.steer = caps.interrupt = False
            caps.approval = caps.tool_events = False
            caps.usage_events = caps.session_persistence = False
            for name in BOOL_CAPABILITY_FIELDS:
                # MCP 的 spawn 配置能力已由导出的正式协议方法面确认，
                # 一次 initialize EOF 不应把下一次真实会话降级成未交付的
                # Agent Plugin 文本计划。
                if name != "mcp":
                    field_sources[name] = SOURCE_STATIC
            degradations.append(
                "initialize 握手未成功：Turn 运行能力降级；"
                "协议 schema 已确认的 spawn 级 MCP 配置能力继续保留")

        caps.permission_modes = ["on-request", "never"]
        field_sources["permission_modes"] = SOURCE_REPORTED
        caps.sandbox_modes = ["read-only", "workspace-write", "danger-full-access"]
        field_sources["sandbox_modes"] = SOURCE_REPORTED
        caps.protocol_version = version
        self._probe_cache = CapabilityProbeReport(
            adapter_id=self.id, instance_id=self.identity.instance_id,
            capabilities=caps, binary_path=self._binary,
            field_sources=field_sources, degradations=degradations,
            detail="" if handshake_ok else "handshake 未通过", model_catalog=model_catalog)
        return caps

    # -- 启动（任务书 7.6 步骤 4 的 Runtime 侧动作） ---------------------------

    def _spawn_env(self, extra: dict[str, str]) -> dict[str, str]:
        env = dict(os.environ)
        env.update(self._default_env)
        env.update(extra)
        if self._codex_home:
            env["CODEX_HOME"] = self._codex_home
        return subprocess_environment(env) or env

    @staticmethod
    def _provider_spawn_args(env: dict[str, str]) -> list[str]:
        """Mirror CLI provider binding for app-server custom endpoints.

        Credential accounts with ``API_KEY`` + ``BASE_URL`` inject
        ``OPENAI_BASE_URL`` and need a synthetic ``muteki`` provider block.
        Login-style accounts only set ``CODEX_HOME`` (host ``config.toml`` with
        ``model_provider`` + ``[model_providers.*]``); always re-emit those as
        ``-c`` so spawn does not depend solely on Codex reading the file.
        """
        from muteki.solver.cli_engines.codex_provider import (
            codex_provider_spawn_args,
        )
        return codex_provider_spawn_args(env)

    def _mcp_thread_config(
        self, plan: Optional[CapabilityInjectionPlan],
        bearer_token: Optional[str],
    ) -> dict[str, Any]:
        """Materialize target-thread MCP overrides, as T3's thread runtime params.

        Credentials belong to the target thread's grant, not to the shared
        app-server environment. Never mutate the persisted injection plan.
        """
        if plan is None or plan.injection_kind is not InjectionKind.MCP:
            return {}
        endpoint = plan.gateway_endpoint
        if not endpoint:
            return {}
        if not bearer_token:
            error = ValueError("Codex thread MCP configuration requires its capability grant")
            error.code = "codex.mcp.token_missing"
            raise error
        return {MCP_SERVER_NAME: {
            "url": endpoint,
            "http_headers": {"Authorization": f"Bearer {bearer_token}"},
        }}

    @staticmethod
    def _retryable_initialize_error(exc: BaseException) -> bool:
        """握手完成前子进程自行退出（本地状态库初始化竞争的表现）。

        只按类型化的退出原因判定，不匹配 stderr 文本；重试有界，
        耗尽后原始错误（含完整退出详情）原样抛出。
        """
        return (isinstance(exc, CodexPeerClosedError)
                and exc.reason in ("stdout_eof", "not_running"))

    async def _spawn_initialized_peer(
        self,
        *,
        argv: list[str],
        env: dict[str, str],
        cwd: Optional[str],
        init_params: dict[str, Any],
    ) -> CodexPeer:
        """启动并握手；仅重试 Codex 自身明确报告的状态库初始化竞争。"""
        attempts = 3
        for attempt in range(attempts):
            conn: Optional[CodexPeer] = None
            try:
                conn = await CodexPeer.spawn(argv, env=env, cwd=cwd)
                await conn.request(M_INITIALIZE, init_params, timeout=30)
                await conn.notify(M_INITIALIZED)
                return conn
            except BaseException as exc:
                if conn is not None:
                    await conn.close()
                if (not isinstance(exc, (OSError, JsonRpcError, asyncio.TimeoutError))
                        or attempt + 1 >= attempts
                        or not self._retryable_initialize_error(exc)):
                    raise
                await asyncio.sleep(0.25 * (2 ** attempt))
        raise RuntimeError("codex app-server initialize retry exhausted")

    async def _launch(
        self,
        request: SessionStart,
        plan: Optional[CapabilityInjectionPlan],
        bearer_token: Optional[str],
    ) -> dict[str, Any]:
        sid = request.agent_session_id
        await self._ensure_protocol_methods()
        options = request.options
        cwd = options.cwd or self._default_cwd or os.getcwd()
        spawn_env = self._spawn_env(dict(options.env))
        provider_args = self._provider_spawn_args(spawn_env)
        thread_mcp = self._mcp_thread_config(plan, bearer_token)

        init_params: dict[str, Any] = {"clientInfo": {
            "name": "muteki", "title": "Muteki ExternalAgentAdapter",
            "version": "runtime-02"}}
        if self._experimental_api:
            init_params["capabilities"] = {"experimentalApi": True}
        spawn_argv = self._with_launch_args([
            self._binary, "app-server", *provider_args, "--listen", "stdio://"])
        fork_connection = self._fork_connections.get(request.resume_handle or "")
        launch_signature = (spawn_argv, spawn_env, options.chat_native_plugins)
        shared = (fork_connection is not None and fork_connection.alive
                  and fork_connection.launch_signature == launch_signature)
        if shared:
            conn = fork_connection
        else:
            if fork_connection is not None and fork_connection.alive:
                # T3 unloads the native thread before another provider
                # connection adopts it. Codex permits only one active writer.
                await fork_connection.request(
                    "thread/unsubscribe", {"threadId": request.resume_handle},
                    timeout=30)
                fork_connection.release_thread(str(request.resume_handle))
            conn = await self._spawn_initialized_peer(
                # Provider credentials belong to this app-server. A changed
                # binding resumes the exact native fork on the requested
                # connection; failure never creates a new conversation.
                argv=spawn_argv, env=spawn_env, cwd=cwd, init_params=init_params)
            conn.launch_signature = launch_signature
        try:
            reload_status = "thread-config" if thread_mcp else "no-injection"
            for plugin in ([] if shared else options.chat_native_plugins):
                await conn.request("plugin/install", plugin, timeout=60)

            thread_params: dict[str, Any] = {
                "cwd": cwd,
                "serviceName": "muteki",
            }
            thread_config = dict(_THREAD_UPDATE_PLAN_CONFIG)
            if options.is_conversation:
                # Desktop ownership belongs to Muteki's gateway. Imported Codex
                # plugins must not expose a second CUA session to the model.
                home = Path(spawn_env.get("CODEX_HOME") or Path.home() / ".codex")
                config_file = home / "config.toml"
                if config_file.is_file():
                    native_cua = tomllib.loads(config_file.read_text(encoding="utf-8")).get("mcp_servers", {}).get("cua_repl")
                    if isinstance(native_cua, dict):
                        # Codex validates transport even for disabled servers.
                        thread_mcp["cua_repl"] = {**native_cua, "enabled": False}
                thread_config['plugins."unified-computer-use@openai-bundled".enabled'] = False
            if thread_mcp:
                thread_config["mcp_servers"] = thread_mcp
            approvals = options.chat_hook_approvals
            if approvals:
                catalog = await conn.request("hooks/list", {"cwds": [cwd]})
                trusted = {}
                for entry in catalog.get("data", []):
                    for hook in entry.get("hooks", []):
                        if (hook.get("command") in approvals.get(hook.get("pluginId"), [])
                            and hook.get("currentHash") and hook.get("key")):
                            trusted[hook["key"]] = {"trusted_hash": hook["currentHash"], "enabled": True}
                if trusted:
                    thread_config["hooks.state"] = trusted
            thread_params["config"] = thread_config
            model = request.model or self._model
            if model:
                thread_params["model"] = model
            if request.service_tier:
                thread_params["serviceTier"] = request.service_tier
            access_config: Optional[dict[str, Any]] = None
            if request.access_mode:
                access_config = _access_config(str(request.access_mode))
                thread_params.update({
                    "approvalPolicy": access_config["approvalPolicy"],
                    "sandbox": access_config["sandbox"],
                    "approvalsReviewer": access_config["approvalsReviewer"],
                })
            else:
                approval_policy = (
                    request.permission_mode or self._approval_policy)
                if approval_policy:
                    thread_params["approvalPolicy"] = approval_policy
                sandbox = str(request.sandbox_mode or self._sandbox or "")
                if sandbox:
                    thread_params["sandbox"] = sandbox

            # ThreadResumeParams and ThreadForkParams (0.160.1 schema) have no
            # serviceName; they carry the same access, model and cwd fields.
            existing_thread_params = {
                key: value for key, value in thread_params.items()
                if key != "serviceName"}
            fork_from = options.fork_from
            if fork_from:
                result = await conn.request(
                    M_THREAD_FORK,
                    {"threadId": fork_from, "excludeTurns": True,
                     **({"lastTurnId": options.fork_last_turn_id} if options.fork_last_turn_id else {}),
                     **existing_thread_params},
                    timeout=60)
            elif request.resume_handle:
                result = await conn.request(
                    M_THREAD_RESUME,
                    # The Conversation owns history; replaying every turn on
                    # resume would only cost latency and memory.
                    {"threadId": request.resume_handle, "excludeTurns": True,
                     **existing_thread_params},
                    timeout=60)
            else:
                result = await conn.request(M_THREAD_START, thread_params, timeout=60)
        except BaseException:
            if not shared:
                await conn.close()
            raise

        thread = (result or {}).get("thread") or {}
        thread_id = str(thread.get("id") or "")
        if not thread_id:
            if not shared:
                await conn.close()
            raise RuntimeError("thread/start 响应缺少 thread.id")
        self._runs[sid] = {
            "conn": conn,
            "incoming": conn.queue_for(thread_id),
            "conversation_thread_id": request.thread_id,
            "thread_id": thread_id,
            # fork 后 sessionId 保持根线程 id，单独记录（核验 §CODEX 末节）。
            "root_session_id": str(thread.get("sessionId") or thread_id),
            "cwd": cwd,
            "model": str((result or {}).get("model") or model or ""),
            "effort": request.effort or self._effort,
            "collaboration_mode": str(
                ((result or {}).get("collaborationMode") or {}).get("mode")
                or "default"),
            "service_tier": request.service_tier or "",
            "access_config": access_config,
            "turns": 0,
            "current_turn_id": None,
            "approvals": {},      # request_id -> asyncio.Future
            "file_change_items": {},  # item_id -> {files,diff,path,paths}
            "user_inputs": {},    # request_id -> asyncio.Future
            "user_input_params": {},  # request_id -> 原生 request 上下文
            "mcp_reload": reload_status,
            "mcp_injected": bool(thread_mcp),
            "thread_started_native": None,
            "resume_prompt": options.resume_prompt or "Continue from where you left off.",
        }
        if request.resume_handle:
            self._fork_connections.pop(request.resume_handle, None)
        return {"external_session_id": thread_id, "resume_handle": thread_id}

    # -- 审批 / 用户输入应答 -----------------------------------------------------

    def _pending_requests(
        self, session: AgentSessionRef, kind: str
    ) -> dict[Any, asyncio.Future]:
        ctx = self._runs.get(session.agent_session_id) or {}
        return ctx.get(kind) or {}

    async def respond_approval(
        self,
        session: AgentSessionRef,
        request_id: Any,
        decision: str,
    ) -> CommandReceipt:
        """应答一次审批请求（decision ∈ APPROVAL_DECISIONS）。"""
        if decision not in APPROVAL_DECISIONS:
            return self.unsupported_receipt(
                "respond_approval", "approval_decision", session=session,
                detail={"decision": decision,
                        "allowed": list(APPROVAL_DECISIONS)})
        future = self._pending_requests(session, "approvals").get(str(request_id))
        if future is None or future.done():
            return self.unsupported_receipt(
                "respond_approval", "approval_pending", session=session,
                detail={"request_id": str(request_id),
                        "detail": "no pending approval with this id"})
        ctx = self._runs.get(session.agent_session_id) or {}
        delivery = ctx.get("control_deliveries", {}).get(("approval", str(request_id)))
        if delivery is None:
            return self.unsupported_receipt("respond_approval", "control_delivery", session=session)
        future.set_result({"decision": decision})
        return await self._confirm_control_delivery(session, delivery)

    async def _confirm_control_delivery(self, session: AgentSessionRef, delivery: asyncio.Future) -> CommandReceipt:
        from muteki.platform.contracts.errors import ErrorCategory, ErrorEnvelope
        try:
            outcome = await asyncio.wait_for(asyncio.shield(delivery), timeout=30)
        except asyncio.TimeoutError:
            outcome = {"ok": False, "detail": "Native response write confirmation timed out"}
        error = None if outcome["ok"] else ErrorEnvelope(
            code="codex.control.delivery_unknown", message=str(outcome.get("detail") or "Native response delivery was not confirmed"),
            category=ErrorCategory.RUNTIME, detail={"delivery_unknown": True}, retryable=False,
            recovery_hint="Inspect or stop the original turn; do not send a new decision")
        return CommandReceipt(command_id=new_id("cmd"),
            state=ReceiptState.FAILED if error else ReceiptState.COMPLETED,
            error=error, aggregate=AggregateRef(type="agent_session", id=session.agent_session_id))

    async def _send_control_response(self, ctx: dict[str, Any], kind: str,
                                     request_id: Any, response: dict[str, Any]) -> None:
        key = (kind, str(request_id))
        delivery = ctx["control_deliveries"][key]
        try:
            await ctx["conn"].respond(request_id, response)
        except BaseException as exc:
            if not delivery.done():
                delivery.set_result({"ok": False, "detail": f"{type(exc).__name__}: {exc}"})
            raise
        else:
            if not delivery.done():
                delivery.set_result({"ok": True})
        finally:
            ctx["control_deliveries"].pop(key, None)

    async def respond_user_input(
        self,
        session: AgentSessionRef,
        request_id: Any,
        answers: dict[str, Any],
    ) -> CommandReceipt:
        """Serialize delivery and deduplicate only acknowledged answers."""
        ctx = self._runs.get(session.agent_session_id) or {}
        lock = ctx.setdefault("user_input_response_lock", asyncio.Lock())
        async with lock:
            request_key = str(request_id)
            fingerprint = json.dumps(answers, sort_keys=True, ensure_ascii=False)
            completed = ctx.setdefault("user_input_completed", {})
            previous = completed.get(request_key)
            if previous is not None:
                if previous[0] != fingerprint:
                    raise RuntimeError("该问题已经回答，不能重复提交不同答案")
                return previous[1].model_copy(update={"deduplicated": True})
            receipt = await self._respond_user_input_once(session, request_id, answers)
            if receipt.state is ReceiptState.COMPLETED:
                completed[request_key] = (fingerprint, receipt)
            return receipt

    async def _respond_user_input_once(
        self,
        session: AgentSessionRef,
        request_id: Any,
        answers: dict[str, Any],
    ) -> CommandReceipt:
        request_key = str(request_id)
        future = self._pending_requests(session, "user_inputs").get(request_key)
        if future is None or future.done():
            return self.unsupported_receipt(
                "respond_user_input", "user_input_pending", session=session,
                detail={"request_id": str(request_id)})
        ctx = self._runs.get(session.agent_session_id) or {}
        native = dict((ctx.get("user_input_params") or {}).get(request_key) or {})
        method = str(native.get("method") or "")
        params = dict(native.get("params") or {})
        decision = str(answers.get("__decision__") or "submit").strip().lower()
        text = str(answers.get("__text__") or "")
        structured = {
            key: value
            for key, value in answers.items()
            if not str(key).startswith("__")
        }
        delivery = ctx.get("control_deliveries", {}).get(("user_input", request_key))
        if method != "agentMessage/async" and delivery is None:
            return self.unsupported_receipt("respond_user_input", "control_delivery", session=session)
        if method == "agentMessage/async":
            # Async questions are public agentMessage items, not RPC requests.
            # Reply through the documented steer input while the native turn
            # runs, or continue the same thread after it has already ended.
            pending = native.get("pending") or {}
            lines = []
            for question in pending.get("questions") or []:
                value = structured.get(question["question_id"], {})
                if isinstance(value, dict):
                    answer = str(value.get("text") or " / ".join(
                        str(v) for v in value.get("values", [])))
                else:
                    answer = str(value)
                lines.append(f"{question.get('prompt') or ''}: {answer}")
            reply = ("用户已取消此问题，请停止等待并说明已取消。" if decision == "cancel"
                     else "用户对异步问题的回答：\n" + ("\n".join(lines) or text))
            current_turn = ctx.get("current_turn_id") or (
                ((ctx.get("async_started_turn") or {}).get("result") or {}).get("turn") or {}
            ).get("id")
            needs_continuation = not current_turn
            if current_turn:
                try:
                    await ctx["conn"].request(M_TURN_STEER, {
                        "threadId": ctx["thread_id"], "expectedTurnId": current_turn,
                        "input": codex_turn_input(reply),
                    }, timeout=30)
                except JsonRpcError:
                    # A completion can race the steer. Only a thread with no
                    # in-progress turn permits starting a continuation.
                    if await self._thread_has_active_turn(ctx):
                        raise
                    needs_continuation = True
            if future.done():
                raise RuntimeError("问题已取消或过期，回答未再提交")
            if needs_continuation:
                continuation = MessageInput(text=reply,
                    payload={"client_user_message_id": f"muteki-input-{request_key}"})
                # Do not consume the question until the native runtime has
                # acknowledged turn/start. Failure leaves its Future pending,
                # so the durable question can be answered again.
                result = await ctx["conn"].request(
                    M_TURN_START, self._turn_parameters(ctx, continuation), timeout=60)
                started_id = str(((result or {}).get("turn") or {}).get("id") or "")
                if not started_id:
                    raise RuntimeError("Codex 续轮未返回 turn.id，问题仍待回答")
                if future.done():
                    # Stop/timeout won the race while the start RPC was in
                    # flight. Do not leave the acknowledged continuation orphaned.
                    try:
                        await ctx["conn"].request(M_TURN_INTERRUPT, {
                            "threadId": ctx["thread_id"], "turnId": started_id,
                        }, timeout=30)
                    except (JsonRpcError, asyncio.TimeoutError, ConnectionError):
                        pass
                    raise RuntimeError("问题已取消或过期，续轮已请求停止")
                ctx["async_started_turn"] = {"result": result, "input": continuation}
            future.set_result({"answered": decision != "cancel"})
        elif method == M_MCP_ELICITATION:
            if decision in {"cancel", "decline"}:
                future.set_result({"action": decision, "_meta": None})
            else:
                future.set_result({
                    "action": "accept",
                    "content": self._mcp_elicitation_content(
                        params, text, structured),
                    "_meta": None,
                })
        elif decision == "cancel":
            future.set_result({"answers": {}})
        elif structured:
            native_answers: dict[str, Any] = {}
            needs_normalize = False
            for qid, value in structured.items():
                key = str(qid)
                if isinstance(value, dict) and "answers" in value:
                    answers_list = value.get("answers")
                    native_answers[key] = {
                        "answers": (
                            list(answers_list)
                            if isinstance(answers_list, list)
                            else [answers_list] if answers_list is not None else []
                        )
                    }
                else:
                    needs_normalize = True
                    break
            if needs_normalize:
                future.set_result(answers_for_codex_tool({
                    str(qid): (
                        value if isinstance(value, dict)
                        else {"values": [str(value)], "text": str(value)}
                    )
                    for qid, value in structured.items()
                }))
            else:
                future.set_result({"answers": native_answers})
        elif text:
            questions = params.get("questions")
            question_id = "answer"
            if isinstance(questions, list) and questions:
                first = questions[0]
                if isinstance(first, dict):
                    question_id = str(first.get("id") or question_id)
            future.set_result({
                "answers": {question_id: {"answers": [text]}}
            })
        else:
            future.set_result({"answers": dict(answers)})
        if delivery is not None:
            return await self._confirm_control_delivery(session, delivery)
        return CommandReceipt(
            command_id=new_id("cmd"), state=ReceiptState.COMPLETED,
            aggregate=AggregateRef(type="agent_session",
                                   id=session.agent_session_id))

    @staticmethod
    def _mcp_elicitation_content(
        params: dict[str, Any],
        text: str,
        structured: Optional[dict[str, Any]] = None,
    ) -> dict[str, Any]:
        """Map structured answers (or legacy text) onto MCP elicitation fields.

        Legacy text goes to the first schema property and is converted with
        the schema's own wire types; invalid values raise
        ``UserInputValidationError``.
        """
        schema = params.get("requestedSchema")
        schema_dict = schema if isinstance(schema, dict) else {}
        normalized = {
            str(qid): (
                value if isinstance(value, dict)
                else {"values": [str(value)], "text": str(value)}
            )
            for qid, value in (structured or {}).items()
        }
        if not normalized and text:
            properties = schema_dict.get("properties")
            if not isinstance(properties, dict) or not properties:
                return {}
            normalized = {str(next(iter(properties))): {"values": [], "text": text}}
        return content_for_elicitation(normalized, schema=schema_dict)

    # -- turn 流 ----------------------------------------------------------------

    def send(
        self, session: AgentSessionRef, input: AgentInput
    ) -> AsyncIterator[AgentEvent]:
        if isinstance(input, ApprovalResponseInput):
            return self._approval_response_stream(session, input)
        if isinstance(input, UserInputResponseInput):
            return self._user_input_response_stream(session, input)
        if isinstance(input, MessageInput):
            return self._turn_stream(session, input)
        return self.unsupported_input_stream(session, input)

    async def _approval_response_stream(
        self, session: AgentSessionRef, input: ApprovalResponseInput
    ) -> AsyncIterator[AgentEvent]:
        approval = ApprovalDecision.from_payload(input.payload.model_dump())
        receipt = await self.respond_approval(
            session, approval.approval_id, approval.codex_decision())
        if receipt.state is ReceiptState.FAILED:
            yield self.emit(build_event(
                AgentEventType.RUNTIME_ERROR,
                self.sequencer_for(session.agent_session_id),
                agent_session_id=session.agent_session_id,
                external_session_id=session.external_session_id,
                payload=RuntimeErrorPayload(error=self.failure(
                    FailureCategory.UNKNOWN, "control.delivery_failed",
                    message=(receipt.error.message if receipt.error
                             else "approval response was not delivered"),
                    native_code=(receipt.error.code if receipt.error else ""),
                    delivery_unknown=bool(
                        receipt.error
                        and receipt.error.detail.get("delivery_unknown")))),
            ))

    async def _user_input_response_stream(
        self, session: AgentSessionRef, input: UserInputResponseInput
    ) -> AsyncIterator[AgentEvent]:
        answers = dict(input.payload.answers)
        answers["__decision__"] = input.payload.decision
        if input.text and "__text__" not in answers:
            answers["__text__"] = input.text
        receipt = await self.respond_user_input(
            session, input.payload.request_id, answers)
        if receipt.state is ReceiptState.FAILED:
            yield self.emit(build_event(
                AgentEventType.RUNTIME_ERROR,
                self.sequencer_for(session.agent_session_id),
                agent_session_id=session.agent_session_id,
                external_session_id=session.external_session_id,
                payload=RuntimeErrorPayload(error=self.failure(
                    FailureCategory.UNKNOWN, "control.delivery_failed",
                    message=(receipt.error.message if receipt.error
                             else "user input response was not delivered"),
                    native_code=(receipt.error.code if receipt.error else ""),
                    delivery_unknown=bool(
                        receipt.error
                        and receipt.error.detail.get("delivery_unknown")))),
            ))
            raise RuntimeError(receipt.error.message if receipt.error else "用户输入未送达 Runtime")
        else:
            ctx = self._runs.get(session.agent_session_id) or {}
            for key, native in (ctx.get("user_input_params") or {}).items():
                future = (ctx.get("user_inputs") or {}).get(key)
                if native.get("method") == "agentMessage/async" and future is not None and not future.done():
                    yield self.emit(build_event(
                        AgentEventType.USER_INPUT_REQUESTED,
                        self.sequencer_for(session.agent_session_id),
                        agent_session_id=session.agent_session_id,
                        external_session_id=session.external_session_id,
                        turn_id=ctx.get("current_turn_id"),
                        native_type="agentMessage/async",
                        payload=native["pending"],
                    ))
                    break

    def resume(self, session: AgentSessionRef) -> AsyncIterator[AgentEvent]:
        """活动连接上的续跑：SESSION_RESUMED + continue turn。

        进程已关闭的 Session 恢复走 ``start(SessionStart(resume_handle=...))``
        （thread/resume 路径在 ``_launch`` 内）。
        """
        ctx = self._runs.get(session.agent_session_id)
        if ctx is None or not ctx["conn"].alive:
            return self._unsupported_stream(session, "resume", "resume")
        prompt = str(
            ctx.get("resume_prompt") or "Continue from where you left off."
        )
        return self._turn_stream(
            session, MessageInput(text=prompt), resumed=True)

    @staticmethod
    def _collaboration_mode(
        ctx: dict[str, Any], input: MessageInput,
    ) -> Optional[dict[str, Any]]:
        """Per-turn ``collaborationMode`` (T3 ``buildCodexTurnStartParams``).

        ``plan`` is sent on plan turns; the first turn after a plan turn sends
        an explicit ``default`` so the thread leaves plan mode. Threads that
        never planned omit the field.
        """
        wanted = input.payload.interaction_mode
        # Ordinary turns omit the field. A plan turn sends mode=plan. The
        # first turn after a plan turn sends an explicit default so the
        # thread leaves plan mode.
        if wanted != "plan" and not (
                wanted == "default" and ctx.get("collaboration_mode") == "plan"):
            return None
        settings: dict[str, Any] = {"model": ctx["model"]}
        if ctx.get("effort"):
            settings["reasoning_effort"] = ctx["effort"]
        return {"mode": wanted, "settings": settings}

    @staticmethod
    def _turn_parameters(ctx: dict[str, Any], input: MessageInput) -> dict[str, Any]:
        params: dict[str, Any] = {
            "threadId": ctx["thread_id"],
            "input": codex_turn_input(
                input.text, input.payload.attachments, input.payload.runtime_capability),
        }
        # Detailed reasoning summaries are what the Conversation reasoning
        # block renders; the app-server default ("auto") often emits none.
        params["summary"] = "detailed"
        params["cwd"] = ctx["cwd"]
        if ctx.get("model"):
            params["model"] = ctx["model"]
        if ctx.get("effort"):
            params["effort"] = ctx["effort"]
        collaboration = CodexAppServerAdapter._collaboration_mode(ctx, input)
        if collaboration is not None:
            params["collaborationMode"] = collaboration
        if ctx.get("service_tier"):
            # A forked thread is not started with thread params, so the turn
            # carries the tier as well.
            params["serviceTier"] = ctx["service_tier"]
        if input.payload.client_user_message_id:
            params["clientUserMessageId"] = input.payload.client_user_message_id
        access = ctx.get("access_config")
        if access:
            params.update({"approvalPolicy": access["approvalPolicy"],
                           "approvalsReviewer": access["approvalsReviewer"],
                           "sandboxPolicy": access["sandboxPolicy"]})
        return params

    def _plan_mode_block(self, ctx: dict[str, Any]) -> str:
        """Why a plan turn cannot be sent, or ``""`` when it can.

        0.160.1 accepts ``collaborationMode`` only after experimentalApi is
        declared and the thread has a model. A probe that did not find
        CollaborationMode keeps plan_mode false and blocks the turn here.
        """
        if not self._experimental_api:
            return "experimentalApi"
        if not ctx.get("model"):
            return "a resolved model"
        report = self._probe_cache
        if report is not None and not report.capabilities.plan_mode:
            return "collaborationMode support on this Codex build"
        return ""

    async def _turn_stream(
        self,
        session: AgentSessionRef,
        input: MessageInput,
        *,
        resumed: bool = False,
        started_result: Optional[dict[str, Any]] = None,
        supervise: bool = True,
    ) -> AsyncIterator[AgentEvent]:
        sid = session.agent_session_id
        seq = self.sequencer_for(sid)
        ctx = self._runs.get(sid)
        if ctx is None:
            yield self.emit(build_event(
                AgentEventType.RUNTIME_ERROR, seq,
                agent_session_id=sid,
                external_session_id=session.external_session_id,
                payload=RuntimeErrorPayload(error=self.failure(
                    FailureCategory.UNKNOWN, "session.unknown",
                    message="session was not started by this adapter")),
            ))
            return
        if input.text.strip():
            # 中断后继续时重放原始完整指令；只保存于 Runtime 会话内，
            # 重启接管则由 SessionStart.options 中的持久对话历史补齐。
            ctx["resume_prompt"] = input.text
        record = self._tracker.get(sid)
        thread_id = ctx["thread_id"]
        common = dict(
            agent_session_id=sid,
            external_session_id=thread_id,
            run_id=record.run_id if record else None,
            execution_generation=(
                record.execution_generation if record else None),
        )

        if ctx["turns"] == 0 and not resumed:
            yield self.emit(build_event(
                AgentEventType.SESSION_STARTED, seq,
                native_type="codex.thread/started",
                payload=SessionPayload(
                    transport="app-server",
                    adapter_id=self.id,
                    instance_id=self.identity.instance_id,
                    cwd=ctx["cwd"],
                    native={
                        "root_session_id": ctx["root_session_id"],
                        "mcp_injected": ctx["mcp_injected"],
                        "mcp_reload": ctx["mcp_reload"],
                    }),
                **common))
        elif resumed:
            yield self.emit(build_event(
                AgentEventType.SESSION_RESUMED, seq,
                native_type="codex.resume",
                payload=SessionPayload(transport="app-server"),
                **common))

        if ctx["turns"] == 0 and self._probe_cache and not self._probe_cache.capabilities.subagents:
            yield self.emit(build_event(
                AgentEventType.AGENT_UPDATED, seq,
                native_type="codex.capabilities",
                payload=AgentUpdatedPayload(
                    unsupported=True,
                    unsupported_reason="当前 Codex 协议未确认委派 Agent 事件能力"),
                **common))

        # A user-input continuation may already be acknowledged by the reply
        # request. Its existing consumer takes ownership without starting twice.
        conn: CodexPeer = ctx["conn"]
        result = started_result
        turn_timeout = self.conversation_turn_timeout(
            ctx.get("conversation_thread_id"), self._turn_timeout_s)

        async def _on_abort(_failure: Any) -> None:
            await self.interrupt(session)

        runner: Optional[TurnRunner] = None
        if supervise:
            # turn id is filled in after turn/start accepts the input.
            runner = TurnRunner(
                self, session, turn_id=None,
                limits=TurnLimits(idle_s=turn_timeout, overall_s=turn_timeout),
                exit_watch=conn.wait_exit,
                on_abort=_on_abort,
                diagnostics=conn.stderr_text,
                auto_ack=False,
                run_id=common.get("run_id"),
                execution_generation=common.get("execution_generation"),
            )
            if started_result is not None:
                runner.mark_sent()
                runner.ack()
        if result is None:
            try:
                capability = input.payload.runtime_capability
                if (capability.get("invocation") or {}).get("method") == "review/start":
                    if runner is not None:
                        runner.mark_sent()
                    result = await conn.request("review/start", {
                        "threadId": thread_id, "delivery": "inline",
                        "target": codex_review_target(str(capability.get("arguments") or "")),
                    }, timeout=60)
                else:
                    blocked = (
                        self._plan_mode_block(ctx)
                        if input.payload.interaction_mode == "plan" else "")
                    if blocked:
                        raise _PlanModeUnavailable(blocked)
                    if runner is not None:
                        runner.mark_sent()
                    result = await conn.request(
                        M_TURN_START, self._turn_parameters(ctx, input), timeout=60)
                    ctx["collaboration_mode"] = input.payload.interaction_mode
                if runner is not None:
                    runner.ack()
            except _PlanModeUnavailable as exc:
                yield self.emit(build_event(
                    AgentEventType.TURN_FAILED, seq,
                    native_type="codex.turn/start.plan_mode",
                    payload=TurnFailedPayload(error=self.failure(
                        FailureCategory.UNSUPPORTED, "plan_mode_unsupported",
                        message=f"Codex plan mode is unavailable: {exc}")),
                    **common))
                return
            except CodexPeerClosedError as exc:
                detail = f"reason={exc.reason}; exit_code={exc.exit_code}"
                stderr = conn.stderr_text()
                if stderr:
                    detail = f"{detail}\n{stderr}"
                failure = self.failure(
                    FailureCategory.RUNTIME_EXITED, "runtime_exited",
                    message="Codex app-server exited before the turn started",
                    detail=detail,
                    delivery_unknown=bool(runner and runner.delivery_unknown),
                )
                yield self.emit(build_event(
                    AgentEventType.TURN_FAILED, seq,
                    native_type="codex.turn/start.exited",
                    payload=TurnFailedPayload(error=failure),
                    **common))
                yield self.emit(build_event(
                    AgentEventType.RUNTIME_EXITED, seq,
                    native_type="codex.process.exit",
                    payload=RuntimeExitedPayload(
                        classification=EXIT_FAILED,
                        exit_code=exc.exit_code,
                        error=failure),
                    **common))
                return
            except (JsonRpcError, asyncio.TimeoutError) as exc:
                yield self.emit(build_event(
                    AgentEventType.TURN_FAILED, seq,
                    native_type="codex.turn/start.error",
                    payload=TurnFailedPayload(error=self.exception_failure(
                        exc, FailureCategory.TRANSPORT, "turn_start",
                        message=f"Codex turn/start failed: {exc}",
                        delivery_unknown=bool(
                            runner and runner.delivery_unknown))),
                    **common))
                return
        turn = (result or {}).get("turn") or {}
        turn_id = str(turn.get("id") or new_id("turn"))
        ctx["current_turn_id"] = turn_id
        ctx["assistant_unknown_deltas"] = []
        ctx["assistant_final_deltas"] = []
        ctx["assistant_final_text"] = ""
        ctx["assistant_legacy_final_text"] = ""
        ctx["agent_message_phases"] = {}
        ctx["reasoning_summaries"] = {}
        ctx["plan_delta_text"] = ""
        ctx["turn_is_plan"] = input.payload.interaction_mode == "plan"
        yield self.emit(build_event(
            AgentEventType.TURN_STARTED, seq, turn_id=turn_id,
            native_type="codex.turn/start",
            payload=TurnStartedPayload(kind=input.kind),
            **common))

        if runner is not None:
            runner.set_turn_id(turn_id)
            event_stream = runner.stream(self._iterate_turn_events(
                session, ctx, seq, common, turn_id))
        else:
            event_stream = self._iterate_turn_events(
                session, ctx, seq, common, turn_id)
        async for event in event_stream:
            yield event
        holds = int(ctx.get("_continuation_holds") or 0)
        if holds:
            ctx["_continuation_holds"] = holds - 1
        else:
            ctx["turns"] += 1
        ctx["current_turn_id"] = None

    async def _iterate_turn_events(
        self,
        session: AgentSessionRef,
        ctx: dict[str, Any],
        seq: Any,
        common: dict[str, Any],
        turn_id: str,
    ) -> AsyncIterator[AgentEvent]:
        """Yield one turn's events. Exit and idle limits belong to TurnRunner.

        Stdout EOF ends the source without a terminal event so the runner can
        emit ``external_agent.runtime_exited`` from the process exit code.
        """
        conn: CodexPeer = ctx["conn"]
        while True:
            kind, msg = await ctx["incoming"].get()
            if kind == "eof":
                return
            if kind == "request":
                async for event in self._handle_server_request(
                        msg, ctx, seq, common, turn_id):
                    yield event
                continue
            # Native async tools may complete their turn before the user
            # answers. Keep the public turn pending until the questions have
            # been answered, then continue on the native thread when necessary.
            params = msg.get("params") or {}
            terminal = params.get("turn") or {}
            if (msg.get("method") == "turn/completed"
                    and str(terminal.get("id") or "") == turn_id):
                ctx["current_turn_id"] = None
                if terminal.get("status") == "completed":
                    waiting = [future for key, future in ctx.get("user_inputs", {}).items()
                               if not future.done() and ctx.get("user_input_params", {}).get(key, {}).get("method") == "agentMessage/async"]
                    if waiting:
                        try:
                            await asyncio.wait_for(asyncio.gather(*waiting), timeout=self._approval_timeout_s)
                        except asyncio.TimeoutError:
                            yield self.emit(build_event(
                                AgentEventType.TURN_FAILED, seq, turn_id=turn_id,
                                native_type="codex.async_input.timeout",
                                payload=TurnFailedPayload(error=self.failure(
                                    FailureCategory.TIMEOUT, "user_input.timeout",
                                    message="等待用户输入超时")),
                                **common))
                            return
                    if ctx.pop("async_interrupted", False):
                        yield self.emit(build_event(
                            AgentEventType.TURN_FAILED, seq, turn_id=turn_id,
                            native_type="codex.async_input.interrupted",
                            payload=TurnFailedPayload(error=self.failure(
                                FailureCategory.CANCELLED, "interrupted",
                                message="Codex async user input interrupted")),
                            **common))
                        return
                    continuation = ctx.pop("async_started_turn", None)
                    if continuation:
                        # Count this native turn before the nested stream so
                        # its turns==0 session events are not emitted again.
                        # The hold makes that nested stream the one that does
                        # not also count it.
                        ctx["turns"] += 1
                        ctx["_continuation_holds"] = ctx.get("_continuation_holds", 0) + 1
                        async for event in self._turn_stream(
                            session, continuation["input"], resumed=True,
                            started_result=continuation["result"],
                            supervise=False,
                        ):
                            yield event
                        return
            events, done = self._map_notification(
                msg, ctx, seq, common, turn_id)
            for event in events:
                if event.event_type is AgentEventType.AGENT_UPDATED:
                    await self._hydrate_agent_nodes(ctx, event.payload.get("agents", []))
                yield self.emit(event)
            if done:
                return

    async def _hydrate_agent_nodes(
        self, ctx: dict[str, Any], nodes: list[dict[str, Any]]
    ) -> None:
        """Read public summaries for IDs supplied by this thread's delegation events.

        No raw rollout or reasoning is read. The paginated summary view contains
        display items, and only user requests / final agent messages are projected.
        """
        conn = ctx["conn"]
        hydrated = ctx.setdefault("agent_hydrated", set())
        for node in nodes:
            agent_id = node["agent_id"]
            # Current Codex can omit thread/started for a native child. Its
            # delegation/activity frame still supplies a confirmed child id.
            # Bind before reading metadata so a child's approval never waits
            # in an unclaimed queue while the parent waits for that child.
            conn.bind_subagent(agent_id, str(node.get("parent_id") or ctx["thread_id"]))
            terminal = node.get("status") in {"completed", "failed", "cancelled"}
            key = (agent_id, node.get("status"), ctx.get("agent_activity", {}).get(agent_id))
            if key in hydrated:
                continue
            try:
                if not node.get("request") or not node.get("model"):
                    result = await conn.request("thread/read", {
                        "threadId": agent_id, "includeTurns": False,
                    }, timeout=10)
                    child = (result or {}).get("thread") or {}
                    parent = str(child.get("parentThreadId") or "")
                    if parent:
                        conn.bind_subagent(agent_id, parent)
                        node["parent_id"] = None if parent == ctx["thread_id"] else parent
                    if child.get("preview") and "request" not in node:
                        node["request"] = str(child["preview"])
                    if child.get("model"):
                        node["model"] = str(child["model"])
                if (terminal or not node.get("request")) and "thread/turns/list" in self._client_request_methods:
                    result = await conn.request("thread/turns/list", {
                        "threadId": agent_id, "limit": 1,
                        "sortDirection": "desc", "itemsView": "summary",
                    }, timeout=10)
                    for turn in (result or {}).get("data") or []:
                        for item in turn.get("items") or []:
                            if terminal and item.get("type") == "agentMessage" and item.get("phase") != "commentary" and item.get("text"):
                                node["result"] = str(item["text"])
                            if item.get("type") == "userMessage" and "request" not in node:
                                request = "\n".join(str(part.get("text") or "") for part in item.get("content") or [] if part.get("type") == "text")
                                if request:
                                    node["request"] = request
                hydrated.add(key)
                ctx["agent_nodes"][agent_id] = dict(node)
            except (JsonRpcError, asyncio.TimeoutError, ConnectionError):
                # An older engine can report identity/status without readable
                # summaries. Retain confirmed nodes instead of guessing text.
                node.setdefault("result", None)

    # -- 服务器 request（审批 / 用户输入） ---------------------------------------

    async def _handle_server_request(
        self,
        msg: dict[str, Any],
        ctx: dict[str, Any],
        seq: Any,
        common: dict[str, Any],
        turn_id: str,
    ) -> AsyncIterator[AgentEvent]:
        method = str(msg.get("method") or "")
        request_id = msg.get("id")
        params = msg.get("params") or {}
        conn: CodexPeer = ctx["conn"]
        source_thread = str(params.get("threadId") or "")
        agent_id = source_thread if source_thread in (ctx.get("agent_nodes") or {}) else None

        if method in APPROVAL_REQUEST_METHODS:
            kind = APPROVAL_REQUEST_METHODS[method]
            future: asyncio.Future = asyncio.get_running_loop().create_future()
            # 统一用 str 键：codex 的 server-request id 可能是 int，
            # 事件 payload 与 respond_approval 入参都是 str 形态。
            ctx["approvals"][str(request_id)] = future
            ctx.setdefault("control_deliveries", {})[("approval", str(request_id))] = asyncio.get_running_loop().create_future()
            preview: dict[str, Any] = {}
            if kind == "file_change":
                # Join item/started preview (path + Diff) — approval params alone
                # do not carry them on current Codex app-server wire format.
                preview = _file_change_preview_from_ctx(params, ctx)
            files = [
                WorkspaceFileChange(
                    path=str(row["path"]),
                    change=_FILE_CHANGE_KINDS.get(
                        str(row.get("status") or "").lower()),
                    unified_diff=(
                        str(row["diff"]) if row.get("diff") else None),
                )
                for row in (preview.get("files")
                            or _normalize_file_change_entries(
                                params.get("files") or params.get("changes")))
                if row.get("path")
            ]
            yield self.emit(build_event(
                AgentEventType.APPROVAL_REQUESTED, seq, turn_id=turn_id,
                native_type=method,
                payload=ApprovalRequestedPayload(
                    agent_id=agent_id,
                    approval_id=str(request_id),
                    approval_kind=kind,
                    command=(
                        str(params["command"])
                        if params.get("command") is not None else None),
                    cwd=str(
                        preview.get("cwd")
                        or params.get("cwd") or params.get("workdir") or ""
                    ) or None,
                    reason=(
                        str(params["reason"])
                        if params.get("reason") is not None else None),
                    unified_diff=(
                        str(preview["diff"]) if preview.get("diff") else (
                            str(params["diff"]) if params.get("diff") else None)),
                    files=files,
                    path=(str(preview["path"]) if preview.get("path") else None),
                    paths=[str(p) for p in (preview.get("paths") or [])],
                    native={"params": params, "item_id": preview.get("item_id")},
                ),
                **common))
            decision: dict[str, Any]
            try:
                decision = await asyncio.wait_for(
                    future, timeout=self._approval_timeout_s)
            except asyncio.TimeoutError:
                # 审批超时不悬挂 Runtime：按 decline 应答并如实记录。
                decision = {"decision": "decline", "_timeout": True}
            ctx["approvals"].pop(str(request_id), None)
            await self._send_control_response(ctx, "approval", request_id, {"decision": decision["decision"]})
            yield self.emit(build_event(
                AgentEventType.APPROVAL_RESOLVED, seq, turn_id=turn_id,
                native_type=f"{method}.resolved",
                payload=ApprovalResolvedPayload(
                    approval_id=str(request_id),
                    decision=_codex_approval_outcome(decision["decision"]),
                    automatic=bool(decision.get("_timeout")),
                    native={"approval_kind": kind,
                            "native_decision": decision["decision"]},
                ),
                **common))
            return

        if method == M_MCP_ELICITATION and (
            isinstance(params.get("_meta"), dict)
            and params["_meta"].get("codex_approval_kind") == "mcp_tool_call"
        ):
            native_meta = dict(params.get("_meta") or {})
            future = asyncio.get_running_loop().create_future()
            ctx["approvals"][str(request_id)] = future
            ctx.setdefault("control_deliveries", {})[("approval", str(request_id))] = asyncio.get_running_loop().create_future()
            yield self.emit(build_event(
                AgentEventType.APPROVAL_REQUESTED, seq, turn_id=turn_id,
                native_type=method,
                payload=ApprovalRequestedPayload(
                    agent_id=agent_id,
                    approval_id=str(request_id),
                    approval_kind="mcp_tool_call",
                    title=str(
                        native_meta.get("tool_title")
                        or f"{params.get('serverName') or 'MCP'} 工具调用"),
                    reason=(
                        str(params["message"])
                        if params.get("message") is not None else None),
                    input=native_meta.get("tool_params"),
                    scopes=(["session"] if native_meta.get("persist") else []),
                    native={
                        "params": params,
                        "tool_description": native_meta.get("tool_description"),
                        "permission_scope": native_meta.get("persist"),
                    },
                ),
                **common))
            try:
                decision = await asyncio.wait_for(
                    future, timeout=self._approval_timeout_s)
            except asyncio.TimeoutError:
                decision = {"decision": "decline", "_timeout": True}
            ctx["approvals"].pop(str(request_id), None)
            accepted = decision["decision"] in {"accept", "acceptForSession"}
            response: dict[str, Any] = {
                "action": "accept" if accepted else "decline",
                "content": {} if accepted else None,
                "_meta": None,
            }
            if decision["decision"] == "acceptForSession":
                response["_meta"] = {"persist": "session"}
            await self._send_control_response(ctx, "approval", request_id, response)
            yield self.emit(build_event(
                AgentEventType.APPROVAL_RESOLVED, seq, turn_id=turn_id,
                native_type=f"{method}.resolved",
                payload=ApprovalResolvedPayload(
                    approval_id=str(request_id),
                    decision=_codex_approval_outcome(decision["decision"]),
                    scope=("session" if decision["decision"] == "acceptForSession"
                           else None),
                    automatic=bool(decision.get("_timeout")),
                    native={"approval_kind": "mcp_tool_call",
                            "native_decision": decision["decision"]},
                ),
                **common))
            return

        if method in USER_INPUT_REQUEST_METHODS:
            future = asyncio.get_running_loop().create_future()
            ctx["user_inputs"][str(request_id)] = future
            ctx.setdefault("control_deliveries", {})[("user_input", str(request_id))] = asyncio.get_running_loop().create_future()
            ctx["user_input_params"][str(request_id)] = {
                "method": method,
                "params": params,
            }
            questions = questions_from_codex_params(params)
            pending = dump_payload(UserInputRequestedPayload(
                agent_id=agent_id,
                request_id=str(request_id),
                user_input_kind=USER_INPUT_REQUEST_METHODS[method],
                response_actions=(["submit", "cancel", "decline"]
                                  if method == M_MCP_ELICITATION
                                  else ["submit", "cancel"]),
                title=str(params.get("message") or "") or None,
                message=str(params.get("message") or "") or None,
                questions=questions,
                native=params,
            ))
            yield self.emit(build_event(
                AgentEventType.USER_INPUT_REQUESTED, seq, turn_id=turn_id,
                native_type=method,
                payload=pending,
                **common))
            try:
                answers = await asyncio.wait_for(
                    future, timeout=self._approval_timeout_s)
            except asyncio.TimeoutError:
                answers = {"action": "cancel"} if method == M_MCP_ELICITATION else {"answers": {}}
            ctx["user_inputs"].pop(str(request_id), None)
            ctx["user_input_params"].pop(str(request_id), None)
            await self._send_control_response(ctx, "user_input", request_id, answers)
            answered = bool(
                answers.get("answers") or answers.get("action") == "accept")
            yield self.emit(build_event(
                AgentEventType.USER_INPUT_RESOLVED, seq, turn_id=turn_id,
                native_type=f"{method}.resolved",
                payload=UserInputResolvedPayload(
                    request_id=str(request_id),
                    outcome=("answered" if answered else "cancelled"),
                    native={"answered": answered},
                ),
                **common))
            return

        # 未知 server-request（如 item/tool/call dynamic tools）：不静默丢弃，
        # 以 error 应答并留 warning 事件。
        await conn._peer.respond(
            request_id,
            error={"code": -32601,
                   "message": f"unsupported server request {method}"})
        yield self.emit(build_event(
            AgentEventType.RUNTIME_WARNING, seq, turn_id=turn_id,
            native_type=method,
            payload=RuntimeWarningPayload(
                kind="protocol",
                message=f"unsupported server request {method}",
                code="codex.server_request.unsupported",
                native={"params": params}),
            **common))

    # -- 原生通知 → 统一 AgentEvent 映射 -----------------------------------------

    def _map_notification(
        self,
        msg: dict[str, Any],
        ctx: dict[str, Any],
        seq: Any,
        common: dict[str, Any],
        turn_id: str,
    ) -> tuple[list[AgentEvent], bool]:
        """通知映射；返回（事件列表, 当前 turn 是否结束）。

        未映射的通知不产生核心状态事件；完全未知的方法以 RUNTIME_WARNING
        携带完整 native 负载（不修改核心状态机，native_type 留痕）。
        """
        method = str(msg.get("method") or "")
        params = msg.get("params") or {}
        source_thread = str(params.get("threadId") or "")
        msg_turn_id = str(params.get("turnId") or "") or turn_id
        if source_thread in (ctx.get("agent_nodes") or {}):
            # Native child turn ids are private; public activity belongs to
            # the parent turn while keeping the source ids in native fields.
            msg_turn_id = turn_id

        def ev(event_type: AgentEventType, payload: dict[str, Any],
               *, tid: Optional[str] = None) -> AgentEvent:
            return build_event(
                event_type, seq, turn_id=tid or msg_turn_id,
                native_type=method, payload=payload, **common)

        if source_thread and source_thread != str(ctx.get("thread_id") or ""):
            # Native notifications may cover several subscribed threads. Only
            # this session's root stream belongs to its public conversation;
            # a confirmed child thread contributes tool activity to its agent
            # node and nothing else (its answer/turn state stay private).
            if source_thread in (ctx.get("agent_nodes") or {}):
                return self._map_child_notification(
                    ev, method, params, ctx, source_thread), False
            return [], False

        if method == "thread/compacted":
            self._context_compacted(common["agent_session_id"])
            return [], False
        if method == "thread/started":
            thread = params.get("thread") or {}
            child_id = str(thread.get("id") or "")
            if (thread.get("parentThreadId") and child_id
                    and child_id != str(ctx.get("thread_id") or "")):
                return self._child_thread_started(ev, ctx, thread), False
            # thread 身份已在 _launch 回填；保留 native 供排障。
            ctx["thread_started_native"] = params
            return [], False
        if method == "turn/started":
            # TURN_STARTED 已在 turn/start 响应后发出，避免重复。
            return [], False
        if method == "turn/completed":
            turn = params.get("turn") or {}
            status = str(turn.get("status") or "")
            if str(turn.get("id") or "") != turn_id:
                return [], False  # 其他 turn（review/compact）不影响本流
            if status == "completed":
                text = str(ctx.get("assistant_final_text") or "")
                if not text:
                    text = "".join(ctx.get("assistant_final_deltas") or [])
                if not text:
                    text = str(ctx.get("assistant_legacy_final_text") or "")
                if not text:
                    text = "".join(ctx.get("assistant_unknown_deltas") or [])
                if not text.strip() and ctx.get("turn_is_plan"):
                    # collaborationMode=plan：计划本体就是本 turn 的答复。
                    text = str(ctx.get("plan_delta_text") or "")
                if not text.strip():
                    return [
                        ev(AgentEventType.RUNTIME_WARNING,
                           dump_payload(RuntimeWarningPayload(
                               kind="degraded", code="external_agent.empty_assistant",
                               message="Codex turn ended without assistant text"))),
                        ev(AgentEventType.TURN_COMPLETED,
                           dump_payload(TurnCompletedPayload(
                               stop_reason=status, duration_ms=turn.get("durationMs")))),
                    ], True
                return [
                    ev(AgentEventType.MESSAGE_COMPLETED,
                       dump_payload(MessageCompletedPayload(
                           text=text, phase="final_answer")),
                       tid=turn_id),
                    ev(AgentEventType.TURN_COMPLETED,
                       dump_payload(TurnCompletedPayload(
                           stop_reason=status or None,
                           duration_ms=turn.get("durationMs"),
                           native={"item_count": len(turn.get("items") or [])})),
                       tid=turn_id),
                ], True
            # interrupted / failed：如实记 turn.failed，不伪造完成。
            native_error = turn.get("error")
            return [ev(AgentEventType.TURN_FAILED,
                       dump_payload(TurnFailedPayload(
                           error=self.failure(
                               FailureCategory.CANCELLED
                               if status == "interrupted"
                               else FailureCategory.PROVIDER,
                               "interrupted" if status == "interrupted"
                               else "turn_failed",
                               message=str(
                                   native_error
                                   or f"Codex turn {status or 'unknown'}")),
                           native={"status": status or "unknown",
                                   "error": native_error})),
                       tid=turn_id)], True
        if method == "thread/tokenUsage/updated":
            usage = params.get("tokenUsage") or {}
            total = usage.get("total") or {}
            return [ev(AgentEventType.USAGE_UPDATED,
                       dump_payload(UsagePayload(
                           scope="session_cumulative",
                           input_tokens=total.get("inputTokens"),
                           output_tokens=total.get("outputTokens"),
                           cached_input_tokens=total.get("cachedInputTokens"),
                           reasoning_tokens=total.get("reasoningOutputTokens"),
                           total_tokens=total.get("totalTokens"),
                           context_window=usage.get("modelContextWindow"),
                           native=params,
                       )))], False
        if method == "item/agentMessage/delta":
            text = str(params.get("delta") or "")
            if not text:
                return [], False
            item_id = str(params.get("itemId") or "")
            phase = str(
                (ctx.get("agent_message_phases") or {}).get(item_id) or ""
            )
            if phase == "final_answer":
                ctx.setdefault("assistant_final_deltas", []).append(text)
            elif not phase:
                ctx.setdefault("assistant_unknown_deltas", []).append(text)
            return [ev(AgentEventType.MESSAGE_DELTA,
                       dump_payload(MessageDeltaPayload(
                           text=text,
                           phase=(phase if phase in {"commentary", "final_answer"}
                                  else None),
                           message_id=item_id or None,
                       )))], False
        if method == "item/reasoning/summaryTextDelta":
            text = str(params.get("delta") or "")
            if not text:
                return [], False
            item_id = str(params.get("itemId") or "")
            summaries = ctx.setdefault("reasoning_summaries", {})
            summaries[item_id] = f"{summaries.get(item_id, '')}{text}"
            return [ev(AgentEventType.REASONING_SUMMARY,
                       dump_payload(ReasoningPayload(
                           text=text, channel="summary", partial=True,
                           item_id=item_id or None)))], False
        if method == "item/reasoning/textDelta":
            # This notification contains raw reasoning text. The product only
            # exposes summaryTextDelta / the completed item's summary field.
            return [], False
        if method in ("item/commandExecution/outputDelta",
                      "item/mcpToolCall/progress",
                      "item/fileChange/outputDelta"):
            return [ev(AgentEventType.TOOL_PROGRESS,
                       dump_payload(ToolPayload(
                           tool_call_id=str(params.get("itemId") or ""),
                           chunk=str(params.get("delta")
                                     or params.get("output") or ""),
                       )))], False
        if method == "item/started":
            return self._map_item(ev, params, ctx, started=True), False
        if method == "item/completed":
            if (params.get("item") or {}).get("type") == "contextCompaction":
                self._context_compacted(common["agent_session_id"])
            return self._map_item(ev, params, ctx, started=False), False
        if method == "turn/diff/updated":
            diff = str(
                params.get("diff")
                or params.get("unifiedDiff")
                or params.get("patch")
                or ""
            )
            return [ev(AgentEventType.WORKSPACE_CHANGED,
                       dump_payload(WorkspaceChangedPayload(
                           unified_diff=diff or None, native=params)))], False
        if method == "mcpServer/startupStatus/updated":
            status = str(params.get("status") or "")
            if status.lower() in ("failed", "error"):
                return [ev(AgentEventType.RUNTIME_WARNING,
                           dump_payload(RuntimeWarningPayload(
                               kind="degraded",
                               message="mcp server startup failed",
                               code="codex.mcp.startup_failed",
                               native={"server": params.get("name"),
                                       "params": params},
                           )))], False
            return [], False
        if method == "account/rateLimits/updated":
            return [ev(AgentEventType.RUNTIME_WARNING,
                       dump_payload(RuntimeWarningPayload(
                           kind="rate_limit",
                           message="Codex account rate limits updated",
                           rate_limit=RateLimitState(**_codex_rate_limit(params)),
                           native=params,
                       )))], False
        if method == "turn/plan/updated":
            return [ev(AgentEventType.PLAN_UPDATED,
                       _codex_plan_payload(params, patch=False))], False
        if method == "item/plan/delta":
            # Plan-mode turns end with the plan item, not an agentMessage;
            # keep the accumulated text for the completion fallback.
            ctx["plan_delta_text"] = (
                str(ctx.get("plan_delta_text") or "")
                + str(params.get("delta") or ""))
            return [ev(AgentEventType.PLAN_UPDATED,
                       _codex_plan_payload(params, patch=True))], False
        if method in ("thread/status/changed", "serverRequest/resolved",
                      "remoteControl/status/changed", "model/rerouted",
                      "thread/name/updated",
                      "item/reasoning/summaryPartAdded"):
            # 已知信息类通知：不进核心状态机，不产生事件。
            return [], False
        # 未知原生通知：RUNTIME_WARNING + native 保留，不改核心状态机。
        return [ev(AgentEventType.RUNTIME_WARNING,
                   dump_payload(RuntimeWarningPayload(
                       kind="protocol",
                       message=f"unmapped native notification {method}",
                       code="codex.notification.unmapped",
                       native={"params": params},
                   )))], False

    @staticmethod
    def _child_thread_started(
        ev: Any, ctx: dict[str, Any], thread: dict[str, Any],
    ) -> list[AgentEvent]:
        nodes = ctx.setdefault("agent_nodes", {})
        child_id = str(thread.get("id") or "")
        root_id = str(ctx.get("thread_id") or "")
        parent = str(thread.get("parentThreadId") or "")
        previous = nodes.get(child_id, {})
        node = {**previous, "agent_id": child_id, "session_ref": child_id}
        node.setdefault("title", child_id)
        node.setdefault("status", "pending")
        if "parent_id" not in node:
            node["parent_id"] = None if parent == root_id else parent
        for key, native in (("nickname", "agentNickname"),
                            ("role", "agentRole"), ("model", "model")):
            if thread.get(native):
                node[key] = str(thread[native])
        if node == previous:
            return []
        nodes[child_id] = node
        return [ev(AgentEventType.AGENT_UPDATED,
                   dump_payload(AgentUpdatedPayload(
                       agents=[AgentNodePayload(**node)], patch=True)))]

    @classmethod
    def _map_child_notification(
        cls, ev: Any, method: str, params: dict[str, Any],
        ctx: dict[str, Any], child_id: str,
    ) -> list[AgentEvent]:
        if method in ("item/commandExecution/outputDelta",
                      "item/mcpToolCall/progress",
                      "item/fileChange/outputDelta"):
            return [ev(AgentEventType.TOOL_PROGRESS,
                       dump_payload(ToolPayload(
                           tool_call_id=str(params.get("itemId") or ""),
                           chunk=str(params.get("delta")
                                     or params.get("output") or ""),
                           agent_id=child_id,
                       )))]
        if method not in ("item/started", "item/completed"):
            return []
        item = params.get("item") or {}
        item_type = str(item.get("type") or "")
        if item_type == "agentMessage":
            text = str(item.get("text") or "").strip()
            if method != "item/completed" or not text:
                return []
            nodes = ctx.setdefault("agent_nodes", {})
            node = {**nodes.get(child_id, {}), "activity": text}
            nodes[child_id] = node
            return [ev(AgentEventType.AGENT_UPDATED,
                       dump_payload(AgentUpdatedPayload(
                           agents=[AgentNodePayload(**node)], patch=True)))]
        if item_type not in ("commandExecution", "mcpToolCall", "fileChange",
                             "dynamicToolCall", "collabToolCall",
                             "collabAgentToolCall", "subAgentActivity"):
            return []
        events = cls._map_item(
            ev, params, ctx, started=method == "item/started")
        for event in events:
            event.payload.setdefault("native", {}).update({
                "child_thread_id": child_id, "child_turn_id": params.get("turnId"),
            })
            if event.event_type in (AgentEventType.TOOL_STARTED,
                                    AgentEventType.TOOL_COMPLETED):
                event.payload["agent_id"] = child_id
        return events

    @staticmethod
    def _map_item(
        ev: Any,
        params: dict[str, Any],
        ctx: dict[str, Any],
        *,
        started: bool,
    ) -> list[AgentEvent]:
        item = params.get("item") or {}
        item_type = str(item.get("type") or "")
        call_id = str(item.get("id") or "")
        if item_type in {"collabAgentToolCall", "subAgentActivity"}:
            nodes = ctx.setdefault("agent_nodes", {})
            root_id = str(ctx.get("thread_id") or "")
            changed: list[dict[str, Any]] = []
            status_map = {
                "pendingInit": "pending", "running": "running",
                "completed": "completed", "errored": "failed",
                "interrupted": "cancelled", "shutdown": "cancelled",
                "notFound": "failed",
            }
            if item_type == "collabAgentToolCall":
                fingerprint = hashlib.sha256(json.dumps(item, sort_keys=True, ensure_ascii=False).encode()).digest()
                seen = ctx.setdefault("seen_collab_items", set())
                if fingerprint in seen:
                    return []
                seen.add(fingerprint)
                sender = str(item.get("senderThreadId") or root_id)
                states = item.get("agentsStates") or {}
                receivers = list(dict.fromkeys([
                    *(item.get("receiverThreadIds") or []), *states.keys(),
                ]))
                for agent_id in receivers:
                    if not agent_id or agent_id == root_id:
                        continue
                    previous = nodes.get(agent_id, {})
                    native_state = states.get(agent_id) or {}
                    new_work = item.get("tool") in {"spawnAgent", "resumeAgent", "followupTask"} and call_id != previous.get("call_id")
                    node = {**previous, "agent_id": agent_id,
                            "title": previous.get("title") or agent_id,
                            "call_id": call_id if new_work else previous.get("call_id") or call_id}
                    if new_work:
                        # Explicit None: agent-tree patches merge by key.
                        node["result"] = None
                        node["error"] = None
                        if previous:
                            node["request"] = None  # metadata.preview is the first task, not this followup
                        node["status"] = "pending"
                        if item.get("prompt"):
                            node["request"] = str(item["prompt"])
                    if item.get("tool") == "spawnAgent":
                        node.update({"parent_id": None if sender == root_id else sender,
                                     "request": item.get("prompt") or previous.get("request"),
                                     "model": item.get("model") or previous.get("model")})
                    if native_state.get("status") in status_map:
                        status = status_map[native_state["status"]]
                        if not (previous.get("status") in {"completed", "failed", "cancelled"}
                                and status in {"pending", "running"} and not new_work):
                            node["status"] = status
                    else:
                        node.setdefault("status", "pending")
                    if native_state.get("message") and node.get("status") in {"completed", "failed", "cancelled"}:
                        node["result"] = str(native_state["message"])
                    if node != previous:
                        ctx.setdefault("agent_activity", {})[agent_id] = call_id
                        nodes[agent_id] = node
                        changed.append(node)
            else:
                agent_id = str(item.get("agentThreadId") or "")
                if agent_id and agent_id != root_id:
                    activity = (agent_id, call_id, str(item.get("kind") or ""))
                    seen = ctx.setdefault("seen_agent_activity", set())
                    if activity in seen:
                        return []
                    seen.add(activity)
                    previous = nodes.get(agent_id, {})
                    path = str(item.get("agentPath") or agent_id)
                    paths = ctx.setdefault("agent_paths", {})
                    paths[path] = agent_id
                    new_work = item.get("kind") == "started" and call_id != previous.get("call_id")
                    node = {**previous, "agent_id": agent_id, "title": path,
                            "call_id": call_id if new_work else previous.get("call_id") or call_id}
                    if new_work:
                        # Explicit None: agent-tree patches merge by key.
                        node["result"] = None
                        node["error"] = None
                        if previous:
                            node["request"] = None
                    if "parent_id" not in node:
                        node["parent_id"] = paths.get(path.rsplit("/", 1)[0])
                    status = {"started": "running", "interrupted": "cancelled",
                              "completed": "completed"}.get(str(item.get("kind") or ""))
                    if status and not (
                        status == "running" and previous.get("status") in {"completed", "failed", "cancelled"}
                        and previous.get("call_id") == call_id
                    ):
                        node["status"] = status
                    else:
                        node.setdefault("status", "pending")
                    if node != previous:
                        ctx.setdefault("agent_activity", {})[agent_id] = call_id
                        nodes[agent_id] = node
                        changed.append(node)
            return [ev(AgentEventType.AGENT_UPDATED,
                       dump_payload(AgentUpdatedPayload(
                           agents=[AgentNodePayload(**node) for node in changed],
                           patch=True)))] if changed else []
        if item_type in ("commandExecution", "mcpToolCall", "fileChange",
                         "dynamicToolCall", "collabToolCall"):
            tool_name = {
                "commandExecution": "shell",
                "mcpToolCall": str(item.get("tool") or item.get("server") or "mcp"),
                "fileChange": "file_change",
            }.get(item_type, item_type)
            if started:
                if item_type == "fileChange":
                    _cache_file_change_item(ctx, item if isinstance(item, dict) else {})
                return [ev(AgentEventType.TOOL_STARTED,
                           dump_payload(ToolPayload(
                               tool_call_id=call_id,
                               name=tool_name,
                               input=str(item.get("command")
                                         or item.get("arguments") or ""),
                               status="running",
                               kind=("mcp" if item_type == "mcpToolCall"
                                     else "command"
                                     if item_type == "commandExecution"
                                     else "file_change"
                                     if item_type == "fileChange" else None),
                               native=({"mcp_server": str(item["server"])}
                                       if item_type == "mcpToolCall"
                                       and item.get("server") else {}),
                           )))]
            if item_type == "fileChange":
                # Refresh cache on completed so late/retry previews stay accurate.
                _cache_file_change_item(ctx, item if isinstance(item, dict) else {})
            output = (item.get("aggregatedOutput") or item.get("output")
                      or _mcp_result_text(item.get("result")) or "")
            return [ev(AgentEventType.TOOL_COMPLETED,
                       dump_payload(ToolPayload(
                           tool_call_id=call_id,
                           name=tool_name,
                           output=str(output),
                           exit_code=(item.get("exitCode")
                                      if isinstance(item.get("exitCode"), int)
                                      else None),
                           status=("failed"
                                   if str(item.get("status") or "") == "failed"
                                   else "completed"),
                           native={"native_status": item.get("status")},
                       )))]
        if item_type == "agentMessage" and item.get("delivery") == "async" and item.get("questions"):
            if started or not call_id or call_id in ctx.setdefault("async_question_ids", set()):
                return []
            ctx["async_question_ids"].add(call_id)
            pending = dump_payload(UserInputRequestedPayload(
                request_id=call_id,
                user_input_kind="codex_async",
                message=str(item.get("text") or "") or None,
                questions=[
                    normalized
                    for i, raw in enumerate(item["questions"])
                    if (normalized := normalize_question(raw, index=i))
                    is not None
                ] if isinstance(item["questions"], list) else [],
                response_actions=["submit", "cancel"],
                native={"asynchronous": True},
            ))
            futures = ctx.setdefault("user_inputs", {})
            queued = any(not future.done() for future in futures.values())
            futures[call_id] = asyncio.get_running_loop().create_future()
            ctx.setdefault("user_input_params", {})[call_id] = {
                "method": "agentMessage/async", "pending": pending,
            }
            return [] if queued else [ev(AgentEventType.USER_INPUT_REQUESTED, pending)]
        if item_type == "agentMessage" and not started:
            text = str(item.get("text") or "")
            phase = str(item.get("phase") or "")
            if call_id and phase in {"commentary", "final_answer"}:
                ctx.setdefault("agent_message_phases", {})[call_id] = phase
            if text and phase == "final_answer":
                ctx["assistant_final_text"] = text
            elif text and phase != "commentary":
                # Older providers omit phase. Keep the last completed message
                # as the compatibility candidate for the terminal answer.
                ctx["assistant_legacy_final_text"] = text
            # 最终消息只在 turn/completed 确认成功后落库，避免随后失败时
            # 留下一条已完成的助手消息。
            return []
        if item_type == "agentMessage" and started:
            phase = str(item.get("phase") or "")
            if call_id and phase in {"commentary", "final_answer"}:
                ctx.setdefault("agent_message_phases", {})[call_id] = phase
            return []
        if item_type == "reasoning" and not started:
            summary = "\n".join(
                str(part) for part in (item.get("summary") or []) if str(part)
            )
            streamed = str(
                (ctx.get("reasoning_summaries") or {}).get(call_id) or ""
            )
            if summary and not streamed:
                return [ev(AgentEventType.REASONING_SUMMARY,
                           dump_payload(ReasoningPayload(
                               text=summary, channel="summary",
                               item_id=call_id or None)))]
            return []
        return []

    # -- 控制面 -----------------------------------------------------------------

    async def steer(
        self, session: AgentSessionRef, input: AgentInput
    ) -> CommandReceipt:
        """turn/steer：向进行中的 turn 追加输入（不产生新 turn/started）。"""
        if not isinstance(input, SteerInput):
            return self.unsupported_receipt(
                "steer", "steer", session=session, detail={"input_kind": input.kind})
        ctx = self._runs.get(session.agent_session_id)
        if ctx is None or not ctx["conn"].alive:
            return self.unsupported_receipt("steer", "steer", session=session)
        turn_id = ctx.get("current_turn_id")
        if not turn_id:
            return self.unsupported_receipt(
                "steer", "no_active_turn", session=session,
                detail={"detail": "no in-flight turn for this session"})
        params: dict[str, Any] = {
            "threadId": ctx["thread_id"],
            # turn/steer 的前置条件是 Codex App Server 的原生 turn id。
            # Conversation Turn id 只在 Muteki 聚合内使用，不能传到这里。
            "expectedTurnId": str(turn_id),
            "input": codex_turn_input(input.text, input.payload.attachments),
        }
        client_message_id = input.payload.client_user_message_id.strip()
        if client_message_id:
            params["clientUserMessageId"] = client_message_id
        try:
            await ctx["conn"].request(M_TURN_STEER, params, timeout=30)
        except JsonRpcError as exc:
            return self.unsupported_receipt(
                "steer", "steer_rejected", session=session,
                detail={"code": exc.code, "message": exc.message})
        return CommandReceipt(
            command_id=new_id("cmd"), state=ReceiptState.COMPLETED,
            aggregate=AggregateRef(type="agent_session",
                                   id=session.agent_session_id))

    async def interrupt(self, session: AgentSessionRef) -> CommandReceipt:
        self._mark_turn_interrupted(session.agent_session_id)
        """turn/interrupt：turn 以 status=interrupted 结束（真实事件）。"""
        ctx = self._runs.get(session.agent_session_id)
        if ctx is None or not ctx["conn"].alive:
            return self.unsupported_receipt(
                "interrupt", "interrupt", session=session)
        turn_id = ctx.get("current_turn_id")
        if not turn_id:
            waiting = [future for key, future in ctx.get("user_inputs", {}).items()
                       if not future.done() and ctx.get("user_input_params", {}).get(key, {}).get("method") == "agentMessage/async"]
            if waiting:
                ctx["async_interrupted"] = True
                for future in waiting:
                    future.set_result({"answered": False})
                return CommandReceipt(
                    command_id=new_id("cmd"), state=ReceiptState.COMPLETED,
                    aggregate=AggregateRef(type="agent_session", id=session.agent_session_id))
            return self.unsupported_receipt(
                "interrupt", "no_active_turn", session=session,
                detail={"detail": "no in-flight turn for this session"})
        try:
            await ctx["conn"].request(
                M_TURN_INTERRUPT,
                {"threadId": ctx["thread_id"], "turnId": turn_id}, timeout=30)
        except JsonRpcError as exc:
            return self.unsupported_receipt(
                "interrupt", "interrupt_rejected", session=session,
                detail={"code": exc.code, "message": exc.message})

        # app-server 的 server request 与普通 notification 共用同一条消费
        # 协程。Turn 停在审批或用户输入时，turn/interrupt 虽然已经成功，
        # 消费协程仍会阻塞在这里的 Future 上，无法继续读取随后的
        # turn/completed(interrupted)。主动取消所有待处理交互，让 Turn 能够
        # 完整收尾；对应 request 仍会收到合法的拒绝/空答复。
        for future in tuple((ctx.get("approvals") or {}).values()):
            if not future.done():
                future.set_result({"decision": "cancel"})
        user_input_params = ctx.get("user_input_params") or {}
        for request_id, future in tuple(
            (ctx.get("user_inputs") or {}).items()
        ):
            if future.done():
                continue
            native = user_input_params.get(str(request_id)) or {}
            if native.get("method") == M_MCP_ELICITATION:
                future.set_result({
                    "action": "decline", "content": None, "_meta": None,
                })
            else:
                future.set_result({"answers": {}})
        return CommandReceipt(
            command_id=new_id("cmd"), state=ReceiptState.COMPLETED,
            aggregate=AggregateRef(type="agent_session",
                                   id=session.agent_session_id))

    async def fork_thread(
        self, session: AgentSessionRef, *, target_request: Optional[SessionStart] = None,
    ) -> dict[str, Any]:
        """Create the fork on its target connection, as T3 does.

        Without a target, the native fork remains on this shared app-server.
        Supplying a target binds its own provider environment, cwd and MCP
        grant before ``thread/fork``; an existing writer is never migrated.
        """
        ctx = self._runs.get(session.agent_session_id)
        if ctx is None or not ctx["conn"].alive:
            raise RuntimeError("session is not active on this adapter")
        if target_request is not None:
            target = target_request.model_copy(update={
                "resume_handle": None,
                "options": target_request.options.model_copy(update={"fork_from": ctx["thread_id"]}),
            })
            ref = await self.start(target)
            fork_ctx = self._runs[ref.agent_session_id]
            return {"thread_id": fork_ctx["thread_id"], "root_session_id": fork_ctx["root_session_id"],
                    "agent_session_id": ref.agent_session_id, "session_ref": ref.model_dump(mode="json")}
        result = await ctx["conn"].request(
            M_THREAD_FORK,
            {"threadId": ctx["thread_id"], "excludeTurns": True}, timeout=60)
        thread = (result or {}).get("thread") or {}
        fork_id = str(thread.get("id") or "")
        if fork_id:
            self._fork_connections[fork_id] = ctx["conn"]
        return {
            "thread_id": str(thread.get("id") or ""),
            "root_session_id": str(thread.get("sessionId") or ""),
        }

    async def mcp_server_status(self, session: AgentSessionRef) -> list[dict[str, Any]]:
        """mcpServerStatus/list：注入的 MCP server 在 Runtime 侧的真实状态。"""
        ctx = self._runs.get(session.agent_session_id)
        if ctx is None or not ctx["conn"].alive:
            raise RuntimeError("session is not active on this adapter")
        rows: list[dict[str, Any]] = []
        cursor = None
        seen: set[str] = set()
        while True:
            result = await ctx["conn"].request(M_MCP_STATUS, {
                "threadId": ctx["thread_id"], **({"cursor": cursor} if cursor else {}),
            }, timeout=30)
            rows.extend(list((result or {}).get("data") or []))
            cursor = (result or {}).get("nextCursor")
            if not cursor:
                return rows
            if cursor in seen:
                error = RuntimeError("Codex MCP inventory repeated its pagination cursor")
                error.code = "codex.mcp.cursor_invalid"
                raise error
            seen.add(cursor)

    async def _thread_has_active_turn(self, ctx: dict[str, Any]) -> bool:
        result = await ctx["conn"].request(
            "thread/read", {"threadId": ctx["thread_id"], "includeTurns": True}, timeout=30)
        turns = ((result or {}).get("thread") or {}).get("turns") or []
        return any(turn.get("status") == "inProgress" for turn in turns)

    def supports_native_rewind(self, session: AgentSessionRef) -> bool:
        return (session.agent_session_id in self._runs
                and bool({"thread/revert", "thread/rollback"}.intersection(self._client_request_methods)))

    async def rewind_session(self, session: AgentSessionRef, native_turn_id: str) -> dict[str, Any]:
        ctx = self._runs.get(session.agent_session_id)
        if not ctx or not self.supports_native_rewind(session):
            raise RuntimeError("当前 Codex 未提供历史回退接口")
        conn = ctx["conn"]
        if "thread/revert" in self._client_request_methods:
            await conn.request("thread/revert", {"threadId": ctx["thread_id"], "beforeTurnId": native_turn_id}, timeout=60)
            ctx["current_turn_id"] = None
            return {"strategy": "native", "method": "thread/revert"}
        result = await conn.request("thread/read", {"threadId": ctx["thread_id"], "includeTurns": True})
        turns = (result.get("thread") or {}).get("turns") or []
        index = next((i for i, turn in enumerate(turns) if turn.get("id") == native_turn_id), None)
        if index is None:
            raise RuntimeError("目标轮次已不在原生会话中，请刷新聊天后重试")
        await conn.request("thread/rollback", {"threadId": ctx["thread_id"], "numTurns": len(turns) - index}, timeout=60)
        ctx["current_turn_id"] = None
        return {"strategy": "native", "removed": len(turns) - index}

    async def runtime_operation(
        self, session: AgentSessionRef, name: str, arguments: str = ""
    ) -> dict[str, Any]:
        """Invoke operations proven by the installed App Server schema."""
        ctx = self._runs.get(session.agent_session_id)
        if ctx is None or not ctx["conn"].alive:
            raise RuntimeError("session is not active on this adapter")
        normalized = str(name or "").strip().lstrip("/")
        if normalized == "goal":
            method = "thread/goal/get" if not arguments else "thread/goal/clear" if arguments in {"off", "clear"} else "thread/goal/set"
            if method not in self._client_request_methods:
                raise RuntimeError("当前 Codex 未提供目标接口")
            params = {"threadId": ctx["thread_id"]}
            if method.endswith("set"):
                params["objective"] = arguments
            return {"status": "completed", "result": await ctx["conn"].request(method, params)}
        if normalized == "mcp" and arguments == "reload":
            if M_MCP_RELOAD not in self._client_request_methods:
                raise RuntimeError("当前 Codex 未提供 MCP 刷新接口")
            return {"status": "completed", "result": await ctx["conn"].request(M_MCP_RELOAD, {})}
        if arguments:
            raise ValueError(f"/{normalized} 不接受此参数")
        if normalized == "compact":
            method = "thread/compact/start"
            if method not in self._client_request_methods:
                raise RuntimeError("当前 Codex 不支持结构化压缩接口")
            if ctx.get("current_turn_id"):
                raise RuntimeError("当前 Codex 正在回复，请结束后再压缩")
            conn = ctx["conn"]
            await conn.request(method, {"threadId": ctx["thread_id"]}, timeout=30)
            # The RPC only acknowledges scheduling. Wait for the actual native
            # compaction turn, including its terminal status, before succeeding.
            compact_turn = None
            async with asyncio.timeout(180):
                while True:
                    kind, message = await ctx["incoming"].get()
                    if kind == "eof":
                        raise RuntimeError("Codex 在压缩完成前断开连接")
                    if kind != "notification":
                        if kind == "request":
                            await conn._peer.respond(message["id"], error={"code": -32601, "message": "No interactive requests during compact"})
                        continue
                    params = message.get("params") or {}
                    if params.get("threadId") != ctx["thread_id"]:
                        continue
                    event = message.get("method")
                    if event == "turn/started":
                        compact_turn = (params.get("turn") or {}).get("id")
                    if event == "item/started" and (params.get("item") or {}).get("type") == "contextCompaction":
                        compact_turn = params.get("turnId")
                    if event == "turn/completed" and compact_turn and (params.get("turn") or {}).get("id") == compact_turn:
                        status = (params.get("turn") or {}).get("status")
                        if status != "completed":
                            raise RuntimeError(f"Codex 压缩未完成：{status}")
                        return {"status": "completed", "message": "上下文已由 Codex 原生压缩"}
        cwd = str(ctx.get("cwd") or "")
        method_params: dict[str, tuple[str, dict[str, Any]]] = {
            "status": ("thread/read", {"threadId": ctx["thread_id"], "includeTurns": False}),
            "usage": ("account/rateLimits/read", {}),
            "models": (M_MODEL_LIST, {"limit": 100}),
            "skills": (M_SKILLS_LIST, {
                "cwds": [cwd] if cwd else [], "forceReload": False,
            }),
            "hooks": (M_HOOKS_LIST, {"cwds": [cwd] if cwd else []}),
            "plugins": (M_PLUGIN_LIST, {
                "cwds": [cwd] if cwd else [],
                "forceRefetch": False,
                "marketplaceKinds": ["local", "workspace-directory"],
            }),
            "apps": (M_APP_LIST, {
                "threadId": ctx.get("thread_id"),
                "limit": 100,
                "forceRefetch": False,
            }),
            "mcp": (M_MCP_STATUS, {
                "threadId": ctx.get("thread_id"),
                "detail": "toolsAndAuthOnly",
                "limit": 100,
            }),
        }
        method, params = method_params.get(normalized, ("", {}))
        if not method or method not in self._client_request_methods:
            raise RuntimeError(f"Codex App Server 未公布只读操作：{normalized}")
        result = await ctx["conn"].request(method, params, timeout=30)
        return dict(result or {})

    async def runtime_capability_snapshot(
        self, session: Optional[AgentSessionRef] = None
    ) -> RuntimeCapabilitySnapshot:
        base = await super().runtime_capability_snapshot(session)
        if session is None:
            return base
        ctx = self._runs.get(session.agent_session_id)
        if ctx is None or not ctx["conn"].alive:
            return base.model_copy(update={
                "stale": True,
                "diagnostics": ["Codex App Server Session 当前不在本进程"],
            })

        items = list(base.items)
        diagnostics: list[str] = []
        cwd = str(ctx.get("cwd") or "")
        if M_SKILLS_LIST in self._client_request_methods:
            try:
                result = await ctx["conn"].request(M_SKILLS_LIST, {
                    "cwds": [cwd] if cwd else [], "forceReload": False,
                }, timeout=30)
                for entry in (result or {}).get("data") or []:
                    if not isinstance(entry, dict):
                        continue
                    diagnostics.extend(
                        str(error.get("message") or "")
                        for error in entry.get("errors") or []
                        if isinstance(error, dict) and error.get("message")
                    )
                    for skill in entry.get("skills") or []:
                        if not isinstance(skill, dict) or not skill.get("enabled", True):
                            continue
                        name = str(skill.get("name") or "").strip()
                        if not name:
                            continue
                        items.append(dynamic_command_item(
                            adapter_id=self.id,
                            engine="codex",
                            name=name,
                            description=str(skill.get("description") or ""),
                            channel="provider_native",
                            kind="skill",
                            invocation={
                                "command": name,
                                "wire_text": f"${name}",
                                "protocol": "codex.turn/start",
                                "path": str(skill.get("path") or ""),
                            },
                        ))
            except (JsonRpcError, asyncio.TimeoutError) as exc:
                diagnostics.append(f"skills/list 读取失败：{exc}")

        operations = {
            "rewind": ("thread/revert" if "thread/revert" in self._client_request_methods else "thread/rollback", "回退 Codex 原生会话与聊天记录；保留工作区文件"),
            "status": ("thread/read", "查看 Codex 原生会话状态"),
            "usage": ("account/rateLimits/read", "查看 Codex 账户用量限制"),
            "models": (M_MODEL_LIST, "查看 Codex 原生模型目录"),
            "goal": ("thread/goal/get", "查看或设置当前 Codex 会话目标；off 清除"),
            "compact": ("thread/compact/start", "由 Codex 原生压缩当前上下文，保留对话历史"),
            "skills": (M_SKILLS_LIST, "查看当前 Codex Skill 目录"),
            "hooks": (M_HOOKS_LIST, "查看当前 Codex Hook 与信任状态"),
            "plugins": (M_PLUGIN_LIST, "查看本地与项目插件"),
            "apps": (M_APP_LIST, "查看当前 Codex App 连接"),
            "mcp": (M_MCP_STATUS, "查看 Codex Runtime 报告的 MCP 状态"),
        }
        for name, (method, description) in operations.items():
            if method not in self._client_request_methods:
                continue
            if name == "apps" and not self._experimental_api:
                continue
            items.append(RuntimeCapabilityItem(
                id=f"runtime:{self.id}:operation:{name}",
                kind="operation",
                name=name,
                description=description,
                source="Codex App Server",
                scope="session",
                engine="codex",
                channel="app_server_rpc",
                resolution="client",
                origin="verified_static",
                delivery="guaranteed",
                verification="verified",
                action="invoke-runtime-operation",
                invocation={"method": method},
            ))

        if "review/start" in self._client_request_methods:
            items.append(dynamic_command_item(
                adapter_id=self.id, engine="codex", name="review", description="启动 Codex 原生代码审查",
                argument_hint="[说明 | --base 分支 | --commit 提交]", channel="app_server_rpc",
                invocation={"method": "review/start", "wire_text": "/review"},
            ))
        if M_MCP_STATUS in self._client_request_methods:
            try:
                statuses = await self.mcp_server_status(session)
                reported_names = {
                    str(status.get("name") or "")
                    for status in statuses if isinstance(status, dict)
                }
                items = [
                    item for item in items
                    if not (
                        item.kind == "mcp_status"
                        and item.name in reported_names
                    )
                ]
                for status in statuses:
                    if not isinstance(status, dict):
                        continue
                    name = str(status.get("name") or "").strip()
                    if not name:
                        continue
                    tools = status.get("tools") or {}
                    items.append(RuntimeCapabilityItem(
                        id=f"runtime:{self.id}:mcp:{name}",
                        kind="mcp_status",
                        name=name,
                        description=(
                            f"Runtime 已报告 · {len(tools)} 个工具 · "
                            f"认证 {status.get('authStatus') or 'unknown'}"
                        ),
                        source="Codex App Server",
                        scope="session",
                        engine="codex",
                        channel="app_server_rpc",
                        resolution="runtime",
                        origin="dynamic",
                        delivery="guaranteed",
                        verification="verified",
                        status="runtime_reported",
                        invocation={
                            "method": M_MCP_STATUS,
                            "auth_status": status.get("authStatus"),
                            "tool_count": len(tools),
                        },
                    ))
            except (JsonRpcError, asyncio.TimeoutError) as exc:
                diagnostics.append(
                    f"mcpServerStatus/list 读取失败：{exc}")

        revision = self._capability_revisions.get(session.agent_session_id, 0) + 1
        self._capability_revisions[session.agent_session_id] = revision
        return base.model_copy(update={
            "revision": revision,
            "items": items,
            "diagnostics": diagnostics,
        })

    async def _teardown(self, session: AgentSessionRef) -> str:
        ctx = self._runs.get(session.agent_session_id)
        if not ctx:
            return EXIT_CLOSED
        conn: CodexPeer = ctx["conn"]
        had_turn = ctx.get("current_turn_id") is not None
        if conn.alive:
            try:
                await conn.request(
                    M_THREAD_UNSUBSCRIBE, {"threadId": ctx["thread_id"]},
                    timeout=10)
            except (JsonRpcError, asyncio.TimeoutError) as exc:
                # Closing the process below ends the subscription anyway.
                _LOG.warning("codex thread/unsubscribe failed: %s: %s",
                             type(exc).__name__, exc)
        self._runs.pop(session.agent_session_id, None)
        conn.release_thread(ctx["thread_id"])
        if any(run["conn"] is conn for run in self._runs.values()):
            returncode = 0
        else:
            returncode = await conn.close()
            self._fork_connections = {key: peer for key, peer in self._fork_connections.items() if peer is not conn}
        self._capability_revisions.pop(session.agent_session_id, None)
        if had_turn:
            return classify_exit(cancelled=True)
        if returncode not in (0, -1):
            return EXIT_FAILED
        # 保留 resume_handle 的 Session 可经 thread/resume 恢复。
        return classify_exit(returncode=0, resume_handle=ctx["thread_id"])


__all__ = [
    "APPROVAL_DECISIONS",
    "APPROVAL_REQUEST_METHODS",
    "CodexAppServerAdapter",
    "CodexPeer",
    "DEFAULT_CODEX_BIN",
    "JsonRpcError",
    "MCP_TOKEN_ENV",
    "USER_INPUT_REQUEST_METHODS",
    "export_protocol_methods",
]
