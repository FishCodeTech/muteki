"use client";

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Button, Callout, IconButton, TabBar } from "@/components/chat/ui";
import { ConversationShell, type ConversationExperience, type ConversationNavigation } from "@/components/conversation/ConversationShell";
import { ConversationChromeContext, type ConversationChrome } from "@/components/conversationChrome";
import { Icon } from "@/components/Icon";
import { listSrcRuns } from "@/lib/srcApi";
import { srcDemoConversation, srcDemoThreads } from "@/lib/srcDemoData";
import { assetRef, evidenceId, evidenceRef, findingRef, type ResearchEvidence, type ResearchRef, type ResearchSnapshot } from "@/lib/srcResearch";
import { SRC_RESEARCH_DEMO_STEPS } from "@/lib/srcResearchDemo";
import { useMediaQuery } from "@/lib/useMediaQuery";
import type { ConversationView } from "@/lib/useConversation";
import { MonitoringInspector } from "./MonitoringInspector";
import { RunInspector } from "./RunInspector";
import { SrcResearchAssets } from "./SrcResearchAssets";
import { SrcResearchChatContext } from "./SrcResearchChatContext";
import { SrcResearchEvidence } from "./SrcResearchEvidence";
import { SrcResearchFindings, type SrcResearchFindingActions } from "./SrcResearchFindings";
import { PaneHeader } from "./SrcResearchParts";
import { SrcResearchProgress } from "./SrcResearchProgress";
import { loadSrcResearchSnapshot } from "./srcResearchSource";
import { SrcError, SrcLoading, useSrcResource } from "./ui";
import { RESEARCH_VIEWS, useSrcResearchWorkspace, type ResearchSentMessage, type ResearchView } from "./useSrcResearchWorkspace";
import { useSrcConversationExperience } from "./useSrcConversationExperience";
import styles from "./srcResearch.module.css";

const VIEW_LABELS: Record<ResearchView, string> = { progress: "进展", assets: "资产", findings: "发现与证据", conversation: "对话" };

function withSentMessages(view: ConversationView, sent: ResearchSentMessage[]): ConversationView {
  if (!sent.length) return view;
  const base = view.event_watermark;
  const messages = sent.map((row, index) => ({
    message_id: row.id, turn_id: `${row.id}-turn`, role: "user", kind: "message",
    text: `${row.text}\n\n上下文：${row.refs.map(ref => ref.kind === "task" ? "当前研究" : ref.id).join("、")}`,
    stream_seq: base + index + 1, created_at: row.at,
  }));
  return {
    ...view,
    messages: [...view.messages, ...messages],
    turns: [...view.turns, ...messages.map(message => ({ turn_id: message.turn_id, kind: "user", status: "completed", text: message.text, created_at: message.created_at, completed_at: message.created_at }))],
    watermark: view.watermark + sent.length,
    event_watermark: base + sent.length,
  };
}

function RecordPane({ taskId, record, snapshot, onBack }: { taskId: string; record: { kind: "run" | "monitor"; id: string }; snapshot: ResearchSnapshot; onBack: () => void }) {
  const runs = useSrcResource(() => record.kind === "run" ? listSrcRuns(taskId) : Promise.resolve([]), [taskId, record.kind]);
  const job = snapshot.jobs.find(row => row.id === record.id);
  const run = runs.data?.find(row => row.binding.run_id === record.id);
  return <div className={styles.viewScroll}>
    <PaneHeader onBack={onBack} backLabel="返回进展" eyebrow={record.kind === "run" ? "Pentest 评估 Run" : "持续监控"} title={job?.title || record.id} />
    {record.kind === "monitor" ? <div className={styles.embedded}><MonitoringInspector taskId={taskId} monitorId={record.id} /></div>
      : job?.origin === "simulated" ? <Callout tone="neutral" title="本地模拟进展">{job.id} 只存在于本页的演示状态中，没有后端 Run 记录可打开。{job.detail ? `\n${job.detail}` : ""}</Callout>
        : runs.loading && !runs.data ? <SrcLoading label="正在读取 Run 记录…" />
          : runs.error ? <SrcError error={runs.error} onRetry={() => void runs.reload()} />
            : <div className={styles.embedded}><RunInspector taskId={taskId} run={run} onChanged={() => runs.reload()} /></div>}
  </div>;
}

export function SrcResearchWorkspace({ taskId, navigation, basePath, exitHref = "/src" }: {
  taskId: string;
  navigation: ConversationNavigation;
  basePath: string;
  exitHref?: string;
}) {
  const resource = useSrcResource(() => loadSrcResearchSnapshot(taskId), [taskId]);
  if (resource.loading && !resource.data) return <main className="src-task-state"><SrcLoading label="正在读取研究对象、资产与发现…" /></main>;
  if (!resource.data) return <main className="src-task-state"><SrcError error={resource.error || "研究对象不可读"} onRetry={() => void resource.reload()} /></main>;
  return <ResearchWorkspace key={taskId} base={resource.data} navigation={navigation} basePath={basePath} exitHref={exitHref} />;
}

function ResearchWorkspace({ base, navigation, basePath, exitHref }: { base: ResearchSnapshot; navigation: ConversationNavigation; basePath: string; exitHref: string }) {
  const requestedView = navigation.searchParams.get("view") as ResearchView | null;
  const model = useSrcResearchWorkspace(base, requestedView && RESEARCH_VIEWS.includes(requestedView) ? requestedView : "progress");
  const { state, actions, mergedInto, findingsById, assetsById } = model;
  const snapshot = model.snapshot || base;
  const taskId = snapshot.taskId;
  const narrow = useMediaQuery("(max-width: 1100px)");
  const [chatOpen, setChatOpen] = useState(true);
  const [chatNavOpen, setChatNavOpen] = useState(false);
  const [chatMobileNavOpen, setChatMobileNavOpen] = useState(false);
  const [chatNavWidth, setChatNavWidth] = useState(248);
  const chatRef = useRef<HTMLElement | null>(null);
  const headerActionsRef = useRef<HTMLDivElement | null>(null);
  const overlayWasOpen = useRef(false);
  const fullChat = state.view === "conversation";
  const overlayChat = narrow && !fullChat && chatOpen;
  const chatVisible = fullChat || chatOpen;

  useEffect(() => setChatOpen(!narrow), [narrow]);
  useEffect(() => {
    const params = new URLSearchParams(window.location.search);
    if (params.get("view") === state.view) return;
    params.set("view", state.view);
    navigation.router.replace(`${basePath}?${params.toString()}`, { scroll: false });
  }, [basePath, navigation.router, state.view]);
  useEffect(() => {
    if (overlayChat) {
      overlayWasOpen.current = true;
      const frame = window.requestAnimationFrame(() => chatRef.current?.querySelector<HTMLElement>("[data-c34-composer]")?.focus());
      return () => window.cancelAnimationFrame(frame);
    }
    if (!overlayWasOpen.current) return;
    overlayWasOpen.current = false;
    if (chatVisible) return;
    // The closed overlay is inert, so focus would otherwise fall back to <body>.
    const active = document.activeElement;
    if (!active || active === document.body || chatRef.current?.contains(active)) headerActionsRef.current?.querySelector<HTMLElement>("button")?.focus();
  }, [chatVisible, overlayChat]);

  const contextRefs = state.chatContext;
  const inContext = useCallback((ref: ResearchRef) => contextRefs.some(row => row.kind === ref.kind && row.id === ref.id), [contextRefs]);
  const findEvidence = useCallback((id: string): { ref: ResearchEvidence; findingId?: string } | null => {
    for (const finding of snapshot.findings) {
      const ref = finding.evidence.find(row => evidenceId(row) === id || row.artifact_id === id || row.exchange_id === id);
      if (ref) return { ref, findingId: finding.id };
    }
    return null;
  }, [snapshot.findings]);
  const openRef = useCallback((ref: ResearchRef) => {
    if (narrow) setChatOpen(false);
    if (ref.kind === "task" || ref.kind === "import") actions.setView("progress");
    else if (ref.kind === "asset") actions.selectAsset(ref.id, "assets");
    else if (ref.kind === "finding") actions.focusFinding(ref.id, { view: "findings" });
    else if (ref.kind === "run" || ref.kind === "monitor") actions.openRecord({ kind: ref.kind, id: ref.id });
    else if (ref.kind === "evidence") {
      const hit = findEvidence(ref.id);
      if (hit) actions.openEvidence(hit.ref, hit.findingId);
    }
  }, [actions, findEvidence, narrow]);

  const viewing: ResearchRef | undefined = useMemo(() => {
    if (state.evidence) return evidenceRef(state.evidence.ref);
    if (state.view === "findings" && state.focusFindingId) { const row = findingsById.get(state.focusFindingId); return row ? findingRef(row) : undefined; }
    if (state.view === "assets" && state.selectedAssetId) { const row = assetsById.get(state.selectedAssetId); return row ? assetRef(row) : undefined; }
    return undefined;
  }, [assetsById, findingsById, state.evidence, state.focusFindingId, state.selectedAssetId, state.view]);

  const srcConversation = useSrcConversationExperience(undefined, snapshot.demo ? taskId : undefined);
  const demoView = useMemo(() => snapshot.demo ? srcDemoConversation(taskId) : null, [snapshot.demo, taskId]);
  const previewView = useMemo(() => demoView ? withSentMessages(demoView, state.sentMessages) : null, [demoView, state.sentMessages]);
  // ConversationShell resets its preview whenever this object changes identity, so it only follows sent messages.
  const preview = useMemo(() => previewView ? { threads: srcDemoThreads, view: previewView, notice: "演示：消息只追加到本地预览，没有发送给 Agent", onSend: actions.recordSend } : undefined, [actions.recordSend, previewView]);
  const conversationTexts = useMemo(() => previewView?.messages.map(message => message.text) || [], [previewView]);

  const chrome = useMemo<ConversationChrome>(() => ({
    sidebarCollapsed: !(fullChat && chatNavOpen),
    toggleSidebarCollapsed: () => {
      if (!fullChat) { actions.setView("conversation"); setChatNavOpen(true); } else setChatNavOpen(value => !value);
    },
    sidebarWidth: chatNavWidth,
    setSidebarWidth: width => setChatNavWidth(Math.max(200, Math.min(360, width))),
    mobileSidebarOpen: chatMobileNavOpen,
    setMobileSidebarOpen: setChatMobileNavOpen,
  }), [actions, chatMobileNavOpen, chatNavOpen, chatNavWidth, fullChat]);

  const experience: ConversationExperience = {
    ...srcConversation.experience,
    ...(preview ? { preview } : {}),
    threadHref: id => id === snapshot.threadId ? basePath : srcConversation.experience.threadHref?.(id) || "/src",
    headerActions: <>
      <IconButton size="md" icon={fullChat ? "minimize" : "maximize"} label={fullChat ? "回到工作区，保留对话在侧边" : "切换到完整对话"} onClick={() => actions.setView(fullChat ? "progress" : "conversation")} />
      {!fullChat ? <IconButton size="md" icon="panelRightClose" label="收起对话" onClick={() => setChatOpen(false)} /> : null}
    </>,
    composerAccessory: <SrcResearchChatContext refs={contextRefs} viewing={fullChat ? undefined : viewing}
      note={snapshot.demo ? "演示：发送只把消息追加到本地预览，并在进展中记录上下文；不会调用 Agent 或 MCP。" : "上下文目前只在本页标注，尚未随消息提交到后端。"}
      onOpen={openRef} onAdd={ref => actions.addContext(ref)} onRemove={actions.removeContext} />,
  };

  const evidenceFinding = state.evidence?.findingId ? findingsById.get(state.evidence.findingId) : undefined;
  const findingActions: SrcResearchFindingActions = {
    onFilter: actions.setFindingFilter, onPage: actions.setFindingPage, onToggle: actions.toggleFinding, onSelect: actions.selectFindings,
    onFocus: id => actions.focusFinding(id), onStartMerge: actions.startMerge, onMergePrimary: actions.setMergePrimary,
    onCancelMerge: actions.cancelMerge, onConfirmMerge: actions.confirmMerge, onUndoMerge: actions.undoMerge,
    onOpenEvidence: actions.openEvidence, onOpenAsset: id => actions.selectAsset(id, "assets"), onOpenRef: openRef, onAddContext: actions.addContext,
  };
  const backLabel = state.evidence?.returnView === "assets" ? "返回资产" : state.evidence?.returnView === "progress" ? "返回进展"
    : `返回发现${state.selectedFindingIds.length ? `（保留 ${state.selectedFindingIds.length} 项选择与筛选）` : "（保留筛选）"}`;
  const running = snapshot.jobs.some(job => job.state === "running");
  const demoControls = snapshot.demo ? {
    done: state.demoSteps.length, total: SRC_RESEARCH_DEMO_STEPS.length,
    nextLabel: SRC_RESEARCH_DEMO_STEPS[state.demoSteps.length]?.label, onAdvance: actions.advanceDemo, onReset: actions.resetDemo,
  } : undefined;

  const main = state.evidence ? <SrcResearchEvidence taskId={taskId} evidence={state.evidence.ref} finding={evidenceFinding} backLabel={backLabel}
    inContext={inContext(evidenceRef(state.evidence.ref))} onBack={actions.closeEvidence} onAddContext={actions.addContext} onOpenRef={openRef} />
    : state.view === "progress" ? state.openRecord
      ? <RecordPane taskId={taskId} record={state.openRecord} snapshot={snapshot} onBack={() => actions.openRecord(null)} />
      : <SrcResearchProgress snapshot={snapshot} conversationTexts={conversationTexts} onOpenRef={openRef} onAddContext={ref => actions.addContext(ref)}
        onFilterFindings={patch => { actions.setFindingFilter(patch); actions.setView("findings"); }} demo={demoControls} />
      : state.view === "assets" ? <SrcResearchAssets taskId={taskId} snapshot={snapshot} filter={state.assetFilter} page={state.assetPage} selectedAssetId={state.selectedAssetId}
        findingsById={findingsById} contextRefs={contextRefs} onFilter={actions.setAssetFilter} onPage={actions.setAssetPage} onSelect={id => actions.selectAsset(id)}
        onAddContext={ref => actions.addContext(ref)} onOpenFindings={assetId => { actions.setFindingFilter({ assetId }); actions.setView("findings"); }}
        onOpenFinding={(findingId, assetId) => actions.focusFinding(findingId, { view: "findings", assetId })} onOpenRef={openRef} />
        : state.view === "findings" ? <SrcResearchFindings taskId={taskId} snapshot={snapshot} filter={state.findingFilter} page={state.findingPage}
          selectedIds={state.selectedFindingIds} focusId={state.focusFindingId} mergeDraft={state.mergeDraft} merges={state.merges} mergedInto={mergedInto}
          findingsById={findingsById} assetsById={assetsById} contextRefs={contextRefs} actions={findingActions} />
          : null;

  return <main className={styles.root} data-view={state.view} data-chat={fullChat ? "full" : chatOpen ? (narrow ? "overlay" : "dock") : "hidden"}>
    <header className={styles.header}>
      <div className={styles.titleBlock}>
        <span>SRC 研究 · 工作台原型{snapshot.demo ? " · 虚构演示数据" : ""}</span>
        <h1 title={snapshot.title}>{snapshot.title}</h1>
      </div>
      <TabBar<ResearchView> className={styles.tabs} ariaLabel="研究视图" value={state.view} onChange={actions.setView} items={[
        { value: "progress", label: VIEW_LABELS.progress, icon: "activity", alert: running },
        { value: "assets", label: VIEW_LABELS.assets, icon: "database", count: snapshot.assetTotal },
        { value: "findings", label: VIEW_LABELS.findings, icon: "listChecks", count: snapshot.findings.length },
        { value: "conversation", label: VIEW_LABELS.conversation, icon: "messages" },
      ]} />
      <div ref={headerActionsRef} className={styles.headerActions}>
        {!fullChat ? <Button size="sm" variant={chatOpen ? "soft" : "secondary"} icon={chatOpen ? "panelRightClose" : "messageCircle"} aria-expanded={chatOpen} onClick={() => setChatOpen(value => !value)}>
          {chatOpen ? "收起对话" : `对话 · ${contextRefs.length} 个上下文`}
        </Button> : null}
        <Button size="sm" variant="ghost" icon="arrowLeft" onClick={() => navigation.router.push(exitHref)}>返回 SRC</Button>
      </div>
    </header>
    {snapshot.demo ? <div className={styles.demoBanner} role="note">
      <Icon name="info" size={14} />
      <span>虚构演示：{snapshot.assetTotal.toLocaleString()} 项资产与 {snapshot.findings.length} 条发现来自离线 fixture，域名仅使用 *.example.invalid；{snapshot.disclaimer}。进展由你手动推进，未连接实时 MCP、真实 Agent 或任何目标。</span>
    </div> : null}
    <div className={styles.body}>
      <section className={styles.main} aria-label={`${VIEW_LABELS[state.view]}工作区`} hidden={fullChat}
        inert={overlayChat ? true : undefined}>
        {main}
      </section>
      {overlayChat ? <button type="button" className={styles.chatBackdrop} aria-label="关闭对话" tabIndex={-1} onClick={() => setChatOpen(false)} /> : null}
      <aside ref={chatRef} className={styles.chat} aria-label="研究对话" inert={!chatVisible ? true : undefined} aria-hidden={!chatVisible || undefined}
        role={overlayChat ? "dialog" : undefined} aria-modal={overlayChat || undefined}
        onKeyDown={event => { if (overlayChat && event.key === "Escape" && !event.defaultPrevented) setChatOpen(false); }}>
        <ConversationChromeContext.Provider value={chrome}>
          <ConversationShell threadId={snapshot.threadId} navigation={navigation} experience={experience} />
        </ConversationChromeContext.Provider>
      </aside>
    </div>
    {srcConversation.error ? <div className="src-task-list-error"><SrcError error={srcConversation.error} onRetry={() => void srcConversation.reload()} /></div> : null}
  </main>;
}
