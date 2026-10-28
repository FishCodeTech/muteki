from __future__ import annotations

import asyncio
import uuid
from pathlib import Path
from typing import Any, Optional
from urllib.parse import urlsplit

from muteki.conversation.manager import ConversationManager
from muteki.platform.command_handlers.base import HandlerContext
from muteki.platform.contracts.commands import ActorRef
from muteki.platform.contracts.objects import Task, Thread
from muteki.platform.contracts.runs import BoundRunRequest, RunCommand, RunSnapshot
from muteki.platform.store import PlatformStore

from .artifacts import SrcArtifactStore
from .contracts import SrcRunBinding, SrcTaskBinding
from .store import SrcStore, SrcStoreError

TASK_KIND = "src.bounty"
PRODUCT_MODE = "src"
EXECUTION_MODE = "pentest"


class SrcService:
    def __init__(
        self, platform: PlatformStore, *, artifact_root: Path,
        conversation: ConversationManager, run_gateway: Any,
    ) -> None:
        self.platform = platform
        self.store = SrcStore(platform)
        self.artifacts = SrcArtifactStore(artifact_root)
        self.conversation = conversation
        self.run_gateway = run_gateway
        from .projections import SrcProjector
        from .execution import SrcExecutionService
        from .egress import SrcEgressManager
        self.projector = SrcProjector(self.store, self.artifacts)
        self.execution = SrcExecutionService(self)
        self.egress = SrcEgressManager(artifact_root.parent / "egress")
        self.egress.register_execution_service(self.execution)
        self._domain_services: dict[str, Any] = {}
        self._run_locks: dict[str, asyncio.Lock] = {}
        self._run_locks_guard = asyncio.Lock()

    async def _run_lock(self, binding_key: str) -> asyncio.Lock:
        async with self._run_locks_guard:
            return self._run_locks.setdefault(binding_key, asyncio.Lock())

    def domain(self, name: str) -> Any:
        existing = self._domain_services.get(name)
        if existing is not None:
            return existing
        if name == "service":
            service = self
        elif name == "egress":
            service = self.egress
        elif name == "exchanges":
            from .exchanges import ExchangeService
            service = ExchangeService(self)
        elif name == "research":
            from .research import SrcResearchService
            service = SrcResearchService(self.store, self.artifacts)
        elif name == "assets":
            from .assets import SrcAssetService
            service = SrcAssetService(self)
        elif name == "reports":
            from .reports import SrcReportService
            service = SrcReportService(self.store, self.artifacts)
        elif name == "submission":
            from .submission import SrcSubmissionService
            service = SrcSubmissionService(self.domain("reports"), self.artifacts)
        elif name == "retests":
            from .retests import SrcRetestService
            service = SrcRetestService(self.store, self.artifacts, self.domain("reports"), self.run_gateway)
        elif name == "feedback":
            from .feedback import SrcFeedbackService
            service = SrcFeedbackService(self.domain("reports"), self.artifacts)
        elif name == "findings":
            from .findings import SrcFindingService
            service = SrcFindingService(self.store, self.artifacts)
        elif name == "connectors":
            from .connectors import ConnectorService
            service = ConnectorService(self.store, self.artifacts)
        elif name == "browser":
            from .browser import SrcTaskBrowserService
            from .browser_container import DockerAgentBrowserTaskDriver
            service = SrcTaskBrowserService(
                self.store, self.artifacts,
                driver=DockerAgentBrowserTaskDriver(),
                policy_provider=self.execution.current_policy,
                egress=self.egress,
            )
        elif name == "identity":
            from .identity import SrcIdentityService
            service = SrcIdentityService(self.store)
        elif name == "objects":
            from .objects import SrcObjectService
            service = SrcObjectService(self.store, self.artifacts)
        elif name == "oast":
            from .oast import SrcOastService
            service = SrcOastService(self)
        elif name == "streams":
            from .streams import SrcStreamService
            service = SrcStreamService(self)
        elif name == "monitoring":
            from .monitoring import SrcMonitoringService
            service = SrcMonitoringService(self, runner=self._run_monitor_source)
        elif name == "ingestion":
            from .ingestion import SrcIngestionService
            service = SrcIngestionService(self, self.domain("assets"))
        elif name == "query":
            from .query import SrcQueryService
            service = SrcQueryService(self, self.domain("assets"))
        elif name == "knowledge":
            from .knowledge import SrcKnowledgeBase

            def authorizer(task_id: str, principal_id: str, action: str) -> bool:
                try:
                    return self.binding(task_id).principal_id == principal_id
                except SrcStoreError:
                    return False

            service = SrcKnowledgeBase(self.store, self.artifacts, authorizer)
        elif name == "workbench":
            from .workbench import SrcWorkbenchService
            service = SrcWorkbenchService(self.store)
        elif name == "assets_workbench":
            from .assets_workbench import SrcWorkbenchAssetService
            service = SrcWorkbenchAssetService(self)
        else:
            raise SrcStoreError("capability_unavailable", f"unknown SRC domain service {name!r}")
        return self.register_domain_service(name, service)

    async def _run_monitor_source(self, context: dict[str, Any]) -> dict[str, Any]:
        """Collect one pinned, bounded connector batch for an asset monitor."""
        from .contracts import ExecutionRequest, ResourceFact
        from .monitoring import ExternalOutcomeUnknown, SrcMonitorError
        from .policy import evaluate

        task_id = str(context["task_id"])
        binding = self.binding(task_id)
        definition = context["definition"]
        source = definition["source"]
        connector_id = str(source["connector_id"])
        connectors = self.domain("connectors")
        manifest = connectors.manifest(connector_id)
        if source.get("capability_manifest_ref") != manifest.version:
            raise SrcMonitorError("capability_unavailable", "来源能力版本已变化；需要修订监控定义")
        if definition["chain_type"] != "asset" or "query" not in manifest.capabilities:
            raise SrcMonitorError("capability_unavailable", "此来源没有可执行的资产查询监控能力")
        config_version = int(source.get("config_version") or 0)
        if config_version < 1:
            raise SrcMonitorError("capability_unavailable", "监控必须固定来源配置版本")
        config = connectors.config(task_id, connector_id, config_version)
        if int(config["version"]) != config_version or config["manifest"]["version"] != manifest.version:
            raise SrcMonitorError("capability_unavailable", "来源配置或能力版本已变化")
        reference_fields = tuple((config["manifest"].get("credentials") or {}).get("reference_fields") or ())
        credential_refs = {field: str(config["config"][field]) for field in reference_fields if config["config"].get(field)}
        if source.get("credential_refs") != credential_refs:
            raise SrcMonitorError("authorization_unresolved", "监控凭据引用与固定来源配置不一致；需要修订监控定义")
        plan_ref = str(definition.get("plan_ref") or "")
        plan = self.store.get(task_id, "research_plan", plan_ref) if plan_ref else None
        if plan is None or any(int(row.payload.get("version") or 0) > int(plan.payload.get("version") or 0)
                               for row in self.store.list(task_id, "research_plan", parent_id=plan.parent_id)):
            raise SrcMonitorError("authorization_unresolved", "监控计划版本缺失或已过期")
        policy = self.execution.current_policy(task_id)
        authorization_ref = f"{policy.policy_id}:v{policy.version}"
        if definition.get("authorization_ref") != authorization_ref:
            raise SrcMonitorError("authorization_unresolved", "监控授权版本与当前执行策略不一致")
        query = str(source.get("query") or "").strip()
        if not query:
            raise SrcMonitorError("capability_unavailable", "监控来源缺少固定查询语句")
        request_args = {"connector_id": connector_id, "query": query, "config_version": config_version,
                        "credential_refs": credential_refs, **credential_refs}
        identity_ref = f"connector:{connector_id}:{self.store.digest(credential_refs)}" if credential_refs else ""
        decision = evaluate(policy, ExecutionRequest(
            task_id=task_id, thread_id=binding.thread_id, tool_name="src.connector.query",
            action="query", arguments_hash=self.store.digest(request_args), arguments=request_args,
            resource_kind="control", resource_facts={
                "connector_id": ResourceFact(value=connector_id, origin="tool_input"),
                "config_version": ResourceFact(value=config_version, origin="environment"),
                **{field: ResourceFact(value=value, origin="environment") for field, value in credential_refs.items()},
            }, identity_ref=identity_ref, policy_version=policy.version,
        ))
        if decision["effect"] != "allow":
            raise SrcMonitorError(str(decision["reason_code"]), "当前执行策略未允许此监控来源调用")
        if policy.defaults.get("public_research") is not True and not decision["matched_rule_refs"]:
            raise SrcMonitorError("authorization_unresolved", "此监控来源调用需要明确的允许规则")
        call_limit = int(definition["budget"].get("call_limit") or 0)
        if call_limit < 1:
            raise SrcMonitorError("budget_exhausted", "监控需要正数的单轮调用上限")
        max_items = min(max(1, int(source.get("max_items") or 10000)), 10000)
        batch = await connectors.query(
            task_id, connector_id, query, config_version=config_version,
            max_pages=min(call_limit, 100), max_items=max_items,
            idempotency_key=context["cycle_id"], extra=dict(source.get("extra") or {}),
        )
        if batch.get("external_state") == "outcome_unknown":
            raise ExternalOutcomeUnknown("来源调用结果未知，需要核对来源回执", receipt=batch)
        acquisition = dict(batch.get("acquisition") or {})
        complete = batch.get("external_state") == "completed" and acquisition.get("complete") is True
        items = list(batch.get("items") or [])
        snapshot = {str(item.get("input_key") or index): item.get("mapped", item.get("raw"))
                    for index, item in enumerate(items)}
        cost = dict(batch.get("cost") or {})
        actual = cost.get("actual")
        return {
            "raw_snapshot": snapshot,
            "observations": [{"key": key, "attributes": value, "raw_values": items[index].get("raw"),
                              "success": True, "completeness": "complete" if complete else "partial"}
                             for index, (key, value) in enumerate(snapshot.items())],
            "completeness": "complete" if complete else "partial" if items else "unavailable",
            "errors": batch.get("errors") or [], "calls": int(acquisition.get("pages") or 0),
            "cost": {"actual": float(actual or 0), "unknown": actual is None},
            "source_watermarks": {"batch_id": batch.get("batch_id"), "next_token": acquisition.get("next_token")},
            "receipt": {"batch_id": batch.get("batch_id"), "source_refs": batch.get("source_refs") or [],
                        "external_state": batch.get("external_state"), "cost": cost},
            "revalidation": {"source_available": True, "permission_valid": True,
                             "plan_valid": True, "authorization_valid": True,
                             "plan_ref": plan_ref, "authorization_ref": authorization_ref,
                             "config_version": config_version},
        }

    def prepare_task(
        self, actor: ActorRef, payload: dict[str, Any],
    ) -> tuple[Thread, Task, SrcTaskBinding]:
        title = str(payload.get("title") or "").strip() or "SRC 赏金研究"
        goal = str(payload.get("goal") or payload.get("description") or "").strip()
        thread = Thread(
            project_id=str(payload.get("project_id") or "").strip() or None,
            title=title, mode="src",
        )
        task_input = dict(payload.get("input") or {})
        task_input.update({
            "product_mode": PRODUCT_MODE,
            "goal": goal,
            "authorization": dict(payload.get("authorization") or {}),
            "company": str(payload.get("company") or ""),
            "program": dict(payload.get("program") or {}),
        })
        task = Task(
            thread_id=thread.thread_id, project_id=thread.project_id,
            kind=TASK_KIND, title=title, input=task_input,
        )
        binding = SrcTaskBinding(
            task_id=task.task_id, thread_id=thread.thread_id,
            project_id=thread.project_id, principal_id=actor.id or "local-user",
        )
        return thread, task, binding

    def commit_task(
        self, thread: Thread, task: Task, binding: SrcTaskBinding, *,
        principal_id: str, runtime: Optional[dict[str, Any]] = None,
    ) -> None:
        selection = None
        if runtime:
            selection = self.conversation._build_runtime_selection(thread.thread_id, runtime)
        self.conversation.activate_thread(thread, principal_id, selection=selection)
        self.platform.save(task)
        self.store.put(
            task.task_id, "task_binding", binding.model_dump(mode="json"),
            record_id=task.task_id, natural_key=thread.thread_id,
        )

    def task(self, task_id: str) -> Task:
        task = self.platform.get(Task, task_id)
        if task is None or task.kind != TASK_KIND or task.input.get("product_mode") != PRODUCT_MODE:
            raise SrcStoreError("task_binding_invalid", "unknown SRC Task")
        return task

    def binding(self, task_id: str) -> SrcTaskBinding:
        self.task(task_id)
        record = self.store.require(task_id, "task_binding", task_id)
        binding = SrcTaskBinding.model_validate(record.payload)
        if binding.task_id != task_id:
            raise SrcStoreError("task_binding_invalid", "SRC Task binding is inconsistent")
        thread = self.platform.get(Thread, binding.thread_id)
        if thread is None or thread.thread_id != binding.thread_id:
            raise SrcStoreError("task_binding_invalid", "SRC main Thread is unavailable")
        return binding

    def task_for_thread(self, thread_id: str) -> SrcTaskBinding:
        with self.platform.lock:
            rows = list(self.platform.conn.execute(
                "SELECT task_id FROM src_records WHERE kind='task_binding' AND natural_key=?",
                (thread_id,),
            ).fetchall())
        if len(rows) != 1:
            raise SrcStoreError("task_binding_invalid", "Thread is not bound to exactly one SRC Task")
        binding = self.binding(str(rows[0]["task_id"]))
        if binding.thread_id != thread_id:
            raise SrcStoreError("task_binding_invalid", "Thread/Task binding mismatch")
        return binding

    def resolve_actor(self, ctx: HandlerContext, task_id: str = "") -> SrcTaskBinding:
        if task_id:
            binding = self.binding(task_id)
        else:
            thread_id = str(ctx.binding.thread_id if ctx.binding else ctx.principal.thread_id)
            if not thread_id:
                raise SrcStoreError("task_binding_invalid", "trusted Thread is required")
            binding = self.task_for_thread(thread_id)
        if ctx.principal.kind not in {"operator", "system"}:
            if not ctx.binding or ctx.binding.thread_id != binding.thread_id:
                raise SrcStoreError("task_binding_invalid", "CapabilityBinding does not belong to this SRC Task")
            if binding.principal_id != ctx.principal.id:
                raise SrcStoreError("task_binding_invalid", "Principal does not own this SRC Task")
        return binding

    def list_tasks(self, ctx: HandlerContext) -> list[dict[str, Any]]:
        if ctx.principal.kind in {"operator", "system"}:
            tasks = [task for task in self.platform.list(Task, kind=TASK_KIND)
                     if task.input.get("product_mode") == PRODUCT_MODE]
        else:
            tasks = [self.task(self.resolve_actor(ctx).task_id)]
        return [self.task_view(task.task_id) for task in tasks]

    @staticmethod
    def _public_config(value: Any) -> Any:
        sensitive = {"api_key", "apikey", "password", "secret", "secret_key", "access_token", "refresh_token", "private_key"}
        if isinstance(value, dict):
            return {
                str(key): SrcService._public_config(item)
                for key, item in value.items() if str(key).lower() not in sensitive
            }
        if isinstance(value, (list, tuple)):
            return [SrcService._public_config(item) for item in value]
        if hasattr(value, "model_dump"):
            return SrcService._public_config(value.model_dump(mode="json"))
        return value

    def record_chat_configuration(
        self, task_id: str, selection: Any = None, *, source: Optional[dict[str, Any]] = None,
    ) -> dict[str, Any]:
        binding = self.binding(task_id)
        if selection is None:
            selector = getattr(self.conversation, "runtime_selection", None)
            selection = (
                selector(binding.thread_id) if selector is not None
                else {"state": "unavailable", "reason": "runtime_selection_unsupported"}
            )
        resolved = self._public_config(selection)
        version = self.store.digest(resolved)
        parent = "chat"
        existing = [row for row in self.store.all(task_id, "model_config") if row.parent_id == parent]
        for row in existing:
            if row.payload.get("version") == version:
                return row.model_dump(mode="json")
        record = self.store.put(
            task_id, "model_config", {
                "role": "chat", "version": version, "revision": len(existing) + 1,
                "source": source or {"kind": "conversation.runtime_selection"},
                "resolved": resolved,
            }, record_id=f"{task_id}:{parent}:{version}", parent_id=parent,
        )
        return record.model_dump(mode="json")

    def record_worker_configuration(self, task_id: str, run_id: str) -> dict[str, Any]:
        from datetime import datetime, timezone
        config_store = getattr(self, "worker_config", None)
        if config_store is None:
            raise SrcStoreError("capability_unavailable", "Worker configuration store is unavailable")
        resolved = self._public_config(config_store.get())
        version = self.store.digest(resolved)
        parent = run_id
        existing = [row for row in self.store.all(task_id, "model_config") if row.parent_id == parent]
        for row in existing:
            if row.payload.get("version") == version:
                return row.model_dump(mode="json")
        record = self.store.put(
            task_id, "model_config", {
                "role": "worker", "version": version, "revision": len(existing) + 1,
                "source": {"kind": "WorkerConfigStore", "adopted_at": datetime.now(timezone.utc).isoformat()},
                "resolved": resolved,
            }, record_id=f"{task_id}:{parent}:{version}", parent_id=parent,
        )
        return record.model_dump(mode="json")

    def task_view(self, task_id: str) -> dict[str, Any]:
        task = self.task(task_id)
        binding = self.binding(task_id)
        runs = self.store.all(task_id, "run_binding")
        chat_config = self.record_chat_configuration(task_id)
        worker_configs = [row.model_dump(mode="json") for row in self.store.all(task_id, "model_config")
                          if row.payload.get("role") == "worker"]
        return {
            **task.model_dump(mode="json"),
            "binding": binding.model_dump(mode="json"),
            "run_count": len(runs),
            "lifecycle": self._lifecycle(task_id, "task_lifecycle", task_id),
            "chat_configuration": chat_config,
            "worker_configurations": worker_configs,
            "watermarks": self.store.watermarks(task_id),
        }

    def query_records(
        self, task_id: str, ctx: HandlerContext, *, kind: str = "",
        parent_id: Optional[str] = None, limit: Any = 100,
        cursor: str = "", excluded_kinds: Optional[set[str]] = None,
    ) -> dict[str, Any]:
        try:
            page_limit = int(limit)
        except (TypeError, ValueError) as exc:
            raise SrcStoreError("query.invalid_limit", "limit must be an integer") from exc
        if not 1 <= page_limit <= 1000:
            raise SrcStoreError("query.invalid_limit", "limit must be between 1 and 1000")
        from muteki.platform.command_handlers.cursor import (
            CursorError, assert_cursor_scope, encode_cursor,
        )
        aggregate_id = f"{task_id}:{kind}:{parent_id or ''}"
        scope = (
            f"binding:{ctx.binding.binding_id}" if ctx.binding is not None
            else f"{ctx.principal.kind}:{ctx.principal.id}"
        )
        try:
            after = assert_cursor_scope(
                ctx.cursor_key, cursor or None, "src-record", aggregate_id, scope=scope,
            )
        except CursorError as exc:
            raise SrcStoreError("query.cursor_invalid", str(exc)) from exc
        excluded = set(excluded_kinds or set())
        rows = self.store.list(
            task_id, kind, parent_id=parent_id, after_rowid=after,
            limit=page_limit + 1, excluded_kinds=excluded,
        )
        has_more = len(rows) > page_limit
        selected = rows[:page_limit]
        next_cursor = None
        if has_more and selected:
            next_cursor = encode_cursor(
                ctx.cursor_key, "src-record", aggregate_id,
                self.store.rowid(selected[-1]), scope=scope,
            )
        clauses = ["task_id=?"]
        params: list[Any] = [task_id]
        if kind:
            clauses.append("kind=?")
            params.append(kind)
        if parent_id is not None:
            clauses.append("parent_id=?")
            params.append(parent_id)
        if excluded:
            clauses.append("kind NOT IN (" + ",".join("?" for _ in excluded) + ")")
            params.extend(sorted(excluded))
        with self.platform.lock:
            total = int(self.platform.conn.execute(
                "SELECT COUNT(*) FROM src_records WHERE " + " AND ".join(clauses), params,
            ).fetchone()[0])
        watermarks = self.store.watermarks(task_id)
        failures = [row for row in watermarks if row.get("state") in {"failed", "pending"}]
        return {
            "records": [row.model_dump(mode="json") for row in selected],
            "total": total, "returned": len(selected), "next_cursor": next_cursor,
            "complete": not failures,
            "partial_reason": "；".join(
                f"{row.get('source_id')}: {row.get('error') or row.get('state')}"
                for row in failures
            ) or None,
            "watermarks": watermarks,
            "snapshot": {"event_watermark": self.platform.event_watermark()},
        }

    @staticmethod
    def _target_url(payload: dict[str, Any]) -> str:
        value = str(payload.get("target_url") or payload.get("target") or "").strip()
        parts = urlsplit(value)
        if parts.scheme not in {"http", "https"} or not parts.hostname or parts.username or parts.password:
            raise SrcStoreError("task_binding_invalid", "a complete HTTP(S) target URL is required")
        return value

    async def ensure_run(
        self, binding: SrcTaskBinding, payload: dict[str, Any], *,
        binding_revision: int,
    ) -> dict[str, Any]:
        target = self._target_url(payload)
        key_input = str(payload.get("binding_key") or "").strip() or self.store.digest({
            "target": target, "question": str(payload.get("question") or ""),
            "target_version": str(payload.get("target_version") or ""),
        })
        binding_key = f"src:{binding.task_id}:{key_input}"
        async with await self._run_lock(binding_key):
            existing = self.store.by_natural_key(binding.task_id, "run_binding", binding_key)
            if existing is not None:
                data = existing.payload
                if data.get("destroyed_at"):
                    raise SrcStoreError("source_revoked", "Run has been destroyed")
                self.require_active(binding.task_id, existing.record_id)
                if int(data.get("binding_revision") or 0) != binding_revision:
                    raise SrcStoreError("authorization_stale", "Run binding revision is stale")
                contract = dict(data.get("contract") or {})
                if contract.get("target") != target or str(contract.get("target_version") or "") != str(payload.get("target_version") or ""):
                    raise SrcStoreError("version_conflict", "a changed target requires a new binding_key")
                ref = await self.run_gateway.ensure_bound_run(BoundRunRequest(
                    binding_key=binding_key, task_id=binding.task_id,
                    task_kind="pentest.target", task_revision=binding_revision,
                    run_id=existing.record_id, executor_id="swarm.coordinator",
                    payload={"product_mode": PRODUCT_MODE, "src_contract": contract},
                ))
                snapshot = await self.run_gateway.snapshot(ref.run_id)
                return {"binding": data, "run": snapshot.model_dump(mode="json"), "deduplicated": True}
            run_id = f"crun_{uuid.uuid4().hex[:16]}"
            contract = {
                "target": target,
                "question": str(payload.get("question") or ""),
                "evidence_refs": list(payload.get("evidence_refs") or []),
                "identity": dict(payload.get("identity") or {}),
                "object_state": dict(payload.get("object_state") or {}),
                "budget": dict(payload.get("budget") or {}),
                "target_version": str(payload.get("target_version") or ""),
            }
            import json
            contract_artifact = self.artifacts.put(
                binding.task_id,
                json.dumps(contract, ensure_ascii=False, separators=(",", ":")).encode("utf-8"),
                media_type="application/json", name=f"{run_id}-contract.json",
                source_run_id=run_id,
            )
            run_binding = SrcRunBinding(
                task_id=binding.task_id, thread_id=binding.thread_id, run_id=run_id,
                binding_key=binding_key, binding_revision=binding_revision,
                target_contract_ref=contract_artifact.sha256,
                plan_ref=str(payload.get("plan_ref") or ""),
                authorization_ref=str(payload.get("authorization_ref") or ""),
                source_message_id=str(payload.get("source_message_id") or ""),
            )
            record_payload = {
                **run_binding.model_dump(mode="json"), "contract": contract,
                "contract_artifact": contract_artifact.model_dump(mode="json"),
            }
            self.store.put(
                binding.task_id, "run_binding", record_payload,
                record_id=run_id, natural_key=binding_key, generation=1,
            )
            ref = await self.run_gateway.ensure_bound_run(BoundRunRequest(
                binding_key=binding_key, task_id=binding.task_id,
                task_kind="pentest.target", task_revision=binding_revision,
                run_id=run_id, executor_id="swarm.coordinator",
                payload={"product_mode": PRODUCT_MODE, "src_contract": contract},
            ))
            snapshot = await self.run_gateway.snapshot(ref.run_id)
            return {
                "binding": record_payload,
                "run": snapshot.model_dump(mode="json"), "deduplicated": False,
            }

    def run_binding(self, task_id: str, run_id: str) -> SrcRunBinding:
        record = self.store.require(task_id, "run_binding", run_id)
        binding = SrcRunBinding.model_validate(record.payload)
        if binding.run_id != run_id or binding.task_id != task_id:
            raise SrcStoreError("task_binding_invalid", "Run does not belong to this SRC Task")
        return binding

    def _lifecycle(self, task_id: str, kind: str, record_id: str) -> dict[str, Any]:
        record = self.store.get(task_id, kind, record_id)
        return dict(record.payload) if record is not None else {"state": "active"}

    def require_active(self, task_id: str, run_id: str = "") -> None:
        if self._lifecycle(task_id, "task_lifecycle", task_id).get("state") == "archived":
            raise SrcStoreError("task_archived", "SRC Task is archived; restore it before new work")
        if run_id and self._lifecycle(task_id, "run_lifecycle", run_id).get("state") == "archived":
            raise SrcStoreError("run_archived", "SRC Run is archived; restore it before new work")

    async def archive_task(self, task_id: str, actor_id: str) -> dict[str, Any]:
        from datetime import datetime, timezone
        binding = self.binding(task_id)
        for row in self.store.all(task_id, "run_binding"):
            if row.payload.get("destroyed_at"):
                continue
            snapshot = await self.run_gateway.snapshot(row.record_id)
            if snapshot.state not in {"draft", "finished", "solved", "stopped"}:
                receipt = await self.run_gateway.command(row.record_id, RunCommand(
                    command_type="stop", actor_id=actor_id,
                ))
                if receipt.error is not None:
                    raise SrcStoreError("run_stop_failed", receipt.error.message)
        executor = getattr(self, "conversation_executor", None)
        if executor is not None:
            await executor.close_thread(binding.thread_id)
        if self.conversation.conv.list_queue(binding.thread_id):
            self.conversation.conv.pause_queue(binding.thread_id, "task_archived")
        self.conversation.archive_thread(binding.thread_id)
        record = self.store.put(
            task_id, "task_lifecycle", {
                "state": "archived", "actor_id": actor_id,
                "archived_at": datetime.now(timezone.utc).isoformat(),
            }, record_id=task_id,
        )
        return record.model_dump(mode="json")

    async def restore_task(self, task_id: str, actor_id: str) -> dict[str, Any]:
        from datetime import datetime, timezone
        from muteki.conversation import events as conversation_events
        binding = self.binding(task_id)
        thread = self.platform.get(Thread, binding.thread_id)
        self.conversation.activate_thread(thread, binding.principal_id)
        with self.platform.transaction():
            self.platform.append_events(conversation_events.thread_event(
                binding.thread_id, conversation_events.EV_THREAD_UNARCHIVED,
                {"thread_id": binding.thread_id}, actor_id=actor_id,
            ))
        record = self.store.put(
            task_id, "task_lifecycle", {
                "state": "active", "actor_id": actor_id,
                "restored_at": datetime.now(timezone.utc).isoformat(),
            }, record_id=task_id,
        )
        return record.model_dump(mode="json")

    async def archive_run(self, task_id: str, run_id: str, actor_id: str) -> dict[str, Any]:
        from datetime import datetime, timezone
        self.run_binding(task_id, run_id)
        result = await self.run_gateway.mutate_legacy(
            "rail.update", run_id, {"archived": True, "now": datetime.now(timezone.utc).timestamp()},
        )
        if not result.get("ok"):
            raise SrcStoreError("run_archive_failed", "Run could not be archived")
        record = self.store.put(
            task_id, "run_lifecycle", {
                "state": "archived", "actor_id": actor_id,
                "archived_at": datetime.now(timezone.utc).isoformat(),
            }, record_id=run_id, parent_id=task_id,
        )
        return record.model_dump(mode="json")

    async def restore_run(self, task_id: str, run_id: str, actor_id: str) -> dict[str, Any]:
        from datetime import datetime, timezone
        self.run_binding(task_id, run_id)
        self.require_active(task_id)
        result = await self.run_gateway.mutate_legacy(
            "rail.update", run_id, {"archived": False, "now": datetime.now(timezone.utc).timestamp()},
        )
        if not result.get("ok"):
            raise SrcStoreError("run_restore_failed", "Run could not be restored")
        record = self.store.put(
            task_id, "run_lifecycle", {
                "state": "active", "actor_id": actor_id,
                "restored_at": datetime.now(timezone.utc).isoformat(),
            }, record_id=run_id, parent_id=task_id,
        )
        return record.model_dump(mode="json")

    def destroy_artifact(self, task_id: str, sha256: str, actor_id: str) -> dict[str, Any]:
        from datetime import datetime, timezone
        self.binding(task_id)
        if self._lifecycle(task_id, "task_lifecycle", task_id).get("state") != "archived":
            raise SrcStoreError("task_not_archived", "CAS destruction requires an archived Task")
        with self.platform.lock:
            rows = list(self.platform.conn.execute(
                "SELECT task_id,kind,record_id FROM src_records WHERE payload LIKE ?",
                (f"%{sha256}%",),
            ).fetchall())
        protected = [dict(row) for row in rows if row["kind"] != "artifact_disposal"]
        if protected:
            raise SrcStoreError("artifact_in_use", "artifact is referenced and cannot be destroyed", detail={"references": protected})
        self.artifacts.destroy(task_id, sha256)
        record = self.store.put(
            task_id, "artifact_disposal", {
                "sha256": sha256, "state": "destroyed", "actor_id": actor_id,
                "destroyed_at": datetime.now(timezone.utc).isoformat(),
            }, record_id=sha256,
        )
        return record.model_dump(mode="json")

    async def destroy_run(self, task_id: str, run_id: str, actor_id: str) -> dict[str, Any]:
        from datetime import datetime, timezone
        import json
        self.run_binding(task_id, run_id)
        if self._lifecycle(task_id, "run_lifecycle", run_id).get("state") != "archived":
            raise SrcStoreError("run_not_archived", "Run destruction requires an archived Run")
        protected_kinds = {"frozen_report", "asset_publication", "asset_reference", "monitor_baseline", "knowledge_publication"}
        references = []
        for kind in protected_kinds:
            for row in self.store.all(task_id, kind):
                if run_id in json.dumps(row.payload, ensure_ascii=False):
                    references.append({"kind": kind, "record_id": row.record_id})
        if references:
            raise SrcStoreError("artifact_in_use", "Run is protected by frozen or shared references", detail={"references": references})
        self.store.put(
            task_id, "run_disposal", {"state": "approved", "actor_id": actor_id},
            record_id=run_id, parent_id=task_id,
        )
        try:
            result = await self.run_gateway.mutate_legacy("rail.delete", run_id, {})
            if not result.get("ok"):
                raise SrcStoreError("run_destroy_failed", "Run could not be destroyed")
        except Exception as exc:
            self.store.put(
                task_id, "run_disposal", {
                    "state": "failed", "actor_id": actor_id,
                    "error": f"{type(exc).__name__}: {exc}",
                }, record_id=run_id, parent_id=task_id,
            )
            raise
        destroyed_at = datetime.now(timezone.utc).isoformat()
        binding = self.store.require(task_id, "run_binding", run_id)
        self.store.put(
            task_id, "run_binding", {**binding.payload, "destroyed_at": destroyed_at},
            record_id=run_id, expected_revision=binding.revision,
            natural_key=binding.natural_key,
        )
        record = self.store.put(
            task_id, "run_disposal", {
                "state": "destroyed", "actor_id": actor_id, "destroyed_at": destroyed_at,
            }, record_id=run_id, parent_id=task_id,
        )
        return record.model_dump(mode="json")

    async def destroy_task(self, task_id: str, actor_id: str) -> dict[str, Any]:
        from datetime import datetime, timezone
        import json
        self.binding(task_id)
        if self._lifecycle(task_id, "task_lifecycle", task_id).get("state") != "archived":
            raise SrcStoreError("task_not_archived", "Task destruction requires an archived Task")
        protected_kinds = {"frozen_report", "asset_publication", "asset_reference", "monitor_baseline", "knowledge_publication"}
        protected = [
            {"kind": row.kind, "record_id": row.record_id}
            for kind in protected_kinds for row in self.store.all(task_id, kind)
        ]
        if protected:
            raise SrcStoreError("artifact_in_use", "Task is protected by frozen or shared references", detail={"references": protected})
        for row in self.store.all(task_id, "run_binding"):
            if not row.payload.get("destroyed_at") and self._lifecycle(task_id, "run_lifecycle", row.record_id).get("state") != "archived":
                raise SrcStoreError("run_not_archived", f"Run {row.record_id} must be archived or destroyed first")
        root = self.artifacts.task_root(task_id)
        for path in root.iterdir():
            if path.is_file() and not path.name.startswith("."):
                with self.platform.lock:
                    references = list(self.platform.conn.execute(
                        "SELECT task_id,kind,record_id FROM src_records WHERE task_id != ? AND payload LIKE ?",
                        (task_id, f"%{path.name}%"),
                    ).fetchall())
                if references:
                    raise SrcStoreError("artifact_in_use", "Task CAS is referenced by another Task", detail={"sha256": path.name, "references": [dict(row) for row in references]})
        self.artifacts.destroy_root(task_id)
        with self.platform.transaction():
            for row in self.store.all(task_id):
                if row.kind not in {"task_lifecycle", "task_tombstone", "artifact_disposal", "run_lifecycle", "run_disposal", "run_binding"}:
                    self.platform.conn.execute(
                        "DELETE FROM src_records WHERE kind=? AND record_id=?", (row.kind, row.record_id),
                    )
        record = self.store.put(
            task_id, "task_tombstone", {
                "state": "destroyed", "actor_id": actor_id,
                "destroyed_at": datetime.now(timezone.utc).isoformat(),
            }, record_id=task_id,
        )
        return record.model_dump(mode="json")

    def task_id_for_run(self, run_id: str) -> Optional[str]:
        with self.platform.lock:
            row = self.platform.conn.execute(
                "SELECT task_id FROM src_records WHERE kind='run_binding' AND record_id=?",
                (run_id,),
            ).fetchone()
        return str(row["task_id"]) if row is not None else None

    def run_delete_allowed(self, run_id: str) -> bool:
        task_id = self.task_id_for_run(run_id)
        if task_id is None:
            return True
        disposal = self.store.get(task_id, "run_disposal", run_id)
        return bool(disposal and disposal.payload.get("state") == "approved")

    async def run_snapshot(self, task_id: str, run_id: str) -> RunSnapshot:
        binding_record = self.store.require(task_id, "run_binding", run_id)
        if binding_record.payload.get("destroyed_at"):
            raise SrcStoreError("source_revoked", "Run has been destroyed")
        self.run_binding(task_id, run_id)
        snapshot = await self.run_gateway.snapshot(run_id)
        if snapshot.task_kind != "pentest.target":
            raise SrcStoreError("task_binding_invalid", "Run belongs to another product")
        return snapshot

    async def start_run(
        self, task_id: str, run_id: str, payload: dict[str, Any], *, actor_id: str,
    ) -> Any:
        binding_record = self.store.require(task_id, "run_binding", run_id)
        if binding_record.payload.get("destroyed_at"):
            raise SrcStoreError("source_revoked", "Run has been destroyed")
        binding = SrcRunBinding.model_validate(binding_record.payload)
        self.require_active(task_id, run_id)
        task = self.task(task_id)
        target_payload = dict(payload)
        contract = dict(binding_record.payload.get("contract") or {})
        if not target_payload.get("target_url") and not target_payload.get("target"):
            target_payload["target_url"] = contract.get("target")
        target = self._target_url(target_payload)
        if contract.get("target") and target != contract["target"]:
            raise SrcStoreError("version_conflict", "Run target differs from its frozen contract")
        self.record_worker_configuration(task_id, run_id)
        payload = {
            key: value for key, value in payload.items()
            if key not in {"command_id", "idempotency_key", "expected_version", "correlation_id"}
        }
        return await self.run_gateway.command(run_id, RunCommand(
            command_type="start", actor_id=actor_id,
            payload={
                "kind": "swarm", "mode": EXECUTION_MODE,
                "product_mode": PRODUCT_MODE,
                "prompt": str(payload.get("prompt") or task.input.get("goal") or ""),
                "challenge": {
                    "mode": EXECUTION_MODE, "name": task.title,
                    "target": target, "scope": str(payload.get("scope") or target),
                    "goal": str(payload.get("goal") or payload.get("question") or task.input.get("goal") or ""),
                },
                "src_task_id": task_id, "src_binding_revision": binding.binding_revision,
            },
        ))

    async def list_runs(self, task_id: str) -> list[dict[str, Any]]:
        rows = []
        for record in self.store.all(task_id, "run_binding"):
            snapshot = await self.run_snapshot(task_id, record.record_id)
            rows.append({"binding": record.payload, "run": snapshot.model_dump(mode="json")})
        return rows

    async def consume_run_event(self, event: Any) -> None:
        run_id = str(getattr(event, "run_id", "") or "")
        task_id = self.task_id_for_run(run_id)
        if task_id is not None:
            self.projector.project(task_id, run_id, event)

    async def recover(self) -> dict[str, Any]:
        recovered = 0
        projected = 0
        failures: list[dict[str, str]] = []
        with self.platform.lock:
            rows = list(self.platform.conn.execute(
                "SELECT DISTINCT task_id FROM src_records WHERE kind='task_binding'",
            ).fetchall())
        for row in rows:
            task_id = str(row["task_id"])
            for binding in self.store.all(task_id, "run_binding"):
                if binding.payload.get("destroyed_at"):
                    continue
                try:
                    data = binding.payload
                    await self.run_gateway.ensure_bound_run(BoundRunRequest(
                        binding_key=data["binding_key"], task_id=task_id,
                        task_kind="pentest.target", task_revision=int(data["binding_revision"]),
                        run_id=data["run_id"], executor_id="swarm.coordinator",
                    ))
                    recovered += 1
                    result = await self.projector.recover_run(task_id, binding.record_id, self.run_gateway)
                    projected += int(result["projected"])
                except Exception as exc:
                    failures.append({"run_id": binding.record_id, "error": f"{type(exc).__name__}: {exc}"})
        domain_recovery: dict[str, Any] = {}
        for row in rows:
            task_id = str(row["task_id"])
            try:
                domain_recovery[f"{task_id}:egress"] = await asyncio.to_thread(
                    self.egress.recover_enforced, task_id, self.execution.current_policy(task_id),
                )
            except Exception as exc:
                failure = f"{type(exc).__name__}: {exc}"
                domain_recovery[f"{task_id}:egress"] = {"error": failure}
                failures.append({"domain": "egress", "task_id": task_id, "error": failure})
            for name in ("browser", "monitoring", "retests"):
                try:
                    domain = self.domain(name)
                    if name == "browser":
                        domain.ensure_task(task_id)
                    domain_recovery[f"{task_id}:{name}"] = domain.recover(task_id)
                    if name == "monitoring" and domain.list_monitors(task_id):
                        await domain.start(task_id=task_id, poll_seconds=5.0)
                except Exception as exc:
                    failure = f"{type(exc).__name__}: {exc}"
                    domain_recovery[f"{task_id}:{name}"] = {"error": failure}
                    failures.append({"domain": name, "task_id": task_id, "error": failure})
        return {
            "runs": recovered, "projected": projected,
            "domains": domain_recovery, "failures": failures,
        }

    def register_domain_service(self, name: str, service: Any) -> Any:
        if not name or service is None:
            raise ValueError("a named SRC domain service is required")
        existing = self._domain_services.get(name)
        if existing is not None and existing is not service:
            raise ValueError(f"SRC domain service {name!r} is already registered")
        self._domain_services[name] = service
        return service

    def health(self) -> dict[str, Any]:
        return {
            "egress_supported": self.egress.supported(),
            "domains": sorted(self._domain_services),
            "run_event_sink": True,
        }

    async def shutdown(self) -> None:
        errors: list[Exception] = []
        for name, service in self._domain_services.items():
            close = (
                getattr(service, "shutdown", None) or getattr(service, "close", None)
                or getattr(service, "stop", None)
            )
            if close is None:
                continue
            try:
                result = close()
                if asyncio.iscoroutine(result):
                    await result
            except Exception as exc:
                errors.append(exc)
        try:
            await asyncio.to_thread(self.egress.stop_all)
        except Exception as exc:
            errors.append(exc)
        if errors:
            raise ExceptionGroup("SRC domain shutdown failed", errors)


__all__ = ["SrcService", "TASK_KIND", "PRODUCT_MODE", "EXECUTION_MODE"]
