from __future__ import annotations

from dataclasses import replace

import pytest

from muteki.epistemic.contracts import canonical_digest
from muteki.eval.aggregations import aggregate_study
from muteki.eval.manifests import (
    EvalStudyManifest,
    TrialAssignment,
    TrialIdentity,
)
from muteki.eval.receipts import (
    EvalTrialReceipt,
    MissingnessVerdict,
    TrialOutcome,
    UsageCounterStream,
)


def _assignment(
    index: int,
    arm: str,
    pair_id: str,
    *,
    challenge_id: str | None = None,
    budget: tuple[tuple[str, int], ...] = (("tokens", 100),),
    policy_digest: str = "a" * 64,
    protocol_version: int = 2,
) -> TrialAssignment:
    identity = TrialIdentity(
        "study-1", f"trial-{index}", f"intent-{index}", protocol_version
    )
    return TrialAssignment(
        identity,
        challenge_id or f"challenge-{index}",
        arm,
        pair_id,
        budget,
        policy_digest,
    )


def _manifest() -> EvalStudyManifest:
    assignments = tuple(
        _assignment(index, arm, f"smoke-{index}")
        for index, arm in enumerate(("control", "candidate", "candidate"), start=1)
    )
    return EvalStudyManifest("study-1", assignments, "b" * 64)


def _paired_manifest() -> EvalStudyManifest:
    assignments = (
        _assignment(1, "control", "pair-1", challenge_id="challenge-1"),
        _assignment(2, "candidate", "pair-1", challenge_id="challenge-1"),
        _assignment(3, "control", "pair-2", challenge_id="challenge-2"),
        _assignment(4, "candidate", "pair-2", challenge_id="challenge-2"),
    )
    return EvalStudyManifest("study-1", assignments, "b" * 64)


def _complete_receipt(
    assignment: TrialAssignment,
    *,
    identity: TrialIdentity | None = None,
    assignment_digest: str | None = None,
    usage: UsageCounterStream | None = None,
) -> EvalTrialReceipt:
    return EvalTrialReceipt(
        identity or assignment.identity,
        assignment_digest=assignment_digest or assignment.digest,
        outcome=TrialOutcome.SOLVED,
        missingness=MissingnessVerdict.COMPLETE,
        provision_receipt_digest="2" * 64,
        launch_receipt_digest="3" * 64,
        provider_receipt_digest="4" * 64,
        trace_digest="5" * 64,
        gate_receipt_digest="6" * 64,
        oracle_receipt_digest="7" * 64,
        policy_receipt_digest="8" * 64,
        usage=usage
        or UsageCounterStream(
            tuple((axis, ceiling - 1) for axis, ceiling in assignment.budget),
            complete=True,
        ),
    )


def test_manifest_pre_enumerates_trials_and_canonically_hashes_assignments():
    manifest = _manifest()
    assert len(manifest.assignments) == 3
    assert len(manifest.digest) == 64
    assert manifest.assignments[0].digest == canonical_digest(
        manifest.assignments[0].as_dict()
    )
    assert manifest.smoke_only is True


def test_ite_denominator_keeps_prelaunch_and_launch_crashes():
    manifest = _manifest()
    a1, a2, a3 = manifest.assignments
    complete = _complete_receipt(a1)
    prelaunch = EvalTrialReceipt(
        a2.identity,
        assignment_digest=a2.digest,
        outcome=TrialOutcome.NOT_STARTED,
        missingness=MissingnessVerdict.POST_PROVISION_PRE_LAUNCH,
        provision_receipt_digest="a" * 64,
    )
    launch_crash = EvalTrialReceipt(
        a3.identity,
        assignment_digest=a3.digest,
        outcome=TrialOutcome.INFRA_FAILURE,
        missingness=MissingnessVerdict.POST_LAUNCH_INCOMPLETE,
        provision_receipt_digest="c" * 64,
        launch_receipt_digest="d" * 64,
    )
    result = aggregate_study(manifest, [complete, prelaunch, launch_crash])
    assert result.intention_count == 3
    assert result.receipt_count == 3
    assert result.strict_valid_count == 1
    assert result.solved_intention_count == 1
    assert dict(result.missingness)["post_provision_pre_launch"] == 1
    assert result.smoke_only is True
    assert result.is_promotion_evidence is False


def test_contracts_reject_non_exact_schema_and_noncanonical_digests():
    assignment = _manifest().assignments[0]
    with pytest.raises(TypeError, match="built-in tuple"):
        EvalStudyManifest("study-1", [assignment], "b" * 64)  # type: ignore[arg-type]
    with pytest.raises(ValueError, match="lowercase sha256"):
        replace(assignment, policy_digest="A" * 64)
    with pytest.raises(ValueError, match="axes must be unique"):
        UsageCounterStream((("tokens", 1), ("tokens", 2)))
    with pytest.raises(ValueError, match="positive integer"):
        replace(assignment.identity, protocol_version=True)
    with pytest.raises(TypeError, match="outcome must be TrialOutcome"):
        replace(_complete_receipt(assignment), outcome="solved")
    with pytest.raises(ValueError, match="unsupported evaluation contract"):
        replace(_complete_receipt(assignment), contract_version=1)


@pytest.mark.parametrize(
    "identity",
    (
        TrialIdentity("other-study", "trial-1", "intent-1", 2),
        TrialIdentity("study-1", "trial-1", "other-intent", 2),
        TrialIdentity("study-1", "trial-1", "intent-1", 1),
    ),
)
def test_aggregation_binds_study_intention_and_protocol_identity(
    identity: TrialIdentity,
):
    manifest = _manifest()
    assignment = manifest.assignments[0]
    with pytest.raises(ValueError, match="identity does not match"):
        aggregate_study(
            manifest,
            [_complete_receipt(assignment, identity=identity)],
        )


def test_aggregation_rejects_rebound_duplicate_and_unknown_receipts():
    manifest = _manifest()
    assignment = manifest.assignments[0]
    receipt = _complete_receipt(assignment)

    with pytest.raises(ValueError, match="assignment_digest"):
        aggregate_study(
            manifest,
            [_complete_receipt(assignment, assignment_digest="f" * 64)],
        )
    with pytest.raises(ValueError, match="duplicate receipt"):
        aggregate_study(manifest, [receipt, receipt])

    unknown_assignment = TrialAssignment(
        TrialIdentity("study-1", "unknown-trial", "unknown-intent", 2),
        "challenge-x",
        "candidate",
        "smoke-x",
        (("tokens", 100),),
        "a" * 64,
    )
    with pytest.raises(ValueError, match="unknown trial_id"):
        aggregate_study(manifest, [_complete_receipt(unknown_assignment)])


def test_aggregation_requires_exact_bounded_usage_axes():
    assignment = _assignment(
        1,
        "candidate",
        "smoke-1",
        budget=(("tokens", 100), ("wall_ms", 1_000)),
    )
    manifest = EvalStudyManifest("study-1", (assignment,), "b" * 64)

    with pytest.raises(ValueError, match="exactly match"):
        aggregate_study(
            manifest,
            [
                _complete_receipt(
                    assignment,
                    usage=UsageCounterStream((("tokens", 80),), complete=True),
                )
            ],
        )
    with pytest.raises(ValueError, match="unknown frozen-budget axes"):
        aggregate_study(
            manifest,
            [
                replace(
                    _complete_receipt(assignment),
                    usage=UsageCounterStream((("requests", 1),)),
                )
            ],
        )
    with pytest.raises(ValueError, match="exceeds frozen budget"):
        aggregate_study(
            manifest,
            [
                _complete_receipt(
                    assignment,
                    usage=UsageCounterStream(
                        (("tokens", 101), ("wall_ms", 900)), complete=True
                    ),
                )
            ],
        )

    partial = replace(
        _complete_receipt(assignment),
        missingness=MissingnessVerdict.RECEIPT_INCOMPLETE,
        usage=UsageCounterStream((("tokens", 80),)),
    )
    result = aggregate_study(manifest, [partial])
    assert result.strict_valid_count == 0


def test_reused_pair_ids_enforce_consistent_two_arm_pairs():
    manifest = _paired_manifest()
    assert manifest.is_paired_two_arm is True
    assert manifest.smoke_only is False
    result = aggregate_study(manifest, [_complete_receipt(manifest.assignments[0])])
    assert result.paired_two_arm is True
    assert result.is_promotion_evidence is False

    assignments = list(manifest.assignments)
    assignments[1] = replace(assignments[1], arm="control")
    with pytest.raises(ValueError, match="one assignment from each arm"):
        EvalStudyManifest("study-1", tuple(assignments), "b" * 64)

    assignments = list(manifest.assignments)
    assignments[1] = replace(assignments[1], challenge_id="different")
    with pytest.raises(ValueError, match="same challenge_id"):
        EvalStudyManifest("study-1", tuple(assignments), "b" * 64)

    for replacement, message in (
        (replace(manifest.assignments[1], budget=(("tokens", 99),)), "frozen budget"),
        (replace(manifest.assignments[1], policy_digest="c" * 64), "policy_digest"),
        (
            replace(
                manifest.assignments[1],
                identity=replace(manifest.assignments[1].identity, protocol_version=1),
            ),
            "protocol_version",
        ),
    ):
        assignments = list(manifest.assignments)
        assignments[1] = replacement
        with pytest.raises(ValueError, match=message):
            EvalStudyManifest("study-1", tuple(assignments), "b" * 64)

    assignments = list(manifest.assignments)
    assignments[-1] = replace(assignments[-1], pair_id="singleton")
    with pytest.raises(ValueError, match="exactly two assignments per pair_id"):
        EvalStudyManifest("study-1", tuple(assignments), "b" * 64)
