from __future__ import annotations

import asyncio
import json
import sys
import threading
import time
from dataclasses import replace
from dataclasses import dataclass

import pytest

from muteki.epistemic.cas import ReceiptCAS
from muteki.epistemic.contracts import canonical_digest
from muteki.epistemic.sqlite_store import (
    CommandEvent,
    EpistemicSQLiteStore,
    IdempotencyConflict,
    IntegrityError,
    ProjectionMutation,
)
from muteki.models.solve_graph import Challenge
from muteki.runtime.admission import AdmissionRequest, SearchAdmission
from muteki.runtime.c6_transport import (
    C6HostLaunchBroker,
    C6HostLaunchInterlock,
    C6HostLaunchProfileV1,
    C6HostPopenAdapter,
    C6LaunchMaterialV1,
    C6TransportRejected,
)
from muteki.runtime.cognition import (
    COGNITIVE_CONTEXT_ACTOR,
    CognitiveContextAuthority,
    CognitiveFeatureGateV1,
    DecisionContextInputV1,
    PromptLaunchClaimV1,
)
from muteki.runtime.contracts import (
    AttemptIdentity,
    EffectClass,
    ExecutionScope,
    LeaseIdentity,
)
from muteki.runtime.controller import BootRecoveryCapability, LiveHealthGuard
from muteki.runtime.composition import HostRunFactory
from muteki.runtime.live_session import Protocol2LiveRejected, Protocol2RunSession
from muteki.runtime.permit_resolver import CanonicalPermitResolver
from muteki.runtime.run_catalog import RunCatalog
from muteki.solver.cli_driver import CliResult
from muteki.solver.cli_solver import CliSolver


def _runtime(tmp_path):
    store = EpistemicSQLiteStore.create(
        path=tmp_path / "epistemic-v2.db",
        run_id="run-cognition",
        manifest_digest="a" * 64,
    )
    store.commit_command(
        command_id="ready",
        idempotency_key="ready",
        command_payload={},
        events=(CommandEvent("event:ready", "BOOT_READY", "host", 1),),
        committed_at_ns=1,
    )
    store.commit_command(
        command_id="start",
        idempotency_key="start",
        command_payload={},
        events=(
            CommandEvent(
                "event:start",
                "START_EXECUTION",
                "host",
                2,
                {"execution_generation": 1, "run_fence_epoch": 1},
            ),
        ),
        projection_mutations=(
            ProjectionMutation(
                "execution_start_guard",
                {"execution_generation": 1, "run_fence_epoch": 1},
            ),
        ),
        authority_capability=store._lifecycle_commit_capability,
        committed_at_ns=2,
    )
    guard = LiveHealthGuard()
    capability = BootRecoveryCapability(1, 1, "owner")
    guard.begin_boot_finalize(capability)
    guard.open_admission(capability=capability, attestation_digest="b" * 64)
    cas = ReceiptCAS(tmp_path / "receipt-cas")
    admission = SearchAdmission(store=store, guard=guard, cas=cas)
    admission.create_branch(
        branch_id="root", max_attempts=3, occurred_at_ns=3
    )
    admission.create_budget_account(
        account_id="run",
        limits={"attempts": 3, "tokens": 300, "wall_ms": 30_000},
        occurred_at_ns=4,
    )
    return store, admission, cas


def _attempt(ordinal: int = 1) -> tuple[AttemptIdentity, LeaseIdentity]:
    attempt = AttemptIdentity(
        ExecutionScope("run-cognition", 1, 1),
        "root",
        f"attempt-{ordinal}",
        ordinal,
    )
    return attempt, LeaseIdentity(attempt, f"lease-{ordinal}", 1, ordinal)


def _context(*, decision_need: str = "Separate hypothesis A from B."):
    return DecisionContextInputV1(
        objective="Find the next causal distinction.",
        decision_need=decision_need,
        acceptance_boundary="Only the unchanged hard gate accepts.",
        non_negotiable_policy=(
            "offline",
            "proposal prose is not verified evidence",
        ),
        remaining_budget={"attempts": 1, "tokens": 100, "wall_ms": 10_000},
        effect_ambiguity=("no unresolved effect owner",),
    )


def _admit(admission, attempt, lease, packet, *, expires_at_ns: int | None = None):
    if expires_at_ns is None:
        expires_at_ns = time.time_ns() + 60_000_000_000
    return admission.admit(
        AdmissionRequest(
            attempt=attempt,
            lease=lease,
            permit_id=f"permit-{attempt.launch_ordinal}",
            account_id="run",
            requested_budget={
                "attempts": 1,
                "tokens": 100,
                "wall_ms": 10_000,
            },
            conflict_keys=(),
            effect_class=EffectClass.OBSERVABLE,
            fingerprint=canonical_digest(
                {
                    "attempt": attempt.digest,
                    "packet": packet.binding.packet_digest,
                }
            ),
            policy_digest="c" * 64,
            expires_at_ns=expires_at_ns,
            context_packet=packet.binding,
        ),
        occurred_at_ns=20,
    )


def _claim_c6_launch(
    *,
    authority: CognitiveContextAuthority,
    delivered,
    permit,
    staged,
    invocation,
    argv: tuple[str, ...],
    cwd: str,
    occurred_at_ns: int,
):
    profile = C6HostLaunchProfileV1(driver_name="synthetic")
    material = C6LaunchMaterialV1.build(
        argv_artifact_digest=invocation.argv_artifact_digest,
        argv=argv,
        cwd=cwd,
        runtime_env={},
        profile=profile,
    )
    assert authority._seal_host_launch_material(body=material.canonical_body()) == material.digest
    claim = authority.claim_prompt_launch(
        delivered=delivered,
        permit=permit,
        staged=staged,
        invocation=invocation,
        profile_digest=profile.digest,
        launch_material_digest=material.digest,
        occurred_at_ns=occurred_at_ns,
    )
    return claim, profile


def _armed_c6_host_broker(tmp_path):
    """Build one real claimed owner and its host-only C6 runner for race tests."""

    store, admission, cas = _runtime(tmp_path)
    attempt, lease = _attempt()
    authority = CognitiveContextAuthority(store=store, cas=cas)
    delivered = authority.compile_for_attempt(
        attempt=attempt,
        context=_context(),
        feature_gate=CognitiveFeatureGateV1(),
        occurred_at_ns=10,
    )
    permit = _admit(admission, attempt, lease, delivered)
    CanonicalPermitResolver(store=store, scope=attempt.scope).claim_launch(
        permit, now_ns=21
    )
    profile = C6HostLaunchProfileV1(driver_name="synthetic")
    interlock = C6HostLaunchInterlock()
    interlock.register(permit=permit)
    adapter = C6HostPopenAdapter(
        authority=authority,
        delivered=delivered,
        driver=_Driver(),
        profile=profile,
        interlock=interlock,
    )
    broker = C6HostLaunchBroker(
        authority=authority,
        delivered=delivered,
        profile=profile,
        host_adapter=adapter,
    )
    broker.activate(permit=permit)
    prompt = "runtime header\n" + delivered.render_for_prompt()
    argv = (sys.executable, "-c", "print('READY')", prompt)
    return (
        store,
        admission,
        authority,
        permit,
        interlock,
        broker,
        prompt,
        argv,
    )


def test_context_packet_is_canonical_predecessor_and_replays(tmp_path):
    store, admission, cas = _runtime(tmp_path)
    attempt, lease = _attempt()
    authority = CognitiveContextAuthority(store=store, cas=cas)

    first = authority.compile_for_attempt(
        attempt=attempt,
        context=_context(),
        feature_gate=CognitiveFeatureGateV1(),
        occurred_at_ns=10,
    )
    permit = _admit(admission, attempt, lease, first)
    staged = authority.stage_prompt(
        delivered=first,
        permit=permit,
        prompt="runtime header\n" + first.render_for_prompt() + "runtime body\n",
        transport="argv",
        occurred_at_ns=21,
    )

    rows = store.event_rows()
    seq = {row["kind"]: row["seq"] for row in rows}
    assert seq["RUNTIME_CONTEXT_DECISION_REGISTERED"] < seq[
        "CONTEXT_PACKET_COMPILED"
    ]
    assert seq["CONTEXT_PACKET_COMPILED"] < seq["ATTEMPT_ADMITTED"]
    assert seq["ATTEMPT_ADMITTED"] < seq["CONTEXT_PROMPT_STAGED"]
    assert permit.constraints["context_packet"] == first.binding.canonical_body()
    assert first.binding.accepted_set_change is False
    assert "Sealed decision context" in first.render_for_prompt()
    assert "proposal prose is not verified evidence" in first.render_for_prompt()
    stage = store.event_rows(kind="CONTEXT_PROMPT_STAGED")[0]["payload"]
    assert stage["stage_id"] == staged.stage_id
    assert stage["prompt_artifact_digest"] == staged.assembly.full_prompt_digest

    reopened = EpistemicSQLiteStore.open(store.path)
    replayed = CognitiveContextAuthority(store=reopened, cas=cas).compile_for_attempt(
        attempt=attempt,
        context=_context(),
        feature_gate=CognitiveFeatureGateV1(),
        occurred_at_ns=30,
    )
    assert replayed.binding == first.binding
    assert replayed.packet.bytes == first.packet.bytes
    assert reopened.verify().checksum == store.verify().checksum


def test_one_predecision_slot_rejects_divergent_context_before_admission(tmp_path):
    store, _admission, cas = _runtime(tmp_path)
    attempt, _lease = _attempt()
    authority = CognitiveContextAuthority(store=store, cas=cas)
    authority.compile_for_attempt(
        attempt=attempt,
        context=_context(),
        feature_gate=CognitiveFeatureGateV1(),
        occurred_at_ns=10,
    )
    with pytest.raises(IdempotencyConflict, match="different payload"):
        authority.compile_for_attempt(
            attempt=attempt,
            context=_context(decision_need="A divergent mutable worker claim."),
            feature_gate=CognitiveFeatureGateV1(),
            occurred_at_ns=11,
        )


def test_unadmitted_packet_is_terminal_before_any_attempt_permit(tmp_path):
    store, admission, cas = _runtime(tmp_path)
    attempt, lease = _attempt()
    authority = CognitiveContextAuthority(store=store, cas=cas)
    packet = authority.compile_for_attempt(
        attempt=attempt,
        context=_context(),
        feature_gate=CognitiveFeatureGateV1(),
        occurred_at_ns=10,
    )

    receipt = authority.record_packet_unadmitted(
        delivered=packet,
        reason="worker rejected the strict C6 prompt-stage capability",
        occurred_at_ns=12,
    )

    row = store.event_rows(kind="CONTEXT_PACKET_UNADMITTED")[0]
    assert len(receipt) == 64
    assert row["payload"]["packet_digest"] == packet.binding.packet_digest
    assert row["payload"]["feature_state_digest"] == packet.binding.feature_state_digest
    with pytest.raises(IntegrityError, match="closed or diverged"):
        _admit(admission, attempt, lease, packet)
    assert not store.event_rows(kind="ATTEMPT_ADMITTED")


def test_context_event_and_admission_fail_closed_on_forgery(tmp_path):
    store, admission, cas = _runtime(tmp_path)
    attempt, lease = _attempt()
    with pytest.raises(IntegrityError, match="host-only capability"):
        store.commit_command(
            command_id="forged-context",
            idempotency_key="forged-context",
            command_payload={"decision_id": "forged"},
            events=(
                CommandEvent(
                    "event:context-decision:forged",
                    "RUNTIME_CONTEXT_DECISION_REGISTERED",
                    COGNITIVE_CONTEXT_ACTOR,
                    9,
                    {"decision_id": "forged"},
                ),
            ),
            committed_at_ns=9,
        )

    packet = CognitiveContextAuthority(store=store, cas=cas).compile_for_attempt(
        attempt=attempt,
        context=_context(),
        feature_gate=CognitiveFeatureGateV1(),
        occurred_at_ns=10,
    )
    forged = replace(packet.binding, packet_digest="f" * 64)
    with pytest.raises(IntegrityError, match="did not resolve|diverged"):
        admission.admit(
            AdmissionRequest(
                attempt=attempt,
                lease=lease,
                permit_id="permit-forged",
                account_id="run",
                requested_budget={
                    "attempts": 1,
                    "tokens": 100,
                    "wall_ms": 10_000,
                },
                conflict_keys=(),
                effect_class=EffectClass.OBSERVABLE,
                fingerprint="fingerprint-forged",
                policy_digest="c" * 64,
                expires_at_ns=100_000,
                context_packet=forged,
            ),
            occurred_at_ns=20,
        )
    assert not store.event_rows(kind="ATTEMPT_ADMITTED")


def test_context_is_bound_once_and_off_path_prompt_is_unchanged(tmp_path):
    store, _admission, cas = _runtime(tmp_path)
    attempt, _lease = _attempt()
    packet = CognitiveContextAuthority(store=store, cas=cas).compile_for_attempt(
        attempt=attempt,
        context=_context(),
        feature_gate=CognitiveFeatureGateV1(),
        occurred_at_ns=10,
    )
    challenge = Challenge(
        id="run-cognition",
        name="synthetic",
        category="misc",
        description="Choose a causal probe.",
        flag_format="flag{...}",
    )
    baseline = CliSolver(None, challenge, engine="claude", kb=False)
    baseline._protocol2_mode = True
    before = baseline._build_prompt()
    assert "Sealed decision context" not in before

    baseline.bind_cognitive_context(packet)
    baseline.bind_cognitive_context(packet)
    after = baseline._build_prompt()
    assert before != after
    assert packet.binding.packet_digest in after
    with pytest.raises(RuntimeError, match="already bound"):
        other_attempt, _ = _attempt(2)
        other = CognitiveContextAuthority(store=store, cas=cas).compile_for_attempt(
            attempt=other_attempt,
            context=_context(decision_need="Separate C from D."),
            feature_gate=CognitiveFeatureGateV1(),
            occurred_at_ns=30,
        )
        baseline.bind_cognitive_context(other)


def test_context_bound_cli_is_render_only_without_host_runner(tmp_path):
    store, admission, cas = _runtime(tmp_path)
    attempt, lease = _attempt()
    authority = CognitiveContextAuthority(store=store, cas=cas)
    packet = authority.compile_for_attempt(
        attempt=attempt,
        context=_context(),
        feature_gate=CognitiveFeatureGateV1(),
        occurred_at_ns=10,
    )
    _admit(admission, attempt, lease, packet)
    challenge = Challenge(
        id="run-cognition",
        name="synthetic",
        category="misc",
        description="Choose a causal probe.",
        flag_format="flag{...}",
    )
    solver = CliSolver(None, challenge, engine="claude", kb=False)
    solver._protocol2_mode = True
    solver.bind_cognitive_context(packet)
    prompt = solver._build_prompt()

    with pytest.raises(RuntimeError, match="host-owned invocation runner"):
        solver._execute_invocation(prompt, None)
    assert not store.event_rows(kind="CONTEXT_PROMPT_RELEASED")
    assert not store.event_rows(kind="CONTEXT_PROMPT_STAGED")
    with pytest.raises(RuntimeError, match="direct C6 stage ports are retired"):
        solver.bind_cognitive_context(packet, stage_port=object())


def test_prelaunch_abort_closes_scope_but_never_promotes(tmp_path):
    store, admission, cas = _runtime(tmp_path)
    attempt, lease = _attempt()
    authority = CognitiveContextAuthority(store=store, cas=cas)
    delivered = authority.compile_for_attempt(
        attempt=attempt,
        context=_context(),
        feature_gate=CognitiveFeatureGateV1(),
        occurred_at_ns=10,
    )
    permit = _admit(admission, attempt, lease, delivered)
    CanonicalPermitResolver(store=store, scope=attempt.scope).claim_launch(
        permit, now_ns=21
    )
    prompt = "runtime header\n" + delivered.render_for_prompt()
    staged = authority.stage_prompt(
        delivered=delivered,
        permit=permit,
        prompt=prompt,
        transport="argv",
        occurred_at_ns=22,
    )
    argv = ("synthetic-cli", "--prompt", prompt)
    invocation = authority.bind_prompt_invocation(
        delivered=delivered,
        permit=permit,
        staged=staged,
        argv=argv,
        occurred_at_ns=23,
        require_fresh=True,
    )
    claim, _profile = _claim_c6_launch(
        authority=authority,
        delivered=delivered,
        permit=permit,
        staged=staged,
        invocation=invocation,
        argv=argv,
        cwd=str(tmp_path),
        occurred_at_ns=24,
    )

    interlock = C6HostLaunchInterlock()
    interlock.register(permit=permit)
    interlock.arm_claim(
        permit=permit,
        claim=claim,
        claim_live_validator=lambda _permit, _claim: pytest.fail(
            "revoked C6 interlock must not reach its durable-claim validator"
        ),
        claim_launch_fence=lambda _permit, _claim: authority._fence_final_host_launch(
            delivered=delivered,
            permit=permit,
            claim=claim,
        ),
    )
    interlock.revoke(permit=permit, reason="deterministic supervisor terminal race")
    spawn_calls: list[object] = []
    abort_receipts: list[str] = []

    def record_abort(reason: str) -> None:
        abort_receipts.append(
            authority.record_prompt_prelaunch_aborted(
                delivered=delivered,
                permit=permit,
                claim=claim,
                reason=reason,
                occurred_at_ns=25,
            )
        )

    with pytest.raises(C6TransportRejected, match="no longer active"):
        interlock.spawn_under_claim(
            permit=permit,
            claim=claim,
            spawn=lambda: spawn_calls.append(object()),
            on_process_started=lambda _proc: pytest.fail("Popen must not be reached"),
            on_prelaunch_aborted=record_abort,
            on_release_recorded=lambda: pytest.fail("Popen must not be reached"),
            on_release_committed=lambda: pytest.fail("Popen must not be reached"),
            on_fence_rolled_back=lambda: pytest.fail("Popen must not be reached"),
        )
    assert not spawn_calls
    assert len(abort_receipts) == 1
    assert not store.event_rows(kind="CONTEXT_PROMPT_RELEASED")
    assert authority.verify_scope_prompt_stage_closure(
        scope_digest=attempt.scope.digest
    ) == tuple(abort_receipts)
    with pytest.raises(IntegrityError, match="prelaunch-aborted"):
        authority.require_verified_prompt_stages(
            delivered=delivered,
            permit=permit,
        )


@pytest.mark.asyncio
async def test_host_adapter_refuses_an_unpersisted_claim_at_the_popen_fence(
    tmp_path, monkeypatch
):
    """A shaped in-memory claim must never reach the audited spawn callable."""

    store, admission, cas = _runtime(tmp_path)
    attempt, lease = _attempt()
    authority = CognitiveContextAuthority(store=store, cas=cas)
    delivered = authority.compile_for_attempt(
        attempt=attempt,
        context=_context(),
        feature_gate=CognitiveFeatureGateV1(),
        occurred_at_ns=10,
    )
    permit = _admit(admission, attempt, lease, delivered)
    CanonicalPermitResolver(store=store, scope=attempt.scope).claim_launch(
        permit, now_ns=21
    )
    prompt = "runtime header\n" + delivered.render_for_prompt()
    staged = authority.stage_prompt(
        delivered=delivered,
        permit=permit,
        prompt=prompt,
        transport="argv",
        occurred_at_ns=22,
    )
    argv = (sys.executable, "-c", "print('READY')", prompt)
    invocation = authority.bind_prompt_invocation(
        delivered=delivered,
        permit=permit,
        staged=staged,
        argv=argv,
        occurred_at_ns=23,
        require_fresh=True,
    )
    profile = C6HostLaunchProfileV1(driver_name="synthetic")
    material = C6LaunchMaterialV1.build(
        argv_artifact_digest=invocation.argv_artifact_digest,
        argv=argv,
        cwd=str(tmp_path),
        runtime_env={},
        profile=profile,
    )
    assert authority._seal_host_launch_material(body=material.canonical_body()) == material.digest
    forged = PromptLaunchClaimV1(
        staged=staged,
        invocation=invocation,
        claim_id="claim-" + "f" * 32,
        launch_material_digest=material.digest,
        profile_digest=profile.digest,
    )
    interlock = C6HostLaunchInterlock()
    interlock.register(permit=permit)
    adapter = C6HostPopenAdapter(
        authority=authority,
        delivered=delivered,
        driver=_Driver(),
        profile=profile,
        interlock=interlock,
    )
    adapter.activate(permit=permit, on_revoke=lambda _reason: None)
    adapter.arm_claim(claim=forged)

    from muteki.runtime import c6_transport

    spawn_calls: list[object] = []
    abort_reasons: list[str] = []

    def audited_runner(_driver, _argv, **kwargs):
        return kwargs["popen_wrapper"](
            lambda: spawn_calls.append(object())
        )

    monkeypatch.setattr(c6_transport, "_AUDITED_RUN_CLI_STREAMING", audited_runner)
    with pytest.raises(C6TransportRejected, match="durable host launch claim"):
        await adapter.execute(
            final_argv=argv,
            cwd=str(tmp_path),
            timeout=10,
            runtime_env={},
            claim=forged,
            on_process_started=lambda _proc: pytest.fail("Popen must not be reached"),
            on_prelaunch_aborted=abort_reasons.append,
            on_release_committed=lambda: pytest.fail("Popen must not be reached"),
            on_fence_rolled_back=lambda: pytest.fail("Popen must not be reached"),
        )
    assert not spawn_calls
    assert len(abort_reasons) == 1
    assert not store.event_rows(kind="CONTEXT_PROMPT_LAUNCH_CLAIMED")


@pytest.mark.asyncio
async def test_direct_broker_revoke_waits_for_the_popen_interlock(tmp_path, monkeypatch):
    """A direct revoke cannot append PRELAUNCH_ABORTED during a held Popen fence."""

    (
        store,
        _admission,
        _authority,
        _permit,
        interlock,
        broker,
        prompt,
        argv,
    ) = _armed_c6_host_broker(tmp_path)
    from muteki.runtime import c6_transport

    original_stream = c6_transport._AUDITED_RUN_CLI_STREAMING
    original_revoke = interlock.revoke
    at_popen = threading.Event()
    allow_popen = threading.Event()
    revoke_attempted = threading.Event()
    spawned: list[str] = []

    def observed_revoke(*, permit, reason):
        revoke_attempted.set()
        return original_revoke(permit=permit, reason=reason)

    def paused_stream(driver, final_argv, **kwargs):
        wrapped = kwargs["popen_wrapper"]

        def delayed_wrapper(spawn):
            def delayed_spawn():
                at_popen.set()
                if not allow_popen.wait(timeout=5):
                    raise RuntimeError("test did not release the C6 Popen barrier")
                spawned.append("popen")
                return spawn()

            return wrapped(delayed_spawn)

        return original_stream(
            driver,
            final_argv,
            **{**kwargs, "popen_wrapper": delayed_wrapper},
        )

    monkeypatch.setattr(interlock, "revoke", observed_revoke)
    monkeypatch.setattr(c6_transport, "_AUDITED_RUN_CLI_STREAMING", paused_stream)
    launch = asyncio.create_task(
        broker.runner.run(
            prompt=prompt,
            final_argv=list(argv),
            cwd=str(tmp_path),
            timeout=10,
            runtime_env={},
        )
    )
    assert await asyncio.to_thread(at_popen.wait, 2)
    revocation = asyncio.create_task(
        asyncio.to_thread(broker.revoke, "deterministic direct broker revoke")
    )
    assert await asyncio.to_thread(revoke_attempted.wait, 2)
    assert not revocation.done()

    allow_popen.set()
    await launch
    await revocation

    assert spawned == ["popen"]
    assert len(store.event_rows(kind="CONTEXT_PROMPT_RELEASED")) == 1
    assert not store.event_rows(kind="CONTEXT_PROMPT_PRELAUNCH_ABORTED")
    assert not store.event_rows(kind="CONTEXT_PROMPT_UNKNOWN")


@pytest.mark.asyncio
async def test_direct_broker_revoke_can_win_before_popen(tmp_path, monkeypatch):
    """When revoke wins the interlock, no spawn occurs and one abort is canonical."""

    (
        store,
        _admission,
        _authority,
        _permit,
        _interlock,
        broker,
        prompt,
        argv,
    ) = _armed_c6_host_broker(tmp_path)
    from muteki.runtime import c6_transport

    ready_for_revoke = threading.Event()
    allow_wrapper = threading.Event()
    spawned: list[str] = []

    def delayed_stream(_driver, _argv, **kwargs):
        ready_for_revoke.set()
        if not allow_wrapper.wait(timeout=5):
            raise RuntimeError("test did not release the C6 wrapper barrier")
        return kwargs["popen_wrapper"](
            lambda: spawned.append("popen")
        )

    monkeypatch.setattr(c6_transport, "_AUDITED_RUN_CLI_STREAMING", delayed_stream)
    launch = asyncio.create_task(
        broker.runner.run(
            prompt=prompt,
            final_argv=list(argv),
            cwd=str(tmp_path),
            timeout=10,
            runtime_env={},
        )
    )
    assert await asyncio.to_thread(ready_for_revoke.wait, 2)
    await asyncio.to_thread(broker.revoke, "deterministic revoke before Popen")
    allow_wrapper.set()
    with pytest.raises(C6TransportRejected, match="no longer active"):
        await launch

    assert not spawned
    assert len(store.event_rows(kind="CONTEXT_PROMPT_PRELAUNCH_ABORTED")) == 1
    assert not store.event_rows(kind="CONTEXT_PROMPT_RELEASED")
    assert not store.event_rows(kind="CONTEXT_PROMPT_UNKNOWN")


@pytest.mark.asyncio
@pytest.mark.parametrize("follow_on", ("recovery", "unknown", "budget", "boot"))
async def test_cross_store_writer_cannot_overtake_fenced_c6_popen(
    tmp_path, monkeypatch, follow_on
):
    """Recovery cannot write UNKNOWN, then budget/BOOT, before the real Popen.

    The barrier is intentionally inside ``spawn``: it runs after the final
    durable validator and before the actual local Popen.  A second SQLite
    connection first tries the same UNKNOWN path that boot recovery uses, then
    optionally tries a budget or BOOT mutation.  Without the long C6 launch
    transaction this sequence terminalizes the attempt before the held Popen;
    with it, the second writer blocks until RELEASED is committed and then its
    stale UNKNOWN is rejected.
    """

    (
        store,
        _admission,
        _authority,
        _permit,
        _interlock,
        broker,
        prompt,
        argv,
    ) = _armed_c6_host_broker(tmp_path)
    from muteki.runtime import c6_transport

    original_stream = c6_transport._AUDITED_RUN_CLI_STREAMING
    at_post_validation = threading.Event()
    release_spawn = threading.Event()
    contender_started = threading.Event()
    spawned: list[str] = []

    def paused_stream(driver, final_argv, **kwargs):
        wrapped = kwargs["popen_wrapper"]

        def delayed_wrapper(spawn):
            def delayed_spawn():
                at_post_validation.set()
                if not release_spawn.wait(timeout=5):
                    raise RuntimeError("test did not release the C6 Popen barrier")
                spawned.append("popen")
                return spawn()

            return wrapped(delayed_spawn)

        return original_stream(
            driver,
            final_argv,
            **{**kwargs, "popen_wrapper": delayed_wrapper},
        )

    monkeypatch.setattr(c6_transport, "_AUDITED_RUN_CLI_STREAMING", paused_stream)
    launch = asyncio.create_task(
        broker.runner.run(
            prompt=prompt,
            final_argv=list(argv),
            cwd=str(tmp_path),
            timeout=10,
            runtime_env={},
        )
    )
    assert await asyncio.to_thread(at_post_validation.wait, 2)
    plan = broker._plan
    assert plan is not None

    other_store = EpistemicSQLiteStore.open(store.path)
    other_authority = CognitiveContextAuthority(
        store=other_store,
        cas=ReceiptCAS(tmp_path / "receipt-cas"),
    )
    other_guard = LiveHealthGuard()
    other_capability = BootRecoveryCapability(99, 99, "competing-host")
    other_guard.begin_boot_finalize(other_capability)

    if follow_on == "recovery":
        def competing_recovery_then_follow_on():
            contender_started.set()
            return other_authority.recover_dangling_prompt_invocations(
                guard=other_guard,
                occurred_at_ns=30,
            )

    else:
        other_guard.open_admission(
            capability=other_capability,
            attestation_digest="b" * 64,
        )
        other_admission = SearchAdmission(store=other_store, guard=other_guard)

        def competing_recovery_then_follow_on():
            contender_started.set()
            other_authority.record_prompt_unknown(
                delivered=broker._delivered,
                permit=broker._active_permit(),
                staged=plan.staged,
                invocation=plan.invocation,
                reason="competing-host-recovery",
                occurred_at_ns=30,
            )
            if follow_on == "unknown":
                return "unknown"
            if follow_on == "budget":
                return other_admission.hold_unknown_usage(
                    attempt_id="attempt-1",
                    revision=1,
                    occurred_at_ns=31,
                )
            payload = {"boot_epoch": 2, "writer_epoch": 2}
            return other_store.commit_command(
                command_id="BOOT_VERIFYING:competing",
                idempotency_key="BOOT_VERIFYING:competing",
                command_payload=payload,
                events=(
                    CommandEvent(
                        "event:BOOT_VERIFYING:competing",
                        "BOOT_VERIFYING",
                        "competing-host",
                        31,
                        payload,
                    ),
                ),
                committed_at_ns=31,
            )

    contender = asyncio.create_task(
        asyncio.to_thread(competing_recovery_then_follow_on)
    )
    assert await asyncio.to_thread(contender_started.wait, 2)
    with pytest.raises(asyncio.TimeoutError):
        await asyncio.wait_for(asyncio.shield(contender), timeout=0.15)

    release_spawn.set()
    await launch
    if follow_on == "recovery":
        assert await contender == ()
    else:
        with pytest.raises(IntegrityError):
            await contender

    release = store.event_rows(kind="CONTEXT_PROMPT_RELEASED")
    assert spawned == ["popen"]
    assert len(release) == 1
    assert not store.event_rows(kind="CONTEXT_PROMPT_UNKNOWN")
    assert not store.event_rows(kind="BUDGET_USAGE_UNKNOWN")
    assert not store.event_rows(kind="BOOT_VERIFYING")
    other_store.close()


@pytest.mark.asyncio
async def test_fenced_release_storage_fault_rolls_back_before_unknown_closure(
    tmp_path, monkeypatch
):
    """A failed RELEASED write cannot leak an event before fail-closed UNKNOWN.

    The fault runs after command/event/receipt-object insertion but before the
    projection update.  That is the discriminating nested-transaction case: the
    broker catches the failed post-Popen release, kills the child, and records
    UNKNOWN in a second savepoint inside the same long launch fence.  A plain
    outer transaction would otherwise commit a partial RELEASED command when
    the broker handles the error and exits its fence normally.
    """

    (
        store,
        _admission,
        _authority,
        _permit,
        _interlock,
        broker,
        prompt,
        argv,
    ) = _armed_c6_host_broker(tmp_path)
    original_commit = store.commit_command

    def fail_only_release_after_events(**kwargs):
        if str(kwargs.get("command_id", "")).startswith("context:release:"):
            assert kwargs.get("fault_hook") is None

            def injected_fault(phase: str) -> None:
                if phase == "after_events":
                    raise RuntimeError("injected fenced release storage fault")

            kwargs = {**kwargs, "fault_hook": injected_fault}
        return original_commit(**kwargs)

    monkeypatch.setattr(store, "commit_command", fail_only_release_after_events)

    with pytest.raises(RuntimeError, match="injected fenced release storage fault"):
        await broker.runner.run(
            prompt=prompt,
            final_argv=list(argv),
            cwd=str(tmp_path),
            timeout=10,
            runtime_env={},
        )

    # The failed inner RELEASED savepoint leaves no command/event/projection
    # fragment.  The sole canonical terminal is fail-closed UNKNOWN.
    assert not store.event_rows(kind="CONTEXT_PROMPT_RELEASED")
    assert len(store.event_rows(kind="CONTEXT_PROMPT_UNKNOWN")) == 1
    assert not store._conn.execute(
        "SELECT command_id FROM commands WHERE command_id LIKE 'context:release:%'"
    ).fetchall()
    store.verify()


@pytest.mark.asyncio
async def test_outer_fence_commit_fault_kills_child_and_records_unknown(
    tmp_path, monkeypatch
):
    """Outer-commit failure cannot leave a child or an in-memory false release.

    The release savepoint has already completed when the injected outer commit
    fails.  The interlock must kill the real long-lived child before it unwinds;
    only then may the broker write UNKNOWN in a fresh transaction.  This is
    distinct from the nested command-fault test above: it protects the exact
    external-effect gap between a successful RELEASED savepoint and the fence's
    final database commit.
    """

    (
        store,
        _admission,
        _authority,
        _permit,
        _interlock,
        broker,
        prompt,
        _argv,
    ) = _armed_c6_host_broker(tmp_path)
    from muteki.runtime import c6_transport

    original_stream = c6_transport._AUDITED_RUN_CLI_STREAMING
    original_kill = c6_transport._kill_process
    children = []
    killed_pids: list[int] = []
    argv = (
        sys.executable,
        "-c",
        "import time; time.sleep(30)",
        prompt,
    )

    def observed_stream(driver, final_argv, **kwargs):
        wrapped = kwargs["popen_wrapper"]

        def observed_wrapper(spawn):
            def observed_spawn():
                proc = spawn()
                children.append(proc)
                return proc

            return wrapped(observed_spawn)

        return original_stream(
            driver,
            final_argv,
            **{**kwargs, "popen_wrapper": observed_wrapper},
        )

    def observed_kill(proc):
        pid = getattr(proc, "pid", None)
        if type(pid) is int:
            killed_pids.append(pid)
        return original_kill(proc)

    def fail_outer_launch_commit() -> None:
        raise RuntimeError("injected outer C6 launch-fence commit fault")

    monkeypatch.setattr(c6_transport, "_AUDITED_RUN_CLI_STREAMING", observed_stream)
    monkeypatch.setattr(c6_transport, "_kill_process", observed_kill)
    monkeypatch.setattr(
        store,
        "_commit_c6_host_launch_fence_locked",
        fail_outer_launch_commit,
    )

    with pytest.raises(RuntimeError, match="injected outer C6 launch-fence commit fault"):
        await broker.runner.run(
            prompt=prompt,
            final_argv=list(argv),
            cwd=str(tmp_path),
            timeout=10,
            runtime_env={},
        )

    assert len(children) == 1
    assert killed_pids == [children[0].pid]

    def child_exited() -> bool:
        deadline = time.monotonic() + 2
        while time.monotonic() < deadline:
            if children[0].poll() is not None:
                return True
            time.sleep(0.01)
        return children[0].poll() is not None

    assert await asyncio.to_thread(child_exited)
    assert not store.event_rows(kind="CONTEXT_PROMPT_RELEASED")
    assert len(store.event_rows(kind="CONTEXT_PROMPT_UNKNOWN")) == 1
    store.verify()


@pytest.mark.asyncio
async def test_rolled_back_nested_unknown_is_rewritten_after_outer_commit_fault(
    tmp_path, monkeypatch
):
    """Speculative UNKNOWN flags cannot suppress closure after outer rollback.

    First, the RELEASED savepoint faults after event insertion; the broker writes
    UNKNOWN in a second nested savepoint.  Then the outer launch commit faults,
    rolling both nested terminals back.  The rollback callback must clear the
    speculative in-memory UNKNOWN marker so the broker writes one fresh UNKNOWN
    after the fence has unwound.
    """

    (
        store,
        _admission,
        _authority,
        _permit,
        _interlock,
        broker,
        prompt,
        argv,
    ) = _armed_c6_host_broker(tmp_path)
    original_commit = store.commit_command

    def fail_release_after_events(**kwargs):
        if str(kwargs.get("command_id", "")).startswith("context:release:"):
            def injected_fault(phase: str) -> None:
                if phase == "after_events":
                    raise RuntimeError("injected nested release fault")

            kwargs = {**kwargs, "fault_hook": injected_fault}
        return original_commit(**kwargs)

    def fail_outer_launch_commit() -> None:
        raise RuntimeError("injected outer launch commit fault after nested unknown")

    monkeypatch.setattr(store, "commit_command", fail_release_after_events)
    monkeypatch.setattr(
        store,
        "_commit_c6_host_launch_fence_locked",
        fail_outer_launch_commit,
    )

    with pytest.raises(
        RuntimeError, match="injected outer launch commit fault after nested unknown"
    ):
        await broker.runner.run(
            prompt=prompt,
            final_argv=list(argv),
            cwd=str(tmp_path),
            timeout=10,
            runtime_env={},
        )

    assert not store.event_rows(kind="CONTEXT_PROMPT_RELEASED")
    assert not store.event_rows(kind="CONTEXT_PROMPT_PRELAUNCH_ABORTED")
    assert len(store.event_rows(kind="CONTEXT_PROMPT_UNKNOWN")) == 1
    store.verify()


@pytest.mark.asyncio
@pytest.mark.parametrize("failure_kind", ("validator", "popen"))
async def test_rolled_back_prelaunch_terminal_is_rewritten_after_outer_commit_fault(
    tmp_path, monkeypatch, failure_kind
):
    """PRELAUNCH_ABORTED is speculative too until its outer fence commits."""

    (
        store,
        _admission,
        authority,
        _permit,
        _interlock,
        broker,
        prompt,
        argv,
    ) = _armed_c6_host_broker(tmp_path)

    if failure_kind == "validator":
        def reject_live_claim(**_kwargs):
            raise IntegrityError("injected pre-Popen claim rejection")

        monkeypatch.setattr(
            authority,
            "_assert_durable_host_launch_claim_live",
            reject_live_claim,
        )
    else:
        from muteki.runtime import c6_transport

        def popen_raises(_driver, _argv, **kwargs):
            def raise_before_handle():
                raise OSError("injected Popen failure before a handle")

            return kwargs["popen_wrapper"](raise_before_handle)

        monkeypatch.setattr(c6_transport, "_AUDITED_RUN_CLI_STREAMING", popen_raises)

    def fail_outer_launch_commit() -> None:
        raise RuntimeError("injected outer commit fault after prelaunch terminal")

    monkeypatch.setattr(
        store,
        "_commit_c6_host_launch_fence_locked",
        fail_outer_launch_commit,
    )

    with pytest.raises(
        RuntimeError, match="injected outer commit fault after prelaunch terminal"
    ):
        await broker.runner.run(
            prompt=prompt,
            final_argv=list(argv),
            cwd=str(tmp_path),
            timeout=10,
            runtime_env={},
        )

    assert not store.event_rows(kind="CONTEXT_PROMPT_RELEASED")
    assert not store.event_rows(kind="CONTEXT_PROMPT_UNKNOWN")
    assert len(store.event_rows(kind="CONTEXT_PROMPT_PRELAUNCH_ABORTED")) == 1
    store.verify()


@pytest.mark.asyncio
@pytest.mark.parametrize("closure", ("settle", "unknown"))
async def test_budget_owner_closure_cannot_overtake_final_c6_popen_check(
    tmp_path, monkeypatch, closure
):
    """The exact post-validator/pre-Popen budget race is rejected atomically."""

    (
        store,
        admission,
        authority,
        _permit,
        _interlock,
        broker,
        prompt,
        argv,
    ) = _armed_c6_host_broker(tmp_path)
    original_validator = authority._assert_durable_host_launch_claim_live
    closure_errors: list[BaseException] = []

    def validator_then_try_budget_close(*args, **kwargs):
        receipt = original_validator(*args, **kwargs)
        try:
            if closure == "settle":
                admission.settle(
                    attempt_id="attempt-1",
                    actual_usage={"attempts": 1, "tokens": 1, "wall_ms": 1},
                    settlement_revision=1,
                    occurred_at_ns=30,
                )
            else:
                admission.hold_unknown_usage(
                    attempt_id="attempt-1",
                    revision=1,
                    occurred_at_ns=30,
                )
        except BaseException as exc:
            closure_errors.append(exc)
        return receipt

    monkeypatch.setattr(
        authority,
        "_assert_durable_host_launch_claim_live",
        validator_then_try_budget_close,
    )
    await broker.runner.run(
        prompt=prompt,
        final_argv=list(argv),
        cwd=str(tmp_path),
        timeout=10,
        runtime_env={},
    )

    assert len(closure_errors) == 1
    assert isinstance(closure_errors[0], IntegrityError)
    assert "C6 host launch fence permits only prompt terminals" in str(
        closure_errors[0]
    )
    assert len(store.event_rows(kind="CONTEXT_PROMPT_RELEASED")) == 1
    assert not store.event_rows(kind="BUDGET_SETTLED")
    assert not store.event_rows(kind="BUDGET_USAGE_UNKNOWN")
    assert (
        store._conn.execute(
            "SELECT state FROM runtime_attempts WHERE attempt_id='attempt-1'"
        ).fetchone()[0]
        == "running"
    )


@pytest.mark.asyncio
async def test_pessimistic_budget_path_cannot_overtake_final_c6_popen_check(
    tmp_path, monkeypatch
):
    """The v2-only pessimistic terminal mutation shares the same C6 fence."""

    (
        store,
        _admission,
        authority,
        _permit,
        _interlock,
        broker,
        prompt,
        argv,
    ) = _armed_c6_host_broker(tmp_path)
    original_validator = authority._assert_durable_host_launch_claim_live
    closure_errors: list[BaseException] = []

    def validator_then_try_pessimistic_close(*args, **kwargs):
        receipt = original_validator(*args, **kwargs)
        mutation = ProjectionMutation(
            "budget_pessimistic_settle",
            {
                "attempt_id": "attempt-1",
                "charge_basis": "unobserved_reservation_ceiling",
                "charged_usage": {
                    "attempts": 1,
                    "tokens": 100,
                    "wall_ms": 10_000,
                },
                "reservation_ids": [],
                "settlement_revision": 1,
                "usage_report": {},
                "usage_report_digest": "a" * 64,
            },
        )
        try:
            with store._lock:
                store._apply_projection_mutation(mutation)
        except BaseException as exc:
            closure_errors.append(exc)
        return receipt

    monkeypatch.setattr(
        authority,
        "_assert_durable_host_launch_claim_live",
        validator_then_try_pessimistic_close,
    )
    await broker.runner.run(
        prompt=prompt,
        final_argv=list(argv),
        cwd=str(tmp_path),
        timeout=10,
        runtime_env={},
    )

    assert len(closure_errors) == 1
    assert isinstance(closure_errors[0], IntegrityError)
    assert "cannot overtake an unresolved C6 host claim" in str(closure_errors[0])
    assert len(store.event_rows(kind="CONTEXT_PROMPT_RELEASED")) == 1
    assert not store.event_rows(kind="BUDGET_PESSIMISTICALLY_SETTLED")
    assert (
        store._conn.execute(
            "SELECT state FROM runtime_attempts WHERE attempt_id='attempt-1'"
        ).fetchone()[0]
        == "running"
    )


def test_scope_deactivation_refuses_an_unresolved_c6_launch_claim(tmp_path):
    """Pause/stop cannot slip between the final live check and local Popen."""

    store, admission, cas = _runtime(tmp_path)
    attempt, lease = _attempt()
    authority = CognitiveContextAuthority(store=store, cas=cas)
    delivered = authority.compile_for_attempt(
        attempt=attempt,
        context=_context(),
        feature_gate=CognitiveFeatureGateV1(),
        occurred_at_ns=10,
    )
    permit = _admit(admission, attempt, lease, delivered)
    CanonicalPermitResolver(store=store, scope=attempt.scope).claim_launch(
        permit, now_ns=21
    )
    prompt = "runtime header\n" + delivered.render_for_prompt()
    staged = authority.stage_prompt(
        delivered=delivered,
        permit=permit,
        prompt=prompt,
        transport="argv",
        occurred_at_ns=22,
    )
    argv = ("synthetic-cli", "--prompt", prompt)
    invocation = authority.bind_prompt_invocation(
        delivered=delivered,
        permit=permit,
        staged=staged,
        argv=argv,
        occurred_at_ns=23,
        require_fresh=True,
    )
    claim, _profile = _claim_c6_launch(
        authority=authority,
        delivered=delivered,
        permit=permit,
        staged=staged,
        invocation=invocation,
        argv=argv,
        cwd=str(tmp_path),
        occurred_at_ns=24,
    )
    scope_payload = {"scope_digest": attempt.scope.digest}

    with pytest.raises(IntegrityError, match="scope transition cannot overtake"):
        store.commit_command(
            command_id="pause-with-c6-claim",
            idempotency_key="pause-with-c6-claim",
            command_payload={},
            events=(CommandEvent("event:pause-with-c6-claim", "SEARCH_PAUSED", "test", 25),),
            committed_at_ns=25,
        )
    with pytest.raises(IntegrityError, match="scope transition cannot overtake"):
        store.commit_command(
            command_id="stop-with-c6-claim",
            idempotency_key="stop-with-c6-claim",
            command_payload=scope_payload,
            events=(
                CommandEvent(
                    "event:stop-with-c6-claim",
                    "EXECUTION_STOP_REQUESTED",
                    "test",
                    26,
                    scope_payload,
                ),
            ),
            projection_mutations=(
                ProjectionMutation("execution_stop_guard", scope_payload),
            ),
            authority_capability=store._lifecycle_commit_capability,
            committed_at_ns=26,
        )
    boot_payload = {"boot_epoch": 2, "writer_epoch": 2}
    with pytest.raises(IntegrityError, match="scope transition cannot overtake"):
        store.commit_command(
            command_id="verify-with-c6-claim",
            idempotency_key="verify-with-c6-claim",
            command_payload=boot_payload,
            events=(
                CommandEvent(
                    "event:verify-with-c6-claim",
                    "BOOT_VERIFYING",
                    "test",
                    27,
                    boot_payload,
                ),
            ),
            committed_at_ns=27,
        )
    assert store.state().run_execution.value == "running"
    assert store.state().search_mode.value == "active"

    authority.record_prompt_prelaunch_aborted(
        delivered=delivered,
        permit=permit,
        claim=claim,
        reason="supervisor revoked before scope deactivation",
        occurred_at_ns=27,
    )
    admission.settle(
        attempt_id="attempt-1",
        actual_usage={"attempts": 0, "tokens": 0, "wall_ms": 0},
        settlement_revision=1,
        occurred_at_ns=28,
    )
    assert len(store.event_rows(kind="BUDGET_SETTLED")) == 1
    store.commit_command(
        command_id="verify-after-c6-abort",
        idempotency_key="verify-after-c6-abort",
        command_payload=boot_payload,
        events=(
            CommandEvent(
                "event:verify-after-c6-abort",
                "BOOT_VERIFYING",
                "test",
                29,
                boot_payload,
            ),
        ),
        committed_at_ns=29,
    )
    assert store.state().kernel_health.value == "verifying"
    store.commit_command(
        command_id="pause-after-c6-abort",
        idempotency_key="pause-after-c6-abort",
        command_payload={},
        events=(CommandEvent("event:pause-after-c6-abort", "SEARCH_PAUSED", "test", 30),),
        committed_at_ns=30,
    )
    store.commit_command(
        command_id="stop-after-c6-abort",
        idempotency_key="stop-after-c6-abort",
        command_payload=scope_payload,
        events=(
            CommandEvent(
                    "event:stop-after-c6-abort",
                    "EXECUTION_STOP_REQUESTED",
                    "test",
                    31,
                scope_payload,
            ),
        ),
        projection_mutations=(ProjectionMutation("execution_stop_guard", scope_payload),),
        authority_capability=store._lifecycle_commit_capability,
        committed_at_ns=31,
    )
    assert store.state().run_execution.value == "quiescing"


class _Artifacts:
    def read_text(self, _artifact_id: str) -> str:
        return ""


class _Cost:
    def begin_usage_window(self, attempt_id: str) -> str:
        return attempt_id

    def finish_usage_window(self, attempt_id: str, token: str) -> dict[str, int]:
        assert attempt_id == token
        return {"calls": 1, "cost_micro_usd": 1, "tokens": 1}


@dataclass
class _Driver:
    name: str = "synthetic"

    def build_execute(self, prompt, _session, *, web_access, kb_access, stream):
        del web_access, kb_access, stream
        return [
            sys.executable,
            "-c",
            "import sys; print('READY')",
            prompt,
        ]

    def parse(self, stdout: str, stderr: str) -> CliResult:
        assert not stderr
        return CliResult(text=stdout)


class _LiveChallenge:
    name = "causal-fixture"
    description = "Select one local causal probe."
    goal = "Obtain one verified causal distinction."
    flag_format = r"flag\{[^}]+\}"


class _LiveWorker:
    solver_id = "cognitive-live-worker"
    driver = _Driver()
    challenge = _LiveChallenge()
    cost = _Cost()
    mode = "synthetic"
    intent_id_assigned = ""

    def __init__(self) -> None:
        self.packet = None
        self.invocation_runner = None

    def bind_cognitive_context(self, packet, *, invocation_runner=None) -> None:
        self.packet = packet
        self.invocation_runner = invocation_runner


@pytest.mark.asyncio
async def test_live_c6_rejects_generic_worker_before_packet_compilation(tmp_path):
    run_id = "run-cognitive-live"
    catalog = RunCatalog.create(root=tmp_path / "catalog")
    catalog.create_draft(
        draft_id="draft-cognitive-live",
        policy={"offline": True, "protocol": 2},
        occurred_at_ns=1,
    )
    catalog.begin_provision(
        operation_id="provision-cognitive-live",
        draft_id="draft-cognitive-live",
        run_id=run_id,
        target_root=tmp_path / "run",
        manifest_digest=canonical_digest({"run": run_id}),
        owner_epoch=1,
        occurred_at_ns=2,
    )
    catalog.materialize(operation_id="provision-cognitive-live", occurred_at_ns=3)
    factory = HostRunFactory(catalog=catalog, artifacts=_Artifacts())
    _run_context, ports = factory.open(
        run_id=run_id,
        boot_capability=BootRecoveryCapability(1, 1, "cognitive-live"),
        occurred_at_ns=4,
    )
    scope, supervisor = factory.start_execution(
        ports=ports, idempotency_key="start-cognitive-live", occurred_at_ns=5
    )
    budget = {
        "attempts": 1,
        "cost_micro_usd": 10,
        "tokens": 10,
        "wall_ms": 1_000,
        "worker_ms": 1_000,
    }
    ports.admission.create_branch(branch_id="root", max_attempts=1, occurred_at_ns=6)
    ports.admission.create_budget_account(
        account_id="run", limits=budget, occurred_at_ns=7
    )
    session = Protocol2RunSession(
        ports=ports,
        scope=scope,
        supervisor=supervisor,
        policy_digest="c" * 64,
        budget_account_id="run",
        per_attempt_budget=budget,
        max_barren_attempts=2,
        expected_goal_units=1,
        cognitive_feature_gate=CognitiveFeatureGateV1(),
    )
    worker = _LiveWorker()

    async def work():
        raise AssertionError("generic worker must never reach a C6 work coroutine")

    with pytest.raises(Protocol2LiveRejected, match="audited CliSolver"):
        session.schedule_worker(worker, work, name="cognitive-live")
    assert not ports.store.event_rows(kind="RUNTIME_CONTEXT_DECISION_REGISTERED")
    assert not ports.store.event_rows(kind="CONTEXT_PACKET_COMPILED")
    assert not ports.store.event_rows(kind="ATTEMPT_ADMITTED")


@pytest.mark.asyncio
async def test_live_c6_uses_host_popen_and_seals_final_runtime_argv(
    tmp_path, monkeypatch
):
    run_id = "run-cognitive-cli-live"
    catalog = RunCatalog.create(root=tmp_path / "catalog")
    catalog.create_draft(
        draft_id="draft-cognitive-cli-live",
        policy={"offline": True, "protocol": 2},
        occurred_at_ns=1,
    )
    catalog.begin_provision(
        operation_id="provision-cognitive-cli-live",
        draft_id="draft-cognitive-cli-live",
        run_id=run_id,
        target_root=tmp_path / "run",
        manifest_digest=canonical_digest({"run": run_id}),
        owner_epoch=1,
        occurred_at_ns=2,
    )
    catalog.materialize(operation_id="provision-cognitive-cli-live", occurred_at_ns=3)
    factory = HostRunFactory(catalog=catalog, artifacts=_Artifacts())
    _context, ports = factory.open(
        run_id=run_id,
        boot_capability=BootRecoveryCapability(1, 1, "cognitive-cli-live"),
        occurred_at_ns=4,
    )
    scope, supervisor = factory.start_execution(
        ports=ports, idempotency_key="start-cognitive-cli-live", occurred_at_ns=5
    )
    budget = {
        "attempts": 1,
        "cost_micro_usd": 10,
        "tokens": 10,
        "wall_ms": 1_000,
        "worker_ms": 1_000,
    }
    ports.admission.create_branch(branch_id="root", max_attempts=1, occurred_at_ns=6)
    ports.admission.create_budget_account(
        account_id="run", limits=budget, occurred_at_ns=7
    )
    session = Protocol2RunSession(
        ports=ports,
        scope=scope,
        supervisor=supervisor,
        policy_digest="c" * 64,
        budget_account_id="run",
        per_attempt_budget=budget,
        max_barren_attempts=2,
        expected_goal_units=1,
        cognitive_feature_gate=CognitiveFeatureGateV1(),
    )
    challenge = Challenge(
        id=run_id,
        name="synthetic",
        category="misc",
        description="Choose one local causal probe.",
        flag_format=r"flag\{[^}]+\}",
    )
    worker = CliSolver(
        None,
        challenge,
        driver=_Driver(),
        cost=_Cost(),
        engine="claude",
        kb=False,
        web_access=False,
        worker_env={"MUTEKI_WORKER_MODEL": "synthetic-model"},
    )
    from muteki.runtime import c6_transport

    host_calls: list[list[str]] = []
    worker_calls: list[object] = []
    original_host_stream = c6_transport._AUDITED_RUN_CLI_STREAMING

    def observed_host_stream(driver, argv, **kwargs):
        host_calls.append(list(argv))
        return original_host_stream(driver, argv, **kwargs)

    async def poisoned_worker_stream(*_args, **_kwargs):
        worker_calls.append(object())
        raise AssertionError("C6 must not use a worker-owned streaming adapter")

    monkeypatch.setattr(c6_transport, "_AUDITED_RUN_CLI_STREAMING", observed_host_stream)
    worker._run_streaming = poisoned_worker_stream

    async def work():
        prompt = "runtime header\n" + worker._cognitive_context_block()
        argv, stdin_text = worker._execute_invocation(prompt, None)
        assert stdin_text is None
        await worker._run_invocation(
            argv, cwd=str(tmp_path), timeout=10, stdin_text=stdin_text
        )
        retry_prompt = "retry header\n" + worker._cognitive_context_block()
        retry_argv, retry_stdin = worker._execute_invocation(retry_prompt, None)
        with pytest.raises(C6TransportRejected, match="at most one invocation"):
            await worker._run_invocation(
                retry_argv, cwd=str(tmp_path), timeout=10, stdin_text=retry_stdin
            )
        return type("Outcome", (), {"solved": False, "flags": []})()

    outcome = await session.schedule_worker(worker, work, name="cognitive-cli-live")
    assert outcome.solved is False
    invocation = ports.store.event_rows(kind="CONTEXT_PROMPT_INVOCATION_BOUND")
    claim = ports.store.event_rows(kind="CONTEXT_PROMPT_LAUNCH_CLAIMED")
    release = ports.store.event_rows(kind="CONTEXT_PROMPT_RELEASED")
    assert len(invocation) == 1
    assert len(claim) == 1
    assert len(release) == 1
    assert len(ports.store.event_rows(kind="CONTEXT_PROMPT_STAGED")) == 1
    sealed_argv = ports.cas.read_verified(
        invocation[0]["payload"]["argv_artifact_digest"]
    )
    expected_argv = json.loads(sealed_argv)
    assert host_calls == [expected_argv]
    assert not worker_calls
    model_index = expected_argv.index("--model")
    assert expected_argv[model_index + 1] == "synthetic-model"
    assert release[0]["payload"]["process_id"] > 0
    assert release[0]["payload"]["claim_id"] == claim[0]["payload"]["claim_id"]
    material = json.loads(
        ports.cas.read_verified(claim[0]["payload"]["launch_material_digest"])
    )
    assert material["argv_artifact_digest"] == invocation[0]["payload"]["argv_artifact_digest"]
    assert material["profile_digest"] == claim[0]["payload"]["profile_digest"]
    kinds = [row["kind"] for row in ports.store.event_rows()]
    assert kinds.index("RUNTIME_CONTEXT_DECISION_REGISTERED") < kinds.index(
        "CONTEXT_PACKET_COMPILED"
    ) < kinds.index("ATTEMPT_ADMITTED") < kinds.index(
        "WORKER_LAUNCH_PREPARED"
    ) < kinds.index("CONTEXT_PROMPT_STAGED") < kinds.index(
        "CONTEXT_PROMPT_INVOCATION_BOUND"
    ) < kinds.index("CONTEXT_PROMPT_LAUNCH_CLAIMED") < kinds.index(
        "CONTEXT_PROMPT_RELEASED"
    )
    assert not ports.store.event_rows(kind="CONTEXT_PROMPT_UNKNOWN")
    receipts = await session.finalize(solved=False)
    assert len(receipts["context_prompt_closure"]) == 64


@pytest.mark.asyncio
async def test_boot_recovery_marks_dangling_c6_claim_unknown_once(tmp_path):
    run_id = "run-cognitive-recovery"
    catalog = RunCatalog.create(root=tmp_path / "catalog")
    catalog.create_draft(
        draft_id="draft-cognitive-recovery",
        policy={"offline": True, "protocol": 2},
        occurred_at_ns=1,
    )
    catalog.begin_provision(
        operation_id="provision-cognitive-recovery",
        draft_id="draft-cognitive-recovery",
        run_id=run_id,
        target_root=tmp_path / "run",
        manifest_digest=canonical_digest({"run": run_id}),
        owner_epoch=1,
        occurred_at_ns=2,
    )
    catalog.materialize(operation_id="provision-cognitive-recovery", occurred_at_ns=3)
    factory = HostRunFactory(catalog=catalog, artifacts=_Artifacts())
    _run_context, ports = factory.open(
        run_id=run_id,
        boot_capability=BootRecoveryCapability(1, 1, "cognitive-recovery-a"),
        occurred_at_ns=4,
    )
    scope, supervisor = factory.start_execution(
        ports=ports, idempotency_key="start-cognitive-recovery", occurred_at_ns=5
    )
    ports.admission.create_branch(branch_id="root", max_attempts=1, occurred_at_ns=6)
    ports.admission.create_budget_account(
        account_id="run",
        limits={"attempts": 1, "tokens": 100, "wall_ms": 10_000},
        occurred_at_ns=7,
    )
    attempt = AttemptIdentity(scope, "root", "attempt-recovery", 1)
    lease = LeaseIdentity(attempt, "lease-recovery", 1, 1)
    delivered = ports.cognition.compile_for_attempt(
        attempt=attempt,
        context=_context(),
        feature_gate=CognitiveFeatureGateV1(),
        occurred_at_ns=8,
    )
    permit = _admit(ports.admission, attempt, lease, delivered)

    async def crash_after_claim():
        prompt = "runtime header\n" + delivered.render_for_prompt()
        staged = ports.cognition.stage_prompt(
            delivered=delivered,
            permit=permit,
            prompt=prompt,
            transport="argv",
            occurred_at_ns=21,
        )
        argv = ("synthetic-cli", "--", prompt)
        invocation = ports.cognition.bind_prompt_invocation(
            delivered=delivered,
            permit=permit,
            staged=staged,
            argv=argv,
            occurred_at_ns=22,
        )
        _claim_c6_launch(
            authority=ports.cognition,
            delivered=delivered,
            permit=permit,
            staged=staged,
            invocation=invocation,
            argv=argv,
            cwd=str(tmp_path),
            occurred_at_ns=23,
        )
        raise RuntimeError("simulated process crash after durable C6 claim")

    task = supervisor.spawn_owned(permit, crash_after_claim, now_ns=20)
    with pytest.raises(IntegrityError, match="unresolved C6 host claim"):
        await task
    assert len(ports.store.event_rows(kind="CONTEXT_PROMPT_INVOCATION_BOUND")) == 1
    assert len(ports.store.event_rows(kind="CONTEXT_PROMPT_LAUNCH_CLAIMED")) == 1
    assert not ports.store.event_rows(kind="CONTEXT_PROMPT_UNKNOWN")
    ports.store.close()

    # UNKNOWN keeps the attempt/budget owner held, so ordinary boot must remain
    # closed.  The important C6 property is that the boot-finalize sweep recorded
    # one canonical UNKNOWN before the host rejected reopening the owner.
    with pytest.raises(RuntimeError, match="unresolved attempt/effect/budget owners"):
        factory.open(
            run_id=run_id,
            boot_capability=BootRecoveryCapability(2, 2, "cognitive-recovery-b"),
            occurred_at_ns=30,
        )
    raw = EpistemicSQLiteStore.open(tmp_path / "run" / "epistemic-v2.db")
    unknown = raw.event_rows(kind="CONTEXT_PROMPT_UNKNOWN")
    verifying = raw.event_rows(kind="BOOT_VERIFYING")
    assert len(unknown) == 1
    assert len(verifying) == 2
    assert unknown[0]["seq"] < verifying[-1]["seq"]
    assert len(raw.event_rows(kind="CONTEXT_PROMPT_LAUNCH_CLAIMED")) == 1
    assert not raw.event_rows(kind="CONTEXT_PROMPT_RELEASED")
    replay_authority = CognitiveContextAuthority(
        store=raw,
        cas=ReceiptCAS(tmp_path / "run" / "receipt-cas"),
    )
    replay_profile = C6HostLaunchProfileV1(driver_name="synthetic")
    replay_broker = C6HostLaunchBroker(
        authority=replay_authority,
        delivered=delivered,
        profile=replay_profile,
        host_adapter=C6HostPopenAdapter(
            authority=replay_authority,
            delivered=delivered,
            driver=_Driver(),
            profile=replay_profile,
            interlock=C6HostLaunchInterlock(),
        ),
    )
    with pytest.raises(C6TransportRejected, match="redispatch is forbidden"):
        replay_broker.activate(permit=permit)
    first_unknown_digest = unknown[0]["event_digest"]
    raw.close()

    with pytest.raises(RuntimeError, match="unresolved attempt/effect/budget owners"):
        factory.open(
            run_id=run_id,
            boot_capability=BootRecoveryCapability(3, 3, "cognitive-recovery-c"),
            occurred_at_ns=40,
        )
    raw_again = EpistemicSQLiteStore.open(tmp_path / "run" / "epistemic-v2.db")
    unknown_again = raw_again.event_rows(kind="CONTEXT_PROMPT_UNKNOWN")
    assert [row["event_digest"] for row in unknown_again] == [first_unknown_digest]
    raw_again.close()
