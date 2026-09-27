"""#189: interaction_matrix_for must match full Runtime identity on cache hit."""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import MagicMock

from muteki.conversation.executor import ExternalAgentSessionExecutor
from muteki.conversation.store import ConversationStore
from muteki.external_agents.interaction_matrix import (
    InteractionCapabilityMatrix,
    InteractionCapabilityRow,
    build_matrix_from_probe,
)
from muteki.external_agents.runtime_capabilities import RuntimeCapabilitySnapshot
from muteki.platform.store import PlatformStore


def _executor(tmp: str, *, registry: object) -> ExternalAgentSessionExecutor:
    store = PlatformStore(Path(tmp) / "platform.db")
    conv = ConversationStore(store)
    manager = SimpleNamespace(
        runtime_selection=lambda _tid: SimpleNamespace(
            adapter_id="codex.app_server",
            instance_id="default",
            runtime_key="codex.app_server:default",
            session_key="codex.app_server:default|",
            credential_id="",
            model="",
            effort="",
            access_mode="",
            permission_mode="",
            sandbox_mode="",
        ),
        bindings=None,
    )
    return ExternalAgentSessionExecutor(
        store=store,
        conv=conv,
        manager=manager,
        registry=registry,
        sessions_root=Path(tmp),
    )


def _confirmed_matrix(
    *,
    adapter_id: str,
    instance_id: str = "default",
    revision: int = 2,
) -> InteractionCapabilityMatrix:
    """A matrix that falsely claims attachments are supported (confirmed)."""
    return InteractionCapabilityMatrix(
        adapter_id=adapter_id,
        instance_id=instance_id,
        revision=revision,
        stale=False,
        rows=[
            InteractionCapabilityRow(
                key="attachments",
                level="supported",
                reason="confirmed by probe",
                source="probe",
                invocable=True,
            ),
            InteractionCapabilityRow(
                key="approval",
                level="supported",
                reason="confirmed by probe",
                source="probe",
                invocable=True,
            ),
        ],
        diagnostics=[],
    )


def _snapshot(
    *,
    adapter_id: str,
    instance_id: str = "default",
    revision: int = 2,
) -> RuntimeCapabilitySnapshot:
    snap = RuntimeCapabilitySnapshot(
        adapter_id=adapter_id,
        instance_id=instance_id,
        revision=revision,
        stale=False,
        diagnostics=[],
    )
    return snap.model_copy(
        update={"matrix": _confirmed_matrix(
            adapter_id=adapter_id,
            instance_id=instance_id,
            revision=revision,
        )},
    )


class ProviderMatrixCacheIdentityTest(unittest.TestCase):
    def test_provider_switch_returns_requested_identity_not_cached(self) -> None:
        registry = MagicMock()
        registry.record.return_value = SimpleNamespace(last_probe=None)

        with tempfile.TemporaryDirectory() as tmp:
            executor = _executor(tmp, registry=registry)
            thread_id = "thr-189-switch"
            executor._capability_cache[thread_id] = _snapshot(
                adapter_id="codex.app_server",
            )

            payload = executor.interaction_matrix_for(
                thread_id,
                adapter_id="claude.agent_sdk",
                instance_id="default",
            )

            self.assertEqual(payload.get("adapter_id"), "claude.agent_sdk")
            self.assertEqual(payload.get("instance_id"), "default")
            self.assertNotEqual(payload.get("adapter_id"), "codex.app_server")
            rows = {
                str(row.get("key")): row
                for row in (payload.get("rows") or [])
                if isinstance(row, dict)
            }
            attachments = rows.get("attachments") or {}
            self.assertNotEqual(
                attachments.get("level"),
                "supported",
                "old Provider confirmed caps must not appear as new Provider supported",
            )

    def test_cache_hit_when_full_identity_matches(self) -> None:
        registry = MagicMock()
        registry.record.return_value = SimpleNamespace(last_probe=None)

        with tempfile.TemporaryDirectory() as tmp:
            executor = _executor(tmp, registry=registry)
            thread_id = "thr-189-hit"
            cached = _snapshot(adapter_id="codex.app_server", revision=5)
            executor._capability_cache[thread_id] = cached

            payload = executor.interaction_matrix_for(
                thread_id,
                adapter_id="codex.app_server",
                instance_id="default",
            )

            self.assertEqual(payload.get("adapter_id"), "codex.app_server")
            self.assertEqual(payload.get("revision"), 5)
            rows = {
                str(row.get("key")): row
                for row in (payload.get("rows") or [])
                if isinstance(row, dict)
            }
            self.assertEqual(rows["attachments"]["level"], "supported")

    def test_same_adapter_different_instance_isolated(self) -> None:
        registry = MagicMock()
        registry.record.return_value = SimpleNamespace(last_probe=None)

        with tempfile.TemporaryDirectory() as tmp:
            executor = _executor(tmp, registry=registry)
            thread_id = "thr-189-instance"
            executor._capability_cache[thread_id] = _snapshot(
                adapter_id="codex.app_server",
                instance_id="work",
                revision=7,
            )

            # Request same adapter, different instance → must not reuse cache.
            payload = executor.interaction_matrix_for(
                thread_id,
                adapter_id="codex.app_server",
                instance_id="personal",
            )

            self.assertEqual(payload.get("adapter_id"), "codex.app_server")
            self.assertEqual(payload.get("instance_id"), "personal")
            self.assertNotEqual(payload.get("revision"), 7)
            rows = {
                str(row.get("key")): row
                for row in (payload.get("rows") or [])
                if isinstance(row, dict)
            }
            attachments = rows.get("attachments") or {}
            self.assertNotEqual(attachments.get("level"), "supported")

            # Matching instance still hits cache.
            hit = executor.interaction_matrix_for(
                thread_id,
                adapter_id="codex.app_server",
                instance_id="work",
            )
            self.assertEqual(hit.get("instance_id"), "work")
            self.assertEqual(hit.get("revision"), 7)


if __name__ == "__main__":
    unittest.main()
