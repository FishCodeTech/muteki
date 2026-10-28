"use client";

import { useCallback, useEffect, useMemo, useRef, type ComponentType, type ReactNode } from "react";
import { useParams, usePathname, useRouter, useSearchParams } from "next/navigation";
import { ConversationShell, type ConversationExperience } from "@/components/conversation/ConversationShell";
import type { ComposerCustomDrop, ComposerLocalMentionItem, ComposerLocalMentions } from "@/components/ai-native/prompt-bar";
import type { PanelExtensionConfig } from "@/components/chat/panel/extensions";
import { chatPanel, useChatPanel } from "@/lib/chatPanelStore";
import { FindingsWorkspace } from "./ClueBoard";
import { StudioComposerControls, StudioContextTray, StudioHeaderActions, StudioTurnAccessory } from "./ConversationParts";
import { EVIDENCE_KIND_LABELS, submissionStatusLabel, type StudioRef, type StudioRefKind } from "./model";
import {
  STUDIO_BASE,
  STUDIO_PHONE_BREAKPOINT,
  STUDIO_SHEET_BREAKPOINT,
  STUDIO_WORKSPACE,
  WORKSPACE_ORDER,
  WORKSPACES,
  applyWorkspaceQuery,
  applyWorkspaceUrl,
  chatHref,
  openWorkspaceByDefault,
  orderWorkspaceTabs,
  showWorkspace,
  studioComposer,
  studioRouter,
  toggleWorkspace,
  visibleWorkspace,
  workspaceQuery,
  type WorkspaceKey,
} from "./nav";
import { REF_ICON, REF_KIND_LABEL, STUDIO_REF_MIME, citeRef, parseDroppedRef } from "./parts";
import { mergeOf, RUN_STATE_LABEL } from "./records";
import { StudioPersistenceGate } from "./SaveStatus";
import { LibraryWorkspace } from "./SourceViews";
import {
  DEFAULT_THREAD_ID,
  effectiveContext,
  previewThread,
  previewView,
  readStudioState,
  refLabel,
  studio,
  useStudioState,
  type StudioState,
} from "./store";
import { TasksWorkspace } from "./TaskBoard";
import { ReportsWorkspace } from "./Workbench";

const draftThreads = new Map<string, string>();

function newThreadHref(draftId: string): string {
  return `${STUDIO_BASE}?draft=${encodeURIComponent(draftId)}`;
}

const WORKSPACE_VIEW: Record<WorkspaceKey, ComponentType<{ active: boolean; sheet?: boolean }>> = {
  library: LibraryWorkspace,
  findings: FindingsWorkspace,
  tasks: TasksWorkspace,
  reports: ReportsWorkspace,
};

const PANEL_EXTENSIONS: PanelExtensionConfig = {
  exclusive: true,
  focusLayout: "dock",
  widthRatio: { initial: 0.52, mainMin: 420, panelMin: 420 },
  sheetBreakpoint: STUDIO_SHEET_BREAKPOINT,
  sheetWidth: (viewport) => (viewport < STUDIO_PHONE_BREAKPOINT ? "100vw" : "92vw"),
  launcherTitle: "SRC 工作区",
  launcherDescription: "资产库、发现与证据、任务与执行、报告与交付就在对话旁边，正在看的内容会自动带进下一条消息。",
  extensions: WORKSPACE_ORDER.map((key) => {
    const View = WORKSPACE_VIEW[key];
    return {
      key,
      label: WORKSPACES[key].label,
      icon: WORKSPACES[key].icon,
      description: WORKSPACES[key].description,
      shortcut: WORKSPACES[key].shortcut,
      render: ({ active, sheet }) => <View active={active} sheet={sheet} />,
    };
  }),
};

const SHORTCUT_CODES: Record<string, WorkspaceKey> = {
  Digit1: "tasks",
  Digit2: "library",
  Digit3: "findings",
  Digit4: "reports",
};

const MENTION_LIMIT = 4;

/** Offers @ mentions across all 10 top-level StudioRefKind object categories! */
function mentionItems(current: StudioState, threadId: string, query: string): ComposerLocalMentionItem[] {
  const q = query.trim().toLowerCase();
  const projectId = current.threads[threadId]?.projectId;
  const hit = (...fields: Array<string | undefined>) =>
    !q || fields.some((field) => (field ?? "").toLowerCase().includes(q));
  const rank = <T extends { projectId: string }>(items: T[]) =>
    [...items].sort((a, b) => Number(b.projectId === projectId) - Number(a.projectId === projectId));
  const item = (kind: Exclude<StudioRefKind, "passage">, id: string, description: string): ComposerLocalMentionItem => ({
    id: `${kind}:${id}`,
    label: refLabel(current, { kind, id }),
    description,
    section: REF_KIND_LABEL[kind],
    icon: REF_ICON[kind],
  });
  const projectTitle = (id: string) => current.projects.find((project) => project.id === id)?.title ?? "";
  const listed = (kind: "source" | "clue", id: string) => !mergeOf(current.merges, kind, id);

  return [
    ...rank(current.sources.filter((source) => listed("source", source.id) && hit(source.title, source.author, source.canonicalKey))).slice(0, MENTION_LIMIT)
      .map((source) => item("source", source.id, `${source.author} · ${projectTitle(source.projectId)}`)),
    ...rank(current.clues.filter((clue) => listed("clue", clue.id) && hit(clue.title, clue.quote, ...clue.tags))).slice(0, MENTION_LIMIT)
      .map((clue) => item("clue", clue.id, clue.quote)),
    ...rank(current.reports.filter((rep) => hit(rep.id, rep.title, rep.findingId))).slice(0, MENTION_LIMIT)
      .map((rep) => item("report", rep.id, `v${rep.version} · ${rep.readiness} · ${submissionStatusLabel(rep.submission.status)}`)),
    ...rank(current.evidences.filter((ev) => hit(ev.id, ev.title, ev.actorRef))).slice(0, MENTION_LIMIT)
      .map((ev) => item("evidence", ev.id, `${EVIDENCE_KIND_LABELS[ev.kind]} · ${{ complete: "完整", partial: "截断", revoked: "已撤回", unavailable: "缺失" }[ev.completeness] ?? ev.completeness}`)),
    ...rank(current.imports.filter((imp) => hit(imp.id, imp.fileName))).slice(0, MENTION_LIMIT)
      .map((imp) => item("import", imp.id, `${imp.items.length} 条记录 · ${imp.format.toUpperCase()}`)),
    ...rank(current.workRuns.filter((run) => hit(run.id, run.title, run.targetUrl, run.question))).slice(0, MENTION_LIMIT)
      .map((run) => item("run", run.id, `G${run.generation} · ${RUN_STATE_LABEL[run.state]} · ${run.targetUrl}`)),
    ...rank(current.changes.filter((chg) => hit(chg.id, chg.title, chg.baselineCycleId))).slice(0, MENTION_LIMIT)
      .map((chg) => item("change", chg.id, `基线 ${chg.baselineCycleId} · ${chg.observedAt}`)),
    ...rank(current.feedbacks.filter((fb) => hit(fb.id, fb.externalTicketId, fb.platformAuthor, fb.rawText))).slice(0, MENTION_LIMIT)
      .map((fb) => item("feedback", fb.id, `${fb.externalTicketId} · ${fb.rawText.slice(0, 48)}…`)),
    ...rank(current.planVersions.filter((pl) => hit(pl.id, pl.focusQuestion, pl.revisionBasis))).slice(0, MENTION_LIMIT)
      .map((pl) => item("plan", pl.id, `v${pl.version} · ${pl.focusQuestion}`)),
    ...rank(current.tasks.filter((task) => hit(task.title, task.assignee))).slice(0, MENTION_LIMIT)
      .map((task) => item("task", task.id, `${task.assignee} · ${task.due}`)),
  ];
}

const MENTION_KINDS = new Set<StudioRefKind>([
  "source",
  "clue",
  "task",
  "report",
  "evidence",
  "import",
  "run",
  "change",
  "feedback",
  "plan",
]);

function parseMentionId(id: string): StudioRef | null {
  const split = id.indexOf(":");
  if (split < 0) return null;
  const kind = id.slice(0, split) as StudioRefKind;
  if (!MENTION_KINDS.has(kind)) return null;
  return { kind, id: id.slice(split + 1) };
}

function carryWorkspaces(fromThread: string, toThread: string) {
  const from = chatPanel.thread(fromThread);
  const to = chatPanel.thread(toThread);
  const activeSurface = from.surfaces.find((surface) => surface.id === from.activeId);
  if (from.isOpen && activeSurface?.kind === "extension") {
    from.surfaces
      .filter((surface) => surface.kind === "extension" && !to.surfaces.some((item) => item.id === surface.id))
      .forEach((surface) => chatPanel.open(toThread, surface));
    chatPanel.open(toThread, activeSurface);
    chatPanel.setMaximized(toThread, from.maximized);
  } else if (!from.isOpen && to.isOpen) {
    chatPanel.close(toThread);
  }
}

/**
 * Workbench accessories shared by the demo host and the real-task workbench
 * host. The caller composes the rest of `ConversationExperience` (routing,
 * preview, sidebar labels) per host.
 */
export function useStudioWorkbenchExperience(threadId: string): Pick<
  ConversationExperience,
  "headerActions" | "composerAccessory" | "panelExtensions" | "turnAccessory" |
  "localMentions" | "composerDrop" | "composerControls" | "composerPlaceholder" | "composerHandle"
> {
  const state = useStudioState();
  const localMentions = useMemo<ComposerLocalMentions>(() => ({
    title: "带上对话（支持全部研究对象）",
    scopeLabel: "资产 · 发现 · 报告 · 证据 · 批次 · 工作记录 · 变化 · 反馈 · 计划 · 任务",
    emptyLabel: "没有匹配的资产、发现、报告、证据、批次、工作记录、变化或反馈",
    search: (query) => mentionItems(readStudioState(), threadId, query),
    onSelect: (item) => {
      const ref = parseMentionId(item.id);
      if (ref) studio.addContext(threadId, ref);
    },
  }), [threadId]);

  const composerDrop = useMemo<ComposerCustomDrop>(() => ({
    types: [STUDIO_REF_MIME],
    label: "释放以引用到对话",
    onDrop: (data) => {
      const ref = parseDroppedRef(data);
      if (ref) citeRef(ref);
    },
  }), []);

  const turnAccessory = useCallback((turnId: string) => <StudioTurnAccessory threadId={threadId} turnId={turnId} />, [threadId]);

  return useMemo(() => ({
    headerActions: <StudioHeaderActions />,
    composerAccessory: <StudioContextTray threadId={threadId} />,
    panelExtensions: PANEL_EXTENSIONS,
    turnAccessory,
    localMentions,
    composerDrop,
    composerControls: <StudioComposerControls />,
    composerPlaceholder: state.viewing
      ? `关于「${refLabel(state, state.viewing).slice(0, 24)}」问点什么…`
      : "问点什么，@ 引用资产、发现或报告",
    composerHandle: studioComposer,
  }), [composerDrop, localMentions, state, threadId, turnAccessory]);
}

/**
 * Applies `?ws=` deep links (taking precedence over persisted UI state), keeps
 * the URL in sync with the workspace panel, binds the studio router and the
 * mod+alt workspace shortcuts. Shared by both workbench hosts.
 */
export function useStudioWorkspaceUrlSync(
  threadId: string,
  ready: boolean,
  router: { push: (href: string) => void; replace: (href: string) => void },
  searchKey: string,
) {
  const panel = useChatPanel(threadId);
  const state = useStudioState();
  const { library, findings, tasks, reports } = state.workspace;
  const restored = useRef(false);
  useEffect(() => {
    if (!ready || restored.current) return;
    restored.current = true;
    if (!applyWorkspaceQuery(threadId, new URLSearchParams(window.location.search))) openWorkspaceByDefault(threadId);
    orderWorkspaceTabs(threadId);
  }, [ready, threadId]);

  useEffect(() => {
    if (!ready || !restored.current) return;
    const current = new URLSearchParams(window.location.search);
    const next = workspaceQuery(readStudioState(), threadId, current).toString();
    if (next === current.toString()) return;
    window.history.replaceState(null, "", `${window.location.pathname}${next ? `?${next}` : ""}`);
  }, [
    ready, threadId, searchKey,
    panel.isOpen, panel.activeId, panel.maximized,
    library.sourceId, library.passageId, library.clueId, library.importsOpen, library.importBatchId, library.importFocusLine,
    findings.selectedId, findings.view, findings.evidenceId,
    tasks.highlightId, tasks.tab, tasks.selectedRunId, tasks.selectedPlanId, tasks.selectedChangeId,
    reports.tab, reports.selectedReportId, reports.selectedFeedbackId,
  ]);

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      const key = SHORTCUT_CODES[event.code];
      if (!key || !event.altKey || !(event.metaKey || event.ctrlKey) || event.shiftKey || event.isComposing) return;
      event.preventDefault();
      toggleWorkspace(key);
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, []);

  useEffect(() => {
    studioRouter.current = router;
    return () => { studioRouter.current = null; };
  }, [router]);
}

function StudioChatRoute() {
  const params = useParams<{ id?: string | string[] }>();
  const rawId = Array.isArray(params?.id) ? params.id[0] : params?.id;
  const routeId = rawId ? decodeURIComponent(rawId) : "";
  const router = useRouter();
  const pathname = usePathname();
  const search = useSearchParams();
  const draft = search.get("draft") || "";
  const state = useStudioState();

  const threadId = state.threads[routeId]
    ? routeId
    : state.threads[state.activeThreadId] ? state.activeThreadId : DEFAULT_THREAD_ID;
  const record = state.threads[threadId];

  useEffect(() => {
    if (draft) {
      let created = draftThreads.get(draft);
      if (!created) {
        const current = readStudioState();
        created = studio.createThread(current.threads[current.activeThreadId]?.projectId ?? current.projects[0]?.id ?? "");
        draftThreads.set(draft, created);
      }
      router.replace(chatHref(created));
      return;
    }
    if (pathname.startsWith(STUDIO_WORKSPACE)) {
      const key = applyWorkspaceUrl(pathname, new URLSearchParams(search.toString()));
      if (key) showWorkspace(threadId, key);
      router.replace(chatHref(threadId));
      return;
    }
    if (routeId !== threadId) router.replace(`${chatHref(threadId)}${window.location.search}`);
  }, [draft, pathname, routeId, router, search, threadId]);

  const previousThread = useRef("");
  useEffect(() => {
    const previous = previousThread.current;
    previousThread.current = threadId;
    studio.setActiveThread(threadId);
    if (previous && previous !== threadId) carryWorkspaces(previous, threadId);
  }, [threadId]);

  const settled = !draft && !pathname.startsWith(STUDIO_WORKSPACE) && routeId === threadId;
  useStudioWorkspaceUrlSync(threadId, settled, router, search.toString());

  const navigation = useMemo(() => ({
    router,
    pathname,
    searchParams: new URLSearchParams(search.toString()),
  }), [router, pathname, search]);

  const threads = useMemo(
    () => state.threadOrder.flatMap((id) => (state.threads[id] ? [previewThread(state.threads[id])] : [])),
    [state.threadOrder, state.threads],
  );
  const view = useMemo(() => previewView(record), [record]);
  const onSend = useCallback((text: string) => {
    const current = readStudioState();
    const refs = effectiveContext(current, threadId, visibleWorkspace(threadId));
    studio.send(threadId, text, refs, { taskId: current.handoff[threadId] });
  }, [threadId]);
  const preview = useMemo(() => ({ threads, view, notice: "", onSend }), [threads, view, onSend]);

  const workbench = useStudioWorkbenchExperience(threadId);
  const experience = useMemo<ConversationExperience>(() => ({
    defaultMode: "conversation",
    draftScope: "studio-prototype",
    hideProjects: true,
    allowFork: false,
    threadHref: chatHref,
    newThreadHref,
    sidebarLabels: { title: "研究对话", newThread: "新研究对话", search: "搜索研究对话", thread: "研究对话" },
    ...workbench,
    preview,
  }), [preview, workbench]);

  return (
    <div className="flex h-full min-h-0 w-full overflow-hidden" data-testid="studio-prototype">
      <div className="relative flex min-w-0 flex-1">
        <ConversationShell threadId={threadId} navigation={navigation} experience={experience} />
      </div>
    </div>
  );
}

export function StudioPrototypeRoute({ children }: { children: ReactNode }) {
  return (
    <>
      <StudioPersistenceGate>
        <StudioChatRoute />
      </StudioPersistenceGate>
      {children}
    </>
  );
}
