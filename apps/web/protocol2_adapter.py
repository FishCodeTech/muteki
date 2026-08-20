"""Web composition root for Protocol 2 live-local canaries.

Protocol 1 remains available for existing runs.  A request must explicitly select
``protocol: 2`` and satisfy the finite-budget/single-worker canary contract; there
is no hot switch and no permissive fallback.
"""

from __future__ import annotations

import asyncio
import json
import os
import sys
import time
from pathlib import Path
from typing import Any, Mapping, Sequence
from urllib.parse import urlparse

from muteki.epistemic.authority import (
    AcceptedFlagPublicationV1,
    GateInputRejected,
    resolve_accepted_flag_publication,
)
from muteki.epistemic.cas import CASIntegrityError, ReceiptCAS
from muteki.epistemic.contracts import canonical_digest
from muteki.epistemic.sqlite_store import (
    CommandEvent,
    EpistemicSQLiteStore,
    FlagAcceptedOutboxV1,
    IntegrityError,
    ProjectionMutation,
)
from muteki.eval.manifests import (
    EVAL_CONTRACT_VERSION,
    TrialAssignment,
    TrialIdentity,
)
from muteki.runtime.canary import (
    CanaryEvidence,
    CanaryLevel,
    admit_canary,
    missing_s4e_receipts,
)
from muteki.runtime.composition import HostRunFactory
from muteki.runtime.closure import ClosureResolutionError, resolve_s4e_closure
from muteki.runtime.cognition import CognitiveFeatureGateV1
from muteki.runtime.controller import BootRecoveryCapability
from muteki.runtime.contracts import AttemptIdentity, ExecutionScope, LeaseIdentity
from muteki.runtime.egress_proxy import LoopbackAllowlistProxy
from muteki.runtime.live_session import Protocol2RunSession
from muteki.runtime.network import NetworkPolicyAuthority
from muteki.runtime.run_catalog import RunCatalog
from muteki.solver.cli_driver import driver_for


class Protocol2Unavailable(RuntimeError):
    pass


class _CliToolPolicyAdapter:
    """Read back the actual CLI web-tool isolation contract.

    Local full-model workers still need egress to their configured model provider.
    The black-box boundary required by this project is that the CLI cannot expose its
    native WebSearch/WebFetch surface.  We verify the concrete argv, not a manifest
    boolean. Cursor therefore fails this adapter closed.
    """

    def __init__(self, profiles: Sequence[Mapping[str, Any]]) -> None:
        self._profiles = [dict(profile) for profile in profiles]
        self._policy: dict[str, Any] = {}

    def _verify(self) -> None:
        if sys.platform != "darwin" or not Path("/usr/bin/sandbox-exec").is_file():
            raise Protocol2Unavailable(
                "live-local host egress enforcement is unavailable"
            )
        for profile in self._profiles:
            driver = driver_for(profile)
            if not bool(getattr(driver, "offline_web_isolation", False)):
                raise Protocol2Unavailable(
                    f"profile {profile.get('id')} cannot isolate native web tools"
                )
            argv = driver.build_execute(
                "Reply exactly READY",
                driver.new_session(),
                web_access=False,
                kb_access=False,
            )
            if driver.name == "claude":
                try:
                    start = argv.index("--disallowed-tools")
                    end = argv.index("--")
                except ValueError as exc:
                    raise Protocol2Unavailable(
                        "Claude offline deny flags are absent"
                    ) from exc
                denied = set(argv[start + 1 : end])
                if not {"WebSearch", "WebFetch"}.issubset(denied):
                    raise Protocol2Unavailable(
                        "Claude native web tools are not both denied"
                    )
            elif driver.name == "codex" and "--search" in argv:
                raise Protocol2Unavailable("Codex native search remained enabled")
            elif driver.name == "cursor":
                raise Protocol2Unavailable("Cursor has no enforceable offline mode")

    def apply(self, policy: Mapping) -> Mapping:
        self._verify()
        self._policy = {
            "allowlist": tuple(policy.get("allowlist") or ()),
            "mode": str(policy.get("mode") or ""),
        }
        return dict(self._policy)

    def readback(self) -> Mapping:
        self._verify()
        return dict(self._policy)


class Protocol2WebAdapter:
    def __init__(self, *, control_root: Path) -> None:
        self.root = Path(control_root) / "protocol2"
        self.catalog = RunCatalog.open_or_create(root=self.root)
        self._live: dict[str, Protocol2RunSession] = {}

    def has_run(self, run_id: str) -> bool:
        return self.catalog.has_run(run_id)

    def list_run_ids(self) -> tuple[str, ...]:
        return self.catalog.list_run_ids()

    def recover_flag_publications(
        self, run_id: str
    ) -> tuple[AcceptedFlagPublicationV1, ...]:
        """Resolve accepted flag handoffs without inferring run success."""

        view = self.catalog.run_view(run_id)
        root = Path(view["target_root"])
        target = root / "epistemic-v2.db"
        if not target.is_file():
            raise Protocol2Unavailable("canonical run store is unavailable")
        store = EpistemicSQLiteStore.open(target)
        cas = ReceiptCAS(root / "receipt-cas")
        publications: list[AcceptedFlagPublicationV1] = []
        try:
            with store.stable_read_snapshot():
                try:
                    store.verify()
                    rows = store._conn.execute(
                        "SELECT payload_json FROM immutable_outbox "
                        "WHERE topic='flag.accepted' ORDER BY outbox_id"
                    ).fetchall()
                    for row in rows:
                        payload = json.loads(row[0])
                        accepted = FlagAcceptedOutboxV1.from_payload(payload)
                        flag_bytes = cas.read_verified(
                            accepted.flag_object_digest
                        )
                        flag = flag_bytes.decode(
                            accepted.flag_encoding, errors="strict"
                        )
                        publications.append(resolve_accepted_flag_publication(
                            store=store,
                            cas=cas,
                            attempt_digest=accepted.attempt_digest,
                            flag=flag,
                        ))
                except (
                    CASIntegrityError,
                    GateInputRejected,
                    IntegrityError,
                    json.JSONDecodeError,
                    OSError,
                    TypeError,
                    UnicodeError,
                    ValueError,
                ) as exc:
                    raise Protocol2Unavailable(
                        "accepted flag publication did not resolve"
                    ) from exc
        finally:
            store.close()
        return tuple(publications)

    def run_view(self, run_id: str) -> dict[str, Any]:
        return self.catalog.run_view(run_id)

    def _catalog_policy(self, run_id: str) -> dict[str, Any]:
        run = self.catalog._store.catalog_run(run_id)
        provision = self.catalog._store.provision_status(run["operation_id"])
        drafts = [
            row for row in self.catalog._store.event_rows(kind="DRAFT_CREATED")
            if row["payload"].get("draft_id") == provision["draft_id"]
        ]
        if len(drafts) != 1 or type(drafts[0]["payload"].get("policy")) is not dict:
            raise Protocol2Unavailable("catalog policy does not resolve uniquely")
        return dict(drafts[0]["payload"]["policy"])

    def _catalog_policy_digest(self, run_id: str) -> str:
        return canonical_digest(self._catalog_policy(run_id))

    @staticmethod
    def _exact_event_receipt(
        *,
        store: EpistemicSQLiteStore,
        receipt_digest: str,
        command_id: str,
        event_id: str,
        kind: str,
    ) -> dict[str, Any]:
        """Resolve one named receipt through its exact canonical command/event."""
        try:
            receipt = store.resolve_receipt(receipt_digest)
            rows = [
                row
                for row in store.event_rows(kind=kind)
                if row["event_id"] == event_id
                and store.receipt_digest_for_event(row["event_digest"])
                == receipt_digest
            ]
        except (IntegrityError, KeyError, TypeError, ValueError) as exc:
            raise Protocol2Unavailable(
                f"canonical {kind} receipt did not resolve"
            ) from exc
        if receipt.command_id != command_id or len(rows) != 1:
            raise Protocol2Unavailable(
                f"canonical {kind} command/event identity diverged"
            )
        return rows[0]

    @staticmethod
    def _single_event_receipt(
        *,
        store: EpistemicSQLiteStore,
        command_id: str,
        event_id: str,
        kind: str,
    ) -> str:
        rows = [
            row
            for row in store.event_rows(kind=kind)
            if row["event_id"] == event_id
        ]
        if len(rows) != 1:
            raise Protocol2Unavailable(
                f"canonical {kind} event does not resolve uniquely"
            )
        try:
            receipt = store.resolve_receipt_for_event(rows[0]["event_digest"])
        except (IntegrityError, KeyError, TypeError, ValueError) as exc:
            raise Protocol2Unavailable(
                f"canonical {kind} receipt did not resolve"
            ) from exc
        if receipt.command_id != command_id:
            raise Protocol2Unavailable(
                f"canonical {kind} command identity diverged"
            )
        return receipt.digest

    def _resolve_live_closure(
        self, *, run_id: str, receipt_chain: Mapping[str, str]
    ):
        view = self.catalog.run_view(run_id)
        root = Path(view["target_root"])
        target = root / "epistemic-v2.db"
        if not target.is_file():
            raise Protocol2Unavailable("canonical run store is unavailable")
        store = EpistemicSQLiteStore.open(target)
        try:
            resolved = resolve_s4e_closure(
                store=store,
                cas=ReceiptCAS(root / "receipt-cas"),
                receipt_chain=receipt_chain,
            )
        except (CASIntegrityError, ClosureResolutionError) as exc:
            raise Protocol2Unavailable(
                "S4-E semantic closure did not resolve"
            ) from exc
        finally:
            store.close()
        if resolved.policy_digest != self._catalog_policy_digest(run_id):
            raise Protocol2Unavailable(
                "S4-E policy digest is not bound to the catalog assignment"
            )
        return resolved

    def _resolve_live_status_evidence(
        self,
        *,
        run_id: str,
        receipt_chain: Mapping[str, str],
        canary_seq: int,
    ) -> None:
        """Bind every non-release canary name to canonical run/catalog history."""
        catalog_store = self.catalog._store
        catalog_run = catalog_store.catalog_run(run_id)
        operation_id = catalog_run["operation_id"]
        provision = catalog_store.provision_status(operation_id)
        policy = self._catalog_policy(run_id)
        if (
            provision["run_id"] != run_id
            or provision["state"] != "sealed"
            or policy.get("run_id") != run_id
            or policy.get("protocol") != 2
            or policy.get("offline") is not True
        ):
            raise Protocol2Unavailable("catalog run/policy identity diverged")
        limits = policy.get("budget")
        if type(limits) is not dict or type(limits.get("wall_ms")) is not int:
            raise Protocol2Unavailable("catalog canary budget is malformed")

        schema = self._exact_event_receipt(
            store=catalog_store,
            receipt_digest=receipt_chain["schema"],
            command_id=f"provision:sealed:{operation_id}",
            event_id=f"event:provision:sealed:{operation_id}",
            kind="RUN_SEALED",
        )
        expected_schema = {
            "anchor_digest": catalog_run["anchor_digest"],
            "operation_id": operation_id,
            "owner_epoch": provision["owner_epoch"],
            "run_id": run_id,
        }
        if schema["payload"] != expected_schema:
            raise Protocol2Unavailable("catalog schema/run binding diverged")

        platform_operation = f"platform:egress-proxy:{run_id}"
        platform_admission = self._exact_event_receipt(
            store=catalog_store,
            receipt_digest=receipt_chain["platform_admission"],
            command_id=f"{platform_operation}:admitted",
            event_id=f"event:{platform_operation}:admitted",
            kind="PLATFORM_OPERATION_ADMITTED",
        )
        admission_payload = platform_admission["payload"]
        if set(admission_payload) != {
            "conflict_key",
            "destination",
            "operation_id",
            "owner_epoch",
            "run_id",
            "wall_ms",
        }:
            raise Protocol2Unavailable("platform admission payload is malformed")
        destination = admission_payload["destination"]
        if (
            type(destination) is not str
            or not destination
            or admission_payload
            != {
                "conflict_key": f"egress-proxy:{run_id}",
                "destination": destination,
                "operation_id": platform_operation,
                "owner_epoch": 1,
                "run_id": run_id,
                "wall_ms": limits["wall_ms"],
            }
        ):
            raise Protocol2Unavailable("platform admission identity diverged")

        target_root = Path(provision["target_root"])
        target = target_root / "epistemic-v2.db"
        if not target.is_file():
            raise Protocol2Unavailable("canonical run store is unavailable")
        store = EpistemicSQLiteStore.open(target)
        try:
            policy_digest = canonical_digest(policy)
            attachments = catalog_store.draft_attachments(provision["draft_id"])
            attachment_digests = [item["digest"] for item in attachments]
            cas = ReceiptCAS(target_root / "receipt-cas")
            for item in attachments:
                if len(cas.read_verified(item["digest"])) != item["byte_count"]:
                    raise Protocol2Unavailable("canonical CAS byte count diverged")
            cas_event = self._exact_event_receipt(
                store=store,
                receipt_digest=receipt_chain["cas"],
                command_id="CREATE_RUN",
                event_id="event:CREATE_RUN",
                kind="RUN_CREATED",
            )
            expected_create = {
                "attachment_digests": attachment_digests,
                "manifest_digest": provision["manifest_digest"],
            }
            if cas_event["payload"] != expected_create:
                raise Protocol2Unavailable("canonical CAS/run binding diverged")
            create_receipt = store.resolve_receipt(receipt_chain["cas"])
            expected_anchor = canonical_digest(
                {
                    "attachment_digests": attachment_digests,
                    "create_receipt_digest": receipt_chain["cas"],
                    "manifest_digest": provision["manifest_digest"],
                    "run_id": run_id,
                    "state_checksum": create_receipt.payload["state_checksum"],
                }
            )
            if catalog_run["anchor_digest"] != expected_anchor:
                raise Protocol2Unavailable("catalog/run anchor binding diverged")

            boot_verifying_digest = self._single_event_receipt(
                store=store,
                command_id="BOOT_VERIFYING:1",
                event_id="event:BOOT_VERIFYING:1",
                kind="BOOT_VERIFYING",
            )
            boot_verifying = self._exact_event_receipt(
                store=store,
                receipt_digest=boot_verifying_digest,
                command_id="BOOT_VERIFYING:1",
                event_id="event:BOOT_VERIFYING:1",
                kind="BOOT_VERIFYING",
            )
            if boot_verifying["payload"] != {
                "boot_epoch": 1,
                "writer_epoch": 1,
            }:
                raise Protocol2Unavailable("kernel boot identity diverged")
            verifying_receipt = store.resolve_receipt(boot_verifying_digest)
            expected_attestation = canonical_digest(
                {
                    "anchor": store.run_anchor(),
                    "boot_epoch": 1,
                    "state_checksum": verifying_receipt.payload["state_checksum"],
                    "writer_epoch": 1,
                }
            )
            kernel = self._exact_event_receipt(
                store=store,
                receipt_digest=receipt_chain["kernel"],
                command_id="BOOT_READY:1",
                event_id="event:BOOT_READY:1",
                kind="BOOT_READY",
            )
            if kernel["payload"] != {
                "attestation_digest": expected_attestation
            }:
                raise Protocol2Unavailable("kernel readiness binding diverged")

            network_operation = f"network:{run_id}"
            expected_network_policy = {
                "allowlist": [destination],
                "mode": "allowlist",
            }
            network_policy_digest = canonical_digest(expected_network_policy)
            network = self._exact_event_receipt(
                store=store,
                receipt_digest=receipt_chain["network_policy"],
                command_id=f"network-policy:{network_operation}",
                event_id=f"event:network-policy:{network_operation}",
                kind="NETWORK_POLICY_ENFORCED",
            )
            if network["payload"] != {
                "operation_id": network_operation,
                "policy_digest": network_policy_digest,
                "readback_digest": network_policy_digest,
            }:
                raise Protocol2Unavailable("network policy binding diverged")

            identity = TrialIdentity(
                f"live-canary:{run_id}",
                f"trial:{run_id}",
                f"intent:{run_id}",
                EVAL_CONTRACT_VERSION,
            )
            assignment = TrialAssignment(
                identity,
                str(policy.get("challenge_id") or ""),
                "candidate",
                f"pair:{run_id}",
                tuple(sorted((str(key), int(value)) for key, value in limits.items())),
                policy_digest,
            )
            assignment_payload = {
                "assignment": assignment.as_dict(),
                "identity": {
                    "intention_id": identity.intention_id,
                    "protocol_version": identity.protocol_version,
                    "study_id": identity.study_id,
                    "trial_id": identity.trial_id,
                },
                "run_id": run_id,
            }
            eval_assignment = self._exact_event_receipt(
                store=store,
                receipt_digest=receipt_chain["eval_assignment"],
                command_id=f"eval:assignment:{run_id}",
                event_id=f"event:eval:assignment:{run_id}",
                kind="EVAL_ASSIGNMENT_BOUND",
            )
            if eval_assignment["payload"] != assignment_payload:
                raise Protocol2Unavailable("eval assignment binding diverged")

            state = store.state()
            scope = ExecutionScope(
                run_id, state.run_fence_epoch, state.execution_generation
            )

            def canonical_admission(receipt_digest: str) -> tuple[dict, LeaseIdentity]:
                try:
                    receipt = store.resolve_receipt(receipt_digest)
                    rows = [
                        row
                        for row in store.event_rows(kind="ATTEMPT_ADMITTED")
                        if store.receipt_digest_for_event(row["event_digest"])
                        == receipt_digest
                    ]
                except (IntegrityError, KeyError, TypeError, ValueError) as exc:
                    raise Protocol2Unavailable(
                        "canonical attempt admission did not resolve"
                    ) from exc
                if len(rows) != 1:
                    raise Protocol2Unavailable(
                        "canonical attempt admission is not unique"
                    )
                payload = rows[0]["payload"]
                try:
                    attempt = AttemptIdentity(
                        scope,
                        payload["branch_id"],
                        payload["attempt_id"],
                        payload["launch_ordinal"],
                    )
                    lease = LeaseIdentity(
                        attempt,
                        payload["lease_id"],
                        payload["lease_epoch"],
                        payload["worker_generation"],
                    )
                except (KeyError, TypeError, ValueError) as exc:
                    raise Protocol2Unavailable(
                        "attempt admission identity is malformed"
                    ) from exc
                if (
                    receipt.command_id != f"attempt:admit:{attempt.attempt_id}"
                    or rows[0]["event_id"]
                    != f"event:attempt:admit:{attempt.attempt_id}"
                    or payload.get("scope_digest") != scope.digest
                    or payload.get("attempt_digest") != attempt.digest
                    or payload.get("lease_digest") != lease.digest
                    or payload.get("policy_digest") != policy_digest
                ):
                    raise Protocol2Unavailable(
                        "attempt admission run/scope/lease binding diverged"
                    )
                return rows[0], lease

            admission, admission_lease = canonical_admission(
                receipt_chain["admission"]
            )
            destination_digest = canonical_digest(destination)
            egress_id = f"provider:{destination_digest}"
            egress = self._exact_event_receipt(
                store=store,
                receipt_digest=receipt_chain["egress"],
                command_id=f"egress:{egress_id}",
                event_id=f"event:egress:{egress_id}",
                kind="EGRESS_RECEIPT",
            )
            if egress["payload"] != {
                "allowed": True,
                "destination": destination,
                "lease_digest": admission_lease.digest,
                "observed": False,
                "observation_digest": "",
                "policy_digest": network_policy_digest,
            }:
                raise Protocol2Unavailable("egress permit binding diverged")

            observation_id = f"provider-observed:{destination_digest}"
            egress_observation = self._exact_event_receipt(
                store=store,
                receipt_digest=receipt_chain["egress_observation"],
                command_id=f"egress:{observation_id}",
                event_id=f"event:egress:{observation_id}",
                kind="EGRESS_RECEIPT",
            )
            observation_payload = egress_observation["payload"]
            observation_digest = observation_payload.get("observation_digest")
            if (
                type(observation_digest) is not str
                or len(observation_digest) != 64
                or any(char not in "0123456789abcdef" for char in observation_digest)
                or observation_payload
                != {
                    "allowed": True,
                    "destination": destination,
                    "lease_digest": observation_payload.get("lease_digest"),
                    "observed": True,
                    "observation_digest": observation_digest,
                    "policy_digest": network_policy_digest,
                }
            ):
                raise Protocol2Unavailable("egress observation binding diverged")
            observation_admissions = [
                row
                for row in store.event_rows(kind="ATTEMPT_ADMITTED")
                if row["payload"].get("lease_digest")
                == observation_payload["lease_digest"]
            ]
            if len(observation_admissions) != 1:
                raise Protocol2Unavailable(
                    "egress observation has no unique canonical attempt"
                )
            observation_receipt = store.receipt_digest_for_event(
                observation_admissions[0]["event_digest"]
            )
            observation_admission, _ = canonical_admission(observation_receipt)
            if observation_admission != observation_admissions[0]:
                raise Protocol2Unavailable(
                    "egress observation attempt identity diverged"
                )
            if not (
                cas_event["seq"]
                < boot_verifying["seq"]
                < kernel["seq"]
                < network["seq"]
                < eval_assignment["seq"]
                < admission["seq"]
                < egress["seq"]
                < egress_observation["seq"]
            ):
                raise Protocol2Unavailable("run evidence order diverged")
        except (
            CASIntegrityError,
            IntegrityError,
            KeyError,
            OSError,
            TypeError,
            ValueError,
        ) as exc:
            raise Protocol2Unavailable(
                "canonical run evidence did not resolve"
            ) from exc
        finally:
            store.close()

        platform_supervisor = self._exact_event_receipt(
            store=catalog_store,
            receipt_digest=receipt_chain["platform_supervisor"],
            command_id=f"{platform_operation}:started",
            event_id=f"event:{platform_operation}:started",
            kind="PLATFORM_OPERATION_OBSERVED",
        )
        supervisor_payload = platform_supervisor["payload"]
        endpoint_digest = supervisor_payload.get("proxy_endpoint_digest")
        if (
            type(endpoint_digest) is not str
            or len(endpoint_digest) != 64
            or any(char not in "0123456789abcdef" for char in endpoint_digest)
            or supervisor_payload
            != {
                "operation_id": platform_operation,
                "owner_epoch": 1,
                "policy_digest": network_policy_digest,
                "proxy_endpoint_digest": endpoint_digest,
                "run_id": run_id,
            }
        ):
            raise Protocol2Unavailable("platform supervisor identity diverged")
        platform_cleanup = self._exact_event_receipt(
            store=catalog_store,
            receipt_digest=receipt_chain["platform_cleanup"],
            command_id=f"{platform_operation}:settled",
            event_id=f"event:{platform_operation}:settled",
            kind="PLATFORM_OPERATION_SETTLED",
        )
        if platform_cleanup["payload"] != {
            "operation_id": platform_operation,
            "owner_epoch": 1,
            "run_id": run_id,
            "terminal": "observed_closed",
        }:
            raise Protocol2Unavailable("platform cleanup identity diverged")
        if not (
            schema["seq"]
            < platform_admission["seq"]
            < platform_supervisor["seq"]
            < platform_cleanup["seq"]
            < canary_seq
        ):
            raise Protocol2Unavailable("catalog evidence order diverged")

    def archive(
        self, *, run_id: str, operation_id: str, occurred_at_ns: int
    ) -> dict[str, Any]:
        if run_id in self._live:
            raise Protocol2Unavailable(
                "Protocol 2 live owner must drain before archive"
            )
        return self.catalog.request_archive(
            operation_id=operation_id,
            run_id=run_id,
            owner_epoch=1,
            occurred_at_ns=occurred_at_ns,
        )

    def begin_purge(
        self,
        *,
        run_id: str,
        operation_id: str,
        items: tuple[Mapping, ...],
        occurred_at_ns: int,
    ) -> dict[str, Any]:
        if run_id in self._live:
            raise Protocol2Unavailable("Protocol 2 live owner must drain before purge")
        return self.catalog.begin_purge(
            operation_id=operation_id,
            run_id=run_id,
            owner_epoch=1,
            items=items,
            occurred_at_ns=occurred_at_ns,
        )

    def purge_item_absent(self, **kwargs: Any) -> dict[str, Any]:
        return self.catalog.record_purge_item_absent(**kwargs)

    def purge_item_unknown(self, **kwargs: Any) -> dict[str, Any]:
        return self.catalog.record_purge_item_unknown(**kwargs)

    def complete_purge(
        self, *, operation_id: str, occurred_at_ns: int
    ) -> dict[str, Any]:
        return self.catalog.complete_purge(
            operation_id=operation_id, occurred_at_ns=occurred_at_ns
        )

    def archive_status(self, operation_id: str) -> dict[str, Any]:
        return self.catalog._store.archive_status(operation_id)

    def purge_status(self, operation_id: str) -> dict[str, Any]:
        return self.catalog._store.purge_status(operation_id)

    @staticmethod
    def _release_receipts() -> dict[str, str]:
        values = {
            "baseline": os.environ.get("MUTEKI_PROTOCOL2_BASELINE_RECEIPT", "").strip(),
            "fault_suite": os.environ.get(
                "MUTEKI_PROTOCOL2_FAULT_SUITE_RECEIPT", ""
            ).strip(),
        }
        return {
            key: value
            for key, value in values.items()
            if len(value) == 64 and all(char in "0123456789abcdef" for char in value)
        }

    @staticmethod
    def _provider_destination(profiles: Sequence[Mapping[str, Any]]) -> str:
        destinations: set[str] = set()
        for profile in profiles:
            base = str(profile.get("base_url") or "").strip()
            if not base:
                raise Protocol2Unavailable(
                    "live-local canary requires an explicit provider endpoint"
                )
            parsed = urlparse(base)
            if parsed.scheme != "https" or not parsed.hostname:
                raise Protocol2Unavailable(
                    "provider endpoint must be an explicit HTTPS URL"
                )
            port = parsed.port or 443
            destinations.add(f"{parsed.hostname}:{port}")
        if len(destinations) != 1:
            raise Protocol2Unavailable(
                "minimal canary permits exactly one provider destination"
            )
        return next(iter(destinations))

    @staticmethod
    def _provider_base_url(profiles: Sequence[Mapping[str, Any]]) -> str:
        values = {
            str(profile.get("base_url") or "").rstrip("/") for profile in profiles
        }
        if len(values) != 1 or not next(iter(values)):
            raise Protocol2Unavailable(
                "minimal canary requires one explicit provider base URL"
            )
        return next(iter(values))

    def prepare_live_session(
        self,
        *,
        run_id: str,
        challenge_id: str,
        attachments: Sequence[str],
        profiles: Sequence[Mapping[str, Any]],
        artifacts: Any,
        max_attempts: int,
        max_barren_attempts: int,
        wall_ms: int,
        token_budget: int,
        cost_micro_usd: int,
        tool_call_budget: int,
        expected_goal_units: int,
        cognitive_feature_gate: CognitiveFeatureGateV1 | None = None,
    ) -> Protocol2RunSession:
        if run_id in self._live:
            raise Protocol2Unavailable("Protocol 2 run is already live")
        release = self._release_receipts()
        if len(release) != 2:
            missing = sorted({"baseline", "fault_suite"} - set(release))
            raise Protocol2Unavailable(
                "Protocol 2 live canary is release-gated; missing receipt(s): "
                + ", ".join(missing)
            )
        limits = {
            "attempts": int(max_attempts),
            "cost_micro_usd": int(cost_micro_usd),
            "tokens": int(token_budget),
            "tool_calls": int(tool_call_budget),
            "wall_ms": int(wall_ms),
            "worker_ms": int(wall_ms),
        }
        if any(value <= 0 for value in limits.values()):
            raise Protocol2Unavailable(
                "Protocol 2 live canary requires finite positive budgets"
            )
        if len(profiles) != 1:
            raise Protocol2Unavailable(
                "minimal live canary requires exactly one worker profile"
            )
        if type(expected_goal_units) is not int or expected_goal_units != 1:
            raise Protocol2Unavailable(
                "S4-E v1 live canary supports exactly one expected goal unit"
            )
        if cognitive_feature_gate is not None and type(
            cognitive_feature_gate
        ) is not CognitiveFeatureGateV1:
            raise TypeError(
                "cognitive_feature_gate must be CognitiveFeatureGateV1 or None"
            )
        provider_destination = self._provider_destination(profiles)
        provider_base_url = self._provider_base_url(profiles)
        policy = {
            "budget": limits,
            "challenge_id": challenge_id,
            "offline": True,
            "profile_ids": tuple(str(p.get("id") or "") for p in profiles),
            "protocol": 2,
            "run_id": run_id,
        }
        if cognitive_feature_gate is not None:
            # A C6 shape is never selected by an HTTP/UI string.  The immutable
            # policy carries its exact canonical body before provision, while the
            # default web canary remains on the trusted S4 baseline.
            policy["cognitive_feature_gate"] = cognitive_feature_gate.canonical_body()
        policy_digest = canonical_digest(policy)
        draft_id = f"draft:{run_id}"
        operation_id = f"provision:{run_id}"
        now = time.time_ns()
        self.catalog.create_draft(draft_id=draft_id, policy=policy, occurred_at_ns=now)
        for ordinal, raw_path in enumerate(attachments, start=1):
            path = Path(raw_path)
            self.catalog.add_attachment(
                draft_id=draft_id,
                attachment_id=f"attachment:{run_id}:{ordinal}:{path.name}",
                data=path.read_bytes(),
                occurred_at_ns=now + ordinal,
            )
        manifest_digest = canonical_digest(
            {
                "attachment_count": len(attachments),
                "policy_digest": policy_digest,
            }
        )
        target_root = self.root / "runs" / run_id
        self.catalog.begin_provision(
            operation_id=operation_id,
            draft_id=draft_id,
            run_id=run_id,
            target_root=target_root,
            manifest_digest=manifest_digest,
            owner_epoch=1,
            occurred_at_ns=now + len(attachments) + 1,
        )
        self.catalog.materialize(
            operation_id=operation_id, occurred_at_ns=now + len(attachments) + 2
        )
        factory = HostRunFactory(catalog=self.catalog, artifacts=artifacts)
        context, ports = factory.open(
            run_id=run_id,
            boot_capability=BootRecoveryCapability(
                1, 1, canonical_digest({"run_id": run_id, "owner": "web"})
            ),
            occurred_at_ns=now + len(attachments) + 3,
        )
        scope, supervisor = factory.start_execution(
            ports=ports,
            idempotency_key=f"start:{run_id}",
            occurred_at_ns=now + len(attachments) + 4,
        )
        ports.admission.create_branch(
            branch_id="root",
            max_attempts=max_attempts,
            occurred_at_ns=now + len(attachments) + 5,
        )
        ports.admission.create_budget_account(
            account_id="run", limits=limits, occurred_at_ns=now + len(attachments) + 6
        )

        network = NetworkPolicyAuthority(
            store=ports.store, adapter=_CliToolPolicyAdapter(profiles)
        )
        enforced = network.apply_and_readback(
            operation_id=f"network:{run_id}",
            mode="allowlist",
            allowlist=[provider_destination],
            occurred_at_ns=now + len(attachments) + 7,
        )
        assignment = TrialAssignment(
            TrialIdentity(
                f"live-canary:{run_id}",
                f"trial:{run_id}",
                f"intent:{run_id}",
                EVAL_CONTRACT_VERSION,
            ),
            challenge_id,
            "candidate",
            f"pair:{run_id}",
            tuple(sorted(limits.items())),
            policy_digest,
        )
        assignment_payload = {
            "assignment": assignment.as_dict(),
            "identity": {
                "intention_id": assignment.identity.intention_id,
                "protocol_version": assignment.identity.protocol_version,
                "study_id": assignment.identity.study_id,
                "trial_id": assignment.identity.trial_id,
            },
            "run_id": run_id,
        }
        assignment_result = ports.store.commit_command(
            command_id=f"eval:assignment:{run_id}",
            idempotency_key=f"eval:assignment:{run_id}",
            command_payload=assignment_payload,
            events=[
                CommandEvent(
                    f"event:eval:assignment:{run_id}",
                    "EVAL_ASSIGNMENT_BOUND",
                    "protocol2-web-adapter",
                    now + len(attachments) + 8,
                    assignment_payload,
                )
            ],
            committed_at_ns=now + len(attachments) + 8,
        )
        external = {
            "cas": self._single_event_receipt(
                store=ports.store,
                command_id="CREATE_RUN",
                event_id="event:CREATE_RUN",
                kind="RUN_CREATED",
            ),
            "eval_assignment": assignment_result.receipt_digest,
            "kernel": self._single_event_receipt(
                store=ports.store,
                command_id="BOOT_READY:1",
                event_id="event:BOOT_READY:1",
                kind="BOOT_READY",
            ),
            "network_policy": enforced.enforcement_receipt_digest,
            "schema": self._single_event_receipt(
                store=self.catalog._store,
                command_id=f"provision:sealed:{operation_id}",
                event_id=f"event:provision:sealed:{operation_id}",
                kind="RUN_SEALED",
            ),
            **release,
        }
        proxy_operation = f"platform:egress-proxy:{run_id}"
        proxy_admission_payload = {
            "conflict_key": f"egress-proxy:{run_id}",
            "destination": provider_destination,
            "operation_id": proxy_operation,
            "owner_epoch": 1,
            "run_id": run_id,
            "wall_ms": limits["wall_ms"],
        }
        proxy_admission = self.catalog._store.commit_command(
            command_id=f"{proxy_operation}:admitted",
            idempotency_key=f"{proxy_operation}:admitted",
            command_payload=proxy_admission_payload,
            events=[
                CommandEvent(
                    f"event:{proxy_operation}:admitted",
                    "PLATFORM_OPERATION_ADMITTED",
                    "protocol2-web-adapter",
                    time.time_ns(),
                    proxy_admission_payload,
                )
            ],
            committed_at_ns=time.time_ns(),
        )
        proxy = LoopbackAllowlistProxy(provider_destination)
        try:
            proxy.start()
        except Exception:
            proxy.close()
            raise
        proxy_started_payload = {
            "operation_id": proxy_operation,
            "owner_epoch": 1,
            "policy_digest": enforced.policy_digest,
            "proxy_endpoint_digest": canonical_digest(
                {"host": "localhost", "port": proxy.port}
            ),
            "run_id": run_id,
        }
        try:
            proxy_started = self.catalog._store.commit_command(
                command_id=f"{proxy_operation}:started",
                idempotency_key=f"{proxy_operation}:started",
                command_payload=proxy_started_payload,
                events=[
                    CommandEvent(
                        f"event:{proxy_operation}:started",
                        "PLATFORM_OPERATION_OBSERVED",
                        "protocol2-web-adapter",
                        time.time_ns(),
                        proxy_started_payload,
                    )
                ],
                committed_at_ns=time.time_ns(),
            )
        except Exception:
            proxy.close()
            raise
        external["platform_admission"] = proxy_admission.receipt_digest
        external["platform_supervisor"] = proxy_started.receipt_digest
        per_attempt = {
            key: max(1, value // max_attempts) for key, value in limits.items()
        }
        per_attempt["attempts"] = 1

        def admit_completion(
            live_session: Protocol2RunSession,
            receipts: Mapping[str, str],
            solved: bool,
        ) -> Mapping[str, str]:
            settled_payload = {
                "operation_id": proxy_operation,
                "owner_epoch": 1,
                "run_id": run_id,
                "terminal": "observed_closed",
            }
            settled = self.catalog._store.commit_command(
                command_id=f"{proxy_operation}:settled",
                idempotency_key=f"{proxy_operation}:settled",
                command_payload=settled_payload,
                events=[
                    CommandEvent(
                        f"event:{proxy_operation}:settled",
                        "PLATFORM_OPERATION_SETTLED",
                        "protocol2-web-adapter",
                        time.time_ns(),
                        settled_payload,
                    )
                ],
                committed_at_ns=time.time_ns(),
            )
            closure = {**dict(receipts), "platform_cleanup": settled.receipt_digest}
            if not solved:
                return {"platform_cleanup": settled.receipt_digest}
            resolved = resolve_s4e_closure(
                store=live_session.ports.store,
                cas=live_session.ports.cas,
                receipt_chain=closure,
            )
            if (
                resolved.policy_digest != policy_digest
                or resolved.accepted_goal_units != 1
            ):
                raise Protocol2Unavailable(
                    "live canary closure is not bound to its frozen policy"
                )
            canary_digest = admit_canary(
                CanaryEvidence(
                    CanaryLevel.LIVE_LOCAL,
                    closure,
                    fault_suite_green=True,
                    gate_equivalent=live_session.gate_equivalent,
                    projection_rebuild_equivalent=True,
                )
            )
            payload = {
                "canary_digest": canary_digest,
                "level": CanaryLevel.LIVE_LOCAL.value,
                "receipt_chain": {
                    str(key): str(value) for key, value in sorted(closure.items())
                },
                "run_id": run_id,
            }
            self.catalog._store.commit_command(
                command_id=f"canary:{run_id}",
                idempotency_key=f"canary:{run_id}",
                command_payload=payload,
                events=[
                    CommandEvent(
                        f"event:canary:{run_id}",
                        "CANARY_ADMITTED",
                        "protocol2-web-adapter",
                        time.time_ns(),
                        payload,
                    )
                ],
                projection_mutations=[ProjectionMutation(
                    "canary_commit_guard", payload
                )],
                authority_capability=(
                    self.catalog._store._canary_commit_capability
                ),
                committed_at_ns=time.time_ns(),
            )
            return {"canary": canary_digest, "platform_cleanup": settled.receipt_digest}

        try:
            session = Protocol2RunSession(
                ports=ports,
                scope=scope,
                supervisor=supervisor,
                policy_digest=policy_digest,
                budget_account_id="run",
                per_attempt_budget=per_attempt,
                max_barren_attempts=max_barren_attempts,
                expected_goal_units=expected_goal_units,
                external_receipts=external,
                network_authority=network,
                network_policy=enforced,
                provider_destination=provider_destination,
                provider_base_url=provider_base_url,
                egress_proxy=proxy,
                completion_callback=admit_completion,
                cognitive_feature_gate=cognitive_feature_gate,
            )
        except Exception:
            proxy.close()
            ports.store.close()
            raise
        self._live[run_id] = session
        return session

    async def _finalize_live_session(
        self,
        *,
        run_id: str,
        session: Protocol2RunSession,
        solved: bool,
    ) -> dict[str, Any]:
        finalize = asyncio.create_task(session.finalize(solved=solved))
        caller_cancelled: asyncio.CancelledError | None = None
        try:
            receipts = dict(await asyncio.shield(finalize))
        except asyncio.CancelledError as exc:
            caller_cancelled = exc
            while not finalize.done():
                try:
                    await asyncio.shield(finalize)
                except asyncio.CancelledError:
                    continue
                except BaseException:
                    # Inspect the completed task below. Calling exception() there
                    # consumes the failure and prevents an unhandled-task warning.
                    break
            if finalize.cancelled():
                finalize_failure: BaseException | None = asyncio.CancelledError()
            else:
                finalize_failure = finalize.exception()
            if finalize_failure is not None:
                error_class = type(finalize_failure).__name__
                diagnostic = Protocol2Unavailable(
                    "Protocol 2 finalize failed after caller cancellation "
                    f"({error_class}); live owner retained"
                )
                caller_cancelled.add_note(
                    "Protocol 2 finalize failed after caller cancellation; "
                    f"finalizer_error_class={error_class}; live owner retained"
                )
                # The raw finalizer exception may contain provider output or other
                # secrets. Preserve only its class in a local diagnostic while the
                # original caller cancellation remains authoritative.
                raise caller_cancelled from diagnostic
            receipts = dict(finalize.result())
        # Relinquish the only in-process owner and close its canonical store only
        # after successful local finalization. A failed finalization keeps both
        # retained so drain/recovery can be retried instead of presenting an
        # ownerless, partially finalized run.
        self._live.pop(run_id, None)
        session.ports.store.close()
        if caller_cancelled is not None:
            raise caller_cancelled
        return {"canary_digest": receipts.get("canary", ""), "receipts": receipts}

    async def complete_live_session(
        self,
        *,
        run_id: str,
        session: Protocol2RunSession,
        solved: bool,
    ) -> dict[str, Any]:
        if self._live.get(run_id) is not session:
            raise Protocol2Unavailable("Protocol 2 live owner mismatch")
        return await self._finalize_live_session(
            run_id=run_id, session=session, solved=solved
        )

    async def abort_live_session(
        self,
        *,
        run_id: str,
        session: Protocol2RunSession,
    ) -> None:
        """Best-effort canonical pause/close for a failed pre-terminal canary."""
        if self._live.get(run_id) is not session:
            raise Protocol2Unavailable("Protocol 2 live owner mismatch")
        await self._finalize_live_session(
            run_id=run_id, session=session, solved=False
        )

    def status(self) -> dict:
        state = self.catalog._store.verify()
        admitted = self.catalog._store.event_rows(kind="CANARY_ADMITTED")
        latest = admitted[-1]["payload"] if admitted else {}
        release = self._release_receipts()
        chain = (
            latest.get("receipt_chain")
            if isinstance(latest.get("receipt_chain"), dict)
            else {}
        )
        valid_canary = False
        semantic_closure = False
        semantic_status_evidence = False
        if admitted:
            try:
                recomputed = admit_canary(
                    CanaryEvidence(
                        CanaryLevel.LIVE_LOCAL,
                        chain,
                        fault_suite_green=True,
                        gate_equivalent=True,
                        projection_rebuild_equivalent=True,
                    )
                )
                valid_canary = recomputed == latest.get("canary_digest")
            except Exception:
                valid_canary = False
            if valid_canary and type(latest.get("run_id")) is str:
                try:
                    self._resolve_live_closure(
                        run_id=latest["run_id"], receipt_chain=chain
                    )
                    semantic_closure = True
                except (Protocol2Unavailable, KeyError, OSError):
                    semantic_closure = False
                if semantic_closure:
                    try:
                        self._resolve_live_status_evidence(
                            run_id=latest["run_id"],
                            receipt_chain=chain,
                            canary_seq=admitted[-1]["seq"],
                        )
                        semantic_status_evidence = True
                    except (Protocol2Unavailable, KeyError, OSError):
                        semantic_status_evidence = False
        release_matches = bool(
            len(release) == 2
            and all(chain.get(key) == value for key, value in release.items())
        )
        s4e_missing = missing_s4e_receipts(chain)
        production_enabled = bool(
            admitted
            and valid_canary
            and semantic_closure
            and semantic_status_evidence
            and release_matches
            and not s4e_missing
        )
        if production_enabled:
            reason = (
                "live-local semantic canary and operator-attested release policy passed"
            )
        else:
            missing = sorted({"baseline", "fault_suite"} - set(release))
            if missing:
                detail = f"missing release receipt(s): {', '.join(missing)}"
            elif not admitted or not valid_canary:
                detail = "no valid admitted live-local canary"
            elif s4e_missing:
                detail = "missing S4-E receipt(s): " + ", ".join(s4e_missing)
            elif not semantic_closure:
                detail = "no semantically resolved S4-E closure"
            elif not semantic_status_evidence:
                detail = "live-local canary evidence is not canonically bound"
            else:
                detail = "live-local canary is not release-bound"
            reason = (
                "Protocol 2 kernel is healthy; production remains fail-closed; "
                + detail
            )
        return {
            "protocol_version": 2,
            "available": True,
            "production_enabled": production_enabled,
            "reason": reason,
            "catalog_head": state.head_seq,
            "catalog_checksum": state.checksum,
            "latest_canary": latest,
            "latest_receipt_chain": chain,
            "live_run_count": len(self._live),
        }

    def canonical_run_status(self, run_id: str) -> dict[str, Any]:
        """Read lifecycle and canary closure only from canonical V2 stores."""
        view = self.catalog.run_view(run_id)
        canaries = [
            row
            for row in self.catalog._store.event_rows(kind="CANARY_ADMITTED")
            if row["payload"].get("run_id") == run_id
        ]
        latest = canaries[-1] if canaries else None
        result: dict[str, Any] = {
            "run": {key: value for key, value in view.items() if key != "target_root"},
            "catalog_head": self.catalog._store.state().head_seq,
            "catalog_checksum": self.catalog._store.verify().checksum,
            "canary": latest["payload"] if latest else {},
            "receipt_chain": (
                dict(latest["payload"].get("receipt_chain") or {}) if latest else {}
            ),
        }
        target = Path(view["target_root"]) / "epistemic-v2.db"
        if target.is_file():
            from muteki.epistemic.sqlite_store import EpistemicSQLiteStore

            store = EpistemicSQLiteStore.open(target)
            try:
                verified = store.verify()
                result["run_store"] = {
                    "head": verified.head_seq,
                    "checksum": verified.checksum,
                    "execution": verified.run_execution.value,
                    "search_mode": verified.search_mode.value,
                    "runtime_projection_digest": store.runtime_projection_digest(),
                }
            finally:
                store.close()
        else:
            result["run_store"] = {"available": False}
        return result
