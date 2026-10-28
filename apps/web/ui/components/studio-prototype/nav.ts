"use client";

import type { MutableRefObject } from "react";
import type { IconName } from "@/components/Icon";
import type { ConversationComposerHandle } from "@/components/conversation/ConversationShell";
import { extensionSurface } from "@/components/chat/panel/extensions";
import { chatPanel } from "@/lib/chatPanelStore";
import type { StudioRef } from "./model";
import type { FindingsUi } from "./store";
import { readStudioState, refTarget, replyMessageId, studio, type StudioState, type TasksSubTab, type VisibleWorkspace } from "./store";

/**
 * The research conversation is always on screen. The four workspaces are
 * fixed tabs of one panel beside it, so moving between material and the
 * conversation never changes pages. The URL query records the open workspace
 * and object, so a copied address restores the view.
 */

export const STUDIO_BASE = "/studio-prototype";
export const STUDIO_WORKSPACE = `${STUDIO_BASE}/workspace`;
/** At or below this width the workspace is a sheet over the conversation. */
export const STUDIO_SHEET_BREAKPOINT = 1024;
/** Below this width the sheet takes the whole screen. */
export const STUDIO_PHONE_BREAKPOINT = 768;

export type WorkspaceKey = "library" | "findings" | "tasks" | "reports";

export const WORKSPACES: Record<WorkspaceKey, { label: string; icon: IconName; shortcut: string; description: string }> = {
  tasks: { label: "概览", icon: "gauge", shortcut: "mod+alt+1", description: "任务概览：需要处理的事项、执行中的工作、监控与计划入口" },
  library: { label: "资产", icon: "book", shortcut: "mod+alt+2", description: "资产列表、冲突裁决、文件导入与外部连接器" },
  findings: { label: "发现", icon: "bookmark", shortcut: "mod+alt+3", description: "发现档案、主张、证据与重复审阅" },
  reports: { label: "报告", icon: "file", shortcut: "mod+alt+4", description: "报告草稿、冻结、提交反馈与台账" },
};

export const WORKSPACE_ORDER: WorkspaceKey[] = ["tasks", "library", "findings", "reports"];

/** Filled by ConversationShell while the research conversation is mounted. */
export const studioComposer: MutableRefObject<ConversationComposerHandle | null> = { current: null };

/** Set by the route while mounted, so non-React callers can navigate. */
export const studioRouter: { current: { push: (href: string) => void; replace: (href: string) => void } | null } = { current: null };

/** Base route for conversation deep links; the mounted host sets it (prototype default). */
export const studioChatBase: { current: string } = { current: STUDIO_BASE };

export function chatHref(threadId: string): string {
  return `${studioChatBase.current}/${encodeURIComponent(threadId)}`;
}

export function workspaceSurfaceId(key: WorkspaceKey): string {
  return `ext:${key}`;
}

export function workspaceFromSurfaceId(id: string | null | undefined): WorkspaceKey | null {
  const key = id?.startsWith("ext:") ? id.slice(4) : "";
  return WORKSPACE_ORDER.includes(key as WorkspaceKey) ? (key as WorkspaceKey) : null;
}

function activeThread(): string {
  return readStudioState().activeThreadId;
}

function workspaceTab(key: WorkspaceKey) {
  return extensionSurface(key, WORKSPACES[key].label);
}

export function isSheetViewport(): boolean {
  return typeof window !== "undefined" && window.innerWidth <= STUDIO_SHEET_BREAKPOINT;
}

export function visibleWorkspace(threadId: string): VisibleWorkspace {
  const panel = chatPanel.thread(threadId);
  if (!panel.isOpen && !isSheetViewport()) return null;
  return workspaceFromSurfaceId(panel.activeId);
}

export function showWorkspace(threadId: string, active: WorkspaceKey) {
  const panel = chatPanel.thread(threadId);
  WORKSPACE_ORDER.forEach((key) => {
    if (key !== active && !panel.surfaces.some((surface) => surface.id === workspaceSurfaceId(key))) {
      chatPanel.open(threadId, workspaceTab(key));
    }
  });
  chatPanel.open(threadId, workspaceTab(active));
  orderWorkspaceTabs(threadId);
}

/** Keeps the workspace tabs in the fixed WORKSPACE_ORDER, wherever they were first opened. */
export function orderWorkspaceTabs(threadId: string) {
  const wsIds = WORKSPACE_ORDER.map(workspaceSurfaceId);
  for (let slot = 0; slot < wsIds.length; slot += 1) {
    const surfaces = chatPanel.thread(threadId).surfaces;
    const present: string[] = surfaces.filter((surface) => wsIds.includes(surface.id)).map((surface) => surface.id);
    const want = wsIds[slot];
    if (!present.includes(want) || present[slot] === want) continue;
    const anchor = present[slot];
    if (anchor) chatPanel.reorder(threadId, want, anchor);
  }
}

export function openWorkspace(key: WorkspaceKey, options: { focus?: boolean } = {}) {
  const threadId = activeThread();
  showWorkspace(threadId, key);
  if (options.focus) chatPanel.setMaximized(threadId, true);
}

export function openWorkspaceByDefault(threadId: string) {
  if (isSheetViewport()) return;
  const panel = chatPanel.thread(threadId);
  if (panel.isOpen || panel.surfaces.length) return;
  showWorkspace(threadId, "tasks");
}

export function toggleWorkspace(key: WorkspaceKey) {
  const threadId = activeThread();
  const panel = chatPanel.thread(threadId);
  if (panel.isOpen && panel.activeId === workspaceSurfaceId(key)) chatPanel.close(threadId);
  else openWorkspace(key);
}

export function revealConversation() {
  if (isSheetViewport()) chatPanel.close(activeThread());
}

export function openSourceInReader(sourceId: string, options: { passageId?: string; clueId?: string } = {}) {
  studio.setLibraryUi({ sourceId, passageId: options.passageId ?? "", clueId: options.clueId ?? "", importsOpen: false });
  openWorkspace("library");
}

/** Shows any reference (all 11 object types!) in the workspace that owns it. */
export function openRef(ref: StudioRef) {
  const current = readStudioState();
  if (ref.kind === "task") {
    studio.setTasksUi({ tab: "todo", highlightId: ref.id });
    openWorkspace("tasks");
    return;
  }
  if (ref.kind === "plan") {
    studio.setTasksUi({ tab: "plan", selectedPlanId: ref.id });
    openWorkspace("tasks");
    return;
  }
  if (ref.kind === "run") {
    studio.setTasksUi({ tab: "runs", selectedRunId: ref.id });
    openWorkspace("tasks");
    return;
  }
  if (ref.kind === "change") {
    studio.setTasksUi({ tab: "monitors", selectedChangeId: ref.id });
    openWorkspace("tasks");
    return;
  }
  if (ref.kind === "report") {
    studio.setReportsUi({ tab: "reports", view: "report", selectedReportId: ref.id });
    openWorkspace("reports");
    return;
  }
  if (ref.kind === "feedback") {
    studio.setReportsUi({ tab: "feedbacks", selectedFeedbackId: ref.id });
    openWorkspace("reports");
    return;
  }
  if (ref.kind === "evidence") {
    studio.setFindingsUi({ view: "evidences", evidenceId: ref.id });
    openWorkspace("findings");
    return;
  }
  if (ref.kind === "import") {
    studio.setLibraryUi({ importsOpen: true, importBatchId: ref.id, sourceId: "", passageId: "", clueId: "" });
    openWorkspace("library");
    return;
  }
  if (ref.kind === "clue") {
    studio.setFindingsUi({ view: "list", selectedId: ref.id });
    openWorkspace("findings");
    return;
  }
  const target = refTarget(current, ref);
  if (target) openSourceInReader(target.sourceId, { passageId: target.passageId, clueId: target.clueId });
  else openWorkspace("library");
}

export function openRefInReader(ref: StudioRef) {
  const target = refTarget(readStudioState(), ref);
  if (target) openSourceInReader(target.sourceId, { passageId: target.passageId, clueId: target.clueId });
}

export function showFindings(ids: string[], label: string) {
  studio.setFindingsUi({ view: "list", focusIds: ids, focusLabel: label, selectedId: ids[0] ?? "" });
  openWorkspace("findings");
}

export function turnAnchorId(turnId: string): string {
  return `studio-turn-${turnId}`;
}

export function revealTurn(threadId: string, turnId: string) {
  revealConversation();
  const messageId = replyMessageId(turnId);
  const params = new URLSearchParams(window.location.search);
  const sameTarget = threadId === activeThread() && params.get("message") === messageId;
  studioComposer.current?.showReplies();
  if (sameTarget) {
    document.getElementById(turnAnchorId(turnId))?.scrollIntoView({ block: "center", behavior: "smooth" });
    return;
  }
  params.set("message", messageId);
  studioRouter.current?.replace(`${chatHref(threadId)}?${params.toString()}`);
}

// ---------------------------------------------------------------------------
// URL state (Supports shareable links for all workspace objects & import batches)
// ---------------------------------------------------------------------------

const QUERY_KEYS = [
  "ws", "layout", "source", "passage", "clue", "task",
  "import", "line", "evidence", "report", "feedback", "run", "plan", "change",
];

export function workspaceQuery(current: StudioState, threadId: string, base: URLSearchParams): URLSearchParams {
  const params = new URLSearchParams(base.toString());
  QUERY_KEYS.forEach((key) => params.delete(key));
  const panel = chatPanel.thread(threadId);
  const key = panel.isOpen ? workspaceFromSurfaceId(panel.activeId) : null;
  if (!key) return params;
  params.set("ws", key);
  if (panel.maximized) params.set("layout", "focus");
  if (key === "library") {
    const ui = current.workspace.library;
    if (ui.sourceId) params.set("source", ui.sourceId);
    if (ui.sourceId && ui.passageId) params.set("passage", ui.passageId);
    if (ui.sourceId && ui.clueId) params.set("clue", ui.clueId);
    if (!ui.sourceId && ui.importsOpen && ui.importBatchId) {
      params.set("import", ui.importBatchId);
      if (ui.importFocusLine) params.set("line", String(ui.importFocusLine));
    }
  } else if (key === "findings") {
    const ui = current.workspace.findings;
    if (ui.view === "evidences" && ui.evidenceId) params.set("evidence", ui.evidenceId);
    else if (ui.selectedId) params.set("clue", ui.selectedId);
  } else if (key === "tasks") {
    const ui = current.workspace.tasks;
    if (ui.tab === "runs" && ui.selectedRunId) params.set("run", ui.selectedRunId);
    else if (ui.tab === "plan" && ui.selectedPlanId) params.set("plan", ui.selectedPlanId);
    else if (ui.tab === "monitors" && ui.selectedChangeId) params.set("change", ui.selectedChangeId);
    else if (ui.highlightId) params.set("task", ui.highlightId);
  } else if (key === "reports") {
    const ui = current.workspace.reports;
    if (ui.tab === "feedbacks" && ui.selectedFeedbackId) params.set("feedback", ui.selectedFeedbackId);
    else if (ui.selectedReportId) params.set("report", ui.selectedReportId);
  }
  return params;
}

export function applyWorkspaceQuery(threadId: string, search: URLSearchParams): boolean {
  const key = search.get("ws") as WorkspaceKey | null;
  if (!key || !WORKSPACE_ORDER.includes(key)) return false;
  if (key === "library") {
    const imp = search.get("import");
    const line = Number(search.get("line") || 0) || undefined;
    if (imp) {
      studio.setLibraryUi({ importsOpen: true, importBatchId: imp, importFocusLine: line, sourceId: "", passageId: "", clueId: "" });
    } else {
      studio.setLibraryUi({ importsOpen: false, importBatchId: "", importFocusLine: undefined, sourceId: search.get("source") || "", passageId: search.get("passage") || "", clueId: search.get("clue") || "" });
    }
  } else if (key === "findings") {
    const ev = search.get("evidence");
    const clue = search.get("clue");
    const fv = search.get("view") as FindingsUi["view"] | null;
    const parent = search.get("evidenceParent") || undefined;
    if (ev) studio.setFindingsUi({ view: "evidences", evidenceId: ev, evidenceParent: parent, selectedId: "", focusIds: null, focusLabel: "" });
    else if (clue) studio.setFindingsUi({ view: "list", selectedId: clue, evidenceId: "", evidenceParent: undefined, focusIds: null, focusLabel: "" });
    else if (fv) studio.setFindingsUi({ view: fv, evidenceId: "", evidenceParent: undefined, selectedId: "" });
    else studio.setFindingsUi({ view: "list", evidenceId: "", evidenceParent: undefined, selectedId: "" });
  } else if (key === "tasks") {
    const run = search.get("run");
    const plan = search.get("plan");
    const change = search.get("change");
    const task = search.get("task");
    const tab = search.get("tab") as TasksSubTab | null;
    if (run) studio.setTasksUi({ tab: "runs", selectedRunId: run });
    else if (plan) studio.setTasksUi({ tab: "plan", selectedPlanId: plan, selectedRunId: "" });
    else if (change) studio.setTasksUi({ tab: "monitors", selectedChangeId: change, selectedRunId: "" });
    else if (task) studio.setTasksUi({ tab: "todo", highlightId: task, selectedRunId: "" });
    else studio.setTasksUi({ tab: tab ?? "overview", selectedRunId: "", selectedChangeId: "", highlightId: "" });
  } else if (key === "reports") {
    const fb = search.get("feedback");
    const rep = search.get("report");
    if (fb) studio.setReportsUi({ tab: "feedbacks", selectedFeedbackId: fb });
    else if (rep) studio.setReportsUi({ tab: "reports", view: "report", selectedReportId: rep, selectedFeedbackId: "" });
    else studio.setReportsUi({ tab: "reports", view: "list", selectedReportId: "", selectedFeedbackId: "" });
  }
  showWorkspace(threadId, key);
  chatPanel.setMaximized(threadId, search.get("layout") === "focus");
  return true;
}

export function applyWorkspaceUrl(pathname: string, search: URLSearchParams): WorkspaceKey | null {
  if (!pathname.startsWith(STUDIO_WORKSPACE)) return null;
  const rest = pathname.slice(STUDIO_WORKSPACE.length).split("/").filter(Boolean).map((part) => decodeURIComponent(part));
  if (rest[0] === "clues") {
    const clue = search.get("clue") || "";
    const reply = search.get("filter") === "reply";
    if (reply) {
      const current = readStudioState();
      studio.setFindingsUi({ focusIds: current.findings[current.activeThreadId] ?? [], focusLabel: "上次回复引用的发现", selectedId: clue });
    } else if (clue) studio.setFindingsUi({ selectedId: clue });
    return "findings";
  }
  if (rest[0] === "tasks") {
    const task = search.get("task") || "";
    if (task) studio.setTasksUi({ highlightId: task });
    return "tasks";
  }
  if (rest[0] === "library" && rest[1]) {
    studio.setLibraryUi({ sourceId: rest[1], passageId: search.get("passage") || "", clueId: search.get("clue") || "" });
  }
  return "library";
}
