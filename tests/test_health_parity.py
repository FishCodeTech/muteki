"""调度预检与设置页健康检查的后端和探测深度测试。"""

from __future__ import annotations

import pytest

from apps.web.worker_config import (
    DEFAULT_WORKER_BACKEND,
    WorkerConfigStore,
    backend_for_profile,
    resolve_worker_backend,
)


@pytest.mark.parametrize(
    ("kwargs", "expected"),
    [
        ({"request_backend": "local", "config_backend": "container"}, "local"),
        ({"config_backend": "local", "env_backend": "container"}, "local"),
        ({"env_backend": "container"}, "container"),
        ({}, DEFAULT_WORKER_BACKEND),
        ({"request_backend": "container_dockerexec"}, "container"),
        ({"request_backend": "invalid"}, "local"),
    ],
)
def test_backend_resolution_precedence_and_normalization(kwargs, expected) -> None:
    assert resolve_worker_backend(
        **kwargs, in_web_container=False
    ) == expected


def test_web_container_forces_global_container_backend() -> None:
    assert resolve_worker_backend(
        config_backend="local", in_web_container=True
    ) == "container"
    assert backend_for_profile(
        worker_backend="local", in_web_container=True
    ) == "container"


def _capture_health_calls(monkeypatch):
    from muteki.solver.profile_health import ProfileHealth

    calls: list[tuple[str, str, str]] = []

    def fake(profile, *, backend, sessions_root, depth="auth"):
        profile_id = str(profile.get("id") or profile.get("name"))
        calls.append((profile_id, backend, depth))
        return ProfileHealth(
            profile_id=profile_id,
            engine=str(profile.get("engine") or ""),
            backend=backend,
            status="ok",
            layer=None,
            blocker=None,
            detail="ok",
            model=str(profile.get("model") or ""),
            account_id=str(profile.get("credential_account") or ""),
        )

    monkeypatch.setattr(
        "muteki.solver.profile_health.evaluate_profile_health", fake
    )
    return calls


def test_dispatch_checks_enabled_profiles_with_global_backend(tmp_path, monkeypatch) -> None:
    from apps.web.drivers import _missing_profile_accounts

    monkeypatch.setattr("apps.web.drivers.is_web_container", lambda: False)
    calls = _capture_health_calls(monkeypatch)
    profiles = [
        {
            "id": "claude-main",
            "name": "claude-main",
            "engine": "claude",
            "credential_account": "claude-main",
            "enabled": True,
        },
        {
            "id": "codex-disabled",
            "name": "codex-disabled",
            "engine": "codex",
            "credential_account": "codex-main",
            "enabled": False,
        },
    ]

    assert _missing_profile_accounts(
        worker_profiles=profiles,
        worker_backend="container",
        sessions_root=tmp_path,
    ) == []
    assert calls == [("claude-main", "container", "auth")]


def test_settings_health_routes_use_binding_and_auth_depths(tmp_path, monkeypatch) -> None:
    from starlette.testclient import TestClient

    from apps.web.run_manager import RunManager
    from apps.web.server import create_app

    monkeypatch.setattr("muteki.core.runtime_env.is_web_container", lambda: False)
    monkeypatch.delenv("MUTEKI_WEB_PASSWORD", raising=False)
    calls = _capture_health_calls(monkeypatch)

    store = WorkerConfigStore(tmp_path)
    store.set_identity_model(
        seats=[{
            "id": "seat_claude_ab12cd",
            "label": "claude-main",
            "engine": "claude",
            "credential_id": "cred_claude_ab12cd",
            "roles": ["race", "review"],
            "enabled": True,
        }],
        credentials=[{
            "id": "cred_claude_ab12cd",
            "label": "claude-main",
            "engine": "claude",
            "kind": "engine_key",
            "secret_ref": "claude-main",
        }],
    )
    config = store.get()
    legacy_name = "claude-main"
    seat_id = "seat_claude_ab12cd"

    app = create_app(RunManager(sessions_root=str(tmp_path)))
    with TestClient(app) as client:
        batch = client.get("/api/settings/profiles/health")
        single = client.post(f"/api/settings/profiles/{legacy_name}/health")
        by_seat = client.post(f"/api/settings/profiles/{seat_id}/health")
        missing = client.post("/api/settings/profiles/missing/health")

    assert batch.status_code == 200
    assert single.status_code == 200
    assert by_seat.status_code == 200
    assert missing.status_code == 404
    depths = [depth for _profile_id, _backend, depth in calls]
    assert depths.count("binding") == len(config["worker_profiles"])
    assert depths.count("auth") == 2
    assert {backend for _profile_id, backend, _depth in calls} == {"container"}
