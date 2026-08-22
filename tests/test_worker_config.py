"""Worker 配置当前存储模型与调度投影的关键测试。"""

from __future__ import annotations

import json

import pytest

from apps.web.worker_config import (
    DEFAULT_ENGINES,
    DEFAULT_WORKER_PROFILES,
    VALID_ENGINES,
    WorkerConfigStore,
)


def _credential(engine: str, *, kind: str = "engine_key") -> dict:
    return {
        "id": f"cred_{engine}_main",
        "label": f"{engine} main",
        "engine": engine,
        "kind": kind,
        "secret_ref": "" if kind == "system_inherit" else f"{engine}-main",
    }


def _seat(
    engine: str,
    *,
    enabled: bool = True,
    roles: list[str] | None = None,
    max_running: int = 1,
) -> dict:
    return {
        "id": f"seat_{engine}_main",
        "label": f"{engine} main",
        "engine": engine,
        "credential_id": f"cred_{engine}_main",
        "model": "",
        "reasoning_effort": "default",
        "roles": roles or ["race", "bootstrap", "explore"],
        "race": True,
        "capacity": {
            "max_running": max_running,
            "max_review_running": 0,
        },
        "priority": 10,
        "enabled": enabled,
    }


def test_empty_store_returns_current_global_runtime_defaults(tmp_path) -> None:
    config = WorkerConfigStore(tmp_path).get()

    assert config["engines"] == DEFAULT_ENGINES
    assert config["worker_backend"] == "container"
    assert config["worker_network"] == "bridge"
    assert config["worker_profiles"] == DEFAULT_WORKER_PROFILES
    assert len(config["seats"]) == len(DEFAULT_WORKER_PROFILES)
    assert len(config["credentials"]) == len(DEFAULT_WORKER_PROFILES)
    assert "runtime_profiles" not in config
    assert "environments" not in config
    assert all("runtime" not in profile for profile in config["worker_profiles"])


def test_supported_engine_set_contains_all_current_worker_engines() -> None:
    assert set(VALID_ENGINES) == {
        "claude",
        "codex",
        "cursor",
        "pi",
        "omp",
        "kimi",
        "grok",
        "opencode",
        "dsh",
    }


def test_legacy_base_engine_refs_resolve_to_current_profiles(tmp_path) -> None:
    config = WorkerConfigStore(tmp_path).set(
        engines=["claude", "bogus", "codex", "claude"]
    )

    assert config["engines"] == [
        "claude-sub-container",
        "codex-sub-container",
    ]


def test_roster_capacity_derives_max_workers_without_mutating_profiles(tmp_path) -> None:
    profiles = [
        {**profile, "max_running": index + 1}
        for index, profile in enumerate(DEFAULT_WORKER_PROFILES[:3])
    ]
    config = WorkerConfigStore(tmp_path).set(
        worker_profiles=profiles,
        engines=[profile["name"] for profile in profiles],
        max_workers=99,
    )

    assert config["max_workers"] == 6
    assert [profile["max_running"] for profile in config["worker_profiles"]] == [
        1,
        2,
        3,
    ]


def test_identity_projection_uses_enabled_ordinary_seats_only(tmp_path) -> None:
    store = WorkerConfigStore(tmp_path)
    config = store.set_identity_model(
        seats=[
            _seat("claude", max_running=2),
            _seat("codex", enabled=False),
            _seat("cursor", roles=["review"], max_running=9),
        ],
        credentials=[
            _credential("claude"),
            _credential("codex"),
            _credential("cursor"),
        ],
    )

    assert config["engines"] == ["seat_claude_main"]
    assert {profile["id"] for profile in config["worker_profiles"]} == {
        "seat_claude_main",
        "seat_cursor_main",
    }
    assert {seat["id"] for seat in config["seats"]} == {
        "seat_claude_main",
        "seat_codex_main",
        "seat_cursor_main",
    }


def test_identity_model_round_trips_without_environment_fields(tmp_path) -> None:
    store = WorkerConfigStore(tmp_path)
    store.set_identity_model(
        seats=[_seat("claude")],
        credentials=[_credential("claude")],
    )

    reloaded = WorkerConfigStore(tmp_path).get()
    assert reloaded["seats"] == [_seat("claude")]
    assert reloaded["credentials"] == [_credential("claude")]
    assert reloaded["seat_alias"]["claude main"] == "seat_claude_main"
    on_disk = json.loads((tmp_path / "_worker_config.json").read_text())
    assert "environments" not in on_disk
    assert "runtime_profiles" not in on_disk


def test_legacy_runtime_fields_are_removed_during_load(tmp_path) -> None:
    (tmp_path / "_worker_config.json").write_text(
        json.dumps({
            "worker_backend": "container",
            "runtime_profiles": [{"id": "docker-web", "backend": "container"}],
            "environments": [{"id": "docker-web", "backend": "container"}],
            "worker_profiles": [{
                **DEFAULT_WORKER_PROFILES[0],
                "runtime": "docker-web",
            }],
        })
    )

    config = WorkerConfigStore(tmp_path).get()
    assert "runtime_profiles" not in config
    assert "environments" not in config
    assert "runtime" not in config["worker_profiles"][0]


def test_container_backend_rejects_enabled_host_login_seat(tmp_path) -> None:
    store = WorkerConfigStore(tmp_path)

    with pytest.raises(ValueError, match="系统登录"):
        store.set_identity_model(
            seats=[_seat("claude")],
            credentials=[_credential("claude", kind="system_inherit")],
        )


def test_disabled_host_login_seat_can_remain_in_container_config(tmp_path) -> None:
    config = WorkerConfigStore(tmp_path).set_identity_model(
        seats=[_seat("claude", enabled=False), _seat("codex")],
        credentials=[
            _credential("claude", kind="system_inherit"),
            _credential("codex"),
        ],
    )

    assert next(
        seat for seat in config["seats"] if seat["id"] == "seat_claude_main"
    )["enabled"] is False
    assert config["engines"] == ["seat_codex_main"]


def test_atomic_configuration_validates_the_final_backend(tmp_path) -> None:
    store = WorkerConfigStore(tmp_path)
    seat = _seat("claude")
    credential = _credential("claude", kind="system_inherit")

    config = store.set_configuration(
        seats=[seat],
        credentials=[credential],
        worker_backend="local",
        engines=[seat["id"]],
        race_engines=[seat["id"]],
        stage_policy={
            "coordinator": {
                "review": {"enabled": False, "engine": seat["id"]},
                "verifier": {"enabled": False, "engine": ""},
            },
        },
    )

    assert config["worker_backend"] == "local"
    assert config["engines"] == [seat["id"]]
    assert config["stage_policy"]["coordinator"]["review"]["engine"] == seat["id"]

    with pytest.raises(ValueError, match="系统登录"):
        store.set_configuration(
            seats=[seat],
            credentials=[credential],
            worker_backend="container",
            engines=[seat["id"]],
        )

    reloaded = WorkerConfigStore(tmp_path).get()
    assert reloaded["worker_backend"] == "local"
    assert reloaded["credentials"] == [credential]


def test_backend_network_and_budget_settings_persist(tmp_path, monkeypatch) -> None:
    monkeypatch.setattr("apps.web.worker_config.is_web_container", lambda: False)
    WorkerConfigStore(tmp_path).set(
        worker_backend="local",
        worker_network="none",
        wall_clock_budget=900,
        max_total_workers=8,
        cost_budget_usd=2.5,
    )

    config = WorkerConfigStore(tmp_path).get()
    assert config["worker_backend"] == "local"
    assert config["worker_network"] == "none"
    assert config["wall_clock_budget"] == 900
    assert config["max_total_workers"] == 8
    assert config["cost_budget_usd"] == 2.5


@pytest.mark.parametrize(
    ("field", "value"),
    [
        ("worker_backend", "remote"),
        ("worker_network", "public"),
        ("start_workers", 0),
        ("race_timeout", 0),
        ("wall_clock_budget", -1),
        ("cost_budget_usd", -0.1),
    ],
)
def test_invalid_operator_settings_are_rejected(tmp_path, field, value) -> None:
    with pytest.raises(ValueError):
        WorkerConfigStore(tmp_path).set(**{field: value})


def test_category_override_changes_only_the_effective_roster(tmp_path) -> None:
    store = WorkerConfigStore(tmp_path)
    store.set(
        overrides={
            "pwn": {"engines": ["claude", "codex"], "start_workers": 1}
        }
    )

    default = store.resolve("web")
    pwn = store.resolve("pwn")
    assert default["engines"] == DEFAULT_ENGINES
    assert pwn["engines"] == [
        "claude-sub-container",
        "codex-sub-container",
    ]
    assert pwn["start_workers"] == 1
