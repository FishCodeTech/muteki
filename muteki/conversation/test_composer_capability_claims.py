"""#120: composer capability claims must match real gateway injection."""

from __future__ import annotations

from muteki.capability_management import configure, set_enabled
from muteki.conversation.composer_capabilities import (
    discover_skills,
    engine_receives_capability_gateway,
    resolve_capability_refs,
    resolve_composer_catalog,
)
from pathlib import Path
from tempfile import TemporaryDirectory


def _enable_muteki_control(tmp: Path) -> None:
    configure(tmp / "capability_management.json")
    set_enabled("mcp", "muteki-control", True)


def test_engine_gateway_matrix():
    assert engine_receives_capability_gateway("codex") is True
    assert engine_receives_capability_gateway("claude") is True
    assert engine_receives_capability_gateway("devin") is False
    assert engine_receives_capability_gateway("") is False


def test_at_menu_hides_injected_claim_for_devin():
    with TemporaryDirectory() as tmp:
        _enable_muteki_control(Path(tmp))
        items = resolve_composer_catalog(engine="devin", trigger="@", query="muteki")
        control = next(item for item in items if item["id"] == "mcp:muteki-control")
        assert control["support_level"] == "unsupported"
        assert control["invocable"] is False
        assert control["status"] == "unavailable"
        assert "不会注入" in control["reason"]
        assert control["alternative"]
        assert "由 Muteki 注入" not in control["description"]


def test_at_menu_keeps_injected_claim_for_codex():
    with TemporaryDirectory() as tmp:
        _enable_muteki_control(Path(tmp))
        items = resolve_composer_catalog(engine="codex", trigger="@", query="muteki")
        control = next(item for item in items if item["id"] == "mcp:muteki-control")
        assert control["support_level"] == "supported"
        assert control["invocable"] is True
        assert control["status"] == "available"
        assert "由 Muteki 注入" in control["description"]


def test_dollar_menu_marks_plugin_skill_unsupported_for_devin():
    with TemporaryDirectory() as tmp:
        _enable_muteki_control(Path(tmp))
        items = resolve_composer_catalog(engine="devin", trigger="$", query="muteki")
        control = next(item for item in items if item["name"] == "muteki-control")
        assert control["kind"] == "skill"
        assert control["support_level"] == "unsupported"
        assert control["invocable"] is False
        assert control["source"] == "Muteki Agent Plugin"


def test_discover_skills_omits_plugin_skill_for_devin():
    skills = discover_skills("devin")
    assert not any(item["name"] == "muteki-control" for item in skills)
    assert any(
        item["name"] == "muteki-control" for item in discover_skills("codex")
    )


def test_resolve_rejects_stale_mcp_chip_on_devin():
    with TemporaryDirectory() as tmp:
        _enable_muteki_control(Path(tmp))
        try:
            resolve_capability_refs(
                [{
                    "id": "mcp:muteki-control",
                    "kind": "mcp",
                    "name": "muteki-control",
                }],
                engine="devin",
            )
        except Exception as exc:
            assert "重新选择" in str(exc)
        else:
            raise AssertionError("expected ComposerCapabilityError for Devin MCP chip")


if __name__ == "__main__":
    for name, fn in list(globals().items()):
        if name.startswith("test_") and callable(fn):
            fn()
            print(f"{name}: PASS")
    print("test_composer_capability_claims: PASS")
