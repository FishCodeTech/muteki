#!/usr/bin/env python3
from __future__ import annotations

import hashlib
import ipaddress
import json
import os
import re
import select
import socket
import socketserver
import ssl
import subprocess
import threading
import time
import traceback
import uuid
from pathlib import Path
from typing import Any
from urllib.parse import urlsplit

MAX_HEAD = 64 * 1024
BUFFER = 64 * 1024
SO_ORIGINAL_DST = 80


class InspectionError(RuntimeError):
    def __init__(self, code: str, message: str):
        super().__init__(message)
        self.code = code


class PrependSocket:
    def __init__(self, sock: socket.socket, prefix: bytes):
        self._socket = sock
        self._prefix = prefix

    def recv(self, size: int, flags: int = 0) -> bytes:
        if self._prefix:
            data = self._prefix[:size]
            if not flags & socket.MSG_PEEK:
                self._prefix = self._prefix[size:]
            return data
        return self._socket.recv(size, flags)

    def recv_into(self, buffer: Any, size: int = 0, flags: int = 0) -> int:
        data = self.recv(size or len(buffer), flags)
        buffer[:len(data)] = data
        return len(data)

    def prepend(self, data: bytes) -> None:
        self._prefix = data + self._prefix

    def __getattr__(self, name: str) -> Any:
        return getattr(self._socket, name)


def _json(value: Any) -> bytes:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode("utf-8")


def _atomic(path: Path, data: bytes) -> None:
    path.parent.mkdir(mode=0o2750, parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.{os.getpid()}.{time.time_ns()}.tmp")
    try:
        with temporary.open("wb") as handle:
            handle.write(data)
            handle.flush()
            os.fsync(handle.fileno())
        os.chmod(temporary, 0o640)
        os.replace(temporary, path)
    finally:
        temporary.unlink(missing_ok=True)


def _read_until(sock: socket.socket, marker: bytes, limit: int) -> bytes:
    data = bytearray()
    while True:
        chunk = sock.recv(min(BUFFER, limit - len(data)))
        if not chunk:
            raise InspectionError("capture_partial", "connection closed before the protocol header completed")
        data.extend(chunk)
        index = data.find(marker)
        if index >= 0:
            end = index + len(marker)
            if end < len(data):
                if not hasattr(sock, "prepend"):
                    raise InspectionError("inspection_unavailable", "socket cannot preserve bytes beyond the protocol header")
                sock.prepend(bytes(data[end:]))
            return bytes(data[:end])
        if len(data) > limit:
            raise InspectionError("inspection_unavailable", "protocol header exceeds the configured limit")


def _read_exact(sock: socket.socket, size: int) -> bytes:
    data = bytearray()
    while len(data) < size:
        chunk = sock.recv(min(BUFFER, size - len(data)))
        if not chunk:
            raise InspectionError("capture_partial", "connection closed before the body completed")
        data.extend(chunk)
    return bytes(data)


def _headers(raw: bytes) -> tuple[str, list[tuple[str, str]]]:
    try:
        lines = raw.decode("iso-8859-1").split("\r\n")
    except UnicodeError as exc:
        raise InspectionError("inspection_unavailable", "HTTP header is not ISO-8859-1") from exc
    if not lines or not lines[0]:
        raise InspectionError("inspection_unavailable", "missing HTTP start line")
    headers = []
    for line in lines[1:]:
        if not line:
            continue
        if ":" not in line or line[0].isspace():
            raise InspectionError("inspection_unavailable", "malformed or folded HTTP header")
        name, value = line.split(":", 1)
        if not name or not re.fullmatch(r"[!#$%&'*+.^_`|~0-9A-Za-z-]+", name):
            raise InspectionError("inspection_unavailable", "malformed HTTP header name")
        headers.append((name, value.lstrip(" \t")))
    return lines[0], headers


def _request_authority(
    value: str, scheme: str, *, absolute: bool = False,
) -> tuple[str, str, int]:
    try:
        if not absolute and "://" in value:
            raise ValueError("Host header includes a scheme")
        parsed = urlsplit(value if absolute else f"{scheme}://{value}")
        port = parsed.port or (443 if parsed.scheme == "https" else 80)
    except ValueError as exc:
        raise InspectionError("inspection_unavailable", "HTTP request authority is invalid") from exc
    if (parsed.scheme not in {"http", "https"} or not parsed.hostname
            or parsed.username is not None or parsed.password is not None
            or parsed.path or parsed.query or parsed.fragment):
        raise InspectionError("inspection_unavailable", "HTTP request authority is invalid")
    return parsed.scheme, _normalized_host(parsed.hostname), port


def _header(headers: list[tuple[str, str]], name: str) -> list[str]:
    return [value for key, value in headers if key.lower() == name.lower()]


def _body(sock: socket.socket, headers: list[tuple[str, str]], limit: int, *, eof: bool = False) -> bytes:
    transfer = ",".join(_header(headers, "transfer-encoding")).lower()
    if "chunked" in transfer:
        data = bytearray()
        while True:
            line = _read_until(sock, b"\r\n", 8192)
            try:
                size = int(line.split(b";", 1)[0].strip(), 16)
            except ValueError as exc:
                raise InspectionError("inspection_unavailable", "invalid chunk size") from exc
            data.extend(line)
            if size == 0:
                while True:
                    trailer = _read_until(sock, b"\r\n", MAX_HEAD)
                    data.extend(trailer)
                    if trailer == b"\r\n":
                        return bytes(data)
            data.extend(_read_exact(sock, size + 2))
            if len(data) > limit:
                raise InspectionError("inspection_unavailable", "body exceeds the configured inspection budget")
    lengths = _header(headers, "content-length")
    if lengths:
        if len(lengths) != 1 or not lengths[0].isdigit():
            raise InspectionError("inspection_unavailable", "ambiguous Content-Length")
        size = int(lengths[0])
        if size > limit:
            raise InspectionError("inspection_unavailable", "body exceeds the configured inspection budget")
        return _read_exact(sock, size)
    if eof:
        data = bytearray()
        while True:
            chunk = sock.recv(BUFFER)
            if not chunk:
                return bytes(data)
            data.extend(chunk)
            if len(data) > limit:
                raise InspectionError("capture_partial", "response exceeds the capture budget")
    return b""


def _normalized_host(host: str) -> str:
    value = host.strip().rstrip(".").lower()
    if value.startswith("[") and value.endswith("]"):
        value = value[1:-1]
    try:
        return str(ipaddress.ip_address(value))
    except ValueError:
        return value.encode("idna").decode("ascii")


def _path_match(path: str, prefix: str) -> bool:
    path = urlsplit(path).path or "/"
    prefix = urlsplit(prefix).path.rstrip("/") or "/"
    return prefix == "/" or path == prefix or path.startswith(prefix + "/")


def _target_match(url: str, selected: str) -> bool:
    actual, expected = urlsplit(url), urlsplit(selected)
    if actual.scheme.lower() != expected.scheme.lower() or not expected.hostname:
        return False
    if _normalized_host(actual.hostname or "") != _normalized_host(expected.hostname or ""):
        return False
    ap = actual.port or (80 if actual.scheme == "http" else 443)
    ep = expected.port or (80 if expected.scheme == "http" else 443)
    return ap == ep and _path_match(actual.path or "/", expected.path or "/")


def _normalized_value(name: str, value: Any) -> Any:
    if name in {"host", "hostname", "connected_ip", "ip"}:
        return _normalized_host(str(value or "")) if value else value
    if name in {"path", "raw_path"}:
        return str(value or "/")
    if name in {"method", "action", "protocol", "scheme", "resource_kind"}:
        return str(value or "").lower()
    return value


def _compare(actual: Any, operator: str, expected: Any, *, name: str = "") -> bool:
    if operator == "eq":
        return _normalized_value(name, actual) == _normalized_value(name, expected)
    if operator == "neq":
        return not _compare(actual, "eq", expected, name=name)
    if operator in {"in", "not_in"}:
        values = expected if isinstance(expected, list) else [expected]
        found = any(_compare(actual, "eq", item, name=name) for item in values)
        return not found if operator == "not_in" else found
    if operator == "contains":
        return str(expected) in str(actual)
    if operator == "not_contains":
        return str(expected) not in str(actual)
    if operator == "prefix":
        if name in {"path", "raw_path", ""}:
            return _path_match(str(actual), str(expected))
        return str(actual).startswith(str(expected))
    if operator == "wildcard":
        return re.match("^" + ".*".join(re.escape(part) for part in str(expected).split("*")) + "$", str(actual)) is not None
    if operator == "regex":
        return re.search(str(expected), str(actual)) is not None
    if operator == "cidr":
        try:
            return ipaddress.ip_address(str(actual)) in ipaddress.ip_network(str(expected), strict=False)
        except ValueError:
            return False
    if operator == "target":
        return _target_match(str(actual), str(expected))
    if operator in {"lt", "lte", "gt", "gte"}:
        left, right = float(actual), float(expected)
        return {"lt": left < right, "lte": left <= right, "gt": left > right, "gte": left >= right}[operator]
    if operator == "exists":
        return (actual is not None) is bool(expected)
    raise InspectionError("policy_unavailable", f"unsupported selector {operator}")


def _mapping(facts: dict[str, Any], mapping: dict[str, Any]) -> tuple[bool, list[str]]:
    unknown = []
    for name, condition in mapping.items():
        if name not in facts:
            unknown.append(name)
            continue
        operators = condition if isinstance(condition, dict) else {"eq": condition}
        for operator, expected in operators.items():
            if not _compare(facts[name], operator, expected, name=name):
                return False, []
    return not unknown, unknown


def _decision(policy: dict[str, Any], facts: dict[str, Any]) -> dict[str, Any]:
    matched = []
    unknown = []
    for rule in policy.get("rules", []):
        if not rule.get("enabled", True) or rule.get("resource_kind") != "http" or rule.get("evaluation_point") != "request":
            continue
        if rule.get("actions") and facts["method"] not in rule["actions"] and "request" not in rule["actions"]:
            continue
        states = [_mapping(facts, rule.get("selector") or {}), _mapping(facts, rule.get("conditions") or {})]
        if any(not matched and not missing for matched, missing in states):
            continue
        missing = sorted({item for _matched, missing in states for item in missing})
        if missing:
            if rule.get("mode") == "enforce":
                unknown.extend(f"{rule.get('rule_id')}:{item}" for item in missing)
            continue
        if rule.get("mode") == "enforce":
            matched.append(rule)
    denies = [rule for rule in matched if rule.get("effect") == "deny"]
    asks = [rule for rule in matched if rule.get("effect") == "ask"]
    allows = [rule for rule in matched if rule.get("effect") == "allow"]
    refs = lambda rules: [f"{rule.get('rule_id')}:v{rule.get('version')}" for rule in rules]
    target = facts["url"]
    if denies:
        return {"effect": "deny", "code": "policy_denied", "rules": refs(denies)}
    if any(_target_match(target, item) for item in policy.get("excluded_targets", [])):
        return {"effect": "deny", "code": "policy_denied"}
    if unknown:
        return {"effect": "deny", "code": "authorization_unresolved", "unknown": unknown}
    if asks:
        return {"effect": "ask", "code": "approval_required", "rules": refs(asks)}
    if not any(_target_match(target, item) for item in policy.get("authorized_targets", [])):
        actual = urlsplit(target)
        infrastructure = any(
            _normalized_host(str(item.get("host") or "")) == _normalized_host(actual.hostname or "")
            and int(item.get("port") or 0) == (actual.port or (80 if actual.scheme == "http" else 443))
            for item in policy.get("environment_requirements", {}).get("service_exceptions", [])
        )
        if not infrastructure:
            return {"effect": "deny", "code": "authorization_unresolved"}
    if allows:
        return {"effect": "allow", "code": "allowed", "rules": refs(allows)}
    default_effect = str((policy.get("defaults") or {}).get("effect") or "allow").lower()
    if default_effect == "deny":
        return {"effect": "deny", "code": "policy_denied", "rules": []}
    if default_effect == "ask":
        return {"effect": "ask", "code": "approval_required", "rules": []}
    return {"effect": "allow", "code": "default_allow", "rules": []}


def _openssl(*args: str) -> None:
    result = subprocess.run(["openssl", *args], capture_output=True, text=True, timeout=15, check=False)
    if result.returncode:
        raise InspectionError("inspection_unavailable", f"openssl failed: {result.stderr.strip()}")


def _connect(host: str, port: int) -> socket.socket:
    sock = socket.socket(socket.AF_INET6 if ":" in host else socket.AF_INET, socket.SOCK_STREAM)
    sock.settimeout(20.0)
    sock.connect((host, port))
    return sock


class CertificateAuthority:
    def __init__(self, root: Path):
        self.root = root
        self.root.mkdir(mode=0o2750, parents=True, exist_ok=True)
        self.ca_key = root / "ca.key"
        self.ca_pem = root / "ca.pem"
        self._lock = threading.Lock()
        if not self.ca_pem.exists() or not self.ca_key.exists():
            _openssl("req", "-x509", "-newkey", "rsa:3072", "-sha256", "-days", "3650", "-nodes",
                     "-keyout", str(self.ca_key), "-out", str(self.ca_pem),
                     "-subj", "/CN=Muteki SRC Inspection CA",
                     "-addext", "basicConstraints=critical,CA:TRUE",
                     "-addext", "keyUsage=critical,keyCertSign,cRLSign")
            self.ca_pem.chmod(0o644)
            self.ca_key.chmod(0o640)

    def context(self, hostname: str) -> ssl.SSLContext:
        host = _normalized_host(hostname) or "unknown"
        digest = hashlib.sha256(host.encode()).hexdigest()[:24]
        with self._lock:
            cert = self.root / f"{digest}.pem"
            key = self.root / f"{digest}.key"
            if not cert.exists() or not key.exists():
                ip = None
                try:
                    ip = ipaddress.ip_address(host)
                except ValueError:
                    pass
                san = f"IP:{ip}" if ip else f"DNS:{host}"
                config = self.root / f"{digest}.cnf"
                config.write_text(f"[v3]\nsubjectAltName={san}\nbasicConstraints=CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\nsubjectKeyIdentifier=hash\nauthorityKeyIdentifier=keyid:always\n")
                _openssl("req", "-new", "-newkey", "rsa:2048", "-nodes", "-keyout", str(key),
                         "-out", str(self.root / f"{digest}.csr"), "-subj", f"/CN={host}")
                _openssl("x509", "-req", "-in", str(self.root / f"{digest}.csr"),
                         "-CA", str(self.ca_pem), "-CAkey", str(self.ca_key), "-CAcreateserial",
                         "-out", str(cert), "-days", "825", "-sha256", "-extfile", str(config), "-extensions", "v3")
                cert.chmod(0o644)
                key.chmod(0o640)
            context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            context.minimum_version = ssl.TLSVersion.TLSv1_2
            context.load_cert_chain(str(cert), str(key))
            return context


class RawHttpProxy:
    def __init__(self, root: Path):
        self.root = root
        self.policy: dict[str, Any] = {}
        self._policy_lock = threading.RLock()
        self.ca: CertificateAuthority | None = None
        self.server: ThreadingServer | None = None

    def reload(self, policy: dict[str, Any]) -> None:
        with self._policy_lock:
            self.policy = policy
            if policy.get("inspection_requirements", {}).get("mode") == "enforce":
                self.ca = CertificateAuthority(self.root)

    def start(self) -> None:
        if self.server is not None:
            return
        proxy = self

        class Handler(ProxyHandler):
            pass

        Handler.proxy = proxy
        self.server = ThreadingServer(("0.0.0.0", 18080), Handler)
        self.server.daemon_threads = True
        thread = threading.Thread(target=self.server.serve_forever, name="src-http-proxy", daemon=True)
        thread.start()

    def snapshot(self) -> dict[str, Any]:
        with self._policy_lock:
            return dict(self.policy)

    def health(self) -> dict[str, Any]:
        policy = self.snapshot()
        mode = policy.get("inspection_requirements", {}).get("mode", "off")
        return {
            "mode": mode,
            "listening": self.server is not None,
            "enforce_ready": self.server is not None and self.ca is not None and (self.root / "ca.pem").is_file(),
            "record_semantics": "raw HTTP; TLS passthrough marked uninspected",
            "enforce_semantics": "HTTP/1.1 and TLS MITM with upstream certificate verification",
        }


class ThreadingServer(socketserver.ThreadingTCPServer):
    allow_reuse_address = True


class ProxyHandler(socketserver.BaseRequestHandler):
    proxy: RawHttpProxy

    def handle(self) -> None:
        exchange_id = uuid.uuid4().hex
        self.request.settimeout(30.0)
        self.exchange_id = exchange_id
        self.root = self.proxy.root
        self.meta: dict[str, Any] = {
            "initial_hex": "",
            "exchange_id": exchange_id, "capture_state": "unavailable",
            "request_raw_ref": "", "response_raw_ref": None, "errors": [],
        }
        try:
            peek = self.request.recv(16, socket.MSG_PEEK)
            if not peek:
                return
            self.meta["initial_hex"] = peek.hex()
            if peek[0] == 0x16:
                self._handle_tls()
            else:
                self.request = PrependSocket(self.request, b"")
                self._handle_plain()
        except (InspectionError, ssl.SSLError, OSError, ValueError) as exc:
            code = exc.code if isinstance(exc, InspectionError) else "inspection_unavailable"
            self.meta["errors"].append(f"{code}: {exc}")
            self.meta["traceback"] = traceback.format_exc()
            self.meta["capture_state"] = "partial"
            self._error(code, str(exc))
        finally:
            self._save()

    def _policy(self) -> dict[str, Any]:
        return self.proxy.snapshot()

    def _inspection(self, host: str, port: int, scheme: str, path: str = "") -> str:
        policy = self._policy()
        requirements = policy.get("inspection_requirements", {})
        mode = requirements.get("mode", "off")
        selected = requirements.get("targets") or policy.get("authorized_targets", [])
        if path:
            url = f"{scheme}://{host}:{port}{path if path.startswith('/') else '/' + path}"
            return self._request_inspection(url, policy)[0]
        norm_host = _normalized_host(host)
        for target in selected:
            expected = urlsplit(target)
            if expected.scheme.lower() != scheme.lower() or not expected.hostname:
                continue
            expected_port = expected.port or (80 if expected.scheme.lower() == "http" else 443)
            if _normalized_host(expected.hostname) == norm_host and expected_port == port:
                return mode
        return "off"

    def _request_inspection(self, url: str, policy: dict[str, Any]) -> tuple[str, str]:
        requirements = policy.get("inspection_requirements", {})
        mode = requirements.get("mode", "off")
        selected = requirements.get("targets") or policy.get("authorized_targets", [])
        if mode == "off":
            return "off", "inspection_disabled"
        if not any(_target_match(url, target) for target in selected):
            return "off", "inspection_target_not_selected"
        scheme = urlsplit(url).scheme
        for exception in requirements.get("exceptions", []):
            target = exception.get("target")
            if not target or not _target_match(url, target):
                continue
            protocol = exception.get("protocol")
            expires = float(exception.get("expires_at") or 0)
            if (not protocol or protocol == scheme) and (not expires or expires > time.time()):
                return "off", "inspection_exception"
        return mode, ""

    def _policy_host(self, host: str, port: int, scheme: str) -> str:
        try:
            address = ipaddress.ip_address(host)
        except ValueError:
            return host
        for target in self._policy().get("authorized_targets", []):
            parts = urlsplit(target)
            if not parts.hostname:
                continue
            target_port = parts.port or (80 if parts.scheme == "http" else 443)
            if parts.scheme != scheme or target_port != port:
                continue
            try:
                if any(address in ipaddress.ip_network(info[4][0]) for info in socket.getaddrinfo(parts.hostname, port)):
                    return parts.hostname
            except (ValueError, socket.gaierror):
                continue
        return host

    def _handle_tls(self) -> None:
        destination, port = self._destination()
        host = self._policy_host(destination, port, "https")
        mode = self._inspection(host, port, "https")
        if mode != "enforce":
            self._tunnel(destination, port, "uninspected_tls_record" if mode == "record" else "tls_not_selected")
            return
        base = self.proxy.ca.context(host)
        selected = {"host": host, "context": base}
        def choose(sslobj, server_name, _context):
            if server_name:
                selected["host"] = server_name
                sslobj.context = self.proxy.ca.context(server_name)
        base.sni_callback = choose
        client = PrependSocket(base.wrap_socket(self.request, server_side=True), b"")
        self.client = client
        while True:
            done = self._exchange(client, host=selected["host"], port=port, scheme="https", connect_host=destination)
            if done:
                return

    def _handle_plain(self) -> None:
        raw_head = _read_until(self.request, b"\r\n\r\n", MAX_HEAD)
        start, headers = _headers(raw_head[:-4])
        method = start.split(" ", 1)[0]
        if method.upper() == "CONNECT":
            authority = start.split(" ")[1]
            host, port = authority.rsplit(":", 1)
            mode = self._inspection(host.strip("[]"), int(port), "https")
            if mode != "enforce":
                self._tunnel(host.strip("[]"), int(port), "uninspected_tls_record" if mode == "record" else "tls_not_selected", connect=True)
                return
            self.request.sendall(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            self._handle_tls()
            return
        host_header = _header(headers, "host")
        if not host_header:
            raise InspectionError("inspection_unavailable", "HTTP Host is required")
        target = start.split(" ")[1]
        parsed = urlsplit(target)
        scheme = parsed.scheme or "http"
        host_authority = _request_authority(host_header[0], scheme)
        if parsed.hostname:
            target_authority = _request_authority(
                f"{parsed.scheme}://{parsed.netloc}", parsed.scheme, absolute=True,
            )
            if host_authority != target_authority:
                raise InspectionError(
                    "inspection_unavailable",
                    "absolute request target and Host header must identify the same origin",
                )
            _target_scheme, host, port = target_authority
        else:
            _target_scheme, host, port = host_authority
        self.client = self.request
        self._exchange(self.request, host=host, port=port, scheme=scheme, raw_head=raw_head)

    def _upstream_context(self) -> ssl.SSLContext:
        configured = self._policy().get("inspection_requirements", {}).get("upstream_ca_path")
        if configured:
            path = Path(str(configured)).resolve()
            if not path.is_relative_to(self.root.resolve()) or not path.is_file():
                raise InspectionError("inspection_unavailable", "configured upstream CA is unavailable")
            return ssl.create_default_context(cafile=str(path))
        return ssl.create_default_context()

    def _destination(self) -> tuple[str, int]:
        try:
            raw = self.request.getsockopt(socket.SOL_IP, SO_ORIGINAL_DST, 16)
            return socket.inet_ntoa(raw[4:8]), int.from_bytes(raw[2:4], "big")
        except OSError as exc:
            raise InspectionError("inspection_unavailable", "transparent destination is unavailable") from exc

    def _exchange(self, client: socket.socket, *, host: str, port: int, scheme: str, raw_head: bytes | None = None, connect_host: str | None = None) -> bool:
        raw_head = raw_head or _read_until(client, b"\r\n\r\n", MAX_HEAD)
        start, headers = _headers(raw_head[:-4])
        method, target, version = start.split(" ", 2)
        if version.lower() not in {"http/1.0", "http/1.1"}:
            raise InspectionError("inspection_unavailable", f"unsupported HTTP start line {start!r}")
        if _header(headers, "upgrade"):
            raise InspectionError("inspection_unavailable", "protocol upgrade requires the managed stream channel")
        parsed = urlsplit(target)
        host_values = _header(headers, "host")
        if len(host_values) != 1:
            raise InspectionError("inspection_unavailable", "exactly one HTTP Host header is required")
        expected_authority = (scheme, _normalized_host(host), port)
        if _request_authority(host_values[0], scheme) != expected_authority:
            raise InspectionError(
                "inspection_unavailable",
                "HTTP Host header does not match the intercepted request authority",
            )
        if parsed.hostname and _request_authority(
            f"{parsed.scheme}://{parsed.netloc}", parsed.scheme, absolute=True,
        ) != expected_authority:
            raise InspectionError(
                "inspection_unavailable",
                "absolute request target does not match the intercepted request authority",
            )
        path = (parsed.path or "/") if target.startswith("/") else parsed.path or "/"
        raw_path = path + (f"?{parsed.query}" if parsed.query else "")
        url = f"{scheme}://{host}:{port}{raw_path}"
        policy = self._policy()
        self.meta.update({"declared_target": url, "observed_url": url, "method": method.upper(), "protocol": version.lower()})
        if any(_target_match(url, target) for target in policy.get("excluded_targets", [])):
            raise InspectionError("policy_denied", "request target is excluded")
        if not any(_target_match(url, target) for target in policy.get("authorized_targets", [])):
            raise InspectionError("authorization_unresolved", "request target is outside authorized targets")
        inspection_mode, uninspected_reason = self._request_inspection(url, policy)
        limit = int(policy.get("inspection_requirements", {}).get("max_body_bytes", 64 * 1024 * 1024))
        if _header(headers, "expect") and any("100-continue" in value.lower() for value in _header(headers, "expect")):
            client.sendall(b"HTTP/1.1 100 Continue\r\n\r\n")
        body = _body(client, headers, limit)
        inspected = inspection_mode != "off"
        if inspected:
            connected_ip = socket.getaddrinfo(connect_host or host, port)[0][4][0]
            identity_header = policy.get("inspection_requirements", {}).get("identity_header")
            tenant_header = policy.get("inspection_requirements", {}).get("tenant_header")
            facts = {
                "url": url, "scheme": scheme, "host": host, "port": port, "path": path,
                "raw_path": raw_path, "method": method.upper(), "headers": [[k, v] for k, v in headers],
                "body_sha256": hashlib.sha256(body).hexdigest(), "connected_ip": connected_ip,
                "identity_ref": _header(headers, identity_header)[0] if identity_header and _header(headers, identity_header) else "",
                "tenant": _header(headers, tenant_header)[0] if tenant_header and _header(headers, tenant_header) else "",
            }
            decision = _decision(policy, facts)
            self.meta.update({"request_headers": headers, "body_sha256": facts["body_sha256"], "decision": decision})
            request_path = self.root / "traffic" / f"{self.exchange_id}.request"
            _atomic(request_path, raw_head + body)
            self.meta["request_raw_ref"] = str(request_path.relative_to(self.root))
            if decision["effect"] != "allow":
                decision = self._approval(decision, facts, request_path)
            self.meta["decision"] = decision
            if decision["effect"] != "allow":
                raise InspectionError(decision["code"], "request was not dispatched")
        else:
            self.meta.update({"capture_state": "uninspected", "reason": uninspected_reason})
        tls_client = isinstance(getattr(client, "_socket", client), ssl.SSLSocket)
        try:
            client.setblocking(False)
            pending = client.recv(1) if tls_client else client.recv(1, socket.MSG_PEEK)
            if tls_client and pending:
                client.prepend(pending)
        except (BlockingIOError, ssl.SSLWantReadError, ssl.SSLWantWriteError):
            pending = None
        except (OSError, ValueError) as exc:
            raise InspectionError("approval_expired", "client disconnected before upstream dispatch") from exc
        finally:
            client.setblocking(True)
        if pending == b"":
            raise InspectionError("approval_expired", "client disconnected before upstream dispatch")
        upstream = _connect(connect_host or host, port)
        if scheme == "https":
            upstream = self._upstream_context().wrap_socket(upstream, server_hostname=host)
        upstream = PrependSocket(upstream, b"")
        self.upstream = upstream
        self.meta["connected_ip"] = upstream.getpeername()[0]
        upstream.sendall(raw_head + body)
        response_head = _read_until(upstream, b"\r\n\r\n", MAX_HEAD)
        response_start, response_headers = _headers(response_head[:-4])
        while response_start.split(" ", 1)[1].startswith("1"):
            status_token = response_start.split(" ")[1]
            if status_token == "101":
                raise InspectionError("inspection_unavailable", "protocol upgrade requires the managed stream channel")
            client.sendall(response_head)
            response_head = _read_until(upstream, b"\r\n\r\n", MAX_HEAD)
            response_start, response_headers = _headers(response_head[:-4])
        status_code = int(response_start.split(" ")[1])
        if method.upper() == "HEAD" or status_code in {204, 304} or 100 <= status_code < 200:
            response_body = b""
        else:
            response_body = _body(
                upstream, response_headers, limit,
                eof=not _header(response_headers, "content-length")
                and "chunked" not in ",".join(_header(response_headers, "transfer-encoding")).lower(),
            )
        if inspected:
            response_path = self.root / "traffic" / f"{self.exchange_id}.response"
            _atomic(response_path, response_head + response_body)
            self.meta.update({
                "response_raw_ref": str(response_path.relative_to(self.root)),
                "response_headers": response_headers,
                "status_code": status_code, "capture_state": "complete",
            })
        else:
            self.meta["status_code"] = status_code
        client.sendall(response_head + response_body)
        return True

    def _grant(self, decision: dict[str, Any], facts: dict[str, Any]) -> dict[str, Any] | None:
        policy = self._policy()
        policy_digest = hashlib.sha256(_json(policy)).hexdigest()
        for path in (self.root / "inspection_grants").glob("*.json"):
            try:
                grant = json.loads(path.read_text(encoding="utf-8"))
            except (OSError, ValueError):
                continue
            if grant.get("revoked_at") or float(grant.get("expires_at") or 0) <= time.time():
                continue
            if grant.get("policy_digest") != policy_digest or not set(decision.get("rules", [])).issubset(set(grant.get("rule_refs", []))):
                continue
            expected = grant.get("resource") or {}
            if all(facts.get(key) == expected.get(key) for key in ("path", "method", "identity_ref", "tenant")):
                return {"effect": "allow", "code": "task_grant", "grant_digest": grant.get("digest")}
        return None

    def _approval(self, decision: dict[str, Any], facts: dict[str, Any], request_path: Path) -> dict[str, Any]:
        if decision["effect"] != "ask":
            return decision
        grant = self._grant(decision, facts)
        if grant is not None:
            return grant
        request_id = self.exchange_id
        reusable = all(
            rule.get("reusable_approval") for rule in self._policy().get("rules", [])
            if f"{rule.get('rule_id')}:v{rule.get('version')}" in set(decision.get("rules", []))
        )
        payload = {
            "request_id": request_id, "state": "pending", "digest": hashlib.sha256(_json(facts)).hexdigest(),
            "facts": facts, "matched_rule_refs": decision.get("rules", []),
            "reusable_approval": reusable,
            "policy_digest": hashlib.sha256(_json(self._policy())).hexdigest(),
            "request_raw_ref": str(request_path.relative_to(self.root)), "requested_at": time.time(),
        }
        _atomic(self.root / "inspection_requests" / f"{request_id}.json", _json(payload))
        deadline = time.monotonic() + int(self._policy().get("inspection_requirements", {}).get("approval_timeout_seconds", 300))
        path = self.root / "inspection_decisions" / f"{request_id}.json"
        while time.monotonic() < deadline:
            try:
                answer = json.loads(path.read_text(encoding="utf-8"))
            except (OSError, ValueError):
                time.sleep(0.2)
                continue
            if answer.get("request_digest") != payload["digest"]:
                raise InspectionError("approval_stale", "inspection approval is bound to different input")
            payload["state"] = "approved" if answer.get("approved") else "denied"
            _atomic(self.root / "inspection_requests" / f"{request_id}.json", _json(payload))
            if answer.get("approved"):
                return {"effect": "allow", "code": "approved", "rules": decision.get("rules", [])}
            return {"effect": "deny", "code": "policy_denied"}
        raise InspectionError("approval_required", "inspection approval wait expired")

    def _tunnel(self, host: str, port: int, reason: str, connect: bool = False) -> None:
        upstream = _connect(host, port)
        if connect:
            self.request.sendall(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        self.meta.update({"capture_state": "uninspected", "reason": reason, "observed_target": f"{host}:{port}"})
        sockets = [self.request, upstream]
        while True:
            readable, _writable, exceptional = select.select(sockets, [], sockets, 30.0)
            if exceptional or not readable:
                return
            for source in readable:
                data = source.recv(BUFFER)
                if not data:
                    return
                (upstream if source is self.request else self.request).sendall(data)

    def _error(self, code: str, message: str) -> None:
        body = _json({"error": {"code": code, "message": message, "exchange_id": self.exchange_id}})
        try:
            target = getattr(self, "client", self.request)
            target.sendall(
                b"HTTP/1.1 502 Bad Gateway\r\nContent-Type: application/json\r\nContent-Length: "
                + str(len(body)).encode() + b"\r\nConnection: close\r\n\r\n" + body)
        except OSError:
            pass

    def _save(self) -> None:
        try:
            _atomic(self.root / "traffic" / f"{self.exchange_id}.json", _json(self.meta))
        except OSError:
            pass
        for sock in (getattr(self, "client", None), getattr(self, "upstream", None), self.request):
            try:
                if sock:
                    sock.close()
            except OSError:
                pass
