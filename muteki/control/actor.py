"""Single-writer async control actor and live-worker registry contracts."""

from __future__ import annotations

import asyncio
import hmac
import inspect
import threading
import time
from dataclasses import dataclass, field
from typing import Any, Awaitable, Callable, Optional, Protocol, Sequence, runtime_checkable

from muteki.control.admission import AdmissionError, ControlAdmission
from muteki.control.models import (
    ApplyResult,
    ContextKind,
    ContextResource,
    ContextTaint,
    ControlCommand,
    ControlAction,
    ControlScope,
    EffectReceipt,
    EffectState,
    RunControlState,
    ScopeKind,
    WorkerRef,
    DecisionAnswer,
    DecisionStatus,
    context_resource_id_for_command,
    continuation_intent_id_for_command,
)
from muteki.control.store import SQLiteControlJournal


@runtime_checkable
class WorkerRegistry(Protocol):
    def resolve(self, scope: ControlScope) -> Sequence[WorkerRef]: ...
    def snapshot(self) -> Sequence[WorkerRef]: ...


class InMemoryWorkerRegistry:
    """Small coordinator-owned projection of workers that can receive control."""

    def __init__(self) -> None:
        self._workers: dict[str, WorkerRef] = {}
        self._lock = threading.RLock()

    def register(self, worker: WorkerRef) -> WorkerRef:
        with self._lock:
            self._workers[worker.worker_id] = worker
        return worker

    def update(self, worker_id: str, **changes: Any) -> WorkerRef:
        with self._lock:
            current = self._workers.get(worker_id)
            if current is None:
                raise KeyError(worker_id)
            updated = current.model_copy(update=changes)
            self._workers[worker_id] = updated
            return updated

    def unregister(self, worker_id: str) -> Optional[WorkerRef]:
        with self._lock:
            return self._workers.pop(worker_id, None)

    def snapshot(self) -> tuple[WorkerRef, ...]:
        with self._lock:
            return tuple(sorted(self._workers.values(), key=lambda w: w.worker_id))

    def clear(self) -> None:
        with self._lock:
            self._workers.clear()

    def resolve(self, scope: ControlScope) -> tuple[WorkerRef, ...]:
        workers = self.snapshot()
        if scope.kind in {ScopeKind.GLOBAL, ScopeKind.RUN}:
            return workers
        if scope.kind is ScopeKind.WORKER:
            return tuple(w for w in workers if w.worker_id == scope.value)
        if scope.kind is ScopeKind.CHALLENGE:
            return tuple(w for w in workers if w.challenge_id == scope.value)
        if scope.kind is ScopeKind.INTENT:
            return tuple(w for w in workers if w.intent_id == scope.value)
        if scope.kind is ScopeKind.ENGINE:
            return tuple(w for w in workers if w.engine == scope.value)
        if scope.kind is ScopeKind.LANE:
            return tuple(w for w in workers if w.lane == scope.value)
        return ()


@runtime_checkable
class ControlPort(Protocol):
    """Adapter from durable commands to the existing coordinator/runtime."""

    async def apply(
        self,
        command: ControlCommand,
        targets: Sequence[WorkerRef],
        desired: RunControlState,
    ) -> ApplyResult: ...


class UnknownControlPort:
    """Safe default: persistence is real, but execution is explicitly unknown."""

    async def apply(
        self,
        command: ControlCommand,
        targets: Sequence[WorkerRef],
        desired: RunControlState,
    ) -> ApplyResult:
        return ApplyResult(
            state=EffectState.UNKNOWN,
            detail="no runtime control adapter is installed",
            target_ids=[target.worker_id for target in targets],
        )


EffectSink = Callable[[EffectReceipt], Optional[Awaitable[None]]]


@dataclass
class _Envelope:
    command: ControlCommand
    persisted: "asyncio.Future[EffectReceipt]"
    terminal: "asyncio.Future[EffectReceipt]"


_CLOSE = object()


@dataclass(frozen=True)
class _DurableCompanion:
    """Pure plan committed in the same transaction as ``PERSISTED``.

    Keeping planning separate from publication makes the PERSISTED receipt an
    honest durability fence: once that receipt exists, every typed companion it
    promises exists as well.  Runtime routing remains a later, non-replayable
    boundary.
    """

    contexts: tuple[ContextResource, ...] = ()
    expiration_context_ids: tuple[str, ...] = ()
    expiration_actor: str = "operator"
    expiration_reason: str = ""
    decision_answer: Optional[DecisionAnswer] = None
    decision_context: Optional[ContextResource] = None
    metadata: dict[str, Any] = field(default_factory=dict)


class ControlActor:
    """Serialize admission, journal writes, desired state, routing, and receipts.

    ``submit`` returns after the command is durably persisted (or rejected).
    ``submit_and_wait`` additionally waits for an observed terminal effect.  A
    duplicate ``command_id`` is never routed twice, including after restart.
    """

    def __init__(
        self,
        *,
        run_id: str,
        journal: SQLiteControlJournal,
        port: Optional[ControlPort] = None,
        registry: Optional[WorkerRegistry] = None,
        admission: Optional[ControlAdmission] = None,
        effect_sink: Optional[EffectSink] = None,
        secret_resolver: Optional[Callable[[str], str]] = None,
    ) -> None:
        if journal.run_id != run_id:
            raise ValueError("actor and journal run_id must match")
        self.run_id = run_id
        self.journal = journal
        self.port = port or UnknownControlPort()
        self.registry = registry or InMemoryWorkerRegistry()
        self.admission = admission or ControlAdmission()
        self.effect_sink = effect_sink
        # Coordinator-private capability used only for exact comparison of
        # opaque secret refs. Resolved values are never returned or persisted.
        self.secret_resolver = secret_resolver
        self._queue: "asyncio.Queue[_Envelope | object]" = asyncio.Queue()
        self._task: Optional[asyncio.Task[None]] = None
        self._submit_lock = asyncio.Lock()
        self._inflight: dict[str, _Envelope] = {}
        self._closed = False

    @property
    def desired_state(self) -> RunControlState:
        return self.journal.current_state()

    async def start(self) -> None:
        if self._closed:
            raise RuntimeError("control actor is closed")
        if self._task is None:
            self._task = asyncio.create_task(
                self._run(), name=f"control-actor:{self.run_id}"
            )

    async def _enqueue(self, command: ControlCommand) -> _Envelope:
        if command.run_id != self.run_id:
            raise ValueError("command and actor run_id must match")
        await self.start()
        async with self._submit_lock:
            existing = self._inflight.get(command.command_id)
            if existing is not None:
                # Detect a conflicting in-flight reuse before either reaches SQLite.
                if existing.command.semantic_hash() != command.semantic_hash():
                    from muteki.control.store import IdempotencyConflict
                    raise IdempotencyConflict(
                        f"command_id {command.command_id!r} was reused with different content"
                    )
                return existing
            loop = asyncio.get_running_loop()
            envelope = _Envelope(
                command=command,
                persisted=loop.create_future(),
                terminal=loop.create_future(),
            )
            self._inflight[command.command_id] = envelope
            self._queue.put_nowait(envelope)
            return envelope

    async def submit(self, command: ControlCommand) -> EffectReceipt:
        envelope = await self._enqueue(command)
        return await asyncio.shield(envelope.persisted)

    async def submit_and_wait(self, command: ControlCommand) -> EffectReceipt:
        envelope = await self._enqueue(command)
        return await asyncio.shield(envelope.terminal)

    async def join(self) -> None:
        await self._queue.join()

    async def close(self) -> None:
        if self._closed:
            return
        self._closed = True
        if self._task is None:
            return
        await self._queue.join()
        self._queue.put_nowait(_CLOSE)
        await self._task
        self._task = None

    async def _emit(self, receipt: EffectReceipt) -> None:
        if self.effect_sink is None:
            return
        try:
            result = self.effect_sink(receipt)
            if inspect.isawaitable(result):
                await result
        except Exception:
            # Telemetry/UI projection failure cannot invalidate a durable effect.
            pass

    @staticmethod
    def _set_result(future: "asyncio.Future[EffectReceipt]",
                    receipt: EffectReceipt) -> None:
        if not future.done():
            future.set_result(receipt)

    @staticmethod
    def _set_exception(future: "asyncio.Future[EffectReceipt]",
                       exc: BaseException) -> None:
        if not future.done():
            future.set_exception(exc)

    async def _run(self) -> None:
        while True:
            item = await self._queue.get()
            if item is _CLOSE:
                self._queue.task_done()
                return
            envelope = item
            assert isinstance(envelope, _Envelope)
            try:
                await self._process(envelope)
            except Exception as exc:
                self._set_exception(envelope.persisted, exc)
                self._set_exception(envelope.terminal, exc)
            finally:
                self._inflight.pop(envelope.command.command_id, None)
                self._queue.task_done()

    async def _process(self, envelope: _Envelope) -> None:
        command = envelope.command
        appended = self.journal.append_command(command)
        if not appended.inserted:
            # Exactly-once boundary: an existing command is observational only.
            # If the prior process crashed after RECEIVED/PERSISTED/ROUTED, we can
            # prove neither non-delivery nor effect.  Never replay a one-shot command
            # and never hand a non-terminal receipt to ``submit_and_wait`` as though
            # it were final: close the audit trail with an explicit UNKNOWN recovery.
            latest = appended.latest_receipt
            if not latest.state.terminal:
                latest = self.journal.append_effect(EffectReceipt(
                    command_id=command.command_id,
                    run_id=self.run_id,
                    state=EffectState.UNKNOWN,
                    scope=command.scope,
                    observed_generation=self.journal.current_state().generation,
                    detail=("recovered an incomplete command after "
                            f"{latest.state.value}; command was not re-executed"),
                    metadata={
                        **latest.metadata,
                        "code": "recovery_incomplete",
                        "recovered_from": latest.state.value,
                        "reexecuted": False,
                    },
                ))
            # Re-project the durable terminal receipt on every idempotent retry.
            # A prior process may have committed the receipt and crashed before its
            # UI/event projection; command replay remains forbidden, receipt replay
            # is safe because frontends reduce by command_id/receipt_id.
            await self._emit(latest)
            self._set_result(envelope.persisted, latest)
            self._set_result(envelope.terminal, latest)
            return

        for receipt in appended.receipts:
            await self._emit(receipt)
        if not appended.accepted:
            latest = appended.latest_receipt
            self._set_result(envelope.persisted, latest)
            self._set_result(envelope.terminal, latest)
            return

        current = self.journal.current_state()
        try:
            decision = self.admission.admit(command, current)
        except AdmissionError as exc:
            rejected = self.journal.append_effect(EffectReceipt(
                command_id=command.command_id,
                run_id=self.run_id,
                state=EffectState.REJECTED,
                scope=command.scope,
                observed_generation=current.generation,
                detail=exc.detail,
                metadata={"code": exc.code},
            ))
            await self._emit(rejected)
            self._set_result(envelope.persisted, rejected)
            self._set_result(envelope.terminal, rejected)
            return

        try:
            targets = tuple(self.registry.resolve(command.scope))
        except Exception:
            targets = ()

        # Build first, then publish the receipt and every durable companion in one
        # SQLite transaction. A process crash after PERSISTED can therefore never
        # strand a context/decision/expiration behind an exactly-once command id.
        try:
            companion = self._plan_durable_companion(command, targets)
            persisted_metadata = dict(companion.metadata)
            if command.action in {
                    ControlAction.CLEAR_STANDING,
                    ControlAction.RESET_GUIDANCE,
                    ControlAction.EXPIRE_CONTEXT,
            }:
                persisted_metadata["expired_context_count"] = len(
                    companion.expiration_context_ids)
            persisted, expired_count = self.journal.append_persisted_with_companion(
                EffectReceipt(
                    command_id=command.command_id,
                    run_id=self.run_id,
                    state=EffectState.PERSISTED,
                    scope=command.scope,
                    observed_generation=current.generation,
                    metadata=persisted_metadata,
                ),
                contexts=companion.contexts,
                expiration_context_ids=companion.expiration_context_ids,
                expiration_actor=companion.expiration_actor,
                expiration_reason=companion.expiration_reason,
                decision_answer=companion.decision_answer,
                decision_context=companion.decision_context,
            )
            companion_metadata = dict(companion.metadata)
            if (command.action in {
                    ControlAction.CLEAR_STANDING,
                    ControlAction.RESET_GUIDANCE,
                    ControlAction.EXPIRE_CONTEXT,
            }):
                companion_metadata["expired_context_count"] = expired_count
        except Exception as exc:
            failed = self.journal.append_effect(EffectReceipt(
                command_id=command.command_id,
                run_id=self.run_id,
                state=EffectState.FAILED,
                scope=command.scope,
                observed_generation=current.generation,
                detail=f"durable companion rejected: {type(exc).__name__}",
                metadata={"code": "durable_companion_failure"},
            ))
            await self._emit(failed)
            self._set_result(envelope.persisted, failed)
            self._set_result(envelope.terminal, failed)
            return
        await self._emit(persisted)
        self._set_result(envelope.persisted, persisted)

        desired = current
        if decision.desired_mode is not None and decision.desired_mode is not current.mode:
            desired = RunControlState(
                run_id=self.run_id,
                generation=current.generation + 1,
                mode=decision.desired_mode,
                updated_by_command_id=command.command_id,
                reason=f"operator action {command.action.value}",
                updated_at=time.time(),
            )
            self.journal.append_state(desired)

        routed = self.journal.append_effect(EffectReceipt(
            command_id=command.command_id,
            run_id=self.run_id,
            state=EffectState.ROUTED,
            scope=command.scope,
            observed_generation=desired.generation,
        ))
        await self._emit(routed)

        try:
            runtime_command = command
            if companion_metadata:
                runtime_command = command.model_copy(update={
                    "payload": {
                        **command.payload,
                        "_control_companion": companion_metadata,
                    },
                })
            applied = await self.port.apply(runtime_command, targets, desired)
            receipt = EffectReceipt(
                command_id=command.command_id,
                run_id=self.run_id,
                state=applied.state,
                scope=command.scope,
                target_ids=(applied.target_ids or
                            [target.worker_id for target in targets]),
                detail=applied.detail,
                observed_generation=desired.generation,
                metadata={**companion_metadata, **applied.metadata},
            )
        except Exception as exc:
            receipt = EffectReceipt(
                command_id=command.command_id,
                run_id=self.run_id,
                state=EffectState.FAILED,
                scope=command.scope,
                observed_generation=desired.generation,
                # Adapter exceptions may contain materialized prompt/credential
                # values. Persist only the type; plaintext belongs exclusively in
                # SecretStore and transient worker memory.
                detail=f"runtime adapter failed: {type(exc).__name__}",
                metadata={**companion_metadata, "code": "adapter_failure"},
            )
        terminal = self.journal.append_effect(receipt)
        await self._emit(terminal)
        self._set_result(envelope.terminal, terminal)

    @staticmethod
    def _stable_delivery_scope(
        scope: ControlScope, targets: Sequence[WorkerRef], *, command_id: str,
    ) -> Optional[ControlScope]:
        """Translate a live worker mailbox to one exact continuation intent.

        Engine fallback is intentionally forbidden: it turns a private worker hint
        (or secret) into context for an unrelated same-engine task.  The command's
        deterministic continuation intent is both durable and no broader than the
        requested worker.  If the worker did not exist at admission time, create no
        deliverable context: display labels are reused across worker generations, so
        retaining the worker scope could leak a one-shot hint or secret to a future,
        unrelated worker.  The adapter still reports the command effect as UNKNOWN.
        """
        if scope.kind is not ScopeKind.WORKER:
            return scope
        selected = next(
            (target for target in targets if target.worker_id == scope.value), None)
        if selected is None:
            return None
        return ControlScope(
            kind=ScopeKind.INTENT,
            value=continuation_intent_id_for_command(command_id),
        )

    def _contents_match_for_clear(self, candidate: str, exact: str) -> bool:
        """Compare exact-clear values without persisting materialized secrets.

        Secret refs are deliberately opaque and distinct even when they contain
        the same plaintext.  Resolve any referenced operand through the optional
        coordinator-private resolver, compare UTF-8 bytes in constant time, and
        fail closed on every resolver/encoding error.
        """
        left = str(candidate)
        right = str(exact)
        if left.startswith("secret://") or right.startswith("secret://"):
            if self.secret_resolver is None:
                return False
            try:
                if left.startswith("secret://"):
                    left = self.secret_resolver(left)
                if right.startswith("secret://"):
                    right = self.secret_resolver(right)
            except Exception:
                return False
        try:
            return hmac.compare_digest(left.encode("utf-8"), right.encode("utf-8"))
        except Exception:
            return False

    def _plan_durable_companion(
        self, command: ControlCommand, targets: Sequence[WorkerRef],
    ) -> _DurableCompanion:
        payload = command.payload
        if command.action in {
            ControlAction.CLEAR_STANDING,
            ControlAction.RESET_GUIDANCE,
        }:
            exact_text = str(payload.get("text") or payload.get("hint") or "").strip()
            context_ids: list[str] = []
            source_command_ids: set[str] = set()
            for resource in self.journal.context_resources(active_only=True):
                if not resource.standing:
                    continue
                if exact_text and not self._contents_match_for_clear(
                        resource.content, exact_text):
                    continue
                context_ids.append(resource.context_id)
                source_command_id = str(
                    resource.metadata.get("source_command_id") or "").strip()
                if source_command_id:
                    source_command_ids.add(source_command_id)
            return _DurableCompanion(
                expiration_context_ids=tuple(context_ids),
                expiration_actor=command.actor,
                expiration_reason=command.action.value,
                metadata={
                    # This is the exact, closed causal set selected before the
                    # atomic expiration fence. Graph reconciliation may retract
                    # only these source ids and must preserve newer/unknown rows.
                    "matched_source_command_ids": sorted(source_command_ids),
                },
            )
        if command.action is ControlAction.ADD_CONTEXT:
            raw = payload.get("context")
            if isinstance(raw, str):
                raw = {"content": raw}
            if not isinstance(raw, dict):
                raise ValueError("add_context payload.context must be an object")
            requested_kind = ContextKind(str(raw.get("kind") or ContextKind.CLUE.value))
            delivery_scope = self._stable_delivery_scope(
                command.scope, targets, command_id=command.command_id)
            if delivery_scope is None:
                return _DurableCompanion()
            standing = bool(raw.get("standing", False))
            raw_max = raw.get("max_bindings")
            values = {
                **raw,
                # Authority fields are owned by the admitted outer command. An
                # operator cannot smuggle a worker-scoped resource into global or
                # self-label unverified text as trusted_system.
                "context_id": context_resource_id_for_command(command.command_id),
                "run_id": self.run_id,
                "scope": delivery_scope,
                "kind": requested_kind,
                "standing": standing,
                "max_bindings": (int(raw_max) if raw_max is not None else
                                 None if standing else 1),
                "taint": (ContextTaint.SECRET_REFERENCE
                           if (requested_kind is ContextKind.SECRET_REF
                               or str(raw.get("content") or "").startswith("secret://"))
                           else ContextTaint.OPERATOR_UNVERIFIED),
                "metadata": {
                    **(raw.get("metadata")
                       if isinstance(raw.get("metadata"), dict) else {}),
                    "source_command_id": command.command_id,
                    "action": command.action.value,
                    "command_scope": str(command.scope),
                },
            }
            values["created_at"] = command.created_at
            return _DurableCompanion(
                contexts=(ContextResource.model_validate(values),))
        if command.action is ControlAction.EXPIRE_CONTEXT:
            return _DurableCompanion(
                expiration_context_ids=(str(payload.get("context_id") or ""),),
                expiration_actor=command.actor,
                expiration_reason=str(
                    payload.get("reason") or "operator command"),
            )

        if command.action in {
            ControlAction.ASK,
            ControlAction.HINT,
            ControlAction.FOCUS,
            ControlAction.REDIRECT,
            ControlAction.DIRECTIVE,
            ControlAction.CORRECTION,
        }:
            url = str(payload.get("url") or payload.get("target_url") or "").strip()
            text = str(
                url if command.action is ControlAction.REDIRECT and url else
                payload.get("text") or payload.get("hint") or url or ""
            ).strip()
            if text:
                kind = (ContextKind.ENDPOINT if command.action is ControlAction.REDIRECT else
                        ContextKind.SECRET_REF if text.startswith("secret://") else
                        ContextKind.OBJECTIVE if command.action is ControlAction.FOCUS else
                        ContextKind.CLUE)
                ttl_s = payload.get("ttl_s")
                expires_at = (command.created_at + max(0.0, float(ttl_s))
                              if ttl_s is not None else None)
                standing = bool(payload.get("standing", False))
                raw_max = payload.get("max_bindings")
                max_bindings = (int(raw_max) if raw_max is not None else
                                None if standing else 1)
                delivery_scope = self._stable_delivery_scope(
                    command.scope, targets, command_id=command.command_id)
                if delivery_scope is not None:
                    return _DurableCompanion(contexts=(ContextResource(
                        context_id=context_resource_id_for_command(command.command_id),
                        run_id=self.run_id,
                        kind=kind, content=text, scope=delivery_scope,
                        taint=(ContextTaint.SECRET_REFERENCE
                               if text.startswith("secret://")
                               else ContextTaint.OPERATOR_UNVERIFIED),
                        standing=standing, max_bindings=max_bindings,
                        created_at=command.created_at,
                        expires_at=expires_at,
                        metadata={"source_command_id": command.command_id,
                                  "action": command.action.value,
                                  "command_scope": str(command.scope)},
                    ),))

        request_id = str(payload.get("request_id") or "").strip()
        if request_id and command.action in {
            ControlAction.ANSWER_DECISION,
            ControlAction.DISMISS,
            ControlAction.DISMISS_HELP,
        }:
            status = (DecisionStatus.DISMISSED if command.action in {
                ControlAction.DISMISS, ControlAction.DISMISS_HELP,
            } else DecisionStatus.ANSWERED)
            answer = DecisionAnswer(
                answer_id=f"DA-{command.command_id}", request_id=request_id,
                run_id=self.run_id, actor=command.actor, status=status,
                answer=str(payload.get("answer") or payload.get("text") or ""),
                created_at=command.created_at,
                metadata={"source_command_id": command.command_id},
            )
            answer_text = str(payload.get("answer") or payload.get("text") or "").strip()
            answer_context: Optional[ContextResource] = None
            if status is DecisionStatus.ANSWERED and answer_text:
                answer_kind = (ContextKind.SECRET_REF
                               if answer_text.startswith("secret://") else ContextKind.CLUE)
                request = self.journal.get_decision_request(request_id)
                answer_scope = command.scope
                answer_targets = tuple(targets)
                if request is not None:
                    answer_scope = request.blocking_scope
                    try:
                        answer_targets = tuple(self.registry.resolve(answer_scope))
                    except Exception:
                        answer_targets = ()
                if request is not None:
                    # An answer is a new execution edge, never mail for an already
                    # concluded intent and never an engine-wide broadcast.
                    delivery_scope = ControlScope(
                        kind=ScopeKind.INTENT,
                        value=continuation_intent_id_for_command(command.command_id),
                    )
                else:
                    delivery_scope = self._stable_delivery_scope(
                        answer_scope, answer_targets,
                        command_id=command.command_id)
                if delivery_scope is not None:
                    answer_context = ContextResource(
                        context_id=context_resource_id_for_command(command.command_id),
                        run_id=self.run_id, kind=answer_kind, content=answer_text,
                        scope=delivery_scope, standing=False, max_bindings=1,
                        created_at=command.created_at,
                        metadata={"source_command_id": command.command_id,
                                  "action": command.action.value,
                                  "request_id": request_id,
                                  "command_scope": str(command.scope),
                                  "blocking_scope": str(answer_scope)},
                    )
            return _DurableCompanion(
                decision_answer=answer,
                decision_context=answer_context,
                metadata={
                    "decision_closed": True,
                    "decision_status": status.value,
                    "request_id": request_id,
                },
            )
        return _DurableCompanion()
