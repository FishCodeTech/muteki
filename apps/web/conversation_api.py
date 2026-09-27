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
    GET  /api/agent-runtimes                  （委托 RUNTIME-05 AgentRuntimeService）
    POST /api/agent-runtimes/{instance_id}/probe

约束：

- 所有状态修改经 ``command_api.dispatch``（产生 CommandReceipt），查询经
  ``command_api.query``；本模块不直接调 ConversationManager / 写 Store；
- SSE 用 snapshot + watermark + ``after`` sequence 恢复，事件来自
  PlatformStore 的 Thread 聚合流（Public Event 白名单字段），不直接读取
  Adapter 私有事件；
- 最后两个 agent-runtimes 端点委托 RUNTIME-05 的 ``AgentRuntimeService``；
  与 ``agent_runtime_api`` router 同路径，INTEG-01 挂载时二选一
  （``include_runtime_routes=False`` 可关闭本 router 内的这两条）。
"""

from __future__ import annotations

import asyncio
import base64
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
from typing import Any

from fastapi import APIRouter, Body, Form, Request, UploadFile, WebSocket, WebSocketDisconnect
from fastapi.responses import JSONResponse, Response, StreamingResponse
from pydantic import BaseModel, Field

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
from muteki.conversation.manager import ConversationError
from muteki.conversation.module import ConversationService
from muteki.external_agents.c24_cu_fixtures import (
    build_cu_fixture,
    fixture_id_from_title,
)
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


def create_conversation_router(
    *,
    command_api: Any,
    service: ConversationService,
    runtime_service: Any = None,
    actor: ActorRef = OPERATOR,
    register_handlers: bool = True,
    include_runtime_routes: bool = True,
    sse_metrics: Any = None,
    extension_service: Any = None,
) -> Any:
    """构造 Conversation API router。

    ``register_handlers`` 为 True 时把 CONV-01 Handler 注册到该 Command
    API（幂等）。``runtime_service`` 为 RUNTIME-05 的
    ``AgentRuntimeService``；缺省时 agent-runtimes 两端点返回 503。
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
    inbox_broker = InboxBroker()

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
    ) -> JSONResponse:
        error = ErrorEnvelope(
            code=code,
            message=message,
            category=category,
            recovery_hint=recovery_hint,
        )
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
            fixture_key = fixture_id_from_title(thread.title or "")
            if fixture_key:
                runtime_snapshot, matrix_from_fixture, _caps = build_cu_fixture(
                    fixture_key
                )
                service.executor._capability_cache[thread.thread_id] = runtime_snapshot
                runtime_diagnostics.append(
                    f"C24 CU fixture active: {fixture_key}"
                )
            elif adapter_id == selection.adapter_id:
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
        items = resolve_composer_catalog(
            engine=engine,
            workspace_root=str(workspace.root_path if workspace is not None else ""),
            trigger=body.trigger,
            query=body.query,
            extension_service=extension_service,
            threads=service.manager.list_threads(body.project_id),
            current_thread_id=body.thread_id,
            runtime_snapshot=runtime_snapshot,
        )
        matrix_payload = None
        if body.thread_id:
            thread_for_matrix = service.manager.get_thread(body.thread_id)
            fixture_key = fixture_id_from_title(
                thread_for_matrix.title if thread_for_matrix else ""
            )
            if fixture_key and runtime_snapshot is not None:
                matrix_payload = runtime_snapshot.public_matrix()
            else:
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
        return {
            "engine": engine,
            "trigger": body.trigger,
            "items": items,
            "count": len(items),
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
    ) -> Any:
        """Cross-thread attention inbox (SSE).

        First frame ``event: snapshot`` carries attention summaries + list
        rows and the current inbox cursor. Subsequent ``event: event`` frames
        are ``attention.updated`` / ``attention.cleared`` with monotonic
        ``seq`` for ``after`` / Last-Event-ID resume (same habit as C05).
        """
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

            rows = await asyncio.to_thread(
                collect_attention_rows, service.manager, project_id)
            # Snapshot bootstrap must not spam attention.* for the current
            # list; seed fingerprints first, then only emit live diffs.
            if snapshot or inbox_broker.seq == 0:
                inbox_broker.seed_rows(rows)

            cursor = max(0, int(after) if snapshot else resume_after)

            if snapshot:
                yield _frame(
                    "snapshot",
                    {
                        "inbox_seq": inbox_broker.seq,
                        "summaries": [row["summary"] for row in rows],
                        "threads": [row["thread"] for row in rows],
                        "count": len(rows),
                    },
                )
                cursor = max(cursor, inbox_broker.seq)
            else:
                for event in inbox_broker.events_after(cursor):
                    cursor = max(cursor, event.seq)
                    yield _frame("event", event.as_payload(), event.seq)

            while True:
                if await request.is_disconnected():
                    return
                rows = await asyncio.to_thread(
                    collect_attention_rows, service.manager, project_id)
                emitted = inbox_broker.sync_rows(rows)
                sent = False
                for event in emitted:
                    if event.seq <= cursor:
                        continue
                    cursor = max(cursor, event.seq)
                    yield _frame("event", event.as_payload(), event.seq)
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
    ) -> Any:
        try:
            result = await command_api.query(_query(
                "conversation.thread.search", "thread", "",
                q=q,
                project_id=project_id,
                include_archived=include_archived,
                include_superseded=include_superseded,
                limit=limit,
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
        return result.result

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
            status = 404 if "unknown" in str(exc).lower() or "不在" in str(exc) else 400
            return _error_response(
                "conversation.impact.preview_failed",
                str(exc),
                ErrorCategory.VALIDATION,
                status_code=status,
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
    ) -> Any:
        try:
            result = await command_api.query(_query(
                "conversation.turn.process", "thread", thread_id,
                thread_id=thread_id,
                turn_id=turn_id,
                limit=limit,
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
            authed = ticket_store.redeem(websocket.query_params.get("ticket")) or verify_token(
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
        snapshot: bool = True,
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
            while True:
                if await request.is_disconnected():
                    return
                page = store.public_events_for(
                    conv_events.AGGREGATE_THREAD, thread_id,
                    after_seq=cursor, limit=SSE_PAGE_LIMIT)
                sent = False
                for seq, event in page:
                    cursor = max(cursor, seq)
                    payload = event.model_dump(mode="json")
                    payload["seq"] = seq
                    yield _frame("event", payload, seq)
                    sent = True
                if not sent:
                    # 心跳注释行，保持连接并便于代理保活。
                    yield b": heartbeat\n\n"
                    await asyncio.sleep(SSE_POLL_SECONDS)

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
                messages_limit="all",
            ))
        except CommandAPIError as exc:
            status = 404 if exc.error.category is ErrorCategory.NOT_FOUND else 400
            return JSONResponse(_error_body(exc), status_code=status)

        view: dict[str, Any] = result.result
        thread = view.get("thread") or {}
        runtime = view.get("runtime") or {}
        state = view.get("state") or {}
        messages: list[dict[str, Any]] = view.get("messages") or []
        turns: list[dict[str, Any]] = view.get("turns") or []
        artifacts: list[dict[str, Any]] = view.get("artifacts") or []
        watermark: int = int(view.get("watermark") or 0)

        title = str(thread.get("title") or "Conversation").strip() or "Conversation"
        model = str(runtime.get("model") or runtime.get("adapter_id") or "未知模型")
        tid = str(thread.get("thread_id") or thread_id)
        exported_at = datetime.datetime.utcnow().strftime("%Y-%m-%dT%H:%M:%SZ")
        date_slug = datetime.datetime.utcnow().strftime("%Y%m%d")

        _abs_path_re = _re.compile(
            r"(/(?:home|Users|root|tmp|var|opt|workspace|mnt)/[^\s\"'`\])\n,;]+)"
        )

        def _clean(text: str) -> str:
            if not text:
                return ""
            if exclude_paths:
                text = _abs_path_re.sub("<path>", text)
            return text

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
                "record": "header", "title": title, "thread_id": tid,
                "model": model, "exported_at": exported_at, "watermark": watermark,
                "options": {"exclude_tools": bool(exclude_tools), "exclude_paths": bool(exclude_paths)},
            }, ensure_ascii=False) + "\n")
            for turn in ordered_turns:
                turn_id = str(turn.get("turn_id") or "")
                kind = str(turn.get("kind") or "")
                status = str(turn.get("status") or "")
                for msg in turn_messages.get(turn_id, []):
                    role = str(msg.get("role") or "")
                    msg_kind = str(msg.get("kind") or "message")
                    text = _clean(str(msg.get("text") or ""))
                    if not text:
                        continue
                    record: dict[str, Any] = {
                        "record": "message", "turn_id": turn_id, "turn_kind": kind,
                        "turn_status": status, "role": role, "kind": msg_kind, "text": text,
                    }
                    buf.write(json.dumps(record, ensure_ascii=False) + "\n")
            for art in artifacts:
                buf.write(json.dumps({
                    "record": "artifact", "sha256": art.get("sha256"),
                    "name": art.get("name"), "kind": art.get("kind"),
                    "media_type": art.get("media_type"), "size": art.get("size"),
                }, ensure_ascii=False) + "\n")
            # Bug-fix (High): fork-history messages with no turn_id
            for msg in turn_messages.get("_no_turn", []):
                role = str(msg.get("role") or "")
                msg_kind = str(msg.get("kind") or "message")
                text = _clean(str(msg.get("text") or ""))
                if not text:
                    continue
                buf.write(json.dumps({
                    "record": "message", "turn_id": None, "turn_kind": "history",
                    "turn_status": "completed", "role": role, "kind": msg_kind, "text": text,
                }, ensure_ascii=False) + "\n")
            content = buf.getvalue().encode("utf-8")
            return Response(
                content=content,
                media_type="application/x-ndjson",
                headers={"Content-Disposition": _content_disposition("jsonl")},
            )

        # Markdown format
        lines: list[str] = []
        lines.append(f"# {title}")
        lines.append(f"> 模型: {model} | Thread: {tid}")
        lines.append(f"> 导出时间: {exported_at} | 水位: #{watermark}")
        if exclude_tools or exclude_paths:
            opts = []
            if exclude_tools: opts.append("已排除工具参数")
            if exclude_paths: opts.append("已脱敏绝对路径")
            lines.append(f"> 选项: {' · '.join(opts)}")
        lines.append("")

        for i, turn in enumerate(ordered_turns, 1):
            turn_id = str(turn.get("turn_id") or "")
            kind = str(turn.get("kind") or "")
            t_status = str(turn.get("status") or "")
            status_note = f" ⚠ {t_status}" if t_status in {"failed", "interrupted"} else ""

            turn_msgs = turn_messages.get(turn_id, [])
            if not turn_msgs:
                continue

            for msg in turn_msgs:
                role = str(msg.get("role") or "")
                msg_kind = str(msg.get("kind") or "message")
                text = _clean(str(msg.get("text") or "")).strip()
                if not text:
                    continue
                role_label = "用户" if role == "user" else "助手"
                lines.append(f"## 轮次 {i} — {role_label}{status_note}")
                if msg_kind not in ("message", ""):
                    lines.append(f"*({msg_kind})*")
                lines.append("")
                lines.append(text)
                lines.append("")

            # Tool summary from turn usage/error
            if not exclude_tools:
                turn_usage = turn.get("usage") or {}
                if turn_usage.get("tool_calls"):
                    lines.append(f"### 工具调用")
                    lines.append(f"> 共 {turn_usage['tool_calls']} 次工具调用")
                    lines.append("")

        # Bug-fix (High): render fork-inherited messages that have no turn_id
        orphan_msgs = [
            m for m in turn_messages.get("_no_turn", [])
            if str(m.get("text") or "").strip()
        ]
        if orphan_msgs:
            lines.append("## 历史消息（继承自分叉来源）")
            lines.append("")
            for msg in orphan_msgs:
                role = str(msg.get("role") or "")
                text = _clean(str(msg.get("text") or "")).strip()
                role_label = "用户" if role == "user" else "助手"
                lines.append(f"### {role_label}")
                lines.append("")
                lines.append(text)
                lines.append("")

        if artifacts:
            lines.append("---")
            lines.append("## 附件索引")
            lines.append("")
            for art in artifacts:
                sha = str(art.get("sha256") or "")[:12]
                raw_name = str(art.get("name") or "未命名")
                # Medium fix: redact paths inside artifact names too
                name = _clean(raw_name)
                kind = str(art.get("kind") or "")
                size = art.get("size")
                size_str = f", {size} bytes" if size else ""
                lines.append(f"- `{sha}…` — **{name}** ({kind}{size_str})")
            lines.append("")

        content = "\n".join(lines).encode("utf-8")
        return Response(
            content=content,
            media_type="text/markdown; charset=utf-8",
            headers={"Content-Disposition": _content_disposition("md")},
        )

    @router.post("/api/threads/{thread_id}/compact")
    async def compact_thread(thread_id: str) -> Any:
        """C27: Request native context compaction for this thread.

        Returns 501 when the active Runtime does not declare compaction support.
        When supported, emits a ``core.context.window`` event with
        ``compact_status=running`` and returns ``{"status": "accepted"}``.
        The adapter is responsible for emitting the completion event
        (``compact_status=done`` or ``compact_status=failed``) once it finishes.
        """
        try:
            view = await command_api.query(_query(
                "conversation.thread.view", "thread", thread_id,
                thread_id=thread_id, mark_read=False))
        except CommandAPIError as exc:
            status = 404 if exc.error.category is ErrorCategory.NOT_FOUND else 400
            return JSONResponse(_error_body(exc), status_code=status)
        result = view.result or {}
        rt_conn = result.get("runtime_connection") or {}
        caps = rt_conn.get("capabilities") or {}
        compaction_supported = bool(caps.get("compaction"))
        if not compaction_supported:
            return JSONResponse(
                {
                    "error": {
                        "code": "conversation.compact.unsupported",
                        "message": "当前 Runtime 不支持原生上下文压缩",
                        "category": "unsupported",
                    }
                },
                status_code=501,
            )
        # Emit a context-window event marking compaction as in-progress.
        service.manager.emit_context_window_event(
            thread_id,
            compact_status="running",
            source="manual_compact",
        )
        return {"status": "accepted", "thread_id": thread_id}

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
        attached = any(
            event.event_type in {
                conv_events.EV_ARTIFACT_ATTACHED,
                conv_events.EV_ARTIFACT_CREATED,
            }
            and str(event.payload.get("sha256") or "") == sha256
            for event in service.platform.read_events(
                "thread", thread_id, limit=10_000)
        )
        content = service.conv.read_artifact_content(sha256) if attached else None
        if content is None:
            return _error_response(
                "conversation.artifact.not_found",
                "该 Thread 中没有此 Artifact",
                ErrorCategory.NOT_FOUND,
                status_code=404,
            )
        from muteki.platform.contracts.objects import Artifact
        artifact = service.platform.get(Artifact, sha256)
        from muteki.conversation.workspace_surfaces import safe_raw_content_headers

        raw_media = (
            artifact.media_type
            if artifact is not None and artifact.media_type
            else "application/octet-stream"
        )
        media_type, headers = safe_raw_content_headers(
            media_type=raw_media,
            filename=(artifact.name if artifact is not None else sha256),
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
        from muteki.conversation.provider_import import (
            scan_claude_sessions, scan_codex_sessions, scan_to_dict,
        )
        base = _Path(path).expanduser() if path else (
            _Path.home() / ".claude" if adapter == "claude"
            else _Path.home() / ".codex"
        )
        if not base.exists():
            return JSONResponse({"scans": [], "base_path": str(base), "exists": False})
        limit = max(1, min(limit, 500))
        scanner = scan_claude_sessions if adapter == "claude" else scan_codex_sessions
        scans = scanner(base, limit=limit)
        return JSONResponse({
            "scans": [scan_to_dict(s) for s in scans],
            "base_path": str(base),
            "exists": True,
        })

    @router.post("/api/import/apply")
    async def import_apply(body: dict[str, Any] = Body(...)) -> Any:
        from pathlib import Path as _Path
        from muteki.conversation.provider_import import (
            scan_claude_sessions, scan_codex_sessions, batch_import,
        )
        adapter_id = str(body.get("adapter_id") or "claude")
        source_path = str(body.get("source_path") or "")
        session_ids: list[str] = [
            str(s) for s in (body.get("sessions") or [])
        ]
        project_id = str(body.get("project_id") or "")
        dry_run = bool(body.get("dry_run", False))

        base = _Path(source_path).expanduser() if source_path else (
            _Path.home() / ".claude" if adapter_id == "claude"
            else _Path.home() / ".codex"
        )
        if not base.exists():
            return JSONResponse({"error": f"path not found: {base}"}, status_code=404)

        scanner = scan_claude_sessions if adapter_id == "claude" else scan_codex_sessions
        all_scans = scanner(base, limit=500)

        # Filter to requested session_ids (empty = all scanned)
        if session_ids:
            id_set = set(session_ids)
            all_scans = [s for s in all_scans if s.session_id in id_set]

        result = batch_import(
            service.manager, all_scans,
            project_id=project_id, dry_run=dry_run,
        )
        return JSONResponse({
            "imported": [{"session_id": r.session_id, "thread_id": r.thread_id} for r in result.imported],
            "skipped":  [{"session_id": r.session_id, "thread_id": r.thread_id} for r in result.skipped],
            "failed":   [{"session_id": r.session_id, "error": r.error} for r in result.failed],
            "total": result.total,
        })

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

    # -- Agent Runtime（委托 RUNTIME-05 的 service） ---------------------------------

    if include_runtime_routes:

        @router.get("/api/agent-runtimes")
        async def list_agent_runtimes(adapter_id: str = "") -> Any:
            if runtime_service is None:
                return _error_response(
                    "conversation.runtime.unavailable",
                    "AgentRuntimeService 未装配",
                    ErrorCategory.RUNTIME,
                    status_code=503,
                )
            instances = runtime_service.list_instances(adapter_id or None)
            return {"instances": instances, "count": len(instances)}

        @router.post("/api/agent-runtimes/{instance_id}/probe")
        async def probe_agent_runtime(instance_id: str) -> Any:
            if runtime_service is None:
                return _error_response(
                    "conversation.runtime.unavailable",
                    "AgentRuntimeService 未装配",
                    ErrorCategory.RUNTIME,
                    status_code=503,
                )
            # instance_id 形如 ``cli.kimi:default`` 或 bare instance id。
            from apps.web.agent_runtime_api import canonical_adapter_id
            from muteki.solver.worker_profiles import parse_runtime_instance_ref

            ref = parse_runtime_instance_ref(instance_id)
            if ref is not None:
                adapter_id, iid = ref
            else:
                adapter_id, iid = "", instance_id
                matches = [c for c in runtime_service.store.list()
                           if c.instance_id == iid]
                if len(matches) == 1:
                    adapter_id = matches[0].adapter_id
            adapter_id = canonical_adapter_id(adapter_id)
            if not adapter_id:
                return _error_response(
                    "conversation.runtime.unknown",
                    f"unknown runtime instance: {instance_id}",
                    ErrorCategory.NOT_FOUND,
                    status_code=404,
                )
            try:
                health = await runtime_service.probe_one(adapter_id, iid)
            except Exception as exc:  # noqa: BLE001 — 单实例故障隔离
                return _error_response(
                    "conversation.runtime.probe_failed",
                    f"{type(exc).__name__}: {str(exc)[:200]}",
                    ErrorCategory.RUNTIME,
                    status_code=502,
                )
            return {"instance": f"{adapter_id}:{iid}", "health": health}

    return router


__all__ = [
    "ALLOWED_COMMAND_PREFIX",
    "OPERATOR",
    "SSE_PAGE_LIMIT",
    "SSE_POLL_SECONDS",
    "create_conversation_router",
]
