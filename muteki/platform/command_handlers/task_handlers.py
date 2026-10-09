"""Task 聚合的命令 / 查询 Handler（任务书 6.1，COMMAND-01）。

``task.create`` 是事件溯源聚合：计划阶段产出 ``task.created`` 领域事件与
accepted 回执，Command API 统一追加事件（带 expected_version 校验）并持久化
回执；副作用阶段把 Task 对象行写入 platform.db（查询投影），回执落为
completed。幂等由 command_id / idempotency_key 保证：重试不会生成第二个 Task。
"""

from __future__ import annotations

from typing import Any

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
from muteki.platform.contracts.events import EventEnvelope
from muteki.platform.contracts.objects import Task
from muteki.platform.contracts.receipts import AggregateRef, CommandReceipt, ReceiptState

PRODUCER = "builtin.single-security-task"


class TaskCommandHandler:
    """task.* 命令命名空间的属主。"""

    command_types = {"task.create"}

    async def plan(self, command: Any, ctx: HandlerContext) -> CommandPlan:
        payload = dict(command.payload)
        kind = str(payload.get("kind") or "").strip()
        if not kind:
            raise CommandFailed(make_error(
                "task.kind_required",
                "task.create requires payload.kind (如 ctf.challenge / pentest.target)",
                ErrorCategory.VALIDATION,
                correlation_id=correlation_id_of(command)))
        # 调用方可经 aggregate_id 或 payload.task_id 预先确定 Task 身份。
        task_id = str(
            payload.get("task_id") or command.aggregate_id or "").strip() or None
        task = Task(
            **({"task_id": task_id} if task_id else {}),
            thread_id=(str(payload["thread_id"]).strip() or None)
            if payload.get("thread_id") is not None else None,
            project_id=(str(payload["project_id"]).strip() or None)
            if payload.get("project_id") is not None else None,
            kind=kind,
            title=str(payload.get("title") or ""),
            input=dict(payload.get("input") or {}),
            revision=int(payload.get("revision") or 1),
        )
        store = ctx.store

        async def _save() -> SideEffectResult:
            store.save(task)
            return SideEffectResult()

        return CommandPlan(
            events=[EventEnvelope(
                aggregate_type="task",
                aggregate_id=task.task_id,
                event_type="core.task.created",
                producer=PRODUCER,
                actor_id=command.actor.id or "system",
                command_id=command.command_id,
                causation_id=command.command_id,
                correlation_id=correlation_id_of(command),
                idempotency_key=command.idempotency_key,
                payload={
                    "task_id": task.task_id,
                    "thread_id": task.thread_id,
                    "kind": task.kind,
                    "title": task.title,
                    "input": dict(task.input),
                    "revision": task.revision,
                },
            )],
            receipt=CommandReceipt(
                command_id=command.command_id,
                state=ReceiptState.ACCEPTED,
                aggregate=AggregateRef(type="task", id=task.task_id),
            ),
            side_effect=_save,
        )


class TaskQueryHandler:
    """Task 只读查询。"""

    query_types = {"task.get", "task.list"}

    async def handle(self, query: Any, ctx: HandlerContext) -> QueryResult:
        if query.query_type == "task.list":
            params = dict(query.params or {})
            kind = str(params.get("kind") or "").strip()
            kind_prefix = str(params.get("kind_prefix") or "").strip()
            project_id = str(params.get("project_id") or "").strip()
            thread_id = str(params.get("thread_id") or "").strip()
            search = str(params.get("query") or "").strip().casefold()
            try:
                limit = min(2000, max(1, int(params.get("limit") or 500)))
            except (TypeError, ValueError):
                limit = 500

            tasks = sorted(
                ctx.store.list(Task),
                key=lambda item: (item.created_at, item.task_id),
                reverse=True,
            )
            if kind:
                tasks = [item for item in tasks if item.kind == kind]
            if kind_prefix:
                tasks = [
                    item for item in tasks if item.kind.startswith(kind_prefix)
                ]
            if project_id:
                tasks = [item for item in tasks if item.project_id == project_id]
            if thread_id:
                tasks = [item for item in tasks if item.thread_id == thread_id]
            if search:
                tasks = [
                    item for item in tasks
                    if search in " ".join((
                        item.task_id, item.kind, item.title,
                    )).casefold()
                ]
            total = len(tasks)
            from .pagination import page_items
            selected, continuation = page_items(tasks, params, limit, ctx, query.query_type,
                                                 lambda task: task.task_id)
            rows = [
                {
                    "task_id": item.task_id,
                    "thread_id": item.thread_id,
                    "project_id": item.project_id,
                    "kind": item.kind,
                    "title": item.title,
                    "revision": item.revision,
                    "created_at": item.created_at.isoformat(),
                }
                for item in selected
            ]
            return QueryResult(
                query_id=query.query_id,
                query_type=query.query_type,
                result={
                    "tasks": rows,
                    "total": total,
                    "returned": len(rows),
                    **continuation,
                },
            )

        task_id = str(query.params.get("task_id") or query.aggregate_id or "").strip()
        task = ctx.store.get(Task, task_id) if task_id else None
        if task is None:
            raise CommandFailed(make_error(
                "task.not_found",
                f"unknown task {task_id!r}",
                ErrorCategory.NOT_FOUND,
                correlation_id=query.query_id))
        return QueryResult(
            query_id=query.query_id,
            query_type=query.query_type,
            result=task.model_dump(mode="json"),
        )


__all__ = ["PRODUCER", "TaskCommandHandler", "TaskQueryHandler"]
