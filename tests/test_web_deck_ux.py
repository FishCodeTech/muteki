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
    helper = UI_ROOT / "lib" / "workers.ts"
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
        vm.runInNewContext(out, sandbox, {{ filename: "workers.js" }});
        const lib = sandbox.module.exports;
        function assert(cond, msg) {{ if (!cond) throw new Error(msg); }}

        assert(lib.workerShortLabel("cli-cursor") === "cursor", "base cursor label");
        assert(lib.workerShortLabel("cli-cursor-2") === "cursor-2", "numbered cursor label");
        assert(lib.workerShortLabel("cli-claude-3") === "claude-3", "numbered claude label");
        assert(lib.workerShortLabel("cli-codex-10") === "codex-10", "two-digit codex label");
        assert(lib.workerShortLabel("reason") === "reason", "reason role label");
        """
    )
    _run_ui_node(script)

    activity = (UI_ROOT / "components" / "ActivityStream.tsx").read_text()
    lanes = (UI_ROOT / "components" / "WorkerLanes.tsx").read_text()
    css = (UI_ROOT / "app" / "globals.css").read_text()
    i18n = (UI_ROOT / "lib" / "i18n.tsx").read_text()
    chipbar = (UI_ROOT / "components" / "ChipFilterBar.tsx").read_text()
    assert "ChipFilterBar" in activity
    assert "ChipFilterBar" in lanes
    assert "useState(false)" in chipbar
    assert "aria-expanded={open}" in chipbar
    assert "className=\"chip-filter-toggle\"" in chipbar
    assert "className=\"chip-filter-panel\"" in chipbar
    assert "className=\"chip-filter-strip\"" in chipbar
    assert "filterOpen" not in activity
    assert "filterOpen" not in lanes
    assert 'id="activity-filter-chips"' in activity
    assert "filter-chipstrip" not in activity
    assert "workerShortLabel(key)" in activity
    assert "wlane-filterbar" in lanes
    assert 'id="wlane-filter-chips"' in lanes
    assert "wlane-filter-chiprow" not in lanes
    assert "filter-chipstrip" not in lanes
    assert "workerShortLabel(id)" in lanes
    assert "grid-template-columns: auto minmax(0, 1fr) auto" in css
    assert "--chip-filter-rows: 3" in css
    assert "max-height: calc((24px * var(--chip-filter-rows))" in css
    assert "overflow-y: auto" in css
    assert ".activity-filterbar" in css
    assert ".chip-filterbar" in css
    assert ".chip-filter-toggle" in css
    assert ".chip-filter-panel" in css
    assert ".chip-filter-strip" in css
    shared_filter_css = css[css.index(".chip-filter-strip"):css.index(".chip-filter-clear")]
    assert "flex-wrap: wrap" in shared_filter_css
    assert "-webkit-mask-image: none" in shared_filter_css
    assert ".wlane-filterbar" in css
    assert ".wlane-filter-chiprow" not in css
    assert "activity.filterExpand" in i18n and "activity.filterCollapse" in i18n
    assert "wlane.focusExpand" in i18n and "wlane.focusCollapse" in i18n
    assert "wlane.focusSelected" in i18n


def test_worker_identity_uses_transport_not_name_substrings():
    helper = UI_ROOT / "lib" / "workers.ts"
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
        vm.runInNewContext(out, sandbox, {{ filename: "workers.js" }});
        const lib = sandbox.module.exports;
        function assert(cond, msg) {{ if (!cond) throw new Error(msg); }}

        assert(lib.workerEngine("api-prod", "codex_cli") === "Codex", "codex transport label");
        assert(lib.workerEngine("not-a-claude-name", "claude_code") === "Claude Code", "claude transport label");
        assert(lib.workerEngine("plain-id", "cursor_agent") === "Cursor", "cursor transport label");
        assert(lib.resumeCommand("codex_cli", "s1") === "codex exec resume s1", "codex resume from transport");
        assert(lib.resumeCommand("cursor_agent", "s2") === "cursor-agent --resume s2", "cursor resume from transport");
        """
    )
    _run_ui_node(script)


def test_worker_settings_redesigned_ia():
    """Settings panel IA after the v2 redesign (left tab rail + right content) and
    the health self-check unification (per-profile readiness, single source of
    truth shared with the dispatch precheck).

    CONTRACT: the panel is organised by INTENT tabs — roster · accounts · runtime
    · budget · advanced — not the old flat profile table. Account readiness is
    PER PROFILE with a three honest states badge (not the old per-engine
    "an account exists ⇒ green" inference that let a run die on profile_unhealthy
    while the page showed green). This pins the new shape so a future edit can't
    silently regress it.
    """
    src = (UI_ROOT / "components" / "WorkerSettings.tsx").read_text()
    css = (UI_ROOT / "app" / "globals.css").read_text()
    i18n = (UI_ROOT / "lib" / "i18n.tsx").read_text()

    # ── intent tabs (the v2 left rail) ───────────────────────────────────────
    for tab in ("tabRoster", "tabAccounts", "tabRuntime", "tabBudget", "tabAdvanced"):
        assert f't("settings.{tab}")' in src, f"missing intent tab {tab}"

    # ── credential block CHANGES FACE by run environment ─────────────────────
    assert 'workerBackend === "container"' in src
    assert 't("settings.acctSub")' in src        # container: every engine needs an account
    assert 't("settings.credLocalNote")' in src  # local: system-login note
    # system-login status is consumed for local mode
    assert "getSystemLogin" in src and "sysLogin" in src

    # ── run environment = one container per run: backend + recipe ─────────────
    assert "putRuntimeEnvironment" in src
    assert "alignProfileRefs" in src
    assert "worker_backend: workerBackend" in src

    # ── reasoning models: base_url configurable, key NOT in the panel (.env) ──
    assert "llmBaseUrl" in src
    assert "base_url: llmBaseUrl" in src
    assert "testLlmEndpoint" in src

    # ── worker execution models: profile-level selector + real model probe ────
    assert "getWorkerModelOptions" in src
    assert "testWorkerProfileModel" in src
    assert "customModel" in src
    assert "runModelTest" in src
    assert "customModelOpen" in src
    assert 'if (v === "__custom__")' in src
    assert "setCustomModelOpen((cur) => ({ ...cur, [p.id]: true }))" in src
    assert 'updateProfile(p.id, { model: "__custom__" })' not in src
    assert "enabledDispatchRefs" in src
    assert "profileCapacity(workerProfiles)" in src
    assert "alignProfileRefs(enabledDispatchRefs(profilesToSave), profilesToSave, workerProfiles)" in src
    assert "profileCapacity(workerProfiles, engines)" not in src
    # Saving normalizes runtime first, then writes profiles — the runtime response
    # must not overwrite model edits already made in the modal.
    assert "currentById" in src
    assert "runtime: p.runtime" in src

    # ── HEALTH SELF-CHECK UNIFICATION (single source of truth) ────────────────
    # Readiness comes from the server's per-profile health, NOT a client-side
    # per-engine guess. The old engine-dimension inference is GONE.
    assert "fetchProfilesHealth" in src and "testProfileHealth" in src
    assert "profileHealth" in src
    assert "accountForEngine" not in src, "engine-dimension health inference must be gone"
    assert "missingEngines" not in src, "engine-dimension badge must be gone"
    # the account tab renders PER PROFILE (ws2-acct rows), with three honest states.
    # (profileAuthFailed takes an interpolated {layer} arg, so match the t(" prefix
    # rather than a bare closing paren.)
    assert "ws2-acct" in src
    for key in ("acctUnbound", "profileVerified", "profileUnverified", "profileAuthFailed"):
        assert f't("settings.{key}"' in src, f"missing three-state key {key}"
    # the badge has an explicit "bound but unverified" state (not a false green)
    assert '"settings.healthAcctUnverified"' in i18n

    # ── account test wired (the "测连通" button stays a button) ───────────────
    assert "testCredentialAccount" in src        # still used by the registration form
    assert 't("settings.testConn")' in src
    assert "saveAndTestAccount" in src
    assert 't("settings.saveAndTest")' in src
    assert '"settings.saveAndTest"' in i18n

    # ── codex one-click re-auth from the host ~/.codex (after `codex login`) ───
    assert "importHostCodexAuth" in src and "importCodexFromHost" in src
    assert 't("settings.importHostCodex")' in src
    assert '"settings.importHostCodex"' in i18n
    assert 'p.engine === "codex"' in src, "import-from-host must be gated to codex rows"

    # local-vs-container is explicit: tests run against the CURRENT run env and
    # say so (account test labels the backend; self-check probes the right env).
    assert "backendLabel" in src
    assert 't("settings.testsAgainst")' in src
    assert "getEngineHealth(workerBackend)" in src   # self-check follows backend
    assert 't("settings.selfcheckContainerNote")' in src
    assert '"settings.selfcheckContainerNote"' in i18n

    # the old flat 13-col profile table + dead subscription-mode columns are GONE
    assert 'className="ws-profile-table"' not in src
    assert "api_key_ref" not in src

    # i18n for the still-present notes
    assert '"settings.credContainerWarn"' in i18n
    assert '"settings.reasonKeyNote"' in i18n


def test_custom_endpoint_account_form_binds_to_agent():
    """A custom endpoint must declare WHICH agent it overrides — not leave it to an
    undiscoverable account-id naming convention. The form gains an agent selector
    that auto-aligns the id and is persisted as target_engine."""
    src = (UI_ROOT / "components" / "WorkerSettings.tsx").read_text()
    i18n = (UI_ROOT / "lib" / "i18n.tsx").read_text()

    # the selector only shows for the custom-endpoint type, beside base_url
    assert 't("settings.accountTargetEngine")' in src
    assert "accountApiEngine" in src
    assert "onAccountApiEngineChange" in src
    # id auto-aligns to the <engine>-main reference unless the operator customized it
    assert "isDefaultLikeAccountId" in src
    assert "-main`" in src
    # the chosen agent is persisted so inspect()/display can bind it (not orphan "api")
    assert "target_engine: accountApiEngine" in src
    # custom-endpoint accounts read back as registered for their agent + labelled
    assert 'acct?.mode === "custom_endpoint"' in src
    assert 't("settings.modeCustomEndpoint")' in src
    # i18n present
    assert '"settings.accountTargetEngine"' in i18n
    assert '"settings.modeCustomEndpoint"' in i18n


def test_run_inspector_renders_runtime_degraded_badge():
    src = (UI_ROOT / "components" / "RunInspector.tsx").read_text()
    i18n = (UI_ROOT / "lib" / "i18n.tsx").read_text()
    css = (UI_ROOT / "app" / "globals.css").read_text()
    assert 'e.kind === "runtime_degraded"' in src
    assert 'insp-runtime-degraded' in src
    assert '"insp.run.runtimeDegraded"' in i18n
    assert ".insp-runtime-degraded" in css


def test_worker_settings_save_keeps_race_roster_in_sync_with_visible_roster():
    """Saving the visible seat lineup must also update the race roster.

    The settings UI does not expose a separate race-only subset. If save only
    updates `engines`, an old `race_engines`/`stage_policy.race.engines` subset
    can survive on disk and silently drop codex from new runs even though the
    roster shows it enabled.
    """
    src = (UI_ROOT / "components" / "WorkerSettings.tsx").read_text()

    assert "race_engines: nextEngines" in src
    assert "race: { enabled: raceScout, timeout: raceTimeout, engines: nextEngines }" in src


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
    panel = (UI_ROOT / "components" / "ArtifactPanel.tsx").read_text()
    inspector = (UI_ROOT / "components" / "RunInspector.tsx").read_text()
    assert '"findings"' in panel
    assert '"credentials"' in panel
    assert '"pocs"' in panel
    assert '"routes"' in panel
    assert '"directives"' in panel
    assert "CredentialsPanel" in panel
    assert "ReviewFindingsPanel" in panel
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
        s = lib.reduce(s, {{ event_type: lib.EventType.HITL_RESPONSE, run_id: "run-pause", ts: 5,
          payload: {{ target: "global", action: "resume", delivery: "applied_live" }} }});
        assert(!s.awaitingOperator, "hitl resume clears pause");
        assert(lib.swarmDigest(s).phase !== "paused", "digest resumed from hitl response");
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

    assert "workerShortLabel" in graph
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

    assert "workerShortLabel" in blackboard
    assert 'className="bb-toolbar-main"' in blackboard
    assert "ChipFilterBar" in blackboard
    assert "workersOpen" not in blackboard
    assert 'className="bb-worker-toggle"' not in blackboard
    assert 'id="bb-worker-rail"' in blackboard
    assert 'variant="floating"' in blackboard
    assert 'className="bb-workers"' not in blackboard
    assert 'bb-worker-railbox' not in blackboard
    assert 'className="bb-worker-label"' in blackboard
    assert "workerShortLabel(w)" in blackboard
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
    assert "const [expandedLaneIds, setExpandedLaneIds] = useState<Set<string>>(new Set());" in component
    assert 'role="button"' in component
    assert "aria-expanded={isExpanded}" in component
    assert "{isExpanded && (" in component


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
    panel = (UI_ROOT / "components" / "ArtifactPanel.tsx").read_text()
    css = (UI_ROOT / "app" / "globals.css").read_text()
    i18n = (UI_ROOT / "lib" / "i18n.tsx").read_text()

    assert "muteki.artifact.width" in page
    assert "onArtifactResize" in page
    assert "artifact-resizer" in panel
    assert "aria-valuenow={width}" in panel
    assert "onResize(defaultWidth)" in panel
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
                             payload={"action": "pause"}))
    assert run.paused is True
    await run.bus.emit(Event(event_type=EventType.HITL_RESPONSE, run_id="run-p",
                             payload={"action": "resume"}))
    assert run.paused is False


async def test_meta_sink_clears_awaiting_help_on_response(tmp_path):
    """Finding D regression: a worker raises its hand (HITL_REQUEST → awaiting_help),
    then the operator answers with a plain hint (HITL_RESPONSE, action neither pause
    nor resume). The rail's awaiting_help MUST clear. The old _meta_sink had TWO
    `elif HITL_RESPONSE` arms — the second (which cleared the hand) was unreachable, so
    the rail showed '需要输入' forever even after the answer."""
    mgr = RunManager(sessions_root=tmp_path)
    run = mgr.create("run-h")
    run.started = True
    await run.bus.emit(Event(event_type=EventType.HITL_REQUEST, run_id="run-h",
                             payload={"need": "the dashboard token"}))
    assert run.awaiting_help is True
    assert run.help_text
    # a non-pause/resume answer must still lower the hand
    await run.bus.emit(Event(event_type=EventType.HITL_RESPONSE, run_id="run-h",
                             payload={"action": "hint", "text": "try 8080"}))
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
                     payload={"need": "vpn creds"}))
    assert run.awaiting_help is True
    await sink(Event(event_type=EventType.HITL_RESPONSE, run_id="run-sb",
                     payload={"action": "pause"}))
    assert run.paused is True
    assert run.awaiting_help is False  # response lowers the hand even while pausing
    await sink(Event(event_type=EventType.HITL_RESPONSE, run_id="run-sb",
                     payload={"action": "resume"}))
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
            captured["runtime_profiles"] = kw.get("runtime_profiles")

        async def run(self):
            class O:
                flag = None
                solved = False
                winner = None
            return O()

    monkeypatch.setattr(sw, "Swarm", FakeSwarm)

    body = {
        "kind": "swarm", "offline": True,
        "challenge": {"name": "t", "category": "crypto", "description": "d",
                      "attachments": [str(src), str(missing)]},
        "runtime_profiles": [{"id": "docker-web", "backend": "container", "network": "bridge"}],
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
    assert captured["runtime_profiles"][0]["network"] == "none"


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

    monkeypatch.setattr(sw, "Swarm", FakeSwarm)
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

    monkeypatch.setattr(sw, "Swarm", FakeSwarm)
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

    monkeypatch.setattr(sw, "Swarm", FakeSwarm)
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
        cost = None; flag = None

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

    def fake_missing(*, worker_profiles, runtime_profiles, sessions_root):
        captured["precheck_profiles"] = worker_profiles
        return []

    monkeypatch.setattr(sw, "Swarm", FakeSwarm)
    monkeypatch.setattr(drivers, "_missing_profile_accounts", fake_missing)
    mgr = RunManager(sessions_root=tmp_path / "sessions")
    mgr.worker_config.resolve = lambda category: {
        "engines": ["claude-local"],
        "start_workers": 1,
        "worker_backend": "local",
        "runtime_profiles": [{"id": "local", "backend": "local"}],
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
        cost = None; flag = None

    await driver(FakeRun())
    assert [p["id"] for p in captured["precheck_profiles"]] == ["claude-local"]
    assert [p["id"] for p in captured["swarm_profiles"]] == [
        "claude-local", "cursor-api-local"]


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

    monkeypatch.setattr(sw, "Swarm", FakeSwarm)

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

    monkeypatch.setattr(sw, "Swarm", FakeSwarm)

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

    monkeypatch.setattr(sw, "Swarm", FakeSwarm)

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

    monkeypatch.setattr(sw, "Swarm", FakeSwarm)

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
