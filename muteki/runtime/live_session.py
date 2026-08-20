"""Protocol 2 live run-plane adapter.

This is the narrow bridge between the existing CLI worker implementation and the
new authority kernel.  It does not make legacy graph rows authoritative: workers
report candidates, raw tool output is sealed, every launch requires an admitted
permit, and only the unchanged hardcoded flag gate can create a goal receipt.
"""

from __future__ import annotations

import asyncio
import time
from collections.abc import Awaitable, Callable, Mapping
from typing import Any
from urllib.parse import urlparse

from muteki.epistemic.authority import (
    GateInputRejected,
    resolve_accepted_flag_publication,
)
from muteki.epistemic.broker import CandidateBroker, CaptureSession
from muteki.epistemic.contracts import canonical_digest
from muteki.epistemic.sqlite_store import CommandEvent, ProjectionMutation
from muteki.runtime.admission import AdmissionRequest
from muteki.runtime.cognition import (
    COGNITIVE_CONTEXT_VERSION,
    CognitiveFeatureGateV1,
    DeliveredContextPacketV1,
    context_input_from_runtime,
)
from muteki.runtime.composition import RunPorts
from muteki.runtime.c6_transport import (
    C6HostLaunchBroker,
    C6HostLaunchProfileV1,
    C6HostPopenAdapter,
)
from muteki.runtime.contracts import (
    AttemptIdentity,
    AttemptPermit,
    EffectClass,
    ExecutionScope,
    EvaluationExecutionBindingV1,
    LeaseIdentity,
    RuntimeEvaluationBindingV2,
)
from muteki.runtime.controller import CommandClass
from muteki.runtime.ports import CandidateEnvelope
from muteki.runtime.progress import (
    ProgressKind,
    ProgressLedger,
    ProgressOccurrence,
)
from muteki.runtime.network import EnforcedNetworkPolicy, NetworkPolicyAuthority
from muteki.runtime.egress_proxy import LoopbackAllowlistProxy
from muteki.runtime.effects import EffectLedger
from muteki.runtime.supervisor import RunSupervisor
from muteki.runtime.usage import UsageReport


class Protocol2LiveRejected(RuntimeError):
    pass


class Protocol2RunSession:
    """Owns one execution scope and is the only worker scheduling path for it."""

    def __init__(
        self,
        *,
        ports: RunPorts,
        scope: ExecutionScope,
        supervisor: RunSupervisor,
        policy_digest: str,
        budget_account_id: str,
        per_attempt_budget: Mapping[str, int],
        max_barren_attempts: int,
        expected_goal_units: int,
        external_receipts: Mapping[str, str] | None = None,
        network_authority: NetworkPolicyAuthority | None = None,
        network_policy: EnforcedNetworkPolicy | None = None,
        provider_destination: str = "",
        provider_base_url: str = "",
        egress_proxy: LoopbackAllowlistProxy | None = None,
        completion_callback: Callable[["Protocol2RunSession", Mapping[str, str], bool],
                                      Mapping[str, str] | None] | None = None,
        evaluation_binding: EvaluationExecutionBindingV1 | None = None,
        context_packet_version: str = "",
        cognitive_feature_gate: CognitiveFeatureGateV1 | None = None,
    ) -> None:
        self.ports = ports
        self.scope = scope
        self.supervisor = supervisor
        self.policy_digest = policy_digest
        self.budget_account_id = budget_account_id
        if not isinstance(per_attempt_budget, Mapping):
            raise TypeError("per_attempt_budget must be a mapping")
        self.per_attempt_budget = {}
        for axis, amount in per_attempt_budget.items():
            if (
                type(axis) is not str
                or not axis
                or axis != axis.strip()
                or type(amount) is not int
                or amount < 0
            ):
                raise ValueError(
                    "per_attempt_budget requires canonical axes and "
                    "non-negative integers"
                )
            self.per_attempt_budget[axis] = amount
        if not self.per_attempt_budget:
            raise ValueError("per_attempt_budget is required")
        self.max_barren_attempts = max(1, int(max_barren_attempts))
        self.progress = ProgressLedger(
            store=ports.store, expected_goal_units=max(1, expected_goal_units))
        self.receipts = dict(external_receipts or {})
        self.receipts.setdefault("kernel", ports.store.state().checksum)
        self.receipts.setdefault("cas", canonical_digest({
            "root": str(ports.cas.root), "version": 1,
        }))
        self._ordinal = 0
        self._capture_count: dict[str, int] = {}
        self._accepted_flags: dict[tuple[str, str], str] = {}
        self._goal_gate_receipts: dict[str, str] = {}
        self._gate_input_count: dict[str, int] = {}
        self._gate_equivalent = True
        self._finished = False
        self._finalizing = False
        self._finalize_lock = asyncio.Lock()
        self._network_authority = network_authority
        self._network_policy = network_policy
        self._provider_destination = str(provider_destination)
        self._provider_base_url = str(provider_base_url)
        self._egress_proxy = egress_proxy
        self._last_lease: LeaseIdentity | None = None
        self._completion_callback = completion_callback
        if evaluation_binding is not None and type(
            evaluation_binding
        ) is not EvaluationExecutionBindingV1:
            raise TypeError(
                "evaluation_binding must be EvaluationExecutionBindingV1 or None"
            )
        if (
            evaluation_binding is not None
            and ports.store.run_anchor()["manifest_digest"]
            != evaluation_binding.run_manifest_digest
        ):
            raise Protocol2LiveRejected(
                "evaluation binding does not match the immutable run manifest"
            )
        self._evaluation_binding = evaluation_binding
        if type(context_packet_version) is not str:
            raise TypeError("context_packet_version must be an exact string")
        if context_packet_version not in {"", COGNITIVE_CONTEXT_VERSION}:
            raise ValueError("unsupported production context packet version")
        if cognitive_feature_gate is not None and type(
            cognitive_feature_gate
        ) is not CognitiveFeatureGateV1:
            raise TypeError(
                "cognitive_feature_gate must be CognitiveFeatureGateV1 or None"
            )
        if context_packet_version and cognitive_feature_gate is None:
            raise ValueError(
                "production context packets require a frozen cognitive feature gate"
            )
        if (
            cognitive_feature_gate is not None
            and context_packet_version not in {"", cognitive_feature_gate.context_version}
        ):
            raise ValueError("context packet version disagrees with feature gate")
        if evaluation_binding is not None and cognitive_feature_gate is not None:
            raise ValueError(
                "production context packets cannot be mixed with shadow evaluation"
            )
        self._cognitive_feature_gate = cognitive_feature_gate
        self.context_packet_version = (
            cognitive_feature_gate.context_version
            if cognitive_feature_gate is not None
            else ""
        )

    def _next_identity(self, worker: Any) -> tuple[AttemptIdentity, LeaseIdentity, str]:
        self._ordinal += 1
        worker_id = str(getattr(worker, "solver_id", "worker") or "worker")
        suffix = canonical_digest({
            "ordinal": self._ordinal, "scope": self.scope.digest,
            "worker_id": worker_id,
        })[:16]
        attempt_id = f"attempt-{self._ordinal}-{suffix}"
        attempt = AttemptIdentity(
            self.scope, "root", attempt_id, self._ordinal)
        lease = LeaseIdentity(
            attempt, f"lease-{suffix}", 1, self._ordinal)
        return attempt, lease, f"permit-{suffix}"

    def _bind_worker(self, worker: Any, *, permit: AttemptPermit) -> None:
        lease = permit.lease
        capture = CaptureSession(
            store=self.ports.store, cas=self.ports.cas, permit=permit)
        broker = CandidateBroker(store=self.ports.store, permit=permit)
        self._last_lease = lease
        if self._egress_proxy is not None:
            if getattr(worker, "container", None) is not None:
                raise Protocol2LiveRejected(
                    "live-local network sandbox currently requires local backend")
            extra_env = getattr(worker, "_extra_worker_env", None)
            if not isinstance(extra_env, dict):
                extra_env = {}
                worker._extra_worker_env = extra_env
            extra_env.update({
                "ALL_PROXY": self._egress_proxy.proxy_url,
                "HTTP_PROXY": self._egress_proxy.proxy_url,
                "HTTPS_PROXY": self._egress_proxy.proxy_url,
                "NO_PROXY": "",
                "MUTEKI_NETWORK_SANDBOX_PROFILE": (
                    self._egress_proxy.sandbox_profile),
            })
            if str(getattr(worker.driver, "name", "")) == "claude":
                parsed = urlparse(self._provider_base_url)
                suffix = parsed.path.rstrip("/")
                extra_env["ANTHROPIC_BASE_URL"] = (
                    self._egress_proxy.proxy_url + suffix)
        self._capture_count[lease.attempt.attempt_id] = 0
        self._gate_input_count[lease.attempt.attempt_id] = 0
        if (self._network_authority is not None
                and self._network_policy is not None
                and self._provider_destination
                and "egress" not in self.receipts):
            if (self._network_policy.mode != "allowlist"
                    or self._provider_destination not in self._network_policy.allowlist):
                raise Protocol2LiveRejected(
                    "provider destination is outside enforced allowlist")
            self.receipts["egress"] = self._network_authority.record_egress(
                receipt_id=f"provider:{canonical_digest(self._provider_destination)}",
                lease=lease, destination=self._provider_destination,
                policy=self._network_policy, occurred_at_ns=time.time_ns())

        def capture_tool_result(raw: str) -> str:
            self.supervisor.require_not_cancelled(permit.permit_id)
            data = str(raw or "").encode("utf-8", errors="replace")
            ordinal = self._capture_count[lease.attempt.attempt_id]
            chunk = capture.capture(
                capture_id=(f"{lease.attempt.attempt_id}:tool:{ordinal}"),
                stream="tool_result", data=data,
                occurred_at_ns=time.time_ns())
            self._capture_count[lease.attempt.attempt_id] = ordinal + 1
            return chunk.raw_digest

        def report_candidate(kind: str, payload: Mapping[str, Any]) -> str:
            self.supervisor.require_not_cancelled(permit.permit_id)
            self.ports.guard.authorize(
                CommandClass.ORDINARY, self.ports.store.state())
            candidate_id = canonical_digest({
                "attempt": lease.attempt.digest,
                "kind": str(kind), "payload": dict(payload),
            })
            receipt = broker.submit_candidate(CandidateEnvelope(
                candidate_id=candidate_id, lease=lease, kind=str(kind),
                payload=dict(payload)), occurred_at_ns=time.time_ns())
            self.progress.record(ProgressOccurrence(
                occurrence_id=f"candidate:{candidate_id}",
                branch_id=lease.attempt.branch_id,
                attempt_id=lease.attempt.attempt_id,
                kind=ProgressKind.CANDIDATE,
                basis_digest=candidate_id,
                canonical_seq=self.ports.store.state().head_seq,
            ), occurred_at_ns=time.time_ns())
            return receipt

        def commit_gate(flag: str, raw_output: str) -> bool:
            self.supervisor.require_not_cancelled(permit.permit_id)
            # CliSolver calls this only after its existing final gate (including
            # anti-laundering/operator-taint checks) has accepted.  GateAuthority
            # independently invokes the exact hardcoded gate over the sealed bytes.
            self.ports.guard.authorize(
                CommandClass.GATE, self.ports.store.state())
            raw = str(raw_output or "").encode("utf-8", errors="replace")
            flag_digest = canonical_digest(flag)
            candidate_id = canonical_digest({
                "attempt": lease.attempt.digest,
                "flag": flag_digest,
                "kind": "flag-candidate-v1",
            })
            gate_ordinal = self._gate_input_count[lease.attempt.attempt_id]
            gate_input = capture.seal_gate_input(
                capture_id=(
                    f"{lease.attempt.attempt_id}:gate:{gate_ordinal}:"
                    f"{flag_digest[:16]}"
                ),
                candidate_id=candidate_id,
                flag=flag,
                flag_format=str(worker.challenge.flag_format),
                policy_digest=self.policy_digest,
                data=raw,
                occurred_at_ns=time.time_ns(),
            )
            self._gate_input_count[lease.attempt.attempt_id] = gate_ordinal + 1
            evaluation_id = self.ports.gate.evaluation_id_for(gate_input)
            decision = self.ports.gate.evaluate(
                evaluation_id=evaluation_id,
                candidate_id=candidate_id,
                flag=flag,
                gate_input=gate_input,
                permit=permit,
                flag_format=str(worker.challenge.flag_format),
                policy_digest=self.policy_digest,
                occurred_at_ns=time.time_ns(),
            )
            self._gate_equivalent = self._gate_equivalent and decision.accepted
            if decision.accepted:
                self._accepted_flags.setdefault(
                    (lease.attempt.attempt_id, flag_digest),
                    decision.receipt_digest,
                )
            return decision.accepted

        worker._protocol2_mode = True
        worker._protocol2_capture_callback = capture_tool_result
        worker._protocol2_candidate_callback = report_candidate
        worker._protocol2_gate_callback = commit_gate

    @staticmethod
    def _begin_usage_window(worker: Any, attempt_id: str) -> tuple[Any, Any] | None:
        cost = getattr(worker, "cost", None)
        begin = getattr(cost, "begin_usage_window", None)
        finish = getattr(cost, "finish_usage_window", None)
        if not callable(begin) or not callable(finish):
            return None
        return cost, begin(attempt_id)

    @staticmethod
    def _finish_usage_window(
        window: tuple[Any, Any] | None,
        attempt_id: str,
    ) -> dict[str, int] | None:
        if window is None:
            return None
        cost, token = window
        value = cost.finish_usage_window(attempt_id, token)
        if not isinstance(value, Mapping):
            return None
        snapshot: dict[str, int] = {}
        for axis in ("tokens", "cost_micro_usd", "calls"):
            amount = value.get(axis)
            if type(amount) is not int or amount < 0:
                return None
            snapshot[axis] = amount
        return snapshot

    def _usage_report(
        self,
        *,
        attempt_id: str,
        finished_usage: Mapping[str, int] | None,
        elapsed_ms: int,
    ) -> UsageReport:
        observed: dict[str, int] = {}
        complete: set[str] = set()

        def observed_complete(axis: str, amount: int) -> None:
            if axis in self.per_attempt_budget:
                observed[axis] = amount
                complete.add(axis)

        observed_complete("attempts", 1)
        observed_complete("wall_ms", elapsed_ms)
        observed_complete("worker_ms", elapsed_ms)

        if finished_usage is not None and finished_usage.get("calls", 0) > 0:
            for axis in ("tokens", "cost_micro_usd"):
                finish = finished_usage.get(axis)
                if type(finish) is int and finish >= 0:
                    observed_complete(axis, finish)

        if "tool_calls" in self.per_attempt_budget:
            # Stream capture is a trustworthy lower bound, but unrestricted CLIs
            # may perform provider-side or hidden tool effects. Mark it PARTIAL so
            # settlement charges the reservation ceiling instead of undercounting.
            observed["tool_calls"] = self._capture_count.get(attempt_id, 0)

        return UsageReport.from_observed_and_reservation(
            reserved=self.per_attempt_budget,
            observed=observed,
            complete_axes=frozenset(complete),
        )

    def _close_usage(
        self,
        *,
        attempt_id: str,
        report: UsageReport,
        occurred_at_ns: int,
    ) -> bool:
        if report.has_unknown:
            self.ports.admission.hold_unknown_usage(
                attempt_id=attempt_id,
                revision=1,
                occurred_at_ns=occurred_at_ns,
                usage_report=report,
            )
            return False
        self.ports.admission.settle(
            attempt_id=attempt_id,
            usage_report=report,
            settlement_revision=1,
            occurred_at_ns=occurred_at_ns,
        )
        return True

    def schedule_worker(
        self,
        worker: Any,
        coroutine_factory: Callable[[], Awaitable[Any]],
        *,
        name: str,
    ):
        if self._evaluation_binding is not None:
            raise Protocol2LiveRejected(
                "evaluation-bound session must use schedule_shadow_worker"
            )
        return self._schedule_worker(
            worker, coroutine_factory, name=name, shadow=False
        )

    def schedule_shadow_worker(
        self,
        worker: Any,
        coroutine_factory: Callable[[], Awaitable[Any]],
        *,
        name: str,
    ):
        if self._evaluation_binding is None:
            raise Protocol2LiveRejected(
                "shadow worker scheduling requires an evaluation binding"
            )
        return self._schedule_worker(
            worker, coroutine_factory, name=name, shadow=True
        )

    def schedule_shadow_worker_v2(
        self,
        permit: AttemptPermit,
        coroutine_factory: Callable[[], Awaitable[Any]],
        *,
        runtime_binding: RuntimeEvaluationBindingV2,
        name: str,
    ):
        """Launch one already-admitted v2 role without a CLI delivery binding.

        Invocation materialization, worker callback installation, output capture,
        and proposal extraction belong to the separate delivery authority.  This
        boundary therefore accepts only the canonical permit and exact post-permit
        role binding and gives the coroutine no gate/progress/capture capabilities.
        """

        del name  # RunSupervisor owns the canonical task name.
        if self._evaluation_binding is not None:
            raise Protocol2LiveRejected(
                "v1 evaluation-bound session cannot use schedule_shadow_worker_v2"
            )
        if type(runtime_binding) is not RuntimeEvaluationBindingV2:
            raise TypeError(
                "runtime_binding must be RuntimeEvaluationBindingV2"
            )
        if type(permit) is not AttemptPermit:
            raise TypeError("permit must be AttemptPermit")
        if (
            runtime_binding.run_manifest_digest
            != self.ports.store.run_anchor()["manifest_digest"]
            or runtime_binding.run_id != self.scope.run_id
            or runtime_binding.run_fence_epoch != self.scope.run_fence_epoch
            or runtime_binding.execution_generation
            != self.scope.execution_generation
        ):
            raise Protocol2LiveRejected(
                "v2 runtime binding does not match the session run/scope"
            )
        if self._finished or self._finalizing:
            raise Protocol2LiveRejected("execution scope is already finalized")
        if (
            permit.permit_id != runtime_binding.permit_id
            or permit.digest != runtime_binding.permit_digest
            or permit.lease.attempt.digest
            != runtime_binding.attempt_identity_digest
            or permit.lease.attempt.scope.digest != runtime_binding.scope_digest
        ):
            raise Protocol2LiveRejected(
                "v2 permit does not match the exact role binding"
            )
        return self.supervisor.spawn_owned_shadow_v2(
            permit,
            coroutine_factory,
            runtime_binding=runtime_binding,
            now_ns=time.time_ns(),
        )

    def _schedule_worker(
        self,
        worker: Any,
        coroutine_factory: Callable[[], Awaitable[Any]],
        *,
        name: str,
        shadow: bool,
    ):
        del name  # RunSupervisor owns the canonical task name.
        if self._finished or self._finalizing:
            raise Protocol2LiveRejected("execution scope is already finalized")
        attempt, lease, permit_id = self._next_identity(worker)
        now = time.time_ns()
        delivered_context: DeliveredContextPacketV1 | None = None
        context_launch_broker: C6HostLaunchBroker | None = None

        def close_unadmitted_context(reason: str) -> None:
            if delivered_context is None:
                return
            try:
                self.ports.cognition.record_packet_unadmitted(
                    delivered=delivered_context,
                    reason=reason,
                    occurred_at_ns=time.time_ns(),
                )
            except Exception as exc:
                raise Protocol2LiveRejected(
                    "compiled C6 ContextPacket could not be closed before admission"
                ) from exc

        if self._cognitive_feature_gate is not None:
            # Phase-A C6 is deliberately limited to the audited host-side CLI
            # adapter. A generic worker could manufacture a fake start callback.
            from muteki.solver.cli_solver import CliSolver

            if type(worker) is not CliSolver:
                raise Protocol2LiveRejected(
                    "C6 Phase A requires the audited CliSolver host adapter"
                )
            if getattr(worker, "container", None) is not None:
                raise Protocol2LiveRejected(
                    "C6 Phase A requires direct local host-Popen execution"
                )
            if bool(getattr(worker, "_control_secret_values", ())):
                raise Protocol2LiveRejected(
                    "C6 Phase A does not support stdin/secret prompt transport"
                )
            if any(
                getattr(worker, attribute, None) is not None
                for attribute in (
                    "_cognitive_context_packet",
                    "_c6_invocation_runner",
                )
            ) or getattr(worker, "_cognitive_pending_prompt", None) is not None:
                raise Protocol2LiveRejected(
                    "C6 Phase A requires a fresh CliSolver without legacy context state"
                )
            bind_context = getattr(worker, "bind_cognitive_context", None)
            if not callable(bind_context):
                raise Protocol2LiveRejected(
                    "worker cannot accept an attempt-bound cognitive context"
                )
            delivered_context = self.ports.cognition.compile_for_attempt(
                attempt=attempt,
                context=context_input_from_runtime(
                    remaining_budget=self.ports.admission.remaining_budget(
                        account_id=self.budget_account_id
                    ),
                    policy_digest=self.policy_digest,
                ),
                feature_gate=self._cognitive_feature_gate,
                occurred_at_ns=now,
            )
            if (
                delivered_context.binding.feature_state_digest
                != self._cognitive_feature_gate.digest
            ):
                raise Protocol2LiveRejected(
                    "compiled cognitive context is not bound to the frozen feature gate"
                )
            try:
                profile = C6HostLaunchProfileV1(
                    driver_name=str(getattr(worker.driver, "name", ""))
                )
                context_launch_broker = C6HostLaunchBroker(
                    authority=self.ports.cognition,
                    delivered=delivered_context,
                    profile=profile,
                    # The adapter directly invokes the audited module-level local
                    # Popen primitive.  It is deliberately not a bound worker
                    # method, so an instance override cannot fabricate a release.
                    host_adapter=C6HostPopenAdapter(
                        authority=self.ports.cognition,
                        delivered=delivered_context,
                        driver=worker.driver,
                        profile=profile,
                        interlock=self.supervisor.c6_interlock,
                    ),
                )
                # This typed preflight happens before admission.  A worker that
                # cannot consume the host-owned invocation runner never leaves a
                # reserved permit behind.  The runner has no stage/release/UNKNOWN
                # methods; canonical writes remain in the host broker.
                bind_context(
                    delivered_context,
                    invocation_runner=context_launch_broker.runner,
                )
            except Exception as exc:
                close_unadmitted_context(
                    "worker rejected the host-owned C6 invocation runner"
                )
                raise Protocol2LiveRejected(
                    "worker cannot install the host-owned C6 invocation runner"
                ) from exc
        expires_at = now + max(
            1, int(self.per_attempt_budget.get("wall_ms", 1))) * 1_000_000
        fingerprint_body = {
            "engine": str(getattr(worker.driver, "name", "")),
            "intent": str(getattr(worker, "intent_id_assigned", "") or ""),
            "mode": str(getattr(worker, "mode", "")),
            "ordinal": self._ordinal,
        }
        if delivered_context is not None:
            fingerprint_body["context_packet_digest"] = (
                delivered_context.binding.packet_digest
            )
            fingerprint_body["cognitive_feature_state_digest"] = (
                delivered_context.binding.feature_state_digest
            )
        request = AdmissionRequest(
            attempt=attempt,
            lease=lease,
            permit_id=permit_id,
            account_id=self.budget_account_id,
            requested_budget=self.per_attempt_budget,
            conflict_keys=(f"worker:{worker.solver_id}",),
            effect_class=EffectClass.OBSERVABLE,
            fingerprint=canonical_digest(fingerprint_body),
            policy_digest=self.policy_digest,
            expires_at_ns=expires_at,
            context_packet=(
                delivered_context.binding if delivered_context is not None else None
            ),
        )
        try:
            if shadow:
                permit = self.ports.admission.admit_shadow(
                    request,
                    evaluation_binding=self._evaluation_binding,
                    occurred_at_ns=now,
                )
            else:
                permit = self.ports.admission.admit(request, occurred_at_ns=now)
        except Exception:
            close_unadmitted_context("admission rejected the compiled C6 ContextPacket")
            raise
        if delivered_context is not None:
            expected = permit.constraints.get("context_packet")
            if expected != delivered_context.binding.canonical_body():
                raise Protocol2LiveRejected(
                    "admitted permit rebound the cognitive context packet"
                )
            if context_launch_broker is None:
                raise Protocol2LiveRejected("strict C6 launch broker is unavailable")
        admission_rows = [
            row
            for row in self.ports.store.event_rows(kind="ATTEMPT_ADMITTED")
            if row["event_id"] == f"event:attempt:admit:{attempt.attempt_id}"
        ]
        if len(admission_rows) != 1:
            raise Protocol2LiveRejected(
                "canonical attempt admission did not resolve uniquely"
            )
        admission_receipt = self.ports.store.resolve_receipt_for_event(
            admission_rows[0]["event_digest"]
        )
        if admission_receipt.command_id != f"attempt:admit:{attempt.attempt_id}":
            raise Protocol2LiveRejected(
                "canonical attempt admission command identity diverged"
            )
        self.receipts.setdefault("admission", admission_receipt.digest)
        self._bind_worker(worker, permit=permit)
        self.progress.record(ProgressOccurrence(
            occurrence_id=f"activity:{attempt.attempt_id}",
            branch_id=attempt.branch_id,
            attempt_id=attempt.attempt_id,
            kind=ProgressKind.ACTIVITY,
            basis_digest=permit.lease.digest,
            canonical_seq=self.ports.store.state().head_seq,
        ), occurred_at_ns=now)

        async def owned() -> Any:
            started = time.monotonic_ns()
            usage_window = self._begin_usage_window(worker, attempt.attempt_id)
            finished_usage: dict[str, int] | None = None

            def finish_usage() -> dict[str, int] | None:
                nonlocal finished_usage
                if finished_usage is None:
                    finished_usage = self._finish_usage_window(
                        usage_window, attempt.attempt_id
                    )
                return finished_usage
            effects = EffectLedger(self.ports.store)
            operation_id = f"worker-effect:{permit.permit_id}"
            prepared = False
            dispatched = False
            try:
                effects.prepare(
                    operation_id=operation_id,
                    attempt_id=attempt.attempt_id,
                    effect_class=permit.effect_class,
                    conflict_keys=tuple(
                        permit.constraints.get("conflict_keys", ())
                    ),
                    occurred_at_ns=time.time_ns(),
                )
                prepared = True
                effects.transition(
                    operation_id=operation_id,
                    expected_state="prepared",
                    new_state="dispatch_may_have_started",
                    revision=1,
                    occurred_at_ns=time.time_ns(),
                )
                dispatched = True
                if context_launch_broker is not None:
                    # This runs only after RunSupervisor has atomically appended
                    # WORKER_LAUNCH_PREPARED and registered its interlock.
                    context_launch_broker.activate(permit=permit)
                outcome = await coroutine_factory()
                # A worker may swallow CancelledError and return an apparently valid
                # outcome. The supervisor-owned latch must fence that value before
                # any effect, usage, progress, goal, or solved-state publication.
                self.supervisor.require_not_cancelled(permit.permit_id)
                if delivered_context is not None:
                    stage_receipts = self.ports.cognition.require_verified_prompt_stages(
                        delivered=delivered_context,
                        permit=permit,
                    )
                    self.receipts.setdefault(
                        "context_prompt_release",
                        canonical_digest(stage_receipts),
                    )
                solved = bool(getattr(outcome, "solved", False))
                flags = list(getattr(outcome, "flags", []) or [])
                gate_bindings: list[tuple[str, str]] = []
                if solved:
                    if not flags:
                        raise Protocol2LiveRejected(
                            "solved outcome has no gate-bound goal unit"
                        )
                    for flag in flags:
                        if type(flag) is not str or not flag:
                            raise Protocol2LiveRejected(
                                "goal unit must be a non-empty string"
                            )
                        flag_digest = canonical_digest(flag)
                        cache_key = (attempt.attempt_id, flag_digest)
                        binding = self._accepted_flags.get(cache_key)
                        try:
                            publication = resolve_accepted_flag_publication(
                                store=self.ports.store,
                                cas=self.ports.cas,
                                attempt_digest=attempt.digest,
                                flag=flag,
                            )
                        except GateInputRejected as exc:
                            raise Protocol2LiveRejected(
                                "goal unit lacks an exact same-attempt gate receipt"
                            ) from exc
                        if (
                            binding is not None
                            and binding != publication.gate_receipt_digest
                        ):
                            raise Protocol2LiveRejected(
                                "cached gate receipt diverges from canonical authority"
                            )
                        binding = publication.gate_receipt_digest
                        self._accepted_flags[cache_key] = binding
                        gate_bindings.append((flag_digest, binding))
            except BaseException:
                if context_launch_broker is not None:
                    context_launch_broker.revoke(
                        "owned C6 coroutine is unwinding after an exception"
                    )
                if dispatched:
                    effects.transition(
                        operation_id=operation_id,
                        expected_state="dispatch_may_have_started",
                        new_state="unknown",
                        revision=2,
                        occurred_at_ns=time.time_ns(),
                    )
                elif prepared:
                    effects.transition(
                        operation_id=operation_id,
                        expected_state="prepared",
                        new_state="confirmed_not_applied",
                        revision=1,
                        occurred_at_ns=time.time_ns(),
                    )
                elapsed_ms = max(
                    0, (time.monotonic_ns() - started) // 1_000_000
                )
                report = self._usage_report(
                    attempt_id=attempt.attempt_id,
                    finished_usage=finish_usage(),
                    elapsed_ms=elapsed_ms,
                )
                self._close_usage(
                    attempt_id=attempt.attempt_id,
                    report=report,
                    occurred_at_ns=time.time_ns(),
                )
                raise
            elapsed_ms = max(0, (time.monotonic_ns() - started) // 1_000_000)
            effects.transition(
                operation_id=operation_id,
                expected_state="dispatch_may_have_started",
                new_state="observed",
                revision=2,
                occurred_at_ns=time.time_ns(),
            )
            report = self._usage_report(
                attempt_id=attempt.attempt_id,
                finished_usage=finish_usage(),
                elapsed_ms=elapsed_ms,
            )
            if not self._close_usage(
                attempt_id=attempt.attempt_id,
                report=report,
                occurred_at_ns=time.time_ns(),
            ):
                raise Protocol2LiveRejected(
                    "usage telemetry is UNKNOWN; attempt is held and cannot promote"
                )
            if solved:
                for flag_digest, gate_receipt in gate_bindings:
                    self._goal_gate_receipts.setdefault(
                        flag_digest, gate_receipt
                    )
                    self.progress.record(ProgressOccurrence(
                        occurrence_id=("goal:" + flag_digest),
                        branch_id=attempt.branch_id,
                        attempt_id=attempt.attempt_id,
                        kind=ProgressKind.GOAL_UNIT,
                        basis_digest=gate_receipt,
                        canonical_seq=self.ports.store.state().head_seq,
                        goal_unit=flag_digest,
                    ), occurred_at_ns=time.time_ns())
            else:
                self.progress.mark_attempt_barren(
                    branch_id=attempt.branch_id,
                    attempt_id=attempt.attempt_id,
                    occurred_at_ns=time.time_ns())
                barren = self.progress.projection.branches[
                    attempt.branch_id].barren_attempts
                if barren >= self.max_barren_attempts:
                    self.ports.store.commit_command(
                        command_id="SEARCH_PAUSED:BARREN",
                        idempotency_key="SEARCH_PAUSED:BARREN",
                        command_payload={"barren_attempts": barren},
                        events=[CommandEvent(
                            "event:SEARCH_PAUSED:BARREN", "SEARCH_PAUSED",
                            "search-kernel", time.time_ns(),
                            {"barren_attempts": barren})],
                        committed_at_ns=time.time_ns(),
                    )
            if context_launch_broker is not None:
                context_launch_broker.revoke(
                    "owned C6 coroutine completed before supervisor terminalization"
                )
            return outcome

        if shadow:
            return self.supervisor.spawn_owned_shadow(
                permit, owned, now_ns=now
            )

        def hold_preentry_unknown_usage() -> None:
            self.ports.admission.hold_unknown_usage(
                attempt_id=attempt.attempt_id,
                revision=1,
                occurred_at_ns=time.time_ns(),
            )

        return self.supervisor.spawn_owned(
            permit,
            owned,
            now_ns=now,
            on_preentry_cancel=hold_preentry_unknown_usage,
        )

    def _attest_s4e_closure(self, *, solved: bool, occurred_at_ns: int) -> None:
        events = self.ports.store.event_rows()

        def rows(kind: str) -> tuple[dict[str, Any], ...]:
            return tuple(row for row in events if row["kind"] == kind)

        admissions = rows("ATTEMPT_ADMITTED")
        launches = rows("WORKER_LAUNCH_PREPARED")
        terminals = rows("WORKER_TERMINAL") + rows("WORKER_UNKNOWN")
        settled = rows("BUDGET_SETTLED")
        usage_unknown = rows("BUDGET_USAGE_UNKNOWN")
        effect_prepared = rows("EFFECT_PREPARED")
        effect_observed = rows("EFFECT_OBSERVED")
        effect_unknown = rows("EFFECT_UNKNOWN")
        captures = rows("CAPTURE_CHUNK_SEALED")
        manifests = rows("CAPTURE_MANIFEST_ADVANCED")
        accepted = rows("FLAG_ACCEPTED")

        orphaned: list[str] = []
        ambiguous: list[str] = []
        for admission in admissions:
            permit_id = admission["payload"].get("permit_id")
            attempt_id = admission["payload"].get("attempt_id")
            permit_launches = [
                row for row in launches
                if row["payload"].get("permit_id") == permit_id
            ]
            permit_terminals = [
                row for row in terminals
                if row["payload"].get("permit_id") == permit_id
            ]
            attempt_budget = [
                row for row in settled
                if row["payload"].get("attempt_id") == attempt_id
            ]
            attempt_unknown = [
                row for row in usage_unknown
                if row["payload"].get("attempt_id") == attempt_id
            ]
            if len(permit_launches) != 1 or len(permit_terminals) != 1:
                orphaned.append(str(permit_id))
            if len(attempt_budget) + len(attempt_unknown) != 1:
                ambiguous.append(str(attempt_id))

        capture_pairs = {
            row["payload"].get("manifest_digest") for row in captures
        } == {
            row["payload"].get("manifest_digest") for row in manifests
        } and len(captures) == len(manifests)
        accepted_capture_digests = {
            row["payload"].get("capture_event_digest") for row in accepted
        }
        canonical_capture_digests = {row["event_digest"] for row in captures}
        gate_inputs_resolve = accepted_capture_digests.issubset(
            canonical_capture_digests
        )
        effects_close = (
            len(effect_prepared) == len(effect_observed)
            and not effect_unknown
        )
        worker_unknown = bool(rows("WORKER_UNKNOWN"))
        usage_closes = (
            len(settled) == len(admissions)
            and not usage_unknown
            and not ambiguous
        )
        orphan_free = not orphaned and not ambiguous and not worker_unknown
        gate_closes = (
            (not solved or bool(accepted))
            and capture_pairs
            and gate_inputs_resolve
            and self.gate_equivalent
        )
        all_clean = usage_closes and orphan_free and effects_close and gate_closes

        permit_body = {
            "admission_event_digests": [row["event_digest"] for row in admissions],
            "launch_event_digests": [row["event_digest"] for row in launches],
        }
        capture_body = {
            "capture_event_digests": [row["event_digest"] for row in captures],
            "manifest_event_digests": [row["event_digest"] for row in manifests],
            "paired": capture_pairs,
        }
        gate_input_body = {
            "accepted_event_digests": [row["event_digest"] for row in accepted],
            "capture_event_digests": sorted(
                str(item) for item in accepted_capture_digests if item
            ),
            "resolves": gate_inputs_resolve,
        }
        usage_body = {
            "settled_event_digests": [row["event_digest"] for row in settled],
            "unknown_event_digests": [
                row["event_digest"] for row in usage_unknown
            ],
            "complete": usage_closes,
        }
        orphan_body = {
            "ambiguous_attempt_ids": sorted(ambiguous),
            "orphaned_permit_ids": sorted(orphaned),
            "worker_unknown": worker_unknown,
            "complete": orphan_free,
        }
        schema_body = {
            "name": "muteki-s4e-closure",
            "version": 1,
            "required_components": (
                "canonical_permit",
                "capture_manifest",
                "gate_input",
                "orphan_summary",
                "usage_closure",
            ),
        }
        components = {
            "canonical_permit": canonical_digest(permit_body),
            "capture_manifest": canonical_digest(capture_body),
            "gate_input": canonical_digest(gate_input_body),
            "orphan_summary": canonical_digest(orphan_body),
            "s4e_schema": canonical_digest(schema_body),
            "usage_closure": canonical_digest(usage_body),
        }
        closure_payload = {
            "all_clean": all_clean,
            "components": components,
            "invariants": {
                "capture_pairs": capture_pairs,
                "effects_close": effects_close,
                "gate_closes": gate_closes,
                "orphan_free": orphan_free,
                "usage_closes": usage_closes,
            },
            "scope_digest": self.scope.digest,
            "schema": schema_body,
            "solved": solved,
        }
        if solved and not all_clean:
            raise Protocol2LiveRejected(
                "S4-E closure is incomplete; production promotion is forbidden"
            )
        generation = self.scope.execution_generation
        result = self.ports.store.commit_command(
            command_id=f"S4E_CLOSURE_ATTESTED:{generation}",
            idempotency_key=f"S4E_CLOSURE_ATTESTED:{generation}",
            command_payload=closure_payload,
            events=[CommandEvent(
                f"event:S4E_CLOSURE_ATTESTED:{generation}",
                "S4E_CLOSURE_ATTESTED",
                "protocol2-live-session",
                occurred_at_ns,
                closure_payload,
            )],
            projection_mutations=[ProjectionMutation(
                "s4e_closure_guard", closure_payload
            )],
            authority_capability=(
                self.ports.store._lifecycle_commit_capability
            ),
            committed_at_ns=occurred_at_ns,
        )
        self.receipts.update(components)
        self.receipts["s4e_closure"] = result.receipt_digest

    async def finalize(self, *, solved: bool) -> Mapping[str, str]:
        async with self._finalize_lock:
            return await self._finalize_locked(solved=solved)

    async def _finalize_locked(self, *, solved: bool) -> Mapping[str, str]:
        if self._finished:
            return dict(self.receipts)
        if not self._finalizing:
            self._finalizing = True
            self.supervisor.quiesce()
        await self.supervisor.drain()
        if self._egress_proxy is not None:
            observation = self._egress_proxy.close()
            if solved and observation.allowed_connects < 1:
                raise Protocol2LiveRejected(
                    "live-local canary has no observed provider egress")
            if self._last_lease is not None:
                observed_receipt = self._network_authority.record_egress(
                    receipt_id=("provider-observed:"
                                + canonical_digest(self._provider_destination)),
                    lease=self._last_lease,
                    destination=self._provider_destination,
                    policy=self._network_policy,
                    occurred_at_ns=time.time_ns(),
                    observed=observation.allowed_connects > 0,
                    observation_digest=observation.digest,
                )
                if observation.allowed_connects > 0:
                    self.receipts["egress_observation"] = observed_receipt
        now = time.time_ns()
        goal_gate_receipts = tuple(
            (flag_digest, self._goal_gate_receipts[flag_digest])
            for flag_digest in sorted(self._goal_gate_receipts)
        )
        if solved:
            if not goal_gate_receipts or not self.progress.projection.goal_complete:
                raise Protocol2LiveRejected(
                    "legacy outcome claimed solved without Protocol 2 gate/progress receipt")
            goal_result = self.ports.store.commit_command(
                command_id=f"GOAL_COMPLETED:{self.scope.execution_generation}",
                idempotency_key=f"GOAL_COMPLETED:{self.scope.execution_generation}",
                command_payload={"gate_receipts": goal_gate_receipts},
                events=[CommandEvent(
                    f"event:GOAL_COMPLETED:{self.scope.execution_generation}",
                    "GOAL_COMPLETED",
                    "protocol2-live-session", now,
                    {"gate_receipts": goal_gate_receipts})],
                projection_mutations=[ProjectionMutation(
                    "goal_commit_guard",
                    {"gate_receipts": goal_gate_receipts},
                )],
                authority_capability=(
                    self.ports.store._lifecycle_commit_capability
                ),
                committed_at_ns=now,
            )
            self.receipts["gate"] = goal_result.receipt_digest
        elif self.ports.store.state().search_mode.value == "active":
            self.ports.store.commit_command(
                command_id=(
                    f"SEARCH_PAUSED:RUN_FINISHED:"
                    f"{self.scope.execution_generation}"
                ),
                idempotency_key=(
                    f"SEARCH_PAUSED:RUN_FINISHED:"
                    f"{self.scope.execution_generation}"
                ),
                command_payload={"reason": "run_finished_without_goal"},
                events=[CommandEvent(
                    (f"event:SEARCH_PAUSED:RUN_FINISHED:"
                     f"{self.scope.execution_generation}"),
                    "SEARCH_PAUSED",
                    "protocol2-live-session", now,
                    {"reason": "run_finished_without_goal"})],
                committed_at_ns=now,
            )
        if (
            not solved
            and self.ports.store.state().run_execution.value == "running"
        ):
            generation = self.scope.execution_generation
            self.ports.store.commit_command(
                command_id=f"EXECUTION_STOP_REQUESTED:{generation}",
                idempotency_key=f"EXECUTION_STOP_REQUESTED:{generation}",
                command_payload={"scope_digest": self.scope.digest},
                events=[CommandEvent(
                    f"event:EXECUTION_STOP_REQUESTED:{generation}",
                    "EXECUTION_STOP_REQUESTED",
                    "protocol2-live-session",
                    time.time_ns(),
                    {"scope_digest": self.scope.digest},
                )],
                projection_mutations=[ProjectionMutation(
                    "execution_stop_guard",
                    {"scope_digest": self.scope.digest},
                )],
                authority_capability=(
                    self.ports.store._lifecycle_commit_capability
                ),
                committed_at_ns=time.time_ns(),
            )
        generation = self.scope.execution_generation
        drain_result = self.ports.store.commit_command(
            command_id=f"EXECUTION_SCOPE_DRAINED:{generation}",
            idempotency_key=f"EXECUTION_SCOPE_DRAINED:{generation}",
            command_payload={"scope_digest": self.scope.digest},
            events=[CommandEvent(
                f"event:EXECUTION_SCOPE_DRAINED:{generation}",
                "EXECUTION_SCOPE_DRAINED",
                "protocol2-live-session", time.time_ns(),
                {"scope_digest": self.scope.digest})],
            projection_mutations=[ProjectionMutation(
                "execution_drain_guard",
                {"scope_digest": self.scope.digest},
            )],
            authority_capability=self.ports.store._lifecycle_commit_capability,
            committed_at_ns=time.time_ns(),
        )
        self.receipts["execution"] = drain_result.receipt_digest
        before = self.ports.store.runtime_projection_digest()
        after = self.ports.store.rebuild_runtime_projections()
        if before != after:
            raise Protocol2LiveRejected("runtime projection rebuild diverged")
        projection_payload = {
            "after": after,
            "before": before,
            "equivalent": before == after,
            "scope_digest": self.scope.digest,
        }
        projection_result = self.ports.store.commit_command(
            command_id=f"PROJECTION_REBUILD_VERIFIED:{generation}",
            idempotency_key=f"PROJECTION_REBUILD_VERIFIED:{generation}",
            command_payload=projection_payload,
            events=[CommandEvent(
                f"event:PROJECTION_REBUILD_VERIFIED:{generation}",
                "PROJECTION_REBUILD_VERIFIED",
                "protocol2-live-session",
                time.time_ns(),
                projection_payload,
            )],
            projection_mutations=[ProjectionMutation(
                "projection_verify_guard", projection_payload
            )],
            authority_capability=self.ports.store._lifecycle_commit_capability,
            committed_at_ns=time.time_ns(),
        )
        self.receipts["projection_rebuild"] = projection_result.receipt_digest
        if self._cognitive_feature_gate is not None:
            context_releases = self.ports.cognition.verify_scope_prompt_stage_closure(
                scope_digest=self.scope.digest
            )
            if not context_releases:
                raise Protocol2LiveRejected(
                    "strict C6 run has no canonical prompt-stage releases"
                )
            self.receipts["context_prompt_closure"] = canonical_digest(
                context_releases
            )
        self._attest_s4e_closure(
            solved=solved, occurred_at_ns=time.time_ns()
        )
        self.receipts["execution_state"] = self.ports.store.state().checksum
        if self._completion_callback is not None:
            extra = self._completion_callback(self, dict(self.receipts), solved)
            if extra:
                self.receipts.update({str(k): str(v) for k, v in extra.items()})
        self._finished = True
        self.ports.guard.deny()
        return dict(self.receipts)

    @property
    def gate_equivalent(self) -> bool:
        return self._gate_equivalent and bool(self._goal_gate_receipts)
