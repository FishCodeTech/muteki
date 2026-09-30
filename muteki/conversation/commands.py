"""Conversation 命令 / 查询 Handler（CONV-01，任务书 9.1）。

``conversation.*`` 命名空间的属主，注册到共享 ``MutekiCommandAPI``；
API route 不直接调 Manager——所有状态修改经 Command API dispatch，
返回 CommandReceipt（异步命令模式）。

命令清单（任务书 9.1 全量）：

- conversation.project.create / conversation.workspace.bind
- conversation.thread.create / resume / fork / archive
- conversation.turn.send / steer / interrupt
- conversation.artifact.attach
- conversation.approval.resolve / conversation.user_input.resolve

幂等：command_id / idempotency_key 由 Command API 统一去重；Turn 另有
``(thread_id, idempotency_key)`` 唯一约束兜底，网络重试不会重复创建
Turn 或重复交付消息。
"""

from __future__ import annotations

import os
import re
from typing import Any, Optional

from muteki.platform.command_handlers.base import (
    CommandFailed,
    CommandPlan,
    HandlerContext,
    SideEffectResult,
    correlation_id_of,
    make_error,
)
from muteki.platform.contracts.commands import QueryResult
from muteki.platform.contracts.errors import ErrorCategory
from muteki.platform.contracts.receipts import (
    AggregateRef,
    CommandReceipt,
    ReceiptState,
)
from muteki.solver.engine_registry import (
    DSH_DISABLED_REASON,
    ENGINE_TEMPORARILY_UNSUPPORTED_CODE,
    temporarily_disabled_engine_in_payload,
)
from muteki.external_agents.factory import engine_for_adapter
from muteki.external_agents.approvals import ApprovalDecision
from muteki.external_agents.user_input_schema import (
    UserInputValidationError,
    c22_fixture_questions,
    expand_legacy_text_answers,
    flatten_answers_text,
    normalize_pending_user_input,
    validate_user_input_answers,
)
from muteki.platform.contracts.base import new_id

from muteki.platform.contracts.objects import Project

from . import events as ev
from .composer_capabilities import (
    ComposerCapabilityError,
    resolve_capability_refs,
)
from .executor import (
    ControlDeliveryError,
    ExternalAgentSessionExecutor,
    _match_runtime_capability,
)
from .manager import ConversationError, ConversationManager
from .models import (
    TURN_KIND_MESSAGE,
    TURN_RUNNING,
    QueuedTurnRequest,
    TurnRecord,
)

#: 全部 conversation.* 命令类型（注册用）。
COMMAND_TYPES = {
    "conversation.project.create",
    "conversation.project.update",
    "conversation.workspace.bind",
    "conversation.workspace.delete_worktree",
    "conversation.thread.create",
    "conversation.thread.rename",
    "conversation.thread.resume",
    "conversation.thread.fork",
    "conversation.thread.archive",
    "conversation.thread.unarchive",
    "conversation.turn.send",
    "conversation.turn.retry",
    "conversation.turn.edit_resend",
    "conversation.turn.native_rewind",
    "conversation.turn.steer",
    "conversation.turn.interrupt",
    "conversation.turn.resume",
    "conversation.queue.update",
    "conversation.queue.delete",
    "conversation.queue.reorder",
    "conversation.queue.pause",
    "conversation.queue.resume",
    "conversation.queue.steer",
    "conversation.artifact.attach",
    "conversation.approval.resolve",
    "conversation.approval.inject",
    "conversation.user_input.resolve",
    "conversation.user_input.inject",
    "conversation.memory.record",
    "conversation.memory.delete",
    "conversation.plan.inject",
    "conversation.plan.amend",
    "conversation.agents.inject",
}

_RUNTIME_INVOCATION_RE = re.compile(
    r"^(?P<prefix>[/\$])(?P<name>[^\s/\\]+)(?:\s+(?P<args>[\s\S]*))?$"
)
_INTERNAL_COMMANDS = frozenset({"new", "clear", "clear-input"})


def _failed(
    command: Any,
    code: str,
    message: str,
    category: ErrorCategory,
    *,
    state: ReceiptState = ReceiptState.FAILED,
    detail: Optional[dict[str, Any]] = None,
) -> CommandFailed:
    error = make_error(
        code, message, category, correlation_id=correlation_id_of(command),
    )
    if detail:
        error = error.model_copy(update={"detail": dict(detail)})
    return CommandFailed(
        error,
        state=state,
    )


def _receipt(command: Any, aggregate_type: str, aggregate_id: str,
             *, run_id: Optional[str] = None) -> CommandReceipt:
    return CommandReceipt(
        command_id=command.command_id,
        state=ReceiptState.ACCEPTED,
        run_id=run_id,
        aggregate=AggregateRef(type=aggregate_type, id=aggregate_id),
    )


def _reject_paused_runtime(command: Any, runtime: dict[str, Any]) -> None:
    if temporarily_disabled_engine_in_payload(runtime):
        raise _failed(
            command,
            ENGINE_TEMPORARILY_UNSUPPORTED_CODE,
            DSH_DISABLED_REASON,
            ErrorCategory.STATE,
        )


class ConversationCommandHandler:
    """``conversation.*`` 命令命名空间的属主（CONV-01）。"""

    command_types = COMMAND_TYPES

    def __init__(
        self,
        manager: ConversationManager,
        executor: ExternalAgentSessionExecutor,
    ) -> None:
        self._manager = manager
        self._executor = executor

    # -- 入口 --------------------------------------------------------------------

    async def plan(self, command: Any, ctx: HandlerContext) -> CommandPlan:
        ct = command.command_type
        if command.aggregate_id in self._executor._history_mutations:
            raise _failed(command, "conversation.history.busy", "正在回退聊天历史，请稍后重试", ErrorCategory.STATE)
        try:
            if ct == "conversation.project.create":
                return self._plan_project_create(command)
            if ct == "conversation.project.update":
                return self._plan_project_update(command)
            if ct == "conversation.workspace.bind":
                return self._plan_workspace_bind(command)
            if ct == "conversation.workspace.delete_worktree":
                return self._plan_workspace_delete_worktree(command)
            if ct == "conversation.thread.create":
                return self._plan_thread_create(command)
            if ct == "conversation.thread.rename":
                return self._plan_thread_rename(command)
            if ct in ("conversation.thread.resume", "conversation.turn.resume"):
                return self._plan_thread_resume(command)
            if ct == "conversation.thread.fork":
                return self._plan_thread_fork(command)
            if ct == "conversation.thread.archive":
                return self._plan_thread_archive(command)
            if ct == "conversation.thread.unarchive":
                return self._plan_thread_unarchive(command)
            if ct == "conversation.turn.send":
                return await self._plan_turn_send(command)
            if ct == "conversation.turn.retry":
                return self._plan_turn_retry(command)
            if ct == "conversation.turn.edit_resend":
                return self._plan_turn_edit_resend(command)
            if ct == "conversation.turn.native_rewind":
                return self._plan_turn_native_rewind(command)
            if ct == "conversation.turn.steer":
                return self._plan_turn_steer(command)
            if ct == "conversation.turn.interrupt":
                return self._plan_turn_interrupt(command)
            if ct == "conversation.queue.update":
                return self._plan_queue_update(command)
            if ct == "conversation.queue.delete":
                return self._plan_queue_delete(command)
            if ct == "conversation.queue.reorder":
                return self._plan_queue_reorder(command)
            if ct == "conversation.queue.pause":
                return self._plan_queue_pause(command)
            if ct == "conversation.queue.resume":
                return self._plan_queue_resume(command)
            if ct == "conversation.queue.steer":
                return self._plan_queue_steer(command)
            if ct == "conversation.artifact.attach":
                return self._plan_artifact_attach(command)
            if ct == "conversation.approval.inject":
                return self._plan_approval_inject(command)
            if ct == "conversation.approval.resolve":
                return self._plan_approval_resolve(command)
            if ct == "conversation.user_input.resolve":
                return self._plan_user_input_resolve(command)
            if ct == "conversation.user_input.inject":
                return self._plan_user_input_inject(command)
            if ct == "conversation.memory.record":
                return self._plan_memory_record(command)
            if ct == "conversation.memory.delete":
                return self._plan_memory_delete(command)
            if ct == "conversation.plan.inject":
                return self._plan_plan_inject(command)
            if ct == "conversation.agents.inject":
                return self._plan_agents_inject(command)
            return self._plan_plan_amend(command)
        except ConversationError as exc:
            raise _failed(command, "conversation.invalid", str(exc),
                          ErrorCategory.VALIDATION) from exc

    # -- Project / Workspace -------------------------------------------------------

    def _plan_project_create(self, command: Any) -> CommandPlan:
        payload = dict(command.payload)
        name = str(payload.get("name") or "").strip()
        if not name:
            raise _failed(command, "conversation.project.name_required",
                          "project.create requires payload.name",
                          ErrorCategory.VALIDATION)

        project = Project(
            name=name,
            description=str(payload.get("description") or ""),
            settings=dict(payload.get("settings") or {}),
        )
        manager = self._manager
        root_path = str(payload.get("root_path") or "").strip()
        try:
            workspace = (
                manager.plan_workspace(
                    project_id=project.project_id,
                    kind="local",
                    root_path=root_path,
                )
                if root_path else None
            )
        except ConversationError as exc:
            raise _failed(
                command,
                "conversation.directory.inaccessible",
                str(exc),
                ErrorCategory.VALIDATION,
            ) from exc
        if workspace is not None:
            for existing_project in sorted(
                manager.store.list(Project),
                key=lambda item: (item.created_at, item.project_id),
            ):
                existing_workspace = manager.primary_project_workspace(
                    existing_project.project_id,
                )
                if existing_workspace is None:
                    continue
                try:
                    same_directory = os.path.samefile(
                        workspace.root_path, existing_workspace.root_path,
                    )
                except OSError:
                    continue
                if same_directory:
                    raise _failed(
                        command,
                        "conversation.project.directory_already_bound",
                        f"工作目录已属于项目 {existing_project.name}",
                        ErrorCategory.CONFLICT,
                        state=ReceiptState.CONFLICT,
                        detail={
                            "project_id": existing_project.project_id,
                            "workspace_id": existing_workspace.workspace_id,
                            "root_path": existing_workspace.root_path,
                        },
                    )

        async def _save() -> SideEffectResult:
            manager.store.save(project)
            if workspace is not None:
                manager.save_workspace(workspace)
            return SideEffectResult(output={
                **({"workspace_id": workspace.workspace_id,
                    "root_path": workspace.root_path}
                   if workspace is not None else {}),
            })

        return CommandPlan(
            events=[ev.conversation_event(
                ev.AGGREGATE_PROJECT, project.project_id, ev.EV_PROJECT_CREATED, {
                    "project_id": project.project_id,
                    "name": project.name,
                    "description": project.description,
                }, actor_id=command.actor.id,
                command_id=command.command_id,
                correlation_id=correlation_id_of(command),
                idempotency_key=command.idempotency_key),
                *([ev.conversation_event(
                    ev.AGGREGATE_WORKSPACE,
                    workspace.workspace_id,
                    ev.EV_WORKSPACE_BOUND,
                    {
                        "workspace_id": workspace.workspace_id,
                        "project_id": workspace.project_id,
                        "kind": workspace.kind,
                        "root_path": workspace.root_path,
                    },
                    actor_id=command.actor.id,
                    command_id=command.command_id,
                    correlation_id=correlation_id_of(command),
                    idempotency_key=command.idempotency_key,
                )] if workspace is not None else []),
            ],
            receipt=_receipt(command, ev.AGGREGATE_PROJECT, project.project_id),
            side_effect=_save,
        )

    def _plan_project_update(self, command: Any) -> CommandPlan:
        payload = dict(command.payload)
        project_id = str(payload.get("project_id") or "").strip()
        if not project_id:
            raise _failed(command, "conversation.project.id_required",
                          "project.update requires payload.project_id",
                          ErrorCategory.VALIDATION)
        manager = self._manager
        project = manager.get_project(project_id)
        if project is None:
            raise _failed(command, "conversation.project.not_found",
                          f"project {project_id!r} not found",
                          ErrorCategory.NOT_FOUND)
        name = str(payload.get("name") or project.name)
        description = str(payload.get("description") if "description" in payload else project.description)
        settings = dict(project.settings or {})
        if "settings" in payload and isinstance(payload["settings"], dict):
            settings.update(payload["settings"])

        updated = project.model_copy(update={
            "name": name,
            "description": description,
            "settings": settings,
        })

        async def _save() -> SideEffectResult:
            manager.store.save(updated)
            return SideEffectResult(output={
                "project_id": updated.project_id,
                "name": updated.name,
                "settings": dict(updated.settings),
            })

        return CommandPlan(
            events=[ev.conversation_event(
                ev.AGGREGATE_PROJECT, updated.project_id, ev.EV_PROJECT_UPDATED, {
                    "project_id": updated.project_id,
                    "name": updated.name,
                    "description": updated.description,
                }, actor_id=command.actor.id,
                command_id=command.command_id,
                correlation_id=correlation_id_of(command),
                idempotency_key=command.idempotency_key)],
            receipt=_receipt(command, ev.AGGREGATE_PROJECT, updated.project_id),
            side_effect=_save,
        )

    def _plan_workspace_bind(self, command: Any) -> CommandPlan:
        payload = dict(command.payload)
        # 计划阶段做纯校验（目录存在性等），不建目录、不落库；new_worktree
        # 的 git worktree add 在 side_effect / save_workspace 中执行。
        try:
            planned = self._manager.plan_workspace(
                project_id=str(payload.get("project_id") or "") or None,
                kind=str(payload.get("kind") or "local"),
                root_path=str(payload.get("root_path") or ""),
                workspace_id=str(payload.get("workspace_id") or ""),
                mode=str(payload.get("mode") or ""),
                branch=str(payload.get("branch") or ""),
                base_ref=str(payload.get("base_ref") or "") or "HEAD",
                parent_root=str(payload.get("parent_root") or ""),
                settings=dict(payload.get("settings") or {}),
            )
        except ConversationError as exc:
            raise _failed(
                command,
                "conversation.workspace.bind_failed",
                str(exc),
                ErrorCategory.VALIDATION,
            ) from exc
        manager = self._manager

        async def _save() -> SideEffectResult:
            try:
                saved = manager.save_workspace(planned)
            except ConversationError as exc:
                raise _failed(
                    command,
                    "conversation.workspace.bind_failed",
                    str(exc),
                    ErrorCategory.VALIDATION,
                ) from exc
            return SideEffectResult(output={
                "workspace_id": saved.workspace_id,
                "root_path": saved.root_path,
                "kind": saved.kind,
                "settings": dict(saved.settings or {}),
            })

        return CommandPlan(
            events=[ev.conversation_event(
                ev.AGGREGATE_WORKSPACE, planned.workspace_id, ev.EV_WORKSPACE_BOUND, {
                    "workspace_id": planned.workspace_id,
                    "project_id": planned.project_id,
                    "kind": planned.kind,
                    "root_path": planned.root_path,
                    "mode": str((planned.settings or {}).get("mode") or ""),
                    "branch": str((planned.settings or {}).get("branch") or ""),
                }, actor_id=command.actor.id,
                command_id=command.command_id,
                correlation_id=correlation_id_of(command),
                idempotency_key=command.idempotency_key)],
            receipt=_receipt(command, ev.AGGREGATE_WORKSPACE, planned.workspace_id),
            side_effect=_save,
        )

    def _plan_workspace_delete_worktree(self, command: Any) -> CommandPlan:
        payload = dict(command.payload)
        workspace_id = str(
            payload.get("workspace_id") or command.aggregate_id or ""
        ).strip()
        if not workspace_id:
            raise _failed(
                command,
                "conversation.workspace.id_required",
                "delete_worktree requires workspace_id",
                ErrorCategory.VALIDATION,
            )
        force = bool(payload.get("force"))
        manager = self._manager
        workspace = manager.get_workspace(workspace_id)
        if workspace is None:
            raise _failed(
                command,
                "conversation.workspace.not_found",
                f"unknown workspace: {workspace_id}",
                ErrorCategory.NOT_FOUND,
            )

        async def _delete() -> SideEffectResult:
            try:
                result = manager.delete_worktree_workspace(
                    workspace_id, force=force,
                )
            except ConversationError as exc:
                raise _failed(
                    command,
                    "conversation.workspace.delete_failed",
                    str(exc),
                    ErrorCategory.VALIDATION,
                ) from exc
            return SideEffectResult(output=result)

        return CommandPlan(
            events=[ev.conversation_event(
                ev.AGGREGATE_WORKSPACE, workspace_id, ev.EV_WORKSPACE_CHANGED, {
                    "workspace_id": workspace_id,
                    "project_id": workspace.project_id,
                    "kind": workspace.kind,
                    "root_path": workspace.root_path,
                    "action": "delete_worktree",
                    "force": force,
                }, actor_id=command.actor.id,
                command_id=command.command_id,
                correlation_id=correlation_id_of(command),
                idempotency_key=command.idempotency_key)],
            receipt=_receipt(command, ev.AGGREGATE_WORKSPACE, workspace_id),
            side_effect=_delete,
        )

    # -- Thread --------------------------------------------------------------------

    def _plan_thread_create(self, command: Any) -> CommandPlan:
        payload = dict(command.payload)
        project_id = str(payload.get("project_id") or "")
        workspace_id = str(payload.get("workspace_id") or "")
        mode = str(payload.get("mode") or "conversation")
        if project_id and self._manager.get_project(project_id) is None:
            raise _failed(command, "conversation.project.not_found",
                          f"unknown project: {project_id}", ErrorCategory.NOT_FOUND)
        if workspace_id and self._manager.get_workspace(workspace_id) is None:
            raise _failed(command, "conversation.workspace.not_found",
                          f"unknown workspace: {workspace_id}",
                          ErrorCategory.NOT_FOUND)
        from muteki.platform.contracts.capabilities import ThreadMode
        try:
            ThreadMode(mode)
        except ValueError as exc:
            raise _failed(command, "conversation.thread.bad_mode",
                          f"未知 Thread 模式：{mode!r}",
                          ErrorCategory.VALIDATION) from exc
        from muteki.platform.contracts.objects import Thread

        thread = Thread(
            project_id=project_id or None,
            workspace_id=workspace_id or None,
            title=str(payload.get("title") or ""),
            title_source=str(payload.get("title_source") or "user"),
            mode=mode,
        )
        runtime = dict(payload.get("runtime") or {})
        _reject_paused_runtime(command, runtime)
        principal = str(payload.get("principal_id") or command.actor.id or "local-user")
        manager = self._manager

        async def _save() -> SideEffectResult:
            manager.activate_thread(thread, principal, runtime=runtime or None)
            return SideEffectResult()

        return CommandPlan(
            events=[ev.thread_event(thread.thread_id, ev.EV_THREAD_CREATED, {
                "thread_id": thread.thread_id,
                "project_id": thread.project_id,
                "workspace_id": thread.workspace_id,
                "title": thread.title,
                "title_source": thread.title_source,
                "mode": thread.mode,
                "runtime": runtime,
            }, actor_id=command.actor.id,
                command_id=command.command_id,
                correlation_id=correlation_id_of(command),
                idempotency_key=command.idempotency_key)],
            receipt=_receipt(command, ev.AGGREGATE_THREAD, thread.thread_id),
            side_effect=_save,
        )

    def _require_thread(self, command: Any) -> Any:
        thread_id = command.aggregate_id or str(command.payload.get("thread_id") or "")
        thread = self._manager.get_thread(thread_id)
        if thread is None:
            raise _failed(command, "conversation.thread.not_found",
                          f"unknown thread: {thread_id}", ErrorCategory.NOT_FOUND)
        return thread

    def _plan_thread_resume(self, command: Any) -> CommandPlan:
        thread = self._require_thread(command)
        state = self._manager.conv.get_state(thread.thread_id)
        if state.status == "archived":
            raise _failed(command, "conversation.thread.archived",
                          "archived thread cannot resume", ErrorCategory.STATE)
        current_turns = self._manager.conv.list_current_turns(thread.thread_id)
        last_turn = current_turns[-1] if current_turns else None
        if last_turn is None or last_turn.status not in {"failed", "interrupted"}:
            raise _failed(
                command,
                "conversation.turn.resume_not_available",
                "只有失败或中断的最新 Turn 可以继续执行",
                ErrorCategory.STATE,
            )
        text = str(command.payload.get("text") or "")
        # 访问策略是会话启动身份的一部分。恢复前允许前端提交当前选择，
        # 让下一次真实 continuation 使用新的 Runtime 会话；正在运行或
        # 等待审批的旧 Turn 不会被本命令修改。
        runtime_override = dict(command.payload.get("runtime") or {})
        _reject_paused_runtime(command, runtime_override)
        turn, run, _task, created = self._manager.request_turn(
            thread.thread_id, text=text, kind="resume",
            command_id=command.command_id,
            idempotency_key=command.idempotency_key)
        executor = self._executor

        async def _start() -> SideEffectResult:
            if runtime_override:
                self._manager.save_runtime_selection(
                    thread.thread_id, runtime_override)
            if created:
                executor.start_turn(turn.turn_id)
            return SideEffectResult()

        return CommandPlan(
            events=[ev.thread_event(thread.thread_id, ev.EV_THREAD_RESUMED, {
                "thread_id": thread.thread_id,
                "turn_id": turn.turn_id,
                "agent_session_id": state.agent_session_id,
                "runtime_override": runtime_override,
            }, actor_id=command.actor.id,
                command_id=command.command_id,
                correlation_id=correlation_id_of(command),
                idempotency_key=command.idempotency_key),
                *([ev.thread_event(thread.thread_id, ev.EV_TURN_REQUESTED, {
                    "turn_id": turn.turn_id,
                    "run_id": turn.run_id,
                    "task_id": turn.task_id,
                    "seq": turn.seq,
                    "kind": turn.kind,
                    "text": turn.text,
                }, actor_id=command.actor.id,
                    command_id=command.command_id,
                    correlation_id=correlation_id_of(command),
                    idempotency_key=command.idempotency_key)] if created else []),
            ],
            receipt=_receipt(command, ev.AGGREGATE_THREAD, thread.thread_id,
                             run_id=turn.run_id),
            side_effect=_start,
        )

    def _plan_turn_retry(self, command: Any) -> CommandPlan:
        thread = self._require_thread(command)
        turn_id = str(command.payload.get("turn_id") or "").strip()
        if not turn_id:
            raise _failed(
                command,
                "conversation.turn.id_required",
                "turn.retry requires payload.turn_id",
                ErrorCategory.VALIDATION,
            )
        manager = self._manager
        executor = self._executor
        idem = command.idempotency_key or command.command_id
        text = command.payload.get("text")
        turn, run, _task, superseded_ids, created = manager.retry_turn(
            thread.thread_id,
            turn_id,
            command_id=command.command_id,
            idempotency_key=idem,
            text=None if text is None else str(text),
        )
        is_edit = turn.kind == "edit_resend"

        async def _start() -> SideEffectResult:
            if created:
                executor.start_turn(turn.turn_id)
            return SideEffectResult(output={
                "impact": {
                    "mode": "edit_resend" if is_edit else "retry",
                    "superseded_turn_ids": superseded_ids,
                    "workspace_policy": "keep_files",
                    "external_side_effects": "cannot_undo",
                },
            })

        events = []
        if created:
            events.extend([
                ev.thread_event(
                    thread.thread_id,
                    ev.EV_TURN_EDIT_RESENT if is_edit else ev.EV_TURN_RETRIED,
                    {
                        "turn_id": turn.turn_id,
                        "replacement_turn_id": turn.turn_id,
                        "retry_of_turn_id": turn.retry_of_turn_id,
                        "superseded_turn_ids": superseded_ids,
                        "generation": run.generation,
                        "edited": is_edit,
                        "workspace_policy": "keep_files",
                        "external_side_effects": "cannot_undo",
                    },
                    actor_id=command.actor.id,
                    command_id=command.command_id,
                    correlation_id=correlation_id_of(command),
                    idempotency_key=command.idempotency_key,
                ),
                ev.thread_event(thread.thread_id, ev.EV_TURN_REQUESTED, {
                    "turn_id": turn.turn_id,
                    "run_id": turn.run_id,
                    "task_id": turn.task_id,
                    "seq": turn.seq,
                    "kind": turn.kind,
                    "retry_of_turn_id": turn.retry_of_turn_id,
                    "text": turn.text,
                    "attachments": list(turn.attachments),
                }, actor_id=command.actor.id,
                    command_id=command.command_id,
                    correlation_id=correlation_id_of(command),
                    idempotency_key=command.idempotency_key),
            ])

        return CommandPlan(
            events=events,
            receipt=_receipt(
                command, ev.AGGREGATE_THREAD, thread.thread_id,
                run_id=turn.run_id,
            ),
            side_effect=_start,
        )

    def _plan_turn_edit_resend(self, command: Any) -> CommandPlan:
        payload = dict(command.payload)
        if "text" not in payload:
            raise _failed(
                command,
                "conversation.turn.text_required",
                "turn.edit_resend requires payload.text",
                ErrorCategory.VALIDATION,
            )
        # Reuse retry planner with explicit edited text.
        command.payload = {**payload, "text": str(payload.get("text") or "")}
        return self._plan_turn_retry(command)

    def _plan_turn_native_rewind(self, command: Any) -> CommandPlan:
        thread = self._require_thread(command)
        turn_id = str(command.payload.get("turn_id") or "").strip()
        if not turn_id:
            raise _failed(
                command,
                "conversation.turn.id_required",
                "turn.native_rewind requires payload.turn_id",
                ErrorCategory.VALIDATION,
            )
        file_mode = str(command.payload.get("file_mode") or "keep_files")
        self._manager.native_rewind_turn(
            thread.thread_id, turn_id, file_mode=file_mode, dry_run=True,
            capability_override={"invocable": True},
            idempotency_key=command.idempotency_key or command.command_id)

        async def apply_rewind() -> SideEffectResult:
            result = await self._executor.rewind_turn(
                thread.thread_id, turn_id, command_id=command.command_id,
                idempotency_key=command.idempotency_key or command.command_id,
                file_mode=file_mode)
            payload = {"turn_id": turn_id, **result, "file_mode": file_mode,
                       "workspace_policy": file_mode, "external_side_effects": "cannot_undo"}
            return SideEffectResult(
                events=[ev.thread_event(
                    thread.thread_id, ev.EV_TURN_REWOUND, payload,
                    actor_id=command.actor.id, command_id=command.command_id,
                    correlation_id=correlation_id_of(command),
                    idempotency_key=command.idempotency_key or command.command_id,
                )] if result["applied"] else [],
                output={"impact": {"mode": "native_rewind", **payload}})

        return CommandPlan(
            events=[ev.thread_event(thread.thread_id, ev.EV_RUNTIME_OPERATION_REQUESTED,
                    {"name": "rewind", "turn_id": turn_id}, actor_id=command.actor.id,
                    command_id=command.command_id, correlation_id=correlation_id_of(command),
                    idempotency_key=command.idempotency_key)],
            receipt=_receipt(command, ev.AGGREGATE_THREAD, thread.thread_id),
            side_effect=apply_rewind,
        )

    def _plan_thread_fork(self, command: Any) -> CommandPlan:
        thread = self._require_thread(command)
        from_turn_id = str(command.payload.get("from_turn_id") or "")
        if not from_turn_id:
            completed_turns = [
                item for item in self._manager.conv.list_current_turns(thread.thread_id)
                if item.status == "completed"
            ]
            if completed_turns:
                from_turn_id = completed_turns[-1].turn_id
        title = str(command.payload.get("title") or "")
        principal = str(command.payload.get("principal_id")
                        or command.actor.id or "local-user")
        if from_turn_id:
            turn = self._manager.conv.get_turn(from_turn_id)
            if turn is None or turn.thread_id != thread.thread_id:
                raise _failed(command, "conversation.turn.not_found",
                              f"turn {from_turn_id} 不属于 thread {thread.thread_id}",
                              ErrorCategory.NOT_FOUND)
        from muteki.platform.contracts.objects import Thread

        forked = Thread(
            project_id=thread.project_id,
            workspace_id=thread.workspace_id,
            title=title or (f"{thread.title}（fork）" if thread.title else ""),
            mode=thread.mode,
        )
        manager = self._manager

        async def _fork() -> SideEffectResult:
            # 复制 Thread / Binding / Runtime 选择；从历史 Turn 派生新 Task。
            manager.complete_fork(
                forked, thread.thread_id,
                from_turn_id=from_turn_id, principal_id=principal)
            return SideEffectResult()

        return CommandPlan(
            events=[ev.thread_event(forked.thread_id, ev.EV_THREAD_FORKED, {
                "thread_id": forked.thread_id,
                "source_thread_id": thread.thread_id,
                "from_turn_id": from_turn_id or None,
                "title": forked.title,
                "mode": forked.mode,
            }, actor_id=command.actor.id,
                command_id=command.command_id,
                correlation_id=correlation_id_of(command),
                idempotency_key=command.idempotency_key)],
            receipt=_receipt(command, ev.AGGREGATE_THREAD, forked.thread_id),
            side_effect=_fork,
        )

    def _plan_thread_archive(self, command: Any) -> CommandPlan:
        thread = self._require_thread(command)
        manager = self._manager
        executor = self._executor
        state = manager.conv.get_state(thread.thread_id)
        already_archived = state.status == "archived"

        async def _archive() -> SideEffectResult:
            await executor.close_thread(thread.thread_id)
            if manager.conv.list_queue(thread.thread_id):
                manager.conv.pause_queue(thread.thread_id, "thread_archived")
            manager.archive_thread(thread.thread_id)
            return SideEffectResult(events=[ev.thread_event(
                thread.thread_id, ev.EV_THREAD_ARCHIVED,
                {"thread_id": thread.thread_id, "noop": already_archived},
                actor_id=command.actor.id, command_id=command.command_id,
                correlation_id=correlation_id_of(command))])

        return CommandPlan(
            events=[ev.thread_event(thread.thread_id, "core.thread.archive_requested", {
                "thread_id": thread.thread_id,
                "noop": already_archived,
            }, actor_id=command.actor.id,
                command_id=command.command_id,
                correlation_id=correlation_id_of(command),
                idempotency_key=command.idempotency_key)],
            receipt=_receipt(command, ev.AGGREGATE_THREAD, thread.thread_id),
            side_effect=_archive,
        )

    def _plan_thread_unarchive(self, command: Any) -> CommandPlan:
        """Restore Thread visibility without resuming agent execution or queue."""
        thread = self._require_thread(command)
        state = self._manager.conv.get_state(thread.thread_id)
        already_active = state.status != "archived"
        return CommandPlan(
            events=[ev.thread_event(thread.thread_id, ev.EV_THREAD_UNARCHIVED, {
                "thread_id": thread.thread_id,
                "noop": already_active,
            }, actor_id=command.actor.id,
                command_id=command.command_id,
                correlation_id=correlation_id_of(command),
                idempotency_key=command.idempotency_key)],
            receipt=_receipt(command, ev.AGGREGATE_THREAD, thread.thread_id),
        )

    def _plan_thread_rename(self, command: Any) -> CommandPlan:
        thread = self._require_thread(command)
        title = str(command.payload.get("title") or "").strip()
        if not title:
            raise _failed(
                command,
                "conversation.thread.title_required",
                "thread.rename requires payload.title",
                ErrorCategory.VALIDATION,
            )
        manager = self._manager

        async def _rename() -> SideEffectResult:
            updated = manager.rename_thread(thread.thread_id, title)
            return SideEffectResult(events=[ev.thread_event(
                thread.thread_id,
                ev.EV_THREAD_RENAMED,
                {"thread_id": thread.thread_id, "title": updated.title},
                actor_id=command.actor.id,
                command_id=command.command_id,
                correlation_id=correlation_id_of(command),
            )])

        return CommandPlan(
            events=[ev.thread_event(
                thread.thread_id,
                ev.EV_THREAD_RENAME_REQUESTED,
                {"thread_id": thread.thread_id},
                actor_id=command.actor.id,
                command_id=command.command_id,
                correlation_id=correlation_id_of(command),
                idempotency_key=command.idempotency_key,
            )],
            receipt=_receipt(command, ev.AGGREGATE_THREAD, thread.thread_id),
            side_effect=_rename,
        )

    # -- Turn ------------------------------------------------------------------------

    async def _plan_turn_send(self, command: Any) -> CommandPlan:
        thread = self._require_thread(command)
        state = self._manager.conv.get_state(thread.thread_id)
        if state.status == "archived":
            raise _failed(command, "conversation.thread.archived",
                          "已归档的对话不能发送消息，请先取消归档",
                          ErrorCategory.STATE)
        text = str(command.payload.get("text") or "").strip()
        # Frontend treats attachments / structured refs alone as a valid draft.
        # Accept the same contract here so attachment-only (and cite-only) turns
        # enqueue instead of failing before attachments are read.
        attachments = [
            str(a).strip()
            for a in (command.payload.get("attachments") or [])
            if str(a).strip()
        ]
        capability_refs_payload = command.payload.get("capability_refs") or []
        has_capability_refs = (
            isinstance(capability_refs_payload, list) and bool(capability_refs_payload)
        )
        if not text and not attachments and not has_capability_refs:
            raise _failed(
                command,
                "conversation.turn.text_required",
                "发送消息需要正文、附件或上下文引用",
                ErrorCategory.VALIDATION,
            )
        runtime_override = dict(command.payload.get("runtime") or {})
        _reject_paused_runtime(command, runtime_override)
        manager = self._manager
        executor = self._executor
        selection = manager.runtime_selection(thread.thread_id)
        adapter_id = str(runtime_override.get("adapter_id") or selection.adapter_id or "")
        launch_identity_fields = (
            "adapter_id", "instance_id", "credential_id", "access_mode",
            "permission_mode", "sandbox_mode", "model", "effort",
        )
        runtime_switch_pending = any(
            key in runtime_override
            and str(runtime_override.get(key) or "")
            != str(getattr(selection, key, "") or "")
            for key in launch_identity_fields
        )
        if "credential_ref" in runtime_override:
            runtime_switch_pending = runtime_switch_pending or (
                str(runtime_override.get("credential_ref") or "")
                != str(selection.credential_id or "")
            )
        runtime_invocation: dict[str, Any] = {}
        invocation_match = _RUNTIME_INVOCATION_RE.fullmatch(text)
        plugins = getattr(manager, "chat_plugins", None) if thread.mode == "conversation" else None
        if invocation_match is not None and plugins is not None:
            from muteki.conversation.composer_capabilities import discover_skills
            skill_name = invocation_match.group("name")
            managed_name = skill_name.removeprefix("muteki:")
            managed = next((row for row in plugins.skill_rows(engine_for_adapter(adapter_id))
                            if row["name"].casefold() == managed_name.casefold()), None)
            workspace_for_skill = manager.get_workspace(str(thread.workspace_id)) if thread.workspace_id else None
            native_names = {row["name"].casefold() for row in discover_skills(
                engine_for_adapter(adapter_id), workspace_for_skill.root_path if workspace_for_skill else "")}
            if managed is not None and (skill_name.startswith("muteki:") or skill_name.casefold() not in native_names):
                managed = {**managed, "arguments": str(invocation_match.group("args") or "")}
                if managed.get("native_engine"):
                    text = "/" + managed["native_name"] + (" " + managed["arguments"] if managed["arguments"] else "")
                    invocation_match = _RUNTIME_INVOCATION_RE.fullmatch(text)
                else:
                    capability_refs_payload = [*(capability_refs_payload if isinstance(capability_refs_payload, list) else []), managed]
                    invocation_match = None
        if invocation_match is not None:
            name = invocation_match.group("name")
            prefix = invocation_match.group("prefix")
            arguments = str(invocation_match.group("args") or "")
            if prefix == "/" and name.casefold() in _INTERNAL_COMMANDS:
                raise _failed(
                    command,
                    "conversation.command.local_only",
                    f"/{name} 是 Muteki 本地界面命令，不会发送给 Agent",
                    ErrorCategory.VALIDATION,
                )
            wire_text = f"{prefix}{name}".casefold()
            if runtime_switch_pending:
                # Runtime 切换在队列提升时发生；此处保存待解析的 wire text，
                # 真正建立新 Session 后必须再次从它的实时目录解析。
                runtime_invocation = {
                    "id": "",
                    "wire_text": f"{prefix}{name}",
                    "arguments": arguments,
                    "pending_resolution": True,
                }
            else:
                snapshot = executor.cached_runtime_capabilities(thread.thread_id)
                if snapshot is None or snapshot.stale:
                    runtime_invocation = {
                        "id": "",
                        "wire_text": f"{prefix}{name}",
                        "arguments": arguments,
                        "pending_resolution": True,
                    }
                    snapshot = None
                item = (
                    _match_runtime_capability(
                        snapshot, {"id": "", "wire_text": wire_text},
                    )
                    if snapshot is not None else None
                )
                if snapshot is not None and item is None:
                    raise _failed(
                        command,
                        "conversation.command.not_available",
                        f"当前 {snapshot.adapter_id} Session 没有公布 {prefix}{name}",
                        ErrorCategory.VALIDATION,
                    )
                if item is not None and item.kind == "operation" and item.resolution == "client":
                    async def _runtime_operation() -> SideEffectResult:
                        result = await executor.runtime_operation(
                            thread.thread_id, item.name, arguments)
                        return SideEffectResult(
                            events=[ev.thread_event(
                                thread.thread_id,
                                ev.EV_RUNTIME_OPERATION_COMPLETED,
                                {
                                    "capability_id": item.id,
                                    "name": item.name,
                                    "result": result,
                                },
                                actor_id=command.actor.id,
                                command_id=command.command_id,
                                correlation_id=correlation_id_of(command),
                            )],
                            output={
                                "runtime_operation": item.name,
                                "result": result,
                            },
                        )

                    return CommandPlan(
                        events=[ev.thread_event(
                            thread.thread_id,
                            ev.EV_RUNTIME_OPERATION_REQUESTED,
                            {
                                "capability_id": item.id,
                                "name": item.name,
                                "arguments": arguments,
                            },
                            actor_id=command.actor.id,
                            command_id=command.command_id,
                            correlation_id=correlation_id_of(command),
                            idempotency_key=command.idempotency_key,
                        )],
                        receipt=_receipt(
                            command, ev.AGGREGATE_THREAD, thread.thread_id),
                        side_effect=_runtime_operation,
                    )
                if item is not None:
                    runtime_invocation = item.model_dump(mode="json")
                    runtime_invocation["arguments"] = arguments
        workspace = (
            manager.get_workspace(str(thread.workspace_id or ""))
            if thread.workspace_id else None
        )
        try:
            capability_refs, _ = resolve_capability_refs(
                capability_refs_payload if isinstance(capability_refs_payload, list) else [],
                engine=engine_for_adapter(adapter_id),
                workspace_root=str(workspace.root_path if workspace is not None else ""),
                extension_service=manager.extension_service,
                plugin_service=getattr(manager, "chat_plugins", None) if thread.mode == "conversation" else None,
                threads=manager.list_threads(),
                message_loader=manager.conv.list_current_messages,
                message_lookup=manager.conv.get_message,
            )
        except ComposerCapabilityError as exc:
            raise _failed(
                command,
                "conversation.composer.reference_invalid",
                str(exc),
                ErrorCategory.VALIDATION,
            ) from exc

        # 所有普通消息先落持久化队列。线程空闲时副作用立即提升队首为 Turn；
        # 线程忙碌时保留在队列，上一轮成功结束后自动提升。
        idem = command.idempotency_key or command.command_id
        runtime = selection.model_dump(mode="json")
        runtime.update(runtime_override)
        item = QueuedTurnRequest(
            thread_id=thread.thread_id,
            command_id=command.command_id,
            idempotency_key=idem,
            client_message_id=str(
                command.payload.get("client_message_id") or command.command_id
            ),
            actor_id=command.actor.id,
            correlation_id=correlation_id_of(command) or command.command_id,
            text=text,
            attachments=attachments,
            capability_refs=capability_refs,
            runtime_invocation=runtime_invocation,
            runtime=runtime,
        )
        created = True

        def _enqueue() -> None:
            nonlocal item, created
            item, created = manager.conv.enqueue_turn(item)
            event = plan.events[0]
            plan.events[0] = event.model_copy(update={"payload": {
                **event.payload, "queue_id": item.queue_id,
                "position": item.position, "deduplicated": not created,
                "queue_revision": manager.conv.get_state(thread.thread_id).queue_revision,
            }})

        async def _start() -> SideEffectResult:
            await executor.start_next_queued(thread.thread_id)
            return SideEffectResult(output={
                "queue_id": item.queue_id,
                "queued": True,
            })

        plan = CommandPlan(
            events=[ev.thread_event(thread.thread_id, ev.EV_QUEUE_ADDED, {
                "queue_id": item.queue_id,
                "position": item.position,
                "text": item.text,
                "attachments": list(item.attachments),
                "capability_refs": list(item.capability_refs),
                "runtime_invocation": dict(item.runtime_invocation),
                "queue_revision": manager.conv.get_state(
                    thread.thread_id).queue_revision,
                "deduplicated": not created,
            }, actor_id=command.actor.id,
                command_id=command.command_id,
                correlation_id=correlation_id_of(command),
                idempotency_key=command.idempotency_key)],
            receipt=_receipt(command, ev.AGGREGATE_THREAD, thread.thread_id),
            side_effect=_start,
            local_commit=_enqueue,
        )
        return plan

    def _plan_turn_steer(self, command: Any) -> CommandPlan:
        thread = self._require_thread(command)
        text = str(command.payload.get("text") or "").strip()
        if not text:
            raise _failed(command, "conversation.turn.text_required",
                          "turn.steer requires payload.text",
                          ErrorCategory.VALIDATION)
        state = self._manager.conv.get_state(thread.thread_id)
        if state.status == "archived":
            raise _failed(command, "conversation.thread.archived",
                          "已归档的对话不能发送消息，请先取消归档",
                          ErrorCategory.STATE)
        if not state.running_turn_id:
            raise _failed(command, "conversation.turn.not_running",
                          "当前没有执行中的 Turn 可以 steer",
                          ErrorCategory.STATE)
        expected_turn_id = str(
            command.payload.get("expected_turn_id") or ""
        ).strip()
        if not expected_turn_id or expected_turn_id != state.running_turn_id:
            raise _failed(
                command,
                "conversation.turn.expected_mismatch",
                "执行中的 Turn 已变化，请刷新后重新引导",
                ErrorCategory.CONFLICT,
            )
        client_message_id = str(
            command.payload.get("client_message_id") or command.command_id
        )
        capability_revision_raw = command.payload.get("capability_revision")
        capability_revision = (
            int(capability_revision_raw)
            if capability_revision_raw is not None
            and str(capability_revision_raw).strip() != ""
            else None
        )
        executor = self._executor

        async def _steer() -> SideEffectResult:
            try:
                receipt = await executor.steer(
                    thread.thread_id,
                    text,
                    expected_turn_id=expected_turn_id,
                    client_message_id=client_message_id,
                    capability_revision=capability_revision,
                )
            except RuntimeError as exc:
                return SideEffectResult(
                    error=make_error(
                        "conversation.turn.expected_mismatch",
                        str(exc),
                        ErrorCategory.CONFLICT,
                        correlation_id=correlation_id_of(command),
                    ),
                    state=ReceiptState.FAILED,
                )
            if receipt.error is not None:
                # 能力缺失走 typed unsupported receipt：回执 FAILED + 稳定机器码。
                return SideEffectResult(error=receipt.error,
                                        state=ReceiptState.FAILED)
            return SideEffectResult(events=[ev.thread_event(
                thread.thread_id, ev.EV_TURN_STEERED, {
                    "turn_id": state.running_turn_id,
                    "text": text,
                    "client_message_id": client_message_id,
                }, actor_id=command.actor.id,
                command_id=command.command_id,
                correlation_id=correlation_id_of(command),
            )])

        return CommandPlan(
            events=[ev.thread_event(thread.thread_id, "core.turn.steer_requested", {
                "turn_id": state.running_turn_id,
            }, actor_id=command.actor.id,
                command_id=command.command_id,
                correlation_id=correlation_id_of(command),
                idempotency_key=command.idempotency_key)],
            receipt=_receipt(command, ev.AGGREGATE_THREAD, thread.thread_id),
            side_effect=_steer,
        )

    # -- 后续消息队列 ---------------------------------------------------------

    def _queue_item(self, command: Any, thread_id: str) -> Any:
        queue_id = str(command.payload.get("queue_id") or "").strip()
        item = self._manager.conv.get_queue_item(queue_id)
        if item is None or item.thread_id != thread_id:
            raise _failed(
                command,
                "conversation.queue.not_found",
                f"unknown queue item: {queue_id}",
                ErrorCategory.NOT_FOUND,
            )
        return item

    def _plan_queue_update(self, command: Any) -> CommandPlan:
        thread = self._require_thread(command)
        item = self._queue_item(command, thread.thread_id)
        text = str(command.payload.get("text", item.text) or "").strip()
        if not text and not item.attachments and not item.capability_refs:
            raise _failed(
                command, "conversation.queue.text_required",
                "queue.update requires payload.text", ErrorCategory.VALIDATION,
            )
        runtime = None
        if "runtime" in command.payload:
            raw_runtime = dict(command.payload.get("runtime") or {})
            _reject_paused_runtime(command, raw_runtime)
            previous_runtime = dict(item.runtime or {})
            changing_identity = any(
                key in raw_runtime and raw_runtime[key] != previous_runtime.get(key)
                for key in ("adapter_id", "instance_id", "credential_id")
            )
            if changing_identity:
                # Engine-specific overrides belong to the old launch identity.
                previous_runtime.update({"permission_mode": "", "sandbox_mode": ""})
            raw_runtime = {**previous_runtime, **raw_runtime}
            try:
                runtime = self._manager._build_runtime_selection(
                    thread.thread_id, raw_runtime).model_dump(mode="json")
            except ConversationError as exc:
                raise _failed(command, "conversation.queue.runtime_invalid", str(exc),
                              ErrorCategory.VALIDATION) from exc
        capability_refs = None
        if runtime is not None:
            workspace = (
                self._manager.get_workspace(str(thread.workspace_id))
                if thread.workspace_id else None
            )
            try:
                capability_refs, _ = resolve_capability_refs(
                    list(item.capability_refs),
                    engine=engine_for_adapter(str(runtime.get("adapter_id") or "")),
                    workspace_root=str(workspace.root_path if workspace is not None else ""),
                    extension_service=self._manager.extension_service,
                    threads=self._manager.list_threads(),
                    message_loader=self._manager.conv.list_current_messages,
                    message_lookup=self._manager.conv.get_message,
                )
            except ComposerCapabilityError as exc:
                raise _failed(
                    command, "conversation.queue.reference_invalid", str(exc),
                    ErrorCategory.VALIDATION,
                ) from exc
        invocation = None
        if text != item.text or runtime is not None:
            invocation = {}
            match = _RUNTIME_INVOCATION_RE.fullmatch(text)
            if match is not None:
                prefix, name = match.group("prefix"), match.group("name")
                if prefix == "/" and name.casefold() in _INTERNAL_COMMANDS:
                    raise _failed(
                        command, "conversation.command.local_only",
                        f"/{name} 是本地界面命令，请在输入框中执行", ErrorCategory.VALIDATION,
                    )
                # The edited message cannot keep a capability id/revision from
                # another message or Runtime. Resolve the explicit wire command
                # against the selected session's fresh catalog at dispatch.
                invocation = {
                    "id": "", "wire_text": f"{prefix}{name}",
                    "arguments": str(match.group("args") or ""),
                    "pending_resolution": True,
                }
        expected = command.payload.get("expected_revision")
        if expected is not None and (isinstance(expected, bool) or not isinstance(expected, int) or expected < 0):
            raise _failed(command, "conversation.queue.revision_invalid",
                          "expected_revision 必须是非负整数", ErrorCategory.VALIDATION)
        event = ev.thread_event(thread.thread_id, ev.EV_QUEUE_UPDATED, {
            "queue_id": item.queue_id,
        }, actor_id=command.actor.id, command_id=command.command_id,
            correlation_id=correlation_id_of(command), idempotency_key=command.idempotency_key)

        def _update() -> None:
            try:
                updated = self._manager.conv.update_queue_item(
                    thread.thread_id, item.queue_id, text=text, runtime=runtime,
                    capability_refs=capability_refs, runtime_invocation=invocation,
                    expected_revision=expected,
                )
            except (LookupError, ValueError) as exc:
                raise _failed(command, "conversation.queue.update_conflict", str(exc),
                              ErrorCategory.CONFLICT) from exc
            event.payload.update({"text": updated.text, "runtime": updated.runtime,
                "capability_refs": updated.capability_refs,
                "runtime_invocation": updated.runtime_invocation,
                "queue_revision": self._manager.conv.get_state(thread.thread_id).queue_revision})

        return CommandPlan(events=[event],
            receipt=_receipt(command, ev.AGGREGATE_THREAD, thread.thread_id), local_commit=_update)

    def _plan_queue_delete(self, command: Any) -> CommandPlan:
        thread = self._require_thread(command)
        item = self._queue_item(command, thread.thread_id)
        try:
            self._manager.conv.cancel_queue_item(thread.thread_id, item.queue_id)
        except ValueError as exc:
            raise _failed(
                command, "conversation.queue.delete_conflict", str(exc),
                ErrorCategory.CONFLICT,
            ) from exc
        return CommandPlan(
            events=[ev.thread_event(thread.thread_id, ev.EV_QUEUE_DELETED, {
                "queue_id": item.queue_id,
                "queue_revision": self._manager.conv.get_state(
                    thread.thread_id).queue_revision,
            }, actor_id=command.actor.id, command_id=command.command_id,
                correlation_id=correlation_id_of(command),
                idempotency_key=command.idempotency_key)],
            receipt=_receipt(command, ev.AGGREGATE_THREAD, thread.thread_id),
        )

    def _plan_queue_reorder(self, command: Any) -> CommandPlan:
        thread = self._require_thread(command)
        queue_ids = [str(value) for value in command.payload.get("queue_ids") or []]
        expected = command.payload.get("expected_revision")
        try:
            items = self._manager.conv.reorder_queue(
                thread.thread_id,
                queue_ids,
                expected_revision=int(expected) if expected is not None else None,
            )
        except (TypeError, ValueError) as exc:
            raise _failed(
                command, "conversation.queue.reorder_conflict", str(exc),
                ErrorCategory.CONFLICT,
            ) from exc
        return CommandPlan(
            events=[ev.thread_event(thread.thread_id, ev.EV_QUEUE_REORDERED, {
                "queue_ids": [item.queue_id for item in items],
                "queue_revision": self._manager.conv.get_state(
                    thread.thread_id).queue_revision,
            }, actor_id=command.actor.id, command_id=command.command_id,
                correlation_id=correlation_id_of(command),
                idempotency_key=command.idempotency_key)],
            receipt=_receipt(command, ev.AGGREGATE_THREAD, thread.thread_id),
        )

    def _plan_queue_pause(self, command: Any) -> CommandPlan:
        thread = self._require_thread(command)
        reason = str(command.payload.get("reason") or "user_paused")
        state = self._manager.conv.pause_queue(thread.thread_id, reason)
        return CommandPlan(
            events=[ev.thread_event(thread.thread_id, ev.EV_QUEUE_PAUSED, {
                "reason": reason,
                "queue_revision": state.queue_revision,
            }, actor_id=command.actor.id, command_id=command.command_id,
                correlation_id=correlation_id_of(command),
                idempotency_key=command.idempotency_key)],
            receipt=_receipt(command, ev.AGGREGATE_THREAD, thread.thread_id),
        )

    def _plan_queue_resume(self, command: Any) -> CommandPlan:
        thread = self._require_thread(command)
        thread_state = self._manager.conv.get_state(thread.thread_id)
        if thread_state.status == "archived":
            raise _failed(
                command,
                "conversation.thread.archived",
                "archived thread cannot resume its message queue",
                ErrorCategory.STATE,
            )
        state = self._manager.conv.resume_queue(thread.thread_id)
        executor = self._executor

        async def _resume() -> SideEffectResult:
            await executor.start_next_queued(thread.thread_id)
            return SideEffectResult()

        return CommandPlan(
            events=[ev.thread_event(thread.thread_id, ev.EV_QUEUE_RESUMED, {
                "queue_revision": state.queue_revision,
            }, actor_id=command.actor.id, command_id=command.command_id,
                correlation_id=correlation_id_of(command),
                idempotency_key=command.idempotency_key)],
            receipt=_receipt(command, ev.AGGREGATE_THREAD, thread.thread_id),
            side_effect=_resume,
        )

    def _plan_queue_steer(self, command: Any) -> CommandPlan:
        thread = self._require_thread(command)
        item = self._queue_item(command, thread.thread_id)
        if item.status == "dispatching":
            raise _failed(
                command,
                "conversation.queue.dispatching",
                "队列消息正在发送，不能再用于引导",
                ErrorCategory.CONFLICT,
            )
        state = self._manager.conv.get_state(thread.thread_id)
        expected_turn_id = str(
            command.payload.get("expected_turn_id") or ""
        ).strip()
        if not expected_turn_id or expected_turn_id != state.running_turn_id:
            raise _failed(
                command, "conversation.turn.expected_mismatch",
                "执行中的 Turn 已变化，请刷新后重新引导",
                ErrorCategory.CONFLICT,
            )
        if item.attachments or item.capability_refs:
            raise _failed(
                command, "conversation.queue.steer_text_only",
                "含附件或上下文引用的队列消息需要作为下一轮执行",
                ErrorCategory.VALIDATION,
            )
        executor = self._executor

        async def _steer() -> SideEffectResult:
            try:
                receipt = await executor.steer(
                    thread.thread_id,
                    item.text,
                    expected_turn_id=expected_turn_id,
                    client_message_id=item.client_message_id or item.queue_id,
                )
            except RuntimeError as exc:
                return SideEffectResult(
                    error=make_error(
                        "conversation.turn.expected_mismatch", str(exc),
                        ErrorCategory.CONFLICT,
                        correlation_id=correlation_id_of(command),
                    ),
                    state=ReceiptState.FAILED,
                )
            if receipt.error is not None:
                return SideEffectResult(error=receipt.error, state=ReceiptState.FAILED)
            manager = self._manager
            manager.conv.cancel_queue_item(thread.thread_id, item.queue_id)
            revision = manager.conv.get_state(thread.thread_id).queue_revision
            return SideEffectResult(events=[
                ev.thread_event(thread.thread_id, ev.EV_TURN_STEERED, {
                    "turn_id": expected_turn_id,
                    "text": item.text,
                    "client_message_id": item.client_message_id or item.queue_id,
                    "queue_id": item.queue_id,
                }, actor_id=command.actor.id, command_id=command.command_id,
                    correlation_id=correlation_id_of(command)),
                ev.thread_event(thread.thread_id, ev.EV_QUEUE_DELETED, {
                    "queue_id": item.queue_id,
                    "reason": "steered",
                    "queue_revision": revision,
                }, actor_id=command.actor.id, command_id=command.command_id,
                    correlation_id=correlation_id_of(command)),
            ])

        return CommandPlan(
            events=[ev.thread_event(thread.thread_id, "core.queue.steer_requested", {
                "queue_id": item.queue_id,
                "turn_id": expected_turn_id,
            }, actor_id=command.actor.id, command_id=command.command_id,
                correlation_id=correlation_id_of(command),
                idempotency_key=command.idempotency_key)],
            receipt=_receipt(command, ev.AGGREGATE_THREAD, thread.thread_id),
            side_effect=_steer,
        )

    def _plan_turn_interrupt(self, command: Any) -> CommandPlan:
        thread = self._require_thread(command)
        state = self._manager.conv.get_state(thread.thread_id)
        if not state.running_turn_id:
            raise _failed(command, "conversation.turn.not_running",
                          "当前没有执行中的 Turn 可以 interrupt",
                          ErrorCategory.STATE)
        expected_turn_id = str(
            command.payload.get("expected_turn_id") or "").strip()
        if not expected_turn_id:
            raise _failed(
                command,
                "conversation.turn.expected_turn_required",
                "中断请求必须携带当前执行中的 expected_turn_id",
                ErrorCategory.VALIDATION,
            )
        if expected_turn_id != state.running_turn_id:
            raise _failed(
                command,
                "conversation.turn.expected_mismatch",
                "执行中的 Turn 已变化，请刷新后重试",
                ErrorCategory.CONFLICT,
                state=ReceiptState.CONFLICT,
            )
        target_turn_id = state.running_turn_id
        executor = self._executor
        manager = self._manager

        async def _interrupt() -> SideEffectResult:
            try:
                receipt = await executor.interrupt(thread.thread_id)
            except LookupError as exc:
                return SideEffectResult(
                    error=make_error(
                        "conversation.turn.interrupt_unavailable",
                        str(exc),
                        ErrorCategory.STATE,
                        correlation_id=correlation_id_of(command),
                    ),
                    state=ReceiptState.FAILED,
                )
            if receipt.error is not None:
                return SideEffectResult(error=receipt.error,
                                        state=ReceiptState.FAILED)
            # Structured runtimes normally emit their own interrupted event.
            # Keep one confirmed fallback for runtimes that only acknowledge
            # the interrupt receipt, and never mark the turn before that ack.
            current = manager.conv.get_state(thread.thread_id)
            if current.running_turn_id != target_turn_id:
                return SideEffectResult()
            return SideEffectResult(events=[ev.thread_event(
                thread.thread_id, ev.EV_TURN_INTERRUPTED, {
                    "turn_id": target_turn_id,
                    "phase": "runtime_confirmed",
                }, actor_id=command.actor.id,
                command_id=command.command_id,
                correlation_id=correlation_id_of(command))])

        return CommandPlan(
            events=[ev.thread_event(thread.thread_id,
                                    "core.turn.interrupt_requested", {
                "turn_id": target_turn_id,
                "phase": "requested",
            }, actor_id=command.actor.id,
                command_id=command.command_id,
                correlation_id=correlation_id_of(command),
                idempotency_key=command.idempotency_key)],
            receipt=_receipt(command, ev.AGGREGATE_THREAD, thread.thread_id),
            side_effect=_interrupt,
        )

    # -- Artifact / 交互 -------------------------------------------------------------

    def _plan_artifact_attach(self, command: Any) -> CommandPlan:
        thread = self._require_thread(command)
        payload = dict(command.payload)
        name = str(payload.get("name") or "").strip()
        if not name:
            raise _failed(command, "conversation.artifact.name_required",
                          "artifact.attach requires payload.name",
                          ErrorCategory.VALIDATION)
        manager = self._manager

        async def _attach() -> SideEffectResult:
            try:
                artifact = manager.attach_artifact(
                    thread.thread_id,
                    name=name,
                    content_base64=str(payload.get("content_base64") or ""),
                    path=str(payload.get("path") or ""),
                    kind=str(payload.get("kind") or "conversation.upload"),
                    media_type=str(payload.get("media_type") or ""),
                    run_id=str(payload.get("run_id") or "") or None,
                )
            except (ConversationError, ValueError, OSError) as exc:
                return SideEffectResult(
                    error=make_error("conversation.artifact.invalid", str(exc),
                                     ErrorCategory.VALIDATION,
                                     correlation_id=correlation_id_of(command)),
                    state=ReceiptState.FAILED)
            return SideEffectResult(
                output={
                    "sha256": artifact.sha256,
                    "name": artifact.name,
                    "kind": artifact.kind,
                    "media_type": artifact.media_type,
                    "size": artifact.size,
                },
                events=[ev.thread_event(
                    thread.thread_id, ev.EV_ARTIFACT_ATTACHED, {
                    "sha256": artifact.sha256,
                    "name": artifact.name,
                    "kind": artifact.kind,
                    "media_type": artifact.media_type,
                    "size": artifact.size,
                    "run_id": artifact.run_id,
                }, actor_id=command.actor.id,
                command_id=command.command_id,
                correlation_id=correlation_id_of(command))],
            )

        return CommandPlan(
            events=[ev.thread_event(thread.thread_id, "core.artifact.attach_requested", {
                "name": name,
            }, actor_id=command.actor.id,
                command_id=command.command_id,
                correlation_id=correlation_id_of(command),
                idempotency_key=command.idempotency_key)],
            receipt=_receipt(command, ev.AGGREGATE_THREAD, thread.thread_id),
            side_effect=_attach,
        )


    def _plan_approval_inject(self, command: Any) -> CommandPlan:
        """CU / fixture: inject one or more synthetic approval requests.

        Gated by ``MUTEKI_ALLOW_APPROVAL_FIXTURES=1`` (same posture as
        ``conversation.user_input.inject``).
        """
        if os.environ.get("MUTEKI_ALLOW_APPROVAL_FIXTURES", "").strip() != "1":
            raise _failed(
                command,
                "conversation.approval.fixture_disabled",
                "Set MUTEKI_ALLOW_APPROVAL_FIXTURES=1 to inject fixtures",
                ErrorCategory.PERMISSION,
            )
        thread = self._require_thread(command)
        payload = dict(command.payload or {})
        raw_items = payload.get("approvals")
        if raw_items is None:
            raw_items = [payload]
        if not isinstance(raw_items, list) or not raw_items:
            raise _failed(
                command, "conversation.approval.invalid",
                "inject 需要 approvals[] 或单个 approval 载荷",
                ErrorCategory.VALIDATION)
        from muteki.conversation.approval_queue import normalize_approval_payload
        events = []
        for index, item in enumerate(raw_items):
            if not isinstance(item, dict):
                raise _failed(
                    command, "conversation.approval.invalid",
                    f"approvals[{index}] 必须是对象",
                    ErrorCategory.VALIDATION)
            try:
                row = normalize_approval_payload(item, default_status="pending")
            except ValueError as exc:
                raise _failed(
                    command, "conversation.approval.invalid",
                    str(exc), ErrorCategory.VALIDATION) from exc
            events.append(ev.thread_event(
                thread.thread_id, ev.EV_APPROVAL_REQUESTED, row,
                actor_id=command.actor.id,
                command_id=command.command_id,
                correlation_id=correlation_id_of(command),
                idempotency_key=(
                    f"{command.idempotency_key}:inj:{row['approval_id']}"
                ),
            ))
        expire_id = str(payload.get("expire_approval_id") or "").strip()
        if expire_id:
            events.append(ev.thread_event(
                thread.thread_id, ev.EV_APPROVAL_REQUESTED, {
                    "approval_id": expire_id,
                    "status": "expired",
                    "reason": str(payload.get("expire_reason") or "请求已过期"),
                    "title": str(payload.get("expire_title") or "已过期的审批"),
                    "approval_kind": "unknown",
                },
                actor_id=command.actor.id,
                command_id=command.command_id,
                correlation_id=correlation_id_of(command),
                idempotency_key=f"{command.idempotency_key}:exp:{expire_id}",
            ))
        return CommandPlan(
            events=events,
            receipt=_receipt(command, ev.AGGREGATE_THREAD, thread.thread_id),
        )

    def _plan_approval_resolve(self, command: Any) -> CommandPlan:
        thread = self._require_thread(command)
        payload = dict(command.payload)
        try:
            approval = ApprovalDecision.from_payload(payload)
        except ValueError as exc:
            raise _failed(command, "conversation.approval.invalid",
                          str(exc), ErrorCategory.VALIDATION) from exc
        state = self._manager.conv.get_state(thread.thread_id)
        from muteki.conversation.approval_queue import (
            hydrate_approvals,
            lookup_approval,
        )
        queue = hydrate_approvals(state.pending_approvals, state.pending_approval)
        pending = lookup_approval(queue, approval.approval_id)
        if not queue:
            raise _failed(command, "conversation.approval.not_pending",
                          "当前没有待处理的 approval", ErrorCategory.STATE)
        if pending is None:
            raise _failed(
                command, "conversation.approval.mismatch",
                f"approval {approval.approval_id} 不在待审队列中",
                ErrorCategory.CONFLICT)
        if str(pending.get("status") or "pending") == "expired":
            raise _failed(
                command, "conversation.approval.expired",
                f"approval {approval.approval_id} 已过期，不能再决定",
                ErrorCategory.STATE)
        if pending.get("status") == "resolving":
            raise _failed(command, "conversation.approval.resolving",
                          "该审批正在投递，请恢复原决定的回执", ErrorCategory.CONFLICT)
        executor = self._executor
        decision_payload = approval.to_payload()
        if approval.option_id:
            offered = next((row for row in pending.get("options", [])
                            if str(row.get("option_id") or row.get("optionId") or "") == approval.option_id), None)
            expected_kind = ("allow" if approval.allowed else "reject") + (
                "_always" if approval.scope.value == "session" else "_once")
            if offered is None or offered.get("kind") != expected_kind:
                raise _failed(command, "conversation.approval.option_invalid",
                              "该原生选项不属于当前请求或授权范围不匹配", ErrorCategory.VALIDATION)

        def _claim() -> None:
            current = self._manager.conv.get_state(thread.thread_id)
            row = lookup_approval(hydrate_approvals(current.pending_approvals, current.pending_approval), approval.approval_id)
            if row is None or str(row.get("status") or "pending") != "pending":
                raise _failed(command, "conversation.approval.resolving",
                              "审批已失效或正在投递，请刷新原回执", ErrorCategory.CONFLICT)

        async def _resolve() -> SideEffectResult:
            # #122: deliver to the original Runtime session BEFORE consuming the
            # pending approval. On failure keep the card so the user can retry
            # or cancel instead of losing control after a resolved-but-undelivered
            # event.
            try:
                await executor.resolve_approval(
                    thread.thread_id,
                    approval.approval_id,
                    approval.choice.value,
                    note=approval.note,
                    scope=approval.scope.value,
                    **({"option_id": approval.option_id} if approval.option_id else {}),
                )
            except LookupError as exc:
                return SideEffectResult(
                    error=make_error(
                        "conversation.approval.delivery_failed",
                        str(exc),
                        ErrorCategory.STATE,
                        correlation_id=correlation_id_of(command),
                        recovery_hint="审批未能送达原会话，可重试或取消并释放占用",
                        retryable=True,
                    ),
                    state=ReceiptState.FAILED,
                )
            except ControlDeliveryError as exc:
                return SideEffectResult(
                    error=make_error(
                        "conversation.approval.delivery_unknown" if exc.delivery_unknown else "conversation.approval.delivery_failed", str(exc),
                        ErrorCategory.RUNTIME,
                        correlation_id=correlation_id_of(command),
                        retryable=not exc.delivery_unknown,
                    ).model_copy(update={"detail": {"runtime_code": exc.runtime_code,
                                                   "delivery_unknown": exc.delivery_unknown}}),
                    state=ReceiptState.FAILED,
                )
            except Exception as exc:  # noqa: BLE001 — typed failed receipt
                return SideEffectResult(
                    error=make_error(
                        "conversation.approval.delivery_failed",
                        f"{type(exc).__name__}: {exc}",
                        ErrorCategory.INTERNAL,
                        correlation_id=correlation_id_of(command),
                        recovery_hint="审批投递失败，待审请求仍保留，可重试或取消",
                        retryable=True,
                    ),
                    state=ReceiptState.FAILED,
                )
            return SideEffectResult(events=[ev.thread_event(
                thread.thread_id, ev.EV_APPROVAL_RESOLVED,
                decision_payload, actor_id=command.actor.id,
                command_id=command.command_id,
                correlation_id=correlation_id_of(command),
                idempotency_key=command.idempotency_key,
            )])

        return CommandPlan(
            events=[ev.thread_event(
                thread.thread_id, "core.approval.resolve_requested",
                decision_payload, actor_id=command.actor.id,
                command_id=command.command_id,
                correlation_id=correlation_id_of(command),
                idempotency_key=command.idempotency_key)],
            receipt=_receipt(command, ev.AGGREGATE_THREAD, thread.thread_id),
            side_effect=_resolve,
            local_commit=_claim,
            failure_events=[ev.thread_event(thread.thread_id, "core.approval.delivery_failed",
                {"approval_id": approval.approval_id}, command_id=command.command_id)],
        )

    def _plan_user_input_resolve(self, command: Any) -> CommandPlan:
        thread = self._require_thread(command)
        payload = dict(command.payload)
        request_id = str(payload.get("request_id") or "").strip()
        decision = str(payload.get("decision") or "submit").strip().lower() or "submit"
        text = str(payload.get("text") or "")
        raw_answers = payload.get("answers")
        state = self._manager.conv.get_state(thread.thread_id)
        pending = normalize_pending_user_input(dict(state.pending_user_input or {}))
        if not state.pending_user_input:
            raise _failed(command, "conversation.user_input.not_pending",
                          "当前没有待处理的 user input", ErrorCategory.STATE)
        if pending.get("request_id") and pending["request_id"] != request_id:
            raise _failed(command, "conversation.user_input.mismatch",
                          f"待处理 user input 为 {pending['request_id']}",
                          ErrorCategory.CONFLICT)
        if pending.get("status") == "resolving":
            raise _failed(command, "conversation.user_input.resolving",
                          "该回答正在投递，请恢复原命令回执", ErrorCategory.CONFLICT)
        answers: dict[str, Any] = {}
        if isinstance(raw_answers, dict) and raw_answers:
            answers = dict(raw_answers)
        elif text and decision != "cancel":
            answers = expand_legacy_text_answers(pending, text)
        try:
            validated = validate_user_input_answers(
                pending, answers, decision=decision)
        except UserInputValidationError as exc:
            raise _failed(command, exc.code, exc.message, ErrorCategory.VALIDATION) from exc
        flat_text = text or flatten_answers_text(validated)
        executor = self._executor

        def _claim() -> None:
            current = self._manager.conv.get_state(thread.thread_id).pending_user_input or {}
            if current.get("request_id") != request_id or current.get("status") == "resolving":
                raise _failed(command, "conversation.user_input.resolving",
                              "问题已失效或正在投递，请刷新原回执", ErrorCategory.CONFLICT)

        resolved_event = ev.thread_event(thread.thread_id, ev.EV_USER_INPUT_RESOLVED, {
            "request_id": request_id,
            "decision": decision,
            "answers": validated,
            "text": flat_text,
        }, actor_id=command.actor.id,
            command_id=command.command_id,
            correlation_id=correlation_id_of(command),
            idempotency_key=command.idempotency_key)

        async def _resolve() -> SideEffectResult:
            try:
                await executor.resolve_user_input(
                    thread.thread_id, request_id, flat_text,
                    answers=validated, decision=decision,
                )
            except ControlDeliveryError as exc:
                return SideEffectResult(error=make_error(
                    "conversation.user_input.delivery_unknown" if exc.delivery_unknown else "conversation.user_input.delivery_failed",
                    str(exc), ErrorCategory.RUNTIME, retryable=not exc.delivery_unknown,
                    correlation_id=correlation_id_of(command)).model_copy(update={"detail": {
                        "runtime_code": exc.runtime_code, "delivery_unknown": exc.delivery_unknown}}),
                    state=ReceiptState.FAILED)
            return SideEffectResult(events=[resolved_event])

        return CommandPlan(
            events=[ev.thread_event(thread.thread_id, "core.user_input.responding", {
                "request_id": request_id,
                "decision": decision,
            }, actor_id=command.actor.id, command_id=command.command_id,
                correlation_id=correlation_id_of(command),
                idempotency_key=command.idempotency_key)],
            receipt=_receipt(command, ev.AGGREGATE_THREAD, thread.thread_id),
            side_effect=_resolve,
            local_commit=_claim,
            failure_events=[ev.thread_event(thread.thread_id, "core.user_input.delivery_failed",
                {"request_id": request_id}, command_id=command.command_id)],
        )

    def _plan_user_input_inject(self, command: Any) -> CommandPlan:
        """Dev/CU fixture: inject a multi-question pending user-input card.

        Gated by ``MUTEKI_ALLOW_USER_INPUT_FIXTURES=1``. Creates a synthetic
        running turn when needed so the Conversation deck shows the card.
        """
        if os.environ.get("MUTEKI_ALLOW_USER_INPUT_FIXTURES", "").strip() != "1":
            raise _failed(
                command,
                "conversation.user_input.fixture_disabled",
                "Set MUTEKI_ALLOW_USER_INPUT_FIXTURES=1 to inject fixtures",
                ErrorCategory.PERMISSION,
            )
        thread = self._require_thread(command)
        payload = dict(command.payload)
        request_id = str(payload.get("request_id") or new_id("req")).strip()
        raw_questions = payload.get("questions")
        if isinstance(raw_questions, list) and raw_questions:
            questions = list(raw_questions)
        else:
            questions = c22_fixture_questions()
        pending = normalize_pending_user_input({
            "request_id": request_id,
            "user_input_kind": str(payload.get("user_input_kind") or "fixture"),
            "title": str(payload.get("title") or payload.get("message") or ""),
            "message": str(payload.get("message") or payload.get("title") or ""),
            "questions": questions,
            "fixture": True,
            "native": dict(payload.get("native") or {}),
        })
        state = self._manager.conv.get_state(thread.thread_id)
        turn_id = str(state.running_turn_id or "")
        events = []
        if not turn_id:
            turn_id = new_id("turn")
            turns = self._manager.conv.list_current_turns(thread.thread_id)
            seq = (turns[-1].seq + 1) if turns else 1
            turn = TurnRecord(
                turn_id=turn_id,
                thread_id=thread.thread_id,
                command_id=command.command_id,
                idempotency_key=command.idempotency_key or command.command_id,
                seq=seq,
                kind=TURN_KIND_MESSAGE,
                text=str(payload.get("turn_text") or "[c22 fixture]"),
                status=TURN_RUNNING,
            )
            self._manager.conv.save_turn(turn)
            events.append(ev.thread_event(
                thread.thread_id, ev.EV_TURN_STARTED, {
                    "turn_id": turn_id,
                    "fixture": True,
                }, actor_id=command.actor.id,
                command_id=command.command_id,
                correlation_id=correlation_id_of(command),
            ))
        pending["turn_id"] = turn_id
        events.append(ev.thread_event(
            thread.thread_id, ev.EV_USER_INPUT_REQUESTED, pending,
            actor_id=command.actor.id,
            command_id=command.command_id,
            correlation_id=correlation_id_of(command),
            idempotency_key=command.idempotency_key,
        ))
        manager = self._manager

        async def _register() -> SideEffectResult:
            manager.register_user_input_fixture(
                thread.thread_id, request_id, pending=pending)
            return SideEffectResult(output={
                "request_id": request_id,
                "turn_id": turn_id,
                "questions": pending.get("questions") or [],
            })

        return CommandPlan(
            events=events,
            receipt=_receipt(command, ev.AGGREGATE_THREAD, thread.thread_id),
            side_effect=_register,
        )

    def _plan_memory_record(self, command: Any) -> CommandPlan:
        thread = self._require_thread(command)
        payload = dict(command.payload)
        content = str(payload.get("content") or "").strip()
        if not content:
            raise _failed(command, "conversation.memory.content_required",
                          "memory.record requires payload.content",
                          ErrorCategory.VALIDATION)
        if payload.get("consent") is not True:
            raise _failed(command, "conversation.memory.consent_required",
                          "写入长期记忆需要用户明确允许",
                          ErrorCategory.PERMISSION)
        memory_id = str(payload.get("memory_id") or new_id("mem"))
        manager = self._manager

        async def _record() -> SideEffectResult:
            receipt = await manager.record_memory(
                thread.thread_id,
                content,
                kind=str(payload.get("kind") or "note"),
                consent=True,
                actor_id=command.actor.id,
                memory_id=memory_id,
            )
            return SideEffectResult(
                output={"memory_id": memory_id,
                        "graph_stream_seq": receipt.stream_seq},
                events=[ev.thread_event(
                    thread.thread_id,
                    "core.memory.recorded",
                    {"memory_id": memory_id,
                     "kind": str(payload.get("kind") or "note")},
                    actor_id=command.actor.id,
                    command_id=command.command_id,
                    correlation_id=correlation_id_of(command),
                )],
            )

        return CommandPlan(
            events=[ev.thread_event(
                thread.thread_id,
                "core.memory.record_requested",
                {"memory_id": memory_id},
                actor_id=command.actor.id,
                command_id=command.command_id,
                correlation_id=correlation_id_of(command),
                idempotency_key=command.idempotency_key,
            )],
            receipt=_receipt(command, ev.AGGREGATE_THREAD, thread.thread_id),
            side_effect=_record,
        )

    def _plan_memory_delete(self, command: Any) -> CommandPlan:
        thread = self._require_thread(command)
        payload = dict(command.payload)
        memory_id = str(payload.get("memory_id") or "").strip()
        if not memory_id:
            raise _failed(command, "conversation.memory.id_required",
                          "memory.delete requires payload.memory_id",
                          ErrorCategory.VALIDATION)
        if payload.get("confirm") is not True:
            raise _failed(command, "conversation.memory.confirm_required",
                          "删除长期记忆需要明确确认",
                          ErrorCategory.VALIDATION)
        manager = self._manager

        async def _delete() -> SideEffectResult:
            receipt = await manager.delete_memory(
                thread.thread_id,
                memory_id,
                reason=str(payload.get("reason") or "user_requested"),
                actor_id=command.actor.id,
            )
            return SideEffectResult(
                output={"memory_id": memory_id,
                        "graph_stream_seq": receipt.stream_seq},
                events=[ev.thread_event(
                    thread.thread_id,
                    "core.memory.deleted",
                    {"memory_id": memory_id},
                    actor_id=command.actor.id,
                    command_id=command.command_id,
                    correlation_id=correlation_id_of(command),
                )],
            )

        return CommandPlan(
            events=[ev.thread_event(
                thread.thread_id,
                "core.memory.delete_requested",
                {"memory_id": memory_id},
                actor_id=command.actor.id,
                command_id=command.command_id,
                correlation_id=correlation_id_of(command),
                idempotency_key=command.idempotency_key,
            )],
            receipt=_receipt(command, ev.AGGREGATE_THREAD, thread.thread_id),
            side_effect=_delete,
        )

    def _plan_plan_inject(self, command: Any) -> CommandPlan:
        """Synthetic / fixture plan upsert for CU and adapter-less demos."""
        thread = self._require_thread(command)
        payload = dict(command.payload)
        if payload.get("unsupported") is True or str(
            payload.get("phase") or ""
        ) == "unsupported":
            event_type = ev.EV_PLAN_CLEARED
            body = {
                "unsupported": True,
                "unsupported_reason": str(
                    payload.get("unsupported_reason")
                    or payload.get("reason")
                    or "当前 Runtime 不支持结构化计划事件"
                ),
                "phase": "unsupported",
            }
        else:
            event_type = ev.EV_PLAN_UPDATED
            body = {
                **payload,
                "source": str(payload.get("source") or "fixture"),
                "patch": bool(payload.get("patch") or payload.get("delta")),
            }
            if not body.get("tasks") and not body.get("entries") and not body.get("steps"):
                raise _failed(
                    command,
                    "conversation.plan.tasks_required",
                    "plan.inject requires payload.tasks (or unsupported=true)",
                    ErrorCategory.VALIDATION,
                )
        return CommandPlan(
            events=[ev.thread_event(
                thread.thread_id,
                event_type,
                body,
                actor_id=command.actor.id,
                command_id=command.command_id,
                correlation_id=correlation_id_of(command),
                idempotency_key=command.idempotency_key,
            )],
            receipt=_receipt(command, ev.AGGREGATE_THREAD, thread.thread_id),
        )

    def _plan_agents_inject(self, command: Any) -> CommandPlan:
        """Synthetic / fixture agent tree for CU (C21 parent/child ownership)."""
        thread = self._require_thread(command)
        payload = dict(command.payload)
        events = []
        if payload.get("unsupported") is True or str(
            payload.get("phase") or ""
        ) == "unsupported":
            event_type = ev.EV_AGENT_CLEARED
            body = {
                "unsupported": True,
                "unsupported_reason": str(
                    payload.get("unsupported_reason")
                    or payload.get("reason")
                    or "当前 Runtime 未上报委派 Agent 事件"
                ),
                "tool_activity_summary": str(
                    payload.get("tool_activity_summary")
                    or payload.get("activity_summary")
                    or ""
                ).strip() or None,
                "phase": "unsupported",
            }
            events.append(ev.thread_event(
                thread.thread_id,
                event_type,
                body,
                actor_id=command.actor.id,
                command_id=command.command_id,
                correlation_id=correlation_id_of(command),
                idempotency_key=command.idempotency_key,
            ))
        else:
            event_type = ev.EV_AGENT_UPDATED
            body = {
                **{k: v for k, v in payload.items() if k != "tools"},
                "source": str(payload.get("source") or "fixture"),
                "patch": bool(payload.get("patch") or payload.get("delta")),
            }
            agents = body.get("agents") or body.get("nodes")
            if not isinstance(agents, list) or not agents:
                raise _failed(
                    command,
                    "conversation.agents.agents_required",
                    "agents.inject requires payload.agents (or unsupported=true)",
                    ErrorCategory.VALIDATION,
                )
            events.append(ev.thread_event(
                thread.thread_id,
                event_type,
                body,
                actor_id=command.actor.id,
                command_id=command.command_id,
                correlation_id=correlation_id_of(command),
                idempotency_key=command.idempotency_key,
            ))
        # Optional mixed tool trajectory for CU (shell must stay off Agents tree).
        tools = payload.get("tools")
        if isinstance(tools, list):
            turn_id = str(payload.get("turn_id") or "turn-c21-fixture")
            for index, tool in enumerate(tools):
                if not isinstance(tool, dict):
                    continue
                call_id = str(
                    tool.get("call_id")
                    or tool.get("tool_call_id")
                    or tool.get("id")
                    or f"tool-c21-{index + 1}"
                )
                started = {
                    "turn_id": str(tool.get("turn_id") or turn_id),
                    "call_id": call_id,
                    "tool": str(tool.get("tool") or tool.get("name") or "tool"),
                    "input": tool.get("input") or tool.get("arguments") or {},
                }
                if tool.get("parent_tool_use_id") or tool.get("parent_id"):
                    started["parent_tool_use_id"] = str(
                        tool.get("parent_tool_use_id") or tool.get("parent_id")
                    )
                if tool.get("is_agent") is True:
                    started["is_agent"] = True
                events.append(ev.thread_event(
                    thread.thread_id,
                    ev.EV_TOOL_STARTED,
                    started,
                    actor_id=command.actor.id,
                    command_id=command.command_id,
                    correlation_id=correlation_id_of(command),
                    idempotency_key=f"{command.idempotency_key}:tool:{call_id}:start",
                ))
                if tool.get("status") in {"completed", "failed"} or tool.get("output") or tool.get("error"):
                    completed = {
                        "turn_id": started["turn_id"],
                        "call_id": call_id,
                        "output": tool.get("output") or tool.get("result") or "",
                        "is_error": bool(
                            tool.get("is_error")
                            or tool.get("status") == "failed"
                            or tool.get("error")
                        ),
                    }
                    if tool.get("error"):
                        completed["error"] = tool.get("error")
                    events.append(ev.thread_event(
                        thread.thread_id,
                        ev.EV_TOOL_COMPLETED,
                        completed,
                        actor_id=command.actor.id,
                        command_id=command.command_id,
                        correlation_id=correlation_id_of(command),
                        idempotency_key=f"{command.idempotency_key}:tool:{call_id}:done",
                    ))
        return CommandPlan(
            events=events,
            receipt=_receipt(command, ev.AGGREGATE_THREAD, thread.thread_id),
        )

    def _plan_plan_amend(self, command: Any) -> CommandPlan:
        """User proposes plan modifications (steer when running; else overlay)."""
        thread = self._require_thread(command)
        payload = dict(command.payload)
        text = str(payload.get("text") or payload.get("amendment") or "").strip()
        amendments = payload.get("amendments")
        if not text and isinstance(amendments, list):
            parts = []
            for item in amendments:
                if isinstance(item, str) and item.strip():
                    parts.append(item.strip())
                elif isinstance(item, dict):
                    part = str(
                        item.get("text")
                        or item.get("title")
                        or item.get("change")
                        or ""
                    ).strip()
                    if part:
                        parts.append(part)
            text = "；".join(parts)
        if not text:
            raise _failed(
                command,
                "conversation.plan.amend_required",
                "plan.amend requires payload.text or amendments[]",
                ErrorCategory.VALIDATION,
            )
        state = self._manager.conv.get_state(thread.thread_id)
        plan = state.plan
        plan_revision = int(
            payload.get("plan_revision")
            or (plan.revision if plan is not None else 0)
            or 0
        )
        steer_text = (
            f"请根据我对当前执行计划（revision={plan_revision}）的修改建议调整："
            f"{text}"
        )
        overlay = {
            "text": text,
            "status": "unconfirmed",
            "plan_revision": plan_revision,
            "requested_at": command.command_id,
        }
        events = [
            ev.thread_event(
                thread.thread_id,
                ev.EV_PLAN_UPDATED,
                {
                    "revision": max(plan_revision, int(plan.revision if plan else 0)) + 1,
                    "phase": (
                        "awaiting_decision"
                        if plan is not None and plan.phase == "proposed"
                        else (plan.phase if plan is not None else "proposed")
                    ),
                    "patch": True,
                    "tasks": [
                        task.model_dump(mode="json")
                        for task in (plan.tasks if plan is not None else [])
                    ],
                    "pending_amendment": overlay,
                    "awaiting": {
                        "kind": "plan_accept",
                        "summary": "用户已提出计划修改，等待 Runtime 确认",
                    },
                    "source": plan.source if plan is not None else "fixture",
                    "last_change_summary": "用户提出计划修改（未获 Runtime 确认）",
                    "title": plan.title if plan is not None else None,
                },
                actor_id=command.actor.id,
                command_id=command.command_id,
                correlation_id=correlation_id_of(command),
                idempotency_key=command.idempotency_key,
            ),
        ]
        executor = self._executor
        expected_turn_id = state.running_turn_id
        can_steer = bool(expected_turn_id)

        async def _amend() -> SideEffectResult:
            if not can_steer:
                return SideEffectResult(output={
                    "pending_amendment": overlay,
                    "steered": False,
                })
            client_message_id = str(
                payload.get("client_message_id") or command.command_id
            )
            try:
                receipt = await executor.steer(
                    thread.thread_id,
                    steer_text,
                    expected_turn_id=expected_turn_id,
                    client_message_id=client_message_id,
                )
            except RuntimeError as exc:
                return SideEffectResult(
                    error=make_error(
                        "conversation.turn.expected_mismatch",
                        str(exc),
                        ErrorCategory.CONFLICT,
                        correlation_id=correlation_id_of(command),
                    ),
                    state=ReceiptState.FAILED,
                    output={"pending_amendment": overlay, "steered": False},
                )
            if receipt.error is not None:
                return SideEffectResult(
                    error=receipt.error,
                    state=ReceiptState.FAILED,
                    output={"pending_amendment": overlay, "steered": False},
                )
            return SideEffectResult(
                output={"pending_amendment": overlay, "steered": True},
                events=[ev.thread_event(
                    thread.thread_id, ev.EV_TURN_STEERED, {
                        "turn_id": expected_turn_id,
                        "text": steer_text,
                        "client_message_id": client_message_id,
                        "via": "plan.amend",
                    }, actor_id=command.actor.id,
                    command_id=command.command_id,
                    correlation_id=correlation_id_of(command),
                )],
            )

        return CommandPlan(
            events=events,
            receipt=_receipt(command, ev.AGGREGATE_THREAD, thread.thread_id),
            side_effect=_amend,
        )


# ---------------------------------------------------------------------------
# 查询 Handler
# ---------------------------------------------------------------------------


class ConversationThreadViewQueryHandler:
    """conversation.thread.view：Thread 页面快照（对象 + 读模型 + 消息页 + 水位）。"""

    query_types = {"conversation.thread.view"}

    def __init__(self, manager: ConversationManager) -> None:
        self._manager = manager

    async def handle(self, query: Any, ctx: HandlerContext) -> QueryResult:
        thread_id = str(
            query.params.get("thread_id") or query.aggregate_id or "").strip()
        before = query.params.get("before_stream_seq")
        after = query.params.get("after_stream_seq")
        try:
            view = self._manager.thread_view(
                thread_id,
                mark_read=bool(query.params.get("mark_read")),
                messages_limit=query.params.get("messages_limit"),
                before_stream_seq=(
                    int(before) if before not in (None, "") else None
                ),
                after_stream_seq=(
                    int(after) if after not in (None, "") else None
                ),
                include_events=bool(query.params.get("include_events")),
                include_superseded=bool(query.params.get("include_superseded")),
            )
        except ConversationError as exc:
            category = (
                ErrorCategory.NOT_FOUND
                if "unknown" in str(exc).lower()
                else ErrorCategory.VALIDATION
            )
            raise CommandFailed(make_error(
                "conversation.thread.not_found" if category is ErrorCategory.NOT_FOUND
                else "conversation.thread.view_invalid",
                str(exc),
                category, correlation_id=query.query_id)) from exc
        return QueryResult(
            query_id=query.query_id,
            query_type=query.query_type,
            result=view,
        )


class ConversationThreadMessagesQueryHandler:
    """conversation.thread.messages：按 stream_seq 游标分页的消息页。"""

    query_types = {"conversation.thread.messages"}

    def __init__(self, manager: ConversationManager) -> None:
        self._manager = manager

    async def handle(self, query: Any, ctx: HandlerContext) -> QueryResult:
        thread_id = str(
            query.params.get("thread_id") or query.aggregate_id or "").strip()
        before = query.params.get("before_stream_seq")
        after = query.params.get("after_stream_seq")
        around = query.params.get("around_message_id")
        try:
            result = self._manager.messages_page(
                thread_id,
                limit=query.params.get("limit"),
                before_stream_seq=(
                    int(before) if before not in (None, "") else None
                ),
                after_stream_seq=(
                    int(after) if after not in (None, "") else None
                ),
                around_message_id=(
                    str(around) if around not in (None, "") else None
                ),
            )
        except ConversationError as exc:
            raise CommandFailed(make_error(
                "conversation.thread.not_found", str(exc),
                ErrorCategory.NOT_FOUND, correlation_id=query.query_id)) from exc
        except ValueError as exc:
            raise CommandFailed(make_error(
                "conversation.messages.cursor_invalid", str(exc),
                ErrorCategory.VALIDATION,
                correlation_id=query.query_id)) from exc
        return QueryResult(
            query_id=query.query_id,
            query_type=query.query_type,
            result=result,
        )


class ConversationTurnProcessQueryHandler:
    """conversation.turn.process：按需加载单 Turn 的过程/工具事件。"""

    query_types = {"conversation.turn.process"}

    def __init__(self, manager: ConversationManager) -> None:
        self._manager = manager

    async def handle(self, query: Any, ctx: HandlerContext) -> QueryResult:
        thread_id = str(
            query.params.get("thread_id") or query.aggregate_id or "").strip()
        turn_id = str(query.params.get("turn_id") or "").strip()
        limit = query.params.get("limit")
        try:
            result = self._manager.turn_process(
                thread_id,
                turn_id,
                limit=int(limit) if limit not in (None, "") else 2000,
                after_seq=int(query.params.get("after_seq") or 0),
                watermark=int(query.params["watermark"]) if query.params.get("watermark") is not None else None,
            )
        except ConversationError as exc:
            raise CommandFailed(make_error(
                "conversation.turn.not_found", str(exc),
                ErrorCategory.NOT_FOUND, correlation_id=query.query_id)) from exc
        return QueryResult(
            query_id=query.query_id,
            query_type=query.query_type,
            result=result,
        )



class ConversationThreadSearchQueryHandler:
    """conversation.thread.search：跨会话正文全文检索（命中片段 + 消息锚点）。"""

    query_types = {"conversation.thread.search"}

    def __init__(self, manager: ConversationManager) -> None:
        self._manager = manager

    async def handle(self, query: Any, ctx: HandlerContext) -> QueryResult:
        q = str(query.params.get("q") or query.params.get("query") or "").strip()
        project_id = str(query.params.get("project_id") or "").strip()
        include_archived = bool(query.params.get("include_archived"))
        include_superseded = bool(query.params.get("include_superseded"))
        limit = query.params.get("limit")
        try:
            result = self._manager.search_messages(
                q,
                project_id=project_id,
                include_archived=include_archived,
                include_superseded=include_superseded,
                limit=int(limit) if limit not in (None, "") else 30,
                offset=max(0, int(query.params.get("offset") or 0)),
            )
        except ConversationError as exc:
            raise CommandFailed(make_error(
                "conversation.search.failed", str(exc),
                ErrorCategory.VALIDATION,
                correlation_id=query.query_id)) from exc
        return QueryResult(
            query_id=query.query_id,
            query_type=query.query_type,
            result=result,
        )


class ConversationThreadListQueryHandler:
    """conversation.thread.list：Thread 列表 + 读模型状态（unread / running）。"""

    query_types = {"conversation.thread.list"}

    def __init__(self, manager: ConversationManager) -> None:
        self._manager = manager

    async def handle(self, query: Any, ctx: HandlerContext) -> QueryResult:
        project_id = str(query.params.get("project_id") or "").strip()
        threads = self._manager.list_threads(project_id)
        rows = []
        for thread in threads:
            state = self._manager.conv.get_state(thread.thread_id)
            rows.append({
                **thread.model_dump(mode="json"),
                "state": {**state.model_dump(mode="json"),
                          "unread": state.unread},
            })
        return QueryResult(
            query_id=query.query_id,
            query_type=query.query_type,
            result={"threads": rows, "count": len(rows)},
        )


class ConversationProjectDirectoryListQueryHandler:
    """conversation.project.directory_list：列出绑定本机目录的项目。"""

    query_types = {"conversation.project.directory_list"}

    def __init__(self, manager: ConversationManager) -> None:
        self._manager = manager

    async def handle(self, query: Any, ctx: HandlerContext) -> QueryResult:
        from muteki.platform.contracts.objects import Project, Workspace

        latest_workspace: dict[str, Workspace] = {}
        for workspace in self._manager.store.list(Workspace, kind="local"):
            if workspace.project_id:
                latest_workspace[workspace.project_id] = workspace

        rows: list[dict[str, Any]] = []
        for project in self._manager.store.list(Project):
            workspace = latest_workspace.get(project.project_id)
            if workspace is None:
                continue
            rows.append({
                **project.model_dump(mode="json"),
                "workspace_id": workspace.workspace_id,
                "workspace_kind": workspace.kind,
                "root_path": workspace.root_path,
            })
        return QueryResult(
            query_id=query.query_id,
            query_type=query.query_type,
            result={"projects": rows, "count": len(rows)},
        )


class ConversationMemoryQueryHandler:
    """conversation.memory.list：Thread 范围的记忆时间线与检索。"""

    query_types = {"conversation.memory.list"}

    def __init__(self, manager: ConversationManager) -> None:
        self._manager = manager

    async def handle(self, query: Any, ctx: HandlerContext) -> QueryResult:
        thread_id = str(
            query.params.get("thread_id") or query.aggregate_id or "").strip()
        try:
            result = await self._manager.memory_snapshot(
                thread_id,
                include_deleted=bool(query.params.get("include_deleted")),
                query=str(query.params.get("query") or ""),
            )
        except ConversationError as exc:
            raise CommandFailed(make_error(
                "conversation.memory.unavailable", str(exc),
                ErrorCategory.NOT_FOUND,
                correlation_id=query.query_id)) from exc
        return QueryResult(
            query_id=query.query_id,
            query_type=query.query_type,
            result=result,
        )


def register_conversation_handlers(
    api: Any,
    manager: ConversationManager,
    executor: ExternalAgentSessionExecutor,
) -> None:
    """把 CONV-01 的 Command/Query Handler 注册到共享 Command API（幂等）。"""
    known_commands = api.handlers.known_command_types()
    known_queries = api.handlers.known_query_types()
    if "conversation.thread.create" not in known_commands:
        api.register_command(ConversationCommandHandler(manager, executor))
    if "conversation.thread.view" not in known_queries:
        api.register_query(ConversationThreadViewQueryHandler(manager))
    if "conversation.thread.messages" not in known_queries:
        api.register_query(ConversationThreadMessagesQueryHandler(manager))
    if "conversation.turn.process" not in known_queries:
        api.register_query(ConversationTurnProcessQueryHandler(manager))
    if "conversation.thread.search" not in known_queries:
        api.register_query(ConversationThreadSearchQueryHandler(manager))
    if "conversation.thread.list" not in known_queries:
        api.register_query(ConversationThreadListQueryHandler(manager))
    if "conversation.project.directory_list" not in known_queries:
        api.register_query(ConversationProjectDirectoryListQueryHandler(manager))
    if "conversation.memory.list" not in known_queries:
        api.register_query(ConversationMemoryQueryHandler(manager))


__all__ = [
    "COMMAND_TYPES",
    "ConversationCommandHandler",
    "ConversationProjectDirectoryListQueryHandler",
    "ConversationThreadListQueryHandler",
    "ConversationThreadSearchQueryHandler",
    "ConversationThreadMessagesQueryHandler",
    "ConversationThreadViewQueryHandler",
    "ConversationTurnProcessQueryHandler",
    "ConversationMemoryQueryHandler",
    "register_conversation_handlers",
]
