from __future__ import annotations

import hashlib

import pytest

from muteki.runtime.prompt_stage import (
    PromptAssemblyV1,
    PromptInvocationBindingV1,
    PromptStageStateV1,
    PromptStageStatus,
    StagedPromptV1,
)


def _digest(value: str) -> str:
    return hashlib.sha256(value.encode("utf-8")).hexdigest()


def _assembly(*, transport: str = "argv") -> PromptAssemblyV1:
    return PromptAssemblyV1.materialize(
        packet_digest=_digest("packet"),
        context_block="## Sealed decision context\npacket\n",
        full_prompt="system header\n## Sealed decision context\npacket\nuser body\n",
        transport=transport,
    )


def test_materialized_assembly_binds_exact_context_once_and_is_deterministic() -> None:
    first = _assembly()
    second = _assembly()
    assert first == second
    assert first.digest == second.digest
    assert first.full_prompt_byte_count > 0


@pytest.mark.parametrize(
    "full_prompt",
    (
        "missing block",
        "## Sealed decision context\npacket\n## Sealed decision context\npacket\n",
    ),
)
def test_stage_rejects_missing_or_duplicate_context_block(full_prompt: str) -> None:
    with pytest.raises(ValueError, match="exact context block once"):
        PromptAssemblyV1.materialize(
            packet_digest=_digest("packet"),
            context_block="## Sealed decision context\npacket\n",
            full_prompt=full_prompt,
            transport="argv",
        )


def test_stage_identity_binds_attempt_permit_and_assembly() -> None:
    assembly = _assembly()
    stage = StagedPromptV1.create(
        attempt_digest=_digest("attempt"),
        permit_digest=_digest("permit"),
        assembly=assembly,
    )
    assert stage == StagedPromptV1.create(
        attempt_digest=_digest("attempt"),
        permit_digest=_digest("permit"),
        assembly=assembly,
    )
    with pytest.raises(ValueError, match="does not bind"):
        StagedPromptV1(
            attempt_digest=_digest("attempt"),
            permit_digest=_digest("permit"),
            assembly=assembly,
            stage_id="stage-wrong",
        )


def test_unknown_is_terminal_and_never_allows_automatic_redispatch() -> None:
    stage = StagedPromptV1.create(
        attempt_digest=_digest("attempt"),
        permit_digest=_digest("permit"),
        assembly=_assembly(),
    )
    state = PromptStageStateV1(staged=stage)
    unknown = state.unknown(receipt_digest=_digest("unknown"))
    assert unknown.status is PromptStageStatus.UNKNOWN
    assert not unknown.automatic_redispatch_permitted
    with pytest.raises(ValueError, match="only a staged"):
        unknown.release(receipt_digest=_digest("late"))


def test_release_and_unknown_are_mutually_exclusive() -> None:
    stage = StagedPromptV1.create(
        attempt_digest=_digest("attempt"),
        permit_digest=_digest("permit"),
        assembly=_assembly(),
    )
    released = PromptStageStateV1(staged=stage).release(
        receipt_digest=_digest("released")
    )
    assert released.status is PromptStageStatus.RELEASED
    with pytest.raises(ValueError, match="only a staged"):
        released.unknown(receipt_digest=_digest("late"))


def test_only_declared_transports_are_allowed() -> None:
    with pytest.raises(ValueError, match="argv or stdin"):
        _assembly(transport="pipe")


def test_invocation_binding_seals_one_exact_staged_prompt_argument() -> None:
    stage = StagedPromptV1.create(
        attempt_digest=_digest("attempt"),
        permit_digest=_digest("permit"),
        assembly=_assembly(),
    )
    prompt = "system header\n## Sealed decision context\npacket\nuser body\n"
    binding, raw = PromptInvocationBindingV1.bind_argv(
        staged=stage, argv=("cli", "--", prompt)
    )
    repeated, repeated_raw = PromptInvocationBindingV1.bind_argv(
        staged=stage, argv=["cli", "--", prompt]
    )

    assert binding == repeated
    assert raw == repeated_raw
    assert binding.prompt_argument_count == 1
    assert binding.argv_artifact_digest == hashlib.sha256(raw).hexdigest()


def test_invocation_binding_rejects_missing_or_duplicate_staged_prompt() -> None:
    stage = StagedPromptV1.create(
        attempt_digest=_digest("attempt"),
        permit_digest=_digest("permit"),
        assembly=_assembly(),
    )
    prompt = "system header\n## Sealed decision context\npacket\nuser body\n"
    with pytest.raises(ValueError, match="exactly once"):
        PromptInvocationBindingV1.bind_argv(staged=stage, argv=("cli", "--", "other"))
    with pytest.raises(ValueError, match="exactly once"):
        PromptInvocationBindingV1.bind_argv(
            staged=stage, argv=("cli", "--", prompt, prompt)
        )
