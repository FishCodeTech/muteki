from __future__ import annotations

import socket
import socketserver
import subprocess
import sys
import threading
from types import SimpleNamespace

import pytest

from muteki.epistemic.contracts import canonical_digest
from muteki.epistemic.sqlite_store import EpistemicSQLiteStore
from muteki.runtime.canary import (
    CANARY_DIGEST_VERSION,
    CanaryEvidence,
    CanaryLevel,
    CanaryRejected,
    admit_canary,
)
from muteki.runtime.contracts import AttemptIdentity, ExecutionScope, LeaseIdentity
from muteki.runtime.egress_proxy import LoopbackAllowlistProxy
from muteki.runtime.network import NetworkPolicyAuthority, NetworkPolicyUnknown


class _Adapter:
    def __init__(self, *, corrupt=False):
        self.value = {}
        self.corrupt = corrupt
    def apply(self, policy):
        self.value = dict(policy)
        return self.value
    def readback(self):
        return {**self.value, **({"mode": "bridge"} if self.corrupt else {})}


def _store(tmp_path):
    return EpistemicSQLiteStore.create(
        path=tmp_path / "net.db", run_id="run-1", manifest_digest="a" * 64)


def _lease():
    scope = ExecutionScope("run-1", 1, 1)
    attempt = AttemptIdentity(scope, "b1", "a1", 1)
    return LeaseIdentity(attempt, "l1", 1, 1)


def test_network_policy_requires_execution_readback(tmp_path):
    authority = NetworkPolicyAuthority(store=_store(tmp_path), adapter=_Adapter())
    policy = authority.apply_and_readback(
        operation_id="op1", mode="allowlist", allowlist=["127.0.0.1:8000"],
        occurred_at_ns=1)
    assert len(policy.enforcement_receipt_digest) == 64
    allowed = authority.record_egress(
        receipt_id="r1", lease=_lease(), destination="127.0.0.1:8000",
        policy=policy, occurred_at_ns=2)
    denied = authority.record_egress(
        receipt_id="r2", lease=_lease(), destination="example.com:443",
        policy=policy, occurred_at_ns=3)
    assert len(allowed) == len(denied) == 64


def test_network_policy_mismatch_is_unknown_not_allow(tmp_path):
    authority = NetworkPolicyAuthority(
        store=_store(tmp_path), adapter=_Adapter(corrupt=True))
    with pytest.raises(NetworkPolicyUnknown):
        authority.apply_and_readback(
            operation_id="op1", mode="none", allowlist=[], occurred_at_ns=1)


def test_live_canary_fails_closed_without_network_and_eval_receipts():
    base = {key: "a" * 64 for key in ("baseline", "schema", "fault_suite",
                                             "kernel", "cas", "admission")}
    with pytest.raises(CanaryRejected, match="egress"):
        admit_canary(CanaryEvidence(
            CanaryLevel.LIVE_LOCAL, base, True, True, True))
    complete = {**base, "network_policy": "b" * 64, "egress": "c" * 64,
                "egress_observation": "d" * 64,
                "eval_assignment": "e" * 64,
                "platform_admission": "f" * 64,
                "platform_supervisor": "1" * 64,
                "platform_cleanup": "2" * 64}
    admitted = admit_canary(CanaryEvidence(
        CanaryLevel.LIVE_LOCAL, complete, True, True, True))
    assert admitted == canonical_digest({
        "canary_digest_version": CANARY_DIGEST_VERSION,
        "fault_suite_green": True,
        "gate_equivalent": True,
        "level": CanaryLevel.LIVE_LOCAL.value,
        "projection_rebuild_equivalent": True,
        "receipts": complete,
    })
    with pytest.raises(CanaryRejected, match="malformed"):
        admit_canary(CanaryEvidence(
            CanaryLevel.LIVE_LOCAL,
            {**complete, "schema": "z" * 64},
            True,
            True,
            True,
        ))


@pytest.mark.skipif(sys.platform != "darwin", reason="Darwin seatbelt fixture")
def test_loopback_proxy_plus_seatbelt_denies_direct_and_observes_allowlist():
    class Provider(socketserver.BaseRequestHandler):
        def handle(self):
            self.request.settimeout(1)
            try:
                while self.request.recv(1024):
                    pass
            except OSError:
                pass

    provider = socketserver.ThreadingTCPServer(("0.0.0.0", 0), Provider)
    provider.daemon_threads = True
    provider_thread = threading.Thread(target=provider.serve_forever, daemon=True)
    provider_thread.start()
    destination = f"localhost:{provider.server_address[1]}"
    proxy = LoopbackAllowlistProxy(destination)
    proxy.start()
    try:
        denied_code = (
            "import socket,sys\n"
            "try: socket.create_connection(('192.0.2.1',9),0.2)\n"
            "except PermissionError: sys.exit(0)\n"
            "except OSError: sys.exit(2)\n"
            "else: sys.exit(3)\n"
        )
        denied = subprocess.run(
            ["/usr/bin/sandbox-exec", "-p", proxy.sandbox_profile,
             sys.executable, "-c", denied_code],
            capture_output=True, timeout=5)
        assert denied.returncode == 0, (
            "non-local socket was not rejected by seatbelt with PermissionError")

        allowed_code = (
            "import socket; "
            f"s=socket.create_connection(('127.0.0.1',{proxy.port}),2); "
            f"s.sendall(b'CONNECT {destination} HTTP/1.1\\r\\nHost: {destination}\\r\\n\\r\\n'); "
            "assert s.recv(128).startswith(b'HTTP/1.1 200')"
        )
        allowed = subprocess.run(
            ["/usr/bin/sandbox-exec", "-p", proxy.sandbox_profile,
             sys.executable, "-c", allowed_code],
            capture_output=True, timeout=5)
        assert allowed.returncode == 0, allowed.stderr.decode(errors="replace")

        with socket.create_connection(("127.0.0.1", proxy.port), timeout=2) as conn:
            conn.sendall(
                b"CONNECT localhost:1 HTTP/1.1\r\nHost: localhost:1\r\n\r\n")
            assert conn.recv(128).startswith(b"HTTP/1.1 403")
    finally:
        observation = proxy.close()
        provider.shutdown()
        provider.server_close()
        provider_thread.join(timeout=3)
    assert observation.complete is True
    assert observation.allowed_connects == 1
    assert observation.denied_connects == 1


def test_protocol2_runtime_argv_is_wrapped_by_network_sandbox(monkeypatch):
    from muteki.solver import cli_solver

    monkeypatch.setattr(
        cli_solver.os.path,
        "isfile",
        lambda path: path == "/usr/bin/sandbox-exec",
    )

    worker = SimpleNamespace(
        driver=SimpleNamespace(name="claude"), container=None,
        _protocol2_mode=True)
    argv = cli_solver.CliSolver._apply_runtime_argv(
        worker, ["/usr/bin/true"],
        {"MUTEKI_NETWORK_SANDBOX_PROFILE": "(version 1)(allow default)"})
    assert argv[:3] == [
        "/usr/bin/sandbox-exec", "-p", "(version 1)(allow default)"]


def test_protocol2_runtime_argv_fails_closed_without_network_sandbox(monkeypatch):
    from muteki.solver import cli_solver

    monkeypatch.setattr(cli_solver.os.path, "isfile", lambda _path: False)
    worker = SimpleNamespace(
        driver=SimpleNamespace(name="claude"), container=None,
        _protocol2_mode=True)
    with pytest.raises(RuntimeError, match="sandbox-exec is unavailable"):
        cli_solver.CliSolver._apply_runtime_argv(
            worker,
            ["/usr/bin/true"],
            {"MUTEKI_NETWORK_SANDBOX_PROFILE": "(version 1)(allow default)"},
        )


def test_loopback_reverse_proxy_rewrites_host_and_forwards_only_pinned_provider(
    monkeypatch,
):
    captured = {"request": b""}

    class Provider(socketserver.BaseRequestHandler):
        def handle(self):
            data = bytearray()
            while b"\r\n\r\n" not in data:
                data.extend(self.request.recv(4096))
            captured["request"] = bytes(data)
            self.request.sendall(
                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK")

    class PlainContext:
        @staticmethod
        def wrap_socket(sock, *, server_hostname):
            assert server_hostname == "localhost"
            return sock

    import muteki.runtime.egress_proxy as proxy_module
    monkeypatch.setattr(
        proxy_module.ssl, "create_default_context", lambda: PlainContext())
    provider = socketserver.ThreadingTCPServer(("127.0.0.1", 0), Provider)
    provider.daemon_threads = True
    provider_thread = threading.Thread(target=provider.serve_forever, daemon=True)
    provider_thread.start()
    destination = f"localhost:{provider.server_address[1]}"
    proxy = LoopbackAllowlistProxy(destination)
    proxy.start()
    try:
        with socket.create_connection(("127.0.0.1", proxy.port), timeout=2) as conn:
            conn.sendall(
                b"POST /anthropic/v1/messages HTTP/1.1\r\n"
                b"Host: 127.0.0.1\r\nContent-Length: 0\r\n\r\n")
            response = conn.recv(4096)
        assert response.startswith(b"HTTP/1.1 200")
    finally:
        observation = proxy.close()
        provider.shutdown()
        provider.server_close()
        provider_thread.join(timeout=3)
    assert f"Host: {destination}".encode() in captured["request"]
    assert observation.allowed_connects == 1
