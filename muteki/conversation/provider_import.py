"""Provider session scanner and importer — C33.

Supports scanning local Claude (`~/.claude`) and Codex (`~/.codex`) history
directories, previewing sessions before import, and batch-importing into
Muteki threads with:
  - deduplication by external_session_id (idempotent re-import)
  - partial-failure tolerance (bad records skip, don't abort batch)
  - continuability labels: "continuable" | "read_only" | "missing_tools"
  - original provider JSONL files are never modified
"""

from __future__ import annotations

import json
import logging
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Optional

from muteki.platform.contracts.base import utcnow, new_id

_log = logging.getLogger(__name__)

# ---------------------------------------------------------------------------
# Scan result
# ---------------------------------------------------------------------------

@dataclass
class ProviderSessionScan:
    """Preview record returned before committing an import."""
    session_id: str
    adapter_id: str          # "claude" | "codex"
    source_path: str
    title: str
    message_count: int
    created_at: Optional[str]  # ISO timestamp string or None
    # Import feasibility labels (AC3)
    import_status: str        # "continuable" | "read_only" | "missing_tools"
    missing_fields: list[str] = field(default_factory=list)
    # Messages extracted during scan (kept in-memory for apply phase)
    _messages: list[dict[str, Any]] = field(default_factory=list, repr=False)


@dataclass
class ImportRecord:
    """Result of importing a single session."""
    session_id: str
    thread_id: Optional[str]
    skipped: bool        # True when deduplicated (already imported)
    error: Optional[str]


@dataclass
class BatchImportResult:
    """Aggregate result of a batch import."""
    imported: list[ImportRecord]
    skipped: list[ImportRecord]
    failed: list[ImportRecord]

    @property
    def total(self) -> int:
        return len(self.imported) + len(self.skipped) + len(self.failed)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _parse_iso(s: Any) -> Optional[str]:
    if not s:
        return None
    try:
        if isinstance(s, (int, float)):
            return datetime.fromtimestamp(s / 1000 if s > 1e10 else s,
                                          tz=timezone.utc).isoformat()
        return str(s)
    except Exception:
        return None


def _text_of(content: Any) -> str:
    """Extract plain text from Claude/Codex content (string or list of blocks)."""
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        parts = []
        for block in content:
            if isinstance(block, dict):
                if block.get("type") == "text":
                    parts.append(str(block.get("text", "")))
                elif block.get("type") == "tool_use":
                    parts.append(f"[tool:{block.get('name','')}]")
                elif block.get("type") == "tool_result":
                    parts.append(f"[tool_result]")
            elif isinstance(block, str):
                parts.append(block)
        return " ".join(parts)
    return str(content) if content else ""


def _has_tool_calls(messages: list[dict[str, Any]]) -> bool:
    for msg in messages:
        content = msg.get("content", "")
        if isinstance(content, list):
            for block in content:
                if isinstance(block, dict) and block.get("type") in ("tool_use", "tool_result"):
                    return True
        role = msg.get("role", "")
        if role == "tool":
            return True
    return False


def _infer_title(messages: list[dict[str, Any]], session_id: str) -> str:
    """Use the first user message as session title (truncated)."""
    for msg in messages:
        if msg.get("role") == "human" or msg.get("role") == "user":
            text = _text_of(msg.get("content", "")).strip()
            if text:
                return text[:80] + ("…" if len(text) > 80 else "")
    return f"导入会话 {session_id[:8]}"


# ---------------------------------------------------------------------------
# Claude scanner  (~/.claude/projects/<encoded-path>/<uuid>.jsonl)
# ---------------------------------------------------------------------------

def _parse_claude_jsonl(path: Path) -> list[dict[str, Any]]:
    """Parse a Claude conversation JSONL file.

    Each line can be a bare message dict or a wrapper `{"type":"message", ...}`.
    Returns a list of normalised message dicts with keys: role, content.
    """
    messages: list[dict[str, Any]] = []
    try:
        lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError:
        return messages
    for raw in lines:
        raw = raw.strip()
        if not raw:
            continue
        try:
            obj = json.loads(raw)
        except json.JSONDecodeError:
            continue
        # Unwrap envelope: {"type": "message", "message": {...}}
        if isinstance(obj, dict) and "message" in obj and isinstance(obj["message"], dict):
            obj = obj["message"]
        # Claude uses "human" / "assistant"; normalise to "user" / "assistant"
        role = obj.get("role", "")
        if role == "human":
            role = "user"
        content = obj.get("content", obj.get("text", ""))
        if role and content is not None:
            messages.append({"role": role, "content": content})
    return messages


def scan_claude_sessions(base_path: Path, limit: int = 100) -> list[ProviderSessionScan]:
    """Scan Claude history under *base_path*, returning preview records."""
    scans: list[ProviderSessionScan] = []
    # Support both ~/.claude/projects/<hash>/<uuid>.jsonl and ~/.claude/<uuid>.jsonl
    patterns = ["**/*.jsonl", "*.jsonl"]
    seen: set[str] = set()
    for pattern in patterns:
        for p in sorted(base_path.glob(pattern), key=lambda f: f.stat().st_mtime, reverse=True):
            if len(scans) >= limit:
                break
            session_id = p.stem
            if session_id in seen:
                continue
            seen.add(session_id)
            messages = _parse_claude_jsonl(p)
            if not messages:
                continue
            has_tools = _has_tool_calls(messages)
            title = _infer_title(messages, session_id)
            # Determine continuability: Claude sessions are resumable by session_id
            # if the original CLI is still available. We label as "read_only" since
            # we can't guarantee CLI access, but keep "continuable" if resume_handle
            # can be set.
            import_status = "missing_tools" if has_tools else "read_only"
            missing: list[str] = []
            if has_tools:
                missing.append("tool_outputs")
            # Try to extract timestamp from first message metadata
            created_at = _parse_iso(p.stat().st_mtime)
            scans.append(ProviderSessionScan(
                session_id=session_id,
                adapter_id="claude",
                source_path=str(p),
                title=title,
                message_count=len(messages),
                created_at=created_at,
                import_status=import_status,
                missing_fields=missing,
                _messages=messages,
            ))
    return scans


# ---------------------------------------------------------------------------
# Codex scanner  (~/.codex/sessions/*.jsonl or ~/.codex/history/*.json)
# ---------------------------------------------------------------------------

def _parse_codex_jsonl(path: Path) -> list[dict[str, Any]]:
    messages: list[dict[str, Any]] = []
    try:
        text = path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return messages
    # Try as single JSON array first
    text_strip = text.strip()
    if text_strip.startswith("["):
        try:
            objs = json.loads(text_strip)
            if isinstance(objs, list):
                for obj in objs:
                    if isinstance(obj, dict):
                        # Unwrap envelope: {"type":"message","message":{...}}
                        if "message" in obj and isinstance(obj["message"], dict):
                            obj = obj["message"]
                        role = obj.get("role", "")
                        content = obj.get("content", obj.get("text", ""))
                        if role:
                            messages.append({"role": role, "content": content})
                return messages
        except json.JSONDecodeError:
            pass
    # Fall back to JSONL
    for raw in text_strip.splitlines():
        raw = raw.strip()
        if not raw:
            continue
        try:
            obj = json.loads(raw)
            # Unwrap envelope: {"type":"message","message":{...}}
            if isinstance(obj, dict) and "message" in obj and isinstance(obj["message"], dict):
                obj = obj["message"]
            role = obj.get("role", "")
            content = obj.get("content", obj.get("text", ""))
            if role:
                messages.append({"role": role, "content": content})
        except json.JSONDecodeError:
            continue
    return messages


def scan_codex_sessions(base_path: Path, limit: int = 100) -> list[ProviderSessionScan]:
    scans: list[ProviderSessionScan] = []
    patterns = ["**/*.jsonl", "**/*.json", "*.jsonl", "*.json"]
    seen: set[str] = set()
    for pattern in patterns:
        for p in sorted(base_path.glob(pattern), key=lambda f: f.stat().st_mtime, reverse=True):
            if len(scans) >= limit:
                break
            session_id = p.stem
            if session_id in seen:
                continue
            seen.add(session_id)
            messages = _parse_codex_jsonl(p)
            if not messages:
                continue
            has_tools = _has_tool_calls(messages)
            title = _infer_title(messages, session_id)
            import_status = "missing_tools" if has_tools else "read_only"
            missing = ["tool_outputs"] if has_tools else []
            created_at = _parse_iso(p.stat().st_mtime)
            scans.append(ProviderSessionScan(
                session_id=session_id,
                adapter_id="codex",
                source_path=str(p),
                title=title,
                message_count=len(messages),
                created_at=created_at,
                import_status=import_status,
                missing_fields=missing,
                _messages=messages,
            ))
    return scans


# ---------------------------------------------------------------------------
# Importer
# ---------------------------------------------------------------------------

def _find_existing_thread(store: Any, external_session_id: str) -> Optional[str]:
    """Return thread_id if this external_session_id was already imported."""
    try:
        from muteki.platform.contracts.objects import AgentSession
        sessions = store.list(AgentSession, external_session_id=external_session_id)
        if sessions:
            return sessions[0].thread_id
    except Exception:
        pass
    return None


def import_session(
    manager: Any,
    scan: ProviderSessionScan,
    *,
    project_id: str = "",
    dry_run: bool = False,
) -> ImportRecord:
    """Import a single scanned session.  Idempotent by external_session_id."""
    try:
        store = manager._store
        # AC1: deduplication
        existing = _find_existing_thread(store, scan.session_id)
        if existing:
            return ImportRecord(
                session_id=scan.session_id, thread_id=existing,
                skipped=True, error=None)

        if dry_run:
            return ImportRecord(
                session_id=scan.session_id, thread_id=None,
                skipped=False, error=None)

        # Create thread
        thread = manager.create_thread(
            project_id=project_id,
            title=scan.title,
            title_source="user",
        )
        thread_id = thread.thread_id

        # Fix (Medium): save AgentSession FIRST as a durable dedup marker.
        # Even if message-save fails, subsequent re-import sees this record
        # and skips instead of creating a second orphaned thread.
        from muteki.platform.contracts.objects import AgentSession
        agent_session = AgentSession(
            agent_session_id=new_id("asess"),
            external_session_id=scan.session_id,
            adapter_id=scan.adapter_id,
            thread_id=thread_id,
            resume_handle=None,
        )
        try:
            store.save(agent_session)
        except Exception as save_exc:
            # AgentSession save failed — attempt to clean up the thread to
            # avoid a truly un-deduplicatable orphan, then surface the error.
            try:
                from muteki.platform.contracts.objects import Thread as _T
                store.delete(_T, thread_id)
            except Exception:
                pass
            raise save_exc

        # Save messages as ConversationMessages.
        # Fix (High): offset stream_seq from current message count so
        # a partial-then-resumed import never collides within the thread.
        conv = manager.conv
        from muteki.conversation.models import ConversationMessage
        seq_base = len(conv.list_messages(thread_id))
        for i, msg in enumerate(scan._messages):
            role = msg.get("role", "user")
            text = _text_of(msg.get("content", ""))
            cm = ConversationMessage(
                message_id=new_id("msg"),
                thread_id=thread_id,
                turn_id=None,
                role=role if role in ("user", "assistant", "system") else "user",
                kind="message",
                text=text,
                stream_seq=seq_base + i,
            )
            conv.save_message(cm)

        # Mark thread with import metadata via last_message_preview (AC3 label).
        state = conv.get_state(thread_id)
        state.last_message_preview = (
            f"[{scan.import_status}] 导入自 {scan.adapter_id} · "
            f"{scan.message_count} 条消息"
        )
        conv.save_state(state)

        _log.info("C33 imported %s → thread %s (%s)", scan.session_id, thread_id, scan.import_status)
        return ImportRecord(session_id=scan.session_id, thread_id=thread_id,
                            skipped=False, error=None)

    except Exception as exc:
        _log.warning("C33 import failed for %s: %s", scan.session_id, exc)
        return ImportRecord(session_id=scan.session_id, thread_id=None,
                            skipped=False, error=str(exc))


def batch_import(
    manager: Any,
    scans: list[ProviderSessionScan],
    *,
    project_id: str = "",
    dry_run: bool = False,
) -> BatchImportResult:
    """Import multiple sessions; partial failures don't abort the batch (AC1)."""
    imported: list[ImportRecord] = []
    skipped: list[ImportRecord] = []
    failed: list[ImportRecord] = []
    for scan in scans:
        record = import_session(manager, scan, project_id=project_id, dry_run=dry_run)
        if record.error:
            failed.append(record)
        elif record.skipped:
            skipped.append(record)
        else:
            imported.append(record)
    return BatchImportResult(imported=imported, skipped=skipped, failed=failed)


def scan_to_dict(scan: ProviderSessionScan) -> dict[str, Any]:
    """Serialise scan for API response (omit _messages)."""
    return {
        "session_id": scan.session_id,
        "adapter_id": scan.adapter_id,
        "source_path": scan.source_path,
        "title": scan.title,
        "message_count": scan.message_count,
        "created_at": scan.created_at,
        "import_status": scan.import_status,
        "missing_fields": scan.missing_fields,
    }


__all__ = [
    "ProviderSessionScan",
    "ImportRecord",
    "BatchImportResult",
    "scan_claude_sessions",
    "scan_codex_sessions",
    "scan_to_dict",
    "import_session",
    "batch_import",
]
