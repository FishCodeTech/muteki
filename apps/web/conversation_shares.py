"""User-created, fixed, bounded conversation snapshots with revocable read access.

Management stays behind the Web operator gate. Only exact GET share-resource
routes accept a separate, snapshot-scoped token; no thread/workspace authority
is conveyed by that token. Link tokens live in URL fragments, not server URLs.
"""
from __future__ import annotations

import hashlib
import hmac
import json
import re
import secrets
import sqlite3
import time
from contextlib import contextmanager
from pathlib import Path
from typing import Any, Literal

from fastapi import APIRouter, HTTPException, Query, Request
from fastapi.responses import JSONResponse, Response
from pydantic import BaseModel, Field

from apps.web.auth import bearer_from_header, verify_token
from muteki.conversation.workspace_surfaces import content_disposition_header

MAX_MESSAGES = 500
MAX_SNAPSHOT_BYTES = 2 * 1024 * 1024
MAX_ATTACHMENT_BYTES = 256 * 1024
MAX_ATTACHMENTS = 10
PREVIEW_TTL = 15 * 60
SHARE_ID = r"[A-Za-z0-9_-]{24}"
PUBLIC_SHARE_GET = re.compile(rf"/api/conversation-shares/{SHARE_ID}(?:/attachments/[a-f0-9]{{24}})?\Z")
DIGEST = re.compile(r"[a-f0-9]{64}\Z")
PRIVATE_HEADERS = {"Cache-Control": "no-store", "Referrer-Policy": "no-referrer", "X-Robots-Tag": "noindex, nofollow", "X-Content-Type-Options": "nosniff"}
# Conservative path removal covers POSIX and drive/UNC forms without touching URLs.
ABSOLUTE_PATH = re.compile(r"(?<![\w:/])(?:[A-Za-z]:[\\/][^\s\"'`<>]+|\\\\[^\s\"'`<>]+|/(?!/)[^\s\"'`<>]+)")


def is_public_share_read(method: str, path: str) -> bool:
    return method == "GET" and PUBLIC_SHARE_GET.fullmatch(path) is not None


class SharePreviewRequest(BaseModel):
    message_ids: list[str] | None = Field(default=None, max_length=MAX_MESSAGES)
    attachment_ids: list[str] = Field(default_factory=list, max_length=MAX_ATTACHMENTS)
    redact_paths: bool = True
    include_tool_summaries: bool = False


class ShareCreateRequest(BaseModel):
    preview_id: str = Field(min_length=24, max_length=24, pattern=rf"^{SHARE_ID}$")
    access_mode: Literal["authenticated", "link"]
    expires_in_seconds: int = Field(ge=300, le=7 * 86400)


class ConversationShareStore:
    def __init__(self, path: Path):
        self.path = path
        path.parent.mkdir(parents=True, exist_ok=True)
        # Avoid keeping a shared SQLite connection across request threads.
        with self.connection() as db:
            db.executescript("""
                CREATE TABLE IF NOT EXISTS previews (
                    id TEXT PRIMARY KEY, thread_id TEXT NOT NULL, snapshot TEXT NOT NULL,
                    created REAL NOT NULL, expires REAL NOT NULL
                );
                CREATE TABLE IF NOT EXISTS shares (
                    id TEXT PRIMARY KEY, thread_id TEXT NOT NULL, token_hash TEXT NOT NULL,
                    snapshot TEXT NOT NULL, mode TEXT NOT NULL, created REAL NOT NULL,
                    expires REAL NOT NULL, revoked REAL
                );
                CREATE INDEX IF NOT EXISTS shares_thread ON shares(thread_id, created);
            """)
        path.chmod(0o600)

    @contextmanager
    def connection(self):
        db = sqlite3.connect(self.path, timeout=5)
        db.row_factory = sqlite3.Row
        try:
            with db:
                yield db
        finally:
            db.close()

    def preview(self, thread_id: str, snapshot: dict[str, Any]) -> str:
        payload = json.dumps(snapshot, ensure_ascii=False)
        if len(payload.encode("utf-8")) > MAX_SNAPSHOT_BYTES:
            raise HTTPException(413, "快照超过 2 MiB，请减少选中的消息或附件")
        now = time.time()
        preview_id = secrets.token_urlsafe(18)
        with self.connection() as db:
            db.execute("DELETE FROM previews WHERE expires <= ?", (now,))
            if db.execute("SELECT COUNT(*) FROM previews WHERE thread_id = ?", (thread_id,)).fetchone()[0] >= 20:
                raise HTTPException(429, "待确认预览过多，请稍后重试")
            db.execute("INSERT INTO previews VALUES (?,?,?,?,?)", (preview_id, thread_id, payload, now, now + PREVIEW_TTL))
        return preview_id

    def create(self, thread_id: str, body: ShareCreateRequest) -> dict[str, Any]:
        now = time.time()
        share_id, token = secrets.token_urlsafe(18), secrets.token_urlsafe(32)
        with self.connection() as db:
            db.execute("BEGIN IMMEDIATE")
            row = db.execute("SELECT * FROM previews WHERE id = ? AND thread_id = ? AND expires > ?", (body.preview_id, thread_id, now)).fetchone()
            if row is None:
                raise HTTPException(409, "预览已过期或已创建分享，请重新预览")
            if db.execute("SELECT COUNT(*) FROM shares WHERE thread_id = ? AND revoked IS NULL AND expires > ?", (thread_id, now)).fetchone()[0] >= 50:
                raise HTTPException(409, "本会话有效分享已达 50 条，请先撤销旧分享")
            expires = now + body.expires_in_seconds
            db.execute("INSERT INTO shares VALUES (?,?,?,?,?,?,?,NULL)", (share_id, thread_id, hashlib.sha256(token.encode()).hexdigest(), row["snapshot"], body.access_mode, now, expires))
            db.execute("DELETE FROM previews WHERE id = ?", (body.preview_id,))
        return {"share_id": share_id, "token": token, "access_mode": body.access_mode, "created_at": now, "expires_at": expires}

    def list(self, thread_id: str) -> list[dict[str, Any]]:
        with self.connection() as db:
            rows = db.execute("SELECT id, mode, created, expires, revoked, snapshot FROM shares WHERE thread_id = ? ORDER BY (revoked IS NULL AND expires > ?) DESC, created DESC LIMIT 100", (thread_id, time.time())).fetchall()
        now = time.time()
        return [{
            "share_id": row["id"], "access_mode": row["mode"], "created_at": row["created"], "expires_at": row["expires"],
            "status": "revoked" if row["revoked"] is not None else "expired" if row["expires"] <= now else "active",
            "message_count": len(json.loads(row["snapshot"])["messages"]),
            "watermark": json.loads(row["snapshot"])["watermark"],
        } for row in rows]

    def revoke(self, thread_id: str, share_id: str) -> None:
        with self.connection() as db:
            row = db.execute("UPDATE shares SET revoked = COALESCE(revoked, ?) WHERE id = ? AND thread_id = ?", (time.time(), share_id, thread_id))
            if not row.rowcount:
                raise HTTPException(404, "分享不存在")

    def read(self, share_id: str, token: str, *, authenticated: bool) -> dict[str, Any]:
        with self.connection() as db:
            row = db.execute("SELECT * FROM shares WHERE id = ?", (share_id,)).fetchone()
        if row is None or not hmac.compare_digest(row["token_hash"], hashlib.sha256(token.encode()).hexdigest()):
            raise HTTPException(404, "分享不存在或链接不完整")
        if row["revoked"] is not None:
            raise HTTPException(410, "此分享已撤销")
        if row["expires"] <= time.time():
            raise HTTPException(410, "此分享已过期")
        if row["mode"] == "authenticated" and not authenticated:
            raise HTTPException(401, "此分享仅允许已登录的工作台用户查看")
        return {"share_id": share_id, "access_mode": row["mode"], "created_at": row["created"], "expires_at": row["expires"], "snapshot": json.loads(row["snapshot"])}


def build_share_snapshot(service: Any, thread_id: str, body: SharePreviewRequest) -> tuple[dict[str, Any], dict[str, list[dict[str, Any]]]]:
    def clean(value: Any) -> str:
        text = str(value or "")
        return ABSOLUTE_PATH.sub("<path>", text) if body.redact_paths else text

    # One database lock fixes branch membership, message order, and watermark.
    # Reading these records does not probe or start a Runtime session.
    with service.platform.lock:
        thread = service.manager.get_thread(thread_id)
        if thread is None:
            raise HTTPException(404, "对话不存在")
        messages = service.conv.list_current_messages(thread_id)
        # Only conversational prose; never expose reasoning, tools, or arbitrary metadata.
        messages = [m for m in messages if m.role in {"user", "assistant"} and m.kind in {"message", "steer"}]
        if body.message_ids is not None:
            selected = set(body.message_ids)
            if not selected or not selected.issubset({m.message_id for m in messages}):
                raise HTTPException(400, "请选择当前有效分支中的正文消息")
            messages = [m for m in messages if m.message_id in selected]
        if not messages:
            raise HTTPException(400, "当前有效分支还没有可分享的正文")
        if len(messages) > MAX_MESSAGES:
            raise HTTPException(413, "消息超过 500 条，请选择部分回复及必要上下文")
        watermark = service.platform.stream_head("thread", thread_id)
        # Page to the captured watermark. Long conversations must not silently
        # lose recent attachment membership or tool summaries after 10k events.
        # Retain only the event kinds needed by this snapshot.
        events = []
        after_seq = 0
        while after_seq < watermark:
            page = service.platform.read_events("thread", thread_id, after_seq=after_seq, limit=1000)
            if not page:
                break
            events.extend(event for event in page if event.stream_seq <= watermark
                          and event.event_type in {"core.artifact.attached", "core.artifact.created", "core.tool.completed"})
            next_seq = int(page[-1].stream_seq)
            if next_seq <= after_seq:
                raise HTTPException(409, "对话事件水位已变化，请重新生成预览")
            after_seq = next_seq
        selected_turns = {m.turn_id for m in messages if m.turn_id}
        # Attachment membership comes from selected messages, never global digest lookup.
        referenced = {str(digest) for turn in service.conv.list_turns(thread_id) if turn.turn_id in selected_turns for digest in turn.attachments if DIGEST.fullmatch(str(digest))}
        attached = {str(e.payload.get("sha256") or ""): e.payload
                    for e in events if e.event_type in {"core.artifact.attached", "core.artifact.created"}}
        allowed = referenced & attached.keys()
        requested = set(body.attachment_ids)
        if not requested.issubset(allowed):
            raise HTTPException(400, "附件不属于所选正文，请重新选择")
        candidates, attachments = [], []
        for digest in sorted(allowed):
            # Artifact metadata is globally keyed by content digest. Reusing
            # identical bytes in another thread may overwrite its global name;
            # only this thread's attachment event may supply a visible filename.
            name = Path(str(attached[digest].get("name") or f"附件-{digest[:12]}.txt").replace("\\", "/")).name
            try:
                with (service.conv.artifacts_dir / digest).open("rb") as source:
                    content = source.read(MAX_ATTACHMENT_BYTES + 1)
            except OSError:
                content = None
            reason, text = "", None
            if content is None:
                reason = "原附件已不可用"
            elif len(content) > MAX_ATTACHMENT_BYTES:
                reason = "仅支持不超过 256 KiB 的 UTF-8 文本附件"
            else:
                try:
                    text = content.decode("utf-8")
                    if "\x00" in text:
                        reason = "仅支持可预览的 UTF-8 文本附件"
                except UnicodeDecodeError:
                    reason = "仅支持可预览的 UTF-8 文本附件"
            candidates.append({"id": digest, "name": name, "size": len(content or b""), "unavailable_reason": reason})
            if digest in requested:
                if reason:
                    raise HTTPException(400, f"附件 {name}：{reason}")
                visible = clean(text)
                attachments.append({"id": secrets.token_hex(12), "name": clean(name), "size": len(visible.encode("utf-8")), "text": visible})
        tools = []
        if body.include_tool_summaries:
            for event in events:
                p = event.payload
                if event.event_type != "core.tool.completed" or p.get("turn_id") not in selected_turns:
                    continue
                status = str(p.get("status") or "ended")
                if status not in {"completed", "failed", "cancelled", "declined"}:
                    status = "ended"
                if status not in {"cancelled", "declined"} and (p.get("error") or str(p.get("exit_code") or "") not in {"0", ""}):
                    status = "failed"
                tools.append({"name": clean(str(p.get("tool_name") or p.get("tool") or "工具")[:120]), "status": status})
        snapshot = {
            "version": 1, "title": clean(thread.title or "会话快照"), "captured_at": time.time(), "watermark": watermark,
            "branch": "current", "redact_paths": body.redact_paths, "tools_exclude_parameters": True,
            "messages": [{"id": f"message-{index+1}", "role": m.role, "text": clean(m.text)} for index, m in enumerate(messages)],
            "tool_summaries": tools, "attachments": attachments,
        }
        # Source IDs only appear in the authenticated selection UI, not the snapshot.
        choices = [{"id": m.message_id, "role": m.role, "preview": clean(m.text)[:160]} for m in messages]
    return snapshot, {"attachments": candidates, "messages": choices}


def create_conversation_shares_router(*, service: Any) -> APIRouter:
    router = APIRouter()
    store = ConversationShareStore(Path(service.platform.db_path).parent / "conversation-shares.sqlite3")

    def assert_thread(thread_id: str):
        if service.manager.get_thread(thread_id) is None:
            raise HTTPException(404, "对话不存在")

    @router.get("/api/threads/{thread_id}/shares/source")
    async def source(thread_id: str, offset: int = Query(0, ge=0)):
        assert_thread(thread_id)
        with service.platform.lock:
            messages = [m for m in service.conv.list_current_messages(thread_id) if m.role in {"user", "assistant"} and m.kind in {"message", "steer"}]
        end = max(0, len(messages) - offset)
        selected = [m.message_id for m in messages[max(0, end - MAX_MESSAGES):end]]
        initial_ids = selected if len(messages) > MAX_MESSAGES else None
        if not messages:
            raise HTTPException(400, "当前有效分支还没有可分享的正文")
        if not selected:
            raise HTTPException(400, "该消息页已不存在，请返回最新一页")
        _snapshot, choices = build_share_snapshot(service, thread_id, SharePreviewRequest(message_ids=selected))
        return JSONResponse({"choices": choices, "initial_message_ids": initial_ids, "total_message_count": len(messages), "offset": offset, "has_older": offset + MAX_MESSAGES < len(messages)}, headers=PRIVATE_HEADERS)

    @router.post("/api/threads/{thread_id}/shares/preview")
    async def preview(thread_id: str, body: SharePreviewRequest):
        snapshot, choices = build_share_snapshot(service, thread_id, body)
        preview_id = store.preview(thread_id, snapshot)
        return JSONResponse({"preview_id": preview_id, "snapshot": snapshot, "choices": choices, "preview_expires_in_seconds": PREVIEW_TTL}, headers=PRIVATE_HEADERS)

    @router.post("/api/threads/{thread_id}/shares", status_code=201)
    async def create(thread_id: str, body: ShareCreateRequest, request: Request):
        assert_thread(thread_id)
        if body.access_mode == "authenticated" and not request.app.state.auth.enabled:
            raise HTTPException(409, "工作台尚未启用登录认证，请先配置认证，或明确选择链接持有人访问")
        return JSONResponse(store.create(thread_id, body), headers=PRIVATE_HEADERS, status_code=201)

    @router.get("/api/threads/{thread_id}/shares")
    async def list_shares(thread_id: str):
        assert_thread(thread_id)
        return JSONResponse({"shares": store.list(thread_id)}, headers=PRIVATE_HEADERS)

    @router.delete("/api/threads/{thread_id}/shares/{share_id}")
    async def revoke(thread_id: str, share_id: str):
        assert_thread(thread_id)
        store.revoke(thread_id, share_id)
        return JSONResponse({"revoked": True}, headers=PRIVATE_HEADERS)

    def read_scope(share_id: str, request: Request):
        cfg = request.app.state.auth
        authenticated = cfg.enabled and verify_token(cfg, bearer_from_header(request.headers.get("Authorization")))
        token = request.headers.get("X-Muteki-Share-Token", "")
        if len(token) != 43:
            raise HTTPException(404, "分享不存在或链接不完整")
        return store.read(share_id, token, authenticated=authenticated)

    @router.get("/api/conversation-shares/{share_id}")
    async def read(share_id: str, request: Request):
        try:
            return JSONResponse(read_scope(share_id, request), headers=PRIVATE_HEADERS)
        except HTTPException as exc:
            return JSONResponse({"detail": exc.detail}, status_code=exc.status_code, headers=PRIVATE_HEADERS)

    @router.get("/api/conversation-shares/{share_id}/attachments/{attachment_id}")
    async def attachment(share_id: str, attachment_id: str, request: Request):
        try:
            share = read_scope(share_id, request)
        except HTTPException as exc:
            return JSONResponse({"detail": exc.detail}, status_code=exc.status_code, headers=PRIVATE_HEADERS)
        item = next((a for a in share["snapshot"]["attachments"] if a["id"] == attachment_id), None)
        if item is None:
            raise HTTPException(404, "此快照没有所选附件", headers=PRIVATE_HEADERS)
        return Response(item["text"].encode("utf-8"), media_type="text/plain; charset=utf-8", headers={**PRIVATE_HEADERS, "Content-Disposition": content_disposition_header(item["name"], disposition="attachment")})

    return router
