#!/usr/bin/env python3
import json
import os
import signal
import time
from pathlib import Path

from http_proxy import RawHttpProxy

ROOT = Path("/run/src")
POLICY_PATH = ROOT / "policy.json"
HEALTH_PATH = ROOT / "proxy-health.json"
proxy = RawHttpProxy(ROOT)


def write_health(state, error=""):
    health = proxy.health()
    health.update({"state": state, "error": error, "pid": os.getpid(), "verified_at": time.time()})
    temporary = HEALTH_PATH.with_suffix(".tmp")
    temporary.write_text(json.dumps(health))
    temporary.replace(HEALTH_PATH)


def reload_policy(_signum=None, _frame=None):
    try:
        proxy.reload(json.loads(POLICY_PATH.read_text()))
        proxy.start()
    except Exception as exc:
        write_health("failed", f"{type(exc).__name__}: {exc}")
        raise
    write_health("ready")


def stop(_signum=None, _frame=None):
    raise SystemExit(0)


def main():
    signal.signal(signal.SIGHUP, reload_policy)
    signal.signal(signal.SIGTERM, stop)
    reload_policy()
    while True:
        time.sleep(3600)


if __name__ == "__main__":
    main()
