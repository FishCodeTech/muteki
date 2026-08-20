"""Pure C6 prompt-assembly and staging contracts.

The contracts in this module intentionally have no runtime authority.  They do
not launch a process, write a receipt, open a provider connection, or decide a
retry.  A host authority may use them to prove a much narrower fact than
"delivery": an exact prompt containing one sealed ContextPacket block was staged
before release to a worker transport.

That distinction matters.  A post-``Popen`` or post-pipe-write callback cannot
prove that no CLI/provider effect happened earlier.  The only terminal states
available here are therefore release observed and UNKNOWN; UNKNOWN never becomes
an automatic redispatch instruction.
"""

from __future__ import annotations

import hashlib
from dataclasses import dataclass
from enum import Enum

from muteki.epistemic.contracts import canonical_digest, canonical_json_bytes


PROMPT_STAGE_VERSION = "muteki.runtime-prompt-stage.v1"
_TRANSPORTS = frozenset({"argv", "stdin"})


def _text(value: object, name: str) -> str:
    if type(value) is not str or not value or value != value.strip():
        raise ValueError(f"{name} must be exact non-empty text")
    return value


def _digest(value: object, name: str) -> str:
    result = _text(value, name)
    if len(result) != 64 or any(character not in "0123456789abcdef" for character in result):
        raise ValueError(f"{name} must be a lowercase sha256 digest")
    return result


def _transport(value: object) -> str:
    result = _text(value, "transport")
    if result not in _TRANSPORTS:
        raise ValueError("transport must be argv or stdin")
    return result


@dataclass(frozen=True, slots=True)
class PromptAssemblyV1:
    """A checked, lossless prompt assembly before a worker is released."""

    packet_digest: str
    context_block_digest: str
    full_prompt_digest: str
    full_prompt_byte_count: int
    transport: str
    template_version: str = PROMPT_STAGE_VERSION

    def __post_init__(self) -> None:
        for name in (
            "packet_digest",
            "context_block_digest",
            "full_prompt_digest",
        ):
            object.__setattr__(self, name, _digest(getattr(self, name), name))
        if type(self.full_prompt_byte_count) is not int or self.full_prompt_byte_count <= 0:
            raise ValueError("full_prompt_byte_count must be a positive exact integer")
        object.__setattr__(self, "transport", _transport(self.transport))
        object.__setattr__(
            self,
            "template_version",
            _text(self.template_version, "template_version"),
        )
        if self.template_version != PROMPT_STAGE_VERSION:
            raise ValueError("unsupported prompt-stage template version")

    @classmethod
    def materialize(
        cls,
        *,
        packet_digest: str,
        context_block: str,
        full_prompt: str,
        transport: str,
    ) -> "PromptAssemblyV1":
        packet = _digest(packet_digest, "packet_digest")
        if type(context_block) is not str or not context_block:
            raise ValueError("context_block must be non-empty text")
        if type(full_prompt) is not str or not full_prompt:
            raise ValueError("full_prompt must be non-empty text")
        if full_prompt.count(context_block) != 1:
            raise ValueError("full_prompt must include the exact context block once")
        encoded = full_prompt.encode("utf-8")
        return cls(
            packet_digest=packet,
            context_block_digest=hashlib.sha256(context_block.encode("utf-8")).hexdigest(),
            full_prompt_digest=hashlib.sha256(encoded).hexdigest(),
            full_prompt_byte_count=len(encoded),
            transport=_transport(transport),
        )

    def canonical_body(self) -> dict[str, object]:
        return {
            "context_block_digest": self.context_block_digest,
            "full_prompt_byte_count": self.full_prompt_byte_count,
            "full_prompt_digest": self.full_prompt_digest,
            "packet_digest": self.packet_digest,
            "template_version": self.template_version,
            "transport": self.transport,
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())


@dataclass(frozen=True, slots=True)
class StagedPromptV1:
    """A deterministic stage identity; an exact retry resolves to the same id."""

    attempt_digest: str
    permit_digest: str
    assembly: PromptAssemblyV1
    stage_id: str

    def __post_init__(self) -> None:
        object.__setattr__(self, "attempt_digest", _digest(self.attempt_digest, "attempt_digest"))
        object.__setattr__(self, "permit_digest", _digest(self.permit_digest, "permit_digest"))
        if type(self.assembly) is not PromptAssemblyV1:
            raise TypeError("assembly must be PromptAssemblyV1")
        expected = self.expected_stage_id(
            attempt_digest=self.attempt_digest,
            permit_digest=self.permit_digest,
            assembly=self.assembly,
        )
        if self.stage_id != expected:
            raise ValueError("stage_id does not bind attempt, permit, and prompt assembly")

    @staticmethod
    def expected_stage_id(
        *,
        attempt_digest: str,
        permit_digest: str,
        assembly: PromptAssemblyV1,
    ) -> str:
        return "stage-" + canonical_digest(
            {
                "assembly_digest": assembly.digest,
                "attempt_digest": _digest(attempt_digest, "attempt_digest"),
                "permit_digest": _digest(permit_digest, "permit_digest"),
                "version": PROMPT_STAGE_VERSION,
            }
        )[:40]

    @classmethod
    def create(
        cls,
        *,
        attempt_digest: str,
        permit_digest: str,
        assembly: PromptAssemblyV1,
    ) -> "StagedPromptV1":
        return cls(
            attempt_digest=attempt_digest,
            permit_digest=permit_digest,
            assembly=assembly,
            stage_id=cls.expected_stage_id(
                attempt_digest=attempt_digest,
                permit_digest=permit_digest,
                assembly=assembly,
            ),
        )

    def canonical_body(self) -> dict[str, object]:
        return {
            "assembly": self.assembly.canonical_body(),
            "attempt_digest": self.attempt_digest,
            "permit_digest": self.permit_digest,
            "stage_id": self.stage_id,
        }


@dataclass(frozen=True, slots=True)
class PromptInvocationBindingV1:
    """Exact argv binding for a staged prompt before process start.

    This is deliberately narrower than proof a CLI parsed the prompt or a provider
    received it.  It proves only that the host-side argv handed to the process
    runner contains the sealed, staged prompt exactly once.  The argv bytes are
    sealed by the authority, never copied into canonical event payloads.
    """

    staged: StagedPromptV1
    argv_artifact_digest: str
    argv_byte_count: int
    prompt_argument_count: int
    invocation_id: str

    def __post_init__(self) -> None:
        if type(self.staged) is not StagedPromptV1:
            raise TypeError("staged must be StagedPromptV1")
        object.__setattr__(
            self,
            "argv_artifact_digest",
            _digest(self.argv_artifact_digest, "argv_artifact_digest"),
        )
        if type(self.argv_byte_count) is not int or self.argv_byte_count <= 0:
            raise ValueError("argv_byte_count must be a positive exact integer")
        if self.prompt_argument_count != 1:
            raise ValueError("argv must carry the staged prompt exactly once")
        expected = self.expected_invocation_id(
            staged=self.staged,
            argv_artifact_digest=self.argv_artifact_digest,
        )
        if self.invocation_id != expected:
            raise ValueError("invocation_id does not bind stage and argv artifact")

    @staticmethod
    def _canonical_argv(argv: object) -> tuple[str, ...]:
        if type(argv) not in {tuple, list}:
            raise TypeError("argv must be a built-in list or tuple")
        # Prompt arguments legitimately preserve leading/trailing whitespace and
        # commonly end in a newline.  Command-line argument validation must not
        # reuse schema-token validation, which deliberately strips such values.
        result: list[str] = []
        for value in argv:
            if type(value) is not str or not value:
                raise ValueError("argv items must be non-empty exact strings")
            result.append(value)
        if not result:
            raise ValueError("argv must not be empty")
        return tuple(result)

    @staticmethod
    def expected_invocation_id(
        *, staged: StagedPromptV1, argv_artifact_digest: str
    ) -> str:
        if type(staged) is not StagedPromptV1:
            raise TypeError("staged must be StagedPromptV1")
        return "invocation-" + canonical_digest(
            {
                "argv_artifact_digest": _digest(
                    argv_artifact_digest, "argv_artifact_digest"
                ),
                "stage_id": staged.stage_id,
                "version": PROMPT_STAGE_VERSION,
            }
        )[:40]

    @classmethod
    def bind_argv(
        cls, *, staged: StagedPromptV1, argv: object
    ) -> tuple["PromptInvocationBindingV1", bytes]:
        if type(staged) is not StagedPromptV1:
            raise TypeError("staged must be StagedPromptV1")
        values = cls._canonical_argv(argv)
        prompt_matches = [
            value
            for value in values
            if len(value.encode("utf-8")) == staged.assembly.full_prompt_byte_count
            and hashlib.sha256(value.encode("utf-8")).hexdigest()
            == staged.assembly.full_prompt_digest
        ]
        raw = canonical_json_bytes(list(values))
        artifact_digest = hashlib.sha256(raw).hexdigest()
        binding = cls(
            staged=staged,
            argv_artifact_digest=artifact_digest,
            argv_byte_count=len(raw),
            prompt_argument_count=len(prompt_matches),
            invocation_id=cls.expected_invocation_id(
                staged=staged, argv_artifact_digest=artifact_digest
            ),
        )
        return binding, raw

    def canonical_body(self) -> dict[str, object]:
        return {
            "argv_artifact_digest": self.argv_artifact_digest,
            "argv_byte_count": self.argv_byte_count,
            "invocation_id": self.invocation_id,
            "prompt_argument_count": self.prompt_argument_count,
            "stage_id": self.staged.stage_id,
        }


class PromptStageStatus(str, Enum):
    STAGED = "staged"
    RELEASED = "released"
    UNKNOWN = "unknown"


@dataclass(frozen=True, slots=True)
class PromptStageStateV1:
    """A closed, retry-hostile state machine for one staged prompt."""

    staged: StagedPromptV1
    status: PromptStageStatus = PromptStageStatus.STAGED
    terminal_receipt_digest: str = ""

    def __post_init__(self) -> None:
        if type(self.staged) is not StagedPromptV1:
            raise TypeError("staged must be StagedPromptV1")
        if type(self.status) is not PromptStageStatus:
            raise TypeError("status must be PromptStageStatus")
        if self.status is PromptStageStatus.STAGED:
            if self.terminal_receipt_digest:
                raise ValueError("a staged prompt has no terminal receipt")
        else:
            object.__setattr__(
                self,
                "terminal_receipt_digest",
                _digest(self.terminal_receipt_digest, "terminal_receipt_digest"),
            )

    def release(self, *, receipt_digest: str) -> "PromptStageStateV1":
        if self.status is not PromptStageStatus.STAGED:
            raise ValueError("only a staged prompt can be released")
        return PromptStageStateV1(
            staged=self.staged,
            status=PromptStageStatus.RELEASED,
            terminal_receipt_digest=receipt_digest,
        )

    def unknown(self, *, receipt_digest: str) -> "PromptStageStateV1":
        if self.status is not PromptStageStatus.STAGED:
            raise ValueError("only a staged prompt can become UNKNOWN")
        return PromptStageStateV1(
            staged=self.staged,
            status=PromptStageStatus.UNKNOWN,
            terminal_receipt_digest=receipt_digest,
        )

    @property
    def automatic_redispatch_permitted(self) -> bool:
        return False


__all__ = [
    "PROMPT_STAGE_VERSION",
    "PromptAssemblyV1",
    "PromptInvocationBindingV1",
    "PromptStageStateV1",
    "PromptStageStatus",
    "StagedPromptV1",
]
