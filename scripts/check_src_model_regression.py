from __future__ import annotations

import argparse
import asyncio
import base64
from datetime import datetime, timezone
import json
import os
import sys
import tempfile
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from muteki.platform.store import PlatformStore
from muteki.src.artifacts import SrcArtifactStore
from muteki.src.knowledge import (
    CodexRuntimeExecutor, RegressionScenario, RegressionSuite, SrcKnowledgeBase,
    default_regression_suite, run_regression_suite,
)
from muteki.src.store import SrcStore, SrcStoreError


def expect_error(code: str, operation) -> None:
    try:
        operation()
    except SrcStoreError as exc:
        assert exc.code == code, (code, exc.code, exc.message)
        return
    raise AssertionError(f"expected {code}")


def authorizer(task_id: str, principal_id: str, action: str) -> bool:
    return principal_id == "local-user" and task_id in {"task-a", "task-b", "src-model-regression"}


def publish_fixture(
    kb: SrcKnowledgeBase, ref, dataset_id: str, entry_id: str,
    idempotency_key: str,
):
    return kb.publish(
        "task-a", "local-user", source_kind="frozen_report", source_id="report-1",
        source_revision=1, selected_source_fields=["title", "finding"],
        entry={
            "kind": "case", "title": "跨身份对象读取",
            "problem_pattern": "服务端对象所有权校验",
            "business_preconditions": {"accounts": ["A", "B"], "tenant": "test"},
            "effective_experiments": [{"baseline": "A reads own", "experiment": "B reads A"}],
            "established_mechanism": "对象标识变化后服务端返回另一账号的对象",
            "decisive_evidence": ["request-A", "request-B"],
            "repair_boundary": "按服务端主体和租户校验对象",
            "invalidation_conditions": ["对象模型变化"],
            "applicability": {"requires_live_validation": True},
        },
        evidence_refs=[ref], dataset_id=dataset_id, entry_id=entry_id,
        validity={"tool_version": "http-v1"}, idempotency_key=idempotency_key,
    )


def check_knowledge(root: Path) -> dict[str, int]:
    platform = PlatformStore(root / "platform.db")
    store = SrcStore(platform)
    artifacts = SrcArtifactStore(root / "cas")
    kb = SrcKnowledgeBase(store, artifacts, authorizer)
    kb.create_dataset(
        "task-a", "local-user", "授权研究案例", dataset_id="shared",
        reader_task_ids=["task-a", "task-b"], contributor_task_ids=["task-a"],
    )
    ref = artifacts.put(
        "task-a", b"Cookie: raw-value\x00\r\nX-Duplicate: one\r\nX-Duplicate: two",
        media_type="application/http", name="exchange.raw",
    )
    store.put("task-a", "frozen_report", {
        "title": "冻结报告 v3", "finding": "完整发现正文", "private_note": "不发布此字段",
        "status": "active",
    }, record_id="report-1")

    publication = publish_fixture(kb, ref, "shared", "entry-1", "publish-1")
    repeated = publish_fixture(kb, ref, "shared", "entry-1", "publish-1")
    assert repeated.publication_id == publication.publication_id
    assert publication.entry_snapshot["reference_only"] is True
    candidates = kb.search("task-b", "local-user", "跨身份", context={"tool_version": "http-v1"})
    assert len(candidates) == 1 and candidates[0]["reference_only"] is True
    assert "private_note" not in json.dumps(candidates, ensure_ascii=False)
    assert kb.search("task-b", "outsider", "跨身份") == []
    expect_error("policy_denied", lambda: kb.read("task-b", "outsider", "shared", publication.publication_id))

    selected = kb.read("task-b", "local-user", "shared", publication.publication_id)
    transmitted = base64.b64decode(selected["evidence_bytes"][0]["data"])
    assert transmitted == b"Cookie: raw-value\x00\r\nX-Duplicate: one\r\nX-Duplicate: two"
    assert selected["does_not_create_fact"] is True
    full_source = kb.read_source("task-a", "local-user", "frozen_report", "report-1")
    assert full_source["payload"]["private_note"] == "不发布此字段"
    assert "private_note" not in selected["source_snapshot"]

    stale = kb.read("task-b", "local-user", "shared", publication.publication_id, context={"tool_version": "http-v2"})
    assert stale["availability"]["state"] == "stale"
    assert base64.b64decode(stale["evidence_bytes"][0]["data"]) == transmitted

    # Both writers read revision 1. Exactly one v2 may commit; the loser must not
    # overwrite the winner or silently allocate a duplicate natural version.
    def compete(key: str):
        try:
            return publish_fixture(kb, ref, "shared", "entry-1", key).version
        except SrcStoreError as exc:
            return exc.code

    with ThreadPoolExecutor(max_workers=2) as pool:
        outcomes = list(pool.map(compete, ["publish-2a", "publish-2b"]))
    assert sorted(outcomes, key=str) == [2, "version_conflict"]
    history = kb.history("task-b", "local-user", "shared", "entry-1")
    assert [row["payload"]["version"] for row in history] == [1, 2]

    counter_ref = artifacts.put("task-a", b"timeout is not a dead end", name="counterexample.txt")
    counter = kb.publish(
        "task-a", "local-user", source_kind="frozen_report", source_id="report-1",
        source_revision=1, selected_source_fields=["finding"],
        entry={
            "kind": "counterexample", "title": "超时不是反证",
            "problem_pattern": "相似超时症状曾误判为已证伪",
            "business_preconditions": {},
            "effective_experiments": [],
            "established_mechanism": "上游接收状态未知，不能推断未执行",
            "decisive_evidence": ["outcome_unknown receipt"],
            "repair_boundary": "先查询原回执",
            "invalidation_conditions": ["取得确定服务端回执"],
            "applicability": {"not_a_category_ban": True},
        },
        evidence_refs=[counter_ref], idempotency_key="counter-1",
    )
    kb.invalidate(
        "task-a", "local-user", "private:task-a", counter.publication_id,
        "工具行为已经变化", expected_revision=1,
    )
    preserved = kb.read("task-a", "local-user", "private:task-a", counter.publication_id)
    assert preserved["availability"]["state"] == "stale"
    assert preserved["kind"] == "counterexample"

    # Revoking a publication keeps immutable history but makes its bytes unreadable.
    kb.revoke(
        "task-a", "local-user", "private:task-a", counter.publication_id,
        "用户撤回选定发布", expected_revision=2,
    )
    expect_error("source_revoked", lambda: kb.read(
        "task-a", "local-user", "private:task-a", counter.publication_id))
    assert len(kb.history("task-a", "local-user", "private:task-a", counter.entry_id)) == 1

    # Source revocation propagates to dependent datasets instead of being hidden
    # by the pinned byte copy.
    source = store.require("task-a", "frozen_report", "report-1")
    store.put(
        "task-a", "frozen_report", {**source.payload, "status": "revoked", "revoked_at": "2026-10-26T00:00:00+00:00"},
        record_id="report-1", expected_revision=source.revision,
    )
    expect_error("source_revoked", lambda: kb.read("task-b", "local-user", "shared", publication.publication_id))
    expect_error("source_revoked", lambda: publish_fixture(kb, ref, "shared", "entry-9", "revoked-source"))
    assert len(kb.history("task-b", "local-user", "shared", "entry-1")) == 2
    platform.close()
    reopened = PlatformStore(root / "platform.db")
    reopened_kb = SrcKnowledgeBase(SrcStore(reopened), artifacts, authorizer)
    expect_error("source_revoked", lambda: reopened_kb.read("task-b", "local-user", "shared", publication.publication_id))
    reopened.close()
    return {"publications": 3, "concurrent_writers": 2, "cross_task_denials": 2}


async def check_regression(root: Path) -> dict[str, int]:
    store = SrcStore(PlatformStore(root / "regression.db"))
    suite = default_regression_suite()
    assert suite.suite_version == "src.model-regression.v1"
    assert len({row.scenario_id for row in suite.scenarios}) == len(suite.scenarios)
    assert all(row.model_settings["seed"] == 20261026 for row in suite.scenarios)
    report = await run_regression_suite(
        suite, store=store, implementation_commit="offline-worktree",
    )
    assert report["scenario_count"] == 27
    assert report["counts"] == {"passed": 7, "failed": 0, "not_verified": 20}
    assert all(row["status"] == "not_verified" for row in report["results"] if row["category"] == "model")
    assert store.require("src-model-regression", "knowledge_regression_run", report["run_id"])

    def bad_scenario(actual_retry_count: int) -> RegressionScenario:
        return RegressionScenario(
            scenario_id="harness.non_idempotent.unknown", category="harness", polarity="negative",
            title="非幂等未知状态不重发",
            model_config={"provider": "fixed-offline", "model": "fixed-model-v1", "temperature": 0, "seed": 20261026},
            tool_contract={"name": "src-controlled-tools", "version": "v1"},
            plan_sample={"version": "plan-v1"},
            replay={"outcome": {"dispatch_state": "outcome_unknown", "dispatch_count": 1, "retry_count": actual_retry_count}},
            expected={"dispatch_state": "outcome_unknown", "dispatch_count": 1, "retry_count": 0},
        )

    failed = await run_regression_suite(
        RegressionSuite(scenarios=[bad_scenario(1)]), store=store,
    )
    assert failed["counts"]["failed"] == 1
    assert failed["results"][0]["reasons"]
    fixed = await run_regression_suite(
        RegressionSuite(scenarios=[bad_scenario(0)]), store=store,
        scenario_ids={"harness.non_idempotent.unknown"},
    )
    assert fixed["counts"]["passed"] == 1
    original_failure = store.require(
        "src-model-regression", "knowledge_regression_result",
        f"{failed['run_id']}:harness.non_idempotent.unknown",
    )
    assert original_failure.payload["status"] == "failed"
    assert original_failure.payload["observed"]["retry_count"] == 1
    return report["counts"] | {"history_preserved": 1}


BROWSER_FIXTURE_SCRIPT = r'''
import json, sys
from pathlib import Path
from playwright.sync_api import sync_playwright
root, url = Path(sys.argv[1]), sys.argv[2]
result = {"tool": "playwright", "actions": ["goto", "fill", "click", "read_signal"]}
with sync_playwright() as p:
    browser = p.chromium.launch(headless=True)
    page = browser.new_page()
    console = []
    page.on("console", lambda message: console.append({"type": message.type, "text": message.text}))
    page.goto(url + "/xss", wait_until="networkidle")
    payload = "<img src=x onerror=\"window.__srcEvidence='page-path-executed';document.getElementById('signal').textContent=window.__srcEvidence\">"
    page.locator("#payload").fill(payload)
    page.locator("#submit").click()
    page.wait_for_function("window.__srcEvidence === 'page-path-executed'")
    result.update({
        "signal": page.locator("#signal").text_content(),
        "origin": page.evaluate("location.origin"),
        "current_url": page.url,
        "console": console,
        "dom": page.content(),
        "evaluate_injected": False,
        "readback": page.evaluate("window.__srcEvidence"),
    })
    page.screenshot(path=str(root / "browser-normal-path.png"), full_page=True)
    browser.close()
print(json.dumps(result, ensure_ascii=False))
'''


def build_live_fixtures(root: Path) -> tuple[dict[str, dict], dict]:
    import hashlib
    import threading
    import urllib.error
    import urllib.request
    from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

    root.mkdir(mode=0o700, parents=True, exist_ok=True)
    artifact_root = root / "artifacts"
    artifact_root.mkdir(mode=0o700, exist_ok=True)
    callbacks = []
    health = []

    def raw_request(method, path, headers, body=b""):
        lines = [f"{method} {path} HTTP/1.1", *[f"{key}: {value}" for key, value in headers], "", ""]
        return "\r\n".join(lines).encode() + body

    def raw_response(status, headers, body):
        return f"HTTP/1.1 {status} OK\r\n".encode() + b"\r\n".join(
            f"{key}: {value}".encode() for key, value in headers
        ) + b"\r\n\r\n" + body

    def artifact(name, data, media_type="text/plain"):
        data = data if isinstance(data, bytes) else str(data).encode()
        digest = hashlib.sha256(data).hexdigest()
        path = artifact_root / f"{digest[:16]}-{name}"
        path.write_bytes(data)
        result = {
            "artifact_id": digest[:16], "sha256": digest, "size": len(data),
            "media_type": media_type, "path": str(path),
        }
        try:
            result["content"] = data.decode("utf-8")
        except UnicodeDecodeError:
            result["content_base64"] = base64.b64encode(data).decode("ascii")
        return result

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            return

        def do_GET(self):
            headers = dict(self.headers.items())
            request = raw_request("GET", self.path, list(headers.items()))
            if self.path.startswith("/oast/callback/"):
                callbacks.append({"request": request.decode(), "source_address": self.client_address[0]})
                body = b'{"received":true}'
                self._reply(200, body)
                return
            if self.path == "/oast/health":
                health.append(request.decode())
                self._reply(200, b'{"healthy":true}')
                return
            if self.path == "/accounts/A/orders/42":
                body = b'{"object_id":42,"owner":"A","tenant":"test","value":"independent-object-proof"}'
                self._reply(200, body)
                return
            if self.path == "/xss":
                body = b'''<!doctype html><html><body><form id="form"><input id="payload" name="payload"><button id="submit" type="submit">Send</button></form><div id="container"></div><div id="signal"></div><script>document.getElementById('form').addEventListener('submit',function(event){event.preventDefault();document.getElementById('container').innerHTML=document.getElementById('payload').value;});</script></body></html>'''
                self._reply(200, body, "text/html")
                return
            if self.path == "/oast/trigger":
                token = "oast-5f7c9d2e"
                callback = f"http://127.0.0.1:{self.server.server_port}/oast/callback/{token}?proof=blind-ssrf"
                with urllib.request.urlopen(callback, timeout=5) as response:
                    response.read()
                self._reply(200, json.dumps({"triggered": True, "token": token}).encode(), "application/json")
                return
            if self.path == "/oast/poll":
                self._reply(200, json.dumps({"events": [{"token": "oast-5f7c9d2e", "path": "/oast/callback/oast-5f7c9d2e"}]}).encode(), "application/json")
                return
            if self.path == "/retest/original":
                self._reply(200, b'{"vulnerable":true,"target_version":"build-1","impact":"protected-object-read","object":{"owner":"A","tenant":"test","content":"independent-object-proof"},"access_rule":"owner-only"}', "application/json")
                return
            if self.path == "/retest/current":
                self._reply(200, b'{"vulnerable":true,"target_version":"build-2","impact":"protected-object-read","object":{"owner":"A","tenant":"test","content":"independent-object-proof"},"access_rule":"owner-only"}', "application/json", [("X-Target-Version", "build-2")])
                return
            if self.path == "/retest/fixed":
                self._reply(403, b'{"vulnerable":false,"target_version":"build-2","access":"owner-only-enforced","error":"forbidden","object":{"owner":"A","tenant":"test"}}', "application/json", [("X-Target-Version", "build-2")])
                return
            if self.path == "/retest/owner-control":
                self._reply(200, b'{"owner_control":"owner-A-allowed","object":{"owner":"A","tenant":"test","content":"independent-object-proof"}}', "application/json")
                return
            if self.path == "/normal":
                self._reply(200, b'{"normal_business":"works"}', "application/json")
                return
            if self.path == "/report/public":
                self._reply(200, b'{"object_id":42,"classification":"public-directory","tenant":"test"}', "application/json")
                return
            if self.path == "/report/candidate":
                self._reply(200, b'{"object_id":42,"classification":"unclear","tenant":"test"}', "application/json")
                return
            self._reply(404, b'{"error":"not-found"}', "application/json")

        def _reply(self, status, body, media_type="application/json", extra=None):
            self.send_response(status)
            self.send_header("Content-Type", media_type)
            self.send_header("Content-Length", str(len(body)))
            for key, value in extra or []:
                self.send_header(key, value)
            self.end_headers()
            self.wfile.write(body)

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    base = f"http://127.0.0.1:{server.server_port}"

    def request(path, identity="anonymous"):
        headers = {"Host": f"127.0.0.1:{server.server_port}", "X-Test-Identity": identity, "Connection": "close"}
        req = urllib.request.Request(base + path, headers=headers)
        try:
            with urllib.request.urlopen(req, timeout=10) as response:
                status = response.status
                response_headers = list(response.headers.items())
                body = response.read()
        except urllib.error.HTTPError as exc:
            status = exc.code
            response_headers = list(exc.headers.items())
            body = exc.read()
        return {
            "request": raw_request("GET", path, list(headers.items())).decode(),
            "response": raw_response(status, response_headers, body).decode(),
            "status": status,
        }

    try:
        baseline = request("/accounts/A/orders/42", "account-A")
        experiment = request("/accounts/A/orders/42", "account-B")
        owner = json.loads(experiment["response"].split("\r\n\r\n", 1)[1])
        assert owner["owner"] == "A" and experiment["status"] == 200

        browser_result = None
        browser_error = None
        try:
            completed = __import__("subprocess").run(
                ["pyenv", "exec", "python", "-c", BROWSER_FIXTURE_SCRIPT, str(root), base],
                capture_output=True, text=True, timeout=90, check=False,
            )
            if completed.returncode:
                browser_error = completed.stderr
            else:
                browser_result = json.loads(completed.stdout)
        except Exception as exc:
            browser_error = f"{type(exc).__name__}: {exc}"
        if browser_result is None:
            raise RuntimeError(f"real browser fixture failed: {browser_error}")
        assert browser_result["signal"] == "page-path-executed"
        assert browser_result["origin"] == base

        trigger = request("/oast/trigger", "research-agent")
        poll = request("/oast/poll", "research-agent")
        health_check = request("/oast/health", "health-probe")
        assert len(callbacks) == 1 and "oast-5f7c9d2e" in callbacks[0]["request"]
        assert len(health) == 1

        original = request("/retest/original", "test-account")
        current = request("/retest/current", "test-account")
        fixed = request("/retest/fixed", "test-account")
        owner_control = request("/retest/owner-control", "account-A")
        normal = request("/normal", "test-account")
        assert "build-2" in current["response"] and "independent-object-proof" in current["response"]
        assert fixed["status"] == 403 and "independent-object-proof" not in fixed["response"]
        assert "works" in normal["response"] and "owner-A-allowed" in owner_control["response"]

        public = request("/report/public", "reviewer")
        candidate = request("/report/candidate", "reviewer")

        independent_source = (
            "def load_order(order_id, principal):\n"
            "    return database.read(order_id)  # missing ownership check; repaired in OrdersService\n\n"
            "def load_export(order_id, principal):\n"
            "    return exporter.render(order_id)  # independent authorization bug; repaired in ExportService\n\n"
            "# Separate implementations and separate repair commits, not one shared load_order.\n"
        )
        dedup_source = (
            "def load_order(order_id, principal):\n"
            "    return database.read(order_id)  # no owner/tenant check in either route\n\n"
            "def get_order(order_id, principal):\n"
            "    return load_order(order_id, principal)\n\n"
            "def get_order_export(order_id, principal):\n"
            "    return load_order(order_id, principal)\n\n"
            "# Both routes reach the same missing check in load_order; the repair point is shared.\n"
        )
        poc_path = root / "poc.py"
        poc_path.write_text("print('independent-object-proof')\n", encoding="utf-8")
        clean_room = root / "clean-room"
        clean_room.mkdir(exist_ok=True)
        clean_poc = clean_room / "poc.py"
        __import__("shutil").copyfile(poc_path, clean_poc)
        poc_run = __import__("subprocess").run(
            [sys.executable, "poc.py"], cwd=clean_room.resolve(),
            capture_output=True, text=True, timeout=15, check=False,
        )
        assert poc_run.returncode == 0 and "independent-object-proof" in poc_run.stdout
        version_store = SrcStore(PlatformStore(root / "platform.db"))
        version_store.put("task-a", "frozen_report", {
            "report_seq": 1, "finding_id": "finding-1", "platform_status": "accepted",
        }, record_id="report-v1")
        version_store.put("task-a", "frozen_report", {
            "report_seq": 2, "finding_id": "finding-1", "supersedes": "report-v1",
        }, record_id="report-v2")
        old_status = version_store.require("task-a", "frozen_report", "report-v1").payload["platform_status"]
        new_status = version_store.require("task-a", "frozen_report", "report-v2").payload.get("platform_status")
        assert old_status == "accepted" and new_status is None
        artifacts = {
            "identity-baseline.http": artifact("identity-baseline.http", baseline["request"] + baseline["response"], "application/http"),
            "identity-experiment.http": artifact("identity-experiment.http", experiment["request"] + experiment["response"], "application/http"),
            "browser-page.json": artifact("browser-page.json", json.dumps(browser_result, ensure_ascii=False), "application/json"),
            "browser-normal-path.png": artifact("browser-normal-path.png", (root / "browser-normal-path.png").read_bytes(), "image/png"),
            "oast-trigger.http": artifact("oast-trigger.http", trigger["request"] + trigger["response"], "application/http"),
            "oast-callback.http": artifact("oast-callback.http", callbacks[0]["request"], "application/http"),
            "oast-poll.http": artifact("oast-poll.http", poll["request"] + poll["response"], "application/http"),
            "retest-original.http": artifact("retest-original.http", original["request"] + original["response"], "application/http"),
            "retest-current.http": artifact("retest-current.http", current["request"] + current["response"], "application/http"),
            "retest-fixed.http": artifact("retest-fixed.http", fixed["request"] + fixed["response"], "application/http"),
            "retest-normal.http": artifact("retest-normal.http", normal["request"] + normal["response"], "application/http"),
            "retest-owner-control.http": artifact("retest-owner-control.http", owner_control["request"] + owner_control["response"], "application/http"),
            "dedup-source.py": artifact("dedup-source.py", dedup_source, "text/x-python"),
            "independent-source.py": artifact("independent-source.py", independent_source, "text/x-python"),
            "poc.py": artifact("poc.py", poc_path.read_bytes(), "text/x-python"),
            "platform-old.json": artifact("platform-old.json", json.dumps(version_store.require("task-a", "frozen_report", "report-v1").payload), "application/json"),
            "platform-new.json": artifact("platform-new.json", json.dumps(version_store.require("task-a", "frozen_report", "report-v2").payload), "application/json"),
            "report-public.http": artifact("report-public.http", public["request"] + public["response"], "application/http"),
            "report-candidate.http": artifact("report-candidate.http", candidate["request"] + candidate["response"], "application/http"),
        }
        common = {"fixture_version": "live-fixtures.v1", "authorized_local_target": base}
        fixtures = {
            "model.cross_identity.read": {
                **common,
                "artifacts": [artifacts["identity-baseline.http"], artifacts["identity-experiment.http"]],
                "tool_results": {
                    "baseline": baseline, "experiment": experiment,
                    "state_readback": {"expected_owner": "A", "actual_owner": owner["owner"], "tenant": owner["tenant"]},
                    "identity_mapping": {"baseline": "account-A", "experiment": "account-B"},
                },
            },
            "model.browser.normal_path": {
                **common,
                "artifacts": [artifacts["browser-page.json"], artifacts["browser-normal-path.png"]],
                "tool_results": browser_result,
            },
            "model.oast.linked": {
                **common,
                "artifacts": [artifacts["oast-trigger.http"], artifacts["oast-callback.http"], artifacts["oast-poll.http"]],
                "tool_results": {
                    "trigger": trigger, "poll": poll, "callback": callbacks[0],
                    "token_match": {"expected": "oast-5f7c9d2e", "actual": "oast-5f7c9d2e"},
                    "health_check": health_check,
                },
            },
            "model.dedup.same_root": {
                **common,
                "artifacts": [artifacts["dedup-source.py"]],
                "tool_results": {
                    "source_review": dedup_source,
                    "root_cause": "load_order 缺少 owner/tenant 检查",
                    "shared_repair_point": "load_order",
                    "affected_routes": ["get_order", "get_order_export"],
                },
            },
            "model.dedup.independent_fix": {
                **common,
                "artifacts": [artifacts["independent-source.py"]],
                "tool_results": {
                    "source_review": independent_source,
                    "root_causes": ["OrdersService ownership check", "ExportService authorization check"],
                    "repair_points": ["OrdersService", "ExportService"],
                    "shared_implementation": False,
                },
            },
            "model.retest.fixed": {
                **common,
                "artifacts": [artifacts["retest-original.http"], artifacts["retest-fixed.http"], artifacts["retest-normal.http"], artifacts["retest-owner-control.http"]],
                "tool_results": {
                    "original_poc": original, "repair_retest": fixed, "normal_control": normal,
                    "owner_control": owner_control, "target_version_readback": "build-2",
                    "identity": "test-account",
                    "negative_result": "原非所有者请求现为 403，响应不包含受保护对象内容",
                },
            },
            "model.retest.vulnerable": {
                **common,
                "artifacts": [artifacts["retest-original.http"], artifacts["retest-current.http"], artifacts["retest-normal.http"], artifacts["retest-owner-control.http"]],
                "tool_results": {
                    "original_poc": original, "repair_retest": current, "normal_control": normal,
                    "owner_control": owner_control,
                    "target_version_readback": "build-2", "identity": "test-account",
                    "permission_boundary": {"rule": "owner-only", "object_owner": "A", "requester": "test-account"},
                },
            },
            "report.evidence.counterevidence": {
                **common,
                "artifacts": [artifacts["report-public.http"], artifacts["report-candidate.http"]],
                "tool_results": {
                    "candidate": candidate, "alternative_explanation": public,
                    "evidence_audit": {
                        "all_hashes_valid": all(
                            hashlib.sha256(Path(row["path"]).read_bytes()).hexdigest() == row["sha256"]
                            for row in artifacts.values()
                        ),
                        "complete_bytes_readable": True,
                    },
                    "remaining_gap": "先读取 object_id=42 的 owner、tenant、visibility 和预期访问规则，确认 public-directory 是否适用；再用已验证的其他 tenant 账号重放 /report/candidate 并读回对象 tenant，分别排除公开目录与同 tenant 预期访问。",
                },
            },
            "report.poc.runnable": {
                **common,
                "artifacts": [artifacts["poc.py"]],
                "tool_results": {
                    "command": [sys.executable, "poc.py"],
                    "cwd": str(clean_room), "exit_code": poc_run.returncode,
                    "stdout": poc_run.stdout, "stderr": poc_run.stderr,
                    "success_assertion": "stdout contains independent-object-proof",
                    "dependencies": ["Python standard library only"],
                    "private_workspace_required": False,
                    "manifest_hashes_valid": all(
                        hashlib.sha256(Path(row["path"]).read_bytes()).hexdigest() == row["sha256"]
                        for row in artifacts.values()
                    ),
                    "cleanup": "remove only the generated clean-room and fixture files",
                },
            },
            "report.platform.version_isolation": {
                **common,
                "artifacts": [artifacts["platform-old.json"], artifacts["platform-new.json"]],
                "tool_results": {
                    "old_report": version_store.require("task-a", "frozen_report", "report-v1").model_dump(mode="json"),
                    "new_report": version_store.require("task-a", "frozen_report", "report-v2").model_dump(mode="json"),
                    "old_report_status_preserved": old_status == "accepted",
                    "new_report_status_absent": new_status is None,
                },
            },
        }
        manifest = {
            "fixture_version": "live-fixtures.v1", "base_url": base,
            "artifacts": list(artifacts.values()),
            "browser": {"engine": "playwright-chromium", "normal_path_actions": ["goto", "fill", "click", "read_signal"]},
        }
        (root / "fixture-manifest.json").write_text(json.dumps(manifest, ensure_ascii=False, indent=2), encoding="utf-8")
        return fixtures, manifest
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


def suite_with_live_fixtures(root: Path):
    fixtures, manifest = build_live_fixtures(root)
    suite = default_regression_suite()
    scenarios = []
    for scenario in suite.scenarios:
        if scenario.scenario_id in fixtures:
            authorized = {
                **scenario.plan_sample,
                "authorized_targets": [manifest["base_url"]],
                "authorization_source": "explicit-user-fixture-authorization",
            }
            scenarios.append(scenario.model_copy(update={
                "replay": fixtures[scenario.scenario_id], "plan_sample": authorized,
            }))
        else:
            scenarios.append(scenario)
    return suite.model_copy(update={
        "suite_version": "src.model-regression.live-fixtures.v1",
        "scenarios": scenarios,
    }), manifest


def real_output_path(value: str, kind: str) -> Path:
    state_root = (Path.cwd() / "state" / "src-acceptance").resolve()
    if value:
        path = Path(value).expanduser().resolve()
        if not path.is_relative_to(state_root):
            raise ValueError("真实回归结果必须保存在 Git 忽略的 state/src-acceptance/ 内")
    else:
        stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
        path = state_root / kind / f"{stamp}.json"
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    return path


def save_real_result(path: Path, report: dict) -> None:
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
    os.chmod(temporary, 0o600)
    temporary.replace(path)


def unavailable_report(executor: CodexRuntimeExecutor, error: Exception) -> dict:
    detail = f"{type(error).__name__}: {error}"
    results = [{
        "scenario_id": row.scenario_id, "category": row.category,
        "status": "not_verified", "expected": row.expected, "observed": {},
        "reasons": [detail], "evaluation_scope": "real_model_required",
    } for row in default_regression_suite().scenarios if row.category == "model"]
    return {
        "schema": "src.model-regression.result.v1",
        "suite_version": "src.model-regression.v1",
        "executor": executor.public_config(), "scenario_count": len(results),
        "counts": {"passed": 0, "failed": 0, "not_verified": len(results)},
        "results": results,
        "limitations": ["真实 Runtime 条件不足；逐项保留缺口，未使用离线结果代替。"],
    }


async def run_real_models(args: argparse.Namespace) -> int:
    output = real_output_path(args.output, "model-regression")
    executor = CodexRuntimeExecutor(model=args.model, effort=args.effort, turn_timeout_s=args.turn_timeout)
    try:
        capabilities = await executor.probe()
    except Exception as exc:
        report = unavailable_report(executor, exc)
        save_real_result(output, report)
        print(f"真实 Runtime 不可用：{type(exc).__name__}: {exc}")
        print(f"20 项均为 not_verified；结果已保存：{output}")
        return 2
    store = SrcStore(PlatformStore(output.parent / "regression.db"))
    suite, fixture_manifest = await asyncio.to_thread(suite_with_live_fixtures, output.parent / "fixtures")
    report = await run_regression_suite(
        suite, store=store, executor=executor,
        implementation_commit="real-codex-runtime",
    )
    report["capabilities"] = capabilities
    report["fixture_manifest"] = fixture_manifest
    save_real_result(output, report)
    print(json.dumps({"executor": executor.public_config(), "counts": report["counts"], "output": str(output)}, ensure_ascii=False))
    for result in report["results"]:
        if result["status"] != "passed":
            print(f"{result['status']}: {result['scenario_id']} — {'；'.join(result['reasons'])}")
    if report["counts"]["not_verified"]:
        return 2
    return 1 if report["counts"]["failed"] else 0


async def run_image_review(args: argparse.Namespace) -> int:
    from PIL import Image, ImageDraw

    output = real_output_path(args.output, "image-review")
    executor = CodexRuntimeExecutor(model=args.model, effort=args.effort, turn_timeout_s=args.turn_timeout)
    try:
        capabilities = await executor.probe()
    except Exception as exc:
        report = unavailable_report(executor, exc)
        save_real_result(output, report)
        print(f"真实图片 Runtime 不可用：{type(exc).__name__}: {exc}")
        return 2
    if not capabilities.get("image_input"):
        report = unavailable_report(executor, RuntimeError("live Runtime does not support native image input"))
        save_real_result(output, report)
        return 2
    marker = "SRC-REVIEW-7429"
    root = output.parent
    image_path = root / "review-source.png"
    image = Image.new("RGB", (720, 240), "#10243e")
    draw = ImageDraw.Draw(image)
    draw.rectangle((20, 20, 150, 220), fill="#f2c14e")
    draw.text((190, 100), marker, fill="#ffffff")
    image.save(image_path, format="PNG")
    raw = image_path.read_bytes()
    import hashlib
    digest = hashlib.sha256(raw).hexdigest()
    report = await executor.review_image(str(image_path), digest, marker)
    report["capabilities"] = capabilities
    report["expected_marker"] = marker
    save_real_result(output, report)
    print(json.dumps({
        "marker_matched": report.get("marker_matched"), "observed_marker": report.get("marker"),
        "colors": report.get("colors"), "artifact_sha256": digest,
        "output": str(output),
    }, ensure_ascii=False))
    return 0 if report.get("marker_matched") else 1


async def main() -> None:
    with tempfile.TemporaryDirectory(prefix="muteki-src-knowledge-") as temporary:
        root = Path(temporary)
        knowledge = await asyncio.to_thread(check_knowledge, root)
        regression = await check_regression(root)
    print("PASS: selected publication, complete bytes, ACL, revoke/stale, idempotency, concurrency, restart")
    print("PASS: 7 offline harness/report fixture scenarios; 20 real-model scenarios remain not_verified")
    print("PASS: failed regression history is preserved after affected scenario is fixed")
    print(json.dumps({"knowledge": knowledge, "regression": regression}, ensure_ascii=False, sort_keys=True))
    print("LIMITATION: offline fixtures do not measure real model quality or vulnerability discovery rate")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--real-codex", action="store_true", help="run 20 real model scenarios")
    parser.add_argument("--image-review", action="store_true", help="verify native Artifact image review")
    parser.add_argument("--model", default="gpt-6.1-sol")
    parser.add_argument("--effort", default="low")
    parser.add_argument("--turn-timeout", type=int, default=120)
    parser.add_argument("--output", default="")
    args = parser.parse_args()
    if args.image_review:
        raise SystemExit(asyncio.run(run_image_review(args)))
    if args.real_codex:
        raise SystemExit(asyncio.run(run_real_models(args)))
    asyncio.run(main())
