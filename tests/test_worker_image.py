"""P2-v3 worker-image health (apps/web/worker_image.py).

Pure unit — no real docker. We monkeypatch worker_image._docker to script the
docker CLI replies and assert the four-check status + overall colour.
"""

from __future__ import annotations

import subprocess

import apps.web.worker_image as wi


def _fake(returncode=0, stdout="", stderr=""):
    return subprocess.CompletedProcess(args=["docker"], returncode=returncode,
                                       stdout=stdout, stderr=stderr)


def _script(monkeypatch, *, daemon=True, present=True, version="0.1.0"):
    """Install a _docker stub that answers version/inspect by the args."""
    def fake_docker(*args, timeout=20.0):
        if args[:1] == ("version",):
            return _fake(0, "27.3.1") if daemon else _fake(1, stderr="Cannot connect")
        if args[:2] == ("image", "inspect"):
            if not present:
                return _fake(1, stderr="No such image")
            # version label query vs id query (last format arg differs)
            fmt = args[-1]
            if "labels" in fmt.lower() or "version" in fmt.lower():
                return _fake(0, version if version is not None else "<no value>")
            return _fake(0, "sha256:abc")
        if args[:1] == ("pull",):
            return _fake(0, "Status: Downloaded")
        return _fake(1, stderr="unexpected")
    monkeypatch.setattr(wi, "_docker", fake_docker)


def test_green_when_pulled_and_version_matches(monkeypatch):
    monkeypatch.setattr(wi, "EXPECTED_WORKER_VERSION", "0.1.0")
    _script(monkeypatch, daemon=True, present=True, version="0.1.0")
    s = wi.image_status()
    assert s["daemon"]["ok"] and s["pulled"]["ok"]
    assert s["version"]["status"] == "match"
    assert s["overall"] == "green"


def test_red_when_not_pulled(monkeypatch):
    monkeypatch.setattr(wi, "EXPECTED_WORKER_VERSION", "0.1.0")
    _script(monkeypatch, daemon=True, present=False)
    s = wi.image_status()
    assert s["pulled"]["ok"] is False
    assert s["overall"] == "red"


def test_red_when_daemon_unreachable(monkeypatch):
    _script(monkeypatch, daemon=False)
    s = wi.image_status()
    assert s["daemon"]["ok"] is False
    assert s["overall"] == "red"


def test_yellow_on_version_mismatch(monkeypatch):
    monkeypatch.setattr(wi, "EXPECTED_WORKER_VERSION", "0.2.0")
    _script(monkeypatch, daemon=True, present=True, version="0.1.0")
    s = wi.image_status()
    assert s["version"]["status"] == "mismatch"
    assert s["overall"] == "yellow"


def test_green_unknown_version_when_no_expected(monkeypatch):
    monkeypatch.setattr(wi, "EXPECTED_WORKER_VERSION", "")
    _script(monkeypatch, daemon=True, present=True, version="0.1.0")
    s = wi.image_status()
    # no expected version → informational only, still green if pulled
    assert s["version"]["status"] == "unknown"
    assert s["overall"] == "green"
