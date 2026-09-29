/**
 * #18 — draft landings must not inherit shared runtime panel open state.
 * Run: node scripts/check_draft_runtime_panel.mjs (Node 22.18+).
 */
import assert from "node:assert/strict";
import {
  isDraftRunId,
  shouldResetRuntimePanelForRun,
  shouldRestoreRuntimePanelOpen,
} from "../apps/web/ui/lib/draftRuntimePanel.ts";

assert.equal(isDraftRunId(""), true);
assert.equal(isDraftRunId("draft-abc"), true);
assert.equal(isDraftRunId("run-123"), false);

// Cross-workspace: stale sessionOpen must not open panel on /ctf or /pentest draft
assert.equal(
  shouldRestoreRuntimePanelOpen({
    concreteRunId: "",
    hasUrlRuntimeView: false,
    sessionOpen: true,
  }),
  false,
  "empty URL run id (new draft) ignores sessionOpen",
);
assert.equal(
  shouldRestoreRuntimePanelOpen({
    concreteRunId: "draft-xyz",
    hasUrlRuntimeView: false,
    sessionOpen: true,
  }),
  false,
  "explicit draft id ignores sessionOpen",
);
assert.equal(
  shouldRestoreRuntimePanelOpen({
    concreteRunId: "",
    hasUrlRuntimeView: true,
    sessionOpen: true,
  }),
  false,
  "draft landing ignores URL view handoff too",
);

// Existing task restore still works
assert.equal(
  shouldRestoreRuntimePanelOpen({
    concreteRunId: "run-abc",
    hasUrlRuntimeView: false,
    sessionOpen: true,
  }),
  true,
  "concrete run restores sessionOpen",
);
assert.equal(
  shouldRestoreRuntimePanelOpen({
    concreteRunId: "run-abc",
    hasUrlRuntimeView: true,
    sessionOpen: false,
  }),
  true,
  "concrete run honors URL runtime view",
);
assert.equal(
  shouldRestoreRuntimePanelOpen({
    concreteRunId: "run-abc",
    hasUrlRuntimeView: false,
    sessionOpen: false,
  }),
  false,
  "concrete run stays closed when session was closed",
);

// Draft id detection resets panel (route-direct entry / onNewSolve parity)
assert.equal(shouldResetRuntimePanelForRun(""), false, "hydration placeholder does not reset");
assert.equal(shouldResetRuntimePanelForRun("draft-new"), true);
assert.equal(shouldResetRuntimePanelForRun("run-abc"), false);

console.log("PASS draft-runtime-panel");
