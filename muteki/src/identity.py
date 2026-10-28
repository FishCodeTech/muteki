from __future__ import annotations

from typing import Any, Optional

from .contracts import IdentityVerification
from .store import SrcStore, SrcStoreError

_SECRET_PREFIXES = ("sk-", "ghp_", "gho_", "xox", "-----BEGIN")


def _credential_reference(value: str) -> str:
    text = str(value or "").strip()
    if not text:
        return ""
    if any(text.startswith(prefix) for prefix in _SECRET_PREFIXES) or any(char.isspace() for char in text):
        raise SrcStoreError("policy_denied", "credential_ref must be a managed reference, not a credential")
    return text


class SrcIdentityService:
    """Named Task identities with evidence-backed authentication verification."""

    def __init__(self, store: SrcStore) -> None:
        self.store = store

    def ensure_identity(
        self, task_id: str, context_id: str, *, account_ref: str = "",
        role: str = "", tenant: str = "", credential_ref: str = "",
        expected_revision: Optional[int] = None,
        success_conditions: Optional[dict[str, Any]] = None,
    ) -> dict[str, Any]:
        if not context_id:
            raise SrcStoreError("task_binding_invalid", "context_id is required")
        existing = self.store.by_natural_key(task_id, "browser_identity", context_id)
        if existing and expected_revision is None:
            raise SrcStoreError("version_conflict", "identity update requires expected_revision")
        if existing and expected_revision != existing.revision:
            raise SrcStoreError("version_conflict", "identity revision is stale")
        old_conditions = existing.payload.get("success_conditions") if existing else None
        conditions = dict(success_conditions if success_conditions is not None else old_conditions or {})
        condition_version = int(existing.payload.get("condition_version") or 0) if existing else 0
        if conditions != old_conditions:
            condition_version += 1
        config_revision = int(existing.payload.get("config_revision") or existing.revision) + 1 if existing else 1
        payload = {
            "context_id": context_id, "account_ref": account_ref,
            "role": role, "tenant": tenant,
            "credential_ref": _credential_reference(credential_ref),
            "success_conditions": conditions, "condition_version": condition_version,
            "config_revision": config_revision,
            "session_version": int(existing.payload.get("session_version") or 0) + 1 if existing else 1,
            "last_verification": existing.payload.get("last_verification") if existing else None,
        }
        record = self.store.put(
            task_id, "browser_identity", payload,
            record_id=existing.record_id if existing else f"{SrcStore.digest(task_id)[:24]}:{context_id}",
            expected_revision=existing.revision if existing else None,
            natural_key=context_id,
        )
        return record.model_dump(mode="json")

    def identity(self, task_id: str, context_id: str) -> Any:
        record = self.store.by_natural_key(task_id, "browser_identity", context_id)
        if record is None:
            raise SrcStoreError("identity_unverified", "identity context has no account mapping")
        return record

    @staticmethod
    def _diagnosis(observed: dict[str, Any]) -> Optional[str]:
        for key, status in (
            ("network_error", "network_error"), ("challenge", "challenge"),
            ("locked", "locked"), ("expired", "expired"),
            ("anonymous", "anonymous"), ("forbidden", "forbidden"),
        ):
            if observed.get(key):
                return status
        return None

    def verify_identity(
        self, task_id: str, context_id: str, *,
        browser_generation: int, observed: dict[str, Any],
        evidence_refs: list[dict[str, Any]],
        expected: Optional[dict[str, Any]] = None,
        condition_version: Optional[int] = None,
    ) -> IdentityVerification:
        identity = self.identity(task_id, context_id)
        expected_conditions = dict(identity.payload["success_conditions"])
        if expected:
            if any(expected_conditions.get(key) != value for key, value in expected.items()):
                raise SrcStoreError(
                    "authorization_stale",
                    "identity success conditions must be revised before verification",
                )
            expected_conditions.update(expected)
        actual_condition_version = int(condition_version or identity.payload["condition_version"])
        if actual_condition_version != identity.payload["condition_version"]:
            raise SrcStoreError("authorization_stale", "identity success condition version is stale")
        diagnosis = self._diagnosis(observed)
        status = diagnosis or "identity_unverified"
        if diagnosis is None:
            account_ref = str(identity.payload.get("account_ref") or "")
            tenant_ref = str(identity.payload.get("tenant") or "")
            role_ref = str(identity.payload.get("role") or "")
            effective_expected = dict(expected_conditions)
            if account_ref and "account" not in effective_expected:
                effective_expected["account"] = account_ref
            if tenant_ref and "tenant" not in effective_expected:
                effective_expected["tenant"] = tenant_ref
            if role_ref and "role" not in effective_expected:
                effective_expected["role"] = role_ref
            mappings_match = all(
                key not in effective_expected
                or observed.get(key) == effective_expected[key]
                for key in ("account", "tenant", "role")
            )
            if account_ref and observed.get("account") not in {None, ""} and str(observed.get("account")) != account_ref:
                mappings_match = False
            protected_source = observed.get("protected_resource")
            mapped = bool(
                account_ref
                or any(effective_expected.get(key) for key in ("account", "tenant", "role"))
            )
            verified = (
                mapped and mappings_match
                and observed.get("session_valid") is True
                and protected_source in {"identity_endpoint", "protected_page", "subsequent_request"}
                and bool(evidence_refs)
            )
            status = "verified" if verified else "identity_unverified"
        config_revision = int(identity.payload.get("config_revision") or identity.revision)
        verification = IdentityVerification(
            task_id=task_id, context_id=context_id,
            identity_revision=config_revision,
            browser_generation=browser_generation,
            condition_version=actual_condition_version,
            expected=expected_conditions, observed=dict(observed),
            status=status, evidence_refs=[
                item if isinstance(item, str) else self.store._json(item)
                for item in evidence_refs
            ],
        )
        self.store.put(
            task_id, "identity_verification", verification.model_dump(mode="json"),
            record_id=verification.verification_id, parent_id=context_id,
        )
        current = self.store.require(task_id, "browser_identity", identity.record_id)
        self.store.put(
            task_id, "browser_identity", {
                **current.payload,
                "last_verification": verification.model_dump(mode="json"),
            }, record_id=current.record_id, expected_revision=current.revision,
            natural_key=context_id,
        )
        return verification

    def assert_verified(
        self, task_id: str, context_id: str, *, browser_generation: int,
    ) -> IdentityVerification:
        identity = self.identity(task_id, context_id)
        verification = identity.payload.get("last_verification")
        if not verification:
            raise SrcStoreError("identity_unverified", "identity has not been verified")
        try:
            parsed = IdentityVerification.model_validate(verification)
        except ValueError as exc:
            raise SrcStoreError("identity_unverified", "identity verification is unreadable") from exc
        if (
            parsed.status != "verified"
            or parsed.browser_generation != browser_generation
            or parsed.condition_version != identity.payload["condition_version"]
            or parsed.identity_revision != int(identity.payload.get("config_revision") or identity.revision)
        ):
            raise SrcStoreError(
                "identity_unverified",
                "identity verification is absent or stale for this browser generation",
            )
        return parsed

    def recover_identity(
        self, task_id: str, context_id: str, *,
        profile_readable: bool, live_session_observed: bool,
    ) -> dict[str, Any]:
        identity = self.identity(task_id, context_id)
        if not profile_readable:
            state = "profile_unavailable"
        elif not live_session_observed:
            state = "session_recreated"
        else:
            state = "verification_required"
        result = {
            "context_id": context_id, "recovery": state,
            "identity_revision": identity.revision,
            "verification_required": True,
            "session_storage_restored": False,
        }
        self.store.put(
            task_id, "identity_recovery", dict(result), parent_id=context_id,
        )
        return result

    def compare_identities(
        self, task_id: str, left_context: str, right_context: str, *,
        left_generation: int, right_generation: int,
    ) -> dict[str, Any]:
        if left_context == right_context:
            raise SrcStoreError("identity_unverified", "A/B comparison requires different identity contexts")
        left = self.assert_verified(task_id, left_context, browser_generation=left_generation)
        right = self.assert_verified(task_id, right_context, browser_generation=right_generation)
        return {
            "ready": True, "left": left.model_dump(mode="json"),
            "right": right.model_dump(mode="json"),
            "credentials_shared": False,
            "same_account": (
                left.expected.get("account") == right.expected.get("account")
                and left.expected.get("tenant") == right.expected.get("tenant")
            ),
        }

    def verifications(self, task_id: str, context_id: str) -> list[dict[str, Any]]:
        return [
            row.model_dump(mode="json")
            for row in self.store.all(task_id, "identity_verification")
            if row.parent_id == context_id
        ]


__all__ = ["SrcIdentityService"]
