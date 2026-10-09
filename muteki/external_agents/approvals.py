"""Canonical approval messages shared by Conversation and Agent adapters."""

from __future__ import annotations

import hashlib
import json
import os
import shlex
import threading
from dataclasses import dataclass, field
from enum import Enum
from typing import Any, Mapping, Optional

from muteki.platform.contracts.external_agents import AccessMode


class ApprovalChoice(str, Enum):
    ALLOW = "allow"
    DENY = "deny"


class ApprovalScope(str, Enum):
    ONCE = "once"
    SESSION = "session"


@dataclass(frozen=True)
class ApprovalRequest:
    approval_id: str
    details: dict[str, Any] = field(default_factory=dict)

    @classmethod
    def from_payload(cls, payload: Mapping[str, Any]) -> "ApprovalRequest":
        approval_id = str(
            payload.get("approval_id") or payload.get("request_id") or ""
        ).strip()
        if not approval_id:
            raise ValueError("approval request requires approval_id")
        details = dict(payload)
        details.pop("approval_id", None)
        details.pop("request_id", None)
        return cls(approval_id=approval_id, details=details)

    def to_payload(self) -> dict[str, Any]:
        return {"approval_id": self.approval_id, **self.details}


@dataclass(frozen=True)
class ApprovalDecision:
    approval_id: str
    choice: ApprovalChoice
    scope: ApprovalScope = ApprovalScope.ONCE
    note: str = ""
    option_id: str = ""

    @classmethod
    def from_payload(cls, payload: Mapping[str, Any]) -> "ApprovalDecision":
        approval_id = str(
            payload.get("approval_id") or payload.get("request_id") or ""
        ).strip()
        if not approval_id:
            raise ValueError("approval decision requires approval_id")
        try:
            choice = ApprovalChoice(str(payload.get("decision") or ""))
        except ValueError as exc:
            raise ValueError("approval decision must be allow or deny") from exc
        try:
            scope = ApprovalScope(str(payload.get("scope") or "once"))
        except ValueError as exc:
            raise ValueError("approval scope must be once or session") from exc
        return cls(
            approval_id=approval_id,
            choice=choice,
            scope=scope,
            note=str(payload.get("note") or payload.get("message") or ""),
            option_id=str(payload.get("option_id") or "").strip(),
        )

    @property
    def allowed(self) -> bool:
        return self.choice is ApprovalChoice.ALLOW

    def to_payload(self) -> dict[str, Any]:
        return {
            "approval_id": self.approval_id,
            "decision": self.choice.value,
            "scope": self.scope.value,
            "note": self.note,
            **({"option_id": self.option_id} if self.option_id else {}),
        }

    def codex_decision(self) -> str:
        if not self.allowed:
            return "decline"
        if self.scope is ApprovalScope.SESSION:
            return "acceptForSession"
        return "accept"


# -- scoped "allow always / for session" -------------------------------------
#
# An engine's own "always allow" option is usually coarser than what the user
# approved (a whole tool kind, a command prefix, every file).  Adapters call
# these helpers so a remembered approval covers exactly the tool kind and
# normalized target that was shown, and never changes the access mode.

APPROVAL_SCOPE_CODE = "external_agent.approval.scope"
APPROVAL_MODE_UPGRADE_CODE = "external_agent.approval.mode_upgrade"

_ACCESS_ORDER = {
    AccessMode.SUPERVISED: 0,
    AccessMode.AUTO_ACCEPT_EDITS: 1,
    AccessMode.AUTO: 2,
    AccessMode.FULL_ACCESS: 3,
}


class ApprovalScopeError(ValueError):
    """An approval cannot be remembered or applied as requested."""

    def __init__(self, code: str, message: str) -> None:
        super().__init__(message)
        self.code = code


@dataclass(frozen=True)
class ApprovalTarget:
    """Exact tool kind plus normalized target of one approval request."""

    kind: str
    target: str

    @classmethod
    def from_payload(
        cls, payload: Mapping[str, Any], *, cwd: str = ""
    ) -> Optional["ApprovalTarget"]:
        """Target of an ``approval.requested`` payload, or ``None``.

        ``None`` means the request names nothing exact enough to remember
        (the decision must stay a one-shot).  Normalization is structural
        only: argv tokenization, absolute normalized paths, canonical JSON.
        """
        kind = str(payload.get("approval_kind") or "").strip()
        if not kind:
            return None
        base = str(payload.get("cwd") or cwd or "")
        if kind == "command_execution":
            command = str(payload.get("command") or "").strip()
            if not command:
                return None
            try:
                canonical = json.dumps(shlex.split(command))
            except ValueError:
                canonical = json.dumps([command])
            return cls(kind, f"{_norm_path('', base)}\n{canonical}")
        if kind in {"file_change", "permissions"}:
            paths = [str(p) for p in payload.get("paths") or [] if str(p)]
            if payload.get("path"):
                paths.append(str(payload["path"]))
            paths = sorted({_norm_path(p, base) for p in paths})
            if not paths:
                return None
            return cls(kind, json.dumps(paths))
        name = str(payload.get("tool_name") or "").strip()
        if kind in {"mcp_tool_call", "tool"}:
            if not name:
                return None
            digest = hashlib.sha256(json.dumps(
                payload.get("input"), sort_keys=True, default=str,
                separators=(",", ":")).encode("utf-8")).hexdigest()
            return cls(kind, f"{name}\n{digest}")
        return None


def _norm_path(path: str, base: str) -> str:
    if not path:
        return os.path.normpath(base) if base else ""
    joined = path if os.path.isabs(path) or not base else os.path.join(base, path)
    return os.path.normpath(joined)


def reject_mode_upgrade(current: str | AccessMode, requested: str | AccessMode) -> None:
    """Approvals never raise the access mode; only an explicit mode change may."""
    if _ACCESS_ORDER[AccessMode(requested)] > _ACCESS_ORDER[AccessMode(current)]:
        raise ApprovalScopeError(
            APPROVAL_MODE_UPGRADE_CODE,
            f"an approval cannot change access mode {AccessMode(current).value!r} "
            f"to the broader {AccessMode(requested).value!r}")


class SessionApprovalGrants:
    """Per-session "allow for session" grants scoped to exact request targets.

    ``remember`` stores a grant only for an allow decision with scope
    ``session`` whose request names an exact target; ``covers`` answers only
    for the same tool kind and normalized target.  The set is bound to one
    access mode: ``rebind_access_mode`` clears every grant, so a mode change
    (in either direction) never inherits earlier approvals.

    Engines whose native session/always option is broader than the exact
    target should answer the engine with a one-shot allow (see
    ``native_decision``) and rely on this ledger for repeats.
    """

    def __init__(self, access_mode: str | AccessMode) -> None:
        self._mode = AccessMode(access_mode)
        self._grants: set[ApprovalTarget] = set()
        self._lock = threading.Lock()

    @property
    def access_mode(self) -> AccessMode:
        return self._mode

    def rebind_access_mode(self, access_mode: str | AccessMode) -> None:
        mode = AccessMode(access_mode)
        with self._lock:
            if mode is not self._mode:
                self._grants.clear()
            self._mode = mode

    def remember(
        self, request: Mapping[str, Any], decision: ApprovalDecision, *, cwd: str = ""
    ) -> Optional[ApprovalTarget]:
        if not decision.allowed or decision.scope is not ApprovalScope.SESSION:
            return None
        target = ApprovalTarget.from_payload(request, cwd=cwd)
        if target is None:
            return None
        with self._lock:
            self._grants.add(target)
        return target

    def covers(self, request: Mapping[str, Any], *, cwd: str = "") -> bool:
        target = ApprovalTarget.from_payload(request, cwd=cwd)
        if target is None:
            return False
        with self._lock:
            return target in self._grants

    def clear(self) -> None:
        with self._lock:
            self._grants.clear()


def native_decision(
    request: Mapping[str, Any],
    decision: ApprovalDecision,
    *,
    native_scope_exact: bool,
    cwd: str = "",
) -> ApprovalDecision:
    """Decision to send to the engine for ``decision``.

    A session-scoped allow is forwarded as such only when the engine's own
    session scope is exactly the requested target (``native_scope_exact``)
    and the request names an exact target.  Otherwise it is downgraded to a
    one-shot allow; the adapter records the grant in ``SessionApprovalGrants``.
    """
    if (decision.allowed and decision.scope is ApprovalScope.SESSION
            and not (native_scope_exact
                     and ApprovalTarget.from_payload(request, cwd=cwd) is not None)):
        return ApprovalDecision(
            approval_id=decision.approval_id, choice=decision.choice,
            scope=ApprovalScope.ONCE, note=decision.note,
            option_id=decision.option_id)
    return decision
