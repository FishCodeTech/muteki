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
import hashlib
import os
import heapq
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
    source_fingerprint: str = ""
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
                if block.get("type") in {"text", "input_text", "output_text"}:
                    parts.append(str(block.get("text", "")))
                elif block.get("type") == "tool_use":
                    parts.append(json.dumps(block, ensure_ascii=False))
                elif block.get("type") == "tool_result":
                    parts.append(_text_of(block.get("content", "")))
                else:
                    parts.append(json.dumps(block, ensure_ascii=False))
            elif isinstance(block, str):
                parts.append(block)
        return "\n".join(parts)
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

class ImportScanBudgetError(ValueError):
    code = "conversation.import.scan_budget_exceeded"


_MAX_IMPORT_FILE_BYTES = 32 * 1024 * 1024
_MAX_IMPORT_RECORD_BYTES = 1024 * 1024
_MAX_IMPORT_RECORDS = 20_000
_MAX_SCAN_FILES = 10_000
_MAX_SCAN_BYTES = 64 * 1024 * 1024


def _source_fingerprint(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(64 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _records(path: Path):
    if path.stat().st_size > _MAX_IMPORT_FILE_BYTES:
        raise ImportScanBudgetError(f"{path.name}: 文件超过 {_MAX_IMPORT_FILE_BYTES} 字节扫描预算，请拆分或明确调整预算。")
    with path.open("rb") as handle:
        first = handle.read(1)
        while first and first.isspace():
            first = handle.read(1)
        handle.seek(0)
        if path.suffix.lower() == ".json" or first == b"[":
            # Legacy JSON arrays are explicitly bounded; JSONL remains streaming.
            objs = json.load(handle)
            if isinstance(objs, dict):
                objs = [objs]
            if not isinstance(objs, list):
                raise ValueError(f"conversation.import.invalid_record: {path.name} requires a JSON object or array")
            if len(objs) > _MAX_IMPORT_RECORDS:
                raise ImportScanBudgetError(f"{path.name}: 记录数超过扫描预算")
            yield from (obj for obj in objs if isinstance(obj, dict))
            return
        count = 0
        while True:
            raw = handle.readline(_MAX_IMPORT_RECORD_BYTES + 1)
            if not raw:
                return
            count += 1
            if len(raw) > _MAX_IMPORT_RECORD_BYTES or count > _MAX_IMPORT_RECORDS:
                raise ImportScanBudgetError(f"{path.name}: 单条记录或记录数超过扫描预算")
            if not raw.strip():
                continue
            try:
                obj = json.loads(raw)
            except (json.JSONDecodeError, UnicodeError):
                raise ValueError(f"conversation.import.invalid_record: {path.name} 记录 {count}")
            if isinstance(obj, dict):
                yield obj


def _parse_provider(path: Path, provider: str) -> list[dict[str, Any]]:
    messages: list[dict[str, Any]] = []
    response_messages: list[dict[str, Any]] = []
    event_messages: list[dict[str, Any]] = []
    for record in _records(path):
        obj = record
        if isinstance(obj.get("message"), dict):
            obj = obj["message"]
        elif provider == "codex" and obj.get("type") == "response_item" and isinstance(obj.get("payload"), dict):
            obj = obj["payload"]
            if obj.get("type") == "function_call_output":
                response_messages.append({"role": "tool", "content": obj.get("output", "")})
                continue
            if obj.get("type") == "function_call":
                response_messages.append({"role": "assistant", "content": [obj]})
                continue
            role = str(obj.get("role") or "")
            if role:
                response_messages.append({"role": role, "content": obj.get("content", obj.get("text", ""))})
            continue
        elif provider == "codex" and obj.get("type") == "event_msg" and isinstance(obj.get("payload"), dict):
            payload = obj["payload"]
            role = {"user_message": "user", "agent_message": "assistant"}.get(str(payload.get("type")))
            if role:
                event_messages.append({"role": role, "content": payload.get("message", "")})
            continue
        role = str(obj.get("role") or "")
        if role:
            messages.append({"role": "user" if role == "human" else role,
                             "content": obj.get("content", obj.get("text", ""))})
    # response_item is the canonical Codex conversation; event_msg mirrors it.
    # Older event-only rollouts remain readable without duplicating both formats.
    return messages + (response_messages if response_messages else event_messages)


def _parse_claude_jsonl(path: Path) -> list[dict[str, Any]]:
    return _parse_provider(path, "claude")


def _parse_codex_jsonl(path: Path) -> list[dict[str, Any]]:
    return _parse_provider(path, "codex")


def _scan_sessions(base_path: Path, provider: str, limit: int, retain_messages: bool) -> list[ProviderSessionScan]:
    candidates: list[tuple[float, str, Path]] = []
    for directory, _dirs, files in os.walk(base_path, followlinks=False):
        for name in files:
            path = Path(directory) / name
            if path.suffix not in ({".jsonl", ".json"} if provider == "codex" else {".jsonl"}) or path.is_symlink():
                continue
            stat = path.stat()
            candidates.append((stat.st_mtime, str(path), path))
            if len(candidates) > _MAX_SCAN_FILES:
                raise ImportScanBudgetError("历史文件数量超过扫描预算，请选择更具体的目录。")
    scans: list[ProviderSessionScan] = []
    total_bytes = 0
    seen: set[str] = set()
    for _mtime, _name, path in heapq.nlargest(len(candidates), candidates):
        if len(scans) >= limit:
            break
        total_bytes += path.stat().st_size
        if total_bytes > _MAX_SCAN_BYTES:
            raise ImportScanBudgetError("本次扫描超过字节预算，请选择更具体的历史目录。")
        if path.stat().st_size > _MAX_IMPORT_FILE_BYTES:
            raise ImportScanBudgetError(f"{path.name}: 文件超过扫描预算")
        fingerprint = _source_fingerprint(path)
        messages = _parse_provider(path, provider)
        if not messages:
            continue
        if fingerprint != _source_fingerprint(path):
            raise ValueError(f"conversation.import.source_changed: {path.name}")
        session_id = path.stem
        if session_id in seen:
            raise ValueError(f"conversation.import.source_conflict: 同目录有重复会话 ID {session_id}")
        seen.add(session_id)
        scans.append(ProviderSessionScan(session_id=session_id, adapter_id=provider,
            source_path=str(path), title=_infer_title(messages, session_id),
            message_count=len(messages), created_at=_parse_iso(path.stat().st_mtime),
            import_status="read_only", missing_fields=[], source_fingerprint=fingerprint,
            _messages=messages if retain_messages else []))
    return scans


def scan_claude_sessions(base_path: Path, limit: int = 100, *, retain_messages: bool = True) -> list[ProviderSessionScan]:
    return _scan_sessions(base_path, "claude", limit, retain_messages)


def scan_codex_sessions(base_path: Path, limit: int = 100, *, retain_messages: bool = True) -> list[ProviderSessionScan]:
    return _scan_sessions(base_path, "codex", limit, retain_messages)


# ---------------------------------------------------------------------------
# Importer
# ---------------------------------------------------------------------------

def _find_existing_thread(store: Any, external_session_id: str, adapter_id: str) -> Optional[str]:
    from muteki.platform.contracts.objects import AgentSession
    sessions = store.list(AgentSession, external_session_id=external_session_id)
    matching = [row for row in sessions if row.adapter_id == adapter_id and row.thread_id]
    return matching[0].thread_id if matching else None


def import_session(manager: Any, scan: ProviderSessionScan, *, project_id: str = "", dry_run: bool = False) -> ImportRecord:
    """Commit source identity and full history in the same database transaction."""
    try:
        store, conv = manager._store, manager.conv
        if dry_run:
            return ImportRecord(scan.session_id, None, False, None)
        with store.transaction():
            existing = _find_existing_thread(store, scan.session_id, scan.adapter_id)
            existing_messages = conv.list_messages(existing) if existing else []
            if existing:
                prefix = existing_messages[:len(scan._messages)]
                if any(old.text != _text_of(msg.get("content", "")) for old, msg in zip(prefix, scan._messages)):
                    raise ValueError("conversation.import.source_conflict: 已有导入内容与当前来源不同")
                if len(existing_messages) >= len(scan._messages):
                    return ImportRecord(scan.session_id, existing, True, None)
            thread_id = existing or manager.create_thread(project_id=project_id, title=scan.title, title_source="user").thread_id
            from muteki.platform.contracts.objects import AgentSession
            from muteki.conversation.models import ConversationMessage
            if not existing:
                store.save(AgentSession(agent_session_id=new_id("asess"), external_session_id=scan.session_id,
                    adapter_id=scan.adapter_id, thread_id=thread_id, resume_handle=None))
            # Imported history precedes event-driven messages (positive event seq).
            # Existing legacy partial imports are rewritten in place on retry.
            for index, msg in enumerate(scan._messages):
                role = str(msg.get("role") or "unknown")
                message_id = existing_messages[index].message_id if index < len(existing_messages) else new_id("msg")
                cm = ConversationMessage(message_id=message_id, thread_id=thread_id, turn_id=None,
                    role=role, kind="imported_tool" if role == "tool" else "message" if role in {"user", "assistant", "system"} else "imported_record",
                    text=_text_of(msg.get("content", "")), stream_seq=index - len(scan._messages),
                    source_provider=scan.adapter_id, source_role=role, source_content=msg.get("content"))
                conv.save_message(cm)
            state = conv.get_state(thread_id)
            state.last_message_preview = f"导入自 {scan.adapter_id} · {scan.message_count} 条历史消息"
            conv.save_state(state)
        return ImportRecord(scan.session_id, thread_id, False, None)
    except Exception as exc:
        _log.warning("history import failed for %s: %s", scan.session_id, exc)
        return ImportRecord(scan.session_id, None, False, str(exc))


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
        "source_fingerprint": scan.source_fingerprint,
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
