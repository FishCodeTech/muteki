"""Optional Conversation CU fixtures for C24 matrix scripts.

Activated only when a Thread title starts with ``C24-FIX-``. Production
Threads are unaffected. Used by computer-use Scripts A–C for issue #16.
"""

from __future__ import annotations

from typing import Any, Optional

from muteki.external_agents.capabilities import SOURCE_PROBE, SOURCE_STATIC
from muteki.external_agents.interaction_matrix import build_interaction_matrix
from muteki.external_agents.runtime_capabilities import (
    RuntimeCapabilityItem,
    RuntimeCapabilitySnapshot,
)
from muteki.platform.contracts.external_agents import AgentCapabilities


def fixture_id_from_title(title: str) -> str:
    text = str(title or "").strip()
    if not text.upper().startswith("C24-FIX-"):
        return ""
    return text.split(maxsplit=1)[0].upper()


def build_cu_fixture(fixture_id: str) -> tuple[
    RuntimeCapabilitySnapshot, dict[str, Any], dict[str, Any],
]:
    """Return (snapshot, matrix_payload, capabilities_bools)."""
    key = fixture_id_from_title(fixture_id) or str(fixture_id or "").upper()
    if key in {"C24-FIX-B", "C24-FIX-UNSUPPORTED"}:
        caps = AgentCapabilities(
            steer=False,
            interrupt=True,
            image_input=False,
            approval=False,
            user_input=False,
            resume=False,
            fork=False,
            capability_source=SOURCE_PROBE,
        )
        sources = {
            "steer": SOURCE_PROBE,
            "interrupt": SOURCE_PROBE,
            "image_input": SOURCE_PROBE,
            "approval": SOURCE_STATIC,
            "user_input": SOURCE_STATIC,
            "resume": SOURCE_PROBE,
            "fork": SOURCE_STATIC,
        }
        snapshot = RuntimeCapabilitySnapshot(
            adapter_id="fixture.b",
            instance_id="default",
            revision=7,
            stale=False,
            items=[
                RuntimeCapabilityItem(
                    id="runtime:fixture.b:command:ok",
                    kind="command",
                    name="ok",
                    description="Verified fixture command",
                    channel="provider_native",
                    origin="dynamic",
                    delivery="guaranteed",
                    verification="verified",
                    invocation={"wire_text": "/ok", "command": "ok"},
                    support_level="supported",
                    invocable=True,
                ),
                RuntimeCapabilityItem(
                    id="runtime:fixture.b:command:draft",
                    kind="command",
                    name="draft",
                    description="Unverified fixture command",
                    channel="provider_native",
                    origin="dynamic",
                    delivery="best_effort",
                    verification="unverified",
                    invocation={"wire_text": "/draft", "command": "draft"},
                    support_level="unknown",
                    reason="该项尚未通过 Runtime 验证，不能当作可调用能力",
                    alternative="请改用已验证的命令，或等待 Runtime 公布确认结果",
                    invocable=False,
                ),
            ],
            diagnostics=["C24 CU fixture B: steer unsupported, approval unknown"],
        )
        matrix = build_interaction_matrix(
            adapter_id="fixture.b",
            capabilities=caps,
            field_sources=sources,
            revision=7,
            stale=False,
            snapshot=snapshot,
            diagnostics=list(snapshot.diagnostics),
        )
        snapshot = snapshot.model_copy(update={"matrix": matrix})
        return snapshot, matrix.model_dump(mode="json"), caps.model_dump(mode="json")

    if key in {"C24-FIX-STALE", "C24-FIX-EXPIRED"}:
        caps = AgentCapabilities(
            steer=True,
            interrupt=True,
            image_input=True,
            approval=True,
            resume=True,
            capability_source=SOURCE_PROBE,
        )
        sources = {name: SOURCE_PROBE for name in (
            "steer", "interrupt", "image_input", "approval", "resume",
        )}
        snapshot = RuntimeCapabilitySnapshot(
            adapter_id="fixture.stale",
            revision=9,
            stale=True,
            items=[
                RuntimeCapabilityItem(
                    id="runtime:fixture.stale:command:old",
                    kind="command",
                    name="old",
                    channel="provider_native",
                    origin="dynamic",
                    delivery="guaranteed",
                    verification="verified",
                    invocation={"wire_text": "/old"},
                ),
            ],
            diagnostics=["C24 CU fixture: stale revision"],
        )
        matrix = build_interaction_matrix(
            adapter_id="fixture.stale",
            capabilities=caps,
            field_sources=sources,
            revision=9,
            stale=True,
            snapshot=snapshot,
            diagnostics=list(snapshot.diagnostics),
        )
        snapshot = snapshot.model_copy(update={"matrix": matrix})
        return snapshot, matrix.model_dump(mode="json"), caps.model_dump(mode="json")

    # Default / FIX-A: fully supported
    caps = AgentCapabilities(
        steer=True,
        interrupt=True,
        image_input=True,
        approval=True,
        user_input=True,
        resume=True,
        fork=True,
        capability_source=SOURCE_PROBE,
    )
    sources = {name: SOURCE_PROBE for name in (
        "steer", "interrupt", "image_input", "approval", "user_input",
        "resume", "fork",
    )}
    snapshot = RuntimeCapabilitySnapshot(
        adapter_id="fixture.a",
        revision=3,
        stale=False,
        items=[
            RuntimeCapabilityItem(
                id="runtime:fixture.a:command:status",
                kind="command",
                name="status",
                description="Verified status command",
                channel="provider_native",
                origin="dynamic",
                delivery="guaranteed",
                verification="verified",
                invocation={"wire_text": "/status", "command": "status"},
                support_level="supported",
                invocable=True,
            ),
        ],
        diagnostics=["C24 CU fixture A: all key rows supported"],
    )
    matrix = build_interaction_matrix(
        adapter_id="fixture.a",
        capabilities=caps,
        field_sources=sources,
        revision=3,
        stale=False,
        snapshot=snapshot,
        diagnostics=list(snapshot.diagnostics),
    )
    snapshot = snapshot.model_copy(update={"matrix": matrix})
    return snapshot, matrix.model_dump(mode="json"), caps.model_dump(mode="json")


def apply_cu_fixture_to_runtime_connection(
    title: str,
    runtime_connection: dict[str, Any],
) -> Optional[RuntimeCapabilitySnapshot]:
    fixture_id = fixture_id_from_title(title)
    if not fixture_id:
        return None
    snapshot, matrix_payload, caps = build_cu_fixture(fixture_id)
    runtime_connection["capabilities"] = caps
    runtime_connection["capability_revision"] = snapshot.revision
    runtime_connection["capability_stale"] = snapshot.stale
    runtime_connection["matrix"] = matrix_payload
    runtime_connection["matrix_diagnostics"] = list(
        matrix_payload.get("diagnostics") or []
    )
    runtime_connection["configured"] = True
    runtime_connection["connected"] = True
    runtime_connection["session_state"] = "active"
    runtime_connection["degradation"] = ""
    return snapshot


__all__ = [
    "apply_cu_fixture_to_runtime_connection",
    "build_cu_fixture",
    "fixture_id_from_title",
]
