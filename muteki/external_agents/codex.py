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
- MCP 注入：``thread/start`` **不接受** ``mcpServers``；按核验结论在 spawn
  时传 ``-c mcp_servers.<name>.url`` / ``-c mcp_servers.<name>.bearer_token_env_var``，
  bearer token 只经子进程环境变量进入（不落 argv、不落盘），随后
  ``config/mcpServer/reload`` 让配置确定性生效（新进程本就是新配置，
  reload 是幂等保险）。可选 ``codex_home`` 实现 CODEX_HOME 隔离。
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
import os
import subprocess
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
    AgentSessionRef,
    ProbeRequest,
    SessionStart,
)
from muteki.platform.contracts.receipts import AggregateRef, CommandReceipt, ReceiptState

from .base import BaseExternalAgentAdapter
from .approvals import ApprovalDecision, ApprovalRequest
from .attachment_input import codex_turn_input
from .capabilities import (
    BOOL_CAPABILITY_FIELDS,
    CapabilityProbeReport,
    SOURCE_PROBE,
    SOURCE_REPORTED,
    SOURCE_STATIC,
    conservative_capabilities,
)
from .events import build_event
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
    normalize_pending_user_input,
    questions_from_codex_params,
)

#: 默认 binary 与 MCP server 名（与 CAP-02 acp_config 默认一致）。
DEFAULT_CODEX_BIN = "codex"
MCP_SERVER_NAME = acp_config.DEFAULT_SERVER_NAME

#: bearer token 经该环境变量进入 app-server 子进程（T3 同名机制核验；
#: 变量名进 ``-c mcp_servers.<name>.bearer_token_env_var``，token 本体不进 argv）。
MCP_TOKEN_ENV = "MUTEKI_CAPABILITY_TOKEN"

#: probe / 注入用的稳定 wire 方法名（实测 0.147.0 schema 复核）。
M_INITIALIZE = "initialize"
M_INITIALIZED = "initialized"
M_THREAD_START = "thread/start"
M_THREAD_RESUME = "thread/resume"
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
    payload: dict[str, Any] = {
        "tasks": tasks,
        "phase": phase,
        "patch": patch,
        "source": "adapter",
    }
    if title:
        payload["title"] = title
    turn_id = str(params.get("turnId") or params.get("turn_id") or "")
    if turn_id:
        payload["turn_id"] = turn_id
    return payload


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


class CodexPeer:
    """一条 codex app-server stdio JSON-RPC 连接（``StdioJsonlPeer`` 封装）。

    - ``request`` 把 error 响应转为 ``JsonRpcError``；
    - 入站非响应消息按是否带 ``id`` 分为服务器 request / 通知，统一进
      ``incoming`` 队列；reader EOF（进程退出）时放入 ``("eof", None)``；
    - stderr 后台排空（app-server 日志走 stderr，pipe 不消费会撑满阻塞）。
    """

    def __init__(self, peer: StdioJsonlPeer) -> None:
        self._peer = peer
        self.incoming: "asyncio.Queue[tuple[str, Any]]" = asyncio.Queue()
        self._stderr_task: Optional[asyncio.Task] = None
        self._stderr_tail = bytearray()

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
            await conn.incoming.put((kind, msg))

        peer = StdioJsonlPeer(
            argv, env=env, cwd=cwd, label="codex-app-server",
            on_message=on_message)
        conn = cls(peer)
        holder["self"] = conn
        await peer.start()
        # EOF 通知：reader task 结束（含进程退出）时唤醒 turn 消费循环。
        if peer._reader_task is not None:
            peer._reader_task.add_done_callback(
                lambda _task: conn.incoming.put_nowait(("eof", None)))
        conn._stderr_task = asyncio.ensure_future(conn._drain_stderr())
        return conn

    async def _drain_stderr(self) -> None:
        proc = self._peer._proc
        if proc is None or proc.stderr is None:
            return
        try:
            while chunk := await proc.stderr.read(65536):
                self._stderr_tail.extend(chunk)
                if len(self._stderr_tail) > 16384:
                    del self._stderr_tail[:-16384]
        except (asyncio.CancelledError, Exception):  # noqa: BLE001
            pass

    def _exit_detail(self) -> str:
        proc = self._peer._proc
        returncode = proc.returncode if proc is not None else None
        stderr = bytes(self._stderr_tail).decode(
            "utf-8", errors="replace").strip()
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
            raise JsonRpcError(
                -32099, f"{exc}; {self._exit_detail()}") from exc
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
        if self._stderr_task is not None:
            self._stderr_task.cancel()
        return await self._peer.close()


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
            capture_output=True, text=True, timeout=timeout,
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
    return methods if any(methods.values()) else {}


def _probe_version(binary: str, *, timeout: float = 15.0) -> str:
    try:
        result = subprocess.run(
            [binary, "--version"], capture_output=True, text=True,
            encoding="utf-8", errors="replace", timeout=timeout,
        )
    except (FileNotFoundError, subprocess.TimeoutExpired, OSError):
        return ""
    lines = (result.stdout or result.stderr or "").strip().splitlines()
    return lines[0].strip()[:120] if result.returncode == 0 and lines else ""


# ---------------------------------------------------------------------------
# Adapter
# ---------------------------------------------------------------------------


class CodexAppServerAdapter(BaseExternalAgentAdapter):
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

    def __init__(
        self,
        *,
        binary: str = DEFAULT_CODEX_BIN,
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
        self._binary = binary
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
                detail="binary 不可用，使用保守默认")
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
            conn = await self._spawn_initialized_peer(
                argv=[self._binary, "app-server", "--listen", "stdio://"],
                env=self._spawn_env({}), cwd=None,
                init_params={"clientInfo": {
                    "name": "muteki-probe", "title": "Muteki Probe",
                    "version": "0.1.0"}},
            )
            handshake_ok = True
            if request.include_models and M_MODEL_LIST in client_req:
                try:
                    result = await conn.request(
                        M_MODEL_LIST, {"limit": 50}, timeout=30)
                    models = (result or {}).get("data") or []
                    caps.supported_models = [
                        str(m.get("model") or m.get("id") or "")
                        for m in models if isinstance(m, dict)][:50]
                    caps.supported_models = [m for m in caps.supported_models if m]
                    efforts: list[str] = []
                    for m in models:
                        for e in (m.get("supportedReasoningEfforts") or []):
                            value = str(e.get("reasoningEffort") or "")
                            if value and value not in efforts:
                                efforts.append(value)
                    caps.supported_efforts = efforts
                    field_sources["supported_models"] = SOURCE_PROBE
                    field_sources["supported_efforts"] = SOURCE_PROBE
                except (JsonRpcError, asyncio.TimeoutError) as exc:
                    degradations.append(f"model/list 失败：{str(exc)[:120]}")
        except (OSError, JsonRpcError, asyncio.TimeoutError) as exc:
            degradations.append(f"app-server initialize 握手失败：{str(exc)[:120]}")
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
            detail="" if handshake_ok else "handshake 未通过")
        return caps

    # -- 启动（任务书 7.6 步骤 4 的 Runtime 侧动作） ---------------------------

    def _spawn_env(self, extra: dict[str, str]) -> dict[str, str]:
        env = dict(os.environ)
        env.update(self._default_env)
        env.update(extra)
        if self._codex_home:
            env["CODEX_HOME"] = self._codex_home
        return env

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

    def _mcp_spawn_args(
        self, plan: Optional[CapabilityInjectionPlan]
    ) -> list[str]:
        """按注入计划生成 spawn 级 MCP 覆盖参数。

        核验结论（§CODEX-2 / §T3）：``thread/start`` 不接受 mcpServers；
        走 spawn 时 ``-c mcp_servers.<name>.url/bearer_token_env_var`` +
        子进程环境变量携带 token 本体。
        """
        if plan is None or plan.injection_kind is not InjectionKind.MCP:
            return []
        endpoint = plan.gateway_endpoint
        if not endpoint:
            return []
        return [
            "-c", f'mcp_servers.{MCP_SERVER_NAME}.url="{endpoint}"',
            "-c", f'mcp_servers.{MCP_SERVER_NAME}.bearer_token_env_var="{MCP_TOKEN_ENV}"',
        ]

    @staticmethod
    def _retryable_initialize_error(exc: BaseException) -> bool:
        """Codex 本地状态库初始化竞争只影响本次子进程启动。"""
        detail = str(exc).casefold()
        return (
            "failed to initialize sqlite state runtime" in detail
            or "failed to initialize state runtime" in detail
        )

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
            except (OSError, JsonRpcError, asyncio.TimeoutError) as exc:
                if conn is not None:
                    await conn.close()
                if (attempt + 1 >= attempts
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
        cwd = str(request.options.get("cwd") or self._default_cwd or os.getcwd())
        extra_env = {k: str(v)
                     for k, v in (request.options.get("env") or {}).items()}
        spawn_env = self._spawn_env(extra_env)
        provider_args = self._provider_spawn_args(spawn_env)
        mcp_args = self._mcp_spawn_args(plan)
        if mcp_args and bearer_token:
            # token 本体只进子进程环境变量（Secret materialization）。
            spawn_env[MCP_TOKEN_ENV] = bearer_token

        init_params: dict[str, Any] = {"clientInfo": {
            "name": "muteki", "title": "Muteki ExternalAgentAdapter",
            "version": "runtime-02"}}
        if self._experimental_api:
            init_params["capabilities"] = {"experimentalApi": True}
        conn = await self._spawn_initialized_peer(
            # -c 覆盖属于 app-server 子命令参数（T3 同名机制核验）。
            # provider_args 必须在 mcp_args 之前：自定义 endpoint 的
            # model_provider 绑定与 MCP 注入同属 spawn 级 -c 覆盖。
            argv=[self._binary, "app-server", *provider_args, *mcp_args,
                  "--listen", "stdio://"],
            env=spawn_env, cwd=cwd,
            init_params=init_params)
        try:
            reload_status = "skipped"
            if mcp_args:
                # 新进程已带 -c 覆盖；reload 是核验建议的确定性保险
                # （对写 config.toml 的路径是必需）。旧版无此方法时容忍。
                try:
                    await conn.request(M_MCP_RELOAD, timeout=30)
                    reload_status = "reloaded"
                except JsonRpcError as exc:
                    reload_status = f"reload unavailable: {exc.code}"

            thread_params: dict[str, Any] = {
                "cwd": cwd,
                "serviceName": "muteki",
            }
            model = request.model or self._model
            if model:
                thread_params["model"] = model
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

            fork_from = str(request.options.get("fork_from") or "")
            if fork_from:
                result = await conn.request(
                    M_THREAD_FORK, {"threadId": fork_from}, timeout=60)
            elif request.resume_handle:
                result = await conn.request(
                    M_THREAD_RESUME,
                    {"threadId": request.resume_handle, **thread_params},
                    timeout=60)
            else:
                result = await conn.request(M_THREAD_START, thread_params, timeout=60)
        except Exception:
            await conn.close()
            raise

        thread = (result or {}).get("thread") or {}
        thread_id = str(thread.get("id") or "")
        if not thread_id:
            await conn.close()
            raise RuntimeError("thread/start 响应缺少 thread.id")
        self._runs[sid] = {
            "conn": conn,
            "conversation_thread_id": request.thread_id,
            "thread_id": thread_id,
            # fork 后 sessionId 保持根线程 id，单独记录（核验 §CODEX 末节）。
            "root_session_id": str(thread.get("sessionId") or thread_id),
            "cwd": cwd,
            "effort": request.effort or self._effort,
            "access_config": access_config,
            "turns": 0,
            "current_turn_id": None,
            "approvals": {},      # request_id -> asyncio.Future
            "file_change_items": {},  # item_id -> {files,diff,path,paths}
            "user_inputs": {},    # request_id -> asyncio.Future
            "user_input_params": {},  # request_id -> 原生 request 上下文
            "mcp_reload": reload_status if mcp_args else "no-injection",
            "mcp_injected": bool(mcp_args),
            "thread_started_native": None,
            "resume_prompt": str(
                request.options.get("resume_prompt")
                or "Continue from where you left off."
            ),
        }
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
        future.set_result({"decision": decision})
        return CommandReceipt(
            command_id=new_id("cmd"), state=ReceiptState.COMPLETED,
            aggregate=AggregateRef(type="agent_session",
                                   id=session.agent_session_id))

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
                        "input": codex_turn_input(reply, {}),
                    }, timeout=30)
                except JsonRpcError as exc:
                    # A completion can race the steer. Only the explicit
                    # no-active-turn condition permits starting a continuation.
                    if "no active turn" not in exc.message.lower():
                        raise
                    needs_continuation = True
            if future.done():
                raise RuntimeError("问题已取消或过期，回答未再提交")
            if needs_continuation:
                continuation = AgentInput(kind="message", text=reply,
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
            if decision == "cancel":
                future.set_result({"action": "decline", "content": {}, "_meta": None})
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
        """Map structured answers (or legacy text) onto MCP elicitation fields."""
        schema = params.get("requestedSchema")
        schema_dict = schema if isinstance(schema, dict) else {}
        if structured:
            normalized = {
                str(qid): (
                    value if isinstance(value, dict)
                    else {"values": [str(value)], "text": str(value)}
                )
                for qid, value in structured.items()
            }
            return content_for_elicitation(normalized, schema=schema_dict)
        properties = schema_dict.get("properties") if schema_dict else None
        if not isinstance(properties, dict) or not properties:
            return {}
        key, raw_spec = next(iter(properties.items()))
        spec = raw_spec if isinstance(raw_spec, dict) else {}
        value: Any = text
        field_type = str(spec.get("type") or "string")
        if field_type == "boolean":
            value = text.strip().lower() in {
                "1", "true", "yes", "on", "是", "允许",
            }
        elif field_type == "integer":
            value = int(text)
        elif field_type == "number":
            value = float(text)
        return {str(key): value}

    # -- turn 流 ----------------------------------------------------------------

    def send(
        self, session: AgentSessionRef, input: AgentInput
    ) -> AsyncIterator[AgentEvent]:
        if input.kind == "approval_response":
            return self._approval_response_stream(session, input)
        if input.kind == "user_input_response":
            return self._user_input_response_stream(session, input)
        return self._turn_stream(session, input)

    async def _approval_response_stream(
        self, session: AgentSessionRef, input: AgentInput
    ) -> AsyncIterator[AgentEvent]:
        approval = ApprovalDecision.from_payload(input.payload)
        receipt = await self.respond_approval(
            session, approval.approval_id, approval.codex_decision())
        if receipt.state is ReceiptState.FAILED:
            yield self.emit(build_event(
                AgentEventType.RUNTIME_ERROR,
                self.sequencer_for(session.agent_session_id),
                agent_session_id=session.agent_session_id,
                external_session_id=session.external_session_id,
                payload={"code": receipt.error.code if receipt.error else "",
                         "operation": "approval_response"},
            ))

    async def _user_input_response_stream(
        self, session: AgentSessionRef, input: AgentInput
    ) -> AsyncIterator[AgentEvent]:
        answers = dict(input.payload.get("answers") or {})
        if not answers and input.text:
            answers = {"__text__": input.text}
        decision = str(input.payload.get("decision") or "submit")
        answers["__decision__"] = decision
        if input.text and "__text__" not in answers:
            answers["__text__"] = input.text
        receipt = await self.respond_user_input(
            session, input.payload.get("request_id"), answers)
        if receipt.state is ReceiptState.FAILED:
            yield self.emit(build_event(
                AgentEventType.RUNTIME_ERROR,
                self.sequencer_for(session.agent_session_id),
                agent_session_id=session.agent_session_id,
                external_session_id=session.external_session_id,
                payload={"code": receipt.error.code if receipt.error else "",
                         "operation": "user_input_response"},
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
                        native_type="agentMessage/async", payload=native["pending"],
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
            session, AgentInput(kind="message", text=prompt), resumed=True)

    @staticmethod
    def _turn_parameters(ctx: dict[str, Any], input: AgentInput) -> dict[str, Any]:
        params: dict[str, Any] = {
            "threadId": ctx["thread_id"],
            "input": codex_turn_input(input.text, input.payload),
        }
        if ctx.get("effort"):
            params["effort"] = ctx["effort"]
        if input.payload.get("client_user_message_id"):
            params["clientUserMessageId"] = str(input.payload["client_user_message_id"])
        access = ctx.get("access_config")
        if access:
            params.update({"approvalPolicy": access["approvalPolicy"],
                           "approvalsReviewer": access["approvalsReviewer"],
                           "sandboxPolicy": access["sandboxPolicy"]})
        return params

    async def _turn_stream(
        self,
        session: AgentSessionRef,
        input: AgentInput,
        *,
        resumed: bool = False,
        started_result: Optional[dict[str, Any]] = None,
    ) -> AsyncIterator[AgentEvent]:
        sid = session.agent_session_id
        seq = self.sequencer_for(sid)
        ctx = self._runs.get(sid)
        if ctx is None:
            yield self.emit(build_event(
                AgentEventType.RUNTIME_ERROR, seq,
                agent_session_id=sid,
                external_session_id=session.external_session_id,
                payload={"code": "external_agent.session.unknown",
                         "detail": "session was not started by this adapter"},
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
                payload={
                    "transport": "app-server",
                    "adapter_id": self.id,
                    "instance_id": self.identity.instance_id,
                    "cwd": ctx["cwd"],
                    "root_session_id": ctx["root_session_id"],
                    "mcp_injected": ctx["mcp_injected"],
                    "mcp_reload": ctx["mcp_reload"],
                },
                **common))
        elif resumed:
            yield self.emit(build_event(
                AgentEventType.SESSION_RESUMED, seq,
                native_type="codex.resume",
                payload={"transport": "app-server"},
                **common))

        if ctx["turns"] == 0 and self._probe_cache and not self._probe_cache.capabilities.subagents:
            yield self.emit(build_event(
                AgentEventType.AGENT_UPDATED, seq,
                native_type="codex.capabilities",
                payload={"agents": [], "unsupported": True, "adapter_id": self.id,
                         "unsupported_reason": "当前 Codex 协议未确认委派 Agent 事件能力"},
                **common))

        # A user-input continuation may already be acknowledged by the reply
        # request. Its existing consumer takes ownership without starting twice.
        conn: CodexPeer = ctx["conn"]
        result = started_result
        if result is None:
            try:
                result = await conn.request(
                    M_TURN_START, self._turn_parameters(ctx, input), timeout=60)
            except (JsonRpcError, asyncio.TimeoutError) as exc:
                yield self.emit(build_event(
                    AgentEventType.TURN_FAILED, seq,
                    native_type="codex.turn/start.error",
                    payload={"error": str(exc)[:300]},
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
        yield self.emit(build_event(
            AgentEventType.TURN_STARTED, seq, turn_id=turn_id,
            native_type="codex.turn/start",
            payload={"kind": input.kind},
            **common))

        done = False
        turn_timeout = self.conversation_turn_timeout(
            ctx.get("conversation_thread_id"), self._turn_timeout_s)
        while not done:
            try:
                if turn_timeout is None:
                    kind, msg = await conn.incoming.get()
                else:
                    kind, msg = await asyncio.wait_for(
                        conn.incoming.get(), timeout=turn_timeout)
            except asyncio.TimeoutError:
                yield self.emit(build_event(
                    AgentEventType.TURN_FAILED, seq, turn_id=turn_id,
                    native_type="codex.turn.timeout",
                    payload={"reason": "turn timeout",
                             "timeout_s": turn_timeout},
                    **common))
                break
            if kind == "eof":
                classification = classify_exit(
                    returncode=None, error="app-server stdout EOF",
                    resume_handle=thread_id)
                yield self.emit(build_event(
                    AgentEventType.TURN_FAILED, seq, turn_id=turn_id,
                    native_type="codex.process.eof",
                    payload={"reason": "app-server exited mid-turn"},
                    **common))
                yield self.emit(build_event(
                    AgentEventType.RUNTIME_EXITED, seq, turn_id=turn_id,
                    native_type="codex.process.exit",
                    payload={"classification": classification},
                    **common))
                break
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
                                payload={"reason": "等待用户输入超时", "code": "codex.user_input.timeout"},
                                **common))
                            break
                    if ctx.pop("async_interrupted", False):
                        yield self.emit(build_event(
                            AgentEventType.TURN_FAILED, seq, turn_id=turn_id,
                            native_type="codex.async_input.interrupted",
                            payload={"reason": "interrupted", "status": "interrupted"},
                            **common))
                        break
                    continuation = ctx.pop("async_started_turn", None)
                    if continuation:
                        ctx["turns"] += 1
                        async for event in self._turn_stream(
                            session, continuation["input"], resumed=True,
                            started_result=continuation["result"],
                        ):
                            yield event
                        return
            # notification
            events, done = self._map_notification(
                msg, ctx, seq, common, turn_id)
            for event in events:
                if event.event_type is AgentEventType.AGENT_UPDATED:
                    await self._hydrate_agent_nodes(ctx, event.payload.get("agents", []))
                yield self.emit(event)
        ctx["turns"] += 1
        ctx["current_turn_id"] = None

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

        if method in APPROVAL_REQUEST_METHODS:
            kind = APPROVAL_REQUEST_METHODS[method]
            future: asyncio.Future = asyncio.get_running_loop().create_future()
            # 统一用 str 键：codex 的 server-request id 可能是 int，
            # 事件 payload 与 respond_approval 入参都是 str 形态。
            ctx["approvals"][str(request_id)] = future
            details = {
                "approval_kind": kind,
                "command": params.get("command"),
                "cwd": params.get("cwd") or params.get("workdir"),
                "reason": params.get("reason"),
                "diff": (
                    params.get("diff")
                    or params.get("patch")
                    or params.get("unifiedDiff")
                    or params.get("unified_diff")
                ),
                "files": params.get("files") or params.get("changes"),
                "native": params,
            }
            if kind == "file_change":
                # Join item/started preview (path + Diff) — approval params alone
                # do not carry them on current Codex app-server wire format.
                details.update(_file_change_preview_from_ctx(params, ctx))
            yield self.emit(build_event(
                AgentEventType.APPROVAL_REQUESTED, seq, turn_id=turn_id,
                native_type=method,
                payload=ApprovalRequest(
                    approval_id=str(request_id),
                    details={
                        key: value for key, value in details.items()
                        if value not in (None, "", [], {})
                    },
                ).to_payload(),
                **common))
            decision: dict[str, Any]
            try:
                decision = await asyncio.wait_for(
                    future, timeout=self._approval_timeout_s)
            except asyncio.TimeoutError:
                # 审批超时不悬挂 Runtime：按 decline 应答并如实记录。
                decision = {"decision": "decline", "_timeout": True}
            ctx["approvals"].pop(str(request_id), None)
            await conn.respond(request_id, {"decision": decision["decision"]})
            yield self.emit(build_event(
                AgentEventType.APPROVAL_RESOLVED, seq, turn_id=turn_id,
                native_type=f"{method}.resolved",
                payload={
                    "approval_id": str(request_id),
                    "approval_kind": kind,
                    "decision": decision["decision"],
                    "auto_declined": bool(decision.get("_timeout")),
                },
                **common))
            return

        if method == M_MCP_ELICITATION and (
            isinstance(params.get("_meta"), dict)
            and params["_meta"].get("codex_approval_kind") == "mcp_tool_call"
        ):
            native_meta = dict(params.get("_meta") or {})
            future = asyncio.get_running_loop().create_future()
            ctx["approvals"][str(request_id)] = future
            yield self.emit(build_event(
                AgentEventType.APPROVAL_REQUESTED, seq, turn_id=turn_id,
                native_type=method,
                payload=ApprovalRequest(
                    approval_id=str(request_id),
                    details={
                        "approval_kind": "mcp_tool_call",
                        "action": (
                            native_meta.get("tool_title")
                            or f"{params.get('serverName') or 'MCP'} 工具调用"
                        ),
                        "message": params.get("message"),
                        "reason": params.get("message"),
                        "arguments": native_meta.get("tool_params"),
                        "tool_description": native_meta.get("tool_description"),
                        "permission_scope": native_meta.get("persist"),
                        "native": params,
                    },
                ).to_payload(),
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
            await conn.respond(request_id, response)
            yield self.emit(build_event(
                AgentEventType.APPROVAL_RESOLVED, seq, turn_id=turn_id,
                native_type=f"{method}.resolved",
                payload={
                    "approval_id": str(request_id),
                    "approval_kind": "mcp_tool_call",
                    "decision": decision["decision"],
                    "auto_declined": bool(decision.get("_timeout")),
                },
                **common))
            return

        if method in USER_INPUT_REQUEST_METHODS:
            future = asyncio.get_running_loop().create_future()
            ctx["user_inputs"][str(request_id)] = future
            ctx["user_input_params"][str(request_id)] = {
                "method": method,
                "params": params,
            }
            questions = questions_from_codex_params(params)
            pending = normalize_pending_user_input({
                "request_id": str(request_id),
                "user_input_kind": USER_INPUT_REQUEST_METHODS[method],
                "title": params.get("message") or "",
                "message": params.get("message") or "",
                "questions": questions,
                "native": params,
            })
            yield self.emit(build_event(
                AgentEventType.USER_INPUT_REQUESTED, seq, turn_id=turn_id,
                native_type=method,
                payload=pending,
                **common))
            try:
                answers = await asyncio.wait_for(
                    future, timeout=self._approval_timeout_s)
            except asyncio.TimeoutError:
                answers = {"answers": {}}
            ctx["user_inputs"].pop(str(request_id), None)
            ctx["user_input_params"].pop(str(request_id), None)
            await conn.respond(request_id, answers)
            yield self.emit(build_event(
                AgentEventType.USER_INPUT_RESOLVED, seq, turn_id=turn_id,
                native_type=f"{method}.resolved",
                payload={"request_id": str(request_id),
                         "answered": bool(
                             answers.get("answers")
                             or answers.get("action") == "accept"
                         )},
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
            payload={"warning": "unsupported server request",
                     "native": params},
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
        if source_thread and source_thread != str(ctx.get("thread_id") or ""):
            # Native notifications may cover several subscribed threads. Only
            # this session's root stream belongs to its public conversation;
            # delegated summaries are fetched explicitly by confirmed child ID.
            return [], False
        msg_turn_id = str(params.get("turnId") or "") or turn_id

        def ev(event_type: AgentEventType, payload: dict[str, Any],
               *, tid: Optional[str] = None) -> AgentEvent:
            return build_event(
                event_type, seq, turn_id=tid or msg_turn_id,
                native_type=method, payload=payload, **common)

        if method == "thread/started":
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
                if not text.strip():
                    return [ev(AgentEventType.TURN_FAILED, {
                        "status": status,
                        "reason": "empty_assistant",
                        "error": {
                            "code": "codex.empty_assistant",
                            "message": "Codex turn ended without assistant text",
                        },
                    }, tid=turn_id)], True
                return [
                    ev(AgentEventType.MESSAGE_COMPLETED, {
                        "text": text,
                        "role": "assistant",
                        "phase": "final_answer",
                    }, tid=turn_id),
                    ev(AgentEventType.TURN_COMPLETED, {
                    "status": status,
                    "duration_ms": turn.get("durationMs"),
                    "item_count": len(turn.get("items") or []),
                    }, tid=turn_id),
                ], True
            # interrupted / failed：如实记 turn.failed，不伪造完成。
            return [ev(AgentEventType.TURN_FAILED, {
                "status": status or "unknown",
                "reason": "interrupted" if status == "interrupted" else "failed",
                "error": turn.get("error"),
            }, tid=turn_id)], True
        if method == "thread/tokenUsage/updated":
            usage = params.get("tokenUsage") or {}
            total = usage.get("total") or {}
            return [ev(AgentEventType.USAGE_UPDATED, {
                "usage": {
                    "input_tokens": total.get("inputTokens"),
                    "output_tokens": total.get("outputTokens"),
                    "cached_input_tokens": total.get("cachedInputTokens"),
                    "reasoning_output_tokens": total.get("reasoningOutputTokens"),
                    "total_tokens": total.get("totalTokens"),
                    "model_context_window": usage.get("modelContextWindow"),
                },
            })], False
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
            payload = {"text": text, "role": "assistant"}
            if phase in {"commentary", "final_answer"}:
                payload["phase"] = phase
            if item_id:
                payload["item_id"] = item_id
            return [ev(AgentEventType.MESSAGE_DELTA, payload)], False
        if method == "item/reasoning/summaryTextDelta":
            text = str(params.get("delta") or "")
            if not text:
                return [], False
            item_id = str(params.get("itemId") or "")
            summaries = ctx.setdefault("reasoning_summaries", {})
            summaries[item_id] = f"{summaries.get(item_id, '')}{text}"
            return [ev(AgentEventType.REASONING_SUMMARY, {
                "text": text,
                "item_id": item_id,
                "delta": True,
            })], False
        if method == "item/reasoning/textDelta":
            # This notification contains raw reasoning text. The product only
            # exposes summaryTextDelta / the completed item's summary field.
            return [], False
        if method in ("item/commandExecution/outputDelta",
                      "item/mcpToolCall/progress",
                      "item/fileChange/outputDelta"):
            return [ev(AgentEventType.TOOL_PROGRESS, {
                "call_id": str(params.get("itemId") or ""),
                "chunk": str(params.get("delta") or params.get("output") or "")[:1000],
            })], False
        if method == "item/started":
            return self._map_item(ev, params, ctx, started=True), False
        if method == "item/completed":
            return self._map_item(ev, params, ctx, started=False), False
        if method == "turn/diff/updated":
            diff = str(
                params.get("diff")
                or params.get("unifiedDiff")
                or params.get("patch")
                or ""
            )
            return [ev(AgentEventType.WORKSPACE_CHANGED, {
                "diff": diff,
                "native": params,
            })], False
        if method == "mcpServer/startupStatus/updated":
            status = str(params.get("status") or "")
            if status.lower() in ("failed", "error"):
                return [ev(AgentEventType.RUNTIME_WARNING, {
                    "warning": "mcp server startup failed",
                    "server": params.get("name"), "native": params})], False
            return [], False
        if method == "account/rateLimits/updated":
            return [ev(AgentEventType.RUNTIME_WARNING,
                       {"warning": "rate limits updated",
                        "native": params})], False
        if method == "turn/plan/updated":
            return [ev(AgentEventType.PLAN_UPDATED, {
                **_codex_plan_payload(params, patch=False),
                "native": params,
            })], False
        if method == "item/plan/delta":
            return [ev(AgentEventType.PLAN_UPDATED, {
                **_codex_plan_payload(params, patch=True),
                "native": params,
            })], False
        if method in ("thread/status/changed", "serverRequest/resolved",
                      "remoteControl/status/changed", "model/rerouted",
                      "thread/name/updated",
                      "item/reasoning/summaryPartAdded"):
            # 已知信息类通知：不进核心状态机，不产生事件。
            return [], False
        # 未知原生通知：RUNTIME_WARNING + native 保留，不改核心状态机。
        return [ev(AgentEventType.RUNTIME_WARNING,
                   {"warning": "unmapped native notification",
                    "native": params})], False

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
                        node.pop("result", None)
                        node.pop("error", None)
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
                        node.pop("result", None)
                        node.pop("error", None)
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
            return [ev(AgentEventType.AGENT_UPDATED, {
                "agents": changed, "patch": True, "adapter_id": "codex.app_server",
            })] if changed else []
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
                return [ev(AgentEventType.TOOL_STARTED, {
                    "tool": tool_name, "call_id": call_id,
                    "input": str(item.get("command")
                                 or item.get("arguments") or "")[:500],
                })]
            if item_type == "fileChange":
                # Refresh cache on completed so late/retry previews stay accurate.
                _cache_file_change_item(ctx, item if isinstance(item, dict) else {})
            output = (item.get("aggregatedOutput") or item.get("output")
                      or item.get("result") or "")
            return [ev(AgentEventType.TOOL_COMPLETED, {
                "tool": tool_name, "call_id": call_id,
                "output": str(output)[:2000],
                "exit_code": item.get("exitCode"),
                "status": item.get("status"),
            })]
        if item_type == "agentMessage" and item.get("delivery") == "async" and item.get("questions"):
            if started or not call_id or call_id in ctx.setdefault("async_question_ids", set()):
                return []
            ctx["async_question_ids"].add(call_id)
            pending = normalize_pending_user_input({
                "request_id": call_id, "user_input_kind": "codex_async",
                "asynchronous": True, "message": str(item.get("text") or ""),
                "questions": item["questions"],
            })
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
            )[:2000]
            streamed = str(
                (ctx.get("reasoning_summaries") or {}).get(call_id) or ""
            )
            if summary and not streamed:
                return [ev(AgentEventType.REASONING_SUMMARY, {
                    "text": summary,
                    "item_id": call_id,
                    "delta": False,
                })]
            return []
        return []

    # -- 控制面 -----------------------------------------------------------------

    async def steer(
        self, session: AgentSessionRef, input: AgentInput
    ) -> CommandReceipt:
        """turn/steer：向进行中的 turn 追加输入（不产生新 turn/started）。"""
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
            "input": codex_turn_input(input.text, input.payload),
        }
        client_message_id = str(
            input.payload.get("client_user_message_id") or ""
        ).strip()
        if client_message_id:
            params["clientUserMessageId"] = client_message_id
        try:
            await ctx["conn"].request(M_TURN_STEER, params, timeout=30)
        except JsonRpcError as exc:
            return self.unsupported_receipt(
                "steer", "steer_rejected", session=session,
                detail={"code": exc.code, "message": exc.message[:200]})
        return CommandReceipt(
            command_id=new_id("cmd"), state=ReceiptState.COMPLETED,
            aggregate=AggregateRef(type="agent_session",
                                   id=session.agent_session_id))

    async def interrupt(self, session: AgentSessionRef) -> CommandReceipt:
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
                detail={"code": exc.code, "message": exc.message[:200]})

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

    async def fork_thread(self, session: AgentSessionRef) -> dict[str, Any]:
        """thread/fork：从当前 thread 分叉（fork 后 sessionId 保持根 id）。"""
        ctx = self._runs.get(session.agent_session_id)
        if ctx is None or not ctx["conn"].alive:
            raise RuntimeError("session is not active on this adapter")
        result = await ctx["conn"].request(
            M_THREAD_FORK, {"threadId": ctx["thread_id"]}, timeout=60)
        thread = (result or {}).get("thread") or {}
        return {
            "thread_id": str(thread.get("id") or ""),
            "root_session_id": str(thread.get("sessionId") or ""),
        }

    async def mcp_server_status(self, session: AgentSessionRef) -> list[dict[str, Any]]:
        """mcpServerStatus/list：注入的 MCP server 在 Runtime 侧的真实状态。"""
        ctx = self._runs.get(session.agent_session_id)
        if ctx is None or not ctx["conn"].alive:
            raise RuntimeError("session is not active on this adapter")
        result = await ctx["conn"].request(M_MCP_STATUS, {}, timeout=30)
        return list((result or {}).get("data") or [])

    async def runtime_operation(
        self, session: AgentSessionRef, name: str
    ) -> dict[str, Any]:
        """调用 Codex App Server 的只读管理 RPC。"""
        ctx = self._runs.get(session.agent_session_id)
        if ctx is None or not ctx["conn"].alive:
            raise RuntimeError("session is not active on this adapter")
        normalized = str(name or "").strip().lstrip("/")
        cwd = str(ctx.get("cwd") or "")
        method_params: dict[str, tuple[str, dict[str, Any]]] = {
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
                diagnostics.append(f"skills/list 读取失败：{str(exc)[:160]}")

        operations = {
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
                    f"mcpServerStatus/list 读取失败：{str(exc)[:160]}")

        revision = self._capability_revisions.get(session.agent_session_id, 0) + 1
        self._capability_revisions[session.agent_session_id] = revision
        return base.model_copy(update={
            "revision": revision,
            "items": items,
            "diagnostics": diagnostics,
        })

    async def _teardown(self, session: AgentSessionRef) -> str:
        ctx = self._runs.pop(session.agent_session_id, None)
        self._capability_revisions.pop(session.agent_session_id, None)
        if not ctx:
            return EXIT_CLOSED
        conn: CodexPeer = ctx["conn"]
        had_turn = ctx.get("current_turn_id") is not None
        returncode = await conn.close()
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
