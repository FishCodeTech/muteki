/**
 * Draft-aware restore/reset for the shared runtime panel (`muteki.artifactOpen`).
 *
 * The panel open flag is session-scoped across /ctf and /pentest. Restoring it
 * onto a new draft hides the challenge input (Conversation only shows the
 * conversation/runtime toggle after a run has started).
 */

/** URL/active id with no concrete backend run (empty or `draft-…`). */
export function isDraftRunId(id: string): boolean {
  return !id || id.startsWith("draft-");
}

/**
 * Whether mounting should reopen the runtime panel from session/URL state.
 * Draft landings always start closed so cross-workspace stale state cannot hide
 * the input; concrete runs keep URL handoff and session restore.
 */
export function shouldRestoreRuntimePanelOpen(input: {
  concreteRunId: string;
  hasUrlRuntimeView: boolean;
  sessionOpen: boolean;
}): boolean {
  if (isDraftRunId(input.concreteRunId)) return false;
  if (input.hasUrlRuntimeView) return true;
  return input.sessionOpen;
}

/**
 * When the active id becomes a draft (route-direct draft entry, "+ New solve",
 * delete-current-run fallback), close the shared runtime/report panels.
 */
export function shouldResetRuntimePanelForRun(runId: string): boolean {
  return Boolean(runId) && runId.startsWith("draft-");
}
