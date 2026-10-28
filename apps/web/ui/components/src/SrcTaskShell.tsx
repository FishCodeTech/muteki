"use client";

import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { Button } from "@heroui/react";
import { Button as ChatButton, IconButton } from "@/components/chat/ui";
import { ConversationShell, type ConversationNavigation } from "@/components/conversation/ConversationShell";
import { Icon, type IconName } from "@/components/Icon";
import { ensureSrcRun, fetchSrcTask, listSrcRuns } from "@/lib/srcApi";
import type { SrcTask } from "@/lib/srcTypes";
import { useMediaQuery } from "@/lib/useMediaQuery";
import { ApprovalInspector } from "./ApprovalInspector";
import { ArtifactCatalog } from "./ArtifactCatalog";
import { ArtifactInspector } from "./ArtifactInspector";
import { AssetCatalog } from "./AssetCatalog";
import { AssetInspector } from "./AssetInspector";
import { ExecutionPolicyPanel } from "./ExecutionPolicyPanel";
import { FindingInspector } from "./FindingInspector";
import { MonitoringInspector } from "./MonitoringInspector";
import { ReportInspector } from "./ReportInspector";
import { RequestWorkspace } from "./RequestWorkspace";
import { ResearchInspector } from "./ResearchInspector";
import { querySrc, type EvidenceReference } from "./reviewPrimitives";
import { RunInspector } from "./RunInspector";
import { SrcDefinitions, SrcEmpty, SrcError, SrcLoading, SrcSection, SrcStatus, stateTone, useSrcResource } from "./ui";
import { SrcWorkbench } from "./SrcWorkbench";
import { useSrcConversationExperience } from "./useSrcConversationExperience";
import { SRC_DEMO_DISCLAIMER } from "@/lib/srcDemoData";

type Selection = {
  kind: "workbench" | "run" | "policy" | "approval" | "grants" | "catalog" | "exchange" | "assetCatalog" | "asset" | "monitoring" | "findingCatalog" | "finding" | "reportCatalog" | "report" | "research" | "artifactCatalog" | "artifact" | "other";
  id: string;
  path: string;
};

function parseSelection(pathname: string, taskId: string): Selection | null {
  const base = `/src/tasks/${taskId}`;
  const rest = pathname.startsWith(base) ? pathname.slice(base.length).replace(/^\/+|\/+$/g, "") : "";
  if (!rest) return null;
  const [kind, ...parts] = rest.split("/");
  const id = decodeURIComponent(parts.join("/"));
  if (kind === "workbench") return { kind: "workbench", id: "workbench", path: rest };
  if (kind === "runs" && id) return { kind: "run", id, path: rest };
  if (kind === "policy") return { kind: "policy", id: "policy", path: rest };
  if (kind === "approvals" && id) return { kind: "approval", id, path: rest };
  if (kind === "approvals") return { kind: "policy", id: "approvals", path: rest };
  if (kind === "grants") return { kind: "grants", id: "grants", path: rest };
  if (kind === "runs") return { kind: "catalog", id: "runs", path: rest };
  if (kind === "exchanges") return { kind: "exchange", id, path: rest };
  if (kind === "assets" && id) return { kind: "asset", id, path: rest };
  if (kind === "assets") return { kind: "assetCatalog", id: "assets", path: rest };
  if (kind === "monitoring") return { kind: "monitoring", id, path: rest };
  if (kind === "findings" && id) return { kind: "finding", id, path: rest };
  if (kind === "findings") return { kind: "findingCatalog", id: "findings", path: rest };
  if (kind === "reports" && id) return { kind: "report", id, path: rest };
  if (kind === "reports") return { kind: "reportCatalog", id: "reports", path: rest };
  if (kind === "research") return { kind: "research", id, path: rest };
  if (kind === "artifacts" && id) return { kind: "artifact", id, path: rest };
  if (kind === "artifacts") return { kind: "artifactCatalog", id: "artifacts", path: rest };
  return { kind: "other", id: rest, path: rest };
}

type ClaimDirectory = { finding_id: string; text: string; source_run_id?: string; version?: number };
type ReadinessValue = string | { status?: string } | undefined;
type ReportDirectory = { report_id: string; finding_id: string; report_seq: number; document?: { title?: string }; review?: { readiness?: ReadinessValue } };

function readinessLabel(value: ReadinessValue): string {
  return typeof value === "object" ? value.status || "unreviewed" : value || "unreviewed";
}

function FindingDirectory({ claims, onOpen }: { claims: ClaimDirectory[]; onOpen: (findingId: string) => void }) {
  const groups = useMemo(() => {
    const map = new Map<string, ClaimDirectory[]>();
    claims.forEach(claim => {
      const rows = map.get(claim.finding_id) || [];
      rows.push(claim); map.set(claim.finding_id, rows);
    });
    return [...map.entries()];
  }, [claims]);
  if (!groups.length) return <SrcEmpty>尚无 Finding 或原子主张。实际观察、推论和未验证内容会在这里分开呈现。</SrcEmpty>;
  return <div className="src-record-list">{groups.map(([findingId, rows]) => <button key={findingId} onClick={() => onOpen(findingId)}><span><strong>{rows[0]?.text || findingId}</strong><small>{findingId} · {rows.length} 条主张{rows[0]?.source_run_id ? ` · ${rows[0].source_run_id}` : ""}</small></span><Icon name="chevronRight" size={14} /></button>)}</div>;
}

function ReportDirectory({ reports, onOpen }: { reports: ReportDirectory[]; onOpen: (reportId: string) => void }) {
  if (!reports.length) return <SrcEmpty>尚无冻结报告。只有固定证据、计划和审阅版本后才能导出交付。</SrcEmpty>;
  return <div className="src-record-list">{reports.map(report => { const readiness = readinessLabel(report.review?.readiness); return <button key={report.report_id} onClick={() => onOpen(report.report_id)}><span><strong>{report.document?.title || report.finding_id}</strong><small>{report.report_id} · v{report.report_seq} · {report.finding_id}</small></span><SrcStatus tone={stateTone(readiness)}>{readiness === "unreviewed" ? "未审阅" : readiness}</SrcStatus></button>; })}</div>;
}

export function SrcTaskShell({ taskId, navigation, renderSelection }: {
  taskId: string;
  navigation: ConversationNavigation;
  renderSelection?: (selection: { kind: string; id: string; path: string }) => ReactNode;
}) {
  const taskResource = useSrcResource(() => fetchSrcTask(taskId), [taskId]);
  const demo = taskId.startsWith("src-demo-");
  const srcConversation = useSrcConversationExperience(taskResource.data || undefined, demo ? taskId : undefined);
  const runsResource = useSrcResource(() => listSrcRuns(taskId), [taskId], { pollMs: 5000, enabled: Boolean(taskResource.data) });
  const directoryResource = useSrcResource(async () => {
    const [reports, claimPage] = await Promise.all([
      querySrc<ReportDirectory[]>(taskId, "src.report.list"),
      querySrc<{ records?: Array<{ payload: ClaimDirectory }> }>(taskId, "src.record.list", { kind: "finding_claim", limit: 1000 }),
    ]);
    return { reports, claims: (claimPage.records || []).map(row => row.payload) };
  }, [taskId], { pollMs: 10000, enabled: Boolean(taskResource.data) });
  const [catalogOpen, setCatalogOpen] = useState(false);
  const [creating, setCreating] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [createError, setCreateError] = useState("");
  const [target, setTarget] = useState("");
  const [question, setQuestion] = useState("");
  const [bindingKey, setBindingKey] = useState("");
  const selection = useMemo(() => parseSelection(navigation.pathname, taskId), [navigation.pathname, taskId]);
  const narrowPanel = useMediaQuery("(max-width: 900px)");
  const panelRef = useRef<HTMLElement | null>(null);
  const panelOpen = catalogOpen || Boolean(selection && selection.kind !== "catalog");
  useEffect(() => setCatalogOpen(selection?.kind === "catalog"), [selection?.kind]);
  const basePath = `/src/tasks/${taskId}`;
  const suffix = navigation.searchParams.toString() ? `?${navigation.searchParams.toString()}` : "";
  const go = useCallback((path = "") => {
    setCatalogOpen(false);
    navigation.router.push(`${basePath}${path ? `/${path}` : ""}${suffix}`);
  }, [basePath, navigation.router, suffix]);
  const closeInspector = useCallback(() => {
    setCatalogOpen(false);
    go();
    if (narrowPanel) window.requestAnimationFrame(() => document.querySelector<HTMLElement>(".src-task-workbench [data-c34-composer]")?.focus());
  }, [go, narrowPanel]);
  useEffect(() => {
    if (!narrowPanel || !panelOpen) return;
    const frame = window.requestAnimationFrame(() => panelRef.current?.focus());
    return () => window.cancelAnimationFrame(frame);
  }, [narrowPanel, panelOpen, selection?.path]);
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== "Escape" || event.defaultPrevented) return;
      if (catalogOpen) { event.preventDefault(); closeInspector(); }
      else if (selection) { event.preventDefault(); closeInspector(); }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [catalogOpen, closeInspector, selection]);
  if (taskResource.loading && !taskResource.data) return <main className="src-task-state"><SrcLoading label="正在恢复 SRC 任务与主对话…" /></main>;
  if (!taskResource.data) return <main className="src-task-state"><SrcError error={taskResource.error || "SRC 任务不可读"} onRetry={() => void taskResource.reload()} /></main>;
  const task: SrcTask = taskResource.data;
  if (!demo && (!selection || selection.kind === "workbench")) return <SrcWorkbench taskId={taskId} navigation={navigation} />;
  if (demo && selection?.kind === "workbench") return <SrcWorkbench taskId={taskId} navigation={navigation} />;
  const runs = runsResource.data || [];
  const reports = directoryResource.data?.reports || [];
  const claims = directoryResource.data?.claims || [];
  const selectedRun = selection?.kind === "run" ? runs.find(row => row.binding.run_id === selection.id) : undefined;
  const openEvidence = (ref: EvidenceReference) => {
    if (ref.exchange_id) go(`exchanges/${encodeURIComponent(ref.exchange_id)}`);
    else {
      const id = ref.sha256 || ref.artifact_id || ref.id;
      if (id) go(`artifacts/${encodeURIComponent(id)}`);
    }
  };
  const openExchange = (exchangeId: string) => go(`exchanges/${encodeURIComponent(exchangeId)}`);
  const openRun = (runId: string) => go(`runs/${encodeURIComponent(runId)}`);
  const openFinding = (findingId: string) => go(`findings/${encodeURIComponent(findingId)}`);
  const openReport = (reportId: string) => go(`reports/${encodeURIComponent(reportId)}`);
  const createRun = async () => {
    setSubmitting(true); setCreateError("");
    try {
      const receipt = await ensureSrcRun(taskId, {
        target_url: target.trim(), question: question.trim(),
        binding_key: bindingKey.trim() || undefined,
      });
      setTarget(""); setQuestion(""); setBindingKey(""); setCreating(false);
      await runsResource.reload();
      go(`runs/${encodeURIComponent(receipt.output.binding.run_id)}`);
    } catch (cause) { setCreateError(cause instanceof Error ? cause.message : String(cause)); }
    finally { setSubmitting(false); }
  };
  const inspector = selection?.kind === "run" ? (
    <RunInspector taskId={taskId} run={selectedRun} onChanged={() => runsResource.reload()} />
  ) : selection?.kind === "policy" || selection?.kind === "grants" ? (
    <ExecutionPolicyPanel taskId={taskId} initialTab={selection.kind === "grants" ? "grants" : selection.id === "approvals" ? "approvals" : "policy"} onOpenApproval={id => go(`approvals/${encodeURIComponent(id)}`)} />
  ) : selection?.kind === "approval" ? (
    <ApprovalInspector taskId={taskId} approvalId={selection.id} />
  ) : selection?.kind === "exchange" ? (
    <RequestWorkspace taskId={taskId} initialExchangeId={selection.id} onExchangeChange={openExchange} />
  ) : selection?.kind === "assetCatalog" ? (
    <AssetCatalog taskId={taskId} onSelectAsset={assetId => go(`assets/${encodeURIComponent(assetId)}`)} />
  ) : selection?.kind === "asset" ? (
    <AssetInspector taskId={taskId} assetId={selection.id} onSelectAsset={assetId => go(`assets/${encodeURIComponent(assetId)}`)} />
  ) : selection?.kind === "monitoring" ? (
    <MonitoringInspector taskId={taskId} monitorId={selection.id || undefined} onSelectMonitor={monitorId => go(`monitoring/${encodeURIComponent(monitorId)}`)} />
  ) : selection?.kind === "findingCatalog" ? (
    <FindingDirectory claims={claims} onOpen={openFinding} />
  ) : selection?.kind === "finding" ? (
    <FindingInspector taskId={taskId} findingId={selection.id} onOpenEvidence={openEvidence} onOpenReport={openReport} onClose={closeInspector} />
  ) : selection?.kind === "reportCatalog" ? (
    <ReportDirectory reports={reports} onOpen={openReport} />
  ) : selection?.kind === "report" ? (
    <ReportInspector taskId={taskId} reportId={selection.id} onOpenEvidence={openEvidence} onOpenFinding={openFinding} onSelectReport={openReport} onClose={closeInspector} />
  ) : selection?.kind === "research" ? (
    <ResearchInspector taskId={taskId} researchId={selection.id || undefined} onOpenEvidence={openEvidence} onOpenExchange={openExchange} onOpenRun={openRun} onClose={closeInspector} />
  ) : selection?.kind === "artifactCatalog" ? (
    <ArtifactCatalog taskId={taskId} selectedArtifactId={undefined} onSelectArtifact={artifactId => go(`artifacts/${encodeURIComponent(artifactId)}`)} />
  ) : selection?.kind === "artifact" ? (
    <ArtifactInspector taskId={taskId} artifactId={selection.id} />
  ) : selection?.kind === "other" && renderSelection ? renderSelection(selection) : selection?.kind === "other" ? (
    <SrcError error={`无法解析当前 SRC 对象：${selection.path}`} />
  ) : null;
  const titles: Partial<Record<Selection["kind"], string>> = {
    run: selection?.kind === "run" ? `Run · ${selection.id}` : undefined,
    approval: selection?.kind === "approval" ? `审批 · ${selection.id}` : undefined,
    grants: "Task 范围授权",
    policy: selection?.kind === "policy" && selection.id === "approvals" ? "审批记录" : "执行权限",
    exchange: selection?.kind === "exchange" ? selection.id ? `请求 · ${selection.id}` : "请求工作区" : undefined,
    assetCatalog: "资产与查询",
    asset: selection?.kind === "asset" ? `资产 · ${selection.id}` : undefined,
    monitoring: selection?.kind === "monitoring" && selection.id ? `监控 · ${selection.id}` : "持续监控",
    findingCatalog: "Finding 目录",
    finding: selection?.kind === "finding" ? `Finding · ${selection.id}` : undefined,
    reportCatalog: "冻结报告目录",
    report: selection?.kind === "report" ? `报告 · ${selection.id}` : undefined,
    research: selection?.kind === "research" && selection.id ? `研究 · ${selection.id}` : "研究资料",
    artifactCatalog: "Artifact 目录",
    artifact: selection?.kind === "artifact" ? `Artifact · ${selection.id}` : undefined,
  };
  const inspectorTitle = selection ? titles[selection.kind] || "选中对象" : "任务资料";
  const links: Array<[string, string, IconName]> = [
    ["exchanges", "请求与协议证据", "activity"],
    ["assets", "长期资产与导入", "database"],
    ["monitoring", "持续监控与变化", "clock"],
    ["findings", "Finding 与主张", "listChecks"],
    ["reports", "冻结报告与交付", "file"],
    ["research", "研究计划与业务线索", "book"],
    ["artifacts", "Artifact 原文与来源", "archive"],
  ];
  return <main className={`src-task-workbench${selection ? " has-inspector" : ""}${catalogOpen ? " has-catalog" : ""}${demo ? " src-preview-workbench" : ""}`}>
    {demo ? <div className="src-preview-banner"><Icon name="info" size={14} /><span>{SRC_DEMO_DISCLAIMER}</span><ChatButton size="xs" icon="layers" onClick={() => navigation.router.push("/src/prototype")}>研究工作台原型</ChatButton><ChatButton size="xs" onClick={() => navigation.router.push("/src")}>返回真实工作台</ChatButton></div> : null}
    <div className="src-task-conversation" inert={narrowPanel && panelOpen ? true : undefined} aria-hidden={narrowPanel && panelOpen ? true : undefined}><ConversationShell threadId={task.binding.thread_id} navigation={navigation} mainContentId="src-main-content" sidebarId="src-conversation-sidebar" experience={{ ...srcConversation.experience, headerActions: <nav className="src-chat-actions" aria-label="SRC 研究资料">
      <ChatButton size="sm" icon="target" active={catalogOpen} onClick={() => catalogOpen ? closeInspector() : go("runs")}>概览</ChatButton>
      <ChatButton size="sm" icon="book" active={selection?.kind === "research"} onClick={() => go("research")}>资料</ChatButton>
      <ChatButton size="sm" icon="activity" active={selection?.kind === "exchange" || selection?.kind === "artifact" || selection?.kind === "artifactCatalog"} onClick={() => go("exchanges")}>证据</ChatButton>
      <ChatButton size="sm" icon="file" active={selection?.kind === "report" || selection?.kind === "reportCatalog"} onClick={() => go("reports")}>报告</ChatButton>
      <ChatButton size="sm" icon="shield" active={selection?.kind === "policy" || selection?.kind === "grants" || selection?.kind === "approval"} onClick={() => go("policy")}>权限</ChatButton>
      <ChatButton size="sm" icon="layers" onClick={() => go("workbench")}>工作台</ChatButton>
    </nav> }} /></div>
    {srcConversation.error ? <div className="src-task-list-error"><SrcError error={srcConversation.error} onRetry={() => void srcConversation.reload()} /></div> : null}
    {catalogOpen ? <aside ref={panelRef} tabIndex={-1} className="src-catalog" aria-label="任务资料">
      <header className="src-inspector-header"><div><h2>{task.title}</h2><p>SRC 研究概览</p></div><IconButton icon="x" label="关闭研究概览" onClick={closeInspector} /></header>
      <div className="src-catalog-body">
        <SrcError error={directoryResource.error} onRetry={() => void directoryResource.reload()} />
        <SrcSection title="任务目标"><p className="src-prose">{task.input.goal || "尚未记录具体目标；可在同一聊天中补充。"}</p><SrcDefinitions rows={[["Task", <code key="task">{task.task_id}</code>], ["主 Thread", <code key="thread">{task.binding.thread_id}</code>], ["授权目标", task.input.authorization?.targets?.join("\n") || "仅公开资料研究；新增主动目标需明确授权"]]} /></SrcSection>
        <SrcSection title="任务资料视图">
          <div className="src-record-list">{links.map(([path, label, icon]) => <button key={path} onClick={() => go(path)}><span><strong><Icon name={icon} size={14} />{label}</strong></span><Icon name="chevronRight" size={14} /></button>)}</div>
        </SrcSection>
        <SrcSection title="深入 Run" count={runs.length} actions={<Button size="sm" variant="secondary" onPress={() => setCreating(value => !value)}><Icon name="plus" size={13} />具体目标</Button>}>
          {creating ? <div className="src-create-run"><label className="src-field">完整 URL<input type="url" value={target} onChange={event => setTarget(event.target.value)} placeholder="https://example.com/api" /></label><label className="src-field">待回答问题<textarea rows={3} value={question} onChange={event => setQuestion(event.target.value)} /></label><label className="src-field">稳定 binding key（可选）<input value={bindingKey} onChange={event => setBindingKey(event.target.value)} placeholder="同一深入目标续跑复用" /></label><SrcError error={createError} /><Button size="sm" variant="primary" isPending={submitting} isDisabled={!target.trim() || !question.trim() || submitting} onPress={() => void createRun()}>绑定 Run，不自动启动</Button></div> : null}
          <SrcError error={runsResource.error} onRetry={() => void runsResource.reload()} />
          {!runs.length ? <SrcEmpty>尚无深入 Run。只有完整 URL 和具体问题才能创建；公司名或搜索候选不会直接下发。</SrcEmpty> : <div className="src-record-list">{runs.map(row => <button key={row.binding.run_id} onClick={() => go(`runs/${encodeURIComponent(row.binding.run_id)}`)}><span><strong>{row.binding.run_id}</strong><small>binding v{row.binding.binding_revision} · generation {row.run.generation}</small></span><SrcStatus tone={stateTone(row.run.state)}>{row.run.state}</SrcStatus></button>)}</div>}
        </SrcSection>
        <SrcSection title="来源水位" count={task.watermarks.length}>
          {!task.watermarks.length ? <SrcEmpty>各 Run 尚未提交投影事件；索引落后或失败会在这里显示，不等于“没有结果”。</SrcEmpty> : <div className="src-watermarks">{task.watermarks.map((row, index) => <article key={index}><code>{String(row.source_id)}</code><SrcDefinitions rows={[["序号", String(row.next_seq ?? 0)], ["generation", String(row.generation ?? 0)], ["状态", String(row.state || "pending")], ["错误", String(row.error || "无")]]} /></article>)}</div>}
        </SrcSection>
      </div>
    </aside> : null}
    {selection && selection.kind !== "catalog" ? <section ref={panelRef} tabIndex={-1} className="src-inspector-wrap" aria-label={inspectorTitle}><header className="src-inspector-title"><strong>{inspectorTitle}</strong><IconButton icon="x" label="关闭资料面板" onClick={closeInspector} /></header><SrcError error={directoryResource.error} onRetry={() => void directoryResource.reload()} />{inspector}</section> : null}
  </main>;
}
