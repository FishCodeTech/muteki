#!/usr/bin/env python3
"""Exercise actual TLS/WSS identity lifecycle on an isolated Web and Runner.

Requires Python websockets and openssl. No cloud account or model credential is used.
With --execution, exercises scoped shell execution, workspace inputs, artifacts,
credential projection/refresh/revoke and lease stopping in both container modes.
--credentials-only limits the Docker fixture to credential lifecycle checks.
Normal task/model scheduling, real providers and external NAT are not covered.
"""
import argparse
import asyncio
import base64
import hashlib
import http.client
import json
import os
from pathlib import Path
import signal
import socket
import ssl
import subprocess
import tempfile
import time
import threading
import urllib.error
import urllib.parse
import urllib.request
import uuid
import websockets
from verify_runner_ipc import read
from runner_tls_fault_proxy import FaultProxy
from verify_runner_credentials import exercise as exercise_credentials


def port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def request(url, body=None, context=None, timeout=5):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(url, data=data, headers={"Content-Type": "application/json", "X-Muteki-CSRF": "1"})
    try:
        with urllib.request.urlopen(req, context=context, timeout=timeout) as result:
            return result.status, json.load(result)
    except urllib.error.HTTPError as error:
        return error.code, json.load(error)


def auth(lease):
    encoded = base64.urlsafe_b64encode(json.dumps(lease).encode()).decode().rstrip("=")
    return {"Authorization": "Bearer " + encoded}


def raw_request(url, method="GET", body=None, headers=None, context=None):
    request_headers = {"X-Muteki-CSRF": "1", **(headers or {})}
    req = urllib.request.Request(url, method=method, data=body, headers=request_headers)
    try:
        with urllib.request.urlopen(req, context=context, timeout=10) as result:
            return result.status, result.headers, result.read()
    except urllib.error.HTTPError as error:
        return error.code, error.headers, error.read()


def exercise_transfer_http(web, remote, context, lease, bulk_full):
    payload = bytes(range(256)) * 8192 + b"final-transfer-block"
    identity = "transfer-" + uuid.uuid4().hex
    manifest = {"id": identity, "scope": {"node_id": lease["node_id"], "project_id": "acceptance-project",
                "run_id": "transfer-fixture", "generation": 1}, "path": "inputs/payload.bin", "size": len(payload),
                "sha256": hashlib.sha256(payload).hexdigest(), "mode": 420}
    status, row = request(web + "/api/runner/transfers", manifest)
    assert status == 200 and row["received"] == 0, row
    source = remote + "/v1/runner/transfers/" + identity
    assert raw_request(source, context=context)[0] == 401
    node_headers = auth(lease)
    assert raw_request(source + "/chunks?offset=0&length=10", headers=node_headers, context=context)[0] == 409
    chunk_size = 1024 * 1024
    first = payload[:chunk_size]
    parsed = urllib.parse.urlsplit(web)
    slow = []
    try:
        for _ in range(4):
            connection = http.client.HTTPConnection(parsed.hostname, parsed.port, timeout=3)
            connection.putrequest("PUT", "/api/runner/transfers/" + identity + "/chunks?offset=0")
            for key, value in {"Content-Length": str(chunk_size), "Content-Type": "application/octet-stream",
                               "X-Muteki-CSRF": "1", "X-Muteki-SHA256": hashlib.sha256(first).hexdigest()}.items():
                connection.putheader(key, value)
            connection.endheaders()
            slow.append(connection)
        deadline = time.monotonic() + 3
        while request(web + "/api/runner/transfers/" + identity)[0] != 429:
            assert time.monotonic() < deadline
            time.sleep(0.02)
        bulk_full.set()
        fifth = http.client.HTTPConnection(parsed.hostname, parsed.port, timeout=3)
        slow.append(fifth)
        fifth.putrequest("PUT", "/api/runner/transfers/" + identity + "/chunks?offset=0")
        for key, value in {"Content-Length": str(chunk_size), "X-Muteki-CSRF": "1", "X-Muteki-SHA256": hashlib.sha256(first).hexdigest()}.items():
            fifth.putheader(key, value)
        fifth.endheaders()
        assert fifth.getresponse().status == 429
        time.sleep(1)
    finally:
        bulk_full.clear()
        for connection in slow:
            connection.close()
    deadline = time.monotonic() + 3
    while True:
        status, row = request(web + "/api/runner/transfers/" + identity)
        if status == 200:
            assert row["received"] == 0
            break
        assert time.monotonic() < deadline
        time.sleep(0.02)
    upload = web + "/api/runner/transfers/" + identity + "/chunks?offset="
    headers = {"Content-Type": "application/octet-stream", "X-Muteki-SHA256": hashlib.sha256(first).hexdigest()}
    status, _, body = raw_request(upload + "0", "PUT", first, headers)
    assert status == 200 and json.loads(body)["received"] == chunk_size
    assert request(web + "/api/runner/transfers/" + identity)[1]["received"] == chunk_size
    assert raw_request(upload + "0", "PUT", first, headers)[0] == 200
    assert raw_request(upload + str(chunk_size), "PUT", b"bad", {**headers, "X-Muteki-SHA256": "0" * 64})[0] == 409
    assert raw_request(upload + str(chunk_size), "PUT", first + b"x", headers)[0] == 413
    for offset in range(chunk_size, len(payload), chunk_size):
        chunk = payload[offset:offset + chunk_size]
        status, _, body = raw_request(upload + str(offset), "PUT", chunk,
            {"Content-Type": "application/octet-stream", "X-Muteki-SHA256": hashlib.sha256(chunk).hexdigest()})
        assert status == 200, body
    status, row = request(web + "/api/runner/transfers/" + identity + "/seal", {})
    assert status == 200 and row["state"] == "verified", row
    downloaded = bytearray()
    for offset in range(0, len(payload), chunk_size):
        status, received_headers, chunk = raw_request(source + f"/chunks?offset={offset}&length={chunk_size}", headers=node_headers, context=context)
        assert status == 200 and received_headers["X-Muteki-SHA256"] == hashlib.sha256(chunk).hexdigest()
        assert int(received_headers["X-Muteki-Offset"]) == offset
        downloaded.extend(chunk)
    assert bytes(downloaded) == payload
    assert raw_request(source + f"/chunks?offset=0&length={chunk_size + 1}", headers=node_headers, context=context)[0] == 409
    assert raw_request(source + f"/chunks?offset={len(payload) + 1}&length=10", headers=node_headers, context=context)[0] == 409
    other_token = request(web + "/api/runner/enrollments", {"name": "Other file node", "projects": ["acceptance-project"], "ttl_seconds": 60})[1]["token"]
    other_credential = request(remote + "/v1/runner/enroll", {"token": other_token, "protocol_version": 1}, context)[1]["credential"]
    other_lease = request(remote + "/v1/runner/session", {"credential": other_credential, "protocol_version": 1}, context)[1]["lease"]
    assert raw_request(source + "/chunks?offset=0&length=10", headers=auth(other_lease), context=context)[0] == 403
    wrong = json.loads(json.dumps(manifest)); wrong["id"] += "-wrong"; wrong["scope"]["project_id"] = "unregistered-project"
    assert request(web + "/api/runner/transfers", wrong)[0] == 409
    assert request(web + "/api/runner/transfers/" + identity + "/cancel", {})[0] == 200
    assert raw_request(source + "/chunks?offset=0&length=10", headers=node_headers, context=context)[0] == 409
    return {"https_file_bytes": len(payload), "http_cursor_resume": True, "duplicate_chunk_idempotent": True,
            "unsealed_download_denied": True, "bad_chunk_digest_denied": True, "oversized_chunk_denied": True,
            "authenticated_bulk_download_verified": True, "cross_node_file_read_denied": True,
            "invalid_download_range_denied": True, "cancelled_source_not_downloadable": True}, identity


async def exercise(web, remote, context):
    status, issued = request(web + "/api/runner/enrollments", {"name": "Acceptance node", "projects": ["acceptance-project"], "ttl_seconds": 60})
    assert status == 200
    status, enrolled = request(remote + "/v1/runner/enroll", {"token": issued["token"], "protocol_version": 1}, context)
    assert status == 200
    credential = enrolled["credential"]
    assert request(remote + "/v1/runner/enroll", {"token": issued["token"], "protocol_version": 1}, context)[0] == 401
    forged = dict(credential, secret="invalid")
    assert request(remote + "/v1/runner/session", {"credential": forged, "protocol_version": 1}, context)[0] == 401
    assert request(remote + "/v1/runner/session", {"credential": credential, "protocol_version": 999}, context)[0] == 409
    status, session = request(remote + "/v1/runner/session", {"credential": credential, "protocol_version": 1}, context)
    assert status == 200
    lease = session["lease"]
    assert request(remote + "/v1/runner/session", {"credential": credential, "protocol_version": 1}, context)[0] == 401
    address = remote.replace("https://", "wss://") + "/v1/runner/connect"
    async with websockets.connect(address, ssl=context, additional_headers=auth(lease)) as ws:
        hello = json.loads(await ws.recv())
        assert hello["type"] == "hello" and hello["generation"] == lease["generation"]
        await ws.send("heartbeat")
        assert json.loads(await ws.recv())["type"] == "lease"
        try:
            async with websockets.connect(address, ssl=context, additional_headers=auth(lease)):
                raise AssertionError("Duplicate node socket accepted")
        except websockets.exceptions.InvalidStatus as error:
            assert error.response.status_code == 409
    await asyncio.sleep(0.1)
    async with websockets.connect(address, ssl=context, additional_headers=auth(lease)) as ws:
        assert json.loads(await ws.recv())["generation"] == lease["generation"]
        status, rotated = request(web + "/api/runner/nodes/" + credential["node_id"] + "/rotate", {})
        assert status == 200
        await ws.send("heartbeat")
        try:
            await ws.recv()
            raise AssertionError("Old socket renewed after rotation")
        except websockets.exceptions.ConnectionClosed:
            pass
    assert request(remote + "/v1/runner/session", {"credential": credential, "protocol_version": 1}, context)[0] == 401
    status, next_session = request(remote + "/v1/runner/session", {"credential": rotated, "protocol_version": 1}, context)
    assert status == 200 and next_session["lease"]["generation"] > lease["generation"]
    async with websockets.connect(address, ssl=context, additional_headers=auth(next_session["lease"])) as ws:
        await ws.recv()
        bulk_full = threading.Event()
        transfer_task = asyncio.create_task(asyncio.to_thread(exercise_transfer_http, web, remote, context, next_session["lease"], bulk_full))
        heartbeats = 0
        heartbeats_while_full = 0
        while not transfer_task.done():
            await ws.send("heartbeat")
            assert json.loads(await ws.recv())["type"] == "lease"
            heartbeats += 1
            if bulk_full.is_set():
                heartbeats_while_full += 1
            await asyncio.sleep(0.1)
        transfer_result, transfer_id = await transfer_task
        transfer_result["heartbeats_during_transfer"] = heartbeats
        transfer_result["heartbeats_while_upload_slots_full"] = heartbeats_while_full
        assert heartbeats_while_full > 0
        transfer_result["upload_buffer_admission_bounded"] = True
        status, revoked = request(web + "/api/runner/nodes/" + credential["node_id"] + "/state", {"state": "revoked"})
        assert status == 200 and revoked["execution_stop_confirmed"] is False
        await ws.send("heartbeat")
        try:
            await ws.recv()
            raise AssertionError("Revoked socket renewed")
        except websockets.exceptions.ConnectionClosed:
            pass
    assert request(remote + "/v1/runner/session", {"credential": rotated, "protocol_version": 1}, context)[0] == 401
    status, nodes = request(web + "/api/runner/nodes")
    assert status == 200 and next(node for node in nodes["nodes"] if node["id"] == credential["node_id"])["state"] == "revoked"
    assert raw_request(remote + "/v1/runner/transfers/" + transfer_id + "/chunks?offset=0&length=10", headers=auth(next_session["lease"]), context=context)[0] == 401
    transfer_result["revoked_file_read_denied"] = True
    assert "_hash" not in json.dumps(nodes) and credential["secret"] not in json.dumps(nodes)
    return {"single_use_enrollment": True, "forged_identity_rejected": True,
            "protocol_mismatch_rejected": True, "duplicate_session_rejected": True,
            "duplicate_socket_rejected": True, "same_lease_reconnect": True,
            "rotation_fences_old_credential_and_socket": True,
            "revocation_fences_new_requests_and_socket": True,
            "revocation_does_not_claim_execution_stopped": True, **transfer_result}


def staged_input(web, node, run, payload):
    manifest = {"id": "input-" + uuid.uuid4().hex, "scope": {"node_id": node, "project_id": "acceptance-project", "run_id": run, "generation": 1},
                "path": "inputs/remote.bin", "size": len(payload), "sha256": hashlib.sha256(payload).hexdigest(), "mode": 420}
    status, result = request(web + "/api/runner/transfers", manifest)
    assert status == 200, result
    for offset in range(0, len(payload), 1024 * 1024):
        block = payload[offset:offset + 1024 * 1024]
        status, _, result = raw_request(web + "/api/runner/transfers/" + manifest["id"] + f"/chunks?offset={offset}", "PUT", block,
            {"Content-Type": "application/octet-stream", "X-Muteki-SHA256": hashlib.sha256(block).hexdigest()})
        assert status == 200, result
    assert request(web + "/api/runner/transfers/" + manifest["id"] + "/seal", {})[0] == 200
    return manifest


def held_uploads(web, transfer_id):
    parsed = urllib.parse.urlsplit(web)
    connections = []
    try:
        for _ in range(4):
            connection = http.client.HTTPConnection(parsed.hostname, parsed.port, timeout=3)
            connection.putrequest("PUT", "/api/runner/transfers/" + transfer_id + "/chunks?offset=0")
            for key, value in {"Content-Length": "1048576", "Expect": "100-continue", "X-Muteki-CSRF": "1", "X-Muteki-SHA256": "0" * 64}.items():
                connection.putheader(key, value)
            connection.endheaders(); connections.append(connection)
            acknowledged = bytearray()
            while not acknowledged.endswith(b"\r\n\r\n"):
                byte = connection.sock.recv(1)
                assert byte, "Upload closed before body admission"
                acknowledged.extend(byte)
            assert acknowledged.startswith(b"HTTP/1.1 100"), acknowledged.decode()
        deadline = time.monotonic() + 3
        while request(web + "/api/runner/transfers/" + transfer_id)[0] != 429:
            assert time.monotonic() < deadline
            time.sleep(0.02)
        return connections
    except BaseException:
        for connection in connections:
            connection.close()
        raise


def exercise_remote_commands(web, client_root, node, runner, credentials_only=False):
    sequence = 0
    handles = []
    prefix = "wss-" + uuid.uuid4().hex[:8]
    input_reference = None
    installed_inputs = 0
    def envelope(run, action, generation=1):
        nonlocal sequence
        sequence += 1
        return {"id": f"{prefix}-{sequence}", "node_id": node, "project_id": "acceptance-project", "run_id": run,
                "generation": generation, "session_generation": 0, "credential_version": 0, "policy_version": 1,
                "expires_at": int(time.time()) + 120, "action": action}
    def dispatch(command):
        return request(web + "/api/runner/nodes/" + node + "/commands", command, timeout=140)
    def command(run, action):
        status, result = dispatch(envelope(run, action))
        assert status == 200, result
        return result
    try:
        time.sleep(0.3)
        if credentials_only:
            bulk = staged_input(web, node, prefix + "-bulk", b"credential-bulk-fixture")
            return exercise_credentials(web, client_root, node, prefix, command, envelope, dispatch, handles, request, raw_request, held_uploads, bulk["id"])
        for suffix, scope in [("isolated", "run"), ("shared-a", "shared"), ("shared-b", "shared")]:
            run = prefix + "-" + suffix
            handle = command(run, {"operation": "ensure", "settings": {"worker_container_scope": scope,
                                "worker_network": "bridge", "worker_privilege": "default"}})
            handles.append(handle)
            start = envelope(run, {"operation": "start", "spec": {
                "argv": ["/bin/sh", "-c", "echo executed >> remote-count; printf 'remote-start\\n'; sleep 2; printf 'remote-finished\\n'"],
                "cwd": handle["container_workspace"], "timeout_sec": 30, "env": {"ACCEPTANCE_B": "two", "ACCEPTANCE_A": "one"},
                "output_limit_bytes": 1048576, "disk_limit_bytes": 10485760}})
            if suffix in ("isolated", "shared-a"):
                payload = bytes(range(256)) * 8192 + suffix.encode()
                source = staged_input(web, node, run, payload)
                input_reference = source["id"]
                start["action"]["spec"]["argv"][2] = "sha256sum inputs/remote.bin > received-input.sha256; grep -q '^" + source["sha256"] + " ' received-input.sha256 || exit 9; " + start["action"]["spec"]["argv"][2]
                slow = held_uploads(web, source["id"])
                try:
                    accepted = command(run, {"operation": "transfer_pull", "transfer_id": source["id"]})
                    assert accepted["state"] in ("queued", "receiving")
                    status, blocked = dispatch(start)
                    assert status == 409 and "workspace inputs" in blocked["error"], blocked
                finally:
                    for connection in slow:
                        connection.close()
                deadline = time.monotonic() + 30
                while True:
                    progress = command(run, {"operation": "transfer_status", "transfer_id": source["id"]})
                    assert progress["state"] != "failed", progress
                    if progress["state"] == "committed":
                        break
                    assert time.monotonic() < deadline, progress
                    time.sleep(0.05)
                assert (Path(handle["host_workspace"]) / source["path"]).read_bytes() == payload
                installed_inputs += 1
            elif input_reference:
                assert dispatch(envelope(run, {"operation": "transfer_pull", "transfer_id": input_reference}))[0] == 409
            status, started = dispatch(start)
            assert status == 200, started
            assert dispatch(start) == (200, started), "Start did not return original receipt"
            changed = json.loads(json.dumps(start)); changed["action"]["spec"]["argv"] = ["/bin/false"]
            assert dispatch(changed)[0] == 409
            wrong_scope = envelope(run, {"operation": "status"}); wrong_scope["project_id"] = "unregistered-project"
            assert dispatch(wrong_scope)[0] == 409
            if suffix == "shared-b":
                assert handles[1]["name"] == handle["name"]
                command(handles[1]["run_id"], {"operation": "stop"})
            cursor, frames, deadline = 0, [], time.monotonic() + 30
            while not any(frame["t"] == "exit" for frame in frames):
                assert time.monotonic() < deadline, frames
                page = command(run, {"operation": "events", "worker_id": started["worker_id"], "after": cursor})
                cursor = page["after"]; frames.extend(page["frames"])
                time.sleep(0.1)
            assert any(frame["t"] == "out" and "remote-finished" in frame["line"] for frame in frames)
            assert next(frame for frame in frames if frame["t"] == "exit")["rc"] == 0
            assert (Path(handle["host_workspace"]) / "remote-count").read_text().splitlines() == ["executed"]
            if suffix in ("isolated", "shared-a"):
                artifact_id = "artifact-" + uuid.uuid4().hex[:12]
                slow = held_uploads(web, source["id"])
                try:
                    accepted = command(run, {"operation": "transfer_push", "transfer_id": artifact_id,
                        "path": "inputs/remote.bin", "max_bytes": len(payload)})
                    assert accepted["direction"] == "output"
                    extra = command(run, {"operation": "start", "spec": {"argv": ["/bin/true"],
                        "cwd": handle["container_workspace"], "timeout_sec": 30, "env": {}}})
                    assert extra["worker_id"], "Pending output incorrectly blocked new Worker admission"
                finally:
                    for connection in slow:
                        connection.close()
                deadline = time.monotonic() + 30
                while True:
                    exported = command(run, {"operation": "transfer_status", "transfer_id": artifact_id})
                    assert exported["state"] != "failed", exported
                    if exported["state"] == "committed":
                        break
                    assert time.monotonic() < deadline, exported
                    time.sleep(0.05)
                received = bytearray()
                while len(received) < len(payload):
                    http_status, headers, chunk = raw_request(web + "/api/runner/transfers/" + artifact_id + "/chunks?offset=" + str(len(received)) + "&length=1048576")
                    assert http_status == 200, chunk
                    received.extend(chunk)
                assert bytes(received) == payload
                assert exported["receipt"]["manifest"]["sha256"] == hashlib.sha256(payload).hexdigest()
                assert dispatch(envelope(run, {"operation": "transfer_push", "transfer_id": "private-" + uuid.uuid4().hex[:12],
                    "path": "../outside", "max_bytes": 64}))[0] == 409
        isolated_project = envelope(prefix + "-other-project", {"operation": "ensure", "settings": {
            "worker_container_scope": "shared", "worker_network": "bridge", "worker_privilege": "default"}})
        isolated_project["project_id"] = "second-project"
        status, other = dispatch(isolated_project)
        assert status == 200, other
        handles.append(other)
        assert other["name"] != handles[1]["name"]
        assert Path(other["host_workspace"]).parent != Path(handles[1]["host_workspace"]).parent
        check = envelope(other["run_id"], {"operation": "start", "spec": {
            "argv": ["/bin/sh", "-c", "test ! -e " + handles[1]["container_workspace"] + "/remote-count"],
            "cwd": other["container_workspace"], "timeout_sec": 20, "env": {}}})
        check["project_id"] = "second-project"
        status, checked = dispatch(check)
        assert status == 200, checked
        deadline = time.monotonic() + 20
        while True:
            query = envelope(other["run_id"], {"operation": "events", "worker_id": checked["worker_id"], "after": 0})
            query["project_id"] = "second-project"
            status, page = dispatch(query)
            assert status == 200, page
            exits = [frame for frame in page["frames"] if frame["t"] == "exit"]
            if exits:
                assert exits[0]["rc"] == 0, exits
                break
            assert time.monotonic() < deadline
            time.sleep(0.1)
        stopped_output_id = "artifact-stopped-" + uuid.uuid4().hex[:12]
        output_run = handles[0]["run_id"]
        slow = held_uploads(web, source["id"])
        try:
            command(output_run, {"operation": "transfer_push", "transfer_id": stopped_output_id,
                "path": "inputs/remote.bin", "max_bytes": 4 * 1024 * 1024})
            deadline = time.monotonic() + 5
            while not (client_root / "runner/transfers" / (stopped_output_id + ".part")).exists():
                assert time.monotonic() < deadline
                time.sleep(0.02)
            started_stop = time.monotonic()
            command(output_run, {"operation": "stop"})
            stop_during_output_seconds = time.monotonic() - started_stop
            assert stop_during_output_seconds < 4
        finally:
            for connection in slow:
                connection.close()
        deadline = time.monotonic() + 15
        while True:
            output_status = command(output_run, {"operation": "transfer_status", "transfer_id": stopped_output_id})
            if output_status["state"] == "failed":
                break
            assert time.monotonic() < deadline, output_status
            time.sleep(0.05)
        assert request(web + "/api/runner/transfers/" + stopped_output_id)[0] != 200
        for handle in handles:
            stop = envelope(handle["run_id"], {"operation": "stop"})
            if handle is other:
                stop["project_id"] = "second-project"
            assert dispatch(stop)[0] == 200
        previous = handles[2]
        replacement = envelope(previous["run_id"], {"operation": "ensure", "settings": {
            "worker_container_scope": "run", "worker_network": "bridge", "worker_privilege": "default"}}, generation=2)
        status, independent = dispatch(replacement)
        assert status == 200, independent
        handles[2] = independent
        assert independent["name"] != previous["name"]
        assert not Path(previous["host_workspace"]).exists()
        assert (Path(independent["host_workspace"]) / "remote-count").read_text().splitlines() == ["executed"]
        stale = envelope(previous["run_id"], {"operation": "status"}, generation=1)
        assert dispatch(stale)[0] == 409
        assert dispatch(envelope(previous["run_id"], {"operation": "stop"}, generation=2))[0] == 200
        stopped_run = prefix + "-stopped-input"
        stopped_handle = command(stopped_run, {"operation": "ensure", "settings": {"worker_container_scope": "run", "worker_network": "bridge"}})
        handles.append(stopped_handle)
        stopped_source = staged_input(web, node, stopped_run, b"must-not-publish")
        slow = held_uploads(web, stopped_source["id"])
        try:
            command(stopped_run, {"operation": "transfer_pull", "transfer_id": stopped_source["id"]})
            started_stop = time.monotonic()
            command(stopped_run, {"operation": "stop"})
            stop_during_input_seconds = time.monotonic() - started_stop
            assert stop_during_input_seconds < 4
        finally:
            for connection in slow:
                connection.close()
        deadline = time.monotonic() + 15
        while True:
            status = command(stopped_run, {"operation": "transfer_status", "transfer_id": stopped_source["id"]})
            if status["state"] == "failed":
                break
            assert time.monotonic() < deadline, status
            time.sleep(0.05)
        assert not (Path(stopped_handle["host_workspace"]) / stopped_source["path"]).exists()
        credential_result = exercise_credentials(web, client_root, node, prefix, command, envelope, dispatch, handles, request, raw_request, held_uploads, stopped_source["id"])
        run = prefix + "-lease"
        handle = command(run, {"operation": "ensure", "settings": {"worker_container_scope": "run", "worker_network": "bridge"}})
        handles.append(handle)
        command(run, {"operation": "start", "spec": {"argv": ["/bin/sh", "-c", "while true; do echo tick >> lease-progress; sleep 1; done"],
                "cwd": handle["container_workspace"], "timeout_sec": 120, "env": {}, "output_limit_bytes": 1048576, "disk_limit_bytes": 10485760}})
        shared_lease_handles = []
        for suffix in ["a", "b"]:
            shared_run = prefix + "-stalled-shared-" + suffix
            shared_handle = command(shared_run, {"operation": "ensure", "settings": {"worker_container_scope": "shared", "worker_network": "bridge"}})
            handles.append(shared_handle)
            shared_lease_handles.append(shared_handle)
            command(shared_run, {"operation": "start", "spec": {"argv": ["/bin/sh", "-c", "while true; do echo tick >> lease-progress; sleep 1; done"],
                    "cwd": shared_handle["container_workspace"], "timeout_sec": 120, "env": {}}})
        assert shared_lease_handles[0]["container_id"] == shared_lease_handles[1]["container_id"]
        local_run = prefix + "-local-unleased"
        local_status, local_handle = read(client_root / "runner/control.sock", "/v1/containers/ensure",
            {"run_id": local_run, "generation": 1, "settings": {"worker_container_scope": "shared", "worker_network": "bridge"}})
        assert local_status == 200, local_handle
        handles.append(local_handle)
        assert local_handle["container_id"] != shared_lease_handles[0]["container_id"]
        local_status, local_worker = read(client_root / "runner/control.sock", "/v1/workers/start",
            {"run_id": local_run, "generation": 1, "command_id": str(uuid.uuid4()), "spec": {
                "argv": ["/bin/sh", "-c", "while true; do echo tick >> local-progress; sleep 1; done"],
                "cwd": local_handle["container_workspace"], "timeout_sec": 120, "env": {}}})
        assert local_status == 200, local_worker
        stalled_at = time.monotonic()
        runner.send_signal(signal.SIGSTOP)
        try:
            pending = {item["container_id"] for item in [handle, *shared_lease_handles]}
            while pending:
                for container_id in list(pending):
                    inspected = subprocess.run(["docker", "inspect", container_id], capture_output=True, text=True)
                    assert inspected.returncode == 0, inspected.stderr
                    state = json.loads(inspected.stdout)[0]["State"]
                    if not state["Running"]:
                        assert state["ExitCode"] == 75, state
                        pending.remove(container_id)
                assert time.monotonic() - stalled_at < 35, pending
                time.sleep(0.2)
            stalled_stop_seconds = time.monotonic() - stalled_at
            local_path = Path(local_handle["host_workspace"]) / "local-progress"
            local_before = local_path.read_bytes()
            time.sleep(1.2)
            assert len(local_path.read_bytes()) > len(local_before), "Unleased local work was stopped"
            for item in [handle, *shared_lease_handles]:
                path = Path(item["host_workspace"]) / "lease-progress"
                before = path.read_bytes()
                time.sleep(1.1)
                assert path.read_bytes() == before
        finally:
            runner.send_signal(signal.SIGCONT)
        status, revoked = request(web + "/api/runner/nodes/" + node + "/state", {"state": "revoked"})
        assert status == 200 and revoked["execution_stop_confirmed"] is False
        assert dispatch(start)[0] == 409, "Revoked node returned a cached authorized result"
        deadline = time.monotonic() + 50
        while True:
            status, runtime = read(client_root / "runner/control.sock", "/v1/runs/" + run)
            if status == 200 and runtime.get("run", {}).get("state") == "stopped":
                break
            assert time.monotonic() < deadline, runtime
            time.sleep(0.25)
        inspected = subprocess.run(["docker", "inspect", runtime["run"]["container_id"]], capture_output=True, text=True)
        assert inspected.returncode != 0 or not json.loads(inspected.stdout)[0]["State"]["Running"]
        progress = Path(handle["host_workspace"]) / "lease-progress"
        before = progress.read_text() if progress.exists() else ""
        time.sleep(1.2)
        assert (progress.read_text() if progress.exists() else "") == before
        return {**credential_result, "remote_execution_modes": ["run", "shared"], "start_executed_once": True,
                "runner_installed_inputs": installed_inputs, "runner_uploaded_verified_artifacts": 2, "pending_outputs_do_not_gate_worker_start": True,
                "stop_during_output_seconds": round(stop_during_output_seconds, 3), "stopped_output_not_published": True, "worker_start_waits_for_inputs": True,
                "cross_run_file_reference_denied": True, "container_read_verified_input_hash": True,
                "stop_during_input_seconds": round(stop_during_input_seconds, 3), "stopped_input_not_published": True,
                "shared_neighbor_survives_stop": True, "cross_project_command_rejected": True,
                "authorized_projects_have_distinct_pool_mounts": True, "cross_pool_file_read_denied": True, "shared_to_independent_preserves_workspace": True,
                "revoked_cached_command_rejected": True, "lease_expiry_confirmed_container_stop": True,
                "sigstop_runner_container_exit_75_both_modes": True, "unleased_local_shared_pool_survives": True, "sigstop_stop_seconds": round(stalled_stop_seconds, 3)}
    finally:
        for handle in reversed(handles):
            try:
                read(client_root / "runner/control.sock", "/v1/containers/" + handle["run_id"] + "/teardown", {"generation": handle.get("generation", 1)})
            except (OSError, TimeoutError):
                pass
        for container_id in {handle["container_id"] for handle in handles}:
            subprocess.run(["docker", "rm", "-f", container_id], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def exercise_runner_cli(binary, root, web, remote, ca, environment, execution=False, credentials_only=False):
    client_root = root / "remote-node"
    token_file = root / "enrollment-token"
    status, issued = request(web + "/api/runner/enrollments", {"name": "CLI node", "projects": ["acceptance-project", "second-project"], "ttl_seconds": 60})
    assert status == 200
    token_file.write_text(issued["token"])
    token_file.chmod(0o600)
    command = [binary, "runner", "enroll", "--state-dir", str(client_root), "--server", remote,
               "--token-file", str(token_file)]
    rejected = subprocess.run(command, env=environment, capture_output=True, timeout=15)
    assert rejected.returncode != 0, "Runner accepted untrusted CA"
    enrolled = subprocess.run(command + ["--ca-file", str(ca)], env=environment, capture_output=True, timeout=15)
    assert enrolled.returncode == 0, enrolled.stderr.decode()
    identity_file = client_root / "runner/remote.json"
    assert identity_file.stat().st_mode & 0o777 == 0o600
    assert identity_file.parent.stat().st_mode & 0o777 == 0o700
    config = json.loads(identity_file.read_text())
    generation = None
    execution_result = {}
    for attempt in range(2):
        with tempfile.TemporaryFile() as log:
            runner = subprocess.Popen([binary, "runner", "--remote", "--state-dir", str(client_root)],
                                      env=environment, stdout=log, stderr=log)
            try:
                deadline = time.monotonic() + 15
                while True:
                    log.seek(0)
                    output = log.read().decode()
                    assert config["credential"]["secret"] not in output
                    if "Runner authenticated to remote control plane" in output:
                        break
                    if runner.poll() is not None or time.monotonic() > deadline:
                        raise AssertionError("Actual Runner did not authenticate: " + output)
                    time.sleep(0.1)
                current = json.loads(identity_file.read_text())["lease"]
                assert current["node_id"] == config["credential"]["node_id"]
                if generation is None:
                    generation = current["generation"]
                assert current["generation"] == generation, "Runner restart replaced unexpired session"
                if execution and attempt == 1:
                    execution_result = exercise_remote_commands(web, client_root, config["credential"]["node_id"], runner, credentials_only)
            finally:
                runner.send_signal(signal.SIGINT)
                runner.wait(timeout=10)
        time.sleep(0.2)
    return {"actual_runner_cli_enrolled": True, "actual_runner_untrusted_ca_rejected": True,
            "identity_file_mode": "0600", "identity_directory_mode": "0700",
            "actual_runner_restart_resumes_same_lease": True, **execution_result}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--execution", action="store_true")
    parser.add_argument("--credentials-only", action="store_true")
    args = parser.parse_args()
    if args.credentials_only:
        args.execution = True
    binary = str(Path(".cache/target/debug/muteki-server").resolve())
    with tempfile.TemporaryDirectory(prefix="runner-tls-", dir="/tmp") as directory:
        root = Path(directory).resolve()
        cert, key = root / "cert.pem", root / "key.pem"
        ca, ca_key, csr = root / "ca.pem", root / "ca-key.pem", root / "server.csr"
        subprocess.run(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
                        "-subj", "/CN=Runner acceptance CA", "-addext", "basicConstraints=critical,CA:TRUE",
                        "-keyout", str(ca_key), "-out", str(ca)], check=True, capture_output=True)
        subprocess.run(["openssl", "req", "-new", "-newkey", "rsa:2048", "-nodes", "-subj", "/CN=localhost",
                        "-keyout", str(key), "-out", str(csr)], check=True, capture_output=True)
        extensions = root / "extensions.cnf"
        extensions.write_text("basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:localhost\n")
        subprocess.run(["openssl", "x509", "-req", "-in", str(csr), "-CA", str(ca), "-CAkey", str(ca_key),
                        "-CAcreateserial", "-days", "1", "-extfile", str(extensions), "-out", str(cert)], check=True, capture_output=True)
        context = ssl.create_default_context(cafile=str(ca))
        web_port, tls_port = port(), port()
        environment = dict(os.environ, MUTEKI_MANAGED_DESKTOP="1", RUST_LOG="muteki_server=info")
        for key_name in ("MUTEKI_RUNNER_SOCKET", "MUTEKI_DESKTOP_MAINTENANCE"):
            environment.pop(key_name, None)
        web = f"http://127.0.0.1:{web_port}"
        remote = f"https://localhost:{tls_port}"
        process = None
        runner_pid = None
        with tempfile.TemporaryFile() as log:
            try:
                process = subprocess.Popen([binary, "--state-dir", str(root), "--port", str(web_port),
                    "--runner-tls-listen", f"127.0.0.1:{tls_port}", "--runner-tls-cert", str(cert), "--runner-tls-key", str(key)],
                    env=environment, stdout=log, stderr=log)
                deadline = time.monotonic() + 30
                while True:
                    if process.poll() is not None or time.monotonic() > deadline:
                        log.seek(0)
                        raise RuntimeError(log.read().decode())
                    try:
                        with socket.create_connection(("127.0.0.1", tls_port), timeout=0.2):
                            runner_pid = read(root / "runner/control.sock", "/v1/health")[1]["pid"]
                            break
                    except OSError:
                        time.sleep(0.1)
                for url, trust in [(remote, None), (f"https://127.0.0.1:{tls_port}", context)]:
                    try:
                        request(url + "/v1/runner/enroll", {"token": "invalid", "protocol_version": 1}, trust)
                        raise AssertionError("Invalid TLS server identity accepted")
                    except urllib.error.URLError as error:
                        assert isinstance(error.reason, ssl.SSLCertVerificationError)
                result = asyncio.run(exercise(web, remote, context))
                with FaultProxy(tls_port, cert, key, ca) as proxy:
                    result.update(exercise_runner_cli(binary, root, web, proxy.url, ca, environment, args.execution, args.credentials_only))
                    if args.execution and not args.credentials_only:
                        assert proxy.cut and proxy.requests.get(proxy.cut_path) == 2, proxy.requests
                        result["truncated_https_chunk_retried_from_same_offset"] = True
                        assert proxy.upload_cut_path and proxy.requests[proxy.upload_cut_path] == 2, proxy.requests
                        result["lost_artifact_chunk_ack_retried_from_same_offset"] = True
                    if args.execution:
                        assert proxy.credential_cut_path and proxy.requests[proxy.credential_cut_path] == 2, proxy.requests
                        result["lost_credential_refresh_ack_recovered"] = True
                result.update({"acceptance_scope": "credentials_only" if args.credentials_only else "full_execution" if args.execution else "identity_and_https", "untrusted_ca_rejected": True, "wrong_hostname_rejected": True,
                               "transport": "real HTTPS + WSS, verified custom CA; localhost",
                               "remote_execution_validated": args.execution, "runner_file_installation_validated": args.execution and not args.credentials_only, "credential_projection_validated": args.execution})
                print(json.dumps(result, indent=2))
            finally:
                if process and process.poll() is None:
                    process.send_signal(signal.SIGINT)
                    process.wait(timeout=10)
                if runner_pid:
                    os.kill(runner_pid, signal.SIGINT)


if __name__ == "__main__":
    main()
