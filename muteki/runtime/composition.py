"""The only Protocol 2 host composition path."""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path

from muteki.epistemic.authority import GateAuthority, PromotionAuthority
from muteki.epistemic.cas import ReceiptCAS
from muteki.epistemic.contracts import canonical_digest
from muteki.epistemic.sqlite_store import (
    CommandEvent,
    EpistemicSQLiteStore,
    ProjectionMutation,
)
from muteki.runtime.admission import SearchAdmission
from muteki.runtime.cognition import CognitiveContextAuthority
from muteki.runtime.contracts import ExecutionScope
from muteki.runtime.controller import BootRecoveryCapability, LiveHealthGuard
from muteki.runtime.run_catalog import RunCatalog
from muteki.runtime.reconciliation import (
    OrphanReconciler,
    ReconciliationDisposition,
)
from muteki.runtime.supervisor import RunSupervisor


@dataclass(frozen=True, slots=True)
class RunContext:
    run_id: str
    root: Path
    manifest_digest: str


@dataclass(slots=True)
class RunPorts:
    store: EpistemicSQLiteStore
    cas: ReceiptCAS
    guard: LiveHealthGuard
    promotion: PromotionAuthority
    gate: GateAuthority
    admission: SearchAdmission
    cognition: CognitiveContextAuthority


class HostRunFactory:
    def __init__(self, *, catalog: RunCatalog, artifacts) -> None:
        self._catalog = catalog
        self._artifacts = artifacts

    def open(self, *, run_id: str, boot_capability: BootRecoveryCapability,
             occurred_at_ns: int) -> tuple[RunContext, RunPorts]:
        sealed = self._catalog.sealed_run(run_id)
        root = Path(sealed["target_root"])
        store = EpistemicSQLiteStore.open(root / "epistemic-v2.db")
        anchor = store.run_anchor()
        if anchor["manifest_digest"] != sealed["manifest_digest"]:
            store.close()
            raise RuntimeError("catalog/run manifest mismatch")
        cas = ReceiptCAS(root / "receipt-cas")
        guard = LiveHealthGuard()
        guard.begin_boot_finalize(boot_capability)
        cognition = CognitiveContextAuthority(store=store, cas=cas)
        # C6 prompt invocation is a separate host-side observation chain.  An old
        # launch cannot be safely re-created after restart, so close every exact
        # invocation-bound/no-terminal record as UNKNOWN *before* BOOT_VERIFYING.
        # The store refuses BOOT_VERIFYING while a C6 claim is unresolved; ordering
        # recovery first preserves that fence without making an honest crash
        # unrecoverable.  This assumes the normal boot contract: the prior host is
        # no longer a live process owner.  Concurrent host takeover needs the later
        # durable host-ownership epoch work and remains outside Phase A.
        try:
            cognition.recover_dangling_prompt_invocations(
                guard=guard, occurred_at_ns=occurred_at_ns
            )
        except Exception:
            guard.deny()
            store.close()
            raise
        store.commit_command(
            command_id=f"BOOT_VERIFYING:{boot_capability.boot_epoch}",
            idempotency_key=f"BOOT_VERIFYING:{boot_capability.boot_epoch}",
            command_payload={"boot_epoch": boot_capability.boot_epoch,
                             "writer_epoch": boot_capability.writer_epoch},
            events=[CommandEvent(
                f"event:BOOT_VERIFYING:{boot_capability.boot_epoch}",
                "BOOT_VERIFYING", "host-run-factory", occurred_at_ns,
                {"boot_epoch": boot_capability.boot_epoch,
                 "writer_epoch": boot_capability.writer_epoch})],
            committed_at_ns=occurred_at_ns,
        )
        # Prompt-side reconciliation must happen before generic orphan handling:
        # a worker terminal may never overtake an unresolved durable C6 claim.
        reconciler = OrphanReconciler(store=store, guard=guard)
        inventory = reconciler.inventory()
        for lifecycle in inventory.permits:
            plan = reconciler.plan(lifecycle.permit_id)
            if plan.disposition is ReconciliationDisposition.MARK_UNKNOWN:
                reconciler.reconcile(
                    lifecycle.permit_id, occurred_at_ns=occurred_at_ns
                )
        inventory = reconciler.inventory()
        owners = store.lifecycle_owner_summary()
        if not inventory.is_unambiguous or any(owners.values()):
            guard.deny()
            store.close()
            raise RuntimeError(
                "boot reconciliation retained unresolved attempt/effect/budget owners"
            )
        attestation = canonical_digest({
            "anchor": anchor, "boot_epoch": boot_capability.boot_epoch,
            "state_checksum": store.verify().checksum,
            "writer_epoch": boot_capability.writer_epoch,
        })
        store.commit_command(
            command_id=f"BOOT_READY:{boot_capability.boot_epoch}",
            idempotency_key=f"BOOT_READY:{boot_capability.boot_epoch}",
            command_payload={"attestation_digest": attestation},
            events=[CommandEvent(
                f"event:BOOT_READY:{boot_capability.boot_epoch}", "BOOT_READY",
                "host-run-factory", occurred_at_ns,
                {"attestation_digest": attestation})],
            committed_at_ns=occurred_at_ns,
        )
        guard.open_admission(capability=boot_capability,
                             attestation_digest=attestation)
        context = RunContext(run_id, root, anchor["manifest_digest"])
        ports = RunPorts(
            store=store, cas=cas, guard=guard,
            promotion=PromotionAuthority(store),
            gate=GateAuthority(store=store, cas=cas, artifacts=self._artifacts),
            admission=SearchAdmission(store=store, guard=guard, cas=cas),
            cognition=cognition,
        )
        return context, ports

    @staticmethod
    def start_execution(*, ports: RunPorts, idempotency_key: str,
                        occurred_at_ns: int) -> tuple[ExecutionScope, RunSupervisor]:
        state = ports.store.state()
        generation = state.execution_generation + 1
        fence = state.run_fence_epoch + 1
        ports.store.commit_command(
            command_id=f"START_EXECUTION:{generation}",
            idempotency_key=idempotency_key,
            command_payload={"execution_generation": generation,
                             "run_fence_epoch": fence},
            events=[CommandEvent(
                f"event:START_EXECUTION:{generation}", "START_EXECUTION",
                "host-run-factory", occurred_at_ns,
                {"execution_generation": generation,
                 "run_fence_epoch": fence})],
            projection_mutations=[ProjectionMutation(
                "execution_start_guard",
                {
                    "execution_generation": generation,
                    "run_fence_epoch": fence,
                },
            )],
            authority_capability=ports.store._lifecycle_commit_capability,
            committed_at_ns=occurred_at_ns,
        )
        current = ports.store.state()
        scope = ExecutionScope(current.run_id, current.run_fence_epoch,
                               current.execution_generation)
        return scope, RunSupervisor(store=ports.store, scope=scope)
