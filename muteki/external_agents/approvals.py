"""Canonical approval messages shared by Conversation and Agent adapters."""

from __future__ import annotations

from dataclasses import dataclass, field
from enum import Enum
from typing import Any, Mapping


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
        }

    def codex_decision(self) -> str:
        if not self.allowed:
            return "decline"
        if self.scope is ApprovalScope.SESSION:
            return "acceptForSession"
        return "accept"
