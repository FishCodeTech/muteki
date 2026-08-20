"""Host-side pentest report completeness and value heuristics."""

from __future__ import annotations

import json

from muteki.solver.vuln_report import (
    VALUE_REJECT_INCOMPLETE,
    VALUE_REJECT_SELF_XSS,
    completeness_code,
    heuristic_value_code,
    missing_report_fields,
    normalize_report,
    parse_report_documents,
    parse_report_text,
    persist_report_collection,
    render_report_collection_markdown,
    render_report_markdown,
    replay_attempted,
    witness_in_corpus,
)


def _complete(**overrides):
    base = {
        "title": "Authenticated UNION SQL injection reads other users",
        "finding_class": "sqli",
        "resource_id": "http://127.0.0.1:4280/vulnerabilities/sqli/",
        "preconditions": "Valid low-privilege session cookie.",
        "affected_role": "Other application users in the users table.",
        "impact": {
            "who": "Other accounts whose credentials sit in the users table.",
            "what": "UNION SELECT returns their usernames and password hashes.",
        },
        "steps": [
            "Log in at /login.php with a valid account.",
            "Open /vulnerabilities/sqli/ and submit a UNION SELECT payload.",
        ],
        "replay": {
            "command": "curl -s -b COOKIE 'http://127.0.0.1:4280/vulnerabilities/sqli/?id=1'"
        },
        "witness": "admin:5f4dcc3b5aa765d61d8327deb882cf99",
    }
    base.update(overrides)
    return normalize_report(base)


def test_reports_bundle_unwraps_each_document():
    blob = json.dumps({
        "reports": [
            _complete(title="Authenticated UNION SQL injection reads other users"),
            _complete(
                title="OS command injection in ip returns id output",
                finding_class="rce",
                resource_id="http://127.0.0.1:4280/vulnerabilities/exec/",
            ),
        ]
    })
    docs = parse_report_documents(blob)
    assert len(docs) == 2
    assert docs[0]["finding_class"] == "sqli"
    assert docs[1]["finding_class"] == "rce"
    report = parse_report_text('{"title":"xss","finding_class":"xss"}')
    assert missing_report_fields(report or {})
    assert completeness_code(report) == VALUE_REJECT_INCOMPLETE


def test_complete_in_scope_report_passes():
    report = _complete()
    assert completeness_code(
        report, scope="http://127.0.0.1:4280", target="http://127.0.0.1:4280",
    ) == "ok"


def test_garbled_fullwidth_target_does_not_raise():
    from muteki.solver.gate import _origin_from_url, parse_engagement_scope

    garbled = "http://127.0.0.1:4280）做黑盒渗透。账号"
    origin = _origin_from_url(garbled)
    assert origin is not None
    assert origin.host == "127.0.0.1"
    assert origin.port == 4280
    allow = parse_engagement_scope(
        "仅 http://127.0.0.1:4280 ，禁止 DoS", garbled)
    assert any(item.host == "127.0.0.1" and item.port == 4280 for item in allow)
    report = _complete()
    assert completeness_code(
        report, scope="仅 http://127.0.0.1:4280 ，禁止 DoS", target=garbled,
    ) == "ok"


def test_out_of_scope_report_is_rejected():
    report = _complete(resource_id="http://evil.test/x")
    assert completeness_code(
        report, scope="http://127.0.0.1:4280", target="http://127.0.0.1:4280",
    ) == "out_of_scope"


def test_reflected_xss_is_self_xss():
    report = _complete(
        title="Reflected XSS in the name parameter",
        finding_class="xss",
        resource_id="http://127.0.0.1:4280/vulnerabilities/xss_r/",
        affected_role="Only the tester's browser.",
        impact={
            "who": "The attacker who submits the payload.",
            "what": "The script executes in the same response in the tester browser.",
        },
    )
    assert heuristic_value_code(report) == VALUE_REJECT_SELF_XSS


def test_stored_xss_is_not_heuristic_rejected():
    report = _complete(
        title="Stored XSS in guestbook executes for other visitors",
        finding_class="xss",
        resource_id="http://127.0.0.1:4280/vulnerabilities/xss_s/",
        affected_role="Other users who open the guestbook.",
        impact={
            "who": "Other application users viewing the guestbook.",
            "what": "The stored script runs in their browser and can send their session cookie.",
        },
    )
    assert heuristic_value_code(report) is None


def test_witness_and_replay_helpers():
    report = _complete()
    assert witness_in_corpus(report["witness"], "uid=33\nadmin:5f4dcc3b5aa765d61d8327deb882cf99\n")
    assert not witness_in_corpus(report["witness"], "nope")
    assert replay_attempted(report, ["curl -s http://127.0.0.1:4280/vulnerabilities/sqli/"])
    assert not replay_attempted(report, ["echo hello"])


def test_ensure_report_repro_intents_proposes_verifier_ahead_of_code(tmp_path):
    from muteki.models.solve_graph import Challenge
    from muteki.solver.vuln_report import normalize_report, repro_intent_id
    from muteki.swarm.coordinator_flags import _FlagsBusMixin
    from muteki.swarm.shared_graph import SQLiteSharedGraph

    ch = Challenge(
        id="c1", name="dvwa", category="web", mode="pentest",
        goal="收集 3 个 Web 漏洞", scope="http://127.0.0.1:4280",
        target="http://127.0.0.1:4280",
    )
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=ch)
    g.propose_intent(actor="planner", intent_id="I-plan-explore", goal="probe stored xss")
    report = normalize_report(_complete())
    g.report_submitted(actor="cli-1", report=report)
    host = _FlagsBusMixin.__new__(_FlagsBusMixin)
    host.challenge = ch
    host.shared_graph = g
    assert host._ensure_report_repro_intents() == 1
    iid = repro_intent_id(report["report_id"])
    rows = g.query_legacy_candidates(now=0)
    assert [row["intent_id"] for row in rows][:2] == [iid, "I-plan-explore"]
    assert rows[0]["worker_class"] == "verifier"
    items = host._verifier_dispatch_items(timeout=120)
    assert items[0]["intent_id"] == iid
    assert items[0]["mode"] == "verifier"
    g.close()


def test_failed_repro_is_retried_by_reopening_verifier_intent(tmp_path):
    from muteki.models.solve_graph import Challenge
    from muteki.solver.vuln_report import normalize_report, repro_intent_id
    from muteki.swarm.coordinator_flags import _FlagsBusMixin
    from muteki.swarm.shared_graph import SQLiteSharedGraph

    ch = Challenge(
        id="c1", name="dvwa", category="web", mode="pentest",
        goal="收集 3 个 Web 漏洞", scope="http://127.0.0.1:4280",
        target="http://127.0.0.1:4280",
    )
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=ch)
    report = normalize_report(_complete())
    g.report_submitted(actor="cli-1", report=report)
    host = _FlagsBusMixin.__new__(_FlagsBusMixin)
    host.challenge = ch
    host.shared_graph = g
    host._ensure_report_repro_intents()
    iid = repro_intent_id(report["report_id"])
    assert g.claim_intent(worker="cli-v1", intent_id=iid)
    g.report_repro_decision(
        actor="cli-v1", report_id=report["report_id"],
        reproduced=False, detail="witness missing", intent_id=iid)
    g.conclude_intent(actor="cli-v1", intent_id=iid, result="explored")
    pending = g.pending_report_repros()
    assert len(pending) == 1
    assert host._ensure_report_repro_intents() == 1
    state = g.intent_claim_state(iid)
    assert state["status"] == "open"
    assert state["dispatch_state"] == "active"
    items = host._verifier_dispatch_items(timeout=120)
    assert items[0]["intent_id"] == iid
    g.close()


def test_render_report_markdown_is_complete_document():
    report = _complete()
    md = render_report_markdown(report)
    assert md.startswith("# Authenticated UNION")
    for heading in ("## 漏洞概要", "## 影响", "## 复现步骤", "## PoC", "## 证明输出"):
        assert heading in md
    assert "**类型**：SQL 注入" in md
    assert "**位置**：" in md
    assert "**严重程度**：高危（类型估算）" in md
    assert "**参考向量**：8.1" in md
    assert "**先决条件**：" in md
    assert "**影响对象**：" in md
    assert "```bash" in md
    assert "admin:5f4dcc3b5aa765d61d8327deb882cf99" in md
    assert "Valid low-privilege session cookie." in md
    assert "Other application users in the users table." in md


def test_collection_markdown_demotes_titles():
    first = _complete()
    second = _complete(
        title="OS command injection in ip returns id output",
        finding_class="rce",
        resource_id="http://127.0.0.1:4280/vulnerabilities/exec/",
    )
    md = render_report_collection_markdown([first, second], title="DVWA 漏洞报告集")
    assert md.startswith("# DVWA 漏洞报告集")
    assert "## 1. Authenticated UNION" in md
    assert "## 2. OS command injection" in md
    assert "### 漏洞概要" in md
    assert md.count("```bash") == 2


def test_persist_report_collection_writes_files(tmp_path):
    report = _complete()
    path = persist_report_collection(tmp_path, [report], title="测试报告集")
    assert path.name == "COLLECTION.md"
    named = tmp_path / f"01-sqli-{report['report_id'][:8]}.md"
    assert named.exists()
    body = path.read_text(encoding="utf-8")
    assert "# 测试报告集" in body
    assert "## 复现步骤" in named.read_text(encoding="utf-8")
