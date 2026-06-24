"""muteki_kit.submit — flag submission helper (no real network).

Confidence-gated (don't brute-force the endpoint) and server-authoritative (trust
the response, never self-assume). These tests fake `requests.post` so they run in
normal CI.
"""

from __future__ import annotations

import json
from typing import Any

import pytest

import muteki_kit.submit as submit_mod
from muteki_kit import submit_flag
from muteki_kit.submit import FlagSubmission


class _FakeResp:
    def __init__(self, body: Any, status: int = 200) -> None:
        self._body = body
        self.status_code = status
        self.text = body if isinstance(body, str) else json.dumps(body)

    def json(self) -> Any:
        if isinstance(self._body, str):
            raise ValueError("not json")
        return self._body


@pytest.fixture
def capture_post(monkeypatch):
    """Capture the outgoing POST and return a programmable fake response."""
    sent: dict[str, Any] = {}

    def make(body: Any, status: int = 200):
        def _post(endpoint, json=None, headers=None, timeout=None):  # noqa: A002
            sent["endpoint"] = endpoint
            sent["json"] = json
            sent["headers"] = headers
            return _FakeResp(body, status)
        monkeypatch.setattr(submit_mod.requests, "post", _post)
        return sent

    return make


def test_confidence_gate_blocks_non_flag_shaped_without_posting(monkeypatch, capture_post):
    posted = capture_post({"correct": True})  # would say correct IF we hit it
    r = submit_flag("not-a-flag", code="c1", server="host")
    assert isinstance(r, FlagSubmission)
    assert r.submitted is False
    assert r.correct is None
    assert "confidence gate" in r.message
    assert posted == {}  # we never POSTed — endpoint is not a brute-force oracle


def test_force_bypasses_the_gate(capture_post):
    sent = capture_post({"correct": False, "message": "nope"})
    r = submit_flag("anything", code="c1", server="host", force=True)
    assert r.submitted is True
    assert sent["endpoint"] == "http://host/api/submit"


def test_correct_flag_reports_server_verdict(capture_post):
    sent = capture_post({"correct": True, "message": "well done"})
    r = submit_flag("flag{real_one}", code="web-7", server="ctf.local", token="tok")
    assert r.submitted is True
    assert r.correct is True
    assert r.message == "well done"
    # endpoint + auth header + payload shape match the submission contract
    assert sent["endpoint"] == "http://ctf.local/api/submit"
    assert sent["headers"]["Agent-Token"] == "tok"
    assert sent["json"] == {"code": "web-7", "flag": "flag{real_one}"}


def test_incorrect_flag_is_not_assumed_correct(capture_post):
    capture_post({"status": "incorrect"})
    r = submit_flag("flag{wrong}", code="c", server="h")
    assert r.submitted is True
    assert r.correct is False


def test_already_solved_detected(capture_post):
    capture_post({"correct": True, "already_solved": True, "message": "dup"})
    r = submit_flag("flag{dup}", code="c", server="h")
    assert r.correct is True
    assert r.already_solved is True


def test_plaintext_success_body(capture_post):
    capture_post("Correct! Congratulations")
    r = submit_flag("flag{txt}", code="c", server="h")
    assert r.submitted is True and r.correct is True


def test_unparseable_verdict_is_unknown_not_false(capture_post):
    capture_post({"echo": "thanks"})  # no verdict field
    r = submit_flag("flag{x}", code="c", server="h")
    assert r.submitted is True
    assert r.correct is None  # conservative: unknown, not a false negative


def test_env_vars_used_when_server_token_omitted(monkeypatch, capture_post):
    monkeypatch.setenv("MUTEKI_SUBMIT_HOST", "envhost:9000")
    monkeypatch.setenv("MUTEKI_SUBMIT_TOKEN", "envtok")
    sent = capture_post({"correct": True})
    r = submit_flag("flag{from_env}", code="c")
    assert r.correct is True
    assert sent["endpoint"] == "http://envhost:9000/api/submit"
    assert sent["headers"]["Agent-Token"] == "envtok"


def test_no_endpoint_returns_clear_message(monkeypatch, capture_post):
    monkeypatch.delenv("MUTEKI_SUBMIT_HOST", raising=False)
    posted = capture_post({"correct": True})
    r = submit_flag("flag{x}", code="c")  # no server, no env
    assert r.submitted is False
    assert "endpoint" in r.message
    assert posted == {}


def test_network_error_is_caught(monkeypatch):
    import requests as _rq

    def _boom(*a, **k):
        raise _rq.ConnectionError("refused")
    monkeypatch.setattr(submit_mod.requests, "post", _boom)
    r = submit_flag("flag{x}", code="c", server="h")
    assert r.submitted is False
    assert "failed" in r.message


def test_empty_flag_short_circuits():
    r = submit_flag("   ", code="c", server="h")
    assert r.submitted is False
    assert "empty" in r.message
