
export type ForkSourceFreeze = {
  threadId: string;
  title: string;
  projectName?: string;
  rootPath?: string;
};

export function resolveForkTurnId(args: {
  sourceThreadId: string;
  sourceTurnId?: string | null;
  currentThreadId?: string | null;
  currentLastTurnId?: string | null;
  sourceLastTurnId?: string | null;
}): string {
  const explicit = String(args.sourceTurnId || "").trim();
  if (explicit) return explicit;
  if (
    args.sourceThreadId
    && args.currentThreadId
    && args.sourceThreadId === args.currentThreadId
  ) {
    return String(args.currentLastTurnId || "").trim();
  }
  return String(args.sourceLastTurnId || "").trim();
}

export function freezeForkSource(args: {
  sourceThreadId: string;
  threads: Array<{
    thread_id: string;
    title?: string | null;
    project_id?: string | null;
  }>;
  projects: Array<{
    project_id: string;
    name?: string | null;
    root_path?: string | null;
  }>;
  fallbackTitle?: string | null;
  fallbackProjectId?: string | null;
  fallbackRootPath?: string | null;
  fallbackProjectName?: string | null;
}): ForkSourceFreeze {
  const row = args.threads.find((t) => t.thread_id === args.sourceThreadId);
  const projectId = row?.project_id || args.fallbackProjectId || "";
  const project = projectId
    ? args.projects.find((p) => p.project_id === projectId)
    : undefined;
  return {
    threadId: args.sourceThreadId,
    title: String(row?.title || args.fallbackTitle || "未命名对话").trim()
      || "未命名对话",
    projectName: (project?.name || args.fallbackProjectName)
      ? String(project?.name || args.fallbackProjectName)
      : undefined,
    rootPath: (project?.root_path || args.fallbackRootPath)
      ? String(project?.root_path || args.fallbackRootPath)
      : undefined,
  };
}

export function impactCommandThreadId(args: {
  mode: string;
  currentThreadId?: string | null;
  frozenSourceThreadId?: string | null;
}): string {
  if (String(args.mode) === "fork") {
    return String(args.frozenSourceThreadId || args.currentThreadId || "").trim();
  }
  return String(args.currentThreadId || "").trim();
}
