"""C09 resolve_capability_refs soft-stale + snapshot tests."""

from __future__ import annotations

from pathlib import Path
from tempfile import TemporaryDirectory
from types import SimpleNamespace

from muteki.conversation.composer_capabilities import discover_skills, resolve_capability_refs


def test_message_span_missing_keeps_snapshot():
    refs, ctx = resolve_capability_refs(
        [{
            "context_schema": 2,
            "node_id": "n1",
            "id": "message_span:x",
            "kind": "message_span",
            "name": "回答摘录",
            "description": "hello",
            "source": "对话",
            "scope": "thread",
            "locator": {
                "message_id": "missing",
                "start_offset": 0,
                "end_offset": 5,
            },
            "snapshot": {
                "label": "回答摘录",
                "text": "hello world",
                "captured_at": "2026-01-01T00:00:00Z",
            },
        }],
        engine="pi",
        message_lookup=lambda _message_id: None,
    )
    assert refs[0]["status"] in {"stale", "missing"}
    assert "hello world" in ctx
    assert refs[0]["snapshot"]["text"] == "hello world"


def test_message_span_hash_mismatch_stale():
    message = SimpleNamespace(text="abcdef")
    refs, ctx = resolve_capability_refs(
        [{
            "context_schema": 2,
            "node_id": "n1b",
            "id": "message_span:y",
            "kind": "message_span",
            "name": "回答摘录",
            "description": "abc",
            "source": "对话",
            "scope": "thread",
            "locator": {
                "message_id": "m1",
                "start_offset": 0,
                "end_offset": 3,
            },
            "snapshot": {
                "label": "回答摘录",
                "text": "xyz",
                "captured_at": "2026-01-01T00:00:00Z",
                "content_hash": "deadbeef",
            },
        }],
        engine="pi",
        message_lookup=lambda _message_id: message,
    )
    assert refs[0]["status"] == "stale"
    assert "xyz" in ctx


def test_file_missing_soft_stale():
    refs, ctx = resolve_capability_refs(
        [{
            "context_schema": 2,
            "node_id": "n2",
            "id": "file:x",
            "kind": "file",
            "name": "gone.md",
            "description": "gone.md",
            "source": "当前项目",
            "scope": "project",
            "locator": {"relative_path": "gone.md"},
            "snapshot": {
                "label": "gone.md",
                "text": "was here",
                "captured_at": "2026-01-01T00:00:00Z",
            },
        }],
        engine="pi",
        workspace_root="/tmp",
    )
    assert refs[0]["status"] == "missing"
    assert "was here" in ctx


def test_file_line_range_ok():
    with TemporaryDirectory() as tmp:
        path = Path(tmp) / "README.md"
        path.write_text("line1\nline2\nline3\n", encoding="utf-8")
        refs, ctx = resolve_capability_refs(
            [{
                "context_schema": 2,
                "node_id": "n3",
                "id": "file:r",
                "kind": "file",
                "name": "README.md",
                "description": "README.md",
                "source": "当前项目",
                "scope": "project",
                "locator": {
                    "relative_path": "README.md",
                    "start_line": 1,
                    "end_line": 2,
                },
                "snapshot": {
                    "label": "README.md:1-2",
                    "text": "line1\nline2",
                    "captured_at": "2026-01-01T00:00:00Z",
                },
            }],
            engine="pi",
            workspace_root=tmp,
        )
        assert refs[0]["status"] == "ok"
        assert "line1" in ctx
        assert "L1-2" in ctx


def test_schema_v2_mcp_strip_chip_uses_catalog():
    refs, ctx = resolve_capability_refs(
        [{
            "context_schema": 2,
            "node_id": "strip_mcp:muteki-control",
            "id": "mcp:muteki-control",
            "kind": "mcp",
            "name": "muteki-control",
            "description": "Runtime 已报告 · 22 个工具",
            "source": "Muteki",
            "scope": "thread",
            "legacy_capability_id": "mcp:muteki-control",
            "locator": {},
            "snapshot": {
                "label": "muteki-control",
                "text": "Runtime 已报告 · 22 个工具",
                "captured_at": "2026-01-01T00:00:00Z",
            },
            "status": "ok",
        }],
        engine="codex",
    )
    assert refs[0]["id"] == "mcp:muteki-control"
    assert refs[0]["kind"] == "mcp"
    assert "未知引用类型" not in str(refs[0].get("status_reason") or "")
    assert "显式引用的上下文" not in ctx


def test_schema_v2_skill_strip_chip_injects_skill_text():
    skills = discover_skills("codex")
    control = next(item for item in skills if item["name"] == "muteki-control")
    refs, ctx = resolve_capability_refs(
        [{
            "context_schema": 2,
            "node_id": f"strip_{control['id']}",
            "id": control["id"],
            "kind": "skill",
            "name": control["name"],
            "description": control.get("description") or control["name"],
            "source": control.get("source") or "Muteki Agent Plugin",
            "scope": control.get("scope") or "thread",
            "legacy_capability_id": control["id"],
            "locator": {},
            "snapshot": {
                "label": control["name"],
                "text": control.get("description") or control["name"],
                "captured_at": "2026-01-01T00:00:00Z",
            },
            "status": "ok",
        }],
        engine="codex",
    )
    assert refs[0]["kind"] == "skill"
    assert refs[0]["name"] == "muteki-control"
    assert "用户显式选择的 Agent Skill: muteki-control" in ctx
    assert "未知引用类型" not in str(refs[0].get("status_reason") or "")


if __name__ == "__main__":
    for name, fn in list(globals().items()):
        if name.startswith("test_") and callable(fn):
            fn()
            print(f"{name}: PASS")
    print("test_composer_context_refs: PASS")
