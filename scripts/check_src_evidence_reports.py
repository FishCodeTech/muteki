"""Live local checks for SRC evidence, protocols and submission bundles.

Run from the repository root: uv run python scripts/check_src_evidence_reports.py
No external target, credential, model call or production data is used.
"""
from __future__ import annotations

import asyncio
from datetime import datetime, timedelta, timezone
from pathlib import Path
import sys
import tempfile

import httpx

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from muteki.platform.store import PlatformStore
from muteki.src import SrcService
from muteki.src.oast import SrcOastService
from muteki.src.store import SrcStoreError


class LocalHttpServer:
    def __init__(self) -> None:
        from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
        import threading

        self.requests: list[tuple[str, str, bytes]] = []
        owner = self

        class Handler(BaseHTTPRequestHandler):
            def _handle(self) -> None:
                length = int(self.headers.get("Content-Length") or 0)
                body = self.rfile.read(length) if length else b""
                owner.requests.append((self.command, self.path, body))
                if self.path.startswith("/slow"):
                    import time
                    time.sleep(0.5)
                if self.path.startswith("/redirect"):
                    self.send_response(302)
                    self.send_header("Location", "/echo")
                    self.send_header("Content-Length", "0")
                    self.end_headers()
                    return
                if self.path.startswith("/sse"):
                    payload = (
                        b"id: event-2\nevent: update\ndata: second\n\n"
                        if self.headers.get("Last-Event-ID") else
                        b"id: event-1\nevent: update\ndata: first\nretry: 100\n\nid: event-2\nevent: update\ndata: second\n\n"
                    )
                    if self.headers.get("Last-Event-ID"):
                        payload += b"id: event-3\nevent: update\ndata: third\n\n"
                    self.send_response(200)
                    self.send_header("Content-Type", "text/event-stream")
                    self.send_header("Content-Length", str(len(payload)))
                    self.end_headers()
                    self.wfile.write(payload)
                    return
                payload = body if self.path.startswith("/large") else b"ok"
                if self.path.startswith("/large"):
                    payload = b"x" * 128
                self.send_response(200)
                self.send_header("Content-Type", "application/octet-stream")
                self.send_header("X-Test", "one")
                self.send_header("X-Test", "two")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

            do_GET = _handle
            do_POST = _handle

            def log_message(self, *args) -> None:
                return

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    @property
    def port(self) -> int:
        return int(self.server.server_address[1])

    def count(self, path: str) -> int:
        return sum(1 for _method, request_path, _body in self.requests if request_path.startswith(path))

    def close(self) -> None:
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)


def make_src(root: Path, platform: PlatformStore) -> SrcService:
    class Conversation:
        def activate_thread(self, thread, principal, selection=None):
            platform.save(thread)

    return SrcService(
        platform, artifact_root=root / "cas", conversation=Conversation(), run_gateway=None,
    )


def make_task(src: SrcService, title: str = "evidence-fixture"):
    class Actor:
        id = "local-user"

    thread, task, binding = src.prepare_task(Actor(), {"title": title})
    src.commit_task(thread, task, binding, principal_id="local-user")
    return thread, task


async def check_oast(root: Path) -> None:
    platform = PlatformStore(root / "oast-platform.db")
    src = make_src(root, platform)
    _thread, task = make_task(src)
    service = SrcOastService(src)
    callback = await service.start_local_receiver()
    now = datetime.now(timezone.utc)
    first = await service.allocate(
        task.task_id, run_id="run-a", step_id="step-a", trigger_exchange_ids=["exchange-a"],
        protocols=["http"], expires_at=now + timedelta(minutes=5),
        authorization_ref="policy-v1", idempotency_key="oast-first",
    )
    repeated = await service.allocate(
        task.task_id, run_id="run-a", step_id="step-a", trigger_exchange_ids=["exchange-a"],
        protocols=["http"], expires_at=now + timedelta(minutes=5),
        authorization_ref="policy-v1", idempotency_key="oast-first",
    )
    assert repeated["interaction_id"] == first["interaction_id"]
    second = await service.allocate(
        task.task_id, trigger_exchange_ids=["exchange-b"], protocols=["http"],
        expires_at=now + timedelta(milliseconds=150), authorization_ref="policy-v1",
    )
    assert second["token"] != first["token"]

    async with httpx.AsyncClient(timeout=5) as client:
        health = await client.get(f"{callback}/health")
        assert health.status_code == 200 and health.json()["state"] == "ready"
        target = service.trigger_target(task.task_id, first["interaction_id"], "http")
        response = await client.get(target + "?source=controlled")
        assert response.status_code == 200 and response.json()["accepted"] is True
    await asyncio.sleep(0.2)
    late_time = datetime.now(timezone.utc)
    late = await service.record_interaction(
        second["token"], protocol="http", raw=b"POST /late HTTP/1.1\r\nHost: local\r\n\r\n",
        source_address="127.0.0.1:12345", request_path="/late",
        source_event_id="late-event-1", received_at=late_time,
    )
    assert late["late"] is True
    try:
        service.trigger_target(task.task_id, second["interaction_id"], "http")
    except SrcStoreError as exc:
        assert exc.code == "oast_expired"
    else:
        raise AssertionError("expired OAST token accepted a new trigger")

    duplicate = await service.record_interaction(
        second["token"], protocol="http", raw=b"POST /late HTTP/1.1\r\nHost: local\r\n\r\n",
        source_address="127.0.0.1:12345", request_path="/late",
        source_event_id="late-event-1", received_at=late_time,
    )
    assert duplicate["deduplicated"] is True
    page = service.events(task.task_id, first["interaction_id"], limit=1)
    assert page["complete"] is True and len(page["events"]) == 1
    raw_ref = page["events"][0]["raw_ref"]
    from muteki.src.contracts import ArtifactRef
    raw = src.artifacts.read(ArtifactRef.model_validate(raw_ref), expected_task_id=task.task_id)
    assert b"source=controlled" in raw and raw.startswith(b"GET /")

    await service.close()
    restarted = SrcOastService(src)
    restarted_duplicate = await restarted.record_interaction(
        second["token"], protocol="http", raw=b"POST /late HTTP/1.1\r\nHost: local\r\n\r\n",
        source_address="127.0.0.1:12345", request_path="/late",
        source_event_id="late-event-1", received_at=late_time,
    )
    assert restarted_duplicate["deduplicated"] is True
    await restarted.close()
    platform.close()
    print("PASS: OAST live callback, token identity, late/expiry, raw bytes and restart dedup")


async def check_http_workspace(root: Path) -> None:
    from muteki.src.exchanges import ExchangeService

    local = LocalHttpServer()
    platform = PlatformStore(root / "http-platform.db")
    src = make_src(root, platform)
    _thread, task = make_task(src, "http-fixture")
    base = f"http://127.0.0.1:{local.port}"
    src.execution.ensure_default_policy(task.task_id, [base + "/"], "local-user")
    service = ExchangeService(src)
    body = b"a=1&a=2\x00\xff"
    raw = (
        f"POST /echo?a=1&a=2&encoded=%2f HTTP/1.1\r\nHost: 127.0.0.1:{local.port}\r\n"
        f"X-Test: one\r\nX-Test: two\r\nContent-Type: application/octet-stream\r\nContent-Length: {len(body)}\r\n\r\n"
    ).encode() + body
    response_raw = b"HTTP/1.1 200 OK\r\nX-Test: one\r\nX-Test: two\r\nContent-Length: 2\r\n\r\nok"
    original = service.record(
        task.task_id, source_kind="import", actor_ref="human", method="POST",
        observed_url=base + "/echo?a=1&a=2&encoded=%2f", request_raw=raw,
        response_raw=response_raw, status_code=200,
    )
    first = service.save_request_draft(task.task_id, original.exchange_id, 1, editor="human", idempotency_key="draft-1")
    assert first["draft"]["revision"] == 1
    second = service.save_request_draft(task.task_id, original.exchange_id, 1, editor="agent", idempotency_key="draft-2")
    assert second["draft"]["revision"] == 2
    try:
        service.save_request_draft(task.task_id, original.exchange_id, 1, editor="other", idempotency_key="draft-stale")
    except SrcStoreError as exc:
        assert exc.code == "version_conflict"
    else:
        raise AssertionError("stale draft revision overwrote another editor")
    draft_id = second["draft"]["draft_id"]
    policy_version = src.execution.current_policy(task.task_id).version
    executed = await service.execute_request_draft(
        task.task_id, draft_id, 2, identity_ref="tester-a", authorization_version=policy_version,
        idempotency_key="replay-1", expected_side_effect="controlled write", budget={"timeout_seconds": 2},
    )
    repeated = await service.execute_request_draft(
        task.task_id, draft_id, 2, identity_ref="tester-a", authorization_version=policy_version,
        idempotency_key="replay-1", expected_side_effect="controlled write", budget={"timeout_seconds": 2},
    )
    assert repeated["execution_id"] == executed["execution_id"]
    assert executed["execution_state"] == "completed" and local.count("/echo") == 1
    replay = service.read_exchange(task.task_id, executed["exchange_id"])
    assert replay["exchange"]["replay_of"] == original.exchange_id
    assert replay["request_raw"] == raw and replay["exchange"]["status_code"] == 200
    assert replay["exchange"]["request_headers"].count(["X-Test", "one"]) == 1
    assert replay["exchange"]["response_headers"].count(["X-Test", "one"]) == 1
    assert replay["exchange"]["response_headers"].count(["X-Test", "two"]) == 1
    comparison = service.compare_exchanges(
        task.task_id, original.exchange_id, executed["exchange_id"],
        baseline_context={"identity": "a", "tenant": "one", "object_version": "v1"},
        experiment_context={"identity": "b", "tenant": "two", "object_version": "v2"},
    )
    assert comparison["single_variable"] is False and comparison["warnings"]
    queried = service.query_exchanges(task.task_id, {"method": "POST", "parameter": "a", "parameter_location": "query"}, limit=1)
    assert queried["returned"] == 1 and queried["next_cursor"]
    exported = service.export_exchanges(task.task_id, filters={"method": "POST"}, format="jsonl")
    assert exported["manifest"]["actual"] >= 2

    redirect_raw = (
        f"POST /redirect HTTP/1.1\r\nHost: 127.0.0.1:{local.port}\r\nContent-Length: 0\r\n\r\n"
    ).encode()
    redirect = service.record(
        task.task_id, source_kind="import", actor_ref="human", method="POST",
        observed_url=base + "/redirect", request_raw=redirect_raw,
        response_raw=b"HTTP/1.1 302 Found\r\nLocation: /echo\r\nContent-Length: 0\r\n\r\n", status_code=302,
    )
    redirect_draft = service.save_request_draft(task.task_id, redirect.exchange_id, 1, editor="human", idempotency_key="redirect-draft")
    redirected = await service.execute_request_draft(
        task.task_id, redirect_draft["draft"]["draft_id"], 1, identity_ref="tester-a",
        authorization_version=policy_version, idempotency_key="redirect-replay",
        budget={"max_redirects": 1, "timeout_seconds": 2},
    )
    assert len(redirected["redirect_results"]) == 1
    child = redirected["redirect_results"][0]
    assert child["execution_state"] == "completed"
    child_exchange = service.read_exchange(task.task_id, child["exchange_id"])
    assert len(child_exchange["redirect_chain"]) == 2
    assert child_exchange["exchange"]["redirect_parent"] == child_exchange["redirect_chain"][0]

    slow_raw = (
        f"POST /slow HTTP/1.1\r\nHost: 127.0.0.1:{local.port}\r\nContent-Length: 1\r\n\r\nx"
    ).encode()
    slow = service.record(task.task_id, source_kind="import", actor_ref="human", method="POST",
                          observed_url=base + "/slow", request_raw=slow_raw)
    slow_draft = service.save_request_draft(task.task_id, slow.exchange_id, 1, editor="human", idempotency_key="slow-draft")
    unknown = await service.execute_request_draft(
        task.task_id, slow_draft["draft"]["draft_id"], 1, identity_ref="tester-a",
        authorization_version=policy_version, idempotency_key="slow-replay",
        budget={"timeout_seconds": 0.05},
    )
    assert unknown["execution_state"] == "outcome_unknown"
    unknown_exchange = service.read_exchange(task.task_id, unknown["exchange_id"])
    assert unknown_exchange["exchange"]["execution_state"] == "outcome_unknown"
    before = local.count("/slow")
    unknown_repeated = await service.execute_request_draft(
        task.task_id, slow_draft["draft"]["draft_id"], 1, identity_ref="tester-a",
        authorization_version=policy_version, idempotency_key="slow-replay",
        budget={"timeout_seconds": 0.05},
    )
    assert unknown_repeated["execution_id"] == unknown["execution_id"] and local.count("/slow") == before

    large = service.record(task.task_id, source_kind="import", actor_ref="human", method="POST",
                           observed_url=base + "/large", request_raw=slow_raw.replace(b"/slow", b"/large"))
    large_draft = service.save_request_draft(task.task_id, large.exchange_id, 1, editor="human", idempotency_key="large-draft")
    partial = await service.execute_request_draft(
        task.task_id, large_draft["draft"]["draft_id"], 1, identity_ref="tester-a",
        authorization_version=policy_version, idempotency_key="large-replay",
        budget={"timeout_seconds": 2, "capture_limit": 4},
    )
    partial_exchange = service.read_exchange(task.task_id, partial["exchange_id"])
    assert partial_exchange["exchange"]["capture_state"] == "partial"
    local.close()
    platform.close()
    print("PASS: HTTP raw evidence, shared drafts, replay/idempotency, redirect, timeout and partial capture")


async def check_streams(root: Path) -> None:
    from muteki.src.streams import SrcStreamService
    from websockets.asyncio.server import serve

    local = LocalHttpServer()
    platform = PlatformStore(root / "stream-platform.db")
    src = make_src(root, platform)
    _thread, task = make_task(src, "stream-fixture")
    base = f"http://127.0.0.1:{local.port}"
    src.execution.ensure_default_policy(task.task_id, [base + "/", f"ws://127.0.0.1:{local.port}/"], "local-user")
    service = SrcStreamService(src)

    async def handler(connection) -> None:
        async for message in connection:
            await connection.send(message)
            if connection.request.path == "/abnormal":
                await connection.close(code=1011, reason="controlled disconnect")
                return

    server = await serve(handler, "127.0.0.1", 0)
    port = server.sockets[0].getsockname()[1]
    src.execution.revise_policy(
        task.task_id, {"authorized_targets": [base + "/", f"ws://127.0.0.1:{port}/"]},
        actor_id="local-user", environment_capabilities=src.execution.environment_capabilities(),
    )
    first = await service.connect_websocket(task.task_id, f"ws://127.0.0.1:{port}/normal", authorization_ref="policy-v2")
    sent = await service.send_websocket(task.task_id, first["stream_id"], "hello")
    assert sent["opcode"] == 1 and sent["state"] == "sent"
    received = await service.receive_websocket(task.task_id, first["stream_id"])
    assert received["state"] == "received"
    binary = await service.send_websocket(task.task_id, first["stream_id"], b"\x00\xff")
    assert binary["opcode"] == 2
    binary_received = await service.receive_websocket(task.task_id, first["stream_id"])
    assert binary_received["opcode"] == 2
    await service.close_websocket(task.task_id, first["stream_id"])

    abnormal = await service.connect_websocket(
        task.task_id, f"ws://127.0.0.1:{port}/abnormal", authorization_ref="policy-v2",
        replay_of=first["stream_id"], idempotency_key="ws-reconnect",
    )
    assert abnormal["connection_index"] == 2 and abnormal["root_stream_id"] == first["stream_id"]
    await service.send_websocket(task.task_id, abnormal["stream_id"], b"\x00\xff")
    echoed = await service.receive_websocket(task.task_id, abnormal["stream_id"])
    assert echoed["opcode"] == 2 and echoed["state"] == "received"
    closed = await service.receive_websocket(task.task_id, abnormal["stream_id"])
    assert closed["code"] == 1011
    exported = service.export(task.task_id, first["stream_id"])
    assert exported["complete"] is False and len(exported["connections"]) == 2 and exported["gaps"]
    server.close()
    await server.wait_closed()

    sse = await service.connect_sse(task.task_id, base + "/sse", authorization_ref="policy-v2")
    one = await service.read_sse(task.task_id, sse["stream_id"])
    two = await service.read_sse(task.task_id, sse["stream_id"])
    assert one["id"] == "event-1" and one["retry"] == 100 and two["id"] == "event-2", (one, two)
    assert (await service.read_sse(task.task_id, sse["stream_id"]))["state"] == "closed"
    replay = await service.connect_sse(
        task.task_id, base + "/sse", authorization_ref="policy-v2",
        replay_of=sse["stream_id"], idempotency_key="sse-reconnect",
    )
    replayed = await service.read_sse(task.task_id, replay["stream_id"])
    assert replayed["id"] == "event-2" and replayed["delivery_state"] == "replayed" and replayed["duplicate_of"]
    third = await service.read_sse(task.task_id, replay["stream_id"])
    assert third["id"] == "event-3" and third["delivery_state"] == "received"
    await service.close_sse(task.task_id, replay["stream_id"])
    stream_export = service.export(task.task_id, sse["stream_id"])
    assert len(stream_export["connections"]) == 2
    await service.close()
    local.close()
    platform.close()
    print("PASS: WebSocket text/binary/abnormal reconnect and SSE replay/order/export")


async def check_report_bundle(root: Path) -> None:
    import hashlib
    import io
    import json
    import zipfile
    from PIL import Image
    from muteki.src.reports import SrcReportService
    from muteki.src.submission import SrcSubmissionService, execute_packaged_poc, verify_bundle

    platform = PlatformStore(root / "report-platform.db")
    src = make_src(root, platform)
    _thread, task = make_task(src, "report-fixture")
    reports = SrcReportService(src.store, src.artifacts)
    submission = SrcSubmissionService(reports, src.artifacts)
    request_bytes = (
        b"POST /api/item?a=1&a=2&encoded=%2f HTTP/1.1\r\n"
        b"Host: example.test\r\nX-Test: one\r\nX-Test: two\r\n"
        b"Cookie: sid=original-value\r\nContent-Length: 12\r\n\r\n"
        b"a=1&a=2\x00\xff"
    )
    response_bytes = b"HTTP/1.1 200 OK\r\nX-Test: one\r\nX-Test: two\r\nContent-Length: 6\r\n\r\n\xe5\x8f\x8d\xe8\xaf\x81"
    request = src.artifacts.put(task.task_id, request_bytes, media_type="message/http", name="request.http")
    response = src.artifacts.put(task.task_id, response_bytes, media_type="message/http", name="response.http")
    script = (
        "import pathlib,sys\n"
        "data=pathlib.Path(sys.argv[1]).read_bytes()\n"
        "assert b'a=1&a=2&encoded=%2f' in data\n"
        "assert b'X-Test: one\\r\\nX-Test: two' in data\n"
        "assert b'Cookie: sid=original-value' in data\n"
        "assert data.endswith(b'a=1&a=2\\x00\\xff')\n"
        "print('POC_OK')\n"
    )
    poc = src.artifacts.put(task.task_id, script.encode(), media_type="text/x-python", name="poc.py")
    image_buffer = io.BytesIO()
    Image.new("RGB", (3, 2), (12, 34, 56)).save(image_buffer, format="PNG")
    image = src.artifacts.put(task.task_id, image_buffer.getvalue(), media_type="image/png", name="proof.png")
    poc_dir = f"evidence/{poc.sha256[:16]}"
    document = {
        "title": "Byte-sensitive `fixture` & 验证", "target": "https://example.test/api",
        "target_version": "2026-10-26", "scope": "example.test", "attacker": "test account",
        "function": "item update", "mechanism": "controlled fixture",
        "normal_behavior": "normal", "actual_behavior": "experiment",
        "steps": [{"action": "send raw request", "identity": "tester-a", "input": "request.http",
                   "expected": "raw bytes preserved", "actual": "assertion passed", "evidence": [request.artifact_id]}],
        "decisive_evidence": [request.artifact_id, response.artifact_id],
        "confirmed_impact": "controlled impact", "conditional_impact": "none beyond fixture",
        "cvss": {"vector": "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:L/I:L/A:N"},
        "remediation": "preserve raw bytes", "counterevidence": ["no production effect"],
        "limitations": ["local fixture only"], "restoration": {"status": "restored"},
        "attachments": [image.artifact_id],
        "exchanges": [{
            "exchange_id": "exchange-raw", "method": "POST", "observed_url": "https://example.test/api/item?a=1&a=2&encoded=%2f",
            "request_raw_ref": request.model_dump(mode="json"), "response_raw_ref": response.model_dump(mode="json"),
            "request_headers": [["X-Test", "one"], ["X-Test", "two"]],
            "response_headers": [["X-Test", "one"], ["X-Test", "two"]],
            "query": [["a", "1"], ["a", "2"]], "status_code": 200, "protocol": "http/1.1",
            "started_at": "2026-10-26T00:00:00Z", "duration_ms": 1, "capture_state": "complete",
        }],
        "poc": {
            "content_ref": poc.model_dump(mode="json"), "language": "python",
            "dependencies": ["Python 3"], "initialization": "none",
            "command": ["{python}", f"{poc_dir}/poc.py", f"evidence/{request.sha256[:16]}/request.http"],
            "success_assertion": {"exit_code": 0, "stdout_contains": "POC_OK"},
            "cleanup": "remove temporary directory", "verification": {"status": "verified", "stdout": "POC_OK"},
        },
    }
    evidence = [request, response, poc, image]
    claims = [{"claim_id": "claim-raw", "status": "observed", "evidence_refs": [request.artifact_id]}]
    image_review = reports.record_image_review(
        task.task_id, image, reviewer="fresh-reviewer", judgment="controlled image inspected",
    )
    assert image_review["status"] == "reviewed"
    base = reports.freeze(
        task.task_id, finding_id="finding-raw", source_run_id="run-a", document=document,
        evidence_refs=evidence, claims=claims,
        review={"validation_type": "artifact_review", "readiness": {"status": "YELLOW"}},
        cvss=document["cvss"], deduplication={"status": "independent"}, template="generic-zh-v1",
        plan_snapshot={"version": 1}, authorization_snapshot={"version": 1},
        source_watermarks={"run-a": 10, "run-b": 20}, idempotency_key="freeze-raw-base", actor_id="local-user",
    )
    reproduction = reports.record_independent_reproduction(
        task.task_id, base["report_id"], reviewer="fresh-reviewer", original_researcher="original-researcher",
        executed_steps=["send raw request", "assert raw bytes"], skipped_steps=[], success=True,
        execution={"command": ["python", "poc.py", "request.http"], "exit_code": 0, "stdout": "POC_OK"},
        evidence_refs=evidence,
    )
    green_review = {
        "validation_type": "live_reproduction", "readiness": {"status": "GREEN"},
        "independent_reproduction": {
            "reproduction_id": reproduction["reproduction_id"], "success": True,
            "reviewer": "fresh-reviewer",
        },
    }
    frozen = reports.freeze(
        task.task_id, finding_id="finding-raw", source_run_id="run-a", document=document,
        evidence_refs=evidence, claims=claims, review=green_review,
        cvss=document["cvss"], deduplication={"status": "independent"}, template="generic-zh-v1",
        plan_snapshot={"version": 1}, authorization_snapshot={"version": 1},
        source_watermarks={"run-a": 10, "run-b": 20}, idempotency_key="freeze-raw",
        actor_id="local-user", supersedes=base["report_id"],
    )
    repeated = reports.freeze(
        task.task_id, finding_id="finding-raw", source_run_id="run-a", document=document,
        evidence_refs=evidence, claims=claims, review=green_review,
        cvss=document["cvss"], deduplication={"status": "independent"}, template="generic-zh-v1",
        plan_snapshot={"version": 1}, authorization_snapshot={"version": 1},
        source_watermarks={"run-a": 10, "run-b": 20}, idempotency_key="freeze-raw",
        actor_id="local-user", supersedes=base["report_id"],
    )
    assert repeated["report_id"] == frozen["report_id"]
    raw = submission.export(task.task_id, frozen["report_id"], format="raw")
    assert raw["bytes"] == request_bytes + b"\r\n\r\n" + response_bytes
    har = submission.export(task.task_id, frozen["report_id"], format="har")
    import base64
    har_entry = json.loads(har["bytes"])["log"]["entries"][0]
    assert base64.b64decode(har_entry["request"]["postData"]["text"]) == b"a=1&a=2\x00\xff"
    assert base64.b64decode(har_entry["_muteki"]["request_raw_base64"]) == request_bytes
    jsonl = submission.export(task.task_id, frozen["report_id"], format="jsonl")
    assert len(jsonl["bytes"].splitlines()) == 1 + 1 + 1 + len(evidence)
    bundle = submission.export(task.task_id, frozen["report_id"], format="zip")
    verification = verify_bundle(bundle["bytes"])
    assert verification["checked_images"] == 1 and bundle["complete"] is True
    executed = execute_packaged_poc(bundle["bytes"])
    assert executed["success"] is True and "POC_OK" in executed["stdout"]
    with zipfile.ZipFile(io.BytesIO(bundle["bytes"])) as archive:
        markdown = archive.read("report.md").decode()
        assert "Cookie: sid=original-value" in markdown and "Byte-sensitive `fixture` & 验证" in markdown
        manifest = json.loads(archive.read("manifest.json"))
        for entry in manifest["attachments"]:
            content = archive.read(entry["path"])
            assert hashlib.sha256(content).hexdigest() == entry["sha256"]
    redacted = submission.export(task.task_id, frozen["report_id"], format="zip", redactions=[{"value": "original-value", "replacement": "redacted"}])
    with zipfile.ZipFile(io.BytesIO(redacted["bytes"])) as archive:
        assert b"sid=redacted" in archive.read(f"evidence/{request.sha256[:16]}/request.http")
    assert src.artifacts.read(request, expected_task_id=task.task_id) == request_bytes
    (src.artifacts.path(response)).unlink()
    try:
        submission.export(task.task_id, frozen["report_id"], format="zip")
    except SrcStoreError as exc:
        assert exc.code == "artifact_missing"
    else:
        raise AssertionError("missing evidence was exported as complete")
    partial = submission.export(task.task_id, frozen["report_id"], format="zip", allow_partial=True)
    assert partial["complete"] is False and partial["manifest"]["status"] == "partial"
    verify_bundle(partial["bytes"], require_complete=False)
    platform.close()
    print("PASS: raw/HAR/JSONL, byte-sensitive ZIP PoC, image/hash, redaction and partial bundle")


async def check() -> None:
    failures: list[str] = []
    for scenario in (check_oast, check_http_workspace, check_streams, check_report_bundle):
        with tempfile.TemporaryDirectory(prefix="muteki-src-evidence-") as temp:
            try:
                await scenario(Path(temp))
            except Exception as exc:
                failures.append(f"{scenario.__name__}: {type(exc).__name__}: {exc}")
                print(f"FAIL: {failures[-1]}")
    if failures:
        raise RuntimeError("SRC evidence checks failed: " + " | ".join(failures))


if __name__ == "__main__":
    asyncio.run(check())
