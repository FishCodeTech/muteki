from __future__ import annotations

from typing import Any

from muteki.conversation import events as conversation_events
from muteki.platform.command_handlers.base import (
    CommandFailed, CommandPlan, HandlerContext, SideEffectResult,
    correlation_id_of, make_error,
)
from muteki.platform.contracts.commands import QueryResult
from muteki.platform.contracts.errors import ErrorCategory
from muteki.platform.contracts.events import EventEnvelope
from muteki.platform.contracts.receipts import AggregateRef, CommandReceipt, ReceiptState

from .service import SrcService
from .store import SrcStoreError
from .control_commands import SRC_CONTROL_COMMAND_TYPES, SRC_CONTROL_QUERY_TYPES

PRODUCER = "builtin.src"
SRC_TASK_COMMAND_TYPES = {"src.task.create", "src.run.ensure", "src.run.start"}
COMMAND_TYPES = {*SRC_TASK_COMMAND_TYPES, *SRC_CONTROL_COMMAND_TYPES}
SRC_AGENT_COMMAND_TYPES = {"src.run.ensure", "src.run.start"}
SRC_BASE_QUERY_TYPES = {"src.task.list", "src.task.get", "src.run.list", "src.record.list", "src.record.get"}
QUERY_TYPES = set(SRC_BASE_QUERY_TYPES)
AGENT_FORBIDDEN_RECORDS = {
    "execution_policy", "execution_approval", "task_grant", "platform_status",
    "credential", "browser_profile", "workbench_slice",
    "workbench_link", "workbench_history", "workbench_merge", "workbench_finding", "workbench_todo", "workbench_link", "workbench_evidence", "workbench_report_draft", "workbench_suggestion", "duplicate_verdict", "asset_version",
}

SRC_DOMAIN_COMMANDS = {
    "src.task.archive": ("service", "archive_task"),
    "src.task.restore": ("service", "restore_task"),
    "src.task.destroy": ("service", "destroy_task"),
    "src.run.archive": ("service", "archive_run"),
    "src.run.restore": ("service", "restore_run"),
    "src.run.destroy": ("service", "destroy_run"),
    "src.artifact.destroy": ("service", "destroy_artifact"),
    "src.exchange.record": ("exchanges", "record"),
    "src.exchange.draft.save": ("exchanges", "save_request_draft"),
    "src.exchange.draft.execute": ("exchanges", "execute_request_draft"),
    "src.research.plan.save": ("research", "save_plan"),
    "src.research.authorization.save": ("research", "save_authorization_snapshot"),
    "src.research.skill.register": ("research", "register_skill_material"),
    "src.research.openapi.ingest": ("research", "ingest_openapi"),
    "src.research.javascript.ingest": ("research", "ingest_javascript"),
    "src.research.runtime.register": ("research", "register_runtime_observation"),
    "src.research.dataflow.register": ("research", "register_dataflow"),
    "src.research.page_action.register": ("research", "register_page_action"),
    "src.research.business.save": ("research", "save_business_model"),
    "src.research.permission_question.add": ("research", "add_permission_question"),
    "src.research.experiment.record": ("research", "record_experiment"),
    "src.research.capability_chain.record": ("research", "record_capability_chain"),
    "src.research.precondition_trial.record": ("research", "record_precondition_trial"),
    "src.asset.observe": ("assets", "observe"),
    "src.asset.correct": ("assets", "correct"),
    "src.asset.relate": ("assets", "relate"),
    "src.asset.derive": ("assets", "derive_endpoint_relations"),
    "src.asset.merge": ("assets", "merge"),
    "src.asset.split": ("assets", "split"),
    "src.asset.lifecycle": ("assets", "lifecycle"),
    "src.asset.destroy": ("assets", "destroy"),
    "src.asset.publish": ("assets", "publish"),
    "src.asset.reference.refresh": ("assets", "refresh_reference"),
    "src.asset.publication.revoke": ("assets", "revoke_publication"),
    "src.ingestion.ingest": ("ingestion", "ingest"),
    "src.ingestion.retry": ("ingestion", "retry_failed"),
    "src.ingestion.reconcile": ("ingestion", "reconcile_external"),
    "src.ingestion.external_event": ("ingestion", "external_event"),
    "src.query.save": ("query", "save_query"),
    "src.query.delete": ("query", "delete_query"),
    "src.vocabulary.save": ("query", "save_vocabulary"),
    "src.tag.save": ("query", "save_tag"),
    "src.tag.assign": ("query", "assign_tags"),
    "src.note.save": ("query", "save_note"),
    "src.connector.config.save": ("connectors", "save_config"),
    "src.connector.config.revoke": ("connectors", "revoke_config"),
    "src.connector.import": ("connectors", "import_file"),
    "src.connector.query": ("connectors", "query"),
    "src.connector.repository": ("connectors", "repository"),
    "src.connector.external_event": ("connectors", "receive_external_event"),
    "src.monitor.create": ("monitoring", "create_monitor"),
    "src.monitor.revise": ("monitoring", "revise_monitor"),
    "src.monitor.state": ("monitoring", "set_state"),
    "src.monitor.wake": ("monitoring", "wake"),
    "src.monitor.run_once": ("monitoring", "run_once"),
    "src.monitor.cycle.run": ("monitoring", "run_cycle"),
    "src.monitor.candidate.handle": ("monitoring", "handle_candidate"),
    "src.monitor.external.reconcile": ("monitoring", "reconcile_external"),
    "src.monitor.notifications.deliver": ("monitoring", "deliver_notifications"),
    "src.report.freeze": ("reports", "freeze"),
    "src.report.reproduction.record": ("reports", "record_independent_reproduction"),
    "src.submission.export": ("submission", "export"),
    "src.report.render": ("submission", "render"),
    "src.review.record": ("findings", "record_review"),
    "src.platform.status.record": ("feedback", "record_platform_status"),
    "src.retest.request": ("retests", "request"),
    "src.retest.dispatch": ("retests", "dispatch"),
    "src.retest.complete": ("retests", "complete"),
    "src.feedback.record": ("feedback", "record_feedback"),
    "src.feedback.platform_status.record": ("feedback", "record_platform_status"),
    "src.finding.claim.create": ("findings", "create_claim"),
    "src.finding.review.record": ("findings", "record_review"),
    "src.finding.invalidate": ("findings", "invalidate"),
    "src.finding.deduplicate": ("findings", "deduplicate"),
    "src.browser.ensure": ("browser", "ensure_task"),
    "src.browser.context.ensure": ("browser", "ensure_context"),
    "src.browser.tab.open": ("browser", "open_tab"),
    "src.browser.tab.release": ("browser", "release_shared_tab"),
    "src.browser.screenshot.capture": ("browser", "capture_screenshot"),
    "src.browser.image.review": ("browser", "review_image"),
    "src.inspection.decide": ("egress", "decide_inspection"),
    "src.inspection.grant.revoke": ("egress", "revoke_inspection_grant"),
    "src.browser.execution.record": ("browser", "record_execution_signal"),
    "src.browser.run.finish": ("browser", "run_finished"),
    "src.browser.task.close": ("browser", "close_task"),
    "src.identity.ensure": ("identity", "ensure_identity"),
    "src.identity.verify": ("identity", "verify_identity"),
    "src.identity.recover": ("identity", "recover_identity"),
    "src.object.lease.acquire": ("objects", "acquire_lease"),
    "src.object.lease.release": ("objects", "release_lease"),
    "src.object.register": ("objects", "register_object"),
    "src.object.experiment.begin": ("objects", "begin_experiment"),
    "src.object.experiment.after": ("objects", "record_after"),
    "src.object.experiment.restore": ("objects", "record_restoration"),
    "src.oast.allocate": ("oast", "allocate"),
    "src.oast.poll": ("oast", "poll"),
    "src.oast.expire": ("oast", "expire"),
    "src.oast.health": ("oast", "health_probe"),
    "src.oast.local.start": ("oast", "start_local_receiver"),
    "src.stream.websocket.connect": ("streams", "connect_websocket"),
    "src.stream.websocket.send": ("streams", "send_websocket"),
    "src.stream.websocket.receive": ("streams", "receive_websocket"),
    "src.stream.websocket.acknowledge": ("streams", "acknowledge"),
    "src.stream.websocket.close": ("streams", "close_websocket"),
    "src.stream.sse.connect": ("streams", "connect_sse"),
    "src.stream.sse.read": ("streams", "read_sse"),
    "src.stream.sse.close": ("streams", "close_sse"),
    "src.knowledge.dataset.create": ("knowledge", "create_dataset"),
    "src.knowledge.publish": ("knowledge", "publish"),
    "src.knowledge.revoke": ("knowledge", "revoke"),
    "src.knowledge.invalidate": ("knowledge", "invalidate"),
    "src.workbench.slice.save": ("workbench", "save_slice"),
    "src.workbench.asset.importance": ("assets_workbench", "set_importance"),
    "src.workbench.finding.merge": ("assets_workbench", "merge_records"),
    "src.workbench.finding.unmerge": ("assets_workbench", "unmerge_records"),
    "src.workbench.finding.meta.save": ("assets_workbench", "save_finding_meta"),
    "src.workbench.finding.stale": ("assets_workbench", "finding_stale"),
    "src.workbench.finding.stale.clear": ("assets_workbench", "finding_stale_clear"),
    "src.workbench.object.link": ("assets_workbench", "bulk_link"),
    "src.workbench.todo.save": ("assets_workbench", "save_todo"),
    "src.workbench.question.status": ("assets_workbench", "question_status"),
    "src.workbench.report.draft.save": ("assets_workbench", "save_report_draft"),
    "src.workbench.report.draft.discard": ("assets_workbench", "discard_report_draft"),
    "src.workbench.suggestion.propose": ("assets_workbench", "propose_suggestion"),
    "src.workbench.suggestion.resolve": ("assets_workbench", "resolve_suggestion"),
    "src.workbench.asset.link": ("assets_workbench", "batch_link"),
    "src.workbench.asset.tasklink": ("assets_workbench", "task_link"),
    "src.workbench.duplicate.verdict": ("assets_workbench", "duplicate_verdict"),
    "src.workbench.history.record": ("assets_workbench", "record_history"),
    "src.workbench.history.revert": ("assets_workbench", "revert_history"),
    "src.asset.merge.revert": ("assets_workbench", "unmerge"),
    "src.ingestion.batch.revert": ("assets_workbench", "batch_revert"),
    "src.asset.version.pin": ("assets_workbench", "pin_version"),
}
SRC_DOMAIN_QUERIES = {
    "src.exchange.query": ("exchanges", "query_exchanges"),
    "src.exchange.read": ("exchanges", "read_exchange"),
    "src.exchange.draft.read": ("exchanges", "read_draft"),
    "src.exchange.compare": ("exchanges", "compare_exchanges"),
    "src.exchange.export": ("exchanges", "export_exchanges"),
    "src.research.records": ("research", "records"),
    "src.research.record.get": ("research", "record"),
    "src.research.plan.current": ("research", "current_plan"),
    "src.research.skill.read": ("research", "read_skill_material"),
    "src.research.openapi.read": ("research", "read_openapi"),
    "src.research.openapi.compare": ("research", "compare_openapi"),
    "src.research.javascript.read": ("research", "read_javascript"),
    "src.research.backlog": ("research", "research_backlog"),
    "src.research.history": ("research", "research_history"),
    "src.research.precondition.summary": ("research", "precondition_summary"),
    "src.asset.detail": ("assets", "detail"),
    "src.asset.published": ("assets", "published"),
    "src.asset.query": ("query", "query"),
    "src.asset.saved_query.read": ("query", "read_query"),
    "src.asset.full_text": ("query", "full_text"),
    "src.asset.pivot": ("query", "pivot"),
    "src.asset.export": ("query", "export"),
    "src.report.get": ("reports", "get"),
    "src.report.list": ("reports", "list"),
    "src.feedback.diff": ("feedback", "diff"),
    "src.feedback.platform_status": ("feedback", "platform_status"),
    "src.feedback.history": ("feedback", "history"),
    "src.finding.claims": ("findings", "list_claims"),
    "src.finding.claim.get": ("findings", "get_claim"),
    "src.finding.reviews": ("findings", "list_reviews"),
    "src.finding.status": ("findings", "review_status"),
    "src.finding.deduplications": ("findings", "list_deduplications"),
    "src.connector.manifests": ("connectors", "manifests"),
    "src.connector.manifest": ("connectors", "manifest"),
    "src.connector.config": ("connectors", "config"),
    "src.monitor.get": ("monitoring", "get_monitor"),
    "src.monitor.list": ("monitoring", "list_monitors"),
    "src.monitor.changes": ("monitoring", "list_changes"),
    "src.monitor.notifications": ("monitoring", "list_notifications"),
    "src.monitor.scheduler_health": ("monitoring", "scheduler_health"),
    "src.retest.recover": ("retests", "recover"),
    "src.identity.get": ("identity", "identity"),
    "src.identity.verifications": ("identity", "verifications"),
    "src.identity.compare": ("identity", "compare_identities"),
    "src.object.history": ("objects", "history"),
    "src.oast.events": ("oast", "events"),
    "src.stream.export": ("streams", "export"),
    "src.knowledge.search": ("knowledge", "search"),
    "src.knowledge.read": ("knowledge", "read"),
    "src.knowledge.source.read": ("knowledge", "read_source"),
    "src.knowledge.history": ("knowledge", "history"),
    "src.browser.service": ("browser", "service"),
    "src.browser.context": ("browser", "context"),
    "src.browser.generation": ("browser", "generation"),
    "src.browser.image.read": ("browser", "read_review_image"),
    "src.inspection.requests": ("egress", "inspection_requests"),
    "src.artifact.read": ("query", "read_artifact"),
    "src.report.templates": ("reports", "templates"),
    "src.workbench.snapshot": ("workbench", "snapshot"),
    "src.workbench.slice.get": ("workbench", "slice_get"),
    "src.workbench.asset.bulk": ("assets_workbench", "bulk"),
    "src.workbench.asset.links": ("assets_workbench", "links"),
    "src.workbench.finding.merges": ("assets_workbench", "merges"),
    "src.workbench.finding.meta": ("assets_workbench", "finding_meta"),
    "src.workbench.evidence.verify": ("assets_workbench", "verify_artifact"),
    "src.workbench.todo.list": ("assets_workbench", "todos"),
    "src.workbench.report.drafts": ("assets_workbench", "report_drafts"),
    "src.workbench.report.diff": ("assets_workbench", "report_diff"),
    "src.workbench.suggestions": ("assets_workbench", "suggestions"),
    "src.workbench.asset.tasklinks": ("assets_workbench", "task_links"),
    "src.workbench.duplicates": ("assets_workbench", "duplicates"),
    "src.workbench.duplicate.verdicts": ("assets_workbench", "verdicts"),
    "src.workbench.history": ("assets_workbench", "history"),
    "src.asset.version.list": ("assets_workbench", "versions"),
}
COMMAND_TYPES = {*COMMAND_TYPES, *SRC_DOMAIN_COMMANDS}
QUERY_TYPES = {*QUERY_TYPES, *SRC_DOMAIN_QUERIES, *SRC_CONTROL_QUERY_TYPES}
SRC_AGENT_DOMAIN_COMMANDS: set[str] = {
    "src.exchange.record", "src.exchange.draft.save", "src.exchange.draft.execute",
    "src.research.plan.save", "src.research.skill.register",
    "src.research.openapi.ingest", "src.research.javascript.ingest",
    "src.research.runtime.register", "src.research.dataflow.register",
    "src.research.page_action.register", "src.research.business.save",
    "src.research.permission_question.add", "src.research.experiment.record",
    "src.research.capability_chain.record", "src.research.precondition_trial.record",
    "src.asset.observe", "src.asset.relate", "src.asset.derive",
    "src.ingestion.ingest", "src.query.save", "src.vocabulary.save",
    "src.tag.save", "src.tag.assign", "src.note.save",
    "src.connector.import", "src.connector.query", "src.connector.repository",
    "src.report.freeze", "src.report.reproduction.record", "src.submission.export",
    "src.report.render", "src.review.record",
    "src.retest.request", "src.retest.dispatch", "src.retest.complete",
    "src.finding.claim.create", "src.finding.review.record", "src.finding.invalidate",
    "src.finding.deduplicate", "src.browser.ensure", "src.browser.context.ensure",
    "src.browser.tab.open", "src.browser.tab.release", "src.browser.screenshot.capture",
    "src.browser.image.review", "src.browser.execution.record", "src.browser.run.finish",
    "src.identity.verify", "src.identity.recover", "src.object.lease.acquire",
    "src.object.lease.release", "src.object.register", "src.object.experiment.begin",
    "src.object.experiment.after", "src.object.experiment.restore",
    "src.oast.allocate", "src.oast.poll", "src.oast.expire", "src.oast.health",
    "src.oast.local.start", "src.stream.websocket.connect", "src.stream.websocket.send",
    "src.stream.websocket.receive", "src.stream.websocket.acknowledge",
    "src.stream.websocket.close", "src.stream.sse.connect", "src.stream.sse.read",
    "src.stream.sse.close",
    "src.workbench.suggestion.propose",
}


def _error(exc: SrcStoreError, correlation_id: str) -> CommandFailed:
    category = ErrorCategory.VALIDATION
    if exc.code in {"task_binding_invalid", "policy_denied"}:
        category = ErrorCategory.PERMISSION
    elif exc.code in {"version_conflict", "authorization_stale"}:
        category = ErrorCategory.CONFLICT
    elif exc.code in {"artifact_missing"}:
        category = ErrorCategory.NOT_FOUND
    error = make_error(exc.code, exc.message, category, correlation_id=correlation_id)
    return CommandFailed(error.model_copy(update={"detail": dict(exc.detail)}))


def _receipt(command: Any, task_id: str, *, output: dict[str, Any] | None = None) -> CommandReceipt:
    return CommandReceipt(
        command_id=command.command_id, state=ReceiptState.ACCEPTED,
        aggregate=AggregateRef(type="task", id=task_id), output=output or {},
    )


class SrcCommandHandler:
    command_types = SRC_TASK_COMMAND_TYPES

    def __init__(self, service: SrcService) -> None:
        self.service = service

    async def plan(self, command: Any, ctx: HandlerContext) -> CommandPlan:
        try:
            if command.command_type == "src.task.create":
                return self._create_task(command, ctx)
            if command.command_type == "src.run.ensure":
                return await self._ensure_run(command, ctx)
            if command.command_type == "src.run.start":
                return await self._start_run(command, ctx)
            raise SrcStoreError("command.unsupported", f"unsupported {command.command_type}")
        except SrcStoreError as exc:
            raise _error(exc, correlation_id_of(command)) from exc

    def _create_task(self, command: Any, ctx: HandlerContext) -> CommandPlan:
        if ctx.principal.kind not in {"operator", "system"}:
            raise SrcStoreError("task_binding_invalid", "SRC Tasks are created by the operator")
        thread, task, binding = self.service.prepare_task(ctx.principal, command.payload)
        runtime = dict(command.payload.get("runtime") or {})
        selection = None
        if runtime:
            selection = self.service.conversation._build_runtime_selection(thread.thread_id, runtime)

        def commit() -> None:
            self.service.conversation.activate_thread(
                thread, binding.principal_id, selection=selection)
            self.service.platform.save(task)
            self.service.store.put(
                task.task_id, "task_binding", binding.model_dump(mode="json"),
                record_id=task.task_id, natural_key=thread.thread_id,
            )
            authorization = dict(task.input.get("authorization") or {})
            self.service.execution.ensure_default_policy(
                task.task_id, list(authorization.get("targets") or []), binding.principal_id,
            )

        return CommandPlan(
            events=[
                conversation_events.thread_event(
                    thread.thread_id, conversation_events.EV_THREAD_CREATED, {
                        "thread_id": thread.thread_id, "project_id": thread.project_id,
                        "workspace_id": thread.workspace_id, "title": thread.title,
                        "title_source": thread.title_source, "mode": thread.mode,
                        "runtime": runtime,
                    }, actor_id=command.actor.id, command_id=command.command_id,
                    correlation_id=correlation_id_of(command),
                    idempotency_key=command.idempotency_key,
                ),
                EventEnvelope(
                    aggregate_type="task", aggregate_id=task.task_id,
                    event_type="src.task.created", producer=PRODUCER,
                    actor_id=command.actor.id, command_id=command.command_id,
                    correlation_id=correlation_id_of(command),
                    idempotency_key=command.idempotency_key,
                    payload={"task": task.model_dump(mode="json"), "binding": binding.model_dump(mode="json")},
                ),
            ],
            receipt=_receipt(command, task.task_id, output={
                "task_id": task.task_id, "thread_id": thread.thread_id,
            }),
            local_commit=commit,
        )

    async def _ensure_run(self, command: Any, ctx: HandlerContext) -> CommandPlan:
        task_id = "" if command.aggregate_type == "thread" else command.aggregate_id
        binding = self.service.resolve_actor(ctx, task_id)
        task_id = binding.task_id
        self.service.require_active(task_id)
        revision = int(command.payload.get("binding_revision") or binding.revision)

        async def ensure() -> SideEffectResult:
            result = await self.service.ensure_run(
                binding, command.payload, binding_revision=revision)
            return SideEffectResult(output=result)

        return CommandPlan(
            events=[EventEnvelope(
                aggregate_type="task", aggregate_id=task_id,
                event_type="src.run.ensure_requested", producer=PRODUCER,
                actor_id=command.actor.id, command_id=command.command_id,
                correlation_id=correlation_id_of(command),
                idempotency_key=command.idempotency_key,
                payload={"target_url": command.payload.get("target_url") or command.payload.get("target"),
                         "binding_revision": revision},
            )],
            receipt=_receipt(command, task_id), side_effect=ensure,
            outbox_destination="src.run.ensure",
        )

    async def _start_run(self, command: Any, ctx: HandlerContext) -> CommandPlan:
        task_id = "" if command.aggregate_type == "thread" else command.aggregate_id
        binding = self.service.resolve_actor(ctx, task_id)
        task_id = binding.task_id
        run_id = str(command.payload.get("run_id") or "")
        self.service.run_binding(task_id, run_id)

        async def start() -> SideEffectResult:
            receipt = await self.service.start_run(
                task_id, run_id, command.payload, actor_id=command.actor.id)
            return SideEffectResult(
                output={"receipt": receipt.model_dump(mode="json")},
                error=receipt.error,
                state=ReceiptState.FAILED if receipt.error else ReceiptState.COMPLETED,
            )

        return CommandPlan(
            events=[EventEnvelope(
                aggregate_type="task", aggregate_id=task_id,
                event_type="src.run.start_requested", producer=PRODUCER,
                actor_id=command.actor.id, command_id=command.command_id,
                correlation_id=correlation_id_of(command),
                idempotency_key=command.idempotency_key,
                payload={"run_id": run_id, "thread_id": binding.thread_id},
            )],
            receipt=_receipt(command, task_id), side_effect=start,
            outbox_destination="src.run.start",
        )


def _json_safe(value: Any) -> Any:
    import base64
    if isinstance(value, bytes):
        return {"encoding": "base64", "data": base64.b64encode(value).decode("ascii"), "size": len(value)}
    if isinstance(value, dict):
        return {str(key): _json_safe(item) for key, item in value.items()}
    if isinstance(value, (list, tuple)):
        return [_json_safe(item) for item in value]
    if hasattr(value, "model_dump"):
        return _json_safe(value.model_dump(mode="json"))
    return value


def _domain_failure(exc: Exception, correlation_id: str) -> CommandFailed:
    if isinstance(exc, SrcStoreError):
        return _error(exc, correlation_id)
    code = str(getattr(exc, "code", "") or "")
    if code:
        error = make_error(code, str(exc), ErrorCategory.VALIDATION, correlation_id=correlation_id)
        return CommandFailed(error.model_copy(update={"detail": dict(getattr(exc, "detail", {}) or {})}))
    raise exc


async def _invoke_domain(
    service: SrcService, domain_name: str, method_name: str, task_id: str,
    payload: dict[str, Any], ctx: HandlerContext,
) -> Any:
    import base64
    import inspect
    domain = service.domain(domain_name)
    method = getattr(domain, method_name)
    kwargs = dict(payload)
    kwargs.pop("task_id", None)
    if "content_b64" in kwargs:
        kwargs["content"] = base64.b64decode(kwargs.pop("content_b64"), validate=True)
    if "payload_b64" in kwargs:
        kwargs["payload"] = base64.b64decode(kwargs.pop("payload_b64"), validate=True)
    if "raw_request_b64" in kwargs:
        kwargs["raw_request"] = base64.b64decode(kwargs.pop("raw_request_b64"), validate=True)
    if "request_raw_b64" in kwargs:
        kwargs["request_raw"] = base64.b64decode(kwargs.pop("request_raw_b64"), validate=True)
    if "response_raw_b64" in kwargs:
        kwargs["response_raw"] = base64.b64decode(kwargs.pop("response_raw_b64"), validate=True)
    signature = inspect.signature(method)
    if any(isinstance(value, dict) for value in kwargs.values()):
        from pydantic import BaseModel
        from typing import get_type_hints
        hints = get_type_hints(method)
        for name, parameter in signature.parameters.items():
            annotation = hints.get(name, parameter.annotation)
            if isinstance(kwargs.get(name), dict) and isinstance(annotation, type) and issubclass(annotation, BaseModel):
                kwargs[name] = annotation.model_validate(kwargs[name])
    for envelope_field in ("command_id", "expected_version", "correlation_id"):
        kwargs.pop(envelope_field, None)
    if "idempotency_key" not in signature.parameters:
        kwargs.pop("idempotency_key", None)
    if "payload" in signature.parameters and "payload" not in kwargs:
        kwargs["payload"] = dict(payload)
    trusted_actor_id = str(ctx.principal.id or "local-user")
    trusted_actor_kind = str(ctx.principal.kind or "operator")
    for name in ("actor_id", "requester", "principal_id", "created_by"):
        supplied = kwargs.get(name)
        if supplied is not None and str(supplied) != trusted_actor_id and trusted_actor_kind not in {"operator", "system"}:
            raise SrcStoreError("task_binding_invalid", f"caller cannot spoof {name}")
        if name in signature.parameters:
            kwargs[name] = trusted_actor_id
        else:
            kwargs.pop(name, None)
    if "actor_kind" in kwargs and str(kwargs["actor_kind"]) != trusted_actor_kind and trusted_actor_kind not in {"operator", "system"}:
        raise SrcStoreError("task_binding_invalid", "caller cannot spoof actor_kind")
    if "actor_kind" in signature.parameters:
        kwargs["actor_kind"] = trusted_actor_kind
    else:
        kwargs.pop("actor_kind", None)
    if "actor" in signature.parameters:
        raw_actor = kwargs.get("actor") if isinstance(kwargs.get("actor"), dict) else {}
        run_id = str(raw_actor.get("run_id") or "").strip()
        if run_id:
            service.run_binding(task_id, run_id)
        thread_id = "" if run_id else str(
            (ctx.binding.thread_id if ctx.binding else ctx.principal.thread_id)
            or service.binding(task_id).thread_id
        )
        kwargs["actor"] = {
            **raw_actor,
            "id": trusted_actor_id,
            "kind": trusted_actor_kind,
            "thread_id": thread_id,
            "run_id": run_id,
        }
    try:
        bound = (
            signature.bind(task_id, **kwargs)
            if "task_id" in signature.parameters else signature.bind(**kwargs)
        )
    except TypeError as exc:
        raise SrcStoreError("query.arguments_invalid", str(exc)) from exc
    result = method(*bound.args, **bound.kwargs)
    if inspect.isawaitable(result):
        result = await result
    return _json_safe(result)


SRC_LIFECYCLE_COMMANDS = {
    "src.task.archive", "src.task.restore", "src.task.destroy",
    "src.run.archive", "src.run.restore", "src.run.destroy", "src.artifact.destroy",
}


class SrcDomainCommandHandler:
    command_types = set(SRC_DOMAIN_COMMANDS)

    def __init__(self, service: SrcService) -> None:
        self.service = service

    async def plan(self, command: Any, ctx: HandlerContext) -> CommandPlan:
        try:
            task_id = "" if command.aggregate_type == "thread" else command.aggregate_id
            task_id = self.service.resolve_actor(ctx, task_id).task_id
            if ctx.principal.kind not in {"operator", "system"} and command.command_type not in SRC_AGENT_DOMAIN_COMMANDS:
                raise SrcStoreError("policy_denied", "Agent cannot invoke this SRC domain command")
            if command.command_type not in SRC_LIFECYCLE_COMMANDS | {"src.approval.decide", "src.grant.revoke"}:
                self.service.require_active(task_id)
            domain_name, method_name = SRC_DOMAIN_COMMANDS[command.command_type]
        except (SrcStoreError, KeyError) as exc:
            if isinstance(exc, KeyError):
                exc = SrcStoreError("command.unsupported", command.command_type)
            raise _domain_failure(exc, correlation_id_of(command)) from exc

        async def apply() -> SideEffectResult:
            try:
                result = await _invoke_domain(
                    self.service, domain_name, method_name, task_id, command.payload, ctx,
                )
                return SideEffectResult(output={"result": result})
            except Exception as exc:
                failure = _domain_failure(exc, correlation_id_of(command))
                return SideEffectResult(error=failure.error, state=failure.state)

        return CommandPlan(
            events=[EventEnvelope(
                aggregate_type="task", aggregate_id=task_id,
                event_type=f"{command.command_type}.requested", producer=PRODUCER,
                actor_id=command.actor.id, command_id=command.command_id,
                correlation_id=correlation_id_of(command),
                idempotency_key=command.idempotency_key, payload=dict(command.payload),
            )],
            receipt=_receipt(command, task_id), side_effect=apply,
            outbox_destination="src.domain",
        )


class SrcDomainQueryHandler:
    query_types = set(SRC_DOMAIN_QUERIES)

    def __init__(self, service: SrcService) -> None:
        self.service = service

    async def handle(self, query: Any, ctx: HandlerContext) -> QueryResult:
        try:
            task_id = str(query.params.get("task_id") or "")
            if not task_id and query.aggregate_type != "thread":
                task_id = str(query.aggregate_id or "")
            task_id = self.service.resolve_actor(ctx, task_id).task_id
            domain_name, method_name = SRC_DOMAIN_QUERIES[query.query_type]
            result = await _invoke_domain(
                self.service, domain_name, method_name, task_id, query.params, ctx,
            )
            return QueryResult(query_id=query.query_id, query_type=query.query_type, result=result)
        except Exception as exc:
            raise _domain_failure(exc, query.query_id) from exc


class SrcQueryHandler:
    query_types = SRC_BASE_QUERY_TYPES

    def __init__(self, service: SrcService) -> None:
        self.service = service

    async def handle(self, query: Any, ctx: HandlerContext) -> QueryResult:
        try:
            task_id = str(query.params.get("task_id") or "")
            if not task_id and query.aggregate_type != "thread":
                task_id = str(query.aggregate_id or "")
            if query.query_type != "src.task.list":
                task_id = self.service.resolve_actor(ctx, task_id).task_id
            if query.query_type == "src.task.list":
                result = self.service.list_tasks(ctx)
            elif query.query_type == "src.task.get":
                result = self.service.task_view(task_id)
            elif query.query_type == "src.run.list":
                result = await self.service.list_runs(task_id)
            elif query.query_type == "src.record.list":
                kind = str(query.params.get("kind") or "")
                agent = ctx.principal.kind not in {"operator", "system"}
                if agent and kind in AGENT_FORBIDDEN_RECORDS:
                    raise SrcStoreError("task_binding_invalid", "Agent cannot enumerate this control-plane record")
                result = self.service.query_records(
                    task_id, ctx, kind=kind,
                    parent_id=query.params.get("parent_id"),
                    limit=query.params.get("limit") or 100,
                    cursor=str(query.params.get("cursor") or query.params.get("after_cursor") or ""),
                    excluded_kinds=AGENT_FORBIDDEN_RECORDS if agent else set(),
                )
            elif query.query_type == "src.record.get":
                kind = str(query.params.get("kind") or "")
                if ctx.principal.kind not in {"operator", "system"} and kind in AGENT_FORBIDDEN_RECORDS:
                    raise SrcStoreError("task_binding_invalid", "Agent cannot read this control-plane record")
                result = self.service.store.require(
                    task_id, kind, str(query.params.get("record_id") or ""),
                ).model_dump(mode="json")
            else:
                raise SrcStoreError("query.unsupported", f"unsupported {query.query_type}")
            return QueryResult(query_id=query.query_id, query_type=query.query_type, result=result)
        except SrcStoreError as exc:
            raise _error(exc, query.query_id) from exc


__all__ = [
    "SrcCommandHandler", "SrcQueryHandler", "SrcDomainCommandHandler",
    "SrcDomainQueryHandler", "COMMAND_TYPES", "QUERY_TYPES",
    "SRC_TASK_COMMAND_TYPES", "SRC_BASE_QUERY_TYPES", "SRC_DOMAIN_COMMANDS",
    "SRC_DOMAIN_QUERIES", "SRC_AGENT_COMMAND_TYPES", "SRC_AGENT_DOMAIN_COMMANDS",
]
