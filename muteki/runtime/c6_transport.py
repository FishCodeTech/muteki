"""Host-owned, one-shot C6 transport around the actual local Popen boundary.

The model may assemble a prompt and request one launch.  It never receives an
event-store writer, a process-start callback, an adapter selection knob, or an
abort/retry capability.  The durable sequence is deliberately narrow:

``STAGED -> INVOCATION_BOUND -> LAUNCH_CLAIMED -> RELEASED | PRELAUNCH_ABORTED | UNKNOWN``.

Phase A proves only that this host sealed final local launch material and observed
one local ``subprocess.Popen`` PID.  It does not claim that a CLI parsed the prompt
or that an upstream provider received it.
"""

from __future__ import annotations

import asyncio
import hashlib
import os
import signal
import subprocess
import threading
import time
from collections.abc import Callable, Mapping
from dataclasses import dataclass
from typing import Any

from muteki.epistemic.contracts import canonical_digest
from muteki.epistemic.sqlite_store import IntegrityError
from muteki.runtime.cognition import (
    CognitiveContextAuthority,
    DeliveredContextPacketV1,
    PromptInvocationAlreadyBound,
    PromptLaunchAlreadyClaimed,
    PromptLaunchClaimV1,
)
from muteki.runtime.contracts import AttemptPermit
from muteki.runtime.prompt_stage import PromptInvocationBindingV1, StagedPromptV1
from muteki.solver.cli_driver import run_cli_streaming as _AUDITED_RUN_CLI_STREAMING


C6_LAUNCH_PROFILE_VERSION = "muteki.runtime-c6-host-launch.v2"
C6_LAUNCH_MATERIAL_VERSION = "muteki.runtime-c6-launch-material.v1"


def _text(value: object, name: str) -> str:
    if type(value) is not str or not value or value != value.strip():
        raise ValueError(f"{name} must be exact non-empty text")
    return value


def _digest(value: object, name: str) -> str:
    text = _text(value, name)
    if len(text) != 64 or any(character not in "0123456789abcdef" for character in text):
        raise ValueError(f"{name} must be a lowercase sha256 digest")
    return text


def _environment(value: Mapping[str, str]) -> dict[str, str]:
    if not isinstance(value, Mapping):
        raise TypeError("runtime_env must be a mapping")
    result: dict[str, str] = {}
    for key, item in value.items():
        if type(key) is not str or type(item) is not str or not key:
            raise TypeError("runtime_env must contain built-in non-empty string pairs")
        upper = key.upper()
        if any(token in upper for token in ("TOKEN", "SECRET", "PASSWORD", "CREDENTIAL", "API_KEY")):
            raise C6TransportRejected(
                "C6 Phase A refuses explicit secret-bearing runtime environment overrides"
            )
        result[key] = item
    return result


@dataclass(frozen=True, slots=True)
class C6HostLaunchProfileV1:
    """Frozen host launch shape supported by the first C6 transport phase."""

    driver_name: str
    backend: str = "host_popen"
    version: str = C6_LAUNCH_PROFILE_VERSION

    def __post_init__(self) -> None:
        object.__setattr__(self, "driver_name", _text(self.driver_name, "driver_name"))
        if self.backend != "host_popen":
            raise ValueError("C6 Phase A supports host_popen only")
        if self.version != C6_LAUNCH_PROFILE_VERSION:
            raise ValueError("unsupported C6 host launch profile version")

    def canonical_body(self) -> dict[str, str]:
        return {
            "backend": self.backend,
            "driver_name": self.driver_name,
            "schema_id": C6_LAUNCH_PROFILE_VERSION,
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())


@dataclass(frozen=True, slots=True)
class C6LaunchMaterialV1:
    """Sealed non-secret manifest of the exact local launch material."""

    argv_artifact_digest: str
    cwd_digest: str
    environment: tuple[tuple[str, str], ...]
    executable_token_digest: str
    profile: C6HostLaunchProfileV1

    @classmethod
    def build(
        cls,
        *,
        argv_artifact_digest: str,
        argv: tuple[str, ...],
        cwd: str,
        runtime_env: Mapping[str, str],
        profile: C6HostLaunchProfileV1,
    ) -> "C6LaunchMaterialV1":
        if type(argv) is not tuple or not argv or any(type(item) is not str or not item for item in argv):
            raise C6TransportRejected("C6 final argv must be a non-empty built-in string tuple")
        if type(cwd) is not str or not cwd or not os.path.isabs(cwd):
            raise C6TransportRejected("C6 Phase A requires an absolute local cwd")
        if type(profile) is not C6HostLaunchProfileV1:
            raise TypeError("profile must be C6HostLaunchProfileV1")
        environment = _environment(runtime_env)
        # Values are represented only by their SHA-256 fingerprints; raw values are
        # never persisted in C6 material.  Sensitive override names are rejected
        # above instead of being fingerprinted into canonical history.
        entries = tuple(
            (key, hashlib.sha256(value.encode("utf-8")).hexdigest())
            for key, value in sorted(environment.items())
        )
        return cls(
            argv_artifact_digest=_digest(argv_artifact_digest, "argv_artifact_digest"),
            cwd_digest=canonical_digest({"cwd": cwd}),
            environment=entries,
            executable_token_digest=canonical_digest({"argv0": argv[0]}),
            profile=profile,
        )

    def canonical_body(self) -> dict[str, Any]:
        return {
            "argv_artifact_digest": self.argv_artifact_digest,
            "cwd_digest": self.cwd_digest,
            "environment": [
                {"name": key, "value_digest": value_digest}
                for key, value_digest in self.environment
            ],
            "executable_token_digest": self.executable_token_digest,
            "profile": self.profile.canonical_body(),
            "profile_digest": self.profile.digest,
            "schema_id": C6_LAUNCH_MATERIAL_VERSION,
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())


@dataclass(frozen=True, slots=True)
class BoundC6InvocationV1:
    """Private broker state for exactly one claimed final argv launch."""

    staged: StagedPromptV1
    invocation: PromptInvocationBindingV1
    claim: PromptLaunchClaimV1
    material: C6LaunchMaterialV1

    def __post_init__(self) -> None:
        if type(self.staged) is not StagedPromptV1:
            raise TypeError("staged must be StagedPromptV1")
        if type(self.invocation) is not PromptInvocationBindingV1:
            raise TypeError("invocation must be PromptInvocationBindingV1")
        if type(self.claim) is not PromptLaunchClaimV1:
            raise TypeError("claim must be PromptLaunchClaimV1")
        if type(self.material) is not C6LaunchMaterialV1:
            raise TypeError("material must be C6LaunchMaterialV1")
        if (
            self.invocation.staged != self.staged
            or self.claim.staged != self.staged
            or self.claim.invocation != self.invocation
            or self.material.argv_artifact_digest
            != self.invocation.argv_artifact_digest
            or self.material.profile.digest != self.claim.profile_digest
            or self.material.digest != self.claim.launch_material_digest
        ):
            raise ValueError("C6 launch plan lineage is internally inconsistent")


class C6TransportRejected(RuntimeError):
    """The requested transport cannot make the Phase-A observation honestly."""


@dataclass(slots=True)
class _InterlockState:
    permit_digest: str
    revoked: bool = False
    popen_started: bool = False
    claim: PromptLaunchClaimV1 | None = None
    claim_live_validator: Callable[[AttemptPermit, PromptLaunchClaimV1], None] | None = None
    claim_launch_fence: Callable[[AttemptPermit, PromptLaunchClaimV1], Any] | None = None
    on_revoke: Callable[[str], None] | None = None


class C6HostLaunchInterlock:
    """Thread-safe host fence spanning revocation, Popen, and first receipt.

    The actual process creation occurs in ``asyncio.to_thread``.  An ``asyncio``
    lock is therefore insufficient: this lock is held by the callback that wraps
    the direct ``subprocess.Popen`` instruction in ``run_cli_streaming``.
    """

    def __init__(self) -> None:
        self._lock = threading.RLock()
        self._states: dict[str, _InterlockState] = {}

    def register(self, *, permit: AttemptPermit) -> None:
        if type(permit) is not AttemptPermit:
            raise TypeError("permit must be AttemptPermit")
        with self._lock:
            existing = self._states.get(permit.permit_id)
            if existing is None:
                self._states[permit.permit_id] = _InterlockState(permit.digest)
                return
            if existing.permit_digest != permit.digest:
                raise IntegrityError("C6 interlock permit identity diverged")

    def bind(
        self,
        *,
        permit: AttemptPermit,
        on_revoke: Callable[[str], None],
    ) -> None:
        if type(permit) is not AttemptPermit or not callable(on_revoke):
            raise TypeError("C6 interlock binding is malformed")
        callback: Callable[[str], None] | None = None
        with self._lock:
            state = self._states.get(permit.permit_id)
            if state is None or state.permit_digest != permit.digest:
                raise C6TransportRejected("C6 interlock has no exact supervisor launch")
            if state.on_revoke is not None and state.on_revoke is not on_revoke:
                raise C6TransportRejected("C6 interlock already has another host broker")
            state.on_revoke = on_revoke
            if state.revoked:
                callback = state.on_revoke
        if callback is not None:
            callback("supervisor revoked the C6 permit before host activation")
            raise C6TransportRejected("C6 permit was revoked before host activation")

    def revoke(self, *, permit: AttemptPermit, reason: str) -> None:
        if type(permit) is not AttemptPermit:
            raise TypeError("permit must be AttemptPermit")
        reason = _text(reason, "reason")
        callback: Callable[[str], None] | None = None
        with self._lock:
            state = self._states.get(permit.permit_id)
            if state is None or state.permit_digest != permit.digest:
                return
            state.revoked = True
            callback = state.on_revoke
        if callback is not None:
            callback(reason)

    def revoke_all(self, *, reason: str) -> None:
        reason = _text(reason, "reason")
        callbacks: list[Callable[[str], None]] = []
        with self._lock:
            for state in self._states.values():
                state.revoked = True
                if state.on_revoke is not None:
                    callbacks.append(state.on_revoke)
        for callback in callbacks:
            callback(reason)

    def arm_claim(
        self,
        *,
        permit: AttemptPermit,
        claim: PromptLaunchClaimV1,
        claim_live_validator: Callable[[AttemptPermit, PromptLaunchClaimV1], None],
        claim_launch_fence: Callable[[AttemptPermit, PromptLaunchClaimV1], Any],
    ) -> None:
        """Bind exactly one durable claim to the in-memory Popen fence."""

        if type(permit) is not AttemptPermit or type(claim) is not PromptLaunchClaimV1:
            raise TypeError("C6 interlock claim identity is malformed")
        if not callable(claim_live_validator) or not callable(claim_launch_fence):
            raise TypeError("C6 interlock requires durable claim validation and fencing")
        if claim.staged.permit_digest != permit.digest:
            raise C6TransportRejected("C6 claim belongs to another permit")
        with self._lock:
            state = self._states.get(permit.permit_id)
            if (
                state is None
                or state.permit_digest != permit.digest
                or state.revoked
                or state.popen_started
            ):
                raise C6TransportRejected("C6 host claim cannot be armed")
            if state.claim is not None:
                raise C6TransportRejected("C6 interlock already armed a host claim")
            state.claim = claim
            state.claim_live_validator = claim_live_validator
            state.claim_launch_fence = claim_launch_fence

    def spawn_under_claim(
        self,
        *,
        permit: AttemptPermit,
        claim: PromptLaunchClaimV1,
        spawn: Callable[[], subprocess.Popen[Any]],
        on_process_started: Callable[[subprocess.Popen[Any]], None],
        on_prelaunch_aborted: Callable[[str], None],
        on_release_recorded: Callable[[], None],
        on_release_committed: Callable[[], None],
        on_fence_rolled_back: Callable[[], None],
    ) -> subprocess.Popen[Any]:
        if type(permit) is not AttemptPermit or type(claim) is not PromptLaunchClaimV1:
            raise TypeError("C6 interlock launch identity is malformed")
        if (
            not callable(spawn)
            or not callable(on_process_started)
            or not callable(on_prelaunch_aborted)
            or not callable(on_release_recorded)
            or not callable(on_release_committed)
            or not callable(on_fence_rolled_back)
        ):
            raise TypeError("C6 interlock host callbacks are malformed")
        with self._lock:
            state = self._states.get(permit.permit_id)
            if (
                state is None
                or state.permit_digest != permit.digest
                or state.revoked
                or state.popen_started
                or state.claim != claim
                or state.claim_live_validator is None
                or state.claim_launch_fence is None
                or time.time_ns() >= permit.expires_at_ns
            ):
                try:
                    on_prelaunch_aborted(
                        "C6 interlock denied a revoked, expired, or reused claim"
                    )
                finally:
                    raise C6TransportRejected("C6 host launch claim is no longer active")
            validator = state.claim_live_validator
            launch_fence = state.claim_launch_fence
            validator_error: BaseException | None = None
            popen_error: BaseException | None = None
            post_popen_error: BaseException | None = None
            released_proc: subprocess.Popen[Any] | None = None

            # ``launch_fence`` owns one SQLite BEGIN IMMEDIATE transaction through
            # final validation, the actual Popen call, and a successful RELEASED
            # receipt.  A competing recovery/budget/BOOT writer therefore cannot
            # terminalize the stage in the post-validator/pre-Popen window.
            # Error terminals are best-effort inside the same fence; if recording
            # one fails, the transaction leaves the claim unresolved for the
            # existing fail-closed UNKNOWN recovery path rather than inventing a
            # known-not-started result.
            try:
                with launch_fence(permit, claim):
                    try:
                        validator(permit, claim)
                    except BaseException as exc:
                        # A durable claim is a one-shot pre-Popen boundary.  If its
                        # canonical lineage or live owner is no longer valid, revoke
                        # this in-memory fence before reporting the known-not-started
                        # closure.  Do not re-raise until the abort command has had a
                        # chance to commit in this same launch transaction.
                        state.revoked = True
                        on_prelaunch_aborted(
                            "C6 durable host claim was not live at the local Popen fence"
                        )
                        validator_error = exc
                    else:
                        try:
                            proc = spawn()
                        except BaseException as exc:
                            # ``Popen`` raised before returning an object, so there is
                            # no honest positive local start witness.
                            on_prelaunch_aborted(
                                "local Popen raised before a process handle existed"
                            )
                            popen_error = exc
                        else:
                            state.popen_started = True
                            try:
                                on_process_started(proc)
                            except BaseException as exc:
                                _kill_process(proc)
                                post_popen_error = exc
                            else:
                                # Positive process-start evidence must be durable before
                                # the SQLite writer fence is released.  Do not publish
                                # the in-memory RELEASED state yet: the outer fence can
                                # still fail its final SQLite commit after this nested
                                # receipt savepoint succeeds.
                                try:
                                    on_release_recorded()
                                except BaseException:
                                    _kill_process(proc)
                                    raise
                                released_proc = proc
            except BaseException:
                # A failed outer commit rolls the nested RELEASED savepoint back.
                # The process is now an ambiguous effect, never a released child;
                # kill it before the broker writes fail-closed UNKNOWN in a fresh
                # transaction outside this fence.  UNKNOWN/PRELAUNCH_ABORTED may
                # also have been committed only to nested savepoints, so their
                # in-memory broker flags are speculative until this callback sees
                # the outer transaction commit.
                if released_proc is not None:
                    _kill_process(released_proc)
                on_fence_rolled_back()
                raise

            # ``c6_host_launch_fence`` has committed successfully only after its
            # context exits.  This callback is deliberately outside that context:
            # a commit failure rolls back RELEASED, leaves ``released_proc``
            # unpublished, and reaches the broker's fail-closed UNKNOWN path.
            if released_proc is not None:
                try:
                    on_release_committed()
                except BaseException:
                    _kill_process(released_proc)
                    raise
                return released_proc

            if validator_error is not None:
                raise C6TransportRejected(
                    "C6 durable host launch claim is not active"
                ) from validator_error
            if popen_error is not None:
                raise popen_error
            if post_popen_error is not None:
                raise post_popen_error
            raise C6TransportRejected("C6 host launch fence did not resolve a terminal")


def _kill_process(proc: Any) -> None:
    pid = getattr(proc, "pid", None)
    if type(pid) is int and pid > 0:
        try:
            os.killpg(os.getpgid(pid), signal.SIGKILL)
            return
        except Exception:
            pass
        try:
            os.kill(pid, signal.SIGKILL)
            return
        except Exception:
            pass
    try:
        proc.kill()
    except Exception:
        pass


class C6HostPopenAdapter:
    """Non-overridable host adapter for the audited local ``run_cli_streaming`` path."""

    def __init__(
        self,
        *,
        authority: CognitiveContextAuthority,
        delivered: DeliveredContextPacketV1,
        driver: Any,
        profile: C6HostLaunchProfileV1,
        interlock: C6HostLaunchInterlock,
    ) -> None:
        if type(authority) is not CognitiveContextAuthority:
            raise TypeError("authority must be CognitiveContextAuthority")
        if type(delivered) is not DeliveredContextPacketV1:
            raise TypeError("delivered must be DeliveredContextPacketV1")
        if type(profile) is not C6HostLaunchProfileV1:
            raise TypeError("profile must be C6HostLaunchProfileV1")
        if type(interlock) is not C6HostLaunchInterlock:
            raise TypeError("interlock must be C6HostLaunchInterlock")
        if not callable(getattr(driver, "parse", None)):
            raise TypeError("C6 host adapter requires a concrete CLI driver parser")
        self._authority = authority
        self._delivered = delivered
        self._driver = driver
        self._profile = profile
        self._interlock = interlock
        self._permit: AttemptPermit | None = None
        self._cancel_event = threading.Event()
        self._proc_lock = threading.RLock()
        self._active_proc: subprocess.Popen[Any] | None = None

    def activate(self, *, permit: AttemptPermit, on_revoke: Callable[[str], None]) -> None:
        if type(permit) is not AttemptPermit:
            raise TypeError("permit must be AttemptPermit")
        if getattr(self._driver, "name", None) != self._profile.driver_name:
            raise C6TransportRejected("C6 host adapter driver differs from frozen profile")
        if self._permit is not None and self._permit.digest != permit.digest:
            raise C6TransportRejected("C6 host adapter was activated with another permit")
        self._permit = permit
        self._interlock.bind(permit=permit, on_revoke=on_revoke)

    def abort(self) -> None:
        self._cancel_event.set()
        with self._proc_lock:
            proc = self._active_proc
        if proc is not None:
            _kill_process(proc)

    def revoke_launch_claim(self, *, reason: str) -> None:
        """Synchronously win-or-observe the Popen interlock before cancellation.

        A broker revocation is not allowed to write a known-not-started terminal
        merely because its local bookkeeping has not observed ``Popen`` yet.  The
        interlock spans the durable live-claim check, the actual Popen call, and
        the first PID receipt.  Taking it first therefore gives a binary result:
        either revocation wins before Popen, or the process/PID receipt is already
        ordered before the broker is allowed to close the claim.
        """

        reason = _text(reason, "reason")
        permit = self._permit
        if permit is not None:
            self._interlock.revoke(permit=permit, reason=reason)
        self.abort()

    def arm_claim(self, *, claim: PromptLaunchClaimV1) -> None:
        """Arm the local fence only after the authority committed this claim."""

        permit = self._permit
        if permit is None:
            raise C6TransportRejected("C6 host adapter is not activated")

        def verify_live_claim(
            exact_permit: AttemptPermit, exact_claim: PromptLaunchClaimV1
        ) -> None:
            self._authority._assert_durable_host_launch_claim_live(
                delivered=self._delivered,
                permit=exact_permit,
                claim=exact_claim,
                occurred_at_ns=time.time_ns(),
            )

        def fence_live_claim(
            exact_permit: AttemptPermit, exact_claim: PromptLaunchClaimV1
        ) -> Any:
            return self._authority._fence_final_host_launch(
                delivered=self._delivered,
                permit=exact_permit,
                claim=exact_claim,
            )

        self._interlock.arm_claim(
            permit=permit,
            claim=claim,
            claim_live_validator=verify_live_claim,
            claim_launch_fence=fence_live_claim,
        )

    async def execute(
        self,
        *,
        final_argv: tuple[str, ...],
        cwd: str,
        timeout: int,
        runtime_env: Mapping[str, str],
        claim: PromptLaunchClaimV1,
        on_process_started: Callable[[subprocess.Popen[Any]], None],
        on_prelaunch_aborted: Callable[[str], None],
        on_release_committed: Callable[[], None],
        on_fence_rolled_back: Callable[[], None],
        on_raw_streams: Callable[[str, str], None] | None = None,
    ) -> Any:
        permit = self._permit
        if permit is None:
            raise C6TransportRejected("C6 host adapter is not activated")
        if type(final_argv) is not tuple or not final_argv:
            raise C6TransportRejected("C6 host adapter requires a final argv tuple")
        if type(cwd) is not str or not os.path.isabs(cwd):
            raise C6TransportRejected("C6 host adapter requires an absolute cwd")
        if type(timeout) is not int or timeout <= 0:
            raise C6TransportRejected("C6 host adapter requires a positive timeout")
        environment = _environment(runtime_env)
        if getattr(self._driver, "name", None) != self._profile.driver_name:
            raise C6TransportRejected("C6 driver changed after profile freeze")
        if (
            type(claim) is not PromptLaunchClaimV1
            or claim.staged.permit_digest != permit.digest
            or claim.invocation.staged != claim.staged
            or claim.profile_digest != self._profile.digest
            or C6LaunchMaterialV1.build(
                argv_artifact_digest=claim.invocation.argv_artifact_digest,
                argv=final_argv,
                cwd=cwd,
                runtime_env=environment,
                profile=self._profile,
            ).digest
            != claim.launch_material_digest
        ):
            raise C6TransportRejected(
                "C6 host adapter launch material diverges from its durable claim"
            )

        def _started(proc: subprocess.Popen[Any]) -> None:
            if not isinstance(proc, subprocess.Popen):
                raise C6TransportRejected("C6 host adapter did not receive a real Popen")
            with self._proc_lock:
                self._active_proc = proc
            on_process_started(proc)

        def _wrapped_popen(
            spawn: Callable[[], subprocess.Popen[Any]],
        ) -> subprocess.Popen[Any]:
            def _release_recorded() -> None:
                self._authority._assert_fenced_host_launch_terminal(
                    delivered=self._delivered,
                    permit=permit,
                    claim=claim,
                    expected_kind="CONTEXT_PROMPT_RELEASED",
                )

            return self._interlock.spawn_under_claim(
                permit=permit,
                claim=claim,
                spawn=spawn,
                on_process_started=_started,
                on_prelaunch_aborted=on_prelaunch_aborted,
                on_release_recorded=_release_recorded,
                on_release_committed=on_release_committed,
                on_fence_rolled_back=on_fence_rolled_back,
            )

        try:
            return await asyncio.to_thread(
                _AUDITED_RUN_CLI_STREAMING,
                self._driver,
                list(final_argv),
                cwd=cwd,
                timeout=timeout,
                on_step=lambda _step: None,
                env=environment,
                cancel_event=self._cancel_event,
                container=None,
                stdin_text=None,
                popen_wrapper=_wrapped_popen,
                on_raw_streams=on_raw_streams,
                inherit_env=False,
            )
        except asyncio.CancelledError:
            # ``asyncio.to_thread`` does not stop a queued/running thread.  Merely
            # setting its cancel event is insufficient because the audited runner
            # reaches ``Popen`` before its polling loop.  Revoke the same
            # supervisor-owned interlock first so a late wrapper invocation can
            # only close the claim as PRELAUNCH_ABORTED, never create a child.
            self.revoke_launch_claim(
                reason="C6 host adapter coroutine was cancelled before completion"
            )
            raise
        finally:
            with self._proc_lock:
                self._active_proc = None


class C6InvocationRunner:
    """Worker-facing request façade with no canonical write or callback surface."""

    def __init__(self, *, broker: "C6HostLaunchBroker") -> None:
        if type(broker) is not C6HostLaunchBroker:
            raise TypeError("broker must be C6HostLaunchBroker")
        self._broker = broker

    async def run(
        self,
        *,
        prompt: str,
        final_argv: list[str],
        cwd: str,
        timeout: int,
        runtime_env: Mapping[str, str],
    ) -> Any:
        return await self._broker._run(
            prompt=prompt,
            final_argv=final_argv,
            cwd=cwd,
            timeout=timeout,
            runtime_env=runtime_env,
        )


class C6HostLaunchBroker:
    """The only C6 writer for stage, claim, release, abort, and UNKNOWN."""

    def __init__(
        self,
        *,
        authority: CognitiveContextAuthority,
        delivered: DeliveredContextPacketV1,
        profile: C6HostLaunchProfileV1,
        host_adapter: C6HostPopenAdapter,
    ) -> None:
        if type(authority) is not CognitiveContextAuthority:
            raise TypeError("authority must be CognitiveContextAuthority")
        if type(delivered) is not DeliveredContextPacketV1:
            raise TypeError("delivered must be DeliveredContextPacketV1")
        if type(profile) is not C6HostLaunchProfileV1:
            raise TypeError("profile must be C6HostLaunchProfileV1")
        if type(host_adapter) is not C6HostPopenAdapter:
            raise TypeError("C6 broker requires the exact host Popen adapter")
        self._authority = authority
        self._delivered = delivered
        self._profile = profile
        self._host_adapter = host_adapter
        self._permit: AttemptPermit | None = None
        self._plan: BoundC6InvocationV1 | None = None
        self._lock = threading.RLock()
        self._runner = C6InvocationRunner(broker=self)
        self._invocation_started = False
        self._popen_crossed = False
        self._released = False
        self._prelaunch_aborted = False
        self._unknown_recorded = False
        self._unusable_reason = ""

    @property
    def runner(self) -> C6InvocationRunner:
        return self._runner

    @property
    def profile(self) -> C6HostLaunchProfileV1:
        return self._profile

    def activate(self, *, permit: AttemptPermit) -> None:
        if type(permit) is not AttemptPermit:
            raise TypeError("permit must be AttemptPermit")
        if permit.constraints.get("context_packet") != self._delivered.binding.canonical_body():
            raise IntegrityError("C6 broker permit does not bind the ContextPacket")
        with self._lock:
            if self._permit is not None and self._permit.digest != permit.digest:
                raise IntegrityError("C6 broker was activated with another permit")
            self._permit = permit
        prior = self._authority._close_existing_prompt_invocation_for_host(
            delivered=self._delivered,
            permit=permit,
            reason="C6 broker activation found a prior invocation boundary",
            occurred_at_ns=time.time_ns(),
        )
        if prior != "none":
            with self._lock:
                self._unusable_reason = (
                    "C6 permit already has a persisted invocation boundary "
                    f"({prior}); redispatch is forbidden"
                )
            self._host_adapter.abort()
            raise C6TransportRejected(self._unusable_reason)
        # The interlock callback is deliberately distinct from the public broker
        # method.  Public revocation must acquire the interlock *before* it writes
        # a prelaunch terminal; callbacks are invoked only after that lock has
        # decided whether Popen already crossed.
        self._host_adapter.activate(
            permit=permit, on_revoke=self._on_interlock_revoked
        )

    def revoke(self, reason: str) -> None:
        """Synchronously fence a retained runner before supervisor terminalization."""

        reason = _text(reason, "reason")
        self._host_adapter.revoke_launch_claim(reason=reason)
        # ``C6HostLaunchInterlock.revoke`` normally invokes the callback above.
        # This second, idempotent application also covers a broker that was made
        # unusable before its adapter could bind a supervisor interlock.
        self._apply_revocation_after_interlock(reason)

    def _on_interlock_revoked(self, reason: str) -> None:
        """Apply durable closure only after the interlock made its Popen decision."""

        self._apply_revocation_after_interlock(_text(reason, "reason"))

    def _apply_revocation_after_interlock(self, reason: str) -> None:
        """Idempotently close a revoked plan after its Popen fence is quiescent."""

        with self._lock:
            if not self._unusable_reason:
                self._unusable_reason = f"C6 host launch was revoked: {reason}"
            plan = self._plan
            crossed = self._popen_crossed
            released = self._released
            aborted = self._prelaunch_aborted
        self._host_adapter.abort()
        if plan is None or released or aborted:
            return
        if crossed:
            self._mark_unknown_once(
                plan=plan,
                reason="C6 host launch was revoked after local Popen may have started",
            )
        else:
            self._mark_prelaunch_aborted_once(plan=plan, reason=reason)

    def _active_permit(self) -> AttemptPermit:
        with self._lock:
            permit = self._permit
        if permit is None:
            raise C6TransportRejected("C6 host launch broker is not admitted")
        return permit

    def _assert_phase_a(
        self,
        *,
        prompt: str,
        final_argv: list[str],
        cwd: str,
        timeout: int,
        runtime_env: Mapping[str, str],
    ) -> tuple[tuple[str, ...], dict[str, str]]:
        if type(final_argv) is not list or not final_argv:
            raise C6TransportRejected("C6 final argv must be a non-empty built-in list")
        if type(prompt) is not str or not prompt:
            raise C6TransportRejected("C6 prompt must be non-empty exact text")
        if type(cwd) is not str or not os.path.isabs(cwd):
            raise C6TransportRejected("C6 Phase A requires an absolute local cwd")
        if type(timeout) is not int or timeout <= 0:
            raise C6TransportRejected("C6 host launch timeout must be positive")
        if prompt.count(self._delivered.render_for_prompt()) != 1:
            raise C6TransportRejected(
                "C6 final prompt must contain the exact sealed ContextPacket once"
            )
        try:
            argv = PromptInvocationBindingV1._canonical_argv(final_argv)
            environment = _environment(runtime_env)
        except (TypeError, ValueError) as exc:
            raise C6TransportRejected("C6 final launch material is malformed") from exc
        return argv, environment

    def _mark_unknown_once(self, *, plan: BoundC6InvocationV1, reason: str) -> None:
        with self._lock:
            if self._unknown_recorded or self._released or self._prelaunch_aborted:
                return
            self._unknown_recorded = True
        try:
            self._authority.record_prompt_unknown(
                delivered=self._delivered,
                permit=self._active_permit(),
                staged=plan.staged,
                invocation=plan.invocation,
                reason=reason,
                occurred_at_ns=time.time_ns(),
            )
        except IntegrityError:
            # A concurrent release/abort terminal is already authoritative.  Do not
            # manufacture a second terminal or hide the original race.
            pass

    def _mark_prelaunch_aborted_once(
        self, *, plan: BoundC6InvocationV1, reason: str
    ) -> None:
        with self._lock:
            if self._prelaunch_aborted or self._released or self._popen_crossed:
                return
            self._prelaunch_aborted = True
        try:
            self._authority.record_prompt_prelaunch_aborted(
                delivered=self._delivered,
                permit=self._active_permit(),
                claim=plan.claim,
                reason=reason,
                occurred_at_ns=time.time_ns(),
            )
        except IntegrityError:
            # A concurrent terminal is still fail-closed.  The caller raises and
            # supervisor closure independently resolves the canonical history.
            pass

    def _on_process_started(
        self, *, plan: BoundC6InvocationV1, proc: subprocess.Popen[Any]
    ) -> None:
        with self._lock:
            self._popen_crossed = True
        if not isinstance(proc, subprocess.Popen) or type(proc.pid) is not int or proc.pid <= 0:
            self._mark_unknown_once(
                plan=plan,
                reason="C6 host adapter did not expose a valid local Popen PID",
            )
            self._host_adapter.abort()
            raise C6TransportRejected("C6 Phase A requires a local Popen pid observation")
        try:
            self._authority._record_prompt_release_from_host(
                delivered=self._delivered,
                permit=self._active_permit(),
                staged=plan.staged,
                invocation=plan.invocation,
                claim=plan.claim,
                process_id=proc.pid,
                occurred_at_ns=time.time_ns(),
            )
        except BaseException:
            self._mark_unknown_once(
                plan=plan,
                reason="local C6 child-start observation could not be recorded",
            )
            self._host_adapter.abort()
            raise

    def _seal_audited_runtime_streams(self, stdout: str, stderr: str) -> None:
        """Bind post-drain C6 text to declared cognitive observation ids.

        This callback is installed by the exact broker, not exposed by the
        worker-facing runner facade.  Ordinary C6 attempts are a no-op; executable
        cognitive assignments are sealed under the separate output-port authority.
        """

        from muteki.runtime.cognitive_output_capture_v1 import (
            CognitiveRuntimeOutputCaptureV1,
        )

        CognitiveRuntimeOutputCaptureV1(
            store=self._authority._store,
            cas=self._authority._cas,
            delivered=self._delivered,
            permit=self._active_permit(),
        ).seal_from_audited_runner(
            stdout=stdout,
            stderr=stderr,
            occurred_at_ns=time.time_ns(),
        )

    def _on_release_committed(self, *, plan: BoundC6InvocationV1) -> None:
        """Publish RELEASED only after the enclosing SQLite fence committed.

        ``_record_prompt_release_from_host`` uses a nested savepoint because it
        runs inside the long Popen fence.  A later failure of the outer commit
        rolls that savepoint back.  In-memory broker state must therefore not
        claim release until ``spawn_under_claim`` has returned from the fence
        normally; otherwise its exception path would suppress the required
        UNKNOWN closure for a real child whose release receipt vanished.
        """

        with self._lock:
            if self._plan != plan or not self._popen_crossed:
                raise C6TransportRejected(
                    "C6 host release commit callback has no live Popen boundary"
                )
            if self._unknown_recorded or self._prelaunch_aborted:
                raise C6TransportRejected(
                    "C6 host release commit callback follows another terminal"
                )
            self._released = True

    def _on_launch_fence_rolled_back(self, *, plan: BoundC6InvocationV1) -> None:
        """Discard terminal flags that existed only inside a rolled-back fence.

        `RELEASED` is already deferred until outer commit, but the broker's
        UNKNOWN/PRELAUNCH callbacks can run in nested savepoints while the C6
        fence is still open.  If the final outer commit fails, those rows vanish
        with the transaction.  Keeping their in-memory booleans would suppress
        the fresh fail-closed closure required after a real Popen or a failed
        prelaunch boundary.
        """

        with self._lock:
            # This callback is invoked while the interlock still owns the exact
            # launch boundary.  A different plan cannot legitimately reuse this
            # broker, but resetting to the conservative state is safer than
            # preserving a speculative terminal if an internal caller diverges.
            if self._plan != plan:
                self._unusable_reason = "C6 launch fence rolled back for another plan"
            self._released = False
            self._prelaunch_aborted = False
            self._unknown_recorded = False

    async def _run(
        self,
        *,
        prompt: str,
        final_argv: list[str],
        cwd: str,
        timeout: int,
        runtime_env: Mapping[str, str],
    ) -> Any:
        with self._lock:
            if self._unusable_reason:
                raise C6TransportRejected(self._unusable_reason)
            if self._invocation_started:
                raise C6TransportRejected(
                    "C6 Phase A permits at most one invocation for an admitted permit"
                )
            self._invocation_started = True
        try:
            argv, environment = self._assert_phase_a(
                prompt=prompt,
                final_argv=final_argv,
                cwd=cwd,
                timeout=timeout,
                runtime_env=runtime_env,
            )
            permit = self._active_permit()
            prior = self._authority._close_existing_prompt_invocation_for_host(
                delivered=self._delivered,
                permit=permit,
                reason="C6 broker found a competing invocation before local Popen",
                occurred_at_ns=time.time_ns(),
            )
            if prior != "none":
                raise C6TransportRejected(
                    "C6 permit already has an invocation boundary; redispatch is forbidden"
                )
            staged = self._authority.stage_prompt(
                delivered=self._delivered,
                permit=permit,
                prompt=prompt,
                transport="argv",
                occurred_at_ns=time.time_ns(),
            )
            try:
                invocation = self._authority.bind_prompt_invocation(
                    delivered=self._delivered,
                    permit=permit,
                    staged=staged,
                    argv=argv,
                    occurred_at_ns=time.time_ns(),
                    require_fresh=True,
                )
            except PromptInvocationAlreadyBound as exc:
                self._authority._close_existing_prompt_invocation_for_host(
                    delivered=self._delivered,
                    permit=permit,
                    reason="C6 broker rejected an idempotent invocation reuse",
                    occurred_at_ns=time.time_ns(),
                )
                raise C6TransportRejected(
                    "C6 invocation was already bound; host execution is forbidden"
                ) from exc
            material = C6LaunchMaterialV1.build(
                argv_artifact_digest=invocation.argv_artifact_digest,
                argv=argv,
                cwd=cwd,
                runtime_env=environment,
                profile=self._profile,
            )
            if self._authority._seal_host_launch_material(
                body=material.canonical_body()
            ) != material.digest:
                raise IntegrityError("C6 launch material digest diverged from CAS")
            try:
                claim = self._authority.claim_prompt_launch(
                    delivered=self._delivered,
                    permit=permit,
                    staged=staged,
                    invocation=invocation,
                    profile_digest=self._profile.digest,
                    launch_material_digest=material.digest,
                    occurred_at_ns=time.time_ns(),
                )
            except PromptLaunchAlreadyClaimed as exc:
                self._authority._close_existing_prompt_invocation_for_host(
                    delivered=self._delivered,
                    permit=permit,
                    reason="C6 broker rejected a persisted host launch claim reuse",
                    occurred_at_ns=time.time_ns(),
                )
                raise C6TransportRejected(
                    "C6 host launch claim was already used; redispatch is forbidden"
                ) from exc
            plan = BoundC6InvocationV1(
                staged=staged,
                invocation=invocation,
                claim=claim,
                material=material,
            )
            with self._lock:
                self._plan = plan
            # A reproduction must freeze intent and then receive a separately
            # authorized launcher snapshot while the durable claim is live but
            # before this broker arms the only Popen boundary.  Ordinary C6
            # attempts return ``None`` from the declaration authority and keep
            # their byte-for-byte path unchanged.
            from muteki.runtime.cognitive_reproduction_evidence_v1 import (
                CognitiveReproductionDeclarationAuthorityV1,
                CognitiveReproductionLaunchWitnessAuthorityV1,
            )

            declaration = CognitiveReproductionDeclarationAuthorityV1(
                store=self._authority._store,
                cas=self._authority._cas,
            ).declare_if_reproduction(
                delivered=self._delivered,
                permit=permit,
                staged=staged,
                invocation=invocation,
                claim=claim,
                material=material,
                cwd=cwd,
                runtime_env=environment,
                occurred_at_ns=time.time_ns(),
            )
            CognitiveReproductionLaunchWitnessAuthorityV1(
                store=self._authority._store,
            ).witness_if_declared(
                declaration=declaration,
                permit=permit,
                staged=staged,
                invocation=invocation,
                claim=claim,
                material=material,
                cwd=cwd,
                runtime_env=environment,
                occurred_at_ns=time.time_ns(),
            )
            # The adapter cannot start merely because a claim-shaped object exists:
            # arm the supervisor-owned Popen fence with this exact durable claim.
            self._host_adapter.arm_claim(claim=claim)
            result = await self._host_adapter.execute(
                final_argv=argv,
                cwd=cwd,
                timeout=timeout,
                runtime_env=environment,
                claim=claim,
                on_process_started=lambda proc: self._on_process_started(
                    plan=plan, proc=proc
                ),
                on_prelaunch_aborted=lambda reason: self._mark_prelaunch_aborted_once(
                    plan=plan, reason=reason
                ),
                on_release_committed=lambda: self._on_release_committed(plan=plan),
                on_fence_rolled_back=lambda: self._on_launch_fence_rolled_back(
                    plan=plan
                ),
                on_raw_streams=self._seal_audited_runtime_streams,
            )
            with self._lock:
                released = self._released
                crossed = self._popen_crossed
                aborted = self._prelaunch_aborted
            if not released:
                if crossed:
                    self._mark_unknown_once(
                        plan=plan,
                        reason="C6 final argv returned after a Popen without release receipt",
                    )
                elif not aborted:
                    self._mark_prelaunch_aborted_once(
                        plan=plan,
                        reason="C6 final argv returned without reaching local Popen",
                    )
                raise C6TransportRejected(
                    "C6 final argv did not produce a verified host release receipt"
                )
            return result
        except BaseException:
            with self._lock:
                plan = self._plan
                crossed = self._popen_crossed
                released = self._released
                aborted = self._prelaunch_aborted
                if not self._unusable_reason:
                    self._unusable_reason = "C6 host launch failed closed"
            if plan is not None and not released and not aborted:
                if crossed:
                    self._mark_unknown_once(
                        plan=plan,
                        reason="C6 final argv failed after local Popen may have started",
                    )
                else:
                    self._mark_prelaunch_aborted_once(
                        plan=plan,
                        reason="C6 host launch failed before local Popen",
                    )
            self._host_adapter.abort()
            raise


__all__ = [
    "BoundC6InvocationV1",
    "C6HostLaunchBroker",
    "C6HostLaunchInterlock",
    "C6HostLaunchProfileV1",
    "C6HostPopenAdapter",
    "C6InvocationRunner",
    "C6LaunchMaterialV1",
    "C6TransportRejected",
    "C6_LAUNCH_MATERIAL_VERSION",
    "C6_LAUNCH_PROFILE_VERSION",
]
