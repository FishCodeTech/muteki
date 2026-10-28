"""WP2 deterministic policy, approval, grant, receipt and egress checks."""
from __future__ import annotations

import asyncio
from datetime import datetime, timedelta, timezone
import socket
import socketserver
import subprocess
import sys
import tempfile
import threading
import traceback
from pathlib import Path
from typing import Any, Awaitable, Callable

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from muteki.platform.contracts.objects import Thread
from muteki.platform.store import PlatformStore
from muteki.src import SrcService
from muteki.src.contracts import ExecutionPolicy, ExecutionRequest, ExecutionRule, ResourceFact
from muteki.src.egress import SrcEgressManager
from muteki.src.policy import policy_digest
from muteki.src.store import SrcStoreError


class ConversationFixture:
    def __init__(self, store: PlatformStore) -> None:
        self.store = store

    def activate_thread(self, thread: Thread, principal_id: str, selection: Any = None) -> None:
        self.store.save(thread)


class World:
    def __init__(self, root: Path) -> None:
        self.store = PlatformStore(root / "platform.db")
        self.service = SrcService(
            self.store, artifact_root=root / "cas",
            conversation=ConversationFixture(self.store), run_gateway=None,
        )
        actor = type("Actor", (), {"id": "local-user"})()
        self.thread, self.task, self.binding = self.service.prepare_task(actor, {})
        self.service.commit_task(self.thread, self.task, self.binding, principal_id="local-user")
        self.service.execution.ensure_default_policy(
            self.task.task_id, ["https://example.test/api"], "local-user",
        )
        for run_id in ("run-a", "run-b", "run-c", "run-d"):
            self.service.store.put(
                self.task.task_id, "run_binding", {
                    "task_id": self.task.task_id, "thread_id": self.thread.thread_id,
                    "run_id": run_id, "binding_key": f"src:{self.task.task_id}:{run_id}",
                    "binding_revision": 1, "product_mode": "src", "execution_mode": "pentest",
                    "target_contract_ref": "fixture", "contract": {"target": "https://example.test/api"},
                }, record_id=run_id, natural_key=f"src:{self.task.task_id}:{run_id}",
            )
        self.calls: list[str] = []

    @property
    def task_id(self) -> str:
        return self.task.task_id

    @property
    def version(self) -> int:
        return self.service.execution.current_policy(self.task_id).version

    def request(
        self, name: str, *, run_id: str = "run-a", identity: str = "tester-a",
        body: str = "payload", cwd: str = "/workspace", path: str = "/api/orders",
    ) -> ExecutionRequest:
        return ExecutionRequest(
            invocation_id=name, task_id=self.task_id, thread_id=self.thread.thread_id,
            run_id=run_id, tool_name="http", action="request",
            arguments={"url": f"https://example.test{path}", "method": "POST", "body": body, "idempotent": False},
            arguments_hash=self.service.store.digest({"body": body, "cwd": cwd}),
            resource_kind="http", resource_facts={"tenant": ResourceFact(value="prod", origin="request")},
            identity_ref=identity, cwd=cwd, policy_version=self.version,
        )

    async def effect(self, request: ExecutionRequest) -> dict[str, Any]:
        self.calls.append(request.invocation_id)
        return {"ok": True, "invocation_id": request.invocation_id}

    def rules(self, *, deny: bool = False, observe: bool = False) -> list[ExecutionRule]:
        rules = [
            ExecutionRule(
                rule_id="domain-allow", effect="allow", resource_kind="http", evaluation_point="request",
                selector={"url": {"target": "https://example.test/api"}}, actions=["request"],
            ),
            ExecutionRule(
                rule_id="tenant-ask", effect="ask" if not observe else "deny", resource_kind="http",
                evaluation_point="request", mode="observe" if observe else "enforce",
                selector={"path": {"prefix": "/api/orders"}}, conditions={"tenant": "prod"},
                actions=["request"], reusable_approval=True,
            ),
        ]
        if deny:
            rules.append(ExecutionRule(
                rule_id="exact-deny", effect="deny", resource_kind="http", evaluation_point="request",
                selector={"path": "/api/orders"}, actions=["request"],
            ))
        return rules

    def revise(self, rules: list[ExecutionRule]) -> None:
        previous = {rule.rule_id: rule for rule in self.service.execution.current_policy(self.task_id).rules}
        versioned = []
        for rule in rules:
            before = previous.get(rule.rule_id)
            if before is not None and before != rule and before.version == rule.version:
                rule = rule.model_copy(update={"version": before.version + 1})
            versioned.append(rule)
        self.service.execution.revise_policy(
            self.task_id, {"rules": [rule.model_dump(mode="json") for rule in versioned]},
            actor_id="local-user", environment_capabilities=self.service.execution.environment_capabilities(),
        )

    def approval(self, invocation_id: str):
        rows = [row for row in self.service.store.all(self.task_id, "execution_approval")
                if row.payload.get("invocation_id") == invocation_id]
        assert len(rows) == 1, f"expected one approval for {invocation_id}, got {len(rows)}"
        return rows[0]

    def decide(self, invocation_id: str, decision: str) -> None:
        approval = self.approval(invocation_id)
        self.service.execution.decide_approval(
            self.task_id, approval.record_id, decision,
            actor_id="local-user", base_revision=approval.revision,
        )


def expect_code(code: str, call: Callable[[], Any]) -> None:
    try:
        call()
    except SrcStoreError as exc:
        assert exc.code == code, f"expected {code}, got {exc.code}"
        return
    raise AssertionError(f"expected {code}, call succeeded")


async def check_allow_deny_and_once(root: Path) -> None:
    world = World(root)
    allowed = await world.service.execution.execute(world.request("autonomous"), world.effect)
    assert allowed.dispatch_state == "completed"
    assert world.calls == ["autonomous"]
    assert not world.service.store.all(world.task_id, "execution_approval")

    world.revise(world.rules(deny=True))
    denied = await world.service.execution.execute(world.request("denied"), world.effect)
    assert denied.dispatch_state == "denied" and denied.error_code == "policy_denied"
    assert world.calls == ["autonomous"]

    world.revise(world.rules())
    pending_request = world.request("once")
    pending = await world.service.execution.execute(pending_request, world.effect)
    assert pending.dispatch_state == "waiting_approval"
    assert world.calls == ["autonomous"]
    queued = world.store.read_events("thread", world.thread.thread_id)
    assert any(row.event_type == "core.approval.requested" and row.payload.get("src_task_id") == world.task_id for row in queued)

    world.decide("once", "approve_once")
    resumed = await world.service.execution.execute(pending_request, world.effect)
    assert resumed.dispatch_state == "completed" and world.calls[-1] == "once"
    repeated = await world.service.execution.execute(pending_request, world.effect)
    assert repeated.dispatch_state == "completed"
    assert world.calls.count("once") == 1

    changed = await world.service.execution.execute(world.request("changed-body", body="different"), world.effect)
    assert changed.dispatch_state == "waiting_approval"
    changed_cwd = await world.service.execution.execute(world.request("changed-cwd", cwd="/other"), world.effect)
    assert changed_cwd.dispatch_state == "waiting_approval"

    rejected = world.request("rejected")
    await world.service.execution.execute(rejected, world.effect)
    world.decide("rejected", "deny")
    rejected_receipt = await world.service.execution.execute(rejected, world.effect)
    assert rejected_receipt.dispatch_state == "denied"
    assert "rejected" not in world.calls

    expired = world.request("expired")
    await world.service.execution.execute(expired, world.effect)
    approval = world.approval("expired")
    world.service.store.put(
        world.task_id, "execution_approval",
        {**approval.payload, "expires_at": (datetime.now(timezone.utc) - timedelta(seconds=1)).isoformat()},
        record_id=approval.record_id, expected_revision=approval.revision,
    )
    expect_code("approval_expired", lambda: world.decide("expired", "approve_once"))


async def check_task_grant(root: Path) -> None:
    world = World(root)
    world.revise(world.rules())
    original = world.request("scoped")
    await world.service.execution.execute(original, world.effect)
    world.decide("scoped", "approve_task_scope")
    assert (await world.service.execution.execute(original, world.effect)).dispatch_state == "completed"
    cross_run = world.request("scoped-other-run", run_id="run-b")
    assert (await world.service.execution.execute(cross_run, world.effect)).dispatch_state == "completed"
    other_identity = await world.service.execution.execute(
        world.request("other-identity", run_id="run-b", identity="other"), world.effect,
    )
    assert other_identity.dispatch_state == "waiting_approval"

    other = World(root / "other")
    other.revise(other.rules())
    other_task_request = other.request("other-task")
    assert (await other.service.execution.execute(other_task_request, other.effect)).dispatch_state == "waiting_approval"

    grant = world.service.store.all(world.task_id, "task_grant")[0]
    world.service.execution.revoke_grant(world.task_id, grant.record_id, base_revision=grant.revision)
    revoked = await world.service.execution.execute(world.request("revoked", run_id="run-c"), world.effect)
    assert revoked.dispatch_state == "waiting_approval"

    world.revise(world.rules())
    revised = await world.service.execution.execute(world.request("new-policy", run_id="run-d"), world.effect)
    assert revised.dispatch_state == "waiting_approval"


async def check_unknown_and_invalid_policy(root: Path) -> None:
    world = World(root)
    request = world.request("unknown")
    async def timeout(_: ExecutionRequest) -> None:
        raise TimeoutError("receipt connection lost after dispatch")
    unknown = await world.service.execution.execute(request, timeout)
    assert unknown.dispatch_state == "outcome_unknown" and not unknown.outcome_known
    before = list(world.calls)
    repeated = await world.service.execution.execute(request, world.effect)
    assert repeated.dispatch_state == "outcome_unknown" and world.calls == before
    world.service.execution = type(world.service.execution)(world.service)
    recovered = await world.service.execution.execute(request, world.effect)
    assert recovered.dispatch_state == "outcome_unknown" and world.calls == before

    policy_record = world.service.store.all(world.task_id, "execution_policy")[-1]
    world.service.store.put(
        world.task_id, "execution_policy", {**policy_record.payload, "digest": "tampered"},
        record_id=policy_record.record_id, expected_revision=policy_record.revision,
    )
    expect_code("policy_unavailable", lambda: world.service.execution.current_policy(world.task_id))


async def check_observe_and_path_boundary(root: Path) -> None:
    world = World(root)
    world.revise(world.rules(observe=True))
    request = world.request("observe", path="/api/orders")
    request = request.model_copy(update={"resource_facts": {}})
    decision = world.service.execution.evaluate_request(request)
    assert decision.status == "allowed" and decision.observed_rule_refs
    unknown_rule = ExecutionRule(
        rule_id="required-object", effect="deny", resource_kind="http", evaluation_point="request",
        selector={"object_id": "order-7"}, actions=["request"],
    )
    world.revise([unknown_rule])
    unresolved = await world.service.execution.execute(world.request("unknown-field"), world.effect)
    assert unresolved.dispatch_state == "created"
    assert unresolved.error_code == "authorization_unresolved"
    assert "unknown-field" not in world.calls

    path_rule = ExecutionRule(
        rule_id="api-ask", effect="ask", resource_kind="http", evaluation_point="request",
        selector={"path": {"prefix": "/api"}}, actions=["request"],
    )
    world.revise([path_rule])
    world.service.execution.revise_policy(
        world.task_id, {"authorized_targets": ["https://example.test/"]}, actor_id="local-user",
    )
    apix = await world.service.execution.execute(world.request("apix", path="/apix"), world.effect)
    assert apix.dispatch_state == "completed"
    api = await world.service.execution.execute(world.request("api", path="/api"), world.effect)
    assert api.dispatch_state == "waiting_approval"


class AcceptHandler(socketserver.BaseRequestHandler):
    def handle(self) -> None:
        self.request.sendall(b"ok")


class TcpServer(socketserver.TCPServer):
    allow_reuse_address = True


def start_server() -> tuple[TcpServer, int]:
    server = TcpServer(("0.0.0.0", 0), AcceptHandler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, int(server.server_address[1])


def client(gateway_name: str, port: int) -> subprocess.CompletedProcess[str]:
    code = (
        "import os,socket;"
        "os.environ['HTTP_PROXY']='http://127.0.0.1:1';"
        "s=socket.create_connection(('host.docker.internal',%d),timeout=4);"
        "s.recv(2);s.close();print('connected')" % port
    )
    return subprocess.run([
        "docker", "run", "--rm", "--network", f"container:{gateway_name}",
        "--entrypoint", "python", "muteki-src-egress:local", "-c", code,
    ], capture_output=True, text=True, check=False, timeout=30)


def check_egress(root: Path) -> str:
    if not SrcEgressManager.supported():
        return "SKIP: Docker is unavailable"
    allowed_server, allowed_port = start_server()
    denied_server, denied_port = start_server()
    task_id = "src-egress-check"
    manager = SrcEgressManager(root / "egress")
    name = manager._name(task_id)
    subprocess.run(["docker", "rm", "-f", name], capture_output=True, check=False)
    try:
        policy = ExecutionPolicy(
            task_id=task_id, network_mode="enforced",
            authorized_targets=[f"http://host.docker.internal:{allowed_port}"],
            environment_requirements={"service_exceptions": [{
                "host": "host.docker.internal", "port": allowed_port,
                "protocol": "tcp", "purpose": "local-fixture",
            }]},
            created_by="check_src_execution",
        )
        policy = policy.model_copy(update={"digest": policy_digest(policy)})
        current = manager.activate(task_id, policy)
        assert current["network_enforced"], current
        assert current["state"] == "ready" and current["generation"] == 1
        allowed = client(current["container"], allowed_port)
        assert allowed.returncode == 0, allowed.stderr or allowed.stdout
        denied = client(current["container"], denied_port)
        assert denied.returncode != 0, f"unauthorized port connected: {denied.stdout}{denied.stderr}"
        changed = policy.model_copy(update={"version": 2})
        mismatch = manager.current(task_id, changed)
        assert not mismatch["network_enforced"] and mismatch["state"] == "configuration_mismatch"
        return "PASS: enforced gateway allows only the configured port, direct IP/proxy bypass denied, digest mismatch visible"
    finally:
        subprocess.run(["docker", "rm", "-f", name], capture_output=True, check=False)
        allowed_server.shutdown(); allowed_server.server_close()
        denied_server.shutdown(); denied_server.server_close()


def raw_http_client(
    gateway_name: str, port: int, identity: str, path: str, *,
    gateway_address: str = "", managed: bool = True,
) -> tuple[subprocess.CompletedProcess[str], bytes]:
    import base64
    connect_host = gateway_address if managed else "host.docker.internal"
    target = f"http://host.docker.internal:{port}{path}" if managed else path
    raw = (
        f"POST {target}?x=1&x=2 HTTP/1.1\r\n"
        f"Host: host.docker.internal:{port}\r\n"
        "X-Test-Identity: tester-a\r\n"
        "X-Test-Tenant: prod\r\n"
        "X-Duplicate: first\r\n"
        "X-Duplicate: second\r\n"
        "Content-Length: 7\r\n"
        "Connection: close\r\n\r\n"
        "a=1&b=2"
    ).replace("tester-a", identity).encode("iso-8859-1")
    encoded = base64.b64encode(raw).decode("ascii")
    code = (
        "import base64,socket;"
        f"raw=base64.b64decode('{encoded}');"
        f"s=socket.create_connection(('{connect_host}',{18080 if managed else port}),timeout=5);"
        "s.sendall(raw);s.settimeout(45);out=b'';\n"
        "while True:\n"
        " try:\n"
        "  chunk=s.recv(65536)\n"
        "  if not chunk: break\n"
        "  out+=chunk\n"
        " except TimeoutError: break\n"
        "print(out.decode('iso-8859-1'))"
    )
    result = subprocess.run([
        "docker", "run", "--rm", "--network", f"container:{gateway_name}",
        "--entrypoint", "python", "muteki-src-egress:local", "-c", code,
    ], capture_output=True, text=True, check=False, timeout=60)
    if result.returncode or "502 Bad Gateway" in result.stdout:
        print(f"RAW_CLIENT rc={result.returncode} stdout={result.stdout} stderr={result.stderr}")
    return result, raw


def wait_inspection(manager: SrcEgressManager, task_id: str, seen: set[str]) -> dict[str, Any]:
    import time
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        rows = manager.inspection_requests(task_id)
        candidates = [row for row in rows if row["request_id"] not in seen
                      and (row.get("approval") or {}).get("status") == "pending"]
        if candidates:
            return candidates[-1]
        time.sleep(0.1)
    raise AssertionError(f"no new persistent inspection approval for {task_id}")


def check_content_approval_and_recovery(root: Path) -> str:
    import http.server
    world = World(root)
    requests: list[bytes] = []

    class Handler(http.server.BaseHTTPRequestHandler):
        def do_POST(self) -> None:
            body = self.rfile.read(int(self.headers.get("Content-Length") or 0))
            raw = f"{self.requestline}\r\n{self.headers}\r\n".encode("iso-8859-1") + body
            requests.append(raw)
            self.send_response(200)
            self.send_header("Content-Type", "text/plain")
            self.send_header("Content-Length", "2")
            self.end_headers()
            self.wfile.write(b"ok")

        def log_message(self, *_args: Any) -> None:
            return

    server = http.server.ThreadingHTTPServer(("0.0.0.0", 0), Handler)
    port = int(server.server_address[1])
    threading.Thread(target=server.serve_forever, daemon=True).start()
    target = f"http://host.docker.internal:{port}"
    rule = ExecutionRule(
        rule_id="content-ask", effect="ask", resource_kind="http", evaluation_point="request",
        selector={"path": {"prefix": "/api"}}, conditions={"tenant": "prod"},
        actions=["POST", "request"], reusable_approval=True,
    )
    manager = world.service.egress
    name = manager._name(world.task_id)
    subprocess.run(["docker", "rm", "-f", name], capture_output=True, check=False)
    seen: set[str] = set()
    try:
        world.service.execution.revise_policy(
            world.task_id, {
                "network_mode": "enforced", "authorized_targets": [target],
                "environment_requirements": {
                    "required_adapters": ["structured_http"],
                    "service_exceptions": [{
                        "host": "host.docker.internal", "port": port,
                        "protocol": "tcp", "purpose": "local-content-fixture",
                    }],
                },
                "inspection_requirements": {
                    "mode": "enforce", "targets": [target],
                    "identity_header": "x-test-identity", "tenant_header": "x-test-tenant",
                    "approval_timeout_seconds": 20,
                },
                "rules": [rule.model_dump(mode="json")],
            }, actor_id="local-user",
        )
        current = manager.current(world.task_id, world.service.execution.current_policy(world.task_id))
        capability_problem = "" if current["network_enforced"] and current["content_enforced"] else (
            f"activated policy cannot be re-read as enforced: {current}"
        )
        if capability_problem:
            print(capability_problem)

        import importlib.util
        proxy_spec = importlib.util.spec_from_file_location(
            "src_check_http_proxy", Path(__file__).resolve().parents[1] / "docker" / "src-egress" / "http_proxy.py",
        )
        proxy_module = importlib.util.module_from_spec(proxy_spec)
        proxy_spec.loader.exec_module(proxy_module)
        precise_policy = world.service.execution.current_policy(world.task_id).model_dump(mode="json")
        precise_policy["rules"] = [rule.model_copy(update={"selector": {"path": {"prefix": "/api/approve"}}}).model_dump(mode="json")]
        precise_decision = proxy_module._decision(precise_policy, {
            "url": f"{target}/api/approve?x=1&x=2", "method": "POST",
            "path": "/api/approve?x=1&x=2", "tenant": "prod",
        })
        if precise_decision.get("effect") != "ask":
            capability_problem += f"; query-string path bypassed exact rule: {precise_decision}"
            print("QUERY_PATH_RULE_BYPASS", precise_decision)

        def current_gateway_address() -> str:
            info = __import__("json").loads(subprocess.check_output(["docker", "inspect", name], text=True))
            return info[0]["NetworkSettings"]["IPAddress"]

        gateway_address = current_gateway_address()
        direct_result, _ = raw_http_client(name, port, "tester-a", "/api/approve", managed=False)
        direct_bypassed = len(requests) == 1
        if direct_bypassed:
            capability_problem += "; direct non-80/443 request bypassed content approval"
            print("DIRECT_CONTENT_BYPASS", direct_result.stdout, direct_result.stderr)
            requests.clear()

        def dispatch(identity: str = "tester-a"):
            return threading.Thread(
                target=raw_http_client, args=(name, port, identity, "/api/approve"),
                kwargs={"gateway_address": current_gateway_address()}, daemon=True,
            )

        first = dispatch(); first.start()
        pending = wait_inspection(manager, world.task_id, seen)
        assert not requests, "request reached upstream before approval"
        seen.add(pending["request_id"])
        manager.decide_inspection(world.task_id, pending["request_id"], approved=True, actor_id="local-user")
        first.join(timeout=60)
        assert not first.is_alive() and len(requests) == 1
        raw_path = manager._task_root(world.task_id) / "traffic" / f"{pending['request_id']}.request"
        original = raw_path.read_bytes()
        assert original.startswith(f"POST {target}/api/approve?x=1&x=2 HTTP/1.1\r\n".encode())
        assert b"X-Duplicate: first\r\nX-Duplicate: second\r\n" in original
        assert original.endswith(b"a=1&b=2")

        second = dispatch(); second.start()
        scoped = wait_inspection(manager, world.task_id, seen)
        assert len(requests) == 1
        seen.add(scoped["request_id"])
        manager.decide_inspection(
            world.task_id, scoped["request_id"], approved=True,
            actor_id="local-user", task_scope=True,
        )
        second.join(timeout=60)
        assert not second.is_alive() and len(requests) == 2

        automatic = dispatch(); automatic.start()
        automatic.join(timeout=60)
        assert not automatic.is_alive() and len(requests) == 3, "Task Scope grant did not cover the repeated request"

        subprocess.run(["docker", "stop", name], capture_output=True, check=True, timeout=30)
        recovered = manager.recover_enforced(world.task_id, world.service.execution.current_policy(world.task_id))
        assert recovered["recovered"] and recovered["network_enforced"] and recovered["content_enforced"]

        pending_thread = dispatch("tester-b"); pending_thread.start()
        waiting = wait_inspection(manager, world.task_id, seen)
        seen.add(waiting["request_id"])
        assert len(requests) == 3
        subprocess.run(["docker", "stop", name], capture_output=True, check=True, timeout=30)
        pending_thread.join(timeout=60)
        assert len(requests) == 3, "gateway restart duplicated the in-flight request"
        recovered = manager.recover_enforced(world.task_id, world.service.execution.current_policy(world.task_id))
        assert recovered["recovered"]
        manager.decide_inspection(world.task_id, waiting["request_id"], approved=True, actor_id="local-user")
        assert len(requests) == 3, "recovered approval replayed a dead client request"

        final = dispatch("tester-b"); final.start()
        final_pending = wait_inspection(manager, world.task_id, seen)
        seen.add(final_pending["request_id"])
        manager.decide_inspection(world.task_id, final_pending["request_id"], approved=True, actor_id="local-user")
        final.join(timeout=60)
        assert not final.is_alive() and len(requests) == 4
        assert not capability_problem, capability_problem
        return "PASS: content approval blocks upstream, once/Task Scope resume, raw bytes, gateway recovery and no in-flight replay"
    finally:
        manager.stop_inspection_bridge()
        subprocess.run(["docker", "rm", "-f", name], capture_output=True, check=False)
        server.shutdown(); server.server_close()
        world.store.close()


async def run(name: str, call: Callable[[], Awaitable[Any]]) -> tuple[str, str]:
    try:
        result = await call()
        return name, result or "PASS"
    except Exception:
        return name, "FAIL\n" + traceback.format_exc()


async def main() -> None:
    scenarios: list[tuple[str, Callable[[], Awaitable[Any]]]] = [
        ("allow/deny/once", lambda: check_allow_deny_and_once(Path(tempfile.mkdtemp(prefix="muteki-src-once-")))),
        ("task grant", lambda: check_task_grant(Path(tempfile.mkdtemp(prefix="muteki-src-grant-")))),
        ("outcome unknown", lambda: check_unknown_and_invalid_policy(Path(tempfile.mkdtemp(prefix="muteki-src-unknown-")))),
        ("observe/path", lambda: check_observe_and_path_boundary(Path(tempfile.mkdtemp(prefix="muteki-src-observe-")))),
        ("docker egress", lambda: asyncio.to_thread(check_egress, Path(tempfile.mkdtemp(prefix="muteki-src-egress-")))),
        ("docker content approval/recovery", lambda: asyncio.to_thread(check_content_approval_and_recovery, Path(tempfile.mkdtemp(prefix="muteki-src-content-")))),
    ]
    results = await asyncio.gather(*(run(name, call) for name, call in scenarios))
    failed = False
    for name, result in results:
        print(f"[{name}] {result}")
        failed = failed or result.startswith("FAIL") or result.startswith("SKIP")
    if failed:
        raise SystemExit(1)
    print("PASS: WP2 policy, approvals, grants, stable unknown receipts and enforced gateway")


if __name__ == "__main__":
    asyncio.run(main())
