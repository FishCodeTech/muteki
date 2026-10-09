"""Conversation API（CONV-01，任务书 9.2）：FastAPI router 工厂。

端点（任务书 9.2 全量）::

    GET  /api/projects
    POST /api/projects
    GET  /api/projects/{project_id}/git
    POST /api/projects/{project_id}/git/checkout
    GET  /api/threads
    POST /api/threads
    GET  /api/threads/inbox/events            （SSE：跨 Thread 注意力摘要 + after）
    GET  /api/threads/{thread_id}
    GET  /api/threads/{thread_id}/events      （SSE：snapshot + watermark + after）
    POST /api/threads/{thread_id}/commands    （conversation.* 命令统一入口）
    POST /api/threads/{thread_id}/uploads     （multipart 上传 → artifact.attach）

约束：

- 所有状态修改经 ``command_api.dispatch``（产生 CommandReceipt），查询经
  ``command_api.query``；本模块不直接调 ConversationManager / 写 Store；
- SSE 用 snapshot + watermark + ``after`` sequence 恢复，事件来自
  PlatformStore 的 Thread 聚合流（Public Event 白名单字段），不直接读取
  Adapter 私有事件；
- Agent Runtime 端点由 ``agent_runtime_api`` router 提供。
"""

from __future__ import annotations

import asyncio
import base64
import json
import os
import sqlite3
from pathlib import Path
import shutil
import subprocess
import sys
from typing import Any

from fastapi import HTTPException, APIRouter, Body, Form, Request, UploadFile, WebSocket, WebSocketDisconnect
from fastapi.responses import JSONResponse, Response, StreamingResponse
from pydantic import BaseModel, Field
from apps.web.ui_preferences import UiPreferencesWrite

from muteki.platform.command_handlers.base import CommandAPIError
from muteki.platform.contracts.commands import (
    ActorRef,
    CommandEnvelope,
    QueryEnvelope,
)
from muteki.platform.contracts.errors import ErrorCategory, ErrorEnvelope
from muteki.platform.contracts.receipts import CommandReceipt, ReceiptState

from muteki.conversation import events as conv_events
from muteki.conversation.composer_capabilities import (
    SUPPORTED_ENGINES,
    resolve_composer_catalog,
)
from muteki.conversation.inbox import InboxBroker, collect_attention_rows
from muteki.conversation.manager import (
    INTERACTION_MODE_INVALID_CODE,
    THREAD_NOT_FOUND_CODE,
    TURN_NOT_FOUND_CODE,
    ConversationError,
    InteractionModeError,
)
from muteki.conversation.module import CONVERSATION_PREFS_KEY, ConversationService
from muteki.external_agents.descriptors import all_descriptors, find_descriptor
from muteki.external_agents.factory import engine_for_adapter

#: Web 传输入口的本地操作员身份（单操作员产品默认，与 RUNTIME-05 一致）。
OPERATOR = ActorRef(kind="operator", id="local-user")


def _github_pr_api_token() -> str:
    """Use an explicit token, then the local operator's GitHub CLI login."""
    for name in ("MUTEKI_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"):
        token = os.environ.get(name, "").strip()
        if token:
            return token
    if not shutil.which("gh"):
        return ""
    try:
        result = subprocess.run(
            ["gh", "auth", "token", "--hostname", "github.com"],
            capture_output=True,
            text=True,
            timeout=5,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        return ""
    return result.stdout.strip() if result.returncode == 0 else ""


class GitHubAPIError(RuntimeError):
    def __init__(self, status: int, message: str) -> None:
        super().__init__(message)
        self.status = status


def _github_api_json(
    method: str,
    url: str,
    token: str,
    payload: dict[str, Any] | None = None,
    *,
    timeout: float = 15,
) -> Any:
    """Call the GitHub REST API; HTTP failures carry GitHub's own message."""
    import urllib.error
    import urllib.request

    headers = {
        "Accept": "application/vnd.github+json",
        "User-Agent": "muteki/c15",
        "X-GitHub-Api-Version": "2022-11-28",
    }
    if token:
        headers["Authorization"] = f"Bearer {token}"
    data = None
    if payload is not None:
        data = json.dumps(payload).encode()
        headers["Content-Type"] = "application/json"
    request = urllib.request.Request(url, data=data, headers=headers, method=method)
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            return json.loads(response.read().decode() or "null")
    except urllib.error.HTTPError as exc:
        raw = exc.read().decode(errors="replace")
        try:
            parsed = json.loads(raw)
        except ValueError:
            parsed = None
        parts: list[str] = []
        if isinstance(parsed, dict):
            if parsed.get("message"):
                parts.append(str(parsed["message"]))
            for item in parsed.get("errors") or []:
                if isinstance(item, dict):
                    detail = item.get("message") or " ".join(
                        str(item[key]) for key in ("resource", "field", "code") if item.get(key)
                    )
                    if detail:
                        parts.append(str(detail))
                elif item:
                    parts.append(str(item))
        elif raw.strip():
            parts.append(raw.strip())
        message = "; ".join(parts) or (exc.reason and str(exc.reason)) or ""
        raise GitHubAPIError(exc.code, f"GitHub API HTTP {exc.code}: {message}".rstrip(": ")) from exc

#: /commands 入口允许的命令命名空间（其余命令走各自领域 API）。
ALLOWED_COMMAND_PREFIX = "conversation."

#: SSE 轮询间隔与单页大小。
SSE_POLL_SECONDS = 0.5
SSE_PAGE_LIMIT = 200


async def _select_native_directory() -> dict[str, Any]:
    """打开宿主系统的目录选择器，返回用户确认的本机目录。"""
    if sys.platform == "darwin":
        command = [
            "osascript",
            "-e",
            "try\n"
            "  tell application \"System Events\" to activate\n"
            "  set chosenFolder to choose folder with prompt \"选择工作目录\"\n"
            "  return POSIX path of chosenFolder\n"
            "on error number -128\n"
            "  return \"\"\n"
            "end try",
        ]
    elif sys.platform.startswith("win"):
        command = [
            "powershell",
            "-NoProfile",
            "-STA",
            "-Command",
            "Add-Type -AssemblyName System.Windows.Forms; "
            "$dialog = New-Object System.Windows.Forms.FolderBrowserDialog; "
            "$dialog.Description = '选择工作目录'; "
            "if ($dialog.ShowDialog() -eq 'OK') { $dialog.SelectedPath }",
        ]
    elif shutil.which("zenity"):
        command = [
            "zenity", "--file-selection", "--directory", "--title=选择工作目录",
        ]
    elif shutil.which("kdialog"):
        command = ["kdialog", "--getexistingdirectory", "."]
    else:
        raise RuntimeError("当前系统没有可用的目录选择器")

    process = await asyncio.create_subprocess_exec(
        *command,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
    )
    try:
        stdout, stderr = await asyncio.wait_for(process.communicate(), timeout=300)
    except TimeoutError:
        process.kill()
        await process.communicate()
        raise RuntimeError("目录选择器等待超时")

    selected = stdout.decode("utf-8", errors="replace").strip()
    if process.returncode not in (0, 1) and not selected:
        detail = stderr.decode("utf-8", errors="replace").strip()
        raise RuntimeError(detail or "目录选择器启动失败")
    if not selected:
        return {"cancelled": True}

    path = Path(selected).expanduser().resolve()
    if not path.is_dir():
        raise RuntimeError("选择的工作目录已经不存在")
    return {
        "cancelled": False,
        "path": str(path),
        "name": path.name or str(path),
    }


class MemoryRecordBody(BaseModel):
    """用户明确允许后写入 Thread 长期记忆。"""

    content: str = Field(min_length=1, max_length=20_000)
    kind: str = Field(default="note", max_length=80)
    consent: bool
    command_id: str = ""
    idempotency_key: str = ""


class MemoryDeleteBody(BaseModel):
    """删除长期记忆的显式确认。"""

    confirm: bool
    reason: str = Field(default="user_requested", max_length=240)
    command_id: str = ""
    idempotency_key: str = ""


class ComposerCapabilitiesBody(BaseModel):
    """当前输入框、Agent 与项目共同决定的候选能力。"""

    thread_id: str = ""
    adapter_id: str = Field(default="", max_length=120)
    instance_id: str = Field(default="", max_length=120)
    workspace_id: str = Field(default="", max_length=160)
    project_id: str = Field(default="", max_length=160)
    trigger: str = Field(pattern=r"^[/@$]$")
    query: str = Field(default="", max_length=160)


class ProjectGitCheckoutBody(BaseModel):
    """在已登记项目工作目录内切换或创建 Git 分支。"""

    branch: str = Field(min_length=1, max_length=200)
    create: bool = False


class ThreadGitCheckoutBody(BaseModel):
    """在 Thread 绑定的工作区根目录内切换或创建 Git 分支。"""

    branch: str = Field(min_length=1, max_length=200)
    create: bool = False


class ThreadGitCommitBody(BaseModel):
    """暂存并提交 Thread 工作区改动；paths 为空时提交全部改动。"""

    message: str = Field(default="", max_length=20_000)
    paths: list[str] | None = Field(default=None, max_length=2_000)
    amend: bool = False


class ThreadGitPushBody(BaseModel):
    """None 表示仅在分支没有上游时自动设置上游。"""

    set_upstream: bool | None = None


class ThreadGitPullBody(BaseModel):
    rebase: bool = False


class ThreadPullRequestCreateBody(BaseModel):
    title: str = Field(default="", max_length=1_000)
    body: str = Field(default="", max_length=65_000)
    base: str = Field(default="", max_length=255)
    draft: bool = False
    push_first: bool = False


class WorkspaceDeleteWorktreeBody(BaseModel):
    workspace_id: str = Field(min_length=1, max_length=160)
    force: bool = False
    command_id: str = Field(default="", max_length=160)
    idempotency_key: str = Field(default="", max_length=200)


class WorkspaceCommandBody(BaseModel):
    command: str = Field(min_length=1, max_length=8_000)


class TerminalSessionCreateBody(BaseModel):
    cols: int = Field(default=80, ge=20, le=500)
    rows: int = Field(default=24, ge=5, le=200)
    name: str = Field(default="", max_length=80)


def _receipt_status(receipt: CommandReceipt) -> int:
    """回执终态 → HTTP 状态码（与 RUNTIME-05 router 同口径）。"""
    if receipt.state is ReceiptState.COMPLETED:
        return 200
    if receipt.state is ReceiptState.ACCEPTED:
        return 202
    if receipt.state is ReceiptState.CONFLICT:
        return 409
    category = getattr(getattr(receipt, "error", None), "category", None)
    if category is ErrorCategory.NOT_FOUND:
        return 404
    if category is ErrorCategory.VALIDATION:
        return 400
    if category is ErrorCategory.PERMISSION:
        return 403
    if category is ErrorCategory.STATE:
        return 409
    return 502


def _receipt_body(receipt: CommandReceipt) -> dict[str, Any]:
    return {"receipt": receipt.model_dump(mode="json")}


def _error_body(exc: CommandAPIError) -> dict[str, Any]:
    return {"error": exc.error.model_dump(mode="json")}



def _composer_runtime_payload(
    *,
    adapter_id: str,
    runtime_snapshot: Any,
    runtime_diagnostics: list[str],
    matrix_payload: Any,
    executor: Any,
    thread_id: str,
) -> dict[str, Any]:
    """Build composer runtime block, including #188 refresh failure fields."""
    failure = None
    in_flight = False
    if thread_id and executor is not None:
        getter = getattr(executor, "capability_refresh_failure", None)
        if callable(getter):
            failure = getter(thread_id)
        inflight_getter = getattr(executor, "capability_refresh_in_flight", None)
        if callable(inflight_getter):
            in_flight = bool(inflight_getter(thread_id))
    if in_flight:
        refresh_status = "refreshing"
    elif isinstance(failure, dict) and failure:
        refresh_status = "failed"
    elif runtime_snapshot is None:
        refresh_status = "missing"
    elif bool(getattr(runtime_snapshot, "stale", False)):
        refresh_status = "stale"
    else:
        refresh_status = "fresh"
    return {
        "adapter_id": (
            runtime_snapshot.adapter_id
            if runtime_snapshot is not None else adapter_id
        ),
        "revision": (
            runtime_snapshot.revision
            if runtime_snapshot is not None else 0
        ),
        "stale": (
            runtime_snapshot.stale
            if runtime_snapshot is not None else True
        ),
        "diagnostics": [
            *runtime_diagnostics,
            *(runtime_snapshot.diagnostics
              if runtime_snapshot is not None else []),
        ],
        "matrix": matrix_payload,
        "refresh_status": refresh_status,
        "last_error": (
            str(failure.get("error") or "")
            if isinstance(failure, dict) and failure else ""
        ),
        "refresh_attempts": (
            int(failure.get("attempts") or 0)
            if isinstance(failure, dict) and failure else 0
        ),
        "retry_after_seconds": (
            float(failure.get("retry_after_seconds") or 0.0)
            if isinstance(failure, dict) and failure else 0.0
        ),
    }


def _session_import_source(engine: str) -> tuple[Any, Path] | None:
    """Scanner and default host directory for a provider session import."""
    from muteki.conversation.provider_import import scan_claude_sessions, scan_codex_sessions

    descriptor = find_descriptor(engine)
    if descriptor is None or descriptor.session_import == "none":
        return None
    scanner = {
        "claude_projects": scan_claude_sessions,
        "codex_sessions": scan_codex_sessions,
    }[descriptor.session_import]
    return scanner, Path.home() / descriptor.environment.home_relative


def _session_import_invalid_message() -> str:
    names = " 或 ".join(
        item.identity.display_name for item in all_descriptors()
        if item.session_import != "none"
    )
    return f"请选择 {names} 历史来源。"


def create_conversation_router(
    *,
    command_api: Any,
    service: ConversationService,
    actor: ActorRef = OPERATOR,
    register_handlers: bool = True,
    sse_metrics: Any = None,
    extension_service: Any = None,
    browser_control: Any = None,
) -> Any:
    """构造 Conversation API router。

    ``register_handlers`` 为 True 时把 CONV-01 Handler 注册到该 Command
    API（幂等）。
    """
    if register_handlers:
        service.register(command_api)
        # project.list / thread.list 只读查询（能力工具与 API 共用）；幂等守卫。
        from muteki.platform.capability_gateway import (
            ProjectListQueryHandler,
            ThreadListQueryHandler,
        )
        known_queries = command_api.handlers.known_query_types()
        if "project.list" not in known_queries:
            command_api.register_query(ProjectListQueryHandler())
        if "thread.list" not in known_queries:
            command_api.register_query(ThreadListQueryHandler())

    service.manager.extension_service = extension_service

    router = APIRouter(tags=["conversation"])
    store = service.platform
    inbox_brokers: dict[str, InboxBroker] = {}
    inbox_producer_locks: dict[str, asyncio.Lock] = {}

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

    def _query(query_type: str, aggregate_type: str = "",
               aggregate_id: str = "", **params: Any) -> QueryEnvelope:
        return QueryEnvelope(
            query_type=query_type,
            aggregate_type=aggregate_type or None,
            aggregate_id=aggregate_id or None,
            actor=actor,
            params={k: v for k, v in params.items() if v not in (None, "")},
        )

    async def _dispatch(envelope: CommandEnvelope) -> JSONResponse:
        receipt = await command_api.dispatch(envelope)
        return JSONResponse(
            _receipt_body(receipt), status_code=_receipt_status(receipt))

    def _error_response(
        code: str,
        message: str,
        category: ErrorCategory,
        *,
        status_code: int,
        recovery_hint: str = "",
        detail: dict[str, Any] | None = None,
    ) -> JSONResponse:
        error = ErrorEnvelope(
            code=code,
            message=message,
            category=category,
            recovery_hint=recovery_hint,
        )
        if detail:
            error = error.model_copy(update={"detail": dict(detail)})
        return JSONResponse(
            {"error": error.model_dump(mode="json")},
            status_code=status_code,
        )

    # -- Project ---------------------------------------------------------------

    @router.post("/api/conversation/composer-capabilities")
    async def composer_capabilities(body: ComposerCapabilitiesBody) -> Any:
        adapter_id = body.adapter_id.strip()
        workspace_id = body.workspace_id.strip()
        runtime_snapshot = None
        runtime_diagnostics: list[str] = []
        if body.thread_id:
            thread = service.manager.get_thread(body.thread_id)
            if thread is None:
                return _error_response(
                    "conversation.thread.not_found",
                    "当前对话不存在",
                    ErrorCategory.NOT_FOUND,
                    status_code=404,
                )
            selection = service.manager.runtime_selection(thread.thread_id)
            adapter_id = adapter_id or selection.adapter_id
            workspace_id = str(thread.workspace_id or "")
            if adapter_id == selection.adapter_id:
                runtime_snapshot = service.executor.cached_runtime_capabilities(
                    thread.thread_id)
                sel_iid = selection.instance_id or "default"
                if runtime_snapshot is not None and (
                    runtime_snapshot.adapter_id != adapter_id
                    or (runtime_snapshot.instance_id or "default") != sel_iid
                ):
                    # Thread cache may still hold the previous Provider.
                    runtime_snapshot = None
                failure = service.executor.capability_refresh_failure(
                    thread.thread_id)
                if runtime_snapshot is None or runtime_snapshot.stale:
                    started = service.executor.refresh_runtime_capabilities(
                        thread.thread_id)
                    if started:
                        runtime_diagnostics.append(
                            "Runtime 动态能力正在后台刷新；"
                            "当前先显示已确认的插件与 Skill"
                        )
                    elif failure is not None:
                        runtime_diagnostics.append(
                            str(
                                failure.get("error")
                                or "Runtime 能力刷新失败"
                            )
                        )
                    elif runtime_snapshot is None:
                        runtime_diagnostics.append(
                            "会话能力目录尚未加载"
                        )
            else:
                runtime_diagnostics.append(
                    "当前选择与活动 Session 不同；发送后会建立新 Session，"
                    "在此之前不展示未经该 Runtime 确认的命令"
                )

        workspace = service.manager.get_workspace(workspace_id) if workspace_id else None
        if workspace_id and workspace is None:
            return _error_response(
                "conversation.workspace.not_found",
                "当前项目工作目录不存在",
                ErrorCategory.NOT_FOUND,
                status_code=404,
            )
        if body.project_id and workspace is not None and workspace.project_id != body.project_id:
            return _error_response(
                "conversation.workspace.project_mismatch",
                "当前项目与工作目录不匹配",
                ErrorCategory.VALIDATION,
                status_code=400,
            )

        engine = engine_for_adapter(adapter_id)
        if engine not in SUPPORTED_ENGINES:
            return _error_response(
                "conversation.composer.agent_unsupported",
                "请选择可用的 Agent 后再浏览能力",
                ErrorCategory.VALIDATION,
                status_code=400,
            )
        section_errors: list[dict[str, Any]] = []
        try:
            candidate_threads = service.manager.list_threads(body.project_id)
        except Exception as exc:
            candidate_threads = []
            section_errors.append({"section": "threads", "code": "conversation.composer.section_failed",
                "message": str(exc), "exception_type": type(exc).__name__})
        items = resolve_composer_catalog(
            engine=engine,
            workspace_root=str(workspace.root_path if workspace is not None else ""),
            trigger=body.trigger,
            query=body.query,
            extension_service=extension_service,
            threads=candidate_threads,
            current_thread_id=body.thread_id,
            runtime_snapshot=runtime_snapshot,
            plugin_service=(getattr(service.manager, "chat_plugins", None)
                            if not body.thread_id or thread.mode == "conversation" else None),
            section_errors=section_errors,
        )
        matrix_payload = None
        try:
            if body.thread_id:
                # Matrix identity must follow the requested Runtime, not a
                # stale thread-scoped snapshot from another Provider/instance.
                sel = service.manager.runtime_selection(body.thread_id)
                matrix_iid = (
                    (sel.instance_id or "default")
                    if adapter_id == (sel.adapter_id or "")
                    else "default"
                )
                matrix_payload = service.executor.interaction_matrix_for(
                    body.thread_id,
                    adapter_id=adapter_id,
                    instance_id=matrix_iid,
                )
            elif runtime_snapshot is not None and runtime_snapshot.matrix is not None:
                matrix_payload = runtime_snapshot.public_matrix()
        except Exception as exc:
            section_errors.append({"section": "runtime", "code": "conversation.composer.section_failed",
                "message": str(exc), "exception_type": type(exc).__name__})
            runtime_diagnostics.append(f"Runtime capability matrix failed: {type(exc).__name__}: {exc}")
        return {
            "engine": engine,
            "trigger": body.trigger,
            **service.manager.access_mode_availability(
                adapter_id,
                body.instance_id or ((selection.instance_id or "default")
                                     if body.thread_id and adapter_id == selection.adapter_id else "default"),
                str(workspace.root_path) if workspace is not None else "",
            ),
            "items": items,
            "count": len(items),
            "partial": bool(section_errors),
            "section_errors": section_errors,
            "runtime": _composer_runtime_payload(
                adapter_id=adapter_id,
                runtime_snapshot=runtime_snapshot,
                runtime_diagnostics=runtime_diagnostics,
                matrix_payload=matrix_payload,
                executor=service.executor,
                thread_id=body.thread_id,
            ),
        }

    @router.get("/api/projects")
    async def list_projects() -> Any:
        result = await command_api.query(_query(
            "conversation.project.directory_list"))
        return result.result or {"projects": [], "count": 0}

    @router.post("/api/projects")
    async def create_project(body: dict[str, Any] = Body(...)) -> Any:
        command_id = str(body.pop("command_id", "") or "")
        idem = str(body.pop("idempotency_key", "") or "")
        return await _dispatch(_command(
            "conversation.project.create", "project",
            str(body.get("project_id") or ""), dict(body),
            command_id=command_id, idempotency_key=idem))

    @router.patch("/api/projects/{project_id}")
    async def update_project(project_id: str, body: dict[str, Any] = Body(...)) -> Any:
        command_id = str(body.pop("command_id", "") or "")
        idem = str(body.pop("idempotency_key", "") or "")
        payload = {"project_id": project_id, **body}
        return await _dispatch(_command(
            "conversation.project.update", "project",
            project_id, payload,
            command_id=command_id, idempotency_key=idem))

    @router.post("/api/directories/select")
    async def select_directory() -> Any:
        try:
            return await _select_native_directory()
        except RuntimeError as exc:
            return _error_response(
                "conversation.directory_picker.unavailable",
                str(exc),
                ErrorCategory.STATE,
                status_code=503,
                recovery_hint="请确认宿主桌面会话可用后重试",
            )

    def _project_root_path(project_id: str) -> str | None:
        """解析项目主检出路径（忽略 Thread 派生 worktree）。"""
        project_id = str(project_id or "").strip()
        if not project_id or service.manager.get_project(project_id) is None:
            return None
        return service.manager.primary_project_root(project_id)

    def _occupancy_payload(root: str, *, exclude_thread_id: str = "") -> dict[str, Any]:
        occupants = service.manager.active_threads_for_workspace_root(
            root, exclude_thread_id=exclude_thread_id,
        )
        return {
            "occupied": bool(occupants),
            "occupant_count": len(occupants),
            "occupant_thread_ids": [item.thread_id for item in occupants],
        }

    @router.get("/api/projects/{project_id}/git")
    async def project_git_status(project_id: str) -> Any:
        """读取项目主检出的当前分支与本地分支列表。"""
        from muteki.conversation.git_workspace import GitWorkspaceError, inspect_git_workspace

        root = _project_root_path(project_id)
        if root is None:
            return _error_response(
                "conversation.project.not_found",
                "项目不存在或尚未绑定工作目录",
                ErrorCategory.NOT_FOUND,
                status_code=404,
            )
        try:
            status = await asyncio.to_thread(inspect_git_workspace, root)
        except GitWorkspaceError as exc:
            return _error_response(
                "conversation.git.unavailable",
                str(exc),
                ErrorCategory.STATE,
                status_code=503,
            )
        occupancy = _occupancy_payload(root)
        return {"project_id": project_id, **status, **occupancy}

    @router.get("/api/projects/{project_id}/git/worktrees")
    async def project_git_worktrees(project_id: str) -> Any:
        """列出项目仓库下的 Git worktree（含主检出）。"""
        from muteki.conversation.git_workspace import GitWorkspaceError, list_worktrees

        root = _project_root_path(project_id)
        if root is None:
            return _error_response(
                "conversation.project.not_found",
                "项目不存在或尚未绑定工作目录",
                ErrorCategory.NOT_FOUND,
                status_code=404,
            )
        try:
            rows = await asyncio.to_thread(list_worktrees, root)
        except GitWorkspaceError as exc:
            return _error_response(
                "conversation.git.unavailable",
                str(exc),
                ErrorCategory.STATE,
                status_code=503,
            )
        enriched = []
        for row in rows:
            path = str(row.get("path") or "")
            occupancy = _occupancy_payload(path) if path else {
                "occupied": False, "occupant_count": 0, "occupant_thread_ids": [],
            }
            enriched.append({**row, **occupancy})
        return {"project_id": project_id, "root_path": root, "worktrees": enriched}

    @router.post("/api/projects/{project_id}/git/checkout")
    async def project_git_checkout(
        project_id: str,
        body: ProjectGitCheckoutBody,
    ) -> Any:
        """在项目主检出切换或创建分支；多 Thread 共享时拒绝以免交叉覆盖。"""
        from muteki.conversation.git_workspace import GitWorkspaceError, checkout_git_branch

        root = _project_root_path(project_id)
        if root is None:
            return _error_response(
                "conversation.project.not_found",
                "项目不存在或尚未绑定工作目录",
                ErrorCategory.NOT_FOUND,
                status_code=404,
            )
        occupancy = _occupancy_payload(root)
        if occupancy["occupant_count"] > 1:
            return _error_response(
                "conversation.git.root_occupied",
                "多个活动会话共用该检出，请改用各会话自己的分支控制或新建 worktree",
                ErrorCategory.CONFLICT,
                status_code=409,
                recovery_hint="为并行会话选择「新建 worktree」",
            )
        try:
            status = await asyncio.to_thread(
                checkout_git_branch,
                root,
                branch=body.branch,
                create=body.create,
            )
        except GitWorkspaceError as exc:
            return _error_response(
                "conversation.git.checkout_failed",
                str(exc),
                ErrorCategory.VALIDATION,
                status_code=400,
            )
        return {"project_id": project_id, **status, **_occupancy_payload(root)}

    @router.post("/api/workspaces")
    async def bind_workspace(body: dict[str, Any] = Body(...)) -> Any:
        command_id = str(body.pop("command_id", "") or "")
        idem = str(body.pop("idempotency_key", "") or "")
        return await _dispatch(_command(
            "conversation.workspace.bind", "workspace",
            str(body.get("workspace_id") or ""), dict(body),
            command_id=command_id, idempotency_key=idem))

    @router.post("/api/workspaces/delete-worktree")
    async def delete_worktree(body: WorkspaceDeleteWorktreeBody) -> Any:
        """显式删除 worktree（归档 Thread 不会调用）。"""
        return await _dispatch(_command(
            "conversation.workspace.delete_worktree",
            "workspace",
            body.workspace_id,
            {"workspace_id": body.workspace_id, "force": body.force},
            command_id=body.command_id,
            idempotency_key=body.idempotency_key,
        ))

    # -- Thread ------------------------------------------------------------------

    @router.get("/api/threads")
    async def list_threads(project_id: str = "") -> Any:
        result = await command_api.query(_query(
            "conversation.thread.list", project_id=project_id))
        return result.result

    @router.post("/api/threads")
    async def create_thread(body: dict[str, Any] = Body(...)) -> Any:
        command_id = str(body.pop("command_id", "") or "")
        idem = str(body.pop("idempotency_key", "") or "")
        return await _dispatch(_command(
            "conversation.thread.create", "thread",
            str(body.get("thread_id") or ""), dict(body),
            command_id=command_id, idempotency_key=idem))

    @router.get("/api/threads/inbox/events")
    async def inbox_events(
        request: Request,
        after: int = 0,
        snapshot: bool = True,
        project_id: str = "",
        broker_epoch: str = "",
    ) -> Any:
        """Cross-thread attention inbox (SSE).

        First frame ``event: snapshot`` carries attention summaries + list
        rows and the current inbox cursor. Subsequent ``event: event`` frames
        are ``attention.updated`` / ``attention.cleared`` with monotonic
        ``seq`` for ``after`` / Last-Event-ID resume (same habit as C05).
        """
        inbox_broker = inbox_brokers.setdefault(project_id, InboxBroker())
        producer_lock = inbox_producer_locks.setdefault(project_id, asyncio.Lock())
        last_event_id = request.headers.get("last-event-id", "").strip()
        try:
            resume_after = int(last_event_id) if last_event_id else int(after)
        except ValueError:
            return _error_response(
                "conversation.inbox.cursor_invalid",
                "after / Last-Event-ID 必须是整数",
                ErrorCategory.VALIDATION,
                status_code=400,
                recovery_hint="使用上一条已确认 inbox 事件的整数 seq 重新连接",
            )

        async def _stream_body():
            def _frame(event_name: str, data: Any, event_id: int = 0) -> bytes:
                id_line = f"id: {event_id}\n" if event_id > 0 else ""
                return (
                    id_line
                    + f"event: {event_name}\n"
                    f"data: {json.dumps(data, ensure_ascii=False, default=str)}"
                    f"\n\n"
                ).encode("utf-8")

            async with producer_lock:
                rows = await asyncio.to_thread(
                    collect_attention_rows, service.manager, project_id)
                if snapshot or inbox_broker.seq == 0:
                    inbox_broker.seed_rows(rows)
                snapshot_cursor = inbox_broker.seq

            cursor = max(0, int(after) if snapshot else resume_after)
            reset = bool(broker_epoch and broker_epoch != inbox_broker.epoch) or not inbox_broker.covers_cursor(cursor)

            if snapshot or reset:
                cursor = snapshot_cursor
                yield _frame(
                    "snapshot",
                    {
                        "inbox_seq": snapshot_cursor,
                        "broker_epoch": inbox_broker.epoch,
                        "cursor_reset": reset,
                        "summaries": [row["summary"] for row in rows],
                        "threads": [row["thread"] for row in rows],
                        "count": len(rows),
                    },
                )
            else:
                for event in inbox_broker.events_after(cursor):
                    cursor = max(cursor, event.seq)
                    yield _frame("event", {**event.as_payload(), "broker_epoch": inbox_broker.epoch}, event.seq)

            while True:
                if await request.is_disconnected():
                    return
                async with producer_lock:
                    rows = await asyncio.to_thread(
                        collect_attention_rows, service.manager, project_id)
                    inbox_broker.sync_rows(rows)
                    pending_events = inbox_broker.events_after(cursor)
                    needs_snapshot = not inbox_broker.covers_cursor(cursor)
                    snapshot_cursor = inbox_broker.seq
                if needs_snapshot:
                    cursor = snapshot_cursor
                    yield _frame("snapshot", {"inbox_seq": snapshot_cursor,
                        "broker_epoch": inbox_broker.epoch, "cursor_reset": True,
                        "summaries": [row["summary"] for row in rows],
                        "threads": [row["thread"] for row in rows], "count": len(rows)})
                    continue
                sent = False
                for event in pending_events:
                    if event.seq <= cursor:
                        continue
                    cursor = max(cursor, event.seq)
                    yield _frame("event", {**event.as_payload(), "broker_epoch": inbox_broker.epoch}, event.seq)
                    sent = True
                if not sent:
                    yield b": heartbeat\n\n"
                    await asyncio.sleep(SSE_POLL_SECONDS)

        async def _stream():
            if sse_metrics is not None:
                sse_metrics.open_sse("conversation_inbox", resumed=resume_after > 0)
            try:
                async for chunk in _stream_body():
                    yield chunk
            finally:
                if sse_metrics is not None:
                    sse_metrics.close_sse("conversation_inbox")

        return StreamingResponse(
            _stream(),
            media_type="text/event-stream",
            headers={"Cache-Control": "no-cache",
                     "X-Accel-Buffering": "no"},
        )

    @router.get("/api/threads/search")
    async def search_threads(
        q: str = "",
        project_id: str = "",
        include_archived: bool = False,
        include_superseded: bool = False,
        limit: int = 30,
        offset: int = 0,
    ) -> Any:
        try:
            result = await command_api.query(_query(
                "conversation.thread.search", "thread", "",
                q=q,
                project_id=project_id,
                include_archived=include_archived,
                include_superseded=include_superseded,
                limit=limit,
                offset=offset,
            ))
        except CommandAPIError as exc:
            status = 404 if exc.error.category is ErrorCategory.NOT_FOUND else 400
            return JSONResponse(_error_body(exc), status_code=status)
        return result.result

    @router.get("/api/threads/{thread_id}")
    async def get_thread(
        thread_id: str,
        mark_read: bool = True,
        messages_limit: str = "50",
        before_stream_seq: int | None = None,
        after_stream_seq: int | None = None,
        include_events: bool = False,
        include_superseded: bool = False,
    ) -> Any:
        try:
            result = await command_api.query(_query(
                "conversation.thread.view", "thread", thread_id,
                thread_id=thread_id,
                mark_read=mark_read,
                messages_limit=messages_limit,
                before_stream_seq=before_stream_seq,
                after_stream_seq=after_stream_seq,
                include_events=include_events,
                include_superseded=include_superseded,
            ))
        except CommandAPIError as exc:
            status = 404 if exc.error.category is ErrorCategory.NOT_FOUND else 400
            return JSONResponse(_error_body(exc), status_code=status)
        view = result.result
        # Subagent Threads surface a lineage banner; resolve the parent title
        # here so the UI does not need a second lookup.
        lineage = (view.get("state") or {}).get("lineage")
        if lineage:
            parent = service.manager.get_thread(str(lineage.get("parent_thread_id") or ""))
            view["lineage"] = {
                **lineage,
                "parent_title": (parent.title if parent is not None else "") or None,
            }
        return view

    @router.get("/api/threads/{thread_id}/impact-preview")
    async def impact_preview(
        thread_id: str,
        turn_id: str,
        mode: str = "retry",
        text: str = "",
        file_mode: str = "keep_files",
    ) -> Any:
        try:
            return service.manager.preview_turn_impact(
                thread_id,
                turn_id,
                mode=mode,
                text=text,
                file_mode=file_mode,
            )
        except ConversationError as exc:
            not_found = exc.code in {THREAD_NOT_FOUND_CODE, TURN_NOT_FOUND_CODE}
            return _error_response(
                exc.code if not_found else "conversation.impact.preview_failed",
                str(exc),
                ErrorCategory.NOT_FOUND if not_found else ErrorCategory.VALIDATION,
                status_code=404 if not_found else 400,
            )

    @router.get("/api/threads/{thread_id}/messages")
    async def get_thread_messages(
        thread_id: str,
        limit: str = "50",
        before_stream_seq: int | None = None,
        after_stream_seq: int | None = None,
        around_message_id: str | None = None,
    ) -> Any:
        try:
            result = await command_api.query(_query(
                "conversation.thread.messages", "thread", thread_id,
                thread_id=thread_id,
                limit=limit,
                before_stream_seq=before_stream_seq,
                after_stream_seq=after_stream_seq,
                around_message_id=around_message_id,
            ))
        except CommandAPIError as exc:
            status = 404 if exc.error.category is ErrorCategory.NOT_FOUND else 400
            return JSONResponse(_error_body(exc), status_code=status)
        return result.result

    @router.get("/api/threads/{thread_id}/turns/{turn_id}/process")
    async def get_turn_process(
        thread_id: str,
        turn_id: str,
        limit: int = 2000,
        after_seq: int = 0,
        watermark: int | None = None,
    ) -> Any:
        try:
            result = await command_api.query(_query(
                "conversation.turn.process", "thread", thread_id,
                thread_id=thread_id,
                turn_id=turn_id,
                limit=limit,
                after_seq=after_seq,
                watermark=watermark,
            ))
        except CommandAPIError as exc:
            status = 404 if exc.error.category is ErrorCategory.NOT_FOUND else 400
            return JSONResponse(_error_body(exc), status_code=status)
        return result.result

    def _thread_workspace(thread_id: str):
        thread = service.manager.get_thread(thread_id)
        if thread is None:
            return None, None
        workspace = (
            service.manager.get_workspace(str(thread.workspace_id or ""))
            if thread.workspace_id else None
        )
        return thread, workspace

    def _thread_workspace_root(thread_id: str) -> str | None:
        _thread, workspace = _thread_workspace(thread_id)
        root = str(workspace.root_path or "").strip() if workspace is not None else ""
        return root or None

    @router.get("/api/threads/{thread_id}/git")
    async def thread_git_status(thread_id: str) -> Any:
        """读取 Thread 实际绑定工作区的分支/脏状态（非项目主检出）。"""
        from muteki.conversation.git_workspace import GitWorkspaceError, inspect_git_workspace

        thread, workspace = _thread_workspace(thread_id)
        if thread is None:
            return _error_response(
                "conversation.thread.not_found",
                f"unknown thread: {thread_id}",
                ErrorCategory.NOT_FOUND,
                status_code=404,
            )
        root = str(workspace.root_path or "").strip() if workspace is not None else ""
        if not root:
            return _error_response(
                "conversation.workspace.not_found",
                "会话尚未绑定工作目录",
                ErrorCategory.NOT_FOUND,
                status_code=404,
            )
        try:
            status = await asyncio.to_thread(inspect_git_workspace, root)
        except GitWorkspaceError as exc:
            return _error_response(
                "conversation.git.unavailable",
                str(exc),
                ErrorCategory.STATE,
                status_code=503,
            )
        settings = dict(workspace.settings or {}) if workspace is not None else {}
        occupancy = _occupancy_payload(root, exclude_thread_id=thread_id)
        return {
            "thread_id": thread_id,
            "workspace_id": workspace.workspace_id if workspace else None,
            "mode": str(settings.get("mode") or ""),
            "configured_branch": str(settings.get("branch") or "") or None,
            "workspace_kind": workspace.kind if workspace else None,
            **status,
            **occupancy,
            "branch_switch_blocked": occupancy["occupied"],
        }

    @router.get("/api/threads/{thread_id}/pull-requests")
    async def thread_pull_requests(thread_id: str) -> Any:
        """C15: Detect GitHub PRs for the thread workspace's current branch.

        Returns ``{state, branch, remote_url, owner, repo, pull_requests[]}``.

        ``state`` values:
          no_workspace   — thread has no bound workspace
          no_git         — workspace is not a git repo
          no_remote      — no remote named "origin"
          not_github     — remote exists but is not GitHub
          error          — GitHub API call failed
          no_pr          — branch has no associated PRs (open or closed)
          ok             — pull_requests contains ≥1 result
        """
        import urllib.error
        import urllib.request
        from muteki.conversation.git_workspace import (
            GitWorkspaceError,
            detect_github_remote,
            inspect_git_workspace,
        )

        thread, workspace = _thread_workspace(thread_id)
        if thread is None:
            return _error_response(
                "conversation.thread.not_found",
                f"unknown thread: {thread_id}",
                ErrorCategory.NOT_FOUND,
                status_code=404,
            )
        root = str(workspace.root_path or "").strip() if workspace is not None else ""
        if not root:
            return {"state": "no_workspace", "branch": None, "remote_url": "", "owner": "", "repo": "", "pull_requests": []}

        # Get current branch
        try:
            git_info = await asyncio.to_thread(inspect_git_workspace, root)
        except GitWorkspaceError:
            return {"state": "no_git", "branch": None, "remote_url": "", "owner": "", "repo": "", "pull_requests": []}
        branch = git_info.get("current_branch") or ""

        # Detect GitHub remote
        try:
            remote = await asyncio.to_thread(detect_github_remote, root)
        except GitWorkspaceError:
            return {"state": "no_git", "branch": branch, "remote_url": "", "owner": "", "repo": "", "pull_requests": []}

        if not remote["remote_url"]:
            return {"state": "no_remote", "branch": branch, "remote_url": "", "owner": "", "repo": "", "pull_requests": []}
        if not remote["github"]:
            return {"state": "not_github", "branch": branch, "remote_url": remote["remote_url"], "owner": "", "repo": "", "pull_requests": []}

        owner, repo = remote["owner"], remote["repo"]

        # Private repositories require the operator's API credential. Never
        # include it in the response or diagnostic text.
        token = await asyncio.to_thread(_github_pr_api_token)
        headers: dict[str, str] = {"Accept": "application/vnd.github.v3+json", "User-Agent": "muteki/c15"}
        if token:
            headers["Authorization"] = f"Bearer {token}"

        pulls_url = (
            f"https://api.github.com/repos/{owner}/{repo}/pulls"
            f"?state=all&head={owner}:{branch}&per_page=10"
            if branch
            else f"https://api.github.com/repos/{owner}/{repo}/pulls?state=open&per_page=10"
        )

        def _fetch_pulls() -> list[dict[str, Any]]:
            req = urllib.request.Request(pulls_url, headers=headers)
            with urllib.request.urlopen(req, timeout=8) as resp:
                return json.loads(resp.read().decode())

        try:
            raw_pulls = await asyncio.to_thread(_fetch_pulls)
        except urllib.error.HTTPError as exc:
            detail = f"GitHub API HTTP {exc.code}"
            return {"state": "error", "branch": branch, "remote_url": remote["remote_url"],
                    "owner": owner, "repo": repo, "error": detail, "pull_requests": []}
        except Exception as exc:
            return {"state": "error", "branch": branch, "remote_url": remote["remote_url"],
                    "owner": owner, "repo": repo, "error": str(exc), "pull_requests": []}

        pull_requests = [
            {
                "number": pr.get("number"),
                "title": pr.get("title", ""),
                "state": (
                    "merged" if pr.get("merged_at")
                    else "draft" if pr.get("draft")
                    else pr.get("state", "")
                ),
                "html_url": pr.get("html_url", ""),
                "user_login": (pr.get("user") or {}).get("login", ""),
                "created_at": pr.get("created_at", ""),
                "updated_at": pr.get("updated_at", ""),
                "base_ref": (pr.get("base") or {}).get("ref", ""),
                "head_ref": (pr.get("head") or {}).get("ref", ""),
                "mergeable_state": pr.get("mergeable_state"),
            }
            for pr in raw_pulls
            if isinstance(pr, dict)
        ]

        state = "ok" if pull_requests else "no_pr"
        return {
            "state": state,
            "branch": branch,
            "remote_url": remote["remote_url"],
            "owner": owner,
            "repo": repo,
            "pull_requests": pull_requests,
        }

    @router.post("/api/threads/{thread_id}/git/checkout")
    async def thread_git_checkout(
        thread_id: str,
        body: ThreadGitCheckoutBody,
    ) -> Any:
        """在 Thread 工作区根切换分支；其他活动会话占用同一根时拒绝。"""
        from muteki.conversation.git_workspace import GitWorkspaceError, checkout_git_branch

        thread, workspace = _thread_workspace(thread_id)
        if thread is None:
            return _error_response(
                "conversation.thread.not_found",
                f"unknown thread: {thread_id}",
                ErrorCategory.NOT_FOUND,
                status_code=404,
            )
        root = str(workspace.root_path or "").strip() if workspace is not None else ""
        if not root:
            return _error_response(
                "conversation.workspace.not_found",
                "会话尚未绑定工作目录",
                ErrorCategory.NOT_FOUND,
                status_code=404,
            )
        occupancy = _occupancy_payload(root, exclude_thread_id=thread_id)
        if occupancy["occupied"]:
            return _error_response(
                "conversation.git.root_occupied",
                "工作区正被其他活动会话使用，拒绝切换分支以免覆盖对方文件",
                ErrorCategory.CONFLICT,
                status_code=409,
                recovery_hint="为当前会话新建独立 worktree",
            )
        try:
            status = await asyncio.to_thread(
                checkout_git_branch,
                root,
                branch=body.branch,
                create=body.create,
            )
        except GitWorkspaceError as exc:
            return _error_response(
                "conversation.git.checkout_failed",
                str(exc),
                ErrorCategory.VALIDATION,
                status_code=400,
            )
        return {
            "thread_id": thread_id,
            "workspace_id": workspace.workspace_id if workspace else None,
            **status,
            **_occupancy_payload(root, exclude_thread_id=thread_id),
        }

    _GIT_WRITE_ERRORS: dict[str, tuple[ErrorCategory, int]] = {
        "conversation.git.empty_message": (ErrorCategory.VALIDATION, 400),
        "conversation.git.invalid_path": (ErrorCategory.VALIDATION, 400),
        "conversation.git.path_outside": (ErrorCategory.VALIDATION, 400),
        "conversation.git.invalid_branch": (ErrorCategory.VALIDATION, 400),
        "conversation.git.stage_failed": (ErrorCategory.VALIDATION, 400),
        "conversation.git.commit_failed": (ErrorCategory.VALIDATION, 400),
        "conversation.git.not_repo": (ErrorCategory.STATE, 409),
        "conversation.git.detached_head": (ErrorCategory.STATE, 409),
        "conversation.git.no_remote": (ErrorCategory.STATE, 409),
        "conversation.git.no_upstream": (ErrorCategory.STATE, 409),
        "conversation.git.nothing_to_commit": (ErrorCategory.CONFLICT, 409),
        "conversation.git.push_failed": (ErrorCategory.CONFLICT, 409),
        "conversation.git.pull_failed": (ErrorCategory.CONFLICT, 409),
        "conversation.git.pull_conflict": (ErrorCategory.CONFLICT, 409),
        "conversation.git.timeout": (ErrorCategory.TIMEOUT, 504),
    }
    _GIT_WRITE_HINTS: dict[str, str] = {
        "conversation.git.pull_conflict": "在终端中解决冲突后继续（git rebase --continue / git merge --continue），或中止该操作",
        "conversation.git.timeout": "检查网络与凭据配置（服务端不会弹出凭据输入）",
    }

    def _git_write_error(exc: Any) -> JSONResponse:
        code = str(getattr(exc, "code", "") or "conversation.git.error")
        category, status_code = _GIT_WRITE_ERRORS.get(code, (ErrorCategory.VALIDATION, 400))
        return _error_response(
            code,
            str(exc),
            category,
            status_code=status_code,
            recovery_hint=_GIT_WRITE_HINTS.get(code, ""),
        )

    def _thread_git_write_root(thread_id: str) -> tuple[str, Any] | JSONResponse:
        """与 git/checkout 相同的归属检查：Thread 存在、已绑定工作区、根目录未被其他活动会话占用。"""
        thread, workspace = _thread_workspace(thread_id)
        if thread is None:
            return _error_response(
                "conversation.thread.not_found",
                f"unknown thread: {thread_id}",
                ErrorCategory.NOT_FOUND,
                status_code=404,
            )
        root = str(workspace.root_path or "").strip() if workspace is not None else ""
        if not root:
            return _error_response(
                "conversation.workspace.not_found",
                "会话尚未绑定工作目录",
                ErrorCategory.NOT_FOUND,
                status_code=404,
            )
        occupancy = _occupancy_payload(root, exclude_thread_id=thread_id)
        if occupancy["occupied"]:
            return _error_response(
                "conversation.git.root_occupied",
                "工作区正被其他活动会话使用，拒绝执行 Git 写操作以免影响对方",
                ErrorCategory.CONFLICT,
                status_code=409,
                recovery_hint="为当前会话新建独立 worktree",
            )
        return root, workspace

    def _git_status_payload(thread_id: str, workspace: Any, root: str, status: dict[str, Any]) -> dict[str, Any]:
        return {
            "thread_id": thread_id,
            "workspace_id": workspace.workspace_id if workspace else None,
            **status,
            **_occupancy_payload(root, exclude_thread_id=thread_id),
        }

    @router.post("/api/threads/{thread_id}/git/commit")
    async def thread_git_commit(thread_id: str, body: ThreadGitCommitBody) -> Any:
        """暂存并提交 Thread 工作区改动（全部或指定工作区内路径）。"""
        from muteki.conversation.git_workspace import GitWorkspaceError, commit_git_changes

        resolved = _thread_git_write_root(thread_id)
        if isinstance(resolved, JSONResponse):
            return resolved
        root, workspace = resolved
        try:
            result = await asyncio.to_thread(
                commit_git_changes,
                root,
                message=body.message,
                paths=body.paths,
                amend=body.amend,
            )
        except GitWorkspaceError as exc:
            return _git_write_error(exc)
        status = result.pop("status")
        return {**result, "git": _git_status_payload(thread_id, workspace, root, status)}

    @router.post("/api/threads/{thread_id}/git/push")
    async def thread_git_push(thread_id: str, body: ThreadGitPushBody | None = None) -> Any:
        """推送当前分支；分支没有上游时自动设置上游。"""
        from muteki.conversation.git_workspace import GitWorkspaceError, push_git_branch

        resolved = _thread_git_write_root(thread_id)
        if isinstance(resolved, JSONResponse):
            return resolved
        root, workspace = resolved
        try:
            options = body or ThreadGitPushBody()
            result = await asyncio.to_thread(push_git_branch, root, set_upstream=options.set_upstream)
        except GitWorkspaceError as exc:
            return _git_write_error(exc)
        status = result.pop("status")
        return {
            **result,
            "ahead": status.get("ahead"),
            "behind": status.get("behind"),
            "upstream": status.get("upstream"),
            "git": _git_status_payload(thread_id, workspace, root, status),
        }

    @router.post("/api/threads/{thread_id}/git/pull")
    async def thread_git_pull(thread_id: str, body: ThreadGitPullBody | None = None) -> Any:
        """拉取上游；默认仅快进（--ff-only），rebase=true 时使用 --rebase。"""
        from muteki.conversation.git_workspace import GitWorkspaceError, pull_git_branch

        resolved = _thread_git_write_root(thread_id)
        if isinstance(resolved, JSONResponse):
            return resolved
        root, workspace = resolved
        try:
            options = body or ThreadGitPullBody()
            result = await asyncio.to_thread(pull_git_branch, root, rebase=options.rebase)
        except GitWorkspaceError as exc:
            return _git_write_error(exc)
        status = result.pop("status")
        return {
            **result,
            "ahead": status.get("ahead"),
            "behind": status.get("behind"),
            "git": _git_status_payload(thread_id, workspace, root, status),
        }

    @router.post("/api/threads/{thread_id}/git/pull-request")
    async def thread_create_pull_request(thread_id: str, body: ThreadPullRequestCreateBody) -> Any:
        """为 Thread 工作区当前分支创建 GitHub PR（使用操作员的 GitHub token）。"""
        from muteki.conversation.git_workspace import (
            GitWorkspaceError,
            detect_github_remote,
            inspect_git_workspace,
            push_git_branch,
            upstream_remote_branch,
        )

        title = body.title.strip()
        if not title:
            return _error_response(
                "conversation.github.empty_title",
                "PR 标题不能为空",
                ErrorCategory.VALIDATION,
                status_code=400,
            )
        resolved = _thread_git_write_root(thread_id)
        if isinstance(resolved, JSONResponse):
            return resolved
        root, _workspace = resolved

        try:
            origin = await asyncio.to_thread(detect_github_remote, root)
        except GitWorkspaceError as exc:
            return _git_write_error(exc)
        if not origin["remote_url"]:
            return _error_response(
                "conversation.git.no_remote", "仓库未配置远端 origin", ErrorCategory.STATE, status_code=409,
            )
        if not origin["github"]:
            return _error_response(
                "conversation.github.not_github",
                f"远端 origin 不是 GitHub 仓库：{origin['remote_url']}",
                ErrorCategory.STATE,
                status_code=409,
            )
        token = await asyncio.to_thread(_github_pr_api_token)
        if not token:
            return _error_response(
                "conversation.github.token_missing",
                "未配置 GitHub token",
                ErrorCategory.PERMISSION,
                status_code=403,
                recovery_hint="设置 MUTEKI_GITHUB_TOKEN / GH_TOKEN / GITHUB_TOKEN，或在服务端运行 gh auth login",
            )

        pushed_now = False
        try:
            status = await asyncio.to_thread(inspect_git_workspace, root)
            needs_push = not status.get("upstream") or bool(status.get("ahead"))
            if needs_push:
                if not body.push_first:
                    return _error_response(
                        "conversation.git.push_required",
                        "分支尚未推送到远端（或有未推送的提交），请先推送再创建 PR",
                        ErrorCategory.STATE,
                        status_code=409,
                        recovery_hint="先推送，或以 push_first=true 重新请求",
                    )
                await asyncio.to_thread(push_git_branch, root)
                pushed_now = True
            tracking = await asyncio.to_thread(upstream_remote_branch, root)
            head_remote = tracking["remote"] or "origin"
            head_info = (
                origin if head_remote == "origin"
                else await asyncio.to_thread(detect_github_remote, root, head_remote)
            )
        except GitWorkspaceError as exc:
            return _git_write_error(exc)

        owner, repo = origin["owner"], origin["repo"]
        remote_branch = tracking["remote_branch"] or tracking["branch"]
        if head_info.get("github") and head_info["owner"] and head_info["owner"] != owner:
            head = f"{head_info['owner']}:{remote_branch}"
        else:
            head = remote_branch

        api = f"https://api.github.com/repos/{owner}/{repo}"
        try:
            base = body.base.strip()
            if not base:
                repo_info = await asyncio.to_thread(_github_api_json, "GET", api, token)
                base = str((repo_info or {}).get("default_branch") or "").strip()
                if not base:
                    return _error_response(
                        "conversation.github.base_unknown",
                        "无法确定仓库默认分支，请指定 base",
                        ErrorCategory.VALIDATION,
                        status_code=400,
                    )
            created = await asyncio.to_thread(
                _github_api_json,
                "POST",
                f"{api}/pulls",
                token,
                {"title": title, "head": head, "base": base, "body": body.body, "draft": body.draft},
            )
        except GitHubAPIError as exc:
            if exc.status == 422:
                category, status_code, code = ErrorCategory.CONFLICT, 409, "conversation.github.pr_rejected"
            elif exc.status in (401, 403, 404):
                category, status_code, code = ErrorCategory.PERMISSION, 403, "conversation.github.forbidden"
            else:
                category, status_code, code = ErrorCategory.RUNTIME, 502, "conversation.github.api_failed"
            return _error_response(code, str(exc), category, status_code=status_code)
        except Exception as exc:
            return _error_response(
                "conversation.github.api_failed",
                f"GitHub API 请求失败：{exc}",
                ErrorCategory.RUNTIME,
                status_code=502,
            )

        created = created if isinstance(created, dict) else {}
        return {
            "number": created.get("number"),
            "url": created.get("html_url", ""),
            "state": "draft" if created.get("draft") else created.get("state", ""),
            "title": created.get("title", title),
            "base": base,
            "head": head,
            "owner": owner,
            "repo": repo,
            "pushed": pushed_now,
        }

    def _surface_error(message: str, status_code: int = 400) -> JSONResponse:
        return _error_response(
            "conversation.workspace.surface_failed",
            message,
            ErrorCategory.VALIDATION,
            status_code=status_code,
        )

    @router.get("/api/threads/{thread_id}/workspace/files")
    async def workspace_files(thread_id: str, path: str = "") -> Any:
        from muteki.conversation.workspace_surfaces import WorkspaceSurfaceError, list_workspace_directory
        root = _thread_workspace_root(thread_id)
        if root is None:
            return _surface_error("当前会话未绑定工作区", 404)
        try:
            return await asyncio.to_thread(list_workspace_directory, root, path)
        except (WorkspaceSurfaceError, OSError) as exc:
            return _surface_error(str(exc))

    @router.get("/api/threads/{thread_id}/workspace/file")
    async def workspace_file(thread_id: str, path: str, line: int | None = None) -> Any:
        from muteki.conversation.workspace_surfaces import WorkspaceSurfaceError, read_workspace_file
        root = _thread_workspace_root(thread_id)
        if root is None:
            return _surface_error("当前会话未绑定工作区", 404)
        try:
            return await asyncio.to_thread(
                read_workspace_file, root, path, line=line,
            )
        except (WorkspaceSurfaceError, OSError) as exc:
            status = 404 if "不存在" in str(exc) or "已删除" in str(exc) else 400
            return _surface_error(str(exc), status)

    @router.get("/api/threads/{thread_id}/workspace/file/raw")
    async def workspace_file_raw(
        thread_id: str, path: str, download: bool = False,
    ) -> Any:
        """Raw workspace bytes for typed preview / original download (C17)."""
        from muteki.conversation.workspace_surfaces import (
            WorkspaceSurfaceError,
            guess_media_type,
            read_workspace_file_bytes,
        )
        root = _thread_workspace_root(thread_id)
        if root is None:
            return _surface_error("当前会话未绑定工作区", 404)
        try:
            _root, file_path, content = await asyncio.to_thread(
                read_workspace_file_bytes, root, path,
            )
        except (WorkspaceSurfaceError, OSError) as exc:
            status = 404 if "不存在" in str(exc) or "已删除" in str(exc) else 400
            return _surface_error(str(exc), status)
        from muteki.conversation.workspace_surfaces import safe_raw_content_headers

        media_type, headers = safe_raw_content_headers(
            media_type=guess_media_type(file_path),
            filename=file_path.name,
            download=download,
        )
        return Response(
            content=content,
            media_type=media_type,
            headers=headers,
        )

    @router.get("/api/threads/{thread_id}/workspace/search/files")
    async def workspace_search_files(thread_id: str, q: str = "", limit: int = 60) -> Any:
        from muteki.conversation.workspace_surfaces import WorkspaceSurfaceError, search_workspace_files
        root = _thread_workspace_root(thread_id)
        if root is None:
            return _surface_error("当前会话未绑定工作区", 404)
        try:
            return await asyncio.to_thread(search_workspace_files, root, q, min(limit, 100))
        except (WorkspaceSurfaceError, OSError) as exc:
            return _surface_error(str(exc))

    @router.get("/api/threads/{thread_id}/workspace/search/content")
    async def workspace_search_content(thread_id: str, q: str = "", limit: int = 30) -> Any:
        from muteki.conversation.workspace_surfaces import WorkspaceSurfaceError, search_workspace_content
        root = _thread_workspace_root(thread_id)
        if root is None:
            return _surface_error("当前会话未绑定工作区", 404)
        try:
            return await asyncio.to_thread(search_workspace_content, root, q, min(limit, 100))
        except (WorkspaceSurfaceError, OSError) as exc:
            return _surface_error(str(exc))

    @router.get("/api/threads/{thread_id}/workspace/diff")
    async def workspace_diff(
        thread_id: str,
        path: str = "",
        staging: str = "",
    ) -> Any:
        from muteki.conversation.workspace_surfaces import WorkspaceSurfaceError, read_workspace_diff
        root = _thread_workspace_root(thread_id)
        if root is None:
            return _surface_error("当前会话未绑定工作区", 404)
        try:
            return await asyncio.to_thread(
                read_workspace_diff,
                root,
                path=path or None,
                staging=staging or None,
            )
        except (WorkspaceSurfaceError, OSError) as exc:
            return _surface_error(str(exc))

    @router.post("/api/threads/{thread_id}/workspace/terminal")
    async def workspace_terminal(thread_id: str, body: WorkspaceCommandBody) -> Any:
        """Deprecated one-shot runner — prefer interactive PTY sessions (C16)."""
        from muteki.conversation.workspace_surfaces import WorkspaceSurfaceError, run_workspace_command
        root = _thread_workspace_root(thread_id)
        if root is None:
            return _surface_error("当前会话未绑定工作区", 404)
        try:
            return await asyncio.to_thread(run_workspace_command, root, body.command)
        except (WorkspaceSurfaceError, OSError) as exc:
            return _surface_error(str(exc))

    def _terminal_error(exc: Exception, *, status_code: int = 400) -> JSONResponse:
        from muteki.conversation.terminal_sessions import TerminalSessionError
        if isinstance(exc, TerminalSessionError):
            category = ErrorCategory.STATE if "exited" in exc.code or "changed" in exc.code else ErrorCategory.VALIDATION
            return _error_response(exc.code, str(exc), category, status_code=status_code)
        return _surface_error(str(exc), status_code)

    @router.post("/api/threads/{thread_id}/workspace/terminal/sessions")
    async def create_terminal_session(
        thread_id: str,
        body: TerminalSessionCreateBody | None = None,
    ) -> Any:
        from muteki.conversation.terminal_sessions import (
            TerminalSessionError,
            get_terminal_session_manager,
        )

        payload = body or TerminalSessionCreateBody()
        thread, workspace = _thread_workspace(thread_id)
        if thread is None:
            return _surface_error("当前会话不存在", 404)
        root = str(workspace.root_path or "").strip() if workspace is not None else ""
        if not root:
            return _surface_error("当前会话未绑定工作区", 404)
        workspace_id = workspace.workspace_id if workspace is not None else ""
        manager = get_terminal_session_manager()
        try:
            session = manager.create(
                thread_id=thread_id,
                workspace_id=workspace_id,
                root_path=root,
                cols=payload.cols,
                rows=payload.rows,
                name=payload.name,
                loop=asyncio.get_running_loop(),
            )
        except (TerminalSessionError, OSError) as exc:
            return _terminal_error(exc)
        return session.to_public()

    @router.get("/api/threads/{thread_id}/workspace/terminal/sessions")
    async def list_terminal_sessions(thread_id: str) -> Any:
        from muteki.conversation.terminal_sessions import get_terminal_session_manager

        thread, workspace = _thread_workspace(thread_id)
        if thread is None:
            return _surface_error("当前会话不存在", 404)
        root = str(workspace.root_path or "").strip() if workspace is not None else ""
        workspace_id = workspace.workspace_id if workspace is not None else None
        manager = get_terminal_session_manager()
        sessions = manager.list_for_thread(thread_id)
        for session in sessions:
            manager.ensure_workspace(session, root or None, workspace_id)
        return {"sessions": [session.to_public() for session in sessions]}

    @router.post("/api/threads/{thread_id}/workspace/terminal/sessions/{session_id}/interrupt")
    async def interrupt_terminal_session(thread_id: str, session_id: str) -> Any:
        from muteki.conversation.terminal_sessions import (
            TerminalSessionError,
            get_terminal_session_manager,
        )

        manager = get_terminal_session_manager()
        session = manager.get(session_id)
        if session is None or session.thread_id != thread_id:
            return _surface_error("终端会话不存在", 404)
        _thread, workspace = _thread_workspace(thread_id)
        root = str(workspace.root_path or "").strip() if workspace is not None else ""
        workspace_id = workspace.workspace_id if workspace is not None else None
        manager.ensure_workspace(session, root or None, workspace_id)
        try:
            session.interrupt()
        except TerminalSessionError as exc:
            return _terminal_error(exc)
        return session.to_public()

    @router.delete("/api/threads/{thread_id}/workspace/terminal/sessions/{session_id}")
    async def close_terminal_session(thread_id: str, session_id: str) -> Any:
        from muteki.conversation.terminal_sessions import get_terminal_session_manager

        manager = get_terminal_session_manager()
        session = manager.get(session_id)
        if session is None or session.thread_id != thread_id:
            return _surface_error("终端会话不存在", 404)
        manager.close(session_id, reason="用户关闭终端")
        return session.to_public()

    @router.websocket(
        "/api/threads/{thread_id}/workspace/terminal/sessions/{session_id}/stream"
    )
    async def terminal_session_stream(
        websocket: WebSocket,
        thread_id: str,
        session_id: str,
    ) -> None:
        """Ticketed interactive PTY stream (C16). Auth before accept()."""
        import base64

        from apps.web.auth import AuthConfig, bearer_from_header, verify_token
        from muteki.conversation.terminal_sessions import (
            TerminalSessionError,
            get_terminal_session_manager,
        )

        cfg: AuthConfig = websocket.app.state.auth
        if cfg.enabled:
            ticket_store = websocket.app.state.tickets
            authed = ticket_store.redeem(websocket.query_params.get("ticket"), scope=websocket.scope) or verify_token(
                cfg,
                websocket.query_params.get("token")
                or bearer_from_header(websocket.headers.get("Authorization")),
            )
            if not authed:
                await websocket.close(code=4401)
                return

        manager = get_terminal_session_manager()
        session = manager.get(session_id)
        if session is None or session.thread_id != thread_id:
            await websocket.close(code=4404)
            return

        _thread, workspace = _thread_workspace(thread_id)
        root = str(workspace.root_path or "").strip() if workspace is not None else ""
        workspace_id = workspace.workspace_id if workspace is not None else None
        manager.ensure_workspace(session, root or None, workspace_id)

        await websocket.accept()
        session.attach_loop(asyncio.get_running_loop())
        queue: asyncio.Queue[dict[str, Any]] = asyncio.Queue(maxsize=256)
        session.subscribe(queue)
        try:
            await websocket.send_json({
                "type": "hello",
                "session": session.to_public(),
            })
            await websocket.send_json({
                "type": "history",
                "data": session.history_b64(),
                "truncated": session.history.truncated,
            })
            await websocket.send_json({
                "type": "status",
                "session": session.to_public(),
            })

            async def _pump_out() -> None:
                while True:
                    message = await queue.get()
                    await websocket.send_json(message)

            async def _pump_in() -> None:
                while True:
                    raw = await websocket.receive_text()
                    try:
                        message = json.loads(raw)
                    except json.JSONDecodeError:
                        continue
                    if not isinstance(message, dict):
                        continue
                    msg_type = str(message.get("type") or "")
                    if msg_type == "stdin":
                        data = message.get("data")
                        if isinstance(data, str):
                            try:
                                chunk = base64.b64decode(data.encode("ascii"))
                            except Exception:
                                chunk = data.encode("utf-8", errors="replace")
                            try:
                                session.write_stdin(chunk)
                            except TerminalSessionError as exc:
                                await websocket.send_json({
                                    "type": "status",
                                    "session": session.to_public(),
                                    "message": str(exc),
                                })
                    elif msg_type == "resize":
                        try:
                            cols = int(message.get("cols") or session.cols)
                            rows = int(message.get("rows") or session.rows)
                        except (TypeError, ValueError):
                            continue
                        session.resize(cols, rows)
                    elif msg_type == "interrupt":
                        try:
                            session.interrupt()
                        except TerminalSessionError as exc:
                            await websocket.send_json({
                                "type": "status",
                                "session": session.to_public(),
                                "message": str(exc),
                            })

            out_task = asyncio.create_task(_pump_out())
            in_task = asyncio.create_task(_pump_in())
            done, pending = await asyncio.wait(
                {out_task, in_task},
                return_when=asyncio.FIRST_EXCEPTION,
            )
            for task in pending:
                task.cancel()
            for task in done:
                exc = task.exception()
                if exc and not isinstance(exc, WebSocketDisconnect):
                    raise exc
        except WebSocketDisconnect:
            return
        except asyncio.CancelledError:
            return
        finally:
            session.unsubscribe(queue)

    # -- 长期记忆：Thread 范围、显式允许、tombstone 删除 ----------------------

    @router.get("/api/threads/{thread_id}/memory")
    async def list_memory(
        thread_id: str,
        include_deleted: bool = False,
        q: str = "",
    ) -> Any:
        try:
            result = await command_api.query(_query(
                "conversation.memory.list", "thread", thread_id,
                thread_id=thread_id,
                include_deleted=include_deleted,
                query=q,
            ))
        except CommandAPIError as exc:
            status = 404 if exc.error.category is ErrorCategory.NOT_FOUND else 400
            return JSONResponse(_error_body(exc), status_code=status)
        return result.result

    @router.post("/api/threads/{thread_id}/memory")
    async def record_memory(thread_id: str, body: MemoryRecordBody) -> Any:
        return await _dispatch(_command(
            "conversation.memory.record", "thread", thread_id,
            {
                "thread_id": thread_id,
                "content": body.content,
                "kind": body.kind,
                "consent": body.consent,
            },
            command_id=body.command_id,
            idempotency_key=body.idempotency_key,
        ))

    @router.delete("/api/threads/{thread_id}/memory/{memory_id}")
    async def delete_memory(
        thread_id: str,
        memory_id: str,
        body: MemoryDeleteBody,
    ) -> Any:
        return await _dispatch(_command(
            "conversation.memory.delete", "thread", thread_id,
            {
                "thread_id": thread_id,
                "memory_id": memory_id,
                "confirm": body.confirm,
                "reason": body.reason,
            },
            command_id=body.command_id,
            idempotency_key=body.idempotency_key,
        ))

    # -- SSE：snapshot + watermark + after sequence --------------------------------

    @router.get("/api/threads/{thread_id}/events")
    async def thread_events(
        thread_id: str, request: Request, after: int = 0,
        snapshot: bool = True, browser_host: str = "",
    ) -> Any:
        """Thread 事件流（SSE）。

        首帧 ``event: snapshot`` 携带 Thread 视图与水位（``watermark`` 为
        该 Thread 流头，``event_watermark`` 为全局日志水位）；随后
        ``event: event`` 逐条下发 ``seq > after`` 的 Public Event。
        断线重连用最后收到的 seq 作为 ``after`` 参数即可不丢不重。
        """
        try:
            snapshot_result = await command_api.query(_query(
                "conversation.thread.view", "thread", thread_id,
                thread_id=thread_id,
                mark_read=False,
                messages_limit="50",
                include_events=False,
            ))
        except CommandAPIError as exc:
            status = 404 if exc.error.category is ErrorCategory.NOT_FOUND else 400
            return JSONResponse(
                _error_body(exc), status_code=status)
        snapshot_view = snapshot_result.result
        last_event_id = request.headers.get("last-event-id", "").strip()
        try:
            resume_after = int(last_event_id) if last_event_id else int(after)
        except ValueError:
            return _error_response(
                "conversation.events.cursor_invalid",
                "after / Last-Event-ID 必须是整数",
                ErrorCategory.VALIDATION,
                status_code=400,
                recovery_hint="使用上一条已确认事件的整数 seq 重新连接",
            )

        async def _stream_body():
            def _frame(event_name: str, data: Any, event_id: int = 0) -> bytes:
                id_line = f"id: {event_id}\n" if event_id > 0 else ""
                return (id_line
                        + f"event: {event_name}\n"
                        f"data: {json.dumps(data, ensure_ascii=False, default=str)}"
                        f"\n\n").encode("utf-8")

            if snapshot:
                yield _frame("snapshot", snapshot_view)
            # snapshot=true 是新订阅：以 after 为准，避免浏览器带上旧的
            # Last-Event-ID 后跳过本轮已产生的工具/正文事件。
            # 客户端应在重连时传入 after=<已应用水位>；snapshot 仍可刷新视图。
            cursor = max(0, int(after) if snapshot else resume_after)
            # 右侧浏览器请求不入事件日志：无 id 行，不影响 Last-Event-ID 水位。
            browser = (browser_control.subscribe(thread_id, browser_host)
                       if browser_control is not None and browser_host in {"desktop", "web"} else None)
            try:
                while True:
                    if await request.is_disconnected():
                        return
                    sent = False
                    for item in browser.drain() if browser else ():
                        yield _frame("browser_request", item)
                        sent = True
                    page = store.public_events_for(
                        conv_events.AGGREGATE_THREAD, thread_id,
                        after_seq=cursor, limit=SSE_PAGE_LIMIT)
                    for seq, event in page:
                        cursor = max(cursor, seq)
                        payload = event.model_dump(mode="json")
                        payload["seq"] = seq
                        yield _frame("event", payload, seq)
                        sent = True
                    if not sent:
                        # 心跳注释行，保持连接并便于代理保活。
                        yield b": heartbeat\n\n"
                        if browser:
                            await browser.wait(SSE_POLL_SECONDS)
                        else:
                            await asyncio.sleep(SSE_POLL_SECONDS)
            finally:
                if browser:
                    browser_control.unsubscribe(browser)

        async def _stream():
            if sse_metrics is not None:
                sse_metrics.open_sse("conversation", resumed=resume_after > 0)
            try:
                async for chunk in _stream_body():
                    yield chunk
            finally:
                if sse_metrics is not None:
                    sse_metrics.close_sse("conversation")

        return StreamingResponse(
            _stream(),
            media_type="text/event-stream",
            headers={"Cache-Control": "no-cache",
                     "X-Accel-Buffering": "no"},
        )

    @router.post("/api/threads/{thread_id}/browser/{request_id}")
    async def browser_result(thread_id: str, request_id: str, body: dict[str, Any] = Body(...)) -> Any:
        """客户端回传右侧浏览器请求的执行结果。"""
        if browser_control is None or not browser_control.resolve(thread_id, request_id, body):
            return _error_response(
                "chat.browser.request_unknown",
                "浏览器请求不存在、已超时或已有结果",
                ErrorCategory.NOT_FOUND,
                status_code=404,
            )
        return {"accepted": True}

    # -- 命令统一入口（conversation.*） ---------------------------------------------

    @router.post("/api/threads/{thread_id}/commands")
    async def post_command(thread_id: str, body: dict[str, Any] = Body(...)) -> Any:
        command_type = str(body.get("command_type") or "").strip()
        if not command_type.startswith(ALLOWED_COMMAND_PREFIX):
            return _error_response(
                "conversation.command.not_allowed",
                f"该入口只接受 {ALLOWED_COMMAND_PREFIX}* 命令",
                ErrorCategory.VALIDATION,
                status_code=400,
            )
        payload = dict(body.get("payload") or {})
        payload.setdefault("thread_id", thread_id)
        return await _dispatch(_command(
            command_type, "thread", thread_id, payload,
            command_id=str(body.get("command_id") or ""),
            idempotency_key=str(body.get("idempotency_key") or "")))

    @router.get("/api/threads/{thread_id}/export")
    async def export_thread(
        thread_id: str,
        format: str = "markdown",
        exclude_tools: int = 0,
        exclude_paths: int = 0,
    ) -> Any:
        """C32: Export the full thread as a reviewable snapshot.

        format=markdown  → text/markdown download
        format=jsonl     → application/x-ndjson download (one record per line)
        exclude_tools=1  → omit tool call argument detail (keep name + result summary)
        exclude_paths=1  → redact absolute filesystem paths
        """
        import datetime
        import re as _re

        try:
            result = await command_api.query(_query(
                "conversation.thread.view", "thread", thread_id,
                thread_id=thread_id,
                mark_read=False,
                messages_limit="1",
            ))
        except CommandAPIError as exc:
            status = 404 if exc.error.category is ErrorCategory.NOT_FOUND else 400
            return JSONResponse(_error_body(exc), status_code=status)

        try:
            view = await asyncio.to_thread(service.manager.export_snapshot, thread_id)
        except (ValueError, OSError, sqlite3.Error) as exc:
            return _error_response("conversation.export.incomplete", str(exc),
                ErrorCategory.STATE, status_code=409)
        thread = view.get("thread") or {}
        runtime = view.get("runtime") or {}
        messages: list[dict[str, Any]] = view.get("messages") or []
        turns: list[dict[str, Any]] = view.get("turns") or []
        artifacts: list[dict[str, Any]] = view.get("artifacts") or []
        tools: list[dict[str, Any]] = view.get("tools") or []
        watermark: int = int(view.get("watermark") or 0)

        title = str(thread.get("title") or "Conversation").strip() or "Conversation"
        model = str(runtime.get("model") or runtime.get("adapter_id") or "未知模型")
        tid = str(thread.get("thread_id") or thread_id)
        exported_at = datetime.datetime.utcnow().strftime("%Y-%m-%dT%H:%M:%SZ")
        date_slug = datetime.datetime.utcnow().strftime("%Y%m%d")

        _abs_path_re = _re.compile(
            r"(?<![\w:/\\])(?:[A-Za-z]:[\\/]|\\\\|//|/)[^\s\"'`<>\[\]{}(),;]+"
        )
        _quoted_path_re = _re.compile(r'''(["'`])((?:[A-Za-z]:[\\/]|\\\\|//|/)[^\n]*?)\1''')

        def _clean(text: str) -> str:
            if not text:
                return ""
            if exclude_paths:
                text = _quoted_path_re.sub(lambda match: match[1] + "<path>" + match[1], text)
                text = _abs_path_re.sub("<path>", text)
            return text

        def _clean_fields(value: Any) -> Any:
            if isinstance(value, str):
                return _clean(value)
            if isinstance(value, list):
                return [_clean_fields(item) for item in value]
            if isinstance(value, dict):
                return {_clean(str(key)): _clean_fields(item) for key, item in value.items()}
            return value

        view = _clean_fields(view)
        thread, runtime = view["thread"], view["runtime"]
        messages, turns, artifacts, tools = view["messages"], view["turns"], view["artifacts"], view["tools"]
        title = str(thread.get("title") or "Conversation").strip() or "Conversation"
        model = str(runtime.get("model") or runtime.get("adapter_id") or "未知模型")
        tid = str(thread.get("thread_id") or thread_id)
        path_redaction = {"enabled": bool(exclude_paths), "mode": "lexical_best_effort",
                          "limitation": "路径文本有歧义；分享前请检查导出内容" if exclude_paths else ""}
        if exclude_tools:
            tools = [{key: value for key, value in tool.items() if key != "arguments"} for tool in tools]
        export_metadata = {"complete": True, "message_count": len(messages), "turn_count": len(turns),
            "artifact_count": len(artifacts), "artifact_count_kind": "associations", "tool_count": len(tools),
            "watermark": watermark, "schema_version": 2, "scope": "current_branch",
            "binary_content_included": False, "path_redaction": "lexical_best_effort" if exclude_paths else None}

        def _export_headers(ext: str) -> dict[str, str]:
            return {"Content-Disposition": _content_disposition(ext),
                    "X-Muteki-Export-Metadata": json.dumps(export_metadata, separators=(",", ":")),
                    "Access-Control-Expose-Headers": "Content-Disposition, X-Muteki-Export-Metadata"}

        # Group messages by turn_id for ordered rendering.
        # Bug-fix (High): messages with turn_id=None (fork-inherited history) are
        # bucketed to "_no_turn" and rendered separately after the turn loop.
        turn_messages: dict[str, list[dict[str, Any]]] = {}
        for msg in messages:
            tid_key = str(msg.get("turn_id") or "_no_turn")
            turn_messages.setdefault(tid_key, []).append(msg)

        # Build turn order from turns list (respects seq ordering)
        ordered_turns = sorted(turns, key=lambda t: int(t.get("seq") or 0))

        # Bug-fix (High): preserve CJK / non-ASCII chars in filename; strip only
        # chars that are unsafe in HTTP headers and filenames.
        import urllib.parse as _urlparse
        safe_title = _re.sub(r'[<>:"/\\|?*\x00-\x1f]', '', title).strip()[:80] or "export"
        ascii_title = _re.sub(r'[^\x20-\x7e]', '', safe_title).strip()[:60] or "export"
        ascii_slug = ascii_title.replace(' ', '-')
        filename_base = f"{ascii_slug}-{date_slug}"

        def _content_disposition(ext: str) -> str:
            """RFC 5987 Content-Disposition with UTF-8 filename* fallback."""
            fname_ascii = f"{filename_base}.{ext}"
            fname_utf8 = f"{safe_title.replace(' ', '-')}-{date_slug}.{ext}"
            encoded = _urlparse.quote(fname_utf8, safe='')
            return f'attachment; filename="{fname_ascii}"; filename*=UTF-8\'\'{encoded}'

        if format == "jsonl":
            import io as _io
            buf = _io.StringIO()
            # Header record
            buf.write(json.dumps({
                "record": "header", "schema_version": 2, "title": title, "thread_id": tid,
                "model": model, "exported_at": exported_at, "watermark": watermark,
                **export_metadata,
                "current_runtime": runtime, "path_redaction": path_redaction,
                "binary_content_included": False,
                "event_source": {"aggregate_type": "thread", "aggregate_id": tid, "watermark": watermark},
                "options": {"exclude_tools": bool(exclude_tools), "exclude_paths": bool(exclude_paths)},
            }, ensure_ascii=False) + "\n")
            for turn in ordered_turns:
                record = {"record": "turn", "schema_version": 2, **turn}
                if exclude_tools:
                    record.pop("runtime_invocation", None)
                buf.write(json.dumps(record, ensure_ascii=False) + "\n")
            turns_by_id = {turn["turn_id"]: turn for turn in ordered_turns}
            for msg in messages:
                turn = turns_by_id.get(str(msg.get("turn_id") or "")) or {}
                record = {"record": "message", "schema_version": 2, **msg,
                          "turn_kind": turn.get("kind") or "history",
                          "turn_status": turn.get("status") or "not_available"}
                if exclude_tools and (msg.get("role") == "tool" or msg.get("kind") == "imported_tool"):
                    record.pop("source_content", None)
                buf.write(json.dumps(record, ensure_ascii=False) + "\n")
            for art in artifacts:
                buf.write(json.dumps({
                    "record": "artifact", "schema_version": 2, "sha256": art.get("sha256"),
                    "name": art.get("name"), "kind": art.get("kind"),
                    "media_type": art.get("media_type"), "size": art.get("size"),
                    "turn_id": art.get("turn_id"), "event_id": art.get("event_id"),
                    "stream_seq": art.get("stream_seq"),
                }, ensure_ascii=False) + "\n")
            for tool in tools:
                buf.write(json.dumps({"record": "tool", "schema_version": 2, **tool}, ensure_ascii=False) + "\n")
            content = buf.getvalue().encode("utf-8")
            return Response(
                content=content,
                media_type="application/x-ndjson",
                headers=_export_headers("jsonl"),
            )

        # Markdown format
        lines: list[str] = []
        lines.append(f"# {title}")
        lines.append(f"> 模型: {model} | Thread: {tid}")
        lines.append(f"> 导出时间: {exported_at} | 水位: #{watermark}")
        lines.append(f"> 当前分支完整快照 · {len(messages)} 条消息 · 二进制附件仅列索引")
        if exclude_tools or exclude_paths:
            opts = []
            if exclude_tools: opts.append("已排除工具参数")
            if exclude_paths: opts.append("路径已按词法规则脱敏，分享前请检查内容")
            lines.append(f"> 选项: {' · '.join(opts)}")
        lines.append("")

        turns_by_id = {turn["turn_id"]: turn for turn in ordered_turns}
        turn_ordinals = {turn["turn_id"]: index for index, turn in enumerate(ordered_turns, 1)}
        for msg in messages:
            turn_id = str(msg.get("turn_id") or "")
            turn = turns_by_id.get(turn_id) or {}
            role = str(msg.get("role") or "")
            role_label = {"user": "用户", "assistant": "助手", "tool": "工具", "system": "系统"}.get(role, "来源记录")
            text = str(msg.get("text") or "")
            status = str(turn.get("status") or "")
            status_note = f" ⚠ {status}" if status in {"failed", "interrupted"} else ""
            if not text and role == "user" and (turn.get("attachments") or turn.get("capability_refs")):
                text = "（仅附件或上下文引用）"
            if not text:
                continue
            prefix = f"轮次 {turn_ordinals[turn_id]}" if turn_id in turn_ordinals else "历史消息"
            lines.extend([f"## {prefix} — {role_label}{status_note}", "", text, ""])
            if role == "user":
                for digest in turn.get("attachments") or []:
                    lines.append(f"- 附件 SHA-256: `{digest}`")
                for ref in turn.get("capability_refs") or []:
                    lines.append(f"- 上下文引用: {ref.get('name') or ref.get('id') or '未命名'} ({ref.get('kind') or 'unknown'}) · ID `{ref.get('id') or ref.get('node_id') or ''}`")
                if turn.get("attachments") or turn.get("capability_refs"):
                    lines.append("")

        if artifacts:
            lines.append("---")
            lines.append("## 附件索引")
            lines.append("")
            for art in artifacts:
                sha = str(art.get("sha256") or "")
                name = str(art.get("name") or "未命名")
                kind = str(art.get("kind") or "")
                size = art.get("size")
                size_str = f", {size} bytes" if size else ""
                lines.append(f"- `{sha}` — **{name}** ({kind}{size_str})")
            lines.append("")

        if tools:
            lines.extend(["---", "## 工具记录", ""])
            for tool in tools:
                lines.extend([f"### {tool['name']} — {tool['status']}",
                    f"> 轮次: {tool['turn_id'] or '未关联'} | 工具 ID: {tool['tool_id']}", ""])
                if not exclude_tools and "arguments" in tool:
                    lines.extend(["参数:", "", json.dumps(tool["arguments"], ensure_ascii=False), ""])
                summary = tool["output_summary"]
                lines.extend(["结果摘要:", "", summary if isinstance(summary, str) else json.dumps(summary, ensure_ascii=False), ""])
                if not tool["output_complete"]:
                    lines.extend(["> 输出来源为已保存的过程更新，未记录完整最终输出。", ""])
                if tool.get("error"):
                    lines.extend(["错误:", "", json.dumps(tool["error"], ensure_ascii=False), ""])

        content = "\n".join(lines).encode("utf-8")
        return Response(
            content=content,
            media_type="text/markdown; charset=utf-8",
            headers=_export_headers("md"),
        )

    @router.post("/api/threads/{thread_id}/compact")
    async def compact_thread(thread_id: str) -> Any:
        """Invoke the verified native operation, then publish actual completion."""
        thread = service.manager.get_thread(thread_id)
        if thread is None:
            raise HTTPException(404, "对话不存在")
        state = service.conv.get_state(thread_id)
        if state.running_turn_id or service.conv.active_turn_id(thread_id):
            raise HTTPException(409, "请等待当前回复结束后再压缩")
        try:
            snapshot = await service.executor.runtime_capabilities(thread_id)
        except (RuntimeError, LookupError):
            raise HTTPException(409, "会话尚未就绪，请稍后重试") from None
        supported = any(item.name == "compact" and item.kind == "operation"
                        and item.verification == "verified" for item in snapshot.items)
        if not supported:
            raise HTTPException(501, "当前 Runtime 不支持结构化上下文压缩，请使用它公布的原生命令")
        service.manager.emit_context_window_event(thread_id, compact_status="running", source="manual_compact")
        try:
            result = await service.executor.runtime_operation(thread_id, "compact")
        except Exception:
            service.manager.emit_context_window_event(thread_id, compact_status="failed", source="manual_compact")
            raise HTTPException(400, "原生压缩未完成，请查看运行时状态后重试") from None
        service.manager.emit_context_window_event(thread_id, compact_status="done", source="manual_compact")
        return {"status": result.get("status", "completed"), "thread_id": thread_id, "result": result}

    @router.get("/api/threads/{thread_id}/user-input-fixture-capture")
    async def user_input_fixture_capture(thread_id: str) -> Any:
        """Return the last C22 fixture resolve payload (CU / tests)."""
        if os.environ.get("MUTEKI_ALLOW_USER_INPUT_FIXTURES", "").strip() != "1":
            return _error_response(
                "conversation.user_input.fixture_disabled",
                "Set MUTEKI_ALLOW_USER_INPUT_FIXTURES=1 to read fixture captures",
                ErrorCategory.PERMISSION,
                status_code=403,
            )
        capture = service.manager.get_user_input_fixture_capture(thread_id)
        return {"thread_id": thread_id, "capture": capture}

    # -- Artifact 读取与上传 ----------------------------------------------------------

    @router.get("/api/threads/{thread_id}/artifacts/{sha256}")
    async def read_artifact(
        thread_id: str, sha256: str, download: bool = False
    ) -> Any:
        # 先走 Thread 查询完成同一授权判定，再核对摘要确实由该 Thread 的
        # Artifact 事件引用，避免仅凭全局内容摘要读取其它 Thread 产物。
        try:
            await command_api.query(_query(
                "conversation.thread.view", "thread", thread_id,
                thread_id=thread_id, mark_read=False))
        except CommandAPIError as exc:
            status = 404 if exc.error.category is ErrorCategory.NOT_FOUND else 403
            return JSONResponse(_error_body(exc), status_code=status)
        metadata = service.platform.artifact_metadata(thread_id, sha256)
        attached = metadata is not None
        content = service.conv.read_artifact_content(sha256) if attached else None
        if content is None:
            return _error_response(
                "conversation.artifact.not_found",
                "该 Thread 中没有此 Artifact",
                ErrorCategory.NOT_FOUND,
                status_code=404,
            )
        from muteki.conversation.workspace_surfaces import safe_raw_content_headers

        raw_media = (
            metadata.get("media_type")
            if metadata is not None and metadata.get("media_type")
            else "application/octet-stream"
        )
        media_type, headers = safe_raw_content_headers(
            media_type=raw_media,
            filename=(str(metadata.get("name") or sha256) if metadata is not None else sha256),
            download=download,
        )
        return Response(
            content=content,
            media_type=media_type,
            headers=headers,
        )

    @router.post("/api/threads/{thread_id}/uploads")
    async def upload(
        thread_id: str,
        file: UploadFile,
        kind: str = "conversation.upload",
        command_id: str = Form(""),
        idempotency_key: str = Form(""),
    ) -> Any:
        content = await file.read()
        if not content:
            return _error_response(
                "conversation.upload.empty",
                "空文件不能附着",
                ErrorCategory.VALIDATION,
                status_code=400,
            )
        return await _dispatch(_command(
            "conversation.artifact.attach", "thread", thread_id, {
                "thread_id": thread_id,
                "name": file.filename or "upload",
                "content_base64": base64.b64encode(content).decode("ascii"),
                "media_type": file.content_type or "",
                "kind": kind,
            },
            command_id=str(command_id or "").strip(),
            idempotency_key=str(idempotency_key or "").strip(),
        ))

    # -- Provider Session Import（C33）-----------------------------------------

    @router.get("/api/import/scan")
    async def import_scan(
        adapter: str = "claude",
        path: str = "",
        limit: int = 100,
    ) -> Any:
        from pathlib import Path as _Path
        from muteki.conversation.provider_import import scan_to_dict
        source = _session_import_source(adapter)
        if source is None:
            return _error_response("conversation.import.provider_invalid", _session_import_invalid_message(), ErrorCategory.VALIDATION, status_code=400)
        scanner, default_base = source
        if not path and os.environ.get("MUTEKI_HOST_DISCOVERY", "1").strip() == "0":
            return _error_response("conversation.import.host_discovery_disabled", "服务宿主自动发现已禁用，请明确选择历史文件目录。", ErrorCategory.VALIDATION, status_code=409)
        base = _Path(path).expanduser() if path else default_base
        if not base.exists():
            return JSONResponse({"scans": [], "base_path": str(base), "exists": False})
        limit = max(1, min(limit, 500))
        try:
            scans = await asyncio.to_thread(scanner, base, limit=limit, retain_messages=False)
        except ValueError as exc:
            return _error_response(getattr(exc, "code", "conversation.import.scan_failed"), str(exc), ErrorCategory.VALIDATION, status_code=422)
        return JSONResponse({
            "scans": [scan_to_dict(s) for s in scans],
            "base_path": str(base),
            "exists": True,
        })

    @router.post("/api/import/apply")
    async def import_apply(body: dict[str, Any] = Body(...)) -> Any:
        from pathlib import Path as _Path
        from muteki.conversation.provider_import import batch_import
        adapter_id = str(body.get("adapter_id") or "claude")
        source = _session_import_source(adapter_id)
        if source is None:
            return _error_response("conversation.import.provider_invalid", _session_import_invalid_message(), ErrorCategory.VALIDATION, status_code=400)
        scanner, default_base = source
        source_path = str(body.get("source_path") or "")
        if not source_path and os.environ.get("MUTEKI_HOST_DISCOVERY", "1").strip() == "0":
            return _error_response("conversation.import.host_discovery_disabled", "服务宿主自动发现已禁用，请明确选择历史文件目录。", ErrorCategory.VALIDATION, status_code=409)
        session_ids: list[str] = [
            str(s) for s in (body.get("sessions") or [])
        ]
        project_id = str(body.get("project_id") or "")
        dry_run = bool(body.get("dry_run", False))

        base = _Path(source_path).expanduser() if source_path else default_base
        if not base.exists():
            return JSONResponse({"error": f"path not found: {base}"}, status_code=404)

        try:
            all_scans = await asyncio.to_thread(scanner, base, limit=500)
        except ValueError as exc:
            return _error_response(getattr(exc, "code", "conversation.import.scan_failed"), str(exc), ErrorCategory.VALIDATION, status_code=422)

        # Filter to requested session_ids (empty = all scanned)
        if session_ids:
            id_set = set(session_ids)
            all_scans = [s for s in all_scans if s.session_id in id_set]
            if {scan.session_id for scan in all_scans} != id_set:
                return _error_response("conversation.import.source_missing", "选中的会话来源已改变或不在本次扫描范围，请重新扫描。", ErrorCategory.VALIDATION, status_code=409)
        expected = body.get("source_versions") or {}
        if not isinstance(expected, dict) or any(expected.get(scan.session_id) != scan.source_fingerprint for scan in all_scans):
            return _error_response("conversation.import.source_changed", "历史文件在预览后已改变，请重新扫描。", ErrorCategory.VALIDATION, status_code=409)

        result = await asyncio.to_thread(
            batch_import, service.manager, all_scans,
            project_id=project_id, dry_run=dry_run,
        )
        return JSONResponse({
            "imported": [{"session_id": r.session_id, "thread_id": r.thread_id} for r in result.imported],
            "skipped":  [{"session_id": r.session_id, "thread_id": r.thread_id} for r in result.skipped],
            "failed":   [{"session_id": r.session_id, "error": r.error} for r in result.failed],
            "total": result.total,
        })

    # UI preferences are shared by the service's operator (not a per-login user).
    @router.get("/api/settings/ui")
    async def get_ui_preferences() -> Any:
        return JSONResponse(service.conv.get_sidebar_prefs("ui") or {"version": 0, "values": {}},
                            headers={"Cache-Control": "no-store"})

    @router.put("/api/settings/ui")
    async def put_ui_preferences(body: UiPreferencesWrite) -> Any:
        from muteki.platform.store import OptimisticConcurrencyError
        try:
            saved = service.conv.save_sidebar_prefs(
                {"values": body.values.model_dump(exclude_unset=True)},
                key="ui", expected_version=body.version)
        except OptimisticConcurrencyError:
            return JSONResponse(service.conv.get_sidebar_prefs("ui") or {"version": 0, "values": {}},
                                status_code=409, headers={"Cache-Control": "no-store"})
        return JSONResponse(saved, headers={"Cache-Control": "no-store"})

    # -- Sidebar 偏好（C30）--------------------------------------------------

    @router.get("/api/sidebar-preferences")
    async def get_sidebar_preferences() -> Any:
        prefs = service.conv.get_sidebar_prefs()
        if prefs is None:
            return JSONResponse({"version": 0, "pinned_ids": [], "thread_order": [],
                                  "project_order": [], "sort_mode": "updated",
                                  "pinned_sort_mode": "manual", "group_mode": "project"})
        return JSONResponse(prefs)

    @router.put("/api/sidebar-preferences")
    async def put_sidebar_preferences(body: dict[str, Any] = Body(...)) -> Any:
        from muteki.platform.store import OptimisticConcurrencyError
        expected: int | None = body.get("version") if isinstance(body.get("version"), int) else None
        try:
            saved = service.conv.save_sidebar_prefs(body, expected_version=expected)
        except OptimisticConcurrencyError:
            current = service.conv.get_sidebar_prefs() or {"version": 0}
            return JSONResponse(current, status_code=409)
        return JSONResponse(saved)

    # -- Conversation preferences the service acts on without an open window ----

    def _preference_body(
        prefs: dict[str, Any],
        *,
        thread_id: str = "",
        interaction_mode: str = "",
    ) -> dict[str, Any]:
        body: dict[str, Any] = {
            "version": int(prefs.get("version") or 0),
            "auto_resume_on_quota_reset": prefs.get("auto_resume_on_quota_reset") is True,
        }
        if thread_id:
            body["thread_id"] = thread_id
            body["interaction_mode"] = interaction_mode or "default"
        return body

    @router.get("/api/conversation/preferences")
    async def get_conversation_preferences(thread_id: str = "") -> Any:
        prefs = service.conv.get_sidebar_prefs(CONVERSATION_PREFS_KEY) or {"version": 0}
        thread_id = str(thread_id or "").strip()
        mode = ""
        if thread_id:
            if service.manager.get_thread(thread_id) is None:
                return _error_response(
                    THREAD_NOT_FOUND_CODE,
                    f"unknown thread: {thread_id}",
                    ErrorCategory.NOT_FOUND,
                    status_code=404,
                )
            mode = service.manager.runtime_selection(thread_id).interaction_mode
        return JSONResponse(_preference_body(
            prefs, thread_id=thread_id, interaction_mode=mode))

    @router.put("/api/conversation/preferences")
    async def put_conversation_preferences(
        body: dict[str, Any] = Body(...),
        thread_id: str = "",
    ) -> Any:
        has_mode = "interaction_mode" in body
        has_resume = "auto_resume_on_quota_reset" in body
        thread_id = str(body.get("thread_id") or thread_id or "").strip()
        if not has_mode:
            value = body.get("auto_resume_on_quota_reset")
            if not isinstance(value, bool):
                return _error_response(
                    "conversation.preferences.invalid",
                    "auto_resume_on_quota_reset 必须是布尔值",
                    ErrorCategory.VALIDATION,
                    status_code=400,
                )
            from muteki.platform.store import OptimisticConcurrencyError
            expected = body.get("version")
            if expected is not None and (type(expected) is not int or expected < 0):
                raise HTTPException(422, "version must be a non-negative integer")
            try:
                saved = service.conv.save_sidebar_prefs(
                    {"auto_resume_on_quota_reset": value}, key=CONVERSATION_PREFS_KEY,
                    expected_version=expected)
            except OptimisticConcurrencyError:
                return JSONResponse(_preference_body(service.conv.get_sidebar_prefs(CONVERSATION_PREFS_KEY) or {}), status_code=409)
            return JSONResponse({
                "version": saved["version"],
                "auto_resume_on_quota_reset": saved["auto_resume_on_quota_reset"],
            })
        if has_resume and not isinstance(body.get("auto_resume_on_quota_reset"), bool):
            return _error_response(
                "conversation.preferences.invalid",
                "auto_resume_on_quota_reset 必须是布尔值",
                ErrorCategory.VALIDATION,
                status_code=400,
            )
        if not thread_id:
            return _error_response(
                "conversation.preferences.invalid",
                "interaction_mode 按会话保存，需要 thread_id",
                ErrorCategory.VALIDATION,
                status_code=400,
            )
        if service.manager.get_thread(thread_id) is None:
            return _error_response(
                THREAD_NOT_FOUND_CODE,
                f"unknown thread: {thread_id}",
                ErrorCategory.NOT_FOUND,
                status_code=404,
            )
        raw_mode = body.get("interaction_mode")
        if not isinstance(raw_mode, str):
            return _error_response(
                INTERACTION_MODE_INVALID_CODE,
                "interaction_mode 必须是 default 或 plan",
                ErrorCategory.VALIDATION,
                status_code=400,
                detail={"interaction_mode": raw_mode, "allowed": ["default", "plan"]},
            )
        try:
            selection = service.manager.save_runtime_selection(
                thread_id,
                {"interaction_mode": raw_mode},
                validate_credential=False,
            )
        except InteractionModeError as exc:
            return _error_response(
                exc.code,
                str(exc),
                ErrorCategory.VALIDATION,
                status_code=400,
                detail=exc.detail,
            )
        except ConversationError as exc:
            not_found = exc.code in {THREAD_NOT_FOUND_CODE, TURN_NOT_FOUND_CODE}
            return _error_response(
                exc.code if not_found else "conversation.preferences.invalid",
                str(exc),
                ErrorCategory.NOT_FOUND if not_found else ErrorCategory.VALIDATION,
                status_code=404 if not_found else 400,
            )
        if has_resume:
            saved = service.conv.save_sidebar_prefs(
                {"auto_resume_on_quota_reset": body["auto_resume_on_quota_reset"]},
                key=CONVERSATION_PREFS_KEY)
        else:
            saved = service.conv.get_sidebar_prefs(CONVERSATION_PREFS_KEY) or {
                "version": 0, "auto_resume_on_quota_reset": False}
        return JSONResponse(_preference_body(
            saved,
            thread_id=thread_id,
            interaction_mode=selection.interaction_mode,
        ))

    return router


__all__ = [
    "ALLOWED_COMMAND_PREFIX",
    "OPERATOR",
    "SSE_PAGE_LIMIT",
    "SSE_POLL_SECONDS",
    "create_conversation_router",
]
