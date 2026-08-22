"""Unit tests for the conversation-first deck UX (sessions #7 + all-track eval):

- titler: prompt-head fallback + answer cleaning (no network)
- RunMetaStore: pin/archive/rename persistence + archive-unpins + atomic file
- RunManager: rehydrate from JSONL, status() state machine, list_runs started-only
  + archived-hidden, pin/rename/delete mutations
- web driver: attachments threaded into the Challenge + offline implies no-KB
- CliSolver: attachment staging copies files into the worker cwd + lists them in
  the prompt

All offline/synchronous — no model, no Docker, no CLI subprocess.
"""

from __future__ import annotations

import asyncio
import json
import shutil
import subprocess
import textwrap
from pathlib import Path

import pytest

from apps.web.run_manager import RunManager
from apps.web.run_meta import RunMetaStore
from apps.web.titler import _clean, fallback_title, generate_title
from muteki.core.event_bus import EventBus
from muteki.core.events import Event, EventType
from muteki.models.solve_graph import Challenge


ROOT = Path(__file__).resolve().parents[1]
UI_ROOT = ROOT / "apps" / "web" / "ui"


_TS_LOAD_HELPER = """
const fs = require("fs");
const path = require("path");
const ts = require("typescript");
const vm = require("vm");
function compile(rel) {
  return ts.transpileModule(fs.readFileSync(rel, "utf8"), {
    compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 }
  }).outputText;
}
function loadTs(rel) {
  const code = compile(rel);
  const dir = path.dirname(rel);
  const sandbox = {
    module: { exports: {} },
    exports: {},
    require: (name) => {
      if (name.startsWith("./") || name.startsWith("../")) {
        const bare = path.normalize(path.join(dir, name));
        const file = [bare, bare + ".ts", bare + ".tsx"].find((candidate) => fs.existsSync(candidate));
        if (!file) throw new Error("missing local module " + name);
        return loadTs(file);
      }
      return require(name);
    },
  };
  sandbox.exports = sandbox.module.exports;
  vm.runInNewContext(code, sandbox, { filename: rel });
  return sandbox.module.exports;
}
"""


def _run_ui_node(script: str) -> None:
    """Run a Node snippet that transpiles a UI .ts helper via the bundled
    `typescript` package and asserts on its behavior.

    These tests exercise REAL frontend logic, so they need `node` on PATH and the
    UI's node_modules (for `require("typescript")`). Neither is installed by
    `./init.sh` (it's a Python bootstrap; the UI deps land on the first
    `./run.sh web` / `npm install`), so on a fresh checkout we SKIP rather than
    hard-fail — the suite stays green on a bare host, and CI (which installs the UI
    deps) still runs them for real. A genuine assertion failure inside the script
    still raises (check=True)."""
    if shutil.which("node") is None:
        pytest.skip("node not on PATH — UI reducer tests need Node (npm install in apps/web/ui)")
    if not (UI_ROOT / "node_modules" / "typescript").exists():
        pytest.skip("apps/web/ui/node_modules/typescript missing — run `npm install` in apps/web/ui")
    subprocess.run(["node", "-e", script], cwd=UI_ROOT, check=True,
                   capture_output=True, text=True)


# ---- titler -----------------------------------------------------------------

def test_fallback_title_english_word_truncation():
    out = fallback_title("Solve this SQL injection challenge on the login form please now")
    assert out == "Solve this SQL injection challenge on"  # 6 words
    assert fallback_title("   ") == ""


def test_fallback_title_cjk_char_truncation():
    long_cjk = "帮我解一道关于登录表单的SQL注入题目" * 4  # no spaces, >48 chars
    out = fallback_title(long_cjk)
    assert out == long_cjk[:48]


def test_clean_strips_quotes_and_rejects_junk():
    assert _clean('"SQL Injection Login."', "fallback here ok yes") == "SQL Injection Login"
    # empty / oversized model answer → fall back to the prompt head
    assert _clean("", "alpha beta gamma delta") == "alpha beta gamma delta"
    assert _clean("x" * 200, "alpha beta gamma delta") == "alpha beta gamma delta"
    assert _clean("I'm sorry, I cannot access that URL", "solve http://target now") == fallback_title("solve http://target now")


async def test_generate_title_uses_configured_titler_model():
    seen = {}

    class FakeResp:
        content = "Configured Model Title"

    class FakeLLM:
        async def chat(self, **kwargs):
            seen.update(kwargs)
            return FakeResp()

    title = await generate_title("solve target", llm=FakeLLM(), model="titler-x")
    assert title == "Configured Model Title"
    assert seen["model"] == "titler-x"


# ---- worker lane presentation ----------------------------------------------

def test_worker_lane_header_compacts_tool_status():
    """Long live tool commands stay in latest activity; the header status stays short."""
    helper = UI_ROOT / "lib" / "workerLanePresentation.ts"
    script = textwrap.dedent(
        f"""
        const fs = require("fs");
        const ts = require("typescript");
        const vm = require("vm");
        const source = fs.readFileSync({json.dumps(str(helper))}, "utf8");
        const out = ts.transpileModule(source, {{
          compilerOptions: {{ module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 }}
        }}).outputText;
        const sandbox = {{ module: {{ exports: {{}} }}, exports: {{}} }};
        sandbox.exports = sandbox.module.exports;
        vm.runInNewContext(out, sandbox, {{ filename: "workerLanePresentation.js" }});
        const lib = sandbox.module.exports;
        function assert(cond, msg) {{ if (!cond) throw new Error(msg); }}

        const lane = {{ status: 'tool: shell: /bin/zsh -lc "python3 /Users/x/runner.py"' }};
        const status = lib.compactLaneStatusToken(lane, true);
        assert(status.kind === "i18n" && status.key === "wlane.runningTool", "tool status should be compact");

        const latest = lib.latestLaneActivity(lane.status, "", []);
        assert(latest.startsWith("shell: /bin/zsh"), "latest keeps the readable command");
        assert(!latest.startsWith("tool:"), "latest strips the transport prefix");

        const longRaw = lib.compactLaneStatusToken({{ status: "x".repeat(80) }}, true);
        assert(longRaw.kind === "i18n" && longRaw.key === "workerDock.online", "long raw status should collapse");

        const offline = lib.compactLaneStatusToken(lane, false);
        assert(offline.kind === "i18n" && offline.key === "workerDock.offline", "offline wins over tool text");
        """
    )
    _run_ui_node(script)


def test_clipboard_copy_falls_back_after_async_clipboard_rejects():
    """Flag-copy must actually write text, not just flip the UI to "copied".

    Browsers can expose navigator.clipboard but reject writeText on non-secure
    origins, denied permissions, or unfocused documents. The deck should then
    use the textarea/execCommand path and only show copied after a real success.
    """
    helper = UI_ROOT / "lib" / "clipboard.ts"
    hook = UI_ROOT / "lib" / "useCopied.ts"
    script = textwrap.dedent(
        f"""
        const fs = require("fs");
        const ts = require("typescript");
        const vm = require("vm");
        const source = fs.readFileSync({json.dumps(str(helper))}, "utf8");
        const hookSource = fs.readFileSync({json.dumps(str(hook))}, "utf8");
        const out = ts.transpileModule(source, {{
          compilerOptions: {{ module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 }}
        }}).outputText;

        function assert(cond, msg) {{ if (!cond) throw new Error(msg); }}

        let appended = null;
        let copiedViaFallback = "";
        class FakeHTMLElement {{
          focus() {{ this.focused = true; }}
        }}
        const active = new FakeHTMLElement();
        const selection = {{
          rangeCount: 1,
          restored: [],
          getRangeAt(i) {{ return {{ idx: i }}; }},
          removeAllRanges() {{ this.removed = true; }},
          addRange(r) {{ this.restored.push(r); }},
        }};
        const document = {{
          activeElement: active,
          body: {{
            appendChild(el) {{ appended = el; }},
            removeChild(el) {{
              assert(appended === el, "fallback removes the textarea it appended");
              appended = null;
            }},
          }},
          getSelection() {{ return selection; }},
          createElement(tag) {{
            assert(tag === "textarea", "fallback uses a textarea");
            const el = new FakeHTMLElement();
            el.style = {{}};
            el.setAttribute = (key, value) => {{ el[key] = value; }};
            el.select = () => {{ el.selected = true; }};
            el.setSelectionRange = (start, end) => {{ el.range = [start, end]; }};
            return el;
          }},
          execCommand(cmd) {{
            assert(cmd === "copy", "fallback invokes copy");
            copiedViaFallback = appended && appended.value;
            return true;
          }},
        }};
        const navigator = {{
          clipboard: {{
            writeText: async () => {{ throw new Error("permission denied"); }},
          }},
        }};
        const sandbox = {{
          module: {{ exports: {{}} }},
          exports: {{}},
          document,
          navigator,
          HTMLElement: FakeHTMLElement,
        }};
        sandbox.exports = sandbox.module.exports;
        vm.runInNewContext(out, sandbox, {{ filename: "clipboard.js" }});
        const lib = sandbox.module.exports;

        (async () => {{
          const ok = await lib.copyToClipboard("flag{{real}}");
          assert(ok === true, "fallback copy should report success");
          assert(copiedViaFallback === "flag{{real}}", "fallback copies the actual flag text");
          assert(appended === null, "fallback textarea is cleaned up");
          assert(active.focused === true, "fallback restores focus");
          assert(selection.removed === true && selection.restored.length === 1, "selection is restored");

          let asyncText = "";
          sandbox.navigator.clipboard.writeText = async (text) => {{ asyncText = text; }};
          sandbox.document.execCommand = () => {{ throw new Error("should not use fallback after async success"); }};
          const asyncOk = await lib.copyToClipboard("flag{{api}}");
          assert(asyncOk === true && asyncText === "flag{{api}}", "async clipboard success writes the flag");

          sandbox.navigator.clipboard.writeText = async () => {{ throw new Error("denied"); }};
          sandbox.document.body = null;
          const failOk = await lib.copyToClipboard("flag{{lost}}");
          assert(failOk === false, "copy reports false when both clipboard paths fail");

          assert(hookSource.includes("copyToClipboard(text).then((ok)"), "hook waits for copy result");
          assert(hookSource.includes("if (!ok || !mounted.current) return;"), "hook gates copied state on success");
          assert(!hookSource.includes("navigator.clipboard"), "hook no longer silently calls clipboard directly");
        }})().catch((err) => {{
          console.error(err);
          process.exit(1);
        }});
        """
    )
    _run_ui_node(script)


def test_worker_filter_chips_are_compact_scroll_rail():
    script = _TS_LOAD_HELPER + textwrap.dedent(
        """
        const lib = loadTs("lib/workers.ts");
        function assert(cond, msg) { if (!cond) throw new Error(msg); }
        assert(lib.workerShortLabel("cli-cursor") === "cursor", "base cursor label");
        assert(lib.workerShortLabel("cli-cursor-2") === "cursor-2", "numbered cursor label");
        assert(lib.workerShortLabel("cli-claude-3") === "claude-3", "numbered claude label");
        assert(lib.workerShortLabel("cli-codex-10") === "codex-10", "two-digit codex label");
        assert(lib.workerShortLabel("reason") === "reason", "reason role label");
        """
    )
    _run_ui_node(script)

    page = (UI_ROOT / "app" / "page.tsx").read_text()
    lanes = (UI_ROOT / "components" / "WorkerLanes.tsx").read_text()
    graph = (UI_ROOT / "components" / "GraphView.tsx").read_text()
    blackboard = (UI_ROOT / "components" / "Blackboard.tsx").read_text()
    css = (UI_ROOT / "app" / "globals.css").read_text()
    i18n = (UI_ROOT / "lib" / "i18n.tsx").read_text()
    chipbar = (UI_ROOT / "components" / "ChipFilterBar.tsx").read_text()
    assert "projectActivityLedger" in page
    assert "RuntimeActivityStream" in page
    assert "ChipFilterBar" in graph
    assert "ChipFilterBar" in blackboard
    assert "ChipFilterBar" not in lanes
    assert "useState(false)" in chipbar
    assert "aria-expanded={open}" in chipbar
    assert "className=\"chip-filter-toggle\"" in chipbar
    assert "className=\"chip-filter-panel\"" in chipbar
    assert "className=\"chip-filter-strip\"" in chipbar
    assert "wlane-roster" in lanes
    assert "wlane-anomaly" in lanes
    assert "filterOpen" not in lanes
    assert "wlane-filter-chiprow" not in lanes
    assert "--chip-filter-rows: 3" in css
    assert "max-height: calc((24px * var(--chip-filter-rows))" in css
    assert "overflow-y: auto" in css
    assert ".chip-filterbar" in css
    assert ".chip-filter-toggle" in css
    assert ".chip-filter-panel" in css
    assert ".chip-filter-strip" in css
    shared_filter_css = css[css.index(".chip-filter-strip"):css.index(".chip-filter-clear")]
    assert "flex-wrap: wrap" in shared_filter_css
    assert "-webkit-mask-image: none" in shared_filter_css
    assert ".wlane-filter-chiprow" not in css
    assert "wlane.anomaly" in i18n
    assert "wlane.groupLive" in i18n and "wlane.groupIssues" in i18n and "wlane.groupDone" in i18n


def test_worker_identity_uses_transport_not_name_substrings():
    script = _TS_LOAD_HELPER + textwrap.dedent(
        """
        const lib = loadTs("lib/workers.ts");
        function assert(cond, msg) { if (!cond) throw new Error(msg); }
        assert(lib.workerEngine("api-prod", "codex_cli") === "Codex", "codex transport label");
        assert(lib.workerEngine("not-a-claude-name", "claude_code") === "Claude Code", "claude transport label");
        assert(lib.workerEngine("plain-id", "cursor_agent") === "Cursor", "cursor transport label");
        assert(lib.resumeCommand("codex_cli", "s1") === "codex exec resume s1", "codex resume from transport");
        assert(lib.resumeCommand("cursor_agent", "s2") === "cursor-agent --resume s2", "cursor resume from transport");
        """
    )
    _run_ui_node(script)


def test_run_inspector_renders_runtime_degraded_badge():
    src = (UI_ROOT / "components" / "RunInspector.tsx").read_text()
    i18n = (UI_ROOT / "lib" / "i18n.tsx").read_text()
    css = (UI_ROOT / "app" / "globals.css").read_text()
    assert 'e.kind === "runtime_degraded"' in src
    assert 'insp-runtime-degraded' in src
    assert '"insp.run.runtimeDegraded"' in i18n
    assert ".insp-runtime-degraded" in css


def test_next_api_proxy_is_generic_streaming_and_long_lived():
    """Slow/streaming backend calls must not rely on Next's generic rewrite.

    The production deck is served from :3001. FastAPI can finish slow probes or
    docker pulls successfully, but the generic Next rewrite proxy can turn
    long-running requests into browser-visible 500s. The app route proxy must
    cover all HTTP /api paths, stream response bodies (SSE/BTW/download-safe),
    preserve request bodies for uploads, and leave no one-off worker-model proxy
    to maintain separately.
    """
    route = UI_ROOT / "app" / "api" / "[...path]" / "route.ts"
    old_one_off = UI_ROOT / "app" / "api" / "settings" / "worker-model" / "test" / "route.ts"
    assert route.exists()
    assert not old_one_off.exists()
    src = route.read_text()
    next_config = (UI_ROOT / "next.config.mjs").read_text()

    assert 'export const runtime = "nodejs"' in src
    assert "export const maxDuration = 900" in src
    assert 'process.env.MUTEKI_BACKEND || "http://127.0.0.1:8000"' in src
    assert '"/api/"' in src
    assert "upstreamUrl.search = requestUrl.search" in src
    assert "req.body" in src
    assert 'duplex = "half"' in src
    assert "new Response(upstream.body" in src
    assert "await fetch(" in src
    for method in ("GET", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"):
        assert f"export const {method} = proxy" in src
    for header in ("host", "connection", "content-length", "transfer-encoding"):
        assert header in src
    assert '"/api/:path*"' not in next_config


def test_events_reducer_tracks_poc_blackboard_lifecycle():
    helper = UI_ROOT / "lib" / "events.ts"
    script = textwrap.dedent(
        f"""
        const fs = require("fs");
        const ts = require("typescript");
        const vm = require("vm");
        const source = fs.readFileSync({json.dumps(str(helper))}, "utf8");
        const out = ts.transpileModule(source, {{
          compilerOptions: {{ module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 }}
        }}).outputText;
        const sandbox = {{ module: {{ exports: {{}} }}, exports: {{}} }};
        sandbox.exports = sandbox.module.exports;
        vm.runInNewContext(out, sandbox, {{ filename: "events.js" }});
        const lib = sandbox.module.exports;
        function assert(cond, msg) {{ if (!cond) throw new Error(msg); }}

        let s = lib.emptyDeck("run-poc");
        s = lib.reduce(s, {{ event_type: lib.EventType.BLACKBOARD_DELTA, run_id: "run-poc", ts: 1,
          solver_id: "cli-a", payload: {{ kind: "poc_saved", poc_id: "poc-1", name: "poc.py",
          entry_command: "python poc.py", status: "available", intent_id: "I1", artifact_id: "sha" }} }});
        s = lib.reduce(s, {{ event_type: lib.EventType.BLACKBOARD_DELTA, run_id: "run-poc", ts: 2,
          solver_id: "cli-b", payload: {{ kind: "poc_claimed", poc_id: "poc-1", worker: "cli-b" }} }});
        s = lib.reduce(s, {{ event_type: lib.EventType.BLACKBOARD_DELTA, run_id: "run-poc", ts: 3,
          solver_id: "cli-b", payload: {{ kind: "poc_concluded", poc_id: "poc-1", status: "spent", note: "dead" }} }});

        assert(s.blackboard.pocs.length === 1, "one poc row");
        assert(s.blackboard.pocs[0].status === "spent", "status updated");
        assert(s.blackboard.pocs[0].worker === "cli-b", "claim owner stored");
        assert(s.model.nodes.some((n) => n.id === "poc:poc-1" && n.type === "poc"), "poc graph node");
        assert(s.blackboard.events.some((e) => e.kind === "poc_concluded"), "timeline event");
        """
    )
    _run_ui_node(script)


def test_followup_failure_replaces_its_correlated_pending_status():
    helper = UI_ROOT / "lib" / "events.ts"
    script = textwrap.dedent(
        f"""
        const fs = require("fs");
        const ts = require("typescript");
        const vm = require("vm");
        const source = fs.readFileSync({json.dumps(str(helper))}, "utf8");
        const out = ts.transpileModule(source, {{
          compilerOptions: {{ module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 }}
        }}).outputText;
        const sandbox = {{ module: {{ exports: {{}} }}, exports: {{}} }};
        sandbox.exports = sandbox.module.exports;
        vm.runInNewContext(out, sandbox, {{ filename: "events.js" }});
        const lib = sandbox.module.exports;
        function assert(cond, msg) {{ if (!cond) throw new Error(msg); }}

        let s = lib.emptyDeck("run-followup");
        s = lib.reduce(s, {{ event_type: lib.EventType.FOLLOWUP_STARTED,
          run_id: "run-followup", ts: 1, payload: {{ followup_id: "F1",
            kind: "ask", question: "证据来源是什么？" }} }});
        s = lib.reduce(s, {{ event_type: lib.EventType.FOLLOWUP_STARTED,
          run_id: "run-followup", ts: 1.1, payload: {{ followup_id: "F1",
            kind: "ask", question: "证据来源是什么？" }} }});
        assert(s.followupPending, "follow-up is pending");
        assert(s.chat.filter((m) => m.content === "证据来源是什么？").length === 1,
          "duplicate lifecycle event does not duplicate the question");
        assert(s.chat.some((m) => m.followupId === "F1"
          && m.content === "正在回答追问…"), "pending row is correlated");

        s = lib.reduce(s, {{ event_type: lib.EventType.FOLLOWUP_FAILED,
          run_id: "run-followup", ts: 2, payload: {{ followup_id: "F1",
            kind: "ask", detail: "后续操作已取消" }} }});
        assert(!s.followupPending, "terminal failure clears pending state");
        assert(!s.chat.some((m) => m.content === "正在回答追问…"),
          "pending row is removed");
        assert(s.chat.filter((m) => m.followupId === "F1"
          && m.content.includes("后续操作已取消")).length === 1,
          "one correlated failure row remains");

        s = lib.reduce(s, {{ event_type: lib.EventType.FOLLOWUP_STARTED,
          run_id: "run-followup", ts: 3, payload: {{ followup_id: "F1",
            kind: "ask", question: "证据来源是什么？" }} }});
        assert(!s.followupPending,
          "a replayed start after terminal failure cannot restore pending state");
        assert(s.chat.filter((m) => m.followupId === "F1"
          && m.content.includes("后续操作已取消")).length === 1,
          "terminal row remains idempotent after replay");
        """
    )
    _run_ui_node(script)


def test_events_reducer_shows_preflight_before_workers_and_failure_details():
    helper = UI_ROOT / "lib" / "events.ts"
    script = textwrap.dedent(
        f"""
        const fs = require("fs");
        const ts = require("typescript");
        const vm = require("vm");
        const source = fs.readFileSync({json.dumps(str(helper))}, "utf8");
        const out = ts.transpileModule(source, {{
          compilerOptions: {{ module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 }}
        }}).outputText;
        const sandbox = {{ module: {{ exports: {{}} }}, exports: {{}} }};
        sandbox.exports = sandbox.module.exports;
        vm.runInNewContext(out, sandbox, {{ filename: "events.js" }});
        const lib = sandbox.module.exports;
        function assert(cond, msg) {{ if (!cond) throw new Error(msg); }}

        let s = lib.emptyDeck("run-preflight");
        s = lib.reduce(s, {{ event_type: lib.EventType.RUN_PREPARING,
          run_id: "run-preflight", ts: 1, payload: {{ challenge: {{
            name: "probe-me", category: "web", description: "solve target"
          }} }} }});
        assert(s.started && s.preparing && !s.finished, "preflight is visible as active");
        assert(s.chat.filter((m) => m.role === "human").length === 1, "prompt shown immediately");

        s = lib.reduce(s, {{ event_type: lib.EventType.RUN_FINISHED,
          run_id: "run-preflight", ts: 2, payload: {{ solved: false,
            reason: "preflight_failed", detail: "Worker 预检失败（1 个 Profile）",
            profile_failures: [{{ profile_id: "codex-local", engine: "codex",
              layer: "auth", detail: "Authentication required" }}]
          }} }});
        assert(!s.preparing && s.finished, "failed preflight settles the run");
        assert(s.outcomeReason === "preflight_failed", "typed preflight outcome");
        assert(s.preflightFailures.length === 1, "profile failure retained");
        assert(s.preflightFailures[0].profileId === "codex-local", "profile id retained");
        """
    )
    _run_ui_node(script)


def test_events_reducer_upserts_fact_and_dead_end_by_db_seq():
    helper = UI_ROOT / "lib" / "events.ts"
    script = textwrap.dedent(
        f"""
        const fs = require("fs");
        const ts = require("typescript");
        const vm = require("vm");
        const source = fs.readFileSync({json.dumps(str(helper))}, "utf8");
        const out = ts.transpileModule(source, {{
          compilerOptions: {{ module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 }}
        }}).outputText;
        const sandbox = {{ module: {{ exports: {{}} }}, exports: {{}} }};
        sandbox.exports = sandbox.module.exports;
        vm.runInNewContext(out, sandbox, {{ filename: "events.js" }});
        const lib = sandbox.module.exports;
        function assert(cond, msg) {{ if (!cond) throw new Error(msg); }}

        let s = lib.emptyDeck("run-dedupe");
        const fact = {{ event_type: lib.EventType.BLACKBOARD_DELTA, run_id: "run-dedupe", ts: 1,
          solver_id: "cli-a", payload: {{ kind: "fact_added", fact_seq: 42, fact: "admin password",
          verified: true, confidence: 1, verifier: "claude" }} }};
        const dead = {{ event_type: lib.EventType.BLACKBOARD_DELTA, run_id: "run-dedupe", ts: 2,
          solver_id: "cli-a", payload: {{ kind: "dead_end", dead_end_seq: 43, reason: "ftp disabled" }} }};
        s = lib.reduce(s, fact);
        s = lib.reduce(s, fact);
        s = lib.reduce(s, dead);
        s = lib.reduce(s, dead);

        assert(s.blackboard.facts.length === 1, "fact_seq upsert");
        assert(s.blackboard.facts[0].factSeq === 42, "fact seq preserved");
        assert(s.blackboard.deadEnds.length === 1, "dead_end_seq upsert");
        assert(s.blackboard.deadEnds[0].deadEndSeq === 43, "dead end seq preserved");
        """
    )
    _run_ui_node(script)


def test_events_reducer_uses_intent_id_product_edge_and_branch_resolve():
    helper = UI_ROOT / "lib" / "events.ts"
    script = textwrap.dedent(
        f"""
        const fs = require("fs");
        const ts = require("typescript");
        const vm = require("vm");
        const source = fs.readFileSync({json.dumps(str(helper))}, "utf8");
        const out = ts.transpileModule(source, {{
          compilerOptions: {{ module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 }}
        }}).outputText;
        const sandbox = {{ module: {{ exports: {{}} }}, exports: {{}} }};
        sandbox.exports = sandbox.module.exports;
        vm.runInNewContext(out, sandbox, {{ filename: "events.js" }});
        const lib = sandbox.module.exports;
        function assert(cond, msg) {{ if (!cond) throw new Error(msg); }}

        let s = lib.emptyDeck("run-products");
        s = lib.reduce(s, {{ event_type: lib.EventType.BLACKBOARD_DELTA, run_id: "run-products", ts: 1,
          solver_id: "reason", payload: {{ kind: "intent_proposed", intent_id: "I1", goal: "use admin cookie" }} }});
        s = lib.reduce(s, {{ event_type: lib.EventType.BLACKBOARD_DELTA, run_id: "run-products", ts: 2,
          solver_id: "cli-a", payload: {{ kind: "fact_added", fact_seq: 7, fact: "admin cookie works",
          verified: true, intent_id: "I1" }} }});
        // G0: a SECOND fact produced by the same intent — the canvas must draw both
        // produces-edges, not just the single conclude-time toFactSeq.
        s = lib.reduce(s, {{ event_type: lib.EventType.BLACKBOARD_DELTA, run_id: "run-products", ts: 3,
          solver_id: "cli-a", payload: {{ kind: "fact_added", fact_seq: 8, fact: "session id leaks in header",
          verified: true, intent_id: "I1" }} }});
        // a re-emit of fact 7 WITHOUT intent_id (bridge/coordinator double-emit) must
        // not clobber the known producer.
        s = lib.reduce(s, {{ event_type: lib.EventType.BLACKBOARD_DELTA, run_id: "run-products", ts: 4,
          solver_id: "cli-a", payload: {{ kind: "fact_added", fact_seq: 7, fact: "admin cookie works",
          verified: true }} }});
        s = lib.reduce(s, {{ event_type: lib.EventType.BLACKBOARD_DELTA, run_id: "run-products", ts: 5,
          solver_id: "review", payload: {{ kind: "branch_split", branch_id: "branch-a", title: "patched branch" }} }});
        s = lib.reduce(s, {{ event_type: lib.EventType.BLACKBOARD_DELTA, run_id: "run-products", ts: 6,
          solver_id: "review", payload: {{ kind: "branch_resolved", branch_id: "branch-a" }} }});

        assert(s.model.edges.some((e) => e.source === "intent:I1" && e.target === "fact:7"), "product edge");
        // G0 multi-product: fact.intentId is persisted on BOTH produced facts so the
        // canvas (producerByFact / owningIntent) can draw multiple produces-edges.
        const f7 = s.blackboard.facts.find((f) => f.factSeq === 7);
        const f8 = s.blackboard.facts.find((f) => f.factSeq === 8);
        assert(f7 && f7.intentId === "I1", "fact 7 intentId persisted (survives blank re-emit)");
        assert(f8 && f8.intentId === "I1", "fact 8 intentId persisted (second product)");
        assert(s.blackboard.branches[0].status === "resolved", "branch resolved");
        """
    )
    _run_ui_node(script)


def test_blackboard_canvas_consumes_g0_product_edges():
    # G0 surface in the canvas: producerByFact / owningIntent must prefer the
    # per-fact intentId (intent_products) over the single conclude-time toFactSeq
    # and over the time-window guess — otherwise multi-product edges never render.
    src = (UI_ROOT / "components" / "Blackboard.tsx").read_text()
    assert "fact.intentId && intentById.has(fact.intentId)" in src, "producerByFact must consume fact.intentId"
    assert "producerByFact.set(fact.factSeq, fact.intentId)" in src, "multi-product edge build"
    # owningIntent prefers the authoritative producer before the heuristic
    assert "if (fact.intentId) {" in src, "owningIntent prefers G0 intentId"


def test_blackboard_empty_guard_matches_rendered_node_inputs():
    src = (UI_ROOT / "components" / "Blackboard.tsx").read_text()
    assert "bb.events.length" not in src
    assert 'type: "meta"' in src
    assert "bb.deadEnds.length === 0 && bb.pocs.length === 0" in src
    assert "bb.reviewFindings.length === 0" in src
    assert "bb.suppressedRoutes.length === 0" in src
    assert "bb.branches.length === 0" in src
    assert "bb.directives.length === 0 && !bb.flag" in src


def test_artifact_panel_exposes_architecture_side_panels():
    page = (UI_ROOT / "app" / "page.tsx").read_text()
    inspector = (UI_ROOT / "components" / "RunInspector.tsx").read_text()
    assert 'view === "findings"' in page
    assert 'view === "credentials"' in page
    assert 'view === "pocs"' in page
    assert 'view === "routes"' in page
    assert "RuntimeCredentials" in page
    assert "RuntimeDataView" in page
    assert 'panelBtn("credentials"' in inspector


def test_events_reducer_tracks_runtime_degraded_blackboard_delta():
    helper = UI_ROOT / "lib" / "events.ts"
    script = textwrap.dedent(
        f"""
        const fs = require("fs");
        const ts = require("typescript");
        const vm = require("vm");
        const source = fs.readFileSync({json.dumps(str(helper))}, "utf8");
        const out = ts.transpileModule(source, {{
          compilerOptions: {{ module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 }}
        }}).outputText;
        const sandbox = {{ module: {{ exports: {{}} }}, exports: {{}} }};
        sandbox.exports = sandbox.module.exports;
        vm.runInNewContext(out, sandbox, {{ filename: "events.js" }});
        const lib = sandbox.module.exports;
        function assert(cond, msg) {{ if (!cond) throw new Error(msg); }}

        let s = lib.emptyDeck("run-runtime");
        s = lib.reduce(s, {{ event_type: lib.EventType.BLACKBOARD_DELTA, run_id: "run-runtime", ts: 1,
          solver_id: "coordinator", payload: {{ kind: "runtime_degraded", engine: "claude",
          requested_backend: "container", backend: "local", status: "degraded", reason: "docker down" }} }});

        assert(s.blackboard.events.some((e) => e.kind === "runtime_degraded"), "timeline event");
        assert(s.lanes.claude.runtime.status === "degraded", "lane runtime degraded");
        """
    )
    _run_ui_node(script)


def test_events_reducer_tracks_phase_and_budget_events():
    src = (UI_ROOT / "lib" / "events.ts").read_text()
    assert 'case "phase_transition"' in src
    assert 'case "worker_budget_exhausted"' in src
    assert 'case "cost_budget_exhausted"' in src


def test_dispatch_sends_per_run_budget_controls():
    src = (UI_ROOT / "app" / "page.tsx").read_text()
    convo = (UI_ROOT / "components" / "Conversation.tsx").read_text()
    css = (UI_ROOT / "app" / "globals.css").read_text()
    i18n = (UI_ROOT / "lib" / "i18n.tsx").read_text()
    assert "runOverrides.race_timeout = opts.raceTimeout" in src
    assert "runOverrides.max_total_workers = opts.maxTotalWorkers" in src
    assert "runOverrides.cost_budget_usd = opts.costBudgetUsd" in src
    assert "raceTimeout?: number" in convo
    assert 't("composer.maxTotalWorkers")' in convo
    assert "advancedOpen" in convo
    assert 'className="composer-advanced-panel"' in convo
    assert 'aria-controls="dispatch-advanced-controls"' in convo
    assert '"composer.advanced"' in i18n
    assert ".composer-advanced-panel" in css


def test_collect_mode_does_not_force_token_flag_format():
    src = (UI_ROOT / "app" / "page.tsx").read_text()
    convo = (UI_ROOT / "components" / "Conversation.tsx").read_text()
    i18n = (UI_ROOT / "lib" / "i18n.tsx").read_text()

    assert 'flagFormat?: "brace" | "token" | "custom"' in convo
    assert 'opts?.flagFormat === "token"' in src
    assert "multi_flag + token gate" not in convo
    assert "bare token, not flag{...}" not in i18n
    assert '"composer.flagFormat"' in i18n


def test_dispatch_threads_custom_flag_wrapper():
    src = (UI_ROOT / "app" / "page.tsx").read_text()
    convo = (UI_ROOT / "components" / "Conversation.tsx").read_text()
    i18n = (UI_ROOT / "lib" / "i18n.tsx").read_text()

    assert 'flagFormat?: "brace" | "token" | "custom"' in convo
    assert "flagWrapper?: string" in convo
    assert "muteki.flagWrapper" in convo
    assert 't("composer.flagWrapperPlaceholder")' in convo
    assert "challenge.flag_format_wrapper = opts.flagWrapper" in src
    assert '"composer.flagFormatCustom"' in i18n
    assert '"composer.flagWrapperTitle"' in i18n


def test_advanced_flag_format_layout_is_not_cramped():
    convo = (UI_ROOT / "components" / "Conversation.tsx").read_text()
    css = (UI_ROOT / "app" / "globals.css").read_text()

    assert 'className="advanced-metrics-grid"' in convo
    assert 'className="advanced-field advanced-metric-field"' in convo
    assert ".advanced-field.flag-format-field { grid-column: 1 / -1" in css
    assert ".flag-format-controls { display: grid" in css
    assert ".advanced-metrics-grid { display: grid" in css
    assert ".advanced-metric-field { display: flex" in css
    assert ".composer2 .advanced-metric-field .collect-count { width: 100%" in css


def test_per_run_zero_budget_values_are_preserved():
    src = (UI_ROOT / "app" / "page.tsx").read_text()
    convo = (UI_ROOT / "components" / "Conversation.tsx").read_text()

    assert "opts?.wallClockBudget != null" in src
    assert "opts?.maxTotalWorkers != null" in src
    assert "opts?.costBudgetUsd != null" in src
    assert "Number.isNaN" in convo


def test_control_client_uses_generation_cas_and_surfaces_errors():
    use_run = (UI_ROOT / "lib" / "useRun.ts").read_text()
    page = (UI_ROOT / "app" / "page.tsx").read_text()
    assert 'CONTROL_CAS_ACTIONS.has(action)' in use_run
    assert 'body.expected_generation = deck.controlGeneration' in use_run
    assert 'body.command_id = opts.commandId' in use_run
    assert '/api/runs/${runId}/control' in use_run
    assert "if (!res.ok || data?.ok === false)" in use_run
    assert "return data;" in use_run
    assert 'catch {' in page
    assert 't("toast.actionFailed")' in page


def test_hitl_card_is_read_only_after_answer_admission_and_surfaces_recovery():
    convo = (UI_ROOT / "components" / "Conversation.tsx").read_text()
    i18n = (UI_ROOT / "lib" / "i18n.tsx").read_text()
    css = (UI_ROOT / "app" / "globals.css").read_text()

    assert "const answerRecorded = locallyRecorded || durableDelivery.locked" in convo
    assert 'commandIdForDecision(commandIdsRef.current, "answer_decision")' in convo
    assert "onAnswer(req.id, v, commandId)" in convo
    assert "onAnswer(req.id, attempt.value, attempt.commandId)" in convo
    assert '"global", "answer_decision", opt, { requestId, commandId }' in convo
    assert "if (!v || sending || answerRecorded) return" in convo
    assert "pauses && !answerRecorded" in convo
    assert 'className={`hitl-delivery-state ${deliveryPhase}`}' in convo
    assert '"hitl.delivery.unknown"' in i18n
    assert '"hitl.delivery.failed"' in i18n
    assert '"hitl.delivery.partial"' in i18n
    assert "do not answer again" in i18n
    assert ".hitl-card.answered-readonly" in css
    assert ".hitl-delivery-state.failed" in css
    assert ".hitl-delivery-retry" in css


def test_decision_command_id_is_stable_across_first_attempt_and_retry():
    helper = UI_ROOT / "lib" / "controlClient.ts"
    script = textwrap.dedent(
        f"""
        const fs = require("fs");
        const ts = require("typescript");
        const vm = require("vm");
        const source = fs.readFileSync({json.dumps(str(helper))}, "utf8");
        const out = ts.transpileModule(source, {{
          compilerOptions: {{ module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 }}
        }}).outputText;
        const sandbox = {{ module: {{ exports: {{}} }}, exports: {{}} }};
        sandbox.exports = sandbox.module.exports;
        vm.runInNewContext(out, sandbox, {{ filename: "controlClient.js" }});
        const lib = sandbox.module.exports;
        function assert(cond, msg) {{ if (!cond) throw new Error(msg); }}

        const ids = {{}};
        let generated = 0;
        const create = () => `client-command-${{++generated}}`;
        const first = lib.commandIdForDecision(ids, "answer_decision", create);
        const retry = lib.commandIdForDecision(ids, "answer_decision", create);
        assert(first === retry, "first POST and retry must carry the same command_id");
        assert(generated === 1, "retry never allocates a second answer id");
        const dismiss = lib.commandIdForDecision(ids, "dismiss", create);
        assert(dismiss !== first, "a different decision action owns a different stable id");
        assert(generated === 2, "one id is allocated per request/action");
        """
    )
    _run_ui_node(script)


def test_finished_run_does_not_render_rail_footer_as_disconnected():
    rail = (UI_ROOT / "components" / "ThreadRail.tsx").read_text()
    i18n = (UI_ROOT / "lib" / "i18n.tsx").read_text()
    assert "activeRun?.finished" in rail
    assert "rail.runFinished" in rail
    assert '"rail.runFinished"' in i18n


def test_events_reducer_tracks_operator_paused_blackboard_delta():
    helper = UI_ROOT / "lib" / "events.ts"
    script = textwrap.dedent(
        f"""
        const fs = require("fs");
        const ts = require("typescript");
        const vm = require("vm");
        const source = fs.readFileSync({json.dumps(str(helper))}, "utf8");
        const out = ts.transpileModule(source, {{
          compilerOptions: {{ module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 }}
        }}).outputText;
        const sandbox = {{ module: {{ exports: {{}} }}, exports: {{}} }};
        sandbox.exports = sandbox.module.exports;
        vm.runInNewContext(out, sandbox, {{ filename: "events.js" }});
        const lib = sandbox.module.exports;
        function assert(cond, msg) {{ if (!cond) throw new Error(msg); }}

        let s = lib.emptyDeck("run-pause");
        s = lib.reduce(s, {{ event_type: lib.EventType.RUN_STARTED, run_id: "run-pause", ts: 1,
          payload: {{ challenge: {{ name: "pause test", category: "web" }} }} }});
        s = lib.reduce(s, {{ event_type: lib.EventType.BLACKBOARD_DELTA, run_id: "run-pause", ts: 2,
          solver_id: "coordinator", payload: {{ kind: "operator_paused", reason: "manual pause" }} }});
        assert(s.awaitingOperator === "manual pause", "paused reason");
        assert(lib.swarmDigest(s).phase === "paused", "digest paused");
        s = lib.reduce(s, {{ event_type: lib.EventType.BLACKBOARD_DELTA, run_id: "run-pause", ts: 3,
          solver_id: "coordinator", payload: {{ kind: "operator_resumed" }} }});
        assert(!s.awaitingOperator, "resume clears pause");
        assert(lib.swarmDigest(s).phase !== "paused", "digest resumed");

        s = lib.reduce(s, {{ event_type: lib.EventType.BLACKBOARD_DELTA, run_id: "run-pause", ts: 4,
          solver_id: "coordinator", payload: {{ kind: "operator_paused", reason: "manual pause again" }} }});
        assert(lib.swarmDigest(s).phase === "paused", "digest paused again");
        s = lib.reduce(s, {{ event_type: lib.EventType.CONTROL_COMMAND, run_id: "run-pause", ts: 5,
          payload: {{ command_id: "cmd-resume", target: "global", action: "resume",
            status: "routed", generation: 1 }} }});
        assert(!!s.awaitingOperator, "routed command is not proof of resume");
        assert(lib.swarmDigest(s).phase === "paused", "digest stays paused before observed effect");
        s = lib.reduce(s, {{ event_type: lib.EventType.CONTROL_COMMAND, run_id: "run-pause", ts: 5.5,
          payload: {{ command_id: "cmd-newer", target: "global", action: "hint",
            status: "routed", generation: 2 }} }});
        s = lib.reduce(s, {{ event_type: lib.EventType.CONTROL_COMMAND, run_id: "run-pause", ts: 6,
          payload: {{ command_id: "cmd-resume", target: "global", action: "resume",
            status: "effect_observed", generation: 1, effect: {{ kind: "run_resumed" }} }} }});
        assert(!!s.awaitingOperator, "stale generation cannot clear a newer pause");
        assert(lib.swarmDigest(s).phase === "paused", "stale effect leaves run paused");
        assert(s.controlCommands.length === 2, "command lifecycle is folded by command_id");
        assert(s.controlCommands[1].id === "cmd-resume", "terminal update moves to latest tail");
        assert(s.controlCommands[1].status === "effect_observed", "latest terminal status wins");
        assert(s.controlGeneration === 2, "generation never regresses on a late terminal update");
        """
    )
    _run_ui_node(script)


def test_events_reducer_resolves_only_addressed_hitl_request():
    helper = UI_ROOT / "lib" / "events.ts"
    script = textwrap.dedent(
        f"""
        const fs = require("fs");
        const ts = require("typescript");
        const vm = require("vm");
        const source = fs.readFileSync({json.dumps(str(helper))}, "utf8");
        const out = ts.transpileModule(source, {{
          compilerOptions: {{ module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 }}
        }}).outputText;
        const sandbox = {{ module: {{ exports: {{}} }}, exports: {{}} }};
        sandbox.exports = sandbox.module.exports;
        vm.runInNewContext(out, sandbox, {{ filename: "events.js" }});
        const lib = sandbox.module.exports;
        function assert(cond, msg) {{ if (!cond) throw new Error(msg); }}

        let s = lib.emptyDeck("run-decisions");
        s = lib.reduce(s, {{ event_type: lib.EventType.HITL_REQUEST, run_id: "run-decisions", ts: 1,
          payload: {{ request_id: "H-one", worker: "worker-a", need: "need token A" }} }});
        s = lib.reduce(s, {{ event_type: lib.EventType.HITL_REQUEST, run_id: "run-decisions", ts: 2,
          payload: {{ request_id: "H-two", worker: "worker-b", need: "need token B" }} }});
        assert(s.hitlRequests.length === 2, "both decisions are pending");

        s = lib.reduce(s, {{ event_type: lib.EventType.HITL_RESPONSE, run_id: "run-decisions", ts: 3,
          payload: {{ request_id: "H-one", action: "submit", text: "token-a", status: "received" }} }});
        assert(s.hitlRequests.length === 2, "admission echo does not resolve a decision");
        s = lib.reduce(s, {{ event_type: lib.EventType.CONTROL_COMMAND, run_id: "run-decisions", ts: 3.5,
          payload: {{ command_id: "C-answer", request_id: "H-one", action: "answer_decision",
            status: "effect_observed", decision_closed: true }} }});
        assert(s.hitlRequests.length === 1, "durable decision close resolves one decision");
        assert(s.hitlRequests[0].id === "H-two", "unaddressed decision remains");

        s = lib.reduce(s, {{ event_type: lib.EventType.HITL_RESPONSE, run_id: "run-decisions", ts: 4,
          payload: {{ action: "hint", text: "unrelated clue", status: "received" }} }});
        assert(s.hitlRequests.length === 1, "unscoped command cannot erase a decision");
        """
    )
    _run_ui_node(script)


def test_events_reducer_correlates_decision_delivery_without_duplicate_answer():
    helper = UI_ROOT / "lib" / "events.ts"
    script = textwrap.dedent(
        f"""
        const fs = require("fs");
        const ts = require("typescript");
        const vm = require("vm");
        const source = fs.readFileSync({json.dumps(str(helper))}, "utf8");
        const out = ts.transpileModule(source, {{
          compilerOptions: {{ module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 }}
        }}).outputText;
        const sandbox = {{ module: {{ exports: {{}} }}, exports: {{}} }};
        sandbox.exports = sandbox.module.exports;
        vm.runInNewContext(out, sandbox, {{ filename: "events.js" }});
        const lib = sandbox.module.exports;
        function assert(cond, msg) {{ if (!cond) throw new Error(msg); }}

        let s = lib.emptyDeck("run-delivery");
        s = lib.reduce(s, {{ event_type: lib.EventType.HITL_REQUEST, run_id: "run-delivery", ts: 0.1,
          payload: {{ request_id: "H-rejected", worker: "worker-b", need: "choose account" }} }});
        s = lib.reduce(s, {{ event_type: lib.EventType.CONTROL_COMMAND, run_id: "run-delivery", ts: 0.2,
          payload: {{ command_id: "C-rejected", request_id: "H-rejected", action: "answer_decision",
            status: "received" }} }});
        s = lib.reduce(s, {{ event_type: lib.EventType.CONTROL_COMMAND, run_id: "run-delivery", ts: 0.3,
          payload: {{ command_id: "C-rejected", request_id: "H-rejected", action: "answer_decision",
            status: "rejected", detail: "durable companion failure" }} }});
        let rejected = s.hitlRequests.find((r) => r.id === "H-rejected");
        assert(rejected, "pre-persistence rejection keeps decision request visible");
        assert(lib.hitlDeliveryState(rejected).phase === "open",
          "RECEIVED to REJECTED without decision_closed remains editable");

        s = lib.reduce(s, {{ event_type: lib.EventType.HITL_REQUEST, run_id: "run-delivery", ts: 1,
          payload: {{ request_id: "H-answer", worker: "worker-a", need: "choose route" }} }});
        s = lib.reduce(s, {{ event_type: lib.EventType.CONTROL_COMMAND, run_id: "run-delivery", ts: 2,
          payload: {{ command_id: "C-answer", request_id: "H-answer", action: "answer_decision",
            status: "persisted", decision_closed: true }} }});
        assert(s.hitlRequests.length === 2, "persisted admission keeps card until effect proof");
        let req = s.hitlRequests.find((r) => r.id === "H-answer");
        assert(req.deliveryCommandId === "C-answer", "request correlates its durable command");
        assert(req.deliveryStatus === "persisted", "persisted lifecycle is projected onto card");
        let delivery = lib.hitlDeliveryState(req);
        assert(delivery.locked && delivery.phase === "pending", "persisted answer is read-only pending");

        s = lib.reduce(s, {{ event_type: lib.EventType.CONTROL_COMMAND, run_id: "run-delivery", ts: 3,
          payload: {{ command_id: "C-answer", request_id: "H-answer", action: "answer_decision",
            status: "unknown", detail: "effect proof timed out", decision_closed: true }} }});
        assert(s.hitlRequests.length === 2, "UNKNOWN never closes the decision");
        req = s.hitlRequests.find((r) => r.id === "H-answer");
        delivery = lib.hitlDeliveryState(req);
        assert(delivery.locked && delivery.phase === "unknown", "UNKNOWN clears spinner into locked recovery");
        assert(delivery.detail === "effect proof timed out", "recovery detail remains inspectable");

        // A delayed non-terminal frame cannot regress terminal recovery back to a spinner.
        s = lib.reduce(s, {{ event_type: lib.EventType.CONTROL_COMMAND, run_id: "run-delivery", ts: 4,
          payload: {{ command_id: "C-answer", request_id: "H-answer", action: "answer_decision",
            status: "routed" }} }});
        req = s.hitlRequests.find((r) => r.id === "H-answer");
        assert(req.deliveryStatus === "unknown", "late ROUTED cannot regress UNKNOWN");

        s = lib.reduce(s, {{ event_type: lib.EventType.CONTROL_COMMAND, run_id: "run-delivery", ts: 5,
          payload: {{ command_id: "C-answer", request_id: "H-answer", action: "answer_decision",
            status: "effect_observed", decision_closed: true }} }});
        assert(!s.hitlRequests.some((r) => r.id === "H-answer"),
          "only observed decision close removes the addressed card");
        assert(s.hitlRequests.some((r) => r.id === "H-rejected"),
          "pre-persistence rejected decision remains open after another card closes");
        """
    )
    _run_ui_node(script)


def test_events_reducer_surfaces_standby_writeup_in_coordinator_thread():
    helper = UI_ROOT / "lib" / "events.ts"
    script = textwrap.dedent(
        f"""
        const fs = require("fs");
        const ts = require("typescript");
        const vm = require("vm");
        const source = fs.readFileSync({json.dumps(str(helper))}, "utf8");
        const out = ts.transpileModule(source, {{
          compilerOptions: {{ module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 }}
        }}).outputText;
        const sandbox = {{ module: {{ exports: {{}} }}, exports: {{}} }};
        sandbox.exports = sandbox.module.exports;
        vm.runInNewContext(out, sandbox, {{ filename: "events.js" }});
        const lib = sandbox.module.exports;
        function assert(cond, msg) {{ if (!cond) throw new Error(msg); }}

        let s = lib.emptyDeck("run-writeup");
        s = lib.reduce(s, {{ event_type: lib.EventType.RUN_STARTED, run_id: "run-writeup", ts: 1,
          payload: {{ challenge: {{ name: "writeup", category: "web" }} }} }});
        s = lib.reduce(s, {{ event_type: lib.EventType.WORKER_STATUS, run_id: "run-writeup", ts: 2,
          solver_id: "cli-claude-standby", payload: {{ online: true, status: "online", reason: "standby", engine: "claude" }} }});
        s = lib.reduce(s, {{ event_type: lib.EventType.TEXT_MESSAGE_DELTA, run_id: "run-writeup", ts: 3,
          solver_id: "cli-claude-standby", payload: {{ text: "# Writeup\\nSolved via /api/flag." }} }});
        assert(s.chat.some((m) => m.solverId === "cli-claude-standby" && m.mainThread === true), "standby text tagged for main thread");
        assert(lib.coordinatorThread(s).some((m) => m.content.includes("# Writeup")), "writeup appears in coordinator thread");

        let worker = lib.emptyDeck("run-worker");
        worker = lib.reduce(worker, {{ event_type: lib.EventType.WORKER_STATUS, run_id: "run-worker", ts: 1,
          solver_id: "cli-claude-1", payload: {{ online: true, status: "online", reason: "started", engine: "claude" }} }});
        worker = lib.reduce(worker, {{ event_type: lib.EventType.TEXT_MESSAGE_DELTA, run_id: "run-worker", ts: 2,
          solver_id: "cli-claude-1", payload: {{ text: "ordinary worker firehose" }} }});
        assert(!lib.coordinatorThread(worker).some((m) => m.content.includes("ordinary worker")), "ordinary worker text stays out of main thread");
        """
    )
    _run_ui_node(script)


def test_events_reducer_labels_resolve_reopen_separately_from_false_positive():
    helper = UI_ROOT / "lib" / "events.ts"
    script = textwrap.dedent(
        f"""
        const fs = require("fs");
        const ts = require("typescript");
        const vm = require("vm");
        const source = fs.readFileSync({json.dumps(str(helper))}, "utf8");
        const out = ts.transpileModule(source, {{
          compilerOptions: {{ module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 }}
        }}).outputText;
        const sandbox = {{ module: {{ exports: {{}} }}, exports: {{}} }};
        sandbox.exports = sandbox.module.exports;
        vm.runInNewContext(out, sandbox, {{ filename: "events.js" }});
        const lib = sandbox.module.exports;
        function assert(cond, msg) {{ if (!cond) throw new Error(msg); }}
        function lastChat(s) {{ return s.chat[s.chat.length - 1]; }}

        let resolveDeck = lib.emptyDeck("run-resolve");
        resolveDeck = lib.reduce(resolveDeck, {{ event_type: lib.EventType.RUN_STARTED, run_id: "run-resolve", ts: 1,
          payload: {{ challenge: {{ name: "resolve", category: "web" }} }} }});
        resolveDeck = lib.reduce(resolveDeck, {{ event_type: lib.EventType.BLACKBOARD_DELTA, run_id: "run-resolve", ts: 1.5,
          solver_id: "coordinator", payload: {{ kind: "operator_paused", reason: "pause before restart" }} }});
        resolveDeck = lib.reduce(resolveDeck, {{ event_type: lib.EventType.RUN_FINISHED, run_id: "run-resolve", ts: 2,
          payload: {{ solved: true, flag: "flag{{old}}", flags: ["flag{{old}}", "flag{{kept}}"] }} }});
        resolveDeck = lib.reduce(resolveDeck, {{ event_type: lib.EventType.RUN_REOPENED, run_id: "run-resolve", ts: 3,
          payload: {{ reason: "resolve" }} }});
        assert(lastChat(resolveDeck).i18nKey === "sys.resolveReopened", "resolve copy");
        assert(!resolveDeck.awaitingOperator, "resolve reopen clears stale pause");
        assert(lib.swarmDigest(resolveDeck).phase !== "paused", "resolve reopen digest is not paused");
        assert(resolveDeck.flags.length === 2, "resolve keeps prior flags");
        assert(resolveDeck.flags[0] === "flag{{old}}" && resolveDeck.flags[1] === "flag{{kept}}", "resolve preserves flag order");
        assert(resolveDeck.flag === "flag{{old}}", "resolve keeps primary flag");

        let falseDeck = lib.emptyDeck("run-false");
        falseDeck = lib.reduce(falseDeck, {{ event_type: lib.EventType.RUN_STARTED, run_id: "run-false", ts: 1,
          payload: {{ challenge: {{ name: "false", category: "web" }} }} }});
        falseDeck = lib.reduce(falseDeck, {{ event_type: lib.EventType.RUN_FINISHED, run_id: "run-false", ts: 2,
          payload: {{ solved: true, flag: "flag{{bad}}", flags: ["flag{{bad}}", "flag{{good}}"] }} }});
        falseDeck = lib.reduce(falseDeck, {{ event_type: lib.EventType.RUN_REOPENED, run_id: "run-false", ts: 3,
          payload: {{ flag: "flag{{bad}}" }} }});
        assert(lastChat(falseDeck).i18nKey === "sys.reopened", "false-positive copy");
        assert(falseDeck.flags.length === 1 && falseDeck.flags[0] === "flag{{good}}", "drops only bad flag");

        let replayDeck = lib.emptyDeck("run-replay");
        replayDeck = lib.reduce(replayDeck, {{ event_type: lib.EventType.RUN_STARTED, run_id: "run-replay", ts: 1,
          payload: {{ challenge: {{ name: "replay", category: "reverse" }} }} }});
        replayDeck = lib.reduce(replayDeck, {{ event_type: lib.EventType.RUN_FINISHED, run_id: "run-replay", ts: 2,
          payload: {{ solved: true, flag: "flag{{bad}}", flags: ["flag{{bad}}"] }} }});
        replayDeck = lib.reduce(replayDeck, {{ event_type: lib.EventType.BLACKBOARD_DELTA, run_id: "run-replay", ts: 3,
          solver_id: "coordinator", payload: {{ kind: "flag_invalidated", flag: "flag{{bad}}" }} }});
        replayDeck = lib.reduce(replayDeck, {{ event_type: lib.EventType.RUN_REOPENED, run_id: "run-replay", ts: 4,
          payload: {{ flag: "flag{{bad}}" }} }});
        replayDeck = lib.reduce(replayDeck, {{ event_type: lib.EventType.BLACKBOARD_DELTA, run_id: "run-replay", ts: 5,
          solver_id: "coordinator", payload: {{ kind: "flag_found", flag: "flag{{bad}}" }} }});
        assert(replayDeck.flags.length === 0, "invalidated flag replay is ignored");
        assert(!replayDeck.flag, "invalidated flag does not remain primary");
        assert(replayDeck.finished === false && replayDeck.solved === false, "reopened run stays active");
        assert(replayDeck.finishedAt == null, "reopened run clears frozen finish time");
        assert(lib.swarmDigest(replayDeck).phase === "running", "reopened run digest is running, not solved");
        """
    )
    _run_ui_node(script)


def test_flag_accepted_reducer_is_visible_but_nonterminal_and_not_blackboard():
    helper = UI_ROOT / "lib" / "events.ts"
    script = textwrap.dedent(
        f"""
        const fs = require("fs");
        const ts = require("typescript");
        const vm = require("vm");
        const source = fs.readFileSync({json.dumps(str(helper))}, "utf8");
        const out = ts.transpileModule(source, {{
          compilerOptions: {{ module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 }}
        }}).outputText;
        const sandbox = {{ module: {{ exports: {{}} }}, exports: {{}} }};
        sandbox.exports = sandbox.module.exports;
        vm.runInNewContext(out, sandbox, {{ filename: "events.js" }});
        const lib = sandbox.module.exports;
        function assert(cond, msg) {{ if (!cond) throw new Error(msg); }}

        let started = lib.emptyDeck("run-p2-started");
        started = lib.reduce(started, {{ event_type: lib.EventType.RUN_STARTED,
          run_id: "run-p2-started", ts: 1,
          payload: {{ challenge: {{ name: "p2", expected_flags: 1 }} }} }});
        const beforeEvents = started.blackboard.events.length;
        const beforeChat = started.chat.length;
        started = lib.reduce(started, {{ event_type: lib.EventType.FLAG_ACCEPTED,
          run_id: "run-p2-started", ts: 2,
          payload: {{ schema_id: "muteki.flag-accepted-projection.v1",
            publication_id: "outbox:flag:" + "a".repeat(64),
            evaluation_id: "a".repeat(64), flag: "flag{{accepted}}",
            flag_digest: "b".repeat(64), gate_receipt_digest: "c".repeat(64) }} }});
        assert(started.flag === "flag{{accepted}}", "accepted flag is visible");
        assert(started.flags.length === 1, "accepted flag merges into visible set");
        assert(started.solved === false && started.finished === false, "accepted is nonterminal");
        assert(started.blackboard.events.length === beforeEvents, "no blackboard timeline");
        assert(started.chat.length === beforeChat, "no progress narration");
        assert(lib.swarmDigest(started).phase === "collecting", "started accepted stays collecting");
        assert(lib.isRunActive(started) === true, "accepted-only controls remain active");

        let acceptedOnly = lib.emptyDeck("run-p2-accepted-only");
        acceptedOnly = lib.reduce(acceptedOnly, {{ event_type: lib.EventType.FLAG_ACCEPTED,
          run_id: "run-p2-accepted-only", ts: 3,
          payload: {{ flag: "flag{{catalog-only}}" }} }});
        assert(acceptedOnly.started === false, "accepted event does not synthesize started");
        assert(acceptedOnly.solved === false && acceptedOnly.finished === false, "accepted-only is nonterminal");
        assert(lib.swarmDigest(acceptedOnly).phase === "collecting", "accepted-only digest is collecting");
        """
    )
    _run_ui_node(script)


def test_run_active_selector_closes_controls_after_terminal_flag_signal():
    helper = UI_ROOT / "lib" / "events.ts"
    script = textwrap.dedent(
        f"""
        const fs = require("fs");
        const ts = require("typescript");
        const vm = require("vm");
        const source = fs.readFileSync({json.dumps(str(helper))}, "utf8");
        const out = ts.transpileModule(source, {{
          compilerOptions: {{ module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 }}
        }}).outputText;
        const sandbox = {{ module: {{ exports: {{}} }}, exports: {{}} }};
        sandbox.exports = sandbox.module.exports;
        vm.runInNewContext(out, sandbox, {{ filename: "events.js" }});
        const lib = sandbox.module.exports;
        function assert(cond, msg) {{ if (!cond) throw new Error(msg); }}

        let deck = lib.emptyDeck("run-live-missed-finish");
        deck = lib.reduce(deck, {{ event_type: lib.EventType.RUN_STARTED, run_id: "run-live-missed-finish", ts: 1,
          payload: {{ challenge: {{ name: "firmware", category: "misc", expected_flags: 1, multi_flag: false }} }} }});
        deck = lib.reduce(deck, {{ event_type: lib.EventType.BLACKBOARD_DELTA, run_id: "run-live-missed-finish", ts: 2,
          solver_id: "cli-codex", payload: {{ kind: "flag_found", actor: "cli-codex", flag: "flag{{password}}" }} }});
        deck = lib.reduce(deck, {{ event_type: lib.EventType.WORKER_FINISHED, run_id: "run-live-missed-finish", ts: 3,
          solver_id: "cli-codex", payload: {{ solved: true, flag: "flag{{password}}", flags: ["flag{{password}}"] }} }});

        assert(lib.swarmDigest(deck).phase === "solved", "digest recognizes the single-flag solve");
        assert(lib.isRunActive(deck) === false, "live controls should close even if run.finished was missed");

        let collecting = lib.emptyDeck("run-collecting");
        collecting = lib.reduce(collecting, {{ event_type: lib.EventType.RUN_STARTED, run_id: "run-collecting", ts: 1,
          payload: {{ challenge: {{ name: "multi", category: "misc", expected_flags: 2, multi_flag: true }} }} }});
        collecting = lib.reduce(collecting, {{ event_type: lib.EventType.BLACKBOARD_DELTA, run_id: "run-collecting", ts: 2,
          solver_id: "cli-codex", payload: {{ kind: "flag_found", actor: "cli-codex", flag: "flag{{one}}" }} }});
        assert(lib.swarmDigest(collecting).phase === "collecting", "partial multi-flag solve stays collecting");
        assert(lib.isRunActive(collecting) === true, "partial multi-flag run keeps controls active");
        """
    )
    _run_ui_node(script)


def test_pentest_digest_keeps_controls_until_run_finished():
    """Accepted reports must not close stop/steer before RUN_FINISHED.

    The N=1 collect bug showed the hero as goal_met while the coordinator
    kept dispatching; live controls must stay open until the backend finishes.
    """
    helper = UI_ROOT / "lib" / "events.ts"
    script = textwrap.dedent(
        f"""
        const fs = require("fs");
        const ts = require("typescript");
        const vm = require("vm");
        const source = fs.readFileSync({json.dumps(str(helper))}, "utf8");
        const out = ts.transpileModule(source, {{
          compilerOptions: {{ module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 }}
        }}).outputText;
        const sandbox = {{ module: {{ exports: {{}} }}, exports: {{}} }};
        sandbox.exports = sandbox.module.exports;
        vm.runInNewContext(out, sandbox, {{ filename: "events.js" }});
        const lib = sandbox.module.exports;
        function assert(cond, msg) {{ if (!cond) throw new Error(msg); }}

        let deck = lib.emptyDeck("run-pentest-n1");
        deck = lib.reduce(deck, {{ event_type: lib.EventType.RUN_STARTED, run_id: "run-pentest-n1", ts: 1,
          payload: {{ challenge: {{ name: "dvwa", category: "web", mode: "pentest",
            expected_findings: 1, engagement: {{ expected_findings: 1, quantity: "collect" }} }} }} }});
        deck = lib.reduce(deck, {{ event_type: lib.EventType.BLACKBOARD_DELTA, run_id: "run-pentest-n1", ts: 2,
          solver_id: "cli-pi", payload: {{ kind: "report_accepted", actor: "cli-pi",
            report_id: "r1", title: "SQLi on /vulnerabilities/sqli/" }} }});

        assert(lib.swarmDigest(deck).phase === "collecting", "accepted report before goal_complete stays collecting");
        assert(lib.swarmDigest(deck).reports.length === 1, "accepted report is in the digest");
        assert(lib.isRunActive(deck) === true, "stop stays available while the run is not finished");

        deck = lib.reduce(deck, {{ event_type: lib.EventType.BLACKBOARD_DELTA, run_id: "run-pentest-n1", ts: 3,
          payload: {{ kind: "goal_complete", why: "gated_reports", reports: 1 }} }});
        assert(lib.swarmDigest(deck).phase === "goal_met", "goal_complete marks the digest goal_met");
        assert(lib.isRunActive(deck) === true, "stop stays until RUN_FINISHED");

        deck = lib.reduce(deck, {{ event_type: lib.EventType.RUN_FINISHED, run_id: "run-pentest-n1", ts: 4,
          payload: {{ solved: true, reason: "goal_met" }} }});
        assert(lib.swarmDigest(deck).phase === "goal_met", "finished pentest stays goal_met");
        assert(lib.isRunActive(deck) === false, "controls close after RUN_FINISHED");
        """
    )
    _run_ui_node(script)


def test_run_inspector_shows_accepted_pentest_reports():
    inspector = (UI_ROOT / "components" / "RunInspector.tsx").read_text()
    i18n = (UI_ROOT / "lib" / "i18n.tsx").read_text()
    markdown = (UI_ROOT / "lib" / "reportMarkdown.ts").read_text()
    assert "directoryReports" in inspector
    assert "PentestReportDirectory" in inspector
    assert "onOpenReport" in inspector
    assert "insp.run.exportCollection" in inspector
    assert "insp.run.reportHint" in inspector
    assert "reportsToCollectionMarkdown" in inspector
    assert "reportToMarkdown(row)" not in inspector
    assert '"insp.run.reports"' in i18n
    assert '"insp.run.exportCollection"' in i18n
    assert "reportLocationLabel" in markdown


def test_evidence_chain_is_compact_expandable_workbench():
    evidence = (UI_ROOT / "components" / "EvidenceChain.tsx").read_text()
    css = (UI_ROOT / "app" / "globals.css").read_text()
    i18n = (UI_ROOT / "lib" / "i18n.tsx").read_text()

    assert 'className="panel-scroll-wrap evidence-panel"' in evidence
    assert 'className="evi-toolbar"' in evidence
    assert "EvidenceFilter" in evidence
    assert "role=\"tablist\"" in evidence
    assert "aria-selected={filter === b.key}" in evidence
    assert "expandedIds" in evidence
    assert "aria-expanded={expanded}" in evidence
    assert "evi-meta-inline" in evidence
    assert "{expanded && (" in evidence
    assert ".evi-toolbar" in css
    assert ".evi-filter" in css
    assert ".evi-row" in css
    assert "-webkit-line-clamp: 2" in css
    assert ".evi-meta-inline" in css
    assert "evidence.clickHint" in i18n
    assert "evidence.filterLabel" in i18n
    assert "evidence.expandFact" in i18n and "evidence.collapseFact" in i18n


def test_graph_toolbar_solver_chips_use_shared_dropdown():
    graph = (UI_ROOT / "components" / "GraphView.tsx").read_text()
    css = (UI_ROOT / "app" / "globals.css").read_text()
    i18n = (UI_ROOT / "lib" / "i18n.tsx").read_text()

    assert "workerDisplayName" in graph
    assert 'className="graph-toolbar-actions"' in graph
    assert 'className="graph-toolbar-meta"' in graph
    assert 'className="graph-tool-btn"' in graph
    assert "ChipFilterBar" in graph
    assert 'id="graph-solver-chips"' in graph
    assert 'className="graph-solver-filter"' in graph
    assert 'variant="floating"' in graph
    assert "solverRailRef" not in graph
    assert "scrollSolverRail" not in graph
    assert "graph.solversPrev" not in graph and "graph.solversNext" not in graph
    assert "scrollLeft += e.deltaY" not in graph
    assert 'className="legend-item"' in graph
    assert "{solverLabel(s)}" in graph
    assert "--graph-toolbar-h: 72px" in css
    assert "grid-template-rows: 28px 26px" in css
    assert "grid-template-columns: minmax(0, 1fr) minmax(0, 236px)" in css
    graph_toolbar_css = css[css.index(".graph-toolbar {"):css.index(".graph-toolbar-actions")]
    assert "top: 0" in graph_toolbar_css
    assert "bottom: 10px" not in graph_toolbar_css
    assert "border-bottom: 1px solid var(--line)" in graph_toolbar_css
    assert ".graph-canvas { position: absolute; inset: var(--graph-toolbar-h) 0 0; }" in css
    assert ".graph-toolbar-actions" in css
    assert ".graph-toolbar-meta" in css
    assert ".rail-scroll-btn" not in css
    assert ".graph-solver-rail" not in css
    assert ".graph-solvers " not in css
    assert ".graph-solver-filter" in css
    assert ".graph-solver-filter .chip-filter-panel" in css
    assert ".graph-toolbar .legend" in css
    assert "max-width: 82px" in css
    assert "graph.solversExpand" in i18n and "graph.solversCollapse" in i18n
    assert "graph.solversTitle" in i18n


def test_graph_view_does_not_late_load_cytoscape_chunks():
    graph = (UI_ROOT / "components" / "GraphView.tsx").read_text()

    assert 'import cytoscape' in graph
    assert 'await import("cytoscape")' not in graph
    assert 'await import("cytoscape-dagre")' not in graph


def test_conversation_top_badge_uses_paused_digest_state():
    convo = (UI_ROOT / "components" / "Conversation.tsx").read_text()
    i18n = (UI_ROOT / "lib" / "i18n.tsx").read_text()

    assert 'digest.phase === "paused"' in convo
    assert "runStateLabel" in convo
    assert '"convo.paused"' in i18n


def test_running_zero_worker_state_has_idle_copy():
    convo = (UI_ROOT / "components" / "Conversation.tsx").read_text()
    i18n = (UI_ROOT / "lib" / "i18n.tsx").read_text()

    assert "hero.detail.runningIdle" in convo
    assert '"hero.detail.runningIdle"' in i18n


def test_single_flag_solved_detail_does_not_prefix_flag_twice():
    i18n = (UI_ROOT / "lib" / "i18n.tsx").read_text()

    assert '"hero.detail.solved": { zh: "{flag}", en: "{flag}" }' in i18n
    assert '"common.copyFlagAria": { zh: "复制 {flag}", en: "Copy {flag}" }' in i18n


def test_favicon_route_exists():
    route = UI_ROOT / "app" / "favicon.ico" / "route.ts"

    assert route.exists()
    src = route.read_text()
    assert "image/svg+xml" in src
    assert "Cache-Control" in src


def test_blackboard_worker_chips_wrap_when_expanded():
    blackboard = (UI_ROOT / "components" / "Blackboard.tsx").read_text()
    css = (UI_ROOT / "app" / "globals.css").read_text()
    i18n = (UI_ROOT / "lib" / "i18n.tsx").read_text()

    assert "workerDisplayName" in blackboard
    assert 'className="bb-toolbar-main"' in blackboard
    assert "ChipFilterBar" in blackboard
    assert "workersOpen" not in blackboard
    assert 'className="bb-worker-toggle"' not in blackboard
    assert 'id="bb-worker-rail"' in blackboard
    assert 'variant="floating"' in blackboard
    assert 'className="bb-workers"' not in blackboard
    assert 'bb-worker-railbox' not in blackboard
    assert 'className="bb-worker-label"' in blackboard
    assert "workerDisplayName(w" in blackboard
    assert "grid-template-columns: auto minmax(0, 1fr)" in css
    assert ".bb-toolbar-main" in css
    assert ".bb-worker-filter" in css
    assert ".bb-worker-toggle" not in css
    assert ".bb-worker-railbox" not in css
    assert ".bb-worker-filter .chip-filter-panel" in css
    shared_filter_css = css[css.index(".chip-filter-strip"):css.index(".chip-filter-clear")]
    assert "flex-wrap: wrap" in shared_filter_css
    assert "overflow-y: auto" in shared_filter_css
    assert "-webkit-mask-image: none" in shared_filter_css
    assert "max-width: 96px" in css
    assert ".bb-worker-label" in css
    assert ".bb-flagchip" in css and "white-space: nowrap" in css[css.index(".bb-flagchip"):css.index(".bb-avatar")]
    assert "bb.workersExpand" in i18n and "bb.workersCollapse" in i18n
    assert "graph.solversPrev" not in i18n and "graph.solversNext" not in i18n


def test_worker_lane_detail_defaults_collapsed():
    component = (UI_ROOT / "components" / "WorkerLanes.tsx").read_text()
    assert "const [doneOpen, setDoneOpen] = useState(false);" in component
    assert 'className={`wlane-row is-${statusKind}' in component
    assert "onOpenSpeakerTimeline" in component
    assert 'aria-expanded={open}' in component


def test_status_hero_exposes_flow_popover():
    component = (UI_ROOT / "components" / "Conversation.tsx").read_text()
    assert "function FlowPopover" in component
    assert 'className="flow-popover"' in component
    assert 'aria-label={t("flow.open")}' in component
    assert "FLOW_STEPS" in component


def test_settings_moved_to_rail_and_header_toggles_theme():
    rail = (UI_ROOT / "components" / "ThreadRail.tsx").read_text()
    convo = (UI_ROOT / "components" / "Conversation.tsx").read_text()
    page = (UI_ROOT / "app" / "page.tsx").read_text()
    css = (UI_ROOT / "app" / "globals.css").read_text()

    assert "rail-settings-btn" in rail
    assert "rail-foot-state" in rail
    assert "onOpenSettings" in rail
    assert '<Icon name="gear" size={14} />' in rail
    assert '<span>{t("settings.open")}</span>' not in rail
    assert "onToggleTheme" in convo
    assert "theme.toDark" in convo and "theme.toLight" in convo
    assert "muteki.theme" in page
    assert ':root[data-theme="dark"]' in css


def test_run_inspector_is_full_height_resizable_side_rail():
    convo = (UI_ROOT / "components" / "Conversation.tsx").read_text()
    inspector = (UI_ROOT / "components" / "RunInspector.tsx").read_text()
    css = (UI_ROOT / "app" / "globals.css").read_text()

    assert "convo-body" in convo
    assert "convo-mainpane" in convo
    assert "inspector-shell" in convo
    assert "inspector-resizer" in convo
    assert "muteki.runInspector.width" in convo
    assert "sectionHeader" in inspector
    assert "collapsedSections" in inspector
    assert ".inspector-shell" in css
    assert ".convo.has-inspector .convo-mainpane" in css
    assert "var(--inspector-width" in css


def test_artifact_panel_has_resizable_width_handle():
    page = (UI_ROOT / "app" / "page.tsx").read_text()
    css = (UI_ROOT / "app" / "globals.css").read_text()
    i18n = (UI_ROOT / "lib" / "i18n.tsx").read_text()

    assert "muteki.artifact.width" in page
    assert "onArtifactResize" in page
    assert "artifact-resizer" in page
    assert "aria-valuenow={width}" in page
    assert "onResize(defaultWidth)" in page
    assert ".artifact-resizer" in css
    assert "body.artifact-resizing" in css
    assert "art.resizeCanvas" in i18n


# ---- RunMetaStore -----------------------------------------------------------

def test_meta_store_pin_rename_persist(tmp_path):
    store = RunMetaStore(root=tmp_path)
    store.set_pinned("run-1", True, now=123.0)
    store.set_name("run-1", "My Title")
    m = store.get("run-1")
    assert m["pinned"] is True and m["pinned_at"] == 123.0 and m["custom_name"] == "My Title"

    # a fresh store on the same dir reloads from disk
    store2 = RunMetaStore(root=tmp_path)
    m2 = store2.get("run-1")
    assert m2["pinned"] is True and m2["custom_name"] == "My Title"
    assert (tmp_path / "_rail_meta.json").exists()


def test_meta_store_archive_unpins(tmp_path):
    store = RunMetaStore(root=tmp_path)
    store.set_pinned("run-1", True, now=1.0)
    m = store.set_archived("run-1", True)
    assert m["archived"] is True
    assert m["pinned"] is False and m["pinned_at"] is None


def test_meta_store_forget_and_empty_collapse(tmp_path):
    store = RunMetaStore(root=tmp_path)
    store.set_pinned("run-1", True, now=1.0)
    store.set_pinned("run-1", False, now=1.0)  # unpin → row collapses to nothing
    assert store.get("run-1")["pinned"] is False
    store.set_name("run-2", "keep")
    store.forget("run-2")
    assert store.get("run-2")["custom_name"] is None


# ---- Run.status() state machine --------------------------------------------

def test_run_status_state_machine(tmp_path):
    mgr = RunManager(sessions_root=tmp_path)
    r = mgr.create("run-x")
    assert r.status() == "draft"
    r.started = True
    assert r.status() == "running"
    r.paused = True
    assert r.status() == "paused"
    r.paused = False
    r.finished = True
    assert r.status() == "finished"
    r.solved = True
    assert r.status() == "solved"


# ---- RunManager mutations + listing ----------------------------------------

def test_list_runs_started_only_and_archived_hidden(tmp_path):
    mgr = RunManager(sessions_root=tmp_path)
    draft = mgr.create("run-draft")  # never started
    live = mgr.create("run-live")
    live.started = True
    # draft (not started) is excluded
    ids = [r["run_id"] for r in mgr.list_runs()]
    assert ids == ["run-live"]
    # archive hides it by default, shows with include_archived
    mgr.set_archived("run-live", True)
    assert mgr.list_runs() == []
    assert [r["run_id"] for r in mgr.list_runs(include_archived=True)] == ["run-live"]


async def test_list_runs_activity_floats_recently_updated_run_to_top(tmp_path):
    # The rail sorts by latest activity (updated_at), newest first — the run a
    # worker most recently touched floats up so the operator sees the live one
    # first. (Pure creation-order buried an active run under finished ones when the
    # manager rehydrated from disk in a different order than the eval ran — the
    # "为啥没按最新时间排序" complaint.)
    mgr = RunManager(sessions_root=tmp_path)
    older = mgr.create("run-old")
    older.started = True
    newer = mgr.create("run-new")
    newer.started = True

    # before any activity, newest-created floats up (created_seq tiebreak)
    assert [r["run_id"] for r in mgr.list_runs()] == ["run-new", "run-old"]

    # the OLDER run gets fresh worker activity → its updated_at advances past
    # run-new's → it floats to the top.
    for i in range(3):
        await older.bus.emit(Event(
            event_type=EventType.REASONING_DELTA,
            run_id="run-old",
            ts=100.0 + i,
            payload={"text": f"old worker update {i}"},
        ))

    rows = mgr.list_runs()
    assert [r["run_id"] for r in rows] == ["run-old", "run-new"]
    assert rows[0]["updated_at"] == 102.0
    assert rows[0]["updated"] > rows[1]["updated"]


async def test_pin_rename_delete(tmp_path):
    mgr = RunManager(sessions_root=tmp_path)
    r = mgr.create("run-1")
    r.started = True
    assert mgr.set_pinned("run-1", True, now=9.0)
    assert mgr.runs["run-1"].summary()["pinned"] is True
    assert mgr.rename("run-1", "Hello")
    assert mgr.runs["run-1"].summary()["name"] == "Hello"
    # unknown run → False, no crash
    assert mgr.set_pinned("nope", True, now=1.0) is False

    assert await mgr.delete("run-1")
    assert "run-1" not in mgr.runs


async def test_delete_cancels_live_tasks_without_crashing(tmp_path):
    """Finding #6: delete() must cancel a LIVE swarm task AND a live standby_task, then
    AWAIT them before closing the bus — cancel-without-await was a use-after-free race
    (a cancelled coroutine still writing to a just-closed bus). The CancelledError the
    cancelled task raises must NOT propagate out of delete()."""
    mgr = RunManager(sessions_root=tmp_path)
    r = mgr.create("run-live")
    r.started = True

    started = asyncio.Event()

    async def _never_ends():
        started.set()
        await asyncio.sleep(3600)

    r.task = asyncio.create_task(_never_ends())
    r.standby_task = asyncio.create_task(asyncio.sleep(3600))
    await asyncio.wait_for(started.wait(), timeout=2)

    # must not raise, must actually cancel both
    assert await mgr.delete("run-live")
    assert "run-live" not in mgr.runs
    assert r.task.cancelled()
    assert r.standby_task.cancelled()


async def test_shutdown_cancels_task_and_standby(tmp_path):
    """Finding #6: shutdown() must cancel BOTH run.task and standby_task (the standby
    used to leak across a server restart). And it was never called from anywhere —
    the lifespan now invokes it; here we exercise the method directly."""
    mgr = RunManager(sessions_root=tmp_path)
    r = mgr.create("run-sd")
    r.started = True
    r.task = asyncio.create_task(asyncio.sleep(3600))
    r.standby_task = asyncio.create_task(asyncio.sleep(3600))
    await asyncio.sleep(0)  # let them schedule

    await mgr.shutdown()
    assert r.task.cancelled()
    assert r.standby_task.cancelled()


async def test_execution_generation_publishes_one_terminal_event(tmp_path):
    mgr = RunManager(sessions_root=tmp_path)

    async def driver(run):
        for reason in ("first", "duplicate"):
            await run.bus.emit(Event(
                event_type=EventType.RUN_FINISHED,
                run_id=run.run_id,
                payload={"solved": False, "reason": reason},
            ))

    run = await mgr.start("run-terminal-once", driver)
    await run.task
    events = [event async for event in run.store.replay(run.run_id)]
    terminals = [event for event in events
                 if event.event_type is EventType.RUN_FINISHED]

    assert len(terminals) == 1
    assert terminals[0].payload["reason"] == "first"
    assert terminals[0].payload["execution_generation"] == 1
    await mgr.shutdown()


async def test_terminal_generation_rejects_late_worker_events(tmp_path):
    mgr = RunManager(sessions_root=tmp_path)

    async def driver(run):
        await run.bus.emit(Event(
            event_type=EventType.RUN_FINISHED,
            run_id=run.run_id,
            payload={"solved": False, "reason": "operator_stop"},
        ))
        await run.bus.emit(Event(
            event_type=EventType.TOOL_CALL_RESULT,
            run_id=run.run_id,
            solver_id="late-worker",
            payload={"tool": "shell", "result": "late output"},
        ))

    run = await mgr.start("run-terminal-fence", driver)
    await run.task
    events = [event async for event in run.store.replay(run.run_id)]

    assert len([
        event for event in events
        if event.event_type is EventType.RUN_FINISHED
    ]) == 1
    assert not [
        event for event in events
        if event.event_type is EventType.TOOL_CALL_RESULT
        and event.solver_id == "late-worker"
    ]
    await mgr.shutdown()


async def test_operator_stop_fallback_uses_one_operator_terminal(
    tmp_path, monkeypatch,
):
    monkeypatch.setenv("MUTEKI_CONTROL_ACK_TIMEOUT", "0.01")
    monkeypatch.setenv("MUTEKI_CONTROL_CLAIM_TIMEOUT", "0.01")
    mgr = RunManager(sessions_root=tmp_path)
    started = asyncio.Event()

    async def driver(_run):
        started.set()
        await asyncio.sleep(3600)

    run = await mgr.start("run-stop-once", driver)
    await started.wait()
    result = await mgr.post_control(
        run.run_id, {"action": "stop", "target": "global"})
    assert result["command_id"]
    assert run.control_actor is not None
    await run.control_actor.join()
    await asyncio.gather(run.task, return_exceptions=True)

    events = [event async for event in run.store.replay(run.run_id)]
    terminals = [event for event in events
                 if event.event_type is EventType.RUN_FINISHED]
    assert len(terminals) == 1
    assert terminals[0].payload["reason"] == "operator_stop"
    assert terminals[0].payload["failure_phase"] == "runtime"
    assert terminals[0].payload["error_id"].startswith("RT-")
    await mgr.shutdown()


def test_rehydrate_from_jsonl_restores_started_runs(tmp_path):
    # write a minimal JSONL with a run.started so rehydration picks it up
    sess = tmp_path
    (sess / "run-0007.jsonl").write_text(
        json.dumps({
            "event_type": "run.started", "seq": 1, "ts": 1.0, "run_id": "run-0007",
            "payload": {"challenge": {"name": "rehydrated", "category": "crypto"}},
        }) + "\n"
    )
    mgr = RunManager(sessions_root=sess)
    ids = [r["run_id"] for r in mgr.list_runs()]
    assert "run-0007" in ids
    row = next(r for r in mgr.list_runs() if r["run_id"] == "run-0007")
    assert row["updated_at"] == 1.0
    # _seq advanced past 7 → next mint can't collide
    nxt = mgr.create_new()
    assert nxt.run_id != "run-0007"
    assert int(nxt.run_id.split("-")[1]) > 7


def test_rehydrate_adopts_run_titled(tmp_path):
    (tmp_path / "run-0003.jsonl").write_text(
        json.dumps({"event_type": "run.started", "seq": 1, "ts": 1.0, "run_id": "run-0003",
                    "payload": {"challenge": {}}}) + "\n"
        + json.dumps({"event_type": "run.titled", "seq": 2, "ts": 2.0, "run_id": "run-0003",
                      "payload": {"title": "Auto Title"}}) + "\n"
    )
    mgr = RunManager(sessions_root=tmp_path)
    row = next(r for r in mgr.list_runs() if r["run_id"] == "run-0003")
    assert row["name"] == "Auto Title"
    assert row["updated_at"] == 2.0


async def test_meta_sink_tracks_pause_resume(tmp_path):
    mgr = RunManager(sessions_root=tmp_path)
    run = mgr.create("run-p")
    run.started = True
    await run.bus.emit(Event(event_type=EventType.HITL_RESPONSE, run_id="run-p",
                             payload={"action": "pause", "status": "persisted"}))
    assert run.paused is False  # admission is not an observed effect
    await run.bus.emit(Event(event_type=EventType.CONTROL_COMMAND, run_id="run-p",
                             payload={"command_id": "C-p", "action": "pause",
                                      "status": "effect_observed",
                                      "effect": {"kind": "run_quiesced"}}))
    assert run.paused is True
    await run.bus.emit(Event(event_type=EventType.CONTROL_COMMAND, run_id="run-p",
                             payload={"command_id": "C-r", "action": "resume",
                                      "status": "routed"}))
    assert run.paused is True
    await run.bus.emit(Event(event_type=EventType.CONTROL_COMMAND, run_id="run-p",
                             payload={"command_id": "C-r", "action": "resume",
                                      "status": "effect_observed",
                                      "effect": {"kind": "run_resumed"}}))
    assert run.paused is False

    await run.bus.emit(Event(event_type=EventType.CONTROL_COMMAND, run_id="run-p",
                             payload={"command_id": "C-wf", "action": "freeze",
                                      "target": "solver:w-1",
                                      "status": "effect_observed",
                                      "effect": {"kind": "workers_frozen",
                                                 "targets": ["w-1"]}}))
    assert run.paused is False, "targeted worker freeze is not a run-wide hold"


async def test_meta_sink_resolves_only_addressed_help_request(tmp_path):
    mgr = RunManager(sessions_root=tmp_path)
    run = mgr.create("run-h")
    run.started = True
    await run.bus.emit(Event(event_type=EventType.HITL_REQUEST, run_id="run-h",
                             payload={"request_id": "H-a", "need": "token A"}))
    await run.bus.emit(Event(event_type=EventType.HITL_REQUEST, run_id="run-h",
                             payload={"request_id": "H-b", "need": "token B"}))
    assert run.awaiting_help is True
    await run.bus.emit(Event(event_type=EventType.HITL_RESPONSE, run_id="run-h",
                             payload={"action": "hint", "text": "try 8080"}))
    assert set(run.pending_help) == {"H-a", "H-b"}
    await run.bus.emit(Event(event_type=EventType.HITL_RESPONSE, run_id="run-h",
                             payload={"action": "answer_decision", "request_id": "H-a"}))
    assert set(run.pending_help) == {"H-a", "H-b"}
    await run.bus.emit(Event(event_type=EventType.CONTROL_COMMAND, run_id="run-h",
                             payload={"command_id": "C-a", "action": "answer_decision",
                                      "request_id": "H-a", "status": "unknown",
                                      "decision_closed": True}))
    assert set(run.pending_help) == {"H-a", "H-b"}
    await run.bus.emit(Event(event_type=EventType.CONTROL_COMMAND, run_id="run-h",
                             payload={"command_id": "C-a2", "action": "answer_decision",
                                      "request_id": "H-a", "status": "effect_observed",
                                      "decision_closed": True}))
    assert set(run.pending_help) == {"H-b"}
    assert run.awaiting_help is True
    await run.bus.emit(Event(event_type=EventType.HITL_RESPONSE, run_id="run-h",
                             payload={"action": "answer_decision", "request_id": "H-b"}))
    assert run.awaiting_help is True
    await run.bus.emit(Event(event_type=EventType.CONTROL_COMMAND, run_id="run-h",
                             payload={"command_id": "C-b", "action": "answer_decision",
                                      "request_id": "H-b", "status": "effect_observed",
                                      "decision_closed": True}))
    assert run.awaiting_help is False
    assert run.help_text == ""


async def test_meta_sink_for_handles_hitl(tmp_path):
    """Finding D: the standby/_fresh_bus copy `_meta_sink_for` had NO HITL branch at
    all, so during the standby phase a raised hand never showed and an answer never
    cleared. It must now mirror the inline sink."""
    mgr = RunManager(sessions_root=tmp_path)
    run = mgr.create("run-sb")
    run.started = True
    sink = mgr._meta_sink_for(run)
    await sink(Event(event_type=EventType.HITL_REQUEST, run_id="run-sb",
                     payload={"request_id": "H-vpn", "need": "vpn creds"}))
    assert run.awaiting_help is True
    await sink(Event(event_type=EventType.HITL_RESPONSE, run_id="run-sb",
                     payload={"action": "answer_decision", "request_id": "H-vpn"}))
    assert run.awaiting_help is True
    await sink(Event(event_type=EventType.CONTROL_COMMAND, run_id="run-sb",
                     payload={"command_id": "C-vpn", "action": "answer_decision",
                              "request_id": "H-vpn", "status": "effect_observed",
                              "decision_closed": True}))
    assert run.awaiting_help is False
    await sink(Event(event_type=EventType.CONTROL_COMMAND, run_id="run-sb",
                     payload={"command_id": "C-f", "action": "freeze",
                              "status": "effect_observed",
                              "effect": {"kind": "run_frozen"}}))
    assert run.paused is True
    await sink(Event(event_type=EventType.CONTROL_COMMAND, run_id="run-sb",
                     payload={"command_id": "C-t", "action": "thaw",
                              "status": "effect_observed",
                              "effect": {"kind": "run_thawed"}}))
    assert run.paused is False


async def test_meta_sink_reflects_awaiting_operator_pause(tmp_path):
    """run-11189 regression: when the swarm auto-pauses on a NEED_INPUT
    (awaiting_operator blackboard delta), the rail must show paused — not a spinner
    that looks like it's still churning. operator_resumed clears it."""
    mgr = RunManager(sessions_root=tmp_path)
    run = mgr.create("run-aw")
    run.started = True
    await run.bus.emit(Event(event_type=EventType.BLACKBOARD_DELTA, run_id="run-aw",
                             payload={"kind": "awaiting_operator",
                                      "reason": "need the L1 SSH password", "count": 1}))
    assert run.paused is True
    await run.bus.emit(Event(event_type=EventType.BLACKBOARD_DELTA, run_id="run-aw",
                             payload={"kind": "operator_resumed"}))
    assert run.paused is False


async def test_meta_sink_merges_mid_run_flag_found(tmp_path):
    """run-11189 regression: a flag recovered MID-run (collect mode keeps going) must
    show in run.flags immediately, not stay empty until RUN_FINISHED — otherwise the
    N/total counter reads 0 while the swarm has already banked a flag."""
    mgr = RunManager(sessions_root=tmp_path)
    run = mgr.create("run-mf")
    run.started = True
    run.multi_flag = True
    run.expected_flags = 15
    assert run.flags == []
    await run.bus.emit(Event(event_type=EventType.BLACKBOARD_DELTA, run_id="run-mf",
                             payload={"kind": "flag_found", "actor": "cli-claude",
                                      "flag": "bl_18c5039503f973296aa31bbd5ac4fef4"}))
    assert run.flags == ["bl_18c5039503f973296aa31bbd5ac4fef4"]
    # a second distinct flag accumulates (collect mode), deduped
    await run.bus.emit(Event(event_type=EventType.BLACKBOARD_DELTA, run_id="run-mf",
                             payload={"kind": "flag_found", "actor": "cli-codex",
                                      "flag": "bl_62c1be2414c0143a2da6b5b0982e12e7"}))
    await run.bus.emit(Event(event_type=EventType.BLACKBOARD_DELTA, run_id="run-mf",
                             payload={"kind": "flag_found", "actor": "cli-claude",
                                      "flag": "bl_18c5039503f973296aa31bbd5ac4fef4"}))  # dup
    assert run.flags == ["bl_18c5039503f973296aa31bbd5ac4fef4",
                         "bl_62c1be2414c0143a2da6b5b0982e12e7"]


# ---- web driver: attachments + offline-implies-no-kb ------------------------

async def test_swarm_driver_threads_attachments_and_offline_denies_kb(tmp_path, monkeypatch):
    from apps.web import drivers
    import muteki.swarm.swarm as sw

    src = tmp_path / "flag.enc"
    src.write_text("xx")
    missing = tmp_path / "ghost.txt"  # does NOT exist → filtered out

    captured = {}

    class FakeSwarm:
        def __init__(self, challenge, lineup, **kw):
            captured["challenge"] = challenge
            captured["web_access"] = kw.get("web_access")
            captured["kb"] = kw.get("kb")
            captured["worker_network"] = kw.get("worker_network")

        async def run(self):
            class O:
                flag = None
                solved = False
                winner = None
            return O()

    monkeypatch.setattr(drivers, "_resolve_swarm_class", lambda _spec: FakeSwarm)

    body = {
        "kind": "swarm", "offline": True,
        "challenge": {"name": "t", "category": "crypto", "description": "d",
                      "attachments": [str(src), str(missing)]},
        "worker_network": "bridge",
    }
    driver = drivers._swarm_driver(drivers._infer_challenge(body))

    class FakeBus:
        async def emit(self, *a, **k):
            pass

        async def close(self):
            pass

        def add_sink(self, *a):
            pass

    class FakeRun:
        run_id = "r1"
        bus = FakeBus()
        hitl = None
        worker_cmds = None
        cost = None
        flag = None

    await driver(FakeRun())

    ch = captured["challenge"]
    assert ch.attachments == [str(src)]          # missing path filtered out
    assert captured["web_access"] is False        # offline
    assert captured["kb"] is False                # offline implies no KB
    assert captured["worker_network"] == "none"


async def test_swarm_driver_online_keeps_kb(tmp_path, monkeypatch):
    from apps.web import drivers
    import muteki.swarm.swarm as sw

    captured = {}

    class FakeSwarm:
        def __init__(self, challenge, lineup, **kw):
            captured["web_access"] = kw.get("web_access")
            captured["kb"] = kw.get("kb")

        async def run(self):
            class O:
                flag = None
                solved = False
                winner = None
            return O()

    monkeypatch.setattr(drivers, "_resolve_swarm_class", lambda _spec: FakeSwarm)
    driver = drivers._swarm_driver(drivers._infer_challenge(
        {"kind": "swarm", "challenge": {"description": "an http web target"}}))

    class FakeBus:
        async def emit(self, *a, **k): pass
        async def close(self): pass
        def add_sink(self, *a): pass

    class FakeRun:
        run_id = "r2"; bus = FakeBus(); hitl = None; worker_cmds = None
        cost = None; flag = None

    await driver(FakeRun())
    assert captured["web_access"] is True
    assert captured["kb"] is True  # online default keeps KB


async def test_swarm_driver_threads_stage_policy_budgets_and_llm_profiles(tmp_path, monkeypatch):
    from apps.web import drivers
    import muteki.swarm.swarm as sw

    captured = {}

    class FakeSwarm:
        def __init__(self, challenge, lineup, **kw):
            captured.update(kw)
        async def run(self):
            class O:
                flag = None
            return O()

    monkeypatch.setattr(drivers, "_resolve_swarm_class", lambda _spec: FakeSwarm)
    body = {
        "kind": "swarm",
        "challenge": {"description": "solve"},
        "race_timeout": 111,
        "race_engines": ["claude-sub-container"],
        "wall_clock_budget": 222,
        "max_total_workers": 9,
        "cost_budget_usd": 0.75,
        "llm_profiles": {
            "planner": {"provider": "deepseek", "model": "planner-x"},
            "titler": {"provider": "deepseek", "model": "titler-x"},
        },
    }
    driver = drivers._swarm_driver(drivers._infer_challenge(body))

    class FakeBus:
        async def emit(self, *a, **k): pass
        async def close(self): pass
        def add_sink(self, *a): pass

    class FakeRun:
        run_id = "r-stage"; bus = FakeBus(); hitl = None; worker_cmds = None
        cost = None; flag = None

    await driver(FakeRun())
    assert captured["race_timeout"] == 111
    assert captured["race_engines"] == ["claude-sub-container"]
    assert captured["wall_clock_budget"] == 222
    assert captured["max_total_workers"] == 9
    assert captured["cost_budget_usd"] == 0.75
    assert captured["stage_policy"]["budgets"]["max_total_workers"] == 9
    assert captured["llm_profiles"]["planner"]["model"] == "planner-x"
    assert captured["reason_model"] == "planner-x"


async def test_swarm_driver_body_overrides_worker_config_stage_policy(tmp_path, monkeypatch):
    from apps.web import drivers
    import muteki.swarm.swarm as sw

    captured = {}

    class FakeSwarm:
        def __init__(self, challenge, lineup, **kw):
            captured.update(kw)
        async def run(self):
            class O:
                flag = None
            return O()

    monkeypatch.setattr(drivers, "_resolve_swarm_class", lambda _spec: FakeSwarm)
    async def ready(*, profiles, **_kwargs):
        return ({str(profile["id"]): True for profile in profiles}, [])
    monkeypatch.setattr(drivers, "_startup_readiness", ready)
    mgr = RunManager(sessions_root=tmp_path / "sessions")
    mgr.worker_config.resolve = lambda category: {
        "engines": ["claude"],
        "start_workers": 1,
        "stage_policy": {
            "race": {"enabled": True, "timeout": 720, "engines": ["claude"]},
            "coordinator": {"wall_clock_budget": 999},
            "budgets": {"max_total_workers": 42, "cost_budget_usd": 9.9},
        },
    }
    body = {
        "kind": "swarm",
        "challenge": {"description": "solve"},
        "race_timeout": 90,
        "wall_clock_budget": 0,
        "max_total_workers": 0,
        "cost_budget_usd": 0,
    }
    driver = drivers._swarm_driver(drivers._infer_challenge(body), mgr=mgr)

    class FakeBus:
        async def emit(self, *a, **k): pass
        async def close(self): pass
        def add_sink(self, *a): pass

    class FakeRun:
        run_id = "r-stage-override"; bus = FakeBus(); hitl = None; worker_cmds = None
        cost = None; flag = None; profile_readiness = {}

    await driver(FakeRun())
    policy = captured["stage_policy"]
    assert policy["race"]["timeout"] == 90
    assert policy["coordinator"]["wall_clock_budget"] == 0
    assert policy["budgets"]["max_total_workers"] == 0
    assert policy["budgets"]["cost_budget_usd"] == 0.0


async def test_swarm_driver_prechecks_only_selected_worker_profiles(tmp_path, monkeypatch):
    from apps.web import drivers
    import muteki.swarm.swarm as sw

    captured = {}

    class FakeSwarm:
        def __init__(self, challenge, lineup, **kw):
            captured["swarm_profiles"] = kw.get("worker_profiles")
        async def run(self):
            class O:
                flag = None
            return O()

    async def fake_readiness(*, profiles, worker_network, worker_backend,
                             sessions_root, cached_results):
        captured["precheck_profiles"] = profiles
        return ({str(profile["id"]): True for profile in profiles}, [])

    monkeypatch.setattr(drivers, "_resolve_swarm_class", lambda _spec: FakeSwarm)
    monkeypatch.setattr(drivers, "_startup_readiness", fake_readiness)
    mgr = RunManager(sessions_root=tmp_path / "sessions")
    mgr.worker_config.resolve = lambda category: {
        "engines": ["claude-local"],
        "start_workers": 1,
        "worker_backend": "local",
        "worker_profiles": [
            {"id": "claude-local", "name": "claude-local", "engine": "claude",
             "transport": "claude_code", "credential_mode": "subscription",
             "credential_account": "claude-main", "runtime": "local", "enabled": True},
            {"id": "cursor-api-local", "name": "cursor-api-local", "engine": "cursor",
             "transport": "cursor_agent", "credential_mode": "api_key",
             "credential_account": "cursor-main", "runtime": "local", "enabled": True},
        ],
    }
    driver = drivers._swarm_driver(
        drivers._infer_challenge({"kind": "swarm", "challenge": {"description": "solve"}}),
        mgr=mgr,
    )

    class FakeBus:
        async def emit(self, *a, **k): pass
        async def close(self): pass
        def add_sink(self, *a): pass

    class FakeRun:
        run_id = "r-precheck"; bus = FakeBus(); hitl = None; worker_cmds = None
        cost = None; flag = None; profile_readiness = {}

    await driver(FakeRun())
    assert [p["id"] for p in captured["precheck_profiles"]] == ["claude-local"]
    assert [p["id"] for p in captured["swarm_profiles"]] == [
        "claude-local", "cursor-api-local"]


async def test_swarm_driver_emits_preparing_and_structured_preflight_failure(
    tmp_path, monkeypatch,
):
    from apps.web import drivers

    async def failed_readiness(*, profiles, **_kwargs):
        profile_id = str(profiles[0]["id"])
        return ({profile_id: False}, [{
            "profile_id": profile_id,
            "engine": "claude",
            "model": "sonnet",
            "backend": "local",
            "runtime": "local",
            "layer": "auth",
            "code": "preflight_auth_failed",
            "detail": "Authentication required",
        }])

    monkeypatch.setattr(drivers, "_startup_readiness", failed_readiness)
    class MustNotStart:
        def __init__(self, *_args, **_kwargs):
            raise AssertionError("Swarm must not start after a failed preflight")

    monkeypatch.setattr(
        drivers, "_resolve_swarm_class", lambda _spec: MustNotStart)
    mgr = RunManager(sessions_root=tmp_path / "sessions")
    mgr.worker_config.resolve = lambda _category: {
        "engines": ["claude-local"],
        "start_workers": 1,
        "worker_backend": "local",
        "worker_profiles": [{
            "id": "claude-local", "name": "claude-local", "engine": "claude",
            "transport": "claude_code", "credential_mode": "subscription",
            "credential_account": "", "runtime": "local", "model": "sonnet",
            "enabled": True,
        }],
    }
    driver = drivers._swarm_driver(
        drivers._infer_challenge({"challenge": {"description": "solve"}}),
        mgr=mgr,
    )
    events = []

    class FakeBus:
        async def emit(self, event): events.append(event)

    class FakeRun:
        run_id = "r-preflight-failed"; bus = FakeBus()
        hitl = None; worker_cmds = None; cost = None; flag = None
        profile_readiness = {}

    await driver(FakeRun())

    assert [event.event_type for event in events] == [
        EventType.RUN_PREPARING, EventType.RUN_FINISHED]
    assert events[-1].payload["reason"] == "preflight_failed"
    assert events[-1].payload["profile_failures"][0]["profile_id"] == "claude-local"


async def test_swarm_driver_threads_expected_flags(tmp_path, monkeypatch):
    # run-10070 fix: the swarm driver dropped expected_flags, so a multi-flag
    # challenge always defaulted to 1. body.expected_flags (and challenge.*) must
    # reach the Challenge so a ladder run stops only after collecting ALL flags.
    from apps.web import drivers
    import muteki.swarm.swarm as sw

    cap = {}

    class FakeSwarm:
        def __init__(self, challenge, lineup, **kw):
            cap["expected_flags"] = challenge.expected_flags
            cap["flag_format"] = challenge.flag_format

        async def run(self):
            class O:
                flag = None; solved = False; winner = None
            return O()

    monkeypatch.setattr(drivers, "_resolve_swarm_class", lambda _spec: FakeSwarm)

    class FakeBus:
        async def emit(self, *a, **k): pass
        async def close(self): pass
        def add_sink(self, *a): pass

    class FakeRun:
        run_id = "r3"; bus = FakeBus(); hitl = None; worker_cmds = None
        cost = None; flag = None

    # body-level expected_flags + a token flag_format (the run-10070 shape)
    driver = drivers._swarm_driver(drivers._infer_challenge({
        "kind": "swarm", "expected_flags": 22,
        "challenge": {"description": "22-level ssh ladder", "flag_format": "token"}}))
    await driver(FakeRun())
    assert cap["expected_flags"] == 22
    assert cap["flag_format"] == "token"


async def test_swarm_driver_threads_custom_flag_wrapper_as_prompt_hint(tmp_path, monkeypatch):
    from apps.web import drivers
    import muteki.swarm.swarm as sw

    cap = {}

    class FakeSwarm:
        def __init__(self, challenge, lineup, **kw):
            cap["flag_format"] = challenge.flag_format
            cap["flag_format_hint"] = challenge.flag_format_hint

        async def run(self):
            class O:
                flag = None; solved = False; winner = None
            return O()

    monkeypatch.setattr(drivers, "_resolve_swarm_class", lambda _spec: FakeSwarm)

    class FakeBus:
        async def emit(self, *a, **k): pass
        async def close(self): pass
        def add_sink(self, *a): pass

    class FakeRun:
        run_id = "r-wrapper"; bus = FakeBus(); hitl = None; worker_cmds = None
        cost = None; flag = None

    driver = drivers._swarm_driver(drivers._infer_challenge({
        "kind": "swarm",
        "challenge": {
            "description": "custom wrapped flag",
            "flag_format_wrapper": "WMCTF{...}",
        },
    }))
    await driver(FakeRun())
    assert cap["flag_format_hint"] == "WMCTF{...}"
    assert cap["flag_format"] == r"[A-Za-z0-9_]{0,15}\{[^}]{1,200}\}"


async def test_swarm_driver_does_not_inject_flag_hint_without_wrapper(tmp_path, monkeypatch):
    from apps.web import drivers
    import muteki.swarm.swarm as sw

    cap = {}

    class FakeSwarm:
        def __init__(self, challenge, lineup, **kw):
            cap["flag_format_hint"] = challenge.flag_format_hint

        async def run(self):
            class O:
                flag = None; solved = False; winner = None
            return O()

    monkeypatch.setattr(drivers, "_resolve_swarm_class", lambda _spec: FakeSwarm)

    class FakeBus:
        async def emit(self, *a, **k): pass
        async def close(self): pass
        def add_sink(self, *a): pass

    class FakeRun:
        run_id = "r-no-wrapper"; bus = FakeBus(); hitl = None; worker_cmds = None
        cost = None; flag = None

    driver = drivers._swarm_driver(drivers._infer_challenge({
        "kind": "swarm",
        "challenge": {"description": "ordinary ctf"},
    }))
    await driver(FakeRun())
    assert cap["flag_format_hint"] == ""


async def test_swarm_driver_threads_multi_flag(tmp_path, monkeypatch):
    # v3: the multi_flag mode bit (collect vs single) must reach the Challenge so a
    # no-count collection run doesn't finish on the first flag.
    from apps.web import drivers
    import muteki.swarm.swarm as sw

    cap = {}

    class FakeSwarm:
        def __init__(self, challenge, lineup, **kw):
            cap["multi_flag"] = challenge.multi_flag

        async def run(self):
            class O:
                flag = None; solved = False; winner = None
            return O()

    monkeypatch.setattr(drivers, "_resolve_swarm_class", lambda _spec: FakeSwarm)

    class FakeBus:
        async def emit(self, *a, **k): pass
        async def close(self): pass
        def add_sink(self, *a): pass

    class FakeRun:
        run_id = "r4"; bus = FakeBus(); hitl = None; worker_cmds = None
        cost = None; flag = None

    # collect mode, no count → multi_flag=True must thread through
    driver = drivers._swarm_driver(drivers._infer_challenge({
        "kind": "swarm", "multi_flag": True,
        "challenge": {"description": "collect all the things"}}))
    await driver(FakeRun())
    assert cap["multi_flag"] is True

    # default: single-flag (multi_flag absent → False)
    cap.clear()
    driver2 = drivers._swarm_driver(drivers._infer_challenge({
        "kind": "swarm", "challenge": {"description": "normal ctf"}}))
    await driver2(FakeRun())
    assert cap["multi_flag"] is False


# ---- CliSolver attachment staging ------------------------------------------

def test_cli_solver_stages_attachments(tmp_path):
    from muteki.solver.cli_solver import CliSolver
    import json

    src = tmp_path / "src"
    src.mkdir()
    (src / "cipher.txt").write_text("deadbeef")
    (src / "RSA.py").write_text("# rsa")

    ch = Challenge(id="t", name="hybrid2", category="crypto", description="decrypt",
                   attachments=[str(src / "cipher.txt"), str(src / "RSA.py"),
                                str(src / "missing.bin")])
    s = CliSolver.__new__(CliSolver)
    s.challenge = ch
    s._staged_files = []
    s.kb = False

    wd = tmp_path / "wd"
    wd.mkdir()
    staged = s._stage_attachments(wd)
    assert sorted(staged) == ["RSA.py", "cipher.txt"]          # missing skipped
    assert (wd / "cipher.txt").read_text() == "deadbeef"       # read-through works
    assert sorted(p.name for p in wd.iterdir()) == ["RSA.py", "cipher.txt"]
    manifest = json.loads((tmp_path / "manifest.json").read_text())
    assert sorted(i["name"] for i in manifest["inputs"]) == ["RSA.py", "cipher.txt"]
    assert (tmp_path / "inputs" / "objects").exists()

    # the prompt lists the staged files so the agent inspects them first
    s._staged_files = staged
    prompt = s._build_prompt()
    assert "Attached files" in prompt and "cipher.txt" in prompt


def test_cli_solver_staging_symlinks_not_copies(tmp_path):
    """run-11190 disk-bloat fix: staging into N worker dirs must NOT make N physical
    copies of the input (a coordinator spawning hundreds of workers turned a 67 KB
    misions.json into 246 copies = 15 MB). Each worker gets a SYMLINK to the one
    canonical upload, so the bytes live on disk once. Read-through is unchanged."""
    from muteki.solver.cli_solver import CliSolver

    up = tmp_path / "uploads"
    up.mkdir()
    big = up / "misions.json"
    big.write_text("x" * 50_000)

    ch = Challenge(id="t", name="t", category="reverse", description="d",
                   flag_format="token", attachments=[str(big)])
    s = CliSolver.__new__(CliSolver)
    s.challenge = ch
    s._staged_files = []

    wds = []
    for i in range(5):
        wd = tmp_path / f"w{i}"
        wd.mkdir()
        names = s._stage_attachments(wd)
        assert names == ["misions.json"]
        link = wd / "misions.json"
        assert link.is_symlink(), "staged input must be a symlink, not a copy"
        assert link.read_text() == "x" * 50_000           # read-through intact
        assert "/inputs/objects/" in link.resolve().as_posix()
        wds.append(wd)

    # the 5 worker dirs together must NOT hold 5×50 KB of duplicated bytes
    real_bytes = sum(
        (wd / "misions.json").lstat().st_size for wd in wds)  # lstat = link size
    assert real_bytes < 50_000, "5 symlinks must be far smaller than one real copy"

    # re-staging an already-staged dir is idempotent (no error, no double-link)
    again = s._stage_attachments(wds[0])
    assert again == ["misions.json"]
    objects = list((tmp_path / "inputs" / "objects").glob("*/*/*"))
    assert len(objects) == 1
    assert objects[0].read_text() == "x" * 50_000


def test_cli_solver_container_staging_shares_one_copy(tmp_path):
    """Container-mode dedup fix: a symlink to the host upload dangles inside the
    worker container (only the run workspace is mounted), so we USED to copy the
    bytes into EVERY worker cwd — 17 copies / 64 MB of share.zip on one cryptopwn
    run, each worker re-unzipping from scratch. Fix: stage ONE copy at the shared
    mount root (worker_root, which IS the bind mount) and give each worker a
    RELATIVE symlink `../<name>` so it resolves the same on host and in-container.
    """
    from muteki.solver.cli_solver import CliSolver

    up = tmp_path / "uploads"
    up.mkdir()
    zip_in = up / "share.zip"
    zip_in.write_bytes(b"PK\x03\x04" + b"z" * 80_000)

    ch = Challenge(id="t", name="cryptopwn", category="pwn", description="d",
                   flag_format="token", attachments=[str(zip_in)])

    # worker_root lives under the run workspace; each worker cwd is worker_root/<id>/.
    workspace = tmp_path / "workspace"
    worker_root = workspace / "workers"
    worker_root.mkdir(parents=True)

    wds = []
    for i in range(5):
        s = CliSolver.__new__(CliSolver)
        s.challenge = ch
        s._staged_files = []
        s.container = object()  # truthy → container backend → shared-stage path
        wd = worker_root / f"cli-claude-{i}"
        wd.mkdir()
        names = s._stage_attachments(wd)
        assert names == ["share.zip"]
        link = wd / "share.zip"
        assert link.is_symlink(), "container-mode staged input must be a symlink"
        # RELATIVE link (so host abs path != container abs path both resolve)
        assert not link.readlink().is_absolute()
        assert "inputs/by-name/share.zip" in str(link.readlink())
        assert link.read_bytes() == zip_in.read_bytes()  # read-through intact
        wds.append(wd)

    # exactly ONE real copy of the bytes — at the input CAS object, not per worker
    objects = list((workspace / "inputs" / "objects").glob("*/*/*"))
    assert len(objects) == 1
    assert objects[0].read_bytes() == zip_in.read_bytes()
    # the 5 worker entries are links, so their on-disk bytes are negligible
    per_worker_bytes = sum((wd / "share.zip").lstat().st_size for wd in wds)
    assert per_worker_bytes < 80_000, "5 links must be far smaller than one real copy"
    # no leftover staging temp files at the root
    assert not list((workspace / "inputs" / "objects").glob("*/*/.share.zip.staging.*"))
