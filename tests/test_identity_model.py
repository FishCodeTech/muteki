"""当前 Credential/Seat 模型的边界测试。

只覆盖仍在生产路径中使用的迁移、适配和引用解析行为。
"""

from __future__ import annotations

from muteki.solver.identity_model import (
    Credential,
    Seat,
    credential_id_for,
    is_legal_combo,
    kind_from_mode,
    migrate_legacy_config,
    seat_id_for,
    seat_to_legacy_profile,
    seats_to_legacy_profiles,
)
from muteki.solver.worker_profiles import resolve_seat_ref


def test_generated_ids_are_stable_and_already_migrated_ids_are_preserved() -> None:
    seat_id = seat_id_for("claude", legacy_name="claude-main")
    credential_id = credential_id_for("claude", legacy_account_id="claude-main")

    assert seat_id == seat_id_for("claude", legacy_name="claude-main")
    assert credential_id == credential_id_for(
        "claude", legacy_account_id="claude-main"
    )
    assert seat_id_for("claude", legacy_name=seat_id) == seat_id
    assert credential_id_for(
        "claude", legacy_account_id=credential_id
    ) == credential_id


def test_credential_modes_and_backend_legality_match_current_model() -> None:
    assert kind_from_mode("subscription_token") == "engine_key"
    assert kind_from_mode("chatgpt_auth_home") == "engine_key"
    assert kind_from_mode("custom_endpoint") == "custom_endpoint"
    assert is_legal_combo(kind="system_inherit", backend="local")
    assert not is_legal_combo(kind="system_inherit", backend="container")
    assert is_legal_combo(kind="engine_key", backend="container")


def test_migration_uses_present_default_account_for_empty_binding() -> None:
    result = migrate_legacy_config(
        worker_profiles=[{
            "id": "codex-local",
            "name": "codex-local",
            "engine": "codex",
            "credential_mode": "subscription",
            "credential_account": "",
            "model": "gpt-5.4",
        }],
        account_modes={"codex-main": "chatgpt_auth_home"},
    )

    credential = result.credentials[0]
    seat = result.seats[0]
    assert credential.kind == "engine_key"
    assert credential.secret_ref == "codex-main"
    assert seat.credential_id == credential.id
    assert seat.model == "gpt-5.4"
    assert result.credential_alias["codex-main"] == credential.id


def test_migration_without_stored_account_uses_host_login() -> None:
    result = migrate_legacy_config(
        worker_profiles=[{
            "id": "claude-local",
            "name": "claude-local",
            "engine": "claude",
            "credential_mode": "subscription",
            "credential_account": "",
        }],
        account_modes={},
    )

    assert result.credentials[0].kind == "system_inherit"
    assert result.credentials[0].secret_ref == ""


def test_migration_preserves_current_scheduling_fields_and_skips_invalid_rows() -> None:
    result = migrate_legacy_config(
        worker_profiles=[
            None,
            {"engine": "unknown", "name": "invalid"},
            {
                "id": "claude-review",
                "name": "claude-review",
                "label": "Claude Reviewer",
                "engine": "claude",
                "credential_account": "claude-main",
                "roles": ["review"],
                "race": False,
                "max_running": 3,
                "max_review_running": 1,
                "priority": 7,
                "reasoning_effort": "high",
                "enabled": True,
            },
        ],
        account_modes={"claude-main": "subscription_token"},
    )

    assert len(result.seats) == 1
    seat = result.seats[0]
    assert seat.label == "Claude Reviewer"
    assert seat.roles == ["review"]
    assert seat.race is False
    assert seat.max_running == 3
    assert seat.max_review_running == 1
    assert seat.priority == 7
    assert seat.reasoning_effort == "high"


def test_custom_endpoint_round_trips_into_driver_profile() -> None:
    credential = Credential(
        id="cred_codex_endpoint",
        label="Codex Endpoint",
        engine="codex",
        kind="custom_endpoint",
        secret_ref="codex-endpoint",
        target_engine="codex",
        base_url="https://example.test/v1",
        wire_api="responses",
    ).to_dict()
    seat = Seat(
        id="seat_codex_endpoint",
        label="Codex Endpoint",
        engine="codex",
        credential_id=credential["id"],
        model="custom-model",
        roles=["race", "explore"],
        max_running=2,
    ).to_dict()

    profile = seat_to_legacy_profile(seat, credential)
    assert profile["id"] == seat["id"]
    assert profile["credential_account"] == "codex-endpoint"
    assert profile["base_url"] == "https://example.test/v1"
    assert profile["wire_api"] == "responses"
    assert profile["model"] == "custom-model"
    assert profile["max_running"] == 2


def test_system_login_adapter_does_not_emit_a_stored_account() -> None:
    profile = seat_to_legacy_profile(
        Seat(
            id="seat_claude_host",
            label="Claude Host",
            engine="claude",
            credential_id="cred_claude_host",
        ).to_dict(),
        Credential(
            id="cred_claude_host",
            label="Host Login",
            engine="claude",
            kind="system_inherit",
        ).to_dict(),
    )

    assert profile["credential_account"] == ""
    assert profile["credential_mode"] == "subscription"


def test_batch_adapter_keeps_one_profile_per_seat() -> None:
    credentials = [{
        "id": "cred_claude_main",
        "label": "Claude Main",
        "engine": "claude",
        "kind": "engine_key",
        "secret_ref": "claude-main",
    }]
    seats = [{
        "id": "seat_claude_main",
        "label": "Claude Main",
        "engine": "claude",
        "credential_id": "cred_claude_main",
        "roles": ["race"],
        "capacity": {"max_running": 2, "max_review_running": 0},
    }]

    profiles = seats_to_legacy_profiles(seats, credentials)
    assert [profile["id"] for profile in profiles] == ["seat_claude_main"]
    assert profiles[0]["credential_account"] == "claude-main"


def test_seat_reference_resolution_handles_current_and_legacy_forms() -> None:
    seats = [{
        "id": "seat_claude_ab12cd",
        "label": "claude-main",
        "engine": "claude",
    }]
    aliases = {"claude-sub-container": "seat_claude_ab12cd"}

    for reference in (
        "seat_claude_ab12cd",
        "claude-main",
        "claude-sub-container",
        "claude",
    ):
        assert resolve_seat_ref(
            reference, seats=seats, alias_table=aliases
        ) == "seat_claude_ab12cd"
    assert resolve_seat_ref("missing", seats=seats, alias_table=aliases) is None


def test_bare_engine_reference_is_rejected_when_multiple_seats_match() -> None:
    seats = [
        {"id": "seat_claude_a", "label": "A", "engine": "claude"},
        {"id": "seat_claude_b", "label": "B", "engine": "claude"},
    ]

    assert resolve_seat_ref("claude", seats=seats) is None
