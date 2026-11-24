
export function isDraftRunId(id: string): boolean {
  return !id || id.startsWith("draft-");
}

export function shouldRestoreRuntimePanelOpen(input: {
  concreteRunId: string;
  hasUrlRuntimeView: boolean;
  sessionOpen: boolean;
}): boolean {
  if (isDraftRunId(input.concreteRunId)) return false;
  if (input.hasUrlRuntimeView) return true;
  return input.sessionOpen;
}

export function shouldResetRuntimePanelForRun(runId: string): boolean {
  return Boolean(runId) && runId.startsWith("draft-");
}
