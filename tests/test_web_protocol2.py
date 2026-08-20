from __future__ import annotations

import asyncio
from contextlib import contextmanager
from pathlib import Path
import socket
import socketserver
import threading
from urllib.parse import urlparse

import pytest

from fastapi.testclient import TestClient

from apps.web.run_manager import RunManager
from apps.web.server import create_app


def _provision_v2(manager: RunManager, run_id: str) -> None:
    catalog = manager.protocol2.catalog
    now = 1
    catalog.create_draft(
        draft_id=f"draft:{run_id}", policy={"protocol": 2}, occurred_at_ns=now)
    catalog.begin_provision(
        operation_id=f"provision:{run_id}", draft_id=f"draft:{run_id}",
        run_id=run_id, target_root=manager.protocol2.root / "runs" / run_id,
        manifest_digest="a" * 64, owner_epoch=1, occurred_at_ns=now + 1)
    catalog.materialize(
        operation_id=f"provision:{run_id}", occurred_at_ns=now + 2)


class _ProviderHandler(socketserver.BaseRequestHandler):
    def handle(self):
        self.request.settimeout(2.0)
        try:
            while self.request.recv(4096):
                pass
        except (OSError, TimeoutError):
            pass


@contextmanager
def _local_provider():
    server = socketserver.ThreadingTCPServer(
        ("127.0.0.1", 0), _ProviderHandler)
    server.daemon_threads = True
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        port = int(server.server_address[1])
        yield f"https://localhost:{port}/anthropic", f"localhost:{port}"
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=3)


def _observe_provider_egress(worker, destination: str) -> None:
    proxy = urlparse(worker._extra_worker_env["HTTPS_PROXY"])
    with socket.create_connection((proxy.hostname, proxy.port), timeout=3) as conn:
        conn.sendall(
            f"CONNECT {destination} HTTP/1.1\r\nHost: {destination}\r\n\r\n".encode())
        response = conn.recv(4096)
        assert response.startswith(b"HTTP/1.1 200")


def _live_driver_body(base_url: str = "https://api.deepseek.example/anthropic") -> dict:
    profile = {
        "id": "deepseek-claude", "name": "deepseek-claude",
        "engine": "claude", "runtime": "local", "roles": ["bootstrap"],
        "base_url": base_url,
        "enabled": True, "max_running": 1,
    }
    return {
        "protocol": 2,
        "challenge": {
            "name": "neutral synthetic fixture", "category": "misc",
            "description": "Return the deterministic fixture token.",
            "flag_format": r"flag\{[^}]+\}",
        },
        "offline": True, "kb": False, "coordinator": False,
        "race_scout": False, "cli_race": False,
        "cli_engine": "deepseek-claude", "engines": ["deepseek-claude"],
        "worker_profiles": [profile], "start_workers": 1, "max_workers": 1,
        "worker_backend": "local",
        "max_total_workers": 1, "wall_clock_budget": 10,
        "cost_budget_usd": 1.0, "token_budget": 1000,
        "tool_call_budget": 10, "max_barren_attempts": 1,
    }


def _enable_release(monkeypatch) -> None:
    monkeypatch.setenv("MUTEKI_CLAUDE_BIN", "/usr/bin/claude")
    monkeypatch.setenv("MUTEKI_PROTOCOL2_BASELINE_RECEIPT", "a" * 64)
    monkeypatch.setenv("MUTEKI_PROTOCOL2_FAULT_SUITE_RECEIPT", "b" * 64)


def _allow_synthetic_host_egress(monkeypatch) -> None:
    """Remove the Darwin host dependency from non-egress Protocol 2 tests."""
    from apps.web.protocol2_adapter import _CliToolPolicyAdapter

    monkeypatch.setattr(_CliToolPolicyAdapter, "_verify", lambda _self: None)


@pytest.fixture
def local_provider():
    with _local_provider() as provider:
        yield provider


def test_protocol2_cli_policy_fails_closed_without_darwin_seatbelt(monkeypatch):
    from apps.web import protocol2_adapter

    monkeypatch.setattr(protocol2_adapter.sys, "platform", "linux")
    policy = protocol2_adapter._CliToolPolicyAdapter([
        {"id": "claude", "engine": "claude"},
    ])
    with pytest.raises(
        protocol2_adapter.Protocol2Unavailable,
        match="host egress enforcement is unavailable",
    ):
        policy.apply({"mode": "allowlist", "allowlist": ["localhost:443"]})


def test_protocol2_cli_policy_verifies_offline_argv_on_supported_host(
    monkeypatch,
):
    from apps.web import protocol2_adapter

    monkeypatch.setattr(protocol2_adapter.sys, "platform", "darwin")
    monkeypatch.setattr(
        protocol2_adapter.Path,
        "is_file",
        lambda path: str(path) == "/usr/bin/sandbox-exec",
    )
    monkeypatch.setenv("MUTEKI_CLAUDE_BIN", "/usr/bin/claude")
    policy = protocol2_adapter._CliToolPolicyAdapter([
        {"id": "claude", "engine": "claude"},
    ])
    expected = {"mode": "allowlist", "allowlist": ("localhost:443",)}
    assert policy.apply(expected) == expected
    assert policy.readback() == expected


def test_web_initializes_host_only_protocol2_catalog(tmp_path):
    manager = RunManager(
        sessions_root=tmp_path / "sessions",
        control_root=tmp_path / "control")
    assert manager.protocol2 is not None
    assert (tmp_path / "control" / "protocol2" / "catalog-v2.db").is_file()


def test_restart_recovers_catalog_only_accepted_flag_without_rail_promotion(
    tmp_path, monkeypatch,
):
    from muteki.epistemic.authority import GateAuthority
    from muteki.epistemic.broker import CaptureSession
    from muteki.epistemic.cas import ReceiptCAS
    from muteki.core.session_store import SessionStore
    from muteki.epistemic.contracts import canonical_digest
    from muteki.epistemic.sqlite_store import (
        CommandEvent, EpistemicSQLiteStore, ProjectionMutation,
    )
    from muteki.runtime.admission import AdmissionRequest, SearchAdmission
    from muteki.runtime.contracts import (
        AttemptIdentity, EffectClass, ExecutionScope, LeaseIdentity,
    )
    from muteki.runtime.controller import BootRecoveryCapability, LiveHealthGuard
    from muteki.runtime.permit_resolver import CanonicalPermitResolver

    sessions = tmp_path / "sessions"
    control = tmp_path / "control"
    manager = RunManager(sessions_root=sessions, control_root=control)
    run_id = "run-catalog-accepted-only"
    _provision_v2(manager, run_id)
    target = Path(manager.protocol2.run_view(run_id)["target_root"])
    store = EpistemicSQLiteStore.open(target / "epistemic-v2.db")
    try:
        store.commit_command(
            command_id="ready", idempotency_key="ready", command_payload={},
            events=[CommandEvent("event:ready", "BOOT_READY", "host", 4)],
            committed_at_ns=4,
        )
        store.commit_command(
            command_id="start", idempotency_key="start", command_payload={},
            events=[CommandEvent(
                "event:start", "START_EXECUTION", "host", 5,
                {"execution_generation": 1, "run_fence_epoch": 1},
            )],
            projection_mutations=[ProjectionMutation(
                "execution_start_guard",
                {"execution_generation": 1, "run_fence_epoch": 1},
            )],
            authority_capability=store._lifecycle_commit_capability,
            committed_at_ns=5,
        )
        guard = LiveHealthGuard()
        capability = BootRecoveryCapability(1, 1, "owner")
        guard.begin_boot_finalize(capability)
        guard.open_admission(capability=capability, attestation_digest="b" * 64)
        admission = SearchAdmission(store=store, guard=guard)
        admission.create_branch(branch_id="root", max_attempts=1, occurred_at_ns=6)
        admission.create_budget_account(
            account_id="run", limits={"attempts": 1}, occurred_at_ns=7
        )
        scope = ExecutionScope(run_id, 1, 1)
        attempt = AttemptIdentity(scope, "root", "attempt-1", 1)
        lease = LeaseIdentity(attempt, "lease-1", 1, 1)
        policy_digest = canonical_digest({"protocol": 2})
        permit = admission.admit(
            AdmissionRequest(
                attempt=attempt,
                lease=lease,
                permit_id="permit-1",
                account_id="run",
                requested_budget={"attempts": 1},
                conflict_keys=(),
                effect_class=EffectClass.PURE,
                fingerprint="fixture",
                policy_digest=policy_digest,
                expires_at_ns=100,
            ),
            occurred_at_ns=8,
        )
        CanonicalPermitResolver(store=store, scope=scope).claim_launch(
            permit, now_ns=9
        )
        cas = ReceiptCAS(target / "receipt-cas")
        gate = GateAuthority(store=store, cas=cas, artifacts=object())
        flag = "flag{catalog_only}"
        gate_input = CaptureSession(store, cas, permit).seal_gate_input(
            capture_id="catalog-only-gate",
            candidate_id="candidate-a",
            flag=flag,
            flag_format=r"flag\{[^}]+\}",
            policy_digest=policy_digest,
            data=b"observed flag{catalog_only}",
            occurred_at_ns=10,
        )
        gate.evaluate(
            evaluation_id=gate.evaluation_id_for(gate_input),
            candidate_id="candidate-a",
            flag=flag,
            gate_input=gate_input,
            permit=permit,
            flag_format=r"flag\{[^}]+\}",
            policy_digest=policy_digest,
            occurred_at_ns=11,
        )
    finally:
        store.close()

    jsonl = sessions / f"{run_id}.jsonl"
    assert not jsonl.exists()
    restarted = RunManager(sessions_root=sessions, control_root=control)
    rows = restarted.runs.get(run_id)
    assert rows is None
    assert restarted.list_runs() == []
    assert not restarted.meta.contains(run_id)
    projected = restarted.protocol2.recover_flag_publications(run_id)
    assert len(projected) == 1
    persisted = SessionStore(root=sessions).load_all(run_id)
    assert [row["event_type"] for row in persisted] == ["flag.accepted"]
    assert persisted[0]["solver_id"] == "protocol2-authority-projector"
    assert persisted[0]["payload"] == {
        "schema_id": "muteki.flag-accepted-projection.v1",
        "publication_id": projected[0].publication_id,
        "evaluation_id": projected[0].evaluation_id,
        "flag": flag,
        "flag_digest": projected[0].flag_digest,
        "gate_receipt_digest": projected[0].gate_receipt_digest,
    }

    restarted_again = RunManager(sessions_root=sessions, control_root=control)
    persisted_again = SessionStore(root=sessions).load_all(run_id)
    assert restarted_again.get(run_id) is None
    assert len(persisted_again) == 1


def test_restart_started_protocol2_with_accepted_flag_stays_unfinished(
    tmp_path, monkeypatch,
):
    from muteki.core.events import Event, EventType
    from muteki.epistemic.authority import AcceptedFlagPublicationV1

    sessions = tmp_path / "sessions"
    control = tmp_path / "control"
    first = RunManager(sessions_root=sessions, control_root=control)
    run_id = "run-started-accepted"
    _provision_v2(first, run_id)
    run = first.create(run_id)
    asyncio.run(run.store.append(Event(
        event_type=EventType.RUN_STARTED,
        run_id=run_id,
        payload={"challenge": {"name": "Protocol 2 fixture"}},
    )))
    publication = AcceptedFlagPublicationV1(
        publication_id="outbox:flag:" + "1" * 64,
        evaluation_id="2" * 64,
        attempt_digest="3" * 64,
        candidate_id="candidate-1",
        capture_event_digest="4" * 64,
        lease_digest="5" * 64,
        permit_digest="6" * 64,
        policy_digest="7" * 64,
        manifest_digest="8" * 64,
        snapshot_digest="9" * 64,
        flag="flag{accepted_restart}",
        flag_digest="a" * 64,
        flag_object_digest="b" * 64,
        flag_byte_count=22,
        accepted_event_digest="c" * 64,
        gate_receipt_digest="d" * 64,
    )
    monkeypatch.setattr(
        first.protocol2, "recover_flag_publications", lambda rid: (publication,)
    )
    first._reconcile_protocol2_flags()
    monkeypatch.setattr(
        "apps.web.protocol2_adapter.Protocol2WebAdapter.recover_flag_publications",
        lambda self, rid: (publication,),
    )

    restarted = RunManager(sessions_root=sessions, control_root=control)
    recovered = restarted.get(run_id)
    assert recovered is not None
    assert recovered.protocol_version == 2
    assert recovered.started is True
    assert recovered.flag == "flag{accepted_restart}"
    assert recovered.flags == ["flag{accepted_restart}"]
    assert recovered.solved is False
    assert recovered.finished is False


def test_protocol2_projection_failure_is_redacted_and_does_not_stop_other_runs(
    tmp_path, monkeypatch,
):
    from muteki.epistemic.authority import AcceptedFlagPublicationV1
    from muteki.core.session_store import SessionStore

    sessions = tmp_path / "sessions"
    control = tmp_path / "control"
    manager = RunManager(sessions_root=sessions, control_root=control)
    bad_id = "run-bad-projection"
    good_id = "run-good-projection"
    _provision_v2(manager, bad_id)
    _provision_v2(manager, good_id)
    publication = AcceptedFlagPublicationV1(
        publication_id="outbox:flag:" + "e" * 64,
        evaluation_id="f" * 64,
        attempt_digest="1" * 64,
        candidate_id="candidate-secret",
        capture_event_digest="2" * 64,
        lease_digest="3" * 64,
        permit_digest="4" * 64,
        policy_digest="5" * 64,
        manifest_digest="6" * 64,
        snapshot_digest="7" * 64,
        flag="flag{good}",
        flag_digest="8" * 64,
        flag_object_digest="9" * 64,
        flag_byte_count=10,
        accepted_event_digest="a" * 64,
        gate_receipt_digest="b" * 64,
    )

    def recover(run_id):
        if run_id == bad_id:
            raise RuntimeError(
                "flag{secret} cas-bytes candidate-secret password=hunter2"
            )
        return (publication,)

    monkeypatch.setattr(manager.protocol2, "recover_flag_publications", recover)
    manager._reconcile_protocol2_flags()
    store = SessionStore(root=sessions)
    bad_rows = store.load_all(bad_id)
    good_rows = store.load_all(good_id)
    assert [row["event_type"] for row in bad_rows] == ["projection.incomplete"]
    assert bad_rows[0]["payload"] == {
        "schema_id": "muteki.projection-incomplete.v1",
        "diagnostic_id": "protocol2-projection-incomplete:" + bad_id,
        "projection": "flag.accepted",
        "error_class": "RuntimeError",
    }
    assert [row["event_type"] for row in good_rows] == ["flag.accepted"]
    durable = "\n".join(path.read_text() for path in sessions.glob("*.jsonl"))
    assert "flag{secret}" not in durable
    assert "cas-bytes" not in durable
    assert "candidate-secret" not in durable
    assert "hunter2" not in durable


def test_protocol2_status_is_real_but_production_fail_closed(tmp_path, monkeypatch):
    monkeypatch.setenv("MUTEKI_WEB_PASSWORD", "")
    monkeypatch.setenv("MUTEKI_WEB_BIND", "127.0.0.1")
    manager = RunManager(
        sessions_root=tmp_path / "sessions",
        control_root=tmp_path / "control")
    client = TestClient(create_app(manager=manager))
    response = client.get("/api/protocol2/status")
    assert response.status_code == 200
    body = response.json()
    assert body["available"] is True
    assert body["production_enabled"] is False
    assert len(body["catalog_checksum"]) == 64


def test_protocol2_status_redacts_adapter_constructor_failure(
    tmp_path, monkeypatch,
):
    from apps.web.protocol2_adapter import Protocol2WebAdapter

    sentinel = "constructor-secret-must-not-escape"

    def fail_constructor(self, *, control_root):
        raise RuntimeError(sentinel)

    monkeypatch.setattr(Protocol2WebAdapter, "__init__", fail_constructor)
    monkeypatch.setenv("MUTEKI_WEB_PASSWORD", "")
    monkeypatch.setenv("MUTEKI_WEB_BIND", "127.0.0.1")
    manager = RunManager(
        sessions_root=tmp_path / "sessions",
        control_root=tmp_path / "control",
    )
    response = TestClient(create_app(manager=manager)).get(
        "/api/protocol2/status"
    )

    assert response.status_code == 200
    assert response.json() == {
        "protocol_version": 2,
        "available": False,
        "production_enabled": False,
        "reason": "RuntimeError",
    }
    assert sentinel not in response.text


def test_legacy_weaker_canary_chain_cannot_enable_current_release(
    tmp_path, monkeypatch,
):
    from muteki.epistemic.sqlite_store import CommandEvent, IntegrityError

    _enable_release(monkeypatch)
    manager = RunManager(
        sessions_root=tmp_path / "sessions",
        control_root=tmp_path / "control")
    payload = {
        "canary_digest": "c" * 64, "level": "live_local",
        "receipt_chain": {"baseline": "a" * 64, "fault_suite": "b" * 64},
        "run_id": "old-run",
    }
    with pytest.raises(IntegrityError, match="catalog-only canary capability"):
        manager.protocol2.catalog._store.commit_command(
            command_id="old-canary", idempotency_key="old-canary",
            command_payload=payload,
            events=[CommandEvent(
                "event:old-canary", "CANARY_ADMITTED", "old-runtime", 1,
                payload,
            )],
            committed_at_ns=1,
        )
    status = manager.protocol2.status()
    assert status["production_enabled"] is False
    assert "no valid admitted" in status["reason"]


def test_shape_valid_canary_without_semantic_run_closure_cannot_enable(
    tmp_path, monkeypatch,
):
    from muteki.epistemic.sqlite_store import CommandEvent, ProjectionMutation
    from muteki.runtime.canary import (
        CanaryEvidence, CanaryLevel, S4E_LIVE_REQUIRED, admit_canary,
    )

    _enable_release(monkeypatch)
    manager = RunManager(
        sessions_root=tmp_path / "sessions",
        control_root=tmp_path / "control",
    )
    _provision_v2(manager, "run-shaped-only")
    names = {
        "admission", "baseline", "cas", "egress", "egress_observation",
        "eval_assignment", "fault_suite", "kernel", "network_policy",
        "platform_admission", "platform_cleanup", "platform_supervisor",
        "schema",
    } | set(S4E_LIVE_REQUIRED)
    chain = {name: "d" * 64 for name in names}
    chain["baseline"] = "a" * 64
    chain["fault_suite"] = "b" * 64
    canary_digest = admit_canary(CanaryEvidence(
        CanaryLevel.LIVE_LOCAL,
        chain,
        fault_suite_green=True,
        gate_equivalent=True,
        projection_rebuild_equivalent=True,
    ))
    payload = {
        "canary_digest": canary_digest,
        "level": "live_local",
        "receipt_chain": chain,
        "run_id": "run-shaped-only",
    }
    store = manager.protocol2.catalog._store
    store.commit_command(
        command_id="canary:run-shaped-only",
        idempotency_key="canary:run-shaped-only",
        command_payload=payload,
        events=[CommandEvent(
            "event:canary:run-shaped-only", "CANARY_ADMITTED",
            "protocol2-web-adapter", 10, payload,
        )],
        projection_mutations=[ProjectionMutation("canary_commit_guard", payload)],
        authority_capability=store._canary_commit_capability,
        committed_at_ns=10,
    )

    status = manager.protocol2.status()
    assert status["production_enabled"] is False
    assert "no semantically resolved S4-E closure" in status["reason"]


def test_protocol2_legacy_control_resolve_delete_and_reveal_fail_closed(
    tmp_path, monkeypatch,
):
    monkeypatch.setenv("MUTEKI_WEB_PASSWORD", "")
    monkeypatch.setenv("MUTEKI_WEB_BIND", "127.0.0.1")
    manager = RunManager(
        sessions_root=tmp_path / "sessions",
        control_root=tmp_path / "control")
    run = manager.create("run-v2")
    run.protocol_version = 2
    run.started = True
    run.finished = True
    client = TestClient(create_app(manager=manager))

    control = client.post("/api/runs/run-v2/control", json={
        "action": "hint", "target": "global", "payload": {"text": "x"}})
    assert control.status_code == 200
    assert control.json()["code"] == "PROTOCOL2_CONTROL_UNAVAILABLE"
    assert client.post("/api/runs/run-v2/resolve", json={}).json() == {"ok": False}
    assert client.delete("/api/runs/run-v2").status_code == 409
    assert client.post("/api/runs/run-v2/open").status_code == 409
    assert client.get("/api/runs/run-v2/credentials").status_code == 409
    assert client.post("/api/runs/run-v2/btw", json={"question": "status"}).status_code == 409
    assert client.post("/api/runs/run-v2/hitl", json={
        "action": "hint", "target": "global", "text": "x"}).json() == {"ok": False}
    assert client.post("/api/runs/run-v2/workers", json={
        "engine": "claude"}).json() == {"ok": False}
    assert client.get("/api/runs/run-v2/control/command-1").status_code == 409
    assert manager.get("run-v2") is run
    assert not (tmp_path / "sessions" / "run-v2" / "workspace").exists()


async def test_protocol2_archive_purge_manager_saga_and_tombstone(tmp_path):
    manager = RunManager(
        sessions_root=tmp_path / "sessions",
        control_root=tmp_path / "control")
    _provision_v2(manager, "run-life")
    run = manager.create("run-life")
    run.protocol_version = 2
    run.started = True
    run.finished = True
    session_jsonl = manager.sessions_root / "run-life.jsonl"
    session_jsonl.write_text("display projection\n")
    session_tree = manager.sessions_root / "run-life"
    session_tree.mkdir()
    (session_tree / "display.txt").write_text("display")
    legacy_control = manager.control_root / "run-life"
    legacy_control.mkdir()
    (legacy_control / "control.db").write_text("legacy")
    manager.meta.set_name("run-life", "display name")

    archived = await manager.archive_protocol2("run-life")
    assert archived["state"] == "archived"
    assert manager.protocol2.run_view("run-life")["state"] == "archived"
    assert run.archived is True

    purged = await manager.purge_protocol2("run-life")
    assert purged["state"] == "purged"
    assert len(purged["plan_receipt_digest"]) == 64
    assert len(purged["absence_receipt_digest"]) == 64
    assert manager.get("run-life") is None
    assert not session_jsonl.exists()
    assert not session_tree.exists()
    assert not legacy_control.exists()
    assert not (manager.protocol2.root / "runs" / "run-life").exists()
    assert (manager.protocol2.root / "catalog-v2.db").is_file()
    assert manager.protocol2.run_view("run-life")["state"] == "purged"

    # Catalog tombstone and per-item receipts make a retry a no-op success even
    # after the target DB/CAS tree is gone.
    head = manager.protocol2.catalog._store.state().head_seq
    assert (await manager.purge_protocol2("run-life"))["state"] == "purged"
    assert manager.protocol2.catalog._store.state().head_seq == head


def test_protocol2_archive_and_purge_api_are_async_operation_views(
    tmp_path, monkeypatch,
):
    monkeypatch.setenv("MUTEKI_WEB_PASSWORD", "")
    monkeypatch.setenv("MUTEKI_WEB_BIND", "127.0.0.1")
    manager = RunManager(
        sessions_root=tmp_path / "sessions",
        control_root=tmp_path / "control")
    _provision_v2(manager, "run-api-life")
    run = manager.create("run-api-life")
    run.protocol_version = 2
    run.started = True
    run.finished = True
    client = TestClient(create_app(manager=manager))

    archive = client.post("/api/runs/run-api-life/archive")
    assert archive.status_code == 200
    assert archive.json()["state"] == "ARCHIVED"
    archive_status = client.get(
        "/api/archive-operations/archive:run-api-life")
    assert archive_status.status_code == 200
    assert archive_status.json()["state"] == "ARCHIVED"

    purge = client.post("/api/runs/run-api-life/purge")
    assert purge.status_code == 200
    assert purge.json()["state"] == "PURGED"
    purge_status = client.get("/api/purge-operations/purge:run-api-life")
    assert purge_status.status_code == 200
    assert purge_status.json()["state"] == "PURGED"


@pytest.mark.parametrize("operation", ["archive", "purge"])
def test_protocol2_lifecycle_api_redacts_failure_message(
    tmp_path, monkeypatch, operation,
):
    sentinel = "lifecycle-secret-must-not-escape"
    monkeypatch.setenv("MUTEKI_WEB_PASSWORD", "")
    monkeypatch.setenv("MUTEKI_WEB_BIND", "127.0.0.1")
    manager = RunManager(
        sessions_root=tmp_path / "sessions",
        control_root=tmp_path / "control",
    )
    run_id = f"run-{operation}-failure"
    _provision_v2(manager, run_id)

    async def fail_lifecycle(_run_id):
        raise RuntimeError(sentinel)

    monkeypatch.setattr(manager, f"{operation}_protocol2", fail_lifecycle)
    response = TestClient(create_app(manager=manager)).post(
        f"/api/runs/{run_id}/{operation}"
    )

    assert response.status_code == 409
    assert response.json() == {"detail": "RuntimeError"}
    assert sentinel not in response.text


async def test_unknown_id_delete_has_zero_artifact_mutation(tmp_path):
    manager = RunManager(
        sessions_root=tmp_path / "sessions",
        control_root=tmp_path / "control")
    orphan = manager.sessions_root / "unknown.jsonl"
    orphan.write_text("not authoritative")
    tree = manager.sessions_root / "unknown"
    tree.mkdir()
    assert await manager.delete("unknown") is False
    assert orphan.read_text() == "not authoritative"
    assert tree.is_dir()


async def test_protocol2_retention_uses_archive_then_purge_not_legacy_delete(
    tmp_path, monkeypatch,
):
    manager = RunManager(
        sessions_root=tmp_path / "sessions",
        control_root=tmp_path / "control")
    _provision_v2(manager, "run-retain")
    run = manager.create("run-retain")
    run.protocol_version = 2
    run.started = True
    run.finished = True
    monkeypatch.setattr(manager, "_last_activity", lambda _run: 1.0)

    first = await manager.retention_sweep(
        now=100.0, archive_after_s=10.0, delete_after_s=200.0)
    assert first == {"archived": ["run-retain"], "deleted": []}
    assert manager.protocol2.run_view("run-retain")["state"] == "archived"
    second = await manager.retention_sweep(
        now=300.0, archive_after_s=10.0, delete_after_s=200.0)
    assert second == {"archived": [], "deleted": ["run-retain"]}
    assert manager.protocol2.run_view("run-retain")["state"] == "purged"


async def test_protocol2_purge_recovers_delete_before_catalog_receipt_crash(
    tmp_path, monkeypatch,
):
    manager = RunManager(
        sessions_root=tmp_path / "sessions",
        control_root=tmp_path / "control")
    _provision_v2(manager, "run-purge-crash")
    run = manager.create("run-purge-crash")
    run.protocol_version = 2
    run.started = True
    run.finished = True
    display = manager.sessions_root / "run-purge-crash.jsonl"
    display.write_text("display")
    await manager.archive_protocol2("run-purge-crash")

    original = manager.protocol2.purge_item_absent
    injected = {"done": False}

    def crash_once(**kwargs):
        if not injected["done"]:
            injected["done"] = True
            raise RuntimeError("crash after delete before catalog receipt")
        return original(**kwargs)

    monkeypatch.setattr(manager.protocol2, "purge_item_absent", crash_once)
    with pytest.raises(RuntimeError, match="crash after delete"):
        await manager.purge_protocol2("run-purge-crash")
    assert not display.exists()
    status = manager.protocol2.purge_status("purge:run-purge-crash")
    assert status["items"][0]["state"] == "pending"

    monkeypatch.setattr(manager.protocol2, "purge_item_absent", original)
    recovered = await manager.purge_protocol2("run-purge-crash")
    assert recovered["state"] == "purged"
    assert recovered["items"][0]["state"] == "absent"


def test_protocol2_catalog_identity_survives_manager_restart(tmp_path):
    first = RunManager(
        sessions_root=tmp_path / "sessions",
        control_root=tmp_path / "control")
    _provision_v2(first, "run-restart")
    restarted = RunManager(
        sessions_root=tmp_path / "sessions",
        control_root=tmp_path / "control")
    assert restarted.is_protocol2_run("run-restart") is True
    assert restarted.open_workspace("run-restart") is False


async def test_protocol2_real_web_driver_path_has_no_legacy_authority_store(
    tmp_path, monkeypatch, local_provider,
):
    from apps.web.drivers import build_driver
    from muteki.solver.cli_solver import CliSolver
    from muteki.solver.types import SolveOutcome

    _enable_release(monkeypatch)
    _allow_synthetic_host_egress(monkeypatch)
    provider_url, provider_destination = local_provider
    seen_env: dict[str, str] = {}

    async def synthetic_run(self):
        seen_env.update(self._extra_worker_env)
        _observe_provider_egress(self, provider_destination)
        await self.cost.add_external_usd(
            0.001, run_id="run-web-driver", solver_id=self.solver_id,
            input_tokens=10, output_tokens=5,
        )
        raw = "deterministic tool output flag{web-driver}"
        self._protocol2_capture_callback(raw)
        self._protocol2_candidate_callback("fact", {"fact": "candidate only"})
        assert self._protocol2_gate_callback("flag{web-driver}", raw) is True
        return SolveOutcome(
            True, "flag{web-driver}", 1, self.graph, "synthetic",
            flags=["flag{web-driver}"], engine="claude")

    monkeypatch.setattr(CliSolver, "run", synthetic_run)
    manager = RunManager(
        sessions_root=tmp_path / "sessions",
        control_root=tmp_path / "control")
    driver = build_driver(_live_driver_body(provider_url), mgr=manager)
    run = await manager.start("run-web-driver", driver)
    await run.task

    assert run.solved is True
    assert manager.protocol2.status()["production_enabled"] is True
    canonical = manager.protocol2.canonical_run_status("run-web-driver")
    assert canonical["run_store"]["execution"] == "stopped"
    workspace = manager.sessions_root / "run-web-driver" / "workspace"
    assert not (workspace / "graph" / "shared_graph.db").exists()
    assert not (manager.control_root / "run-web-driver" / "control.db").exists()
    assert "MUTEKI_BLACKBOARD_DB" not in seen_env
    assert seen_env["ANTHROPIC_BASE_URL"].startswith("http://127.0.0.1:")
    assert seen_env["ANTHROPIC_BASE_URL"].endswith("/anthropic")
    monkeypatch.setenv("MUTEKI_WEB_PASSWORD", "")
    monkeypatch.setenv("MUTEKI_WEB_BIND", "127.0.0.1")
    response = TestClient(create_app(manager=manager)).get(
        "/api/protocol2/runs/run-web-driver/status")
    assert response.status_code == 200
    assert response.json()["receipt_chain"]["gate"] == canonical[
        "receipt_chain"]["gate"]


class _DriverBus:
    async def emit(self, *_args, **_kwargs):
        return None

    async def close(self):
        return None

    def add_sink(self, *_args, **_kwargs):
        return None


async def _ready_profiles(*, profiles, **_kwargs):
    return ({str(profile.get("id") or profile.get("name")): True
             for profile in profiles}, [])


@pytest.mark.parametrize(
    "failure", ["admission", "capture", "gate", "canary_commit"])
async def test_protocol2_web_driver_failure_never_publishes_solved(
    tmp_path, monkeypatch, local_provider, failure,
):
    from apps.web.drivers import build_driver
    from muteki.epistemic.authority import GateAuthority
    from muteki.epistemic.cas import ReceiptCAS
    from muteki.epistemic.sqlite_store import EpistemicSQLiteStore
    from muteki.runtime.admission import SearchAdmission
    from muteki.solver.cli_solver import CliSolver
    from muteki.solver.types import SolveOutcome

    _enable_release(monkeypatch)
    provider_url, provider_destination = local_provider
    original_seal = ReceiptCAS.seal_bytes
    original_commit = EpistemicSQLiteStore.commit_command
    if failure == "admission":
        def fail_admission(self, request, *, occurred_at_ns):
            raise RuntimeError("admission failpoint")
        monkeypatch.setattr(SearchAdmission, "admit", fail_admission)
    elif failure == "capture":
        def fail_capture(self, data):
            if b"flag{driver-failure}" in data:
                raise RuntimeError("capture failpoint")
            return original_seal(self, data)
        monkeypatch.setattr(ReceiptCAS, "seal_bytes", fail_capture)
    elif failure == "gate":
        def fail_gate(self, **kwargs):
            raise RuntimeError("gate failpoint")
        monkeypatch.setattr(GateAuthority, "evaluate", fail_gate)
    else:
        def fail_canary(self, **kwargs):
            events = kwargs.get("events") or ()
            if any(event.kind == "CANARY_ADMITTED" for event in events):
                raise RuntimeError("catalog commit failpoint")
            return original_commit(self, **kwargs)
        monkeypatch.setattr(EpistemicSQLiteStore, "commit_command", fail_canary)

    worker_started = {"count": 0}

    async def synthetic_run(self):
        worker_started["count"] += 1
        _observe_provider_egress(self, provider_destination)
        await self.cost.add_external_usd(
            0.001, run_id=run_id, solver_id=self.solver_id,
            input_tokens=10, output_tokens=5,
        )
        raw = "deterministic tool output flag{driver-failure}"
        self._protocol2_capture_callback(raw)
        self._protocol2_candidate_callback("fact", {"fact": "candidate only"})
        assert self._protocol2_gate_callback("flag{driver-failure}", raw) is True
        return SolveOutcome(
            True, "flag{driver-failure}", 1, self.graph, "synthetic",
            flags=["flag{driver-failure}"], engine="claude")

    monkeypatch.setattr(CliSolver, "run", synthetic_run)
    manager = RunManager(
        sessions_root=tmp_path / "sessions",
        control_root=tmp_path / "control")
    run_id = f"run-fail-{failure}"
    run = await manager.start(
        run_id, build_driver(_live_driver_body(provider_url), mgr=manager))
    await run.task
    events = run.store.load_all(run_id)
    terminal = [row for row in events if row.get("event_type") == "run.finished"]
    assert terminal
    assert all(not bool(row.get("payload", {}).get("solved")) for row in terminal)
    assert manager.protocol2.catalog._store.event_rows(kind="CANARY_ADMITTED") == ()
    if failure == "admission":
        assert worker_started["count"] == 0
        target = manager.protocol2.run_view(run_id)["target_root"]
        from muteki.epistemic.sqlite_store import EpistemicSQLiteStore as Store
        store = Store.open(Path(target) / "epistemic-v2.db")
        try:
            assert store.event_rows(kind="WORKER_LAUNCH_PREPARED") == ()
        finally:
            store.close()


async def test_protocol2_live_session_admits_captures_gates_and_rebuilds(
    tmp_path, monkeypatch, local_provider,
):
    from apps.web.protocol2_adapter import Protocol2WebAdapter
    from muteki.core.cost import CostController
    from muteki.models.solve_graph import Challenge
    from muteki.solver.cli_driver import driver_for

    monkeypatch.setenv("MUTEKI_CLAUDE_BIN", "/usr/bin/claude")
    monkeypatch.setenv("MUTEKI_PROTOCOL2_BASELINE_RECEIPT", "a" * 64)
    monkeypatch.setenv("MUTEKI_PROTOCOL2_FAULT_SUITE_RECEIPT", "b" * 64)
    _allow_synthetic_host_egress(monkeypatch)
    player = tmp_path / "player.txt"
    player.write_text("fixture")
    provider_url, provider_destination = local_provider
    profile = {
        "id": "deepseek-claude", "name": "deepseek-claude",
        "engine": "claude", "base_url": provider_url,
    }

    class Artifacts:
        def read_text(self, _artifact_id):
            return ""

    adapter = Protocol2WebAdapter(control_root=tmp_path / "control")
    session = adapter.prepare_live_session(
        run_id="run-live", challenge_id="challenge-live",
        attachments=[str(player)], profiles=[profile], artifacts=Artifacts(),
        max_attempts=1, max_barren_attempts=1, wall_ms=10_000,
        token_budget=1000, cost_micro_usd=1_000_000,
        tool_call_budget=10, expected_goal_units=1,
    )

    class Worker:
        solver_id = "cli-claude"
        driver = driver_for(profile)
        challenge = Challenge(
            id="challenge-live", name="fixture", category="misc",
            flag_format=r"flag\{[^}]+\}")
        cost = CostController()
        mode = "bootstrap"
        intent_id_assigned = ""

    worker = Worker()

    async def work():
        _observe_provider_egress(worker, provider_destination)
        await worker.cost.add_external_usd(
            0.001, run_id="run-live", solver_id=worker.solver_id,
            input_tokens=10, output_tokens=5,
        )
        raw = "command output flag{receipt-backed}"
        worker._protocol2_capture_callback(raw)
        worker._protocol2_candidate_callback("fact", {"fact": "lead only"})
        assert worker._protocol2_gate_callback(
            "flag{receipt-backed}", raw) is True
        return type("Outcome", (), {
            "solved": True, "flags": ["flag{receipt-backed}"]})()

    task = session.schedule_worker(worker, work, name="worker")
    await task
    receipts = await session.finalize(solved=True)
    kinds = [row["kind"] for row in session.ports.store.event_rows()]

    assert "ATTEMPT_ADMITTED" in kinds
    assert "CAPTURE_CHUNK_SEALED" in kinds
    assert "CANDIDATE_REPORTED" in kinds
    assert "FLAG_ACCEPTED" in kinds
    assert "GOAL_COMPLETED" in kinds
    assert "EXECUTION_SCOPE_DRAINED" in kinds
    assert len(receipts["canary"]) == 64
    assert session.ports.store.state().run_execution.value == "stopped"
    for name in (
        "admission",
        "cas",
        "egress",
        "egress_observation",
        "eval_assignment",
        "kernel",
        "network_policy",
    ):
        assert session.ports.store.resolve_receipt(receipts[name]).digest == receipts[
            name
        ]
    for name in (
        "platform_admission",
        "platform_cleanup",
        "platform_supervisor",
        "schema",
    ):
        assert adapter.catalog._store.resolve_receipt(receipts[name]).digest == receipts[
            name
        ]

    completed = await adapter.complete_live_session(
        run_id="run-live", session=session, solved=True)
    assert completed["canary_digest"] == receipts["canary"]
    global_status = adapter.status()
    assert global_status["production_enabled"] is True
    assert global_status["latest_receipt_chain"]["gate"] == receipts["gate"]
    canonical = adapter.canonical_run_status("run-live")
    assert canonical["run"]["state"] == "sealed"
    assert canonical["run_store"]["execution"] == "stopped"
    assert canonical["receipt_chain"]["projection_rebuild"] == receipts[
        "projection_rebuild"]

    from muteki.epistemic.contracts import canonical_digest
    from muteki.epistemic.sqlite_store import CommandEvent, ProjectionMutation
    from muteki.runtime.canary import CanaryEvidence, CanaryLevel, admit_canary

    canonical_chain = global_status["latest_receipt_chain"]
    semantic_names = (
        "admission",
        "cas",
        "egress",
        "egress_observation",
        "eval_assignment",
        "kernel",
        "network_policy",
        "platform_admission",
        "platform_cleanup",
        "platform_supervisor",
        "schema",
    )

    def admit_test_chain(label, chain, ordinal):
        canary_digest = admit_canary(CanaryEvidence(
            CanaryLevel.LIVE_LOCAL,
            chain,
            fault_suite_green=True,
            gate_equivalent=True,
            projection_rebuild_equivalent=True,
        ))
        payload = {
            "canary_digest": canary_digest,
            "level": "live_local",
            "receipt_chain": chain,
            "run_id": "run-live",
        }
        store = adapter.catalog._store
        store.commit_command(
            command_id=f"test:canary:{label}",
            idempotency_key=f"test:canary:{label}",
            command_payload=payload,
            events=[CommandEvent(
                f"event:test:canary:{label}",
                "CANARY_ADMITTED",
                "test",
                10_000 + ordinal,
                payload,
            )],
            projection_mutations=[ProjectionMutation(
                "canary_commit_guard", payload
            )],
            authority_capability=store._canary_commit_capability,
            committed_at_ns=10_000 + ordinal,
        )

    ordinal = 0
    for name in semantic_names:
        ordinal += 1
        tampered = {
            **canonical_chain,
            name: canonical_digest({"shape-valid-substitute": name}),
        }
        admit_test_chain(f"tampered:{name}", tampered, ordinal)
        rejected = adapter.status()
        assert rejected["production_enabled"] is False
        assert "not canonically bound" in rejected["reason"]
        ordinal += 1
        admit_test_chain(f"restore:{name}", canonical_chain, ordinal)
        assert adapter.status()["production_enabled"] is True


async def test_direct_task_cancel_uncancel_cannot_publish_live_success(tmp_path):
    from muteki.epistemic.contracts import canonical_digest
    from muteki.epistemic.sqlite_store import IntegrityError
    from muteki.runtime.composition import HostRunFactory
    from muteki.runtime.controller import BootRecoveryCapability
    from muteki.runtime.live_session import Protocol2RunSession
    from muteki.runtime.run_catalog import RunCatalog

    class Artifacts:
        def read_text(self, _artifact_id):
            return ""

    run_id = "run-direct-cancel-uncancel"
    catalog = RunCatalog.create(root=tmp_path / "catalog")
    catalog.create_draft(
        draft_id="draft:direct-cancel",
        policy={"offline": True, "protocol": 2},
        occurred_at_ns=1,
    )
    catalog.begin_provision(
        operation_id="provision:direct-cancel",
        draft_id="draft:direct-cancel",
        run_id=run_id,
        target_root=tmp_path / "run",
        manifest_digest=canonical_digest({"fixture": run_id}),
        owner_epoch=1,
        occurred_at_ns=2,
    )
    catalog.materialize(
        operation_id="provision:direct-cancel", occurred_at_ns=3
    )
    factory = HostRunFactory(catalog=catalog, artifacts=Artifacts())
    _context, ports = factory.open(
        run_id=run_id,
        boot_capability=BootRecoveryCapability(1, 1, "direct-cancel-test"),
        occurred_at_ns=4,
    )
    scope, supervisor = factory.start_execution(
        ports=ports,
        idempotency_key="start:direct-cancel",
        occurred_at_ns=5,
    )
    per_attempt = {"attempts": 1, "tokens": 100}
    ports.admission.create_branch(
        branch_id="root", max_attempts=1, occurred_at_ns=6
    )
    ports.admission.create_budget_account(
        account_id="run", limits=per_attempt, occurred_at_ns=7
    )
    session = Protocol2RunSession(
        ports=ports,
        scope=scope,
        supervisor=supervisor,
        policy_digest=canonical_digest({"policy": "offline"}),
        budget_account_id="run",
        per_attempt_budget=per_attempt,
        max_barren_attempts=1,
        expected_goal_units=1,
    )

    class Driver:
        name = "synthetic"

    class Challenge:
        flag_format = r"flag\{[^}]+\}"

    class Worker:
        solver_id = "direct-cancel-worker"
        driver = Driver()
        challenge = Challenge()
        cost = None
        mode = "synthetic"
        intent_id_assigned = ""

    worker = Worker()
    entered = asyncio.Event()
    attempted: list[str] = []

    async def uncancel_then_report_success():
        entered.set()
        try:
            await asyncio.Event().wait()
        except asyncio.CancelledError:
            current = asyncio.current_task()
            assert current is not None
            assert current.uncancel() == 0
            attempted.append("candidate")
            try:
                worker._protocol2_candidate_callback(
                    "fact", {"fact": "post-cancel candidate"}
                )
            except asyncio.CancelledError:
                pass
            attempted.append("gate")
            try:
                worker._protocol2_gate_callback(
                    "flag{post_cancel}", "output flag{post_cancel}"
                )
            except asyncio.CancelledError:
                pass
            attempted.append("solved")
            return type(
                "Outcome",
                (),
                {"solved": True, "flags": ["flag{post_cancel}"]},
            )()

    task = session.schedule_worker(
        worker, uncancel_then_report_success, name="worker"
    )
    await entered.wait()
    progress_before_cancel = session.ports.store.event_rows(
        kind="PROGRESS_RECORDED"
    )
    assert [row["payload"]["kind"] for row in progress_before_cancel] == [
        "activity"
    ]
    assert task.cancel("external-owner-stop") is True
    with pytest.raises(asyncio.CancelledError):
        await task
    assert attempted == ["candidate", "gate", "solved"]

    with pytest.raises(IntegrityError, match="unresolved runtime owners"):
        await session.finalize(solved=False)
    events = session.ports.store.event_rows()
    kinds = [row["kind"] for row in events]
    for forbidden in (
        "CANDIDATE_REPORTED",
        "FLAG_ACCEPTED",
        "EFFECT_OBSERVED",
        "WORKER_TERMINAL",
        "BUDGET_SETTLED",
        "GOAL_COMPLETED",
    ):
        assert forbidden not in kinds
    assert len(session.ports.store.event_rows(kind="EFFECT_UNKNOWN")) == 1
    assert len(session.ports.store.event_rows(kind="WORKER_UNKNOWN")) == 1
    assert len(session.ports.store.event_rows(kind="BUDGET_USAGE_UNKNOWN")) == 1
    progress_kinds = {
        row["payload"]["kind"]
        for row in session.ports.store.event_rows(kind="PROGRESS_RECORDED")
    }
    assert progress_kinds == {"activity"}
    assert session.ports.store.event_rows(kind="S4E_CLOSURE_ATTESTED") == ()
    session.ports.store.close()


async def test_protocol2_unsolved_finalize_quiesces_then_drains_scope(
    tmp_path, monkeypatch, local_provider,
):
    from apps.web.protocol2_adapter import Protocol2WebAdapter

    _enable_release(monkeypatch)
    _allow_synthetic_host_egress(monkeypatch)
    provider_url, _provider_destination = local_provider
    fixture = tmp_path / "fixture.txt"
    fixture.write_text("neutral fixture")
    profile = {
        "id": "deepseek-claude", "name": "deepseek-claude",
        "engine": "claude", "base_url": provider_url,
    }

    class Artifacts:
        def read_text(self, _artifact_id):
            return ""

    adapter = Protocol2WebAdapter(control_root=tmp_path / "control")
    session = adapter.prepare_live_session(
        run_id="run-unsolved-drain", challenge_id="challenge",
        attachments=[str(fixture)], profiles=[profile], artifacts=Artifacts(),
        max_attempts=1, max_barren_attempts=1, wall_ms=1_000,
        token_budget=100, cost_micro_usd=100_000, tool_call_budget=2,
        expected_goal_units=1,
    )
    receipts = await session.finalize(solved=False)
    kinds = [row["kind"] for row in session.ports.store.event_rows()]
    assert "EXECUTION_STOP_REQUESTED" in kinds
    assert "EXECUTION_SCOPE_DRAINED" in kinds
    assert session.ports.store.state().run_execution.value == "stopped"
    assert "s4e_closure" in receipts
    assert adapter.catalog._store.event_rows(kind="CANARY_ADMITTED") == ()
    await adapter.complete_live_session(
        run_id="run-unsolved-drain", session=session, solved=False
    )


async def test_adapter_retains_live_owner_and_store_until_cancelled_finalize_lands(
    tmp_path,
):
    from apps.web.protocol2_adapter import Protocol2WebAdapter

    adapter = Protocol2WebAdapter(control_root=tmp_path / "control")
    finalize_entered = asyncio.Event()
    release_finalize = asyncio.Event()
    state = {"finalized": False, "closed": False}

    class Store:
        def close(self):
            assert state["finalized"] is True
            state["closed"] = True

    class Session:
        ports = type("Ports", (), {"store": Store()})()

        async def finalize(self, *, solved: bool):
            assert solved is False
            finalize_entered.set()
            await release_finalize.wait()
            state["finalized"] = True
            return {"s4e_closure": "a" * 64}

    session = Session()
    run_id = "run-cancel-finalize"
    adapter._live[run_id] = session
    task = asyncio.create_task(adapter.complete_live_session(
        run_id=run_id, session=session, solved=False,
    ))
    await finalize_entered.wait()
    task.cancel()
    await asyncio.sleep(0)

    assert adapter._live[run_id] is session
    assert state == {"finalized": False, "closed": False}
    release_finalize.set()
    with pytest.raises(asyncio.CancelledError):
        await task

    assert state == {"finalized": True, "closed": True}
    assert run_id not in adapter._live


async def test_abort_owner_mismatch_fails_closed_and_preserves_real_owner(tmp_path):
    from apps.web.protocol2_adapter import (
        Protocol2Unavailable,
        Protocol2WebAdapter,
    )

    adapter = Protocol2WebAdapter(control_root=tmp_path / "control")
    state = {"finalize_calls": 0, "closed": False}

    class Store:
        def close(self):
            state["closed"] = True

    class RealSession:
        ports = type("Ports", (), {"store": Store()})()

        async def finalize(self, *, solved: bool):
            assert solved is False
            state["finalize_calls"] += 1
            return {"s4e_closure": "a" * 64}

    real_session = RealSession()
    impostor_session = object()
    run_id = "run-abort-owner-mismatch"
    adapter._live[run_id] = real_session

    with pytest.raises(
        Protocol2Unavailable, match="^Protocol 2 live owner mismatch$"
    ):
        await adapter.abort_live_session(
            run_id=run_id, session=impostor_session
        )

    assert adapter._live[run_id] is real_session
    assert state == {"finalize_calls": 0, "closed": False}

    await adapter.abort_live_session(run_id=run_id, session=real_session)
    assert state == {"finalize_calls": 1, "closed": True}
    assert run_id not in adapter._live


async def test_adapter_keeps_owner_and_store_when_finalize_fails(tmp_path):
    from apps.web.protocol2_adapter import Protocol2WebAdapter

    adapter = Protocol2WebAdapter(control_root=tmp_path / "control")
    state = {"closed": False}

    class Store:
        def close(self):
            state["closed"] = True

    class Session:
        ports = type("Ports", (), {"store": Store()})()

        async def finalize(self, *, solved: bool):
            assert solved is False
            raise RuntimeError("canonical finalize failed")

    session = Session()
    run_id = "run-failed-finalize"
    adapter._live[run_id] = session

    with pytest.raises(RuntimeError, match="canonical finalize failed"):
        await adapter.complete_live_session(
            run_id=run_id, session=session, solved=False,
        )

    assert adapter._live[run_id] is session
    assert state["closed"] is False


async def test_cancelled_finalize_failure_reraises_caller_cancel_and_retains_owner(
    tmp_path,
):
    from apps.web.protocol2_adapter import Protocol2WebAdapter

    adapter = Protocol2WebAdapter(control_root=tmp_path / "control")
    finalize_entered = asyncio.Event()
    release_finalize = asyncio.Event()
    state = {"closed": False}

    class Store:
        def close(self):
            state["closed"] = True

    class Session:
        ports = type("Ports", (), {"store": Store()})()

        async def finalize(self, *, solved: bool):
            assert solved is False
            finalize_entered.set()
            await release_finalize.wait()
            raise RuntimeError("secret=must-not-escape")

    session = Session()
    run_id = "run-cancel-failed-finalize"
    adapter._live[run_id] = session
    task = asyncio.create_task(adapter.complete_live_session(
        run_id=run_id, session=session, solved=False,
    ))
    await finalize_entered.wait()
    task.cancel("caller-cancelled")
    await asyncio.sleep(0)
    release_finalize.set()

    with pytest.raises(asyncio.CancelledError) as caught:
        await task

    assert caught.value.args == ("caller-cancelled",)
    assert adapter._live[run_id] is session
    assert state["closed"] is False
    diagnostic = caught.value.__cause__
    assert diagnostic is not None
    assert "RuntimeError" in str(diagnostic)
    assert "must-not-escape" not in str(diagnostic)


@pytest.mark.parametrize("protocol", [1, 2], ids=["protocol1", "protocol2"])
async def test_runtime_failure_terminal_protocol_boundary(tmp_path, protocol):
    from muteki.core.events import Event, EventType

    manager = RunManager(sessions_root=tmp_path / "sessions")
    accepted = "flag{typed-public-state}"
    terminal: list[dict] = []

    async def driver(run):
        async def sink(event):
            if event.event_type is EventType.RUN_FINISHED:
                terminal.append(event.payload)

        run.bus.add_sink(sink)
        if protocol == 2:
            # Model a prior canonical typed reconciliation. It may populate the
            # public Run projection, but the fallback lifecycle event must not
            # republish those bytes through the untyped aggregate.
            await run.bus.emit(Event(
                event_type=EventType.FLAG_ACCEPTED,
                run_id=run.run_id,
                payload={
                    "schema_id": "muteki.flag-accepted-projection.v1",
                    "flag": accepted,
                },
            ))
        else:
            run.flag = accepted
            run.flags = [accepted]
        raise RuntimeError("deterministic driver failure")

    driver.protocol_version = protocol
    run = await manager.start(f"runtime-failure-protocol-{protocol}", driver)
    await run.task

    assert run.flag == accepted
    assert run.flags == [accepted]
    assert len(terminal) == 1
    assert terminal[0]["reason"] == "runtime_failure"
    if protocol == 2:
        assert "flag" not in terminal[0]
        assert "flags" not in terminal[0]
    else:
        assert terminal[0]["flag"] == accepted
        assert terminal[0]["flags"] == [accepted]


@pytest.mark.parametrize(
    ("protocol", "expected_public_flag"),
    [(1, "flag{private-driver-outcome}"), (2, None)],
    ids=["protocol1", "protocol2"],
)
async def test_web_driver_outcome_publication_protocol_boundary(
    tmp_path, monkeypatch, protocol, expected_public_flag,
):
    from apps.web import drivers
    import muteki.learning.distill as distill_module
    import muteki.sandbox.manager as sandbox_module
    import muteki.swarm.swarm as swarm_module

    private_flag = "flag{private-driver-outcome}"
    session = object()
    calls: list[str] = []

    class Outcome:
        solved = True
        flag = private_flag
        flags = [private_flag]
        winner = "cli-claude"

    outcome = Outcome()

    class Adapter:
        def prepare_live_session(self, **_kwargs):
            calls.append("prepare")
            return session

        async def complete_live_session(self, *, run_id, session: object, solved):
            assert run_id == f"run-driver-protocol-{protocol}"
            assert session is globals_session
            assert solved is True
            # Completion still receives the canonical owner while the private
            # same-attempt outcome remains intact for finalization/reconciliation.
            assert outcome.flag == private_flag
            assert outcome.flags == [private_flag]
            calls.append("complete")

    globals_session = session

    class Sandbox:
        def __init__(self, **_kwargs):
            pass

        async def shutdown_all(self):
            calls.append("sandbox")

    class Swarm:
        def __init__(self, *_args, **kwargs):
            assert (kwargs.get("protocol2_session") is globals_session) is (
                protocol == 2)

        async def run(self):
            return outcome

    class WorkerConfig:
        @staticmethod
        def resolve(_category):
            return {}

    class Manager:
        protocol2 = Adapter()
        protocol2_error = ""
        sessions_root = tmp_path / "sessions"
        worker_config = WorkerConfig()

        @staticmethod
        def workspace_dir(run_id):
            root = tmp_path / run_id
            root.mkdir(parents=True, exist_ok=True)
            return root

        @staticmethod
        def persist_profile_readiness(_run):
            return None

    class Run:
        def __init__(self):
            self.run_id = f"run-driver-protocol-{protocol}"
            self.bus = _DriverBus()
            self.cost = None
            self.hitl = None
            self.worker_cmds = None
            self.worker_registry = None
            self.flag = None
            self.flags = []
            self.profile_readiness = {}

    monkeypatch.setattr(sandbox_module, "SandboxManager", Sandbox)
    monkeypatch.setattr(drivers, "_resolve_swarm_class", lambda _spec: Swarm)
    monkeypatch.setattr(drivers, "_startup_readiness", _ready_profiles)
    monkeypatch.setattr(distill_module, "TemplateStore", lambda **_kwargs: object())
    body = _live_driver_body()
    body["protocol"] = protocol
    if protocol == 1:
        body.update({"engines": ["claude"], "worker_profiles": []})
    driver = drivers._swarm_driver(body, mgr=Manager())
    run = Run()

    await driver(run)

    assert run.flag == expected_public_flag
    assert run.flags == []
    assert outcome.flag == private_flag
    assert outcome.flags == [private_flag]
    assert calls == (["prepare", "complete", "sandbox"]
                     if protocol == 2 else ["sandbox"])


async def test_protocol2_driver_aborts_canonical_owner_on_baseexception(
    tmp_path, monkeypatch,
):
    from apps.web import drivers
    import muteki.learning.distill as distill_module
    import muteki.sandbox.manager as sandbox_module
    import muteki.swarm.swarm as swarm_module

    calls: list[str] = []
    protocol2_session = object()

    class DriverBoundaryFailure(BaseException):
        pass

    original_failure = DriverBoundaryFailure("owner failed")

    class Adapter:
        def prepare_live_session(self, **_kwargs):
            calls.append("prepare")
            return protocol2_session

        async def abort_live_session(self, *, run_id, session):
            assert run_id == "run-driver-baseexception"
            assert session is protocol2_session
            calls.append("abort")
            raise KeyboardInterrupt("abort cleanup failed")

    class Sandbox:
        def __init__(self, **_kwargs):
            pass

        async def shutdown_all(self):
            calls.append("sandbox")
            raise SystemExit("sandbox cleanup failed")

    class Swarm:
        def __init__(self, *_args, **_kwargs):
            pass

        async def run(self):
            raise original_failure

    class WorkerConfig:
        @staticmethod
        def resolve(_category):
            return {}

    class Manager:
        protocol2 = Adapter()
        protocol2_error = ""
        sessions_root = tmp_path / "sessions"
        worker_config = WorkerConfig()

        @staticmethod
        def workspace_dir(run_id):
            root = tmp_path / run_id
            root.mkdir(parents=True, exist_ok=True)
            return root

        @staticmethod
        def persist_profile_readiness(_run):
            return None

    class Run:
        run_id = "run-driver-baseexception"
        bus = _DriverBus()
        cost = None
        hitl = None
        worker_cmds = None
        worker_registry = None
        flag = None
        profile_readiness = {}

    monkeypatch.setattr(sandbox_module, "SandboxManager", Sandbox)
    monkeypatch.setattr(drivers, "_resolve_swarm_class", lambda _spec: Swarm)
    monkeypatch.setattr(distill_module, "TemplateStore", lambda **_kwargs: object())
    body = _live_driver_body()
    body["challenge"]["id"] = Run.run_id
    driver = drivers._swarm_driver(body, mgr=Manager())

    caught = None
    try:
        await driver(Run())
    except BaseException as exc:
        caught = exc

    assert caught is original_failure
    assert type(caught) is DriverBoundaryFailure
    assert calls == ["prepare", "abort", "sandbox"]


async def test_protocol2_same_named_attachment_is_scoped_per_run(
    tmp_path, monkeypatch, local_provider,
):
    from apps.web.protocol2_adapter import Protocol2WebAdapter

    _enable_release(monkeypatch)
    _allow_synthetic_host_egress(monkeypatch)
    provider_url, _destination = local_provider
    fixture = tmp_path / "same-name.txt"
    fixture.write_text("neutral fixture")
    profile = {
        "id": "deepseek-claude", "name": "deepseek-claude",
        "engine": "claude", "base_url": provider_url,
    }

    class Artifacts:
        def read_text(self, _artifact_id):
            return ""

    adapter = Protocol2WebAdapter(control_root=tmp_path / "control")
    sessions = []
    for run_id in ("run-a", "run-b"):
        sessions.append((run_id, adapter.prepare_live_session(
            run_id=run_id, challenge_id=f"challenge:{run_id}",
            attachments=[str(fixture)], profiles=[profile], artifacts=Artifacts(),
            max_attempts=1, max_barren_attempts=1, wall_ms=1000,
            token_budget=100, cost_micro_usd=1000, tool_call_budget=1,
            expected_goal_units=1)))
    attachments = adapter.catalog._store.draft_attachments("draft:run-a")
    other = adapter.catalog._store.draft_attachments("draft:run-b")
    assert attachments[0]["attachment_id"] != other[0]["attachment_id"]
    for run_id, session in sessions:
        await adapter.abort_live_session(run_id=run_id, session=session)
