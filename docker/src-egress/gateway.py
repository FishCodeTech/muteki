#!/usr/bin/env python3
import hashlib
import json
import os
import signal
import socket
import subprocess
import sys
import time
from pathlib import Path
from urllib.parse import urlsplit

ROOT = Path("/run/src")
POLICY_PATH = ROOT / "policy.json"
HEALTH_PATH = ROOT / "health.json"
PROXY_HEALTH_PATH = ROOT / "proxy-health.json"
PROXY_PROCESS = None
PROXY_UID = 999


def run(*args):
    result = subprocess.run(args, capture_output=True, text=True, check=False)
    if result.returncode:
        raise RuntimeError(f"{' '.join(args)}: {result.stderr.strip() or result.stdout.strip()}")


def targets(policy):
    for target in policy.get("authorized_targets", []):
        parts = urlsplit(target)
        if parts.scheme not in {"http", "https"} or not parts.hostname:
            continue
        yield parts.hostname, parts.port or (80 if parts.scheme == "http" else 443), "tcp", "test-target", target
    for item in policy.get("environment_requirements", {}).get("service_exceptions", []):
        host = str(item.get("host") or "")
        if host:
            yield host, int(item["port"]), str(item.get("protocol") or "tcp"), str(item.get("purpose") or "infrastructure"), ""


def addresses(host):
    rows = set()
    for family, _type, _proto, _canonical, sockaddr in socket.getaddrinfo(host, None):
        if family in {socket.AF_INET, socket.AF_INET6}:
            rows.add((family, sockaddr[0]))
    if not rows:
        raise RuntimeError(f"no address for {host}")
    return rows


def same_target(left, right):
    a, b = urlsplit(left), urlsplit(right)
    if a.scheme != b.scheme or (a.hostname or "").rstrip(".").lower() != (b.hostname or "").rstrip(".").lower():
        return False
    ap = a.port or (80 if a.scheme == "http" else 443)
    bp = b.port or (80 if b.scheme == "http" else 443)
    prefix = (b.path or "/").rstrip("/") or "/"
    path = a.path or "/"
    return ap == bp and (prefix == "/" or path == prefix or path.startswith(prefix + "/"))


def inspection_selected(policy, target):
    requirements = policy.get("inspection_requirements", {})
    selected = requirements.get("targets") or policy.get("authorized_targets", [])
    if not any(same_target(target, item) or same_target(item, target) for item in selected):
        return False
    for exception in requirements.get("exceptions", []):
        if exception.get("target") and same_target(target, exception["target"]):
            protocol = exception.get("protocol")
            expires = float(exception.get("expires_at") or 0)
            if (not protocol or protocol == urlsplit(target).scheme) and (not expires or expires > time.time()):
                return False
    return True


def proxy_health():
    try:
        return json.loads(PROXY_HEALTH_PATH.read_text())
    except (OSError, ValueError):
        return {"state": "starting", "listening": False, "enforce_ready": False}


def ensure_proxy():
    global PROXY_PROCESS
    requested_at = time.time()
    if PROXY_PROCESS is not None and PROXY_PROCESS.poll() is None:
        os.kill(PROXY_PROCESS.pid, signal.SIGHUP)
    else:
        PROXY_HEALTH_PATH.unlink(missing_ok=True)
        gid = int(os.environ.get("MUTEKI_SRC_GID", "0"))
        def demote():
            os.setgid(gid)
            os.setuid(PROXY_UID)
        PROXY_PROCESS = subprocess.Popen(
            [sys.executable, "/usr/local/bin/proxy_process.py"],
            preexec_fn=demote,
        )
    deadline = time.monotonic() + 30.0
    while time.monotonic() < deadline:
        health = proxy_health()
        if health.get("state") == "ready" and float(health.get("verified_at") or 0) >= requested_at:
            return
        if health.get("state") == "failed":
            raise RuntimeError(health.get("error") or "proxy failed")
        if PROXY_PROCESS.poll() is not None:
            raise RuntimeError(f"proxy exited with status {PROXY_PROCESS.returncode}")
        time.sleep(0.05)
    raise RuntimeError("proxy did not become ready within 30 seconds")


def excluded_endpoints(policy):
    rows = []
    for target in policy.get("excluded_targets", []):
        parts = urlsplit(target)
        if parts.scheme not in {"http", "https"} or not parts.hostname:
            continue
        path = (parts.path or "/").rstrip("/") or "/"
        if path == "/":
            rows.append((parts.hostname, parts.port or (80 if parts.scheme == "http" else 443), "tcp"))
    return rows


def local_addresses():
    addrs = {socket.AF_INET: {"127.0.0.0/8"}, socket.AF_INET6: {"::1/128"}}
    try:
        hostname = socket.gethostname()
        for family, _type, _proto, _canon, sockaddr in socket.getaddrinfo(hostname, None):
            if family in addrs:
                addrs[family].add(sockaddr[0])
    except OSError:
        pass
    try:
        out = subprocess.run(["hostname", "-I"], capture_output=True, text=True, check=False)
        for token in (out.stdout or "").split():
            if ":" in token:
                addrs[socket.AF_INET6].add(token)
            elif token.count(".") == 3:
                addrs[socket.AF_INET].add(token)
    except OSError:
        pass
    return addrs


def apply(policy):
    local_addrs = local_addresses()
    resolved = []
    inspection_mode = policy.get("inspection_requirements", {}).get("mode", "off")
    target_entries = []
    for host, port, protocol, purpose, original_target in targets(policy):
        for family, address in addresses(host):
            target_entries.append((family, host, address, port, protocol, purpose, original_target))
            resolved.append({"host": host, "address": address, "port": port, "protocol": protocol, "purpose": purpose})
    excluded_entries = []
    for host, port, protocol in excluded_endpoints(policy):
        for family, address in addresses(host):
            excluded_entries.append((family, address, port, protocol))
    for family, table in ((socket.AF_INET, "iptables"), (socket.AF_INET6, "ip6tables")):
        active_a = subprocess.run([table, "-C", "OUTPUT", "-j", "SRC_EGRESS_A"], capture_output=True).returncode == 0
        next_chain = "SRC_EGRESS_B" if active_a else "SRC_EGRESS_A"
        prev_chain = "SRC_EGRESS_A" if active_a else "SRC_EGRESS_B"
        next_nat = "SRC_EGRESS_NAT_B" if active_a else "SRC_EGRESS_NAT_A"
        prev_nat = "SRC_EGRESS_NAT_A" if active_a else "SRC_EGRESS_NAT_B"
        subprocess.run([table, "-N", next_chain], capture_output=True)
        run(table, "-F", next_chain)
        run(table, "-A", next_chain, "-o", "lo", "-j", "ACCEPT")
        for addr in sorted(local_addrs.get(family, set())):
            run(table, "-A", next_chain, "-d", addr, "-p", "tcp", "--dport", "18080", "-j", "ACCEPT")
        if family == socket.AF_INET:
            run(table, "-A", next_chain, "-d", "127.0.0.11", "-p", "udp", "--dport", "53", "-j", "ACCEPT")
            run(table, "-A", next_chain, "-d", "127.0.0.11", "-p", "tcp", "--dport", "53", "-j", "ACCEPT")
        subprocess.run([table, "-t", "nat", "-N", next_nat], capture_output=True)
        run(table, "-t", "nat", "-F", next_nat)
        run(table, "-t", "nat", "-A", next_nat, "-m", "owner", "--uid-owner", str(PROXY_UID), "-j", "RETURN")
        for ex_family, address, port, protocol in excluded_entries:
            if ex_family == family:
                run(table, "-A", next_chain, "-d", address, "-p", protocol, "--dport", str(port), "-j", "REJECT")
        for t_family, _host, address, port, protocol, purpose, original_target in target_entries:
            if t_family != family:
                continue
            run(table, "-A", next_chain, "-d", address, "-p", protocol, "--dport", str(port), "-j", "ACCEPT")
            if inspection_mode in {"record", "enforce"} and purpose == "test-target" and inspection_selected(policy, original_target):
                run(table, "-t", "nat", "-A", next_nat, "-d", address, "-p", "tcp", "--dport", str(port), "-j", "REDIRECT", "--to-ports", "18080")
        run(table, "-A", next_chain, "-m", "conntrack", "--ctstate", "ESTABLISHED,RELATED", "-j", "ACCEPT")
        run(table, "-A", next_chain, "-j", "REJECT")
        run(table, "-t", "nat", "-A", next_nat, "-j", "RETURN")
        run(table, "-I", "OUTPUT", "1", "-j", next_chain)
        while subprocess.run([table, "-D", "OUTPUT", "-j", prev_chain], capture_output=True).returncode == 0:
            pass
        run(table, "-t", "nat", "-I", "OUTPUT", "1", "-j", next_nat)
        while subprocess.run([table, "-t", "nat", "-D", "OUTPUT", "-j", prev_nat], capture_output=True).returncode == 0:
            pass
    ensure_proxy()
    return resolved


def digest(policy):
    return hashlib.sha256(json.dumps(policy, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def write_health(policy, state, resolved, error=""):
    temporary = HEALTH_PATH.with_suffix(".tmp")
    temporary.write_text(json.dumps({
        "state": state, "digest": digest(policy), "generation": policy.get("environment_generation"),
        "resolved": resolved, "error": error, "verified_at": time.time(), "pid": os.getpid(),
        "network_enforced": state == "ready", "inspection": proxy_health(),
    }))
    temporary.replace(HEALTH_PATH)


def read_policy():
    deadline = time.monotonic() + 5.0
    while True:
        try:
            return json.loads(POLICY_PATH.read_text())
        except (OSError, json.JSONDecodeError):
            if time.monotonic() >= deadline:
                raise
            time.sleep(0.05)


def main():
    policy = {}
    def reload(_signum=None, _frame=None):
        nonlocal policy
        candidate = read_policy()
        try:
            resolved = apply(candidate)
        except Exception as exc:
            write_health(candidate, "failed", [], str(exc))
            raise
        policy = candidate
        write_health(policy, "ready", resolved)
    def stop(_signum=None, _frame=None):
        if PROXY_PROCESS is not None and PROXY_PROCESS.poll() is None:
            PROXY_PROCESS.terminate()
            try:
                PROXY_PROCESS.wait(timeout=10)
            except subprocess.TimeoutExpired:
                PROXY_PROCESS.kill()
                PROXY_PROCESS.wait(timeout=5)
        raise SystemExit(0)
    signal.signal(signal.SIGHUP, reload)
    signal.signal(signal.SIGTERM, stop)
    deadline = time.monotonic() + 30.0
    while not POLICY_PATH.is_file():
        if time.monotonic() >= deadline:
            raise RuntimeError("policy.json did not become available within 30 seconds")
        time.sleep(0.05)
    reload()
    while True:
        if PROXY_PROCESS is not None and PROXY_PROCESS.poll() is not None:
            write_health(policy, "failed", [], f"proxy exited with status {PROXY_PROCESS.returncode}")
            raise SystemExit(1)
        time.sleep(1)


if __name__ == "__main__":
    main()
