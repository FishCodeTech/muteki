"""No network listeners or Agent calls: bounded synthetic snapshot/route checks."""
from __future__ import annotations
import hashlib
import json
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from fastapi import FastAPI, HTTPException, Request
from fastapi.responses import JSONResponse
from fastapi.testclient import TestClient
from apps.web.auth import AuthConfig, bearer_from_header, issue_token, verify_token
from apps.web.conversation_shares import (
    ConversationShareStore, ShareCreateRequest, SharePreviewRequest,
    build_share_snapshot, create_conversation_shares_router, is_public_share_read,
    MAX_ATTACHMENT_BYTES, MAX_SNAPSHOT_BYTES,
)
from muteki.conversation.models import ConversationMessage, TurnRecord
from muteki.conversation.store import ConversationStore
from muteki.platform.contracts.events import EventEnvelope
from muteki.platform.contracts.objects import Artifact, Thread
from muteki.platform.store import PlatformStore


class ConversationSharesTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.platform = PlatformStore(self.root / "platform.db")
        self.conv = ConversationStore(self.platform)
        self.thread = Thread(thread_id="thread-a", title="中文成果 /Users/qa/private/project")
        self.service = SimpleNamespace(platform=self.platform, conv=self.conv, manager=SimpleNamespace(get_thread=lambda tid: self.thread if tid == "thread-a" else None))
        self.conv.save_turn(TurnRecord(turn_id="active", thread_id="thread-a", status="completed"))
        self.conv.save_turn(TurnRecord(turn_id="old", thread_id="thread-a", status="superseded"))
        self.message("user", "active", "user", "查看 /Users/qa/private/input.txt 与 C:\\Users\\qa\\secret.txt", 1)
        self.message("answer", "active", "assistant", "最终结论 https://example.invalid/report", 2)
        self.message("hidden", "old", "assistant", "已替代的旧正文", 3)
        self.message("reasoning", "active", "assistant", "内部推理", 4, kind="reasoning")
        self.cfg = AuthConfig(password="synthetic-password", secret=b"synthetic-test-key")
        self.app = FastAPI()
        self.app.state.auth = self.cfg
        self.app.include_router(create_conversation_shares_router(service=self.service))
        @self.app.middleware("http")
        async def gate(request: Request, call_next):
            if not is_public_share_read(request.method, request.url.path):
                if not verify_token(self.cfg, bearer_from_header(request.headers.get("Authorization"))):
                    return JSONResponse({"error": "unauthorized"}, status_code=401)
            return await call_next(request)
        self.client = TestClient(self.app)
        self.operator = {"Authorization": f"Bearer {issue_token(self.cfg)}"}
        self.base = "/api/threads/thread-a/shares"

    def tearDown(self):
        self.client.close()
        self.platform.close()
        self.tmp.cleanup()

    def message(self, message_id, turn_id, role, text, seq, **kwargs):
        self.conv.save_message(ConversationMessage(message_id=message_id, thread_id="thread-a", turn_id=turn_id, role=role, text=text, stream_seq=seq, **kwargs))

    def add_attachment(self, content=b"source /Users/qa/secret/file.txt", *, turn_id="active"):
        digest = hashlib.sha256(content).hexdigest()
        self.conv.write_artifact_content(digest, content)
        self.platform.save(Artifact(sha256=digest, name="中文附件.txt", media_type="text/plain"))
        turn = self.conv.get_turn(turn_id)
        self.conv.save_turn(turn.model_copy(update={"attachments": [digest]}))
        self.platform.append_events([EventEnvelope(aggregate_type="thread", aggregate_id="thread-a", event_type="core.artifact.attached", payload={"sha256": digest, "turn_id": turn_id})])
        return digest

    def preview(self, **kwargs):
        response = self.client.post(self.base + "/preview", headers=self.operator, json=kwargs)
        self.assertEqual(response.status_code, 200, response.text)
        return response.json()

    def create(self, preview=None, mode="link"):
        preview = preview or self.preview()
        response = self.client.post(self.base, headers=self.operator, json={"preview_id": preview["preview_id"], "access_mode": mode, "expires_in_seconds": 3600})
        self.assertEqual(response.status_code, 201, response.text)
        share = response.json()
        return share, {"X-Muteki-Share-Token": share["token"]}

    def test_current_branch_preview_is_fixed_and_excludes_hidden_data(self):
        preview = self.preview()
        text = json.dumps(preview["snapshot"], ensure_ascii=False)
        self.assertEqual(len(preview["snapshot"]["messages"]), 2)
        self.assertNotIn("已替代", text)
        self.assertNotIn("内部推理", text)
        self.assertNotIn("/Users/qa", text)
        self.assertNotIn("C:\\\\Users", text)
        self.assertIn("https://example.invalid/report", text)
        self.assertNotIn("thread-a", text)
        self.assertEqual(self.client.get(self.base, headers=self.operator).json()["shares"], [])
        self.message("later", "active", "assistant", "未审阅的新正文", 5)
        share, headers = self.create(preview)
        response = self.client.get(f'/api/conversation-shares/{share["share_id"]}', headers=headers)
        self.assertEqual(response.status_code, 200)
        self.assertNotIn("未审阅", response.text)
        self.assertEqual(response.json()["snapshot"], preview["snapshot"])

    def test_selected_reply_and_context_scope_and_invalid_selection(self):
        preview = self.preview(message_ids=["answer"])
        self.assertEqual(len(preview["snapshot"]["messages"]), 1)
        response = self.client.post(self.base + "/preview", headers=self.operator, json={"message_ids": ["hidden"]})
        self.assertEqual(response.status_code, 400)
        self.assertEqual(self.client.post(self.base + "/preview", headers=self.operator, json={"message_ids": []}).status_code, 400)

    def test_scoped_attachment_is_copied_redacted_and_revoked_with_snapshot(self):
        digest = self.add_attachment()
        preview = self.preview(attachment_ids=[digest])
        item = preview["snapshot"]["attachments"][0]
        self.assertNotIn("/Users/qa", item["text"])
        share, headers = self.create(preview)
        source = self.conv.artifacts_dir / digest
        source.write_bytes(b"changed original contents")
        path = f'/api/conversation-shares/{share["share_id"]}/attachments/{item["id"]}'
        download = self.client.get(path, headers=headers)
        self.assertEqual(download.status_code, 200)
        self.assertEqual(download.text, item["text"])
        self.assertIn("filename*=UTF-8", download.headers["content-disposition"])
        self.assertEqual(self.client.delete(f'{self.base}/{share["share_id"]}', headers=self.operator).status_code, 200)
        self.assertEqual(self.client.get(path, headers=headers).status_code, 410)
        self.assertEqual(self.client.get(f'/api/conversation-shares/{share["share_id"]}', headers=headers).status_code, 410)
        self.assertEqual(source.read_bytes(), b"changed original contents")
        self.assertEqual(self.conv.get_message("answer").text, "最终结论 https://example.invalid/report")

    def test_missing_or_unselected_attachment_is_rejected(self):
        digest = self.add_attachment(turn_id="old")
        response = self.client.post(self.base + "/preview", headers=self.operator, json={"attachment_ids": [digest]})
        self.assertEqual(response.status_code, 400)
        response = self.client.post(self.base + "/preview", headers=self.operator, json={"attachment_ids": ["0" * 64]})
        self.assertEqual(response.status_code, 400)

    def test_attachment_and_snapshot_limits(self):
        digest = self.add_attachment(b"x" * (MAX_ATTACHMENT_BYTES + 1))
        snapshot, choices = build_share_snapshot(self.service, "thread-a", SharePreviewRequest())
        self.assertEqual(snapshot["attachments"], [])
        self.assertTrue(choices["attachments"][0]["unavailable_reason"])
        with self.assertRaises(HTTPException):
            build_share_snapshot(self.service, "thread-a", SharePreviewRequest(attachment_ids=[digest]))
        store = ConversationShareStore(self.root / "bounds.sqlite3")
        with self.assertRaises(HTTPException) as caught:
            store.preview("thread-a", {"text": "x" * MAX_SNAPSHOT_BYTES})
        self.assertEqual(caught.exception.status_code, 413)

    def test_access_modes_and_management_require_operator(self):
        share, headers = self.create(mode="authenticated")
        path = f'/api/conversation-shares/{share["share_id"]}'
        self.assertEqual(self.client.get(path, headers=headers).status_code, 401)
        self.assertEqual(self.client.get(path, headers={**headers, **self.operator}).status_code, 200)
        self.assertEqual(self.client.get(path).status_code, 404)
        self.assertEqual(self.client.get(self.base, headers=headers).status_code, 401)
        self.assertEqual(self.client.post(self.base + "/preview", headers=headers, json={}).status_code, 401)
        self.assertEqual(self.client.delete(f'{self.base}/{share["share_id"]}', headers=headers).status_code, 401)
        other, _ = self.create()
        self.assertEqual(self.client.get(f'/api/conversation-shares/{other["share_id"]}', headers=headers).status_code, 404)

    def test_expiration_invalidates_body_and_attachment(self):
        digest = self.add_attachment()
        share, headers = self.create(self.preview(attachment_ids=[digest]))
        path = f'/api/conversation-shares/{share["share_id"]}'
        item = self.client.get(path, headers=headers).json()["snapshot"]["attachments"][0]
        with patch("apps.web.conversation_shares.time.time", return_value=share["expires_at"] + 1):
            for url in [path, path + "/attachments/" + item["id"]]:
                response = self.client.get(url, headers=headers)
                self.assertEqual(response.status_code, 410)
                self.assertEqual(response.headers["cache-control"], "no-store")
                self.assertNotIn("最终结论", response.text)

    def test_exact_read_only_exemption(self):
        share_id = "a" * 24
        self.assertTrue(is_public_share_read("GET", f"/api/conversation-shares/{share_id}"))
        self.assertTrue(is_public_share_read("GET", f"/api/conversation-shares/{share_id}/attachments/" + "b" * 24))
        for method, path in [("POST", f"/api/conversation-shares/{share_id}"), ("DELETE", f"/api/conversation-shares/{share_id}"), ("GET", "/api/threads/thread-a"), ("GET", f"/api/conversation-shares/{share_id}/commands"), ("GET", f"/api/conversation-shares/{share_id}/attachments/unknown"), ("GET", "/api/conversation-shares/")]:
            self.assertFalse(is_public_share_read(method, path))

    def test_preview_expiry_and_create_ttl_bounds(self):
        preview = self.preview()
        response = self.client.post(self.base, headers=self.operator, json={"preview_id": preview["preview_id"], "access_mode": "link", "expires_in_seconds": 0})
        self.assertEqual(response.status_code, 422)
        with patch("apps.web.conversation_shares.time.time", return_value=preview["snapshot"]["captured_at"] + 3600):
            response = self.client.post(self.base, headers=self.operator, json={"preview_id": preview["preview_id"], "access_mode": "link", "expires_in_seconds": 3600})
            self.assertEqual(response.status_code, 409)

    def test_tool_summary_never_carries_parameters_or_output(self):
        self.platform.append_events([EventEnvelope(aggregate_type="thread", aggregate_id="thread-a", event_type="core.tool.completed", payload={"turn_id": "active", "tool_name": "shell", "status": "completed", "exit_code": 1, "input": {"secret": "synthetic-private-argument"}, "output": "synthetic-private-output"})])
        snapshot = self.preview(include_tool_summaries=True)["snapshot"]
        self.assertEqual(snapshot["tool_summaries"], [{"name": "shell", "status": "failed"}])
        self.assertNotIn("synthetic-private", json.dumps(snapshot))

    def test_long_thread_source_allows_bounded_selection(self):
        for i in range(501):
            self.message(f"extra-{i}", "active", "assistant", f"visible {i}", i + 10)
        result = self.client.get(self.base + "/source", headers=self.operator)
        self.assertEqual(result.status_code, 200)
        source = result.json()
        self.assertEqual(source["total_message_count"], 503)
        self.assertEqual(len(source["initial_message_ids"]), 500)
        self.assertEqual(len(source["choices"]["messages"]), 500)
        preview = self.preview(message_ids=source["initial_message_ids"])
        self.assertEqual(len(preview["snapshot"]["messages"]), 500)
        older = self.client.get(self.base + "/source?offset=500", headers=self.operator).json()
        self.assertEqual(len(older["choices"]["messages"]), 3)
        self.assertFalse(older["has_older"])
        selected = self.preview(message_ids=[older["choices"]["messages"][0]["id"]])
        self.assertEqual(len(selected["snapshot"]["messages"]), 1)


if __name__ == "__main__":
    unittest.main()
