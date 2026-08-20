"""Loopback CONNECT proxy used by the Darwin Protocol 2 live-local sandbox.

The worker process is denied direct outbound sockets by ``sandbox-exec`` and may
connect only to this loopback port. The proxy, which is owned by the host runtime,
permits one pinned provider destination and records every allow/deny decision.
"""

from __future__ import annotations

import select
import socket
import socketserver
import ssl
import threading
from dataclasses import dataclass
from urllib.parse import urlsplit

from muteki.epistemic.contracts import canonical_digest


class EgressProxyError(RuntimeError):
    pass


def _split_destination(value: str) -> tuple[str, int]:
    host, separator, raw_port = str(value).rpartition(":")
    if not separator or not host:
        raise ValueError("destination must be host:port")
    port = int(raw_port)
    if port < 1 or port > 65535:
        raise ValueError("destination port is out of range")
    return host.lower(), port


@dataclass(frozen=True, slots=True)
class EgressProxyObservation:
    destination: str
    allowed_connects: int
    denied_connects: int
    complete: bool

    @property
    def digest(self) -> str:
        return canonical_digest({
            "allowed_connects": self.allowed_connects,
            "complete": self.complete,
            "denied_connects": self.denied_connects,
            "destination": self.destination,
        })


class _ProxyServer(socketserver.ThreadingTCPServer):
    allow_reuse_address = False
    daemon_threads = True

    def __init__(self, address, handler, *, owner):
        self.owner = owner
        super().__init__(address, handler)


class _ConnectHandler(socketserver.BaseRequestHandler):
    @staticmethod
    def _relay(client: socket.socket, upstream: socket.socket) -> None:
        client.settimeout(None)
        upstream.settimeout(None)
        sockets = [client, upstream]
        while True:
            readable, _, _ = select.select(sockets, [], [], 30.0)
            if not readable:
                return
            for source in readable:
                data = source.recv(64 * 1024)
                if not data:
                    return
                target = upstream if source is client else client
                target.sendall(data)

    def handle(self) -> None:
        owner: LoopbackAllowlistProxy = self.server.owner  # type: ignore[attr-defined]
        client = self.request
        client.settimeout(10.0)
        header = bytearray()
        while b"\r\n\r\n" not in header and len(header) <= 16 * 1024:
            chunk = client.recv(4096)
            if not chunk:
                return
            header.extend(chunk)
        if b"\r\n\r\n" not in header:
            owner._record_denied()
            client.sendall(b"HTTP/1.1 431 Request Header Fields Too Large\r\n\r\n")
            return
        first = bytes(header).split(b"\r\n", 1)[0].decode("ascii", "replace")
        parts = first.split()
        if len(parts) != 3:
            owner._record_denied()
            client.sendall(b"HTTP/1.1 400 Bad Request\r\n\r\n")
            return
        if parts[0].upper() != "CONNECT":
            self._handle_reverse(owner, client, bytes(header), parts)
            return
        try:
            requested = _split_destination(parts[1])
        except (TypeError, ValueError):
            owner._record_denied()
            client.sendall(b"HTTP/1.1 400 Bad Request\r\n\r\n")
            return
        if requested != owner.allowed_destination:
            owner._record_denied()
            client.sendall(b"HTTP/1.1 403 Forbidden\r\n\r\n")
            return
        try:
            upstream = socket.create_connection(requested, timeout=10.0)
        except OSError:
            owner._record_denied()
            client.sendall(b"HTTP/1.1 502 Bad Gateway\r\n\r\n")
            return
        owner._record_allowed()
        try:
            client.sendall(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            self._relay(client, upstream)
        finally:
            upstream.close()

    def _handle_reverse(
        self, owner: "LoopbackAllowlistProxy", client: socket.socket,
        header: bytes, parts: list[str],
    ) -> None:
        """TLS-forward ordinary loopback HTTP to the one pinned provider."""
        host, port = owner.allowed_destination
        try:
            raw_upstream = socket.create_connection((host, port), timeout=10.0)
            upstream = ssl.create_default_context().wrap_socket(
                raw_upstream, server_hostname=host)
        except OSError:
            owner._record_denied()
            client.sendall(b"HTTP/1.1 502 Bad Gateway\r\n\r\n")
            return
        marker = header.index(b"\r\n\r\n")
        head, buffered_body = header[:marker], header[marker + 4:]
        lines = head.split(b"\r\n")
        target = parts[1]
        if target.startswith(("http://", "https://")):
            parsed = urlsplit(target)
            target = parsed.path or "/"
            if parsed.query:
                target += "?" + parsed.query
        first = f"{parts[0]} {target} {parts[2]}".encode("ascii", "strict")
        authority = host if port == 443 else f"{host}:{port}"
        forwarded = [first]
        for line in lines[1:]:
            lower = line.lower()
            if lower.startswith(b"host:"):
                forwarded.append(f"Host: {authority}".encode())
            elif not lower.startswith((b"proxy-connection:", b"proxy-authorization:")):
                forwarded.append(line)
        try:
            upstream.sendall(b"\r\n".join(forwarded) + b"\r\n\r\n" + buffered_body)
            owner._record_allowed()
            self._relay(client, upstream)
        finally:
            upstream.close()


class LoopbackAllowlistProxy:
    def __init__(self, destination: str) -> None:
        self.destination = str(destination)
        self.allowed_destination = _split_destination(destination)
        self._server: _ProxyServer | None = None
        self._thread: threading.Thread | None = None
        self._lock = threading.Lock()
        self._allowed = 0
        self._denied = 0
        self._closed = False

    def start(self) -> None:
        if self._server is not None:
            return
        server = _ProxyServer(("127.0.0.1", 0), _ConnectHandler, owner=self)
        thread = threading.Thread(
            target=server.serve_forever, name="protocol2-egress-proxy", daemon=True)
        thread.start()
        self._server = server
        self._thread = thread

    @property
    def port(self) -> int:
        if self._server is None:
            raise EgressProxyError("egress proxy is not started")
        return int(self._server.server_address[1])

    @property
    def proxy_url(self) -> str:
        return f"http://127.0.0.1:{self.port}"

    @property
    def sandbox_profile(self) -> str:
        # The worker and every descendant inherit the seatbelt. Only the pinned
        # loopback proxy is reachable; provider DNS/socket access happens outside
        # the worker boundary in this host-owned proxy.
        return (
            "(version 1)"
            "(allow default)"
            "(deny network-outbound)"
            # Bun's HTTP client does not match Darwin seatbelt's single-port
            # localhost rule consistently. L0 explicitly permits loopback; allow
            # loopback TCP while the host proxy remains the only process capable
            # of reaching the pinned external provider.
            "(allow network-outbound (remote tcp \"localhost:*\"))"
        )

    def _record_allowed(self) -> None:
        with self._lock:
            self._allowed += 1

    def _record_denied(self) -> None:
        with self._lock:
            self._denied += 1

    def observation(self) -> EgressProxyObservation:
        with self._lock:
            return EgressProxyObservation(
                self.destination, self._allowed, self._denied, self._closed)

    def close(self) -> EgressProxyObservation:
        server, thread = self._server, self._thread
        if server is not None:
            server.shutdown()
            server.server_close()
        if thread is not None:
            thread.join(timeout=5.0)
            if thread.is_alive():
                raise EgressProxyError("egress proxy owner did not terminate")
        self._closed = True
        return self.observation()
