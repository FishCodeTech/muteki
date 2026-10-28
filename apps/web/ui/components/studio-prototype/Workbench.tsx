"use client";
import { type IconName } from "@/components/Icon";


import { useMemo, useState } from "react";

import {
  Button,
  Callout,
  IconButton,
  Menu,
  MenuItem,
  MenuSeparator,
  SearchInput,
  StatusDot,
  Dialog,
  EmptyState,
  Label,
  SegmentedControl,
  Select,
  Switch,
  TextArea,
  TextField,
  toast,
} from "@/components/chat/ui";
import {
  RETEST_VERDICTS,
  REWARD_STATUSES,
  SUBMISSION_STATUSES,
  retestVerdictLabel,
  rewardStatusLabel,
  submissionStatusLabel,
  type ReportTemplateId,
  type RetestVerdict,
  type RewardStatus,
  type StudioPlatformFeedback,
  type StudioRef,
  type StudioReport,
  type StudioRetestRecord,
  type SubmissionStatus,
} from "./model";
import { openRef } from "./nav";
import {
  BTN,
  BTN_SM,
  FIELD,
  MENU_HIT,
  OPTION_HIT,
  RefLink,
  RefLinkList,
  SEGMENTED_HIT,
  SheetAskBar,
  StudioBadge,
  displayTitle,
  dragRef,
  useRememberedScroll,
  useViewing,
} from "./parts";
import { REPORT_TEMPLATES, studio, useStudioState } from "./store";
import { quoteToComposer } from "./parts";
import { SectionHeader, SurfaceToolbar } from "@/components/chat/surfaces/shared";
import { SurfaceCard, SurfaceRow } from "@/components/chat/surfaces/OverviewSurface";
import { PathBreadcrumb } from "@/components/chat/surfaces/PathBreadcrumb";
import { TodoList, type TodoItem } from "@/components/agentui/agents/todo-list";
import { MessageMarkdown } from "@/components/ai-native/message-markdown";
import { Dl, sendAbout } from "./Kit";

function buildReportMarkdown(report: StudioReport, redact = false): string {
  const mask = (text: string) =>
    redact ? text.replace(/Bearer\s+[A-Za-z0-9._-]+/g, "Bearer [REDACTED_BY_USER]") : text;
  const tpl = REPORT_TEMPLATES.find((t) => t.id === report.templateId) ?? REPORT_TEMPLATES[0];
  return [
    `# ${report.title}`,
    "",
    `- **报告编号与版本**：${report.id} (v${report.version} · ${report.status === "frozen" ? `已冻结于 ${report.frozenAt}` : "草稿"})`,
    `- **适配平台模板**：${tpl.name} (v${tpl.version})`,
    `- **关联发现与适用版本**：${report.findingId} · ${report.targetVersion}`,
    `- **计划与授权快照**：${report.planRef} · ${report.scopeRef}`,
    `- **内部交付准备度**：${report.readiness} · **CVSS 3.1**：${report.sections.cvssScore} (\`${report.sections.cvssVector}\`)`,
    `- **原值/脱敏模式**：${redact ? "用户指定脱敏派生版（部分命令需替换凭证后运行）" : "原值保真导出（保留原始字节与测试凭证）"}`,
    "",
    "## 1. 目标范围与前置条件",
    report.sections.scopeAndTarget,
    "",
    "**攻击者最低角色与前提**：",
    report.sections.preconditions,
    "",
    "## 2. 漏洞机制与正常/实际行为对照",
    report.sections.mechanism,
    "",
    "**正常行为 vs 实际观察行为**：",
    report.sections.normalVsActual,
    "",
    "## 3. 逐步复现步骤",
    ...report.sections.steps,
    "",
    "## 4. 可运行原值 PoC",
    "```bash",
    mask(report.sections.pocScript),
    "```",
    "",
    "## 5. 业务影响与边界",
    `- **已证实影响**：${report.sections.confirmedImpact}`,
    `- **条件性影响**：${report.sections.conditionalImpact}`,
    "",
    "## 6. 决定性证据包清单",
    ...report.evidenceIds.map((id) => `- \`artifacts/${id}\``),
    "",
    "## 7. 修复建议与限制说明",
    report.sections.remediation,
    "",
    `> 限制与恢复说明：${report.sections.limitations}`,
  ].join("\n");
}

function EvidenceBundleDialog({ open, onOpenChange, report }: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  report: StudioReport;
}) {
  const state = useStudioState();
  const [format, setFormat] = useState<"markdown" | "json" | "manifest">("markdown");
  const [redact, setRedact] = useState(false);

  const evidences = useMemo(
    () => report.evidenceIds.map((id) => state.evidences.find((e) => e.id === id)).filter((e): e is NonNullable<typeof e> => Boolean(e)),
    [report.evidenceIds, state.evidences],
  );

  const manifestObj = useMemo(() => ({
    schema: "muteki-src-evidence-bundle-manifest.v1",
    report_id: report.id,
    version: report.version,
    status: report.status,
    template_id: report.templateId,
    redacted_derivative: redact,
    completeness: evidences.some((e) => e.completeness !== "complete") ? "partial" : "complete",
    exported_at: new Date().toISOString(),
    entries: [
      { path: "report.md", media_type: "text/markdown", category: redact ? "derived_redacted" : "original", purpose: "提交正文" },
      { path: "report.json", media_type: "application/json", category: "original", purpose: "规范结构化报告" },
      ...evidences.map((ev) => ({
        path: `artifacts/${ev.id}`,
        evidence_id: ev.id,
        title: ev.title,
        media_type: ev.mediaType,
        size: ev.size,
        sha256: ev.sha256,
        digest_verified: ev.digestVerified,
        completeness: ev.completeness,
        unreadable_reason: ev.unreadableReason ?? null,
      })),
    ],
  }), [evidences, redact, report]);

  const previewText = useMemo(() => {
    if (format === "markdown") return buildReportMarkdown(report, redact);
    if (format === "json") return JSON.stringify({ report, redacted: redact, manifest: manifestObj }, null, 2);
    return JSON.stringify(manifestObj, null, 2);
  }, [format, manifestObj, redact, report]);

  const download = () => {
    const ext = format === "markdown" ? "md" : "json";
    const mime = format === "markdown" ? "text/markdown" : "application/json";
    const url = URL.createObjectURL(new Blob([previewText], { type: `${mime};charset=utf-8` }));
    const link = document.createElement("a");
    link.href = url;
    link.download = `${report.id}-v${report.version}-${format}.${ext}`;
    document.body.appendChild(link);
    link.click();
    link.remove();
    window.setTimeout(() => URL.revokeObjectURL(url), 0);
    toast({ title: `已导出 ${report.id} (${format.toUpperCase()})`, tone: "success" });
  };

  return (
    <Dialog
      open={open}
      onOpenChange={onOpenChange}
      title={`导出报告与证据包清单 · ${report.id} (v${report.version})`}
      description="支持 Markdown、结构化 JSON 与完整证据包 Manifest（含每个附件的 SHA-256、字节数、完整性与原值/脱敏切换）。"
      icon="package"
      size="xl"
      testId="studio-report-export-dialog"
      footer={(
        <>
          <Button
            variant="secondary"
            icon="copy"
            className={BTN}
            onClick={() => {
              void navigator.clipboard?.writeText(previewText);
              toast({ title: "已复制到剪贴板", tone: "success" });
            }}
          >
            复制全文
          </Button>
          <Button variant="ghost" className={BTN} onClick={() => onOpenChange(false)}>关闭</Button>
          <Button variant="primary" icon="download" className={BTN} onClick={download}>
            下载 {format === "markdown" ? "Markdown" : format === "json" ? "规范 JSON" : "证据包 Manifest"}
          </Button>
        </>
      )}
    >
      <div className="flex flex-col gap-3">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <SegmentedControl<"markdown" | "json" | "manifest">
            value={format}
            onChange={setFormat}
            size="md"
            ariaLabel="导出格式"
            className={SEGMENTED_HIT}
            options={[
              { value: "markdown", label: "Markdown 报告正文" },
              { value: "json", label: "结构化 JSON" },
              { value: "manifest", label: `证据包清单 (${evidences.length + 2} 项)` },
            ]}
          />
          <Switch
            checked={redact}
            onCheckedChange={setRedact}
            label="生成脱敏派生版本（默认保留原值可复现字节）"
          />
        </div>
        <pre className="cx-scroll max-h-[380px] overflow-auto whitespace-pre-wrap rounded-xl border border-cx-border-subtle bg-cx-bg-subtle p-3.5 font-cx-mono text-[12.5px] leading-5 text-cx-fg">
          {previewText}
        </pre>
      </div>
    </Dialog>
  );
}

// ---------------------------------------------------------------------------
// Module 9: Report Delivery (Drafts, Templates, Manual Editor, Gap Check, Freeze, Version Diff)
// ---------------------------------------------------------------------------

function ReportDeliveryTab({ active }: { active: boolean }) {
  const state = useStudioState();
  const ui = state.workspace.reports;
  const report = state.reports.find((r) => r.id === ui.selectedReportId) ?? state.reports[0];
  const [editing, setEditing] = useState(false);
  const [exportOpen, setExportOpen] = useState(false);
  const [submitOpen, setSubmitOpen] = useState(false);
  const [feedbackOpen, setFeedbackOpen] = useState(false);
  const [diffVersion, setDiffVersion] = useState<StudioReport | null>(null);
  const [submitStatus, setSubmitStatus] = useState<SubmissionStatus>("submitted");
  const [ticketId, setTicketId] = useState("");

  useViewing(active, report ? { kind: "report", id: report.id } : null);

  if (!report) return <EmptyState icon="file" title="还没有报告" />;
  const reportRef: StudioRef = { kind: "report", id: report.id };
  const finding = state.clues.find((c) => c.id === report.findingId);
  const feedbacks = state.feedbacks.filter((f) => f.reportId === report.id);
  const retests = state.retests.filter((rt) => rt.baseReportId === report.id || rt.newReportId === report.id);
  const versions = state.reports.filter((r) => r.findingId === report.findingId).sort((a, b) => b.version - a.version);
  const gaps = report.materialGaps ?? [];
  const checklist = report.strangerChecklist ?? [];
  const passedCount = checklist.filter((c) => c.passed).length;
  const lifecycle = report.status === "frozen" ? "已冻结" : report.status === "superseded" ? "已被新版替代" : report.status === "invalidated" ? "已作废" : "草稿";
  const lifecycleTone = report.status === "frozen" ? "success" : report.status === "draft" ? "accent" : "neutral";
  const needsMore = feedbacks.some((f) => f.status !== "resolved" && (f.kind === "needs_more_evidence" || f.kind === "fix_claim"));

  const onFreeze = () => {
    const res = studio.freezeOrForkReport(report.id, "freeze");
    if (!res.ok) { toast({ title: "无法冻结", description: res.error, tone: "danger" }); return; }
    toast({ title: `已冻结报告 ${report.id} (v${report.version})`, tone: "success" });
  };
  const onFork = () => {
    const res = studio.freezeOrForkReport(report.id, "new_version");
    if (res.ok && res.newReportId) toast({ title: `已派生修订草稿 ${res.newReportId}`, tone: "success" });
  };
  const onSubmit = () => {
    studio.updateSubmissionLedger(report.id, { status: submitStatus, externalTicketId: ticketId || report.submission.externalTicketId });
    setSubmitOpen(false);
    toast({ title: "已登记提交", tone: "success" });
  };

  const primary = report.status === "draft" ? (
    <Button size="sm" variant="primary" disabled={gaps.length > 0} tooltip={gaps.length ? `还有 ${gaps.length} 项材料缺口，解决后才能冻结` : undefined} onClick={onFreeze}>冻结此版本</Button>
  ) : needsMore ? (
    <Button size="sm" variant="primary" onClick={onFork}>派生新版本</Button>
  ) : report.submission.status === "not_submitted" ? (
    <Button size="sm" variant="primary" onClick={() => setSubmitOpen(true)}>登记提交</Button>
  ) : (
    <Button size="sm" variant="primary" onClick={() => setFeedbackOpen(true)}>登记反馈</Button>
  );

  const checkItems: TodoItem[] = [
    ...checklist.map((c, i) => ({ id: `chk-${i}`, title: c.item, status: (c.passed ? "completed" : "pending") as TodoItem["status"], detail: c.note })),
    ...gaps.map((g, i) => ({ id: `gap-${i}`, title: g, status: "pending" as const })),
  ];

  const markdown = `### 目标范围与前提\n${report.sections.scopeAndTarget}\n\n${report.sections.preconditions}\n\n### 漏洞机制\n${report.sections.mechanism}\n\n${report.sections.normalVsActual}\n\n### 复现步骤\n${report.sections.steps.map((s2, i) => `${i + 1}. ${s2}`).join("\n")}\n\n\`\`\`\n${report.sections.pocScript}\n\`\`\`\n\n### 影响与修复\n**已证实影响：**${report.sections.confirmedImpact}\n\n**条件性影响：**${report.sections.conditionalImpact}\n\n**修复建议：**${report.sections.remediation}`;

  const events: Array<{ icon: IconName; title: string; subtitle: string; onClick?: () => void }> = [
    ...(report.submission.status !== "not_submitted" ? [{ icon: "send" as IconName, title: `提交 · ${submissionStatusLabel(report.submission.status)}`, subtitle: `${report.submission.platformName || "平台"} ${report.submission.externalTicketId || ""} ${report.submission.submittedAt ?? ""}`.trim() }] : []),
    ...(report.submission.rewardStatus !== "none" ? [{ icon: "trophy" as IconName, title: `奖励 · ${rewardStatusLabel(report.submission.rewardStatus)}`, subtitle: report.submission.rewardAmount ? `¥${report.submission.rewardAmount.toLocaleString()} ${report.submission.rewardCurrency}` : "未登记金额" }] : []),
    ...feedbacks.map((f): { icon: IconName; title: string; subtitle: string; onClick: () => void } => ({ icon: "messageCircle", title: f.externalTicketId, subtitle: `${f.receivedAt} · ${f.rawText.slice(0, 40)}`, onClick: () => studio.setReportsUi({ tab: "feedbacks", selectedFeedbackId: f.id }) })),
    ...retests.map((rt) => ({ icon: "refresh" as IconName, title: `复测 ${rt.targetFixVersion}`, subtitle: `${retestVerdictLabel(rt.result)} · ${rt.basisAndEvidence.slice(0, 36)}` })),
  ];

  return (
    <div className="mx-auto flex w-full max-w-[760px] flex-col gap-4 p-4 pb-10" data-testid="studio-report-delivery">
      <SurfaceCard className="p-3" {...dragRef(reportRef)}>
        <div className="flex items-start gap-3">
          <div className="min-w-0 flex-1">
            <p className="truncate text-[15px] font-semibold text-cx-fg">{displayTitle(report.title)}</p>
          </div>
          <StudioBadge tone={lifecycleTone}>{lifecycle}</StudioBadge>
        </div>
        <Dl className="mt-3" items={[
          { label: "编号", value: report.id, mono: true },
          { label: "发现", value: finding ? <button type="button" className="text-cx-accent hover:underline" onClick={() => openRef({ kind: "clue", id: finding.id })}>{displayTitle(finding.title)}</button> : report.findingId },
          { label: "模板", value: <Select<ReportTemplateId> value={report.templateId} onChange={(templateId) => studio.updateReportDraft(report.id, { templateId })} size="sm" options={REPORT_TEMPLATES.map((t) => ({ value: t.id, label: t.name }))} ariaLabel="切换模板" className="h-7 min-w-[160px] text-[13px]" popoverClassName={OPTION_HIT} /> },
          { label: "版本", value: `v${report.version}${report.supersedesReportId ? ` · 演进自 ${report.supersedesReportId}` : ""}` },
          { label: "提交", value: report.submission.externalTicketId || "未提交", mono: Boolean(report.submission.externalTicketId) },
        ]} />
        <div className="mt-3 flex items-center gap-2">{primary}</div>
      </SurfaceCard>

      {gaps.length && report.evidenceIds.includes("ART-REV-009") ? (
        <Callout tone="warning" action={<Button size="sm" variant="secondary" onClick={() => { studio.updateReportDraft(report.id, { evidenceIds: report.evidenceIds.filter((id) => id !== "ART-REV-009").concat(["EX-014", "ART-008"]), sections: { pocVerified: true } }); toast({ title: "已替换撤回证据", tone: "success" }); }}>一键替换撤回证据</Button>}>
          引用的证据 ART-REV-009 已被撤回
        </Callout>
      ) : null}

      {checkItems.length ? (
        <TodoList
          items={checkItems}
          collapseOnComplete
          title={(
            <span className="flex w-full items-center justify-between gap-2">
              <span>冻结前检查 {passedCount}/{checklist.length + gaps.length}</span>
              {gaps.length ? <button type="button" className="text-[12px] font-medium text-cx-accent hover:underline" onClick={(e) => { e.stopPropagation(); sendAbout(reportRef, `报告 ${report.id} 还缺 ${gaps.length} 项材料，请帮我补齐。`); }}>让 Agent 补齐</button> : null}
            </span>
          )}
        />
      ) : null}

      <div>
        <SectionHeader title="正文" action={<button type="button" className="text-[12px] font-medium text-cx-accent hover:underline" onClick={() => setEditing((v) => !v)}>{editing ? "完成" : "编辑"}</button>} />
        {editing ? (
          <div className="flex flex-col gap-3">
            <TextArea label="漏洞机制" value={report.sections.mechanism} onChange={(e) => studio.updateReportDraft(report.id, { sections: { mechanism: e.target.value } })} className="min-h-[88px] text-[13px]" />
            <TextArea label="复现步骤（每行一步）" value={report.sections.steps.join("\n")} onChange={(e) => studio.updateReportDraft(report.id, { sections: { steps: e.target.value.split("\n") } })} className="min-h-[110px] text-[13px]" />
            <TextArea label="PoC 脚本" value={report.sections.pocScript} onChange={(e) => studio.updateReportDraft(report.id, { sections: { pocScript: e.target.value } })} className="min-h-[90px] font-cx-mono text-[12.5px]" />
            <TextArea label="已证实影响" value={report.sections.confirmedImpact} onChange={(e) => studio.updateReportDraft(report.id, { sections: { confirmedImpact: e.target.value } })} className="min-h-[70px] text-[13px]" />
            <TextArea label="修复建议" value={report.sections.remediation} onChange={(e) => studio.updateReportDraft(report.id, { sections: { remediation: e.target.value } })} className="min-h-[70px] text-[13px]" />
          </div>
        ) : (
          <div className="rounded-2xl border border-cx-border-subtle bg-cx-elevated p-4" data-testid="studio-report-preview">
            <MessageMarkdown text={markdown} />
          </div>
        )}
      </div>

      <div>
        <SectionHeader title="进展" count={events.length} />
        <SurfaceCard>
          {events.length ? events.map((ev, i) => <SurfaceRow key={i} icon={ev.icon} title={ev.title} subtitle={ev.subtitle} onClick={ev.onClick} />) : <SurfaceRow icon="history" title="暂无提交或反馈" />}
          {report.submission.rewardStatus !== "none" ? (
            <div className="px-2 py-2">
              <Dl items={[
                { label: "奖励状态", value: <Select<RewardStatus> value={report.submission.rewardStatus} onChange={(rewardStatus) => studio.updateSubmissionLedger(report.id, { rewardStatus })} size="sm" options={REWARD_STATUSES.map((r) => ({ value: r.value, label: r.label }))} ariaLabel="奖励状态" className="h-7 min-w-[120px] text-[13px]" popoverClassName={OPTION_HIT} /> },
                { label: "金额", value: report.submission.rewardAmount ? `¥${report.submission.rewardAmount.toLocaleString()} ${report.submission.rewardCurrency}` : "未登记" },
                { label: "备注", value: report.submission.notes || "—" },
              ]} />
            </div>
          ) : null}
        </SurfaceCard>
      </div>

      <div>
        <SectionHeader title="版本" count={versions.length} />
        <SurfaceCard>
          {versions.map((v) => (
            <SurfaceRow key={v.id} icon="history" title={`v${v.version}${v.id === report.id ? " · 当前" : ""}`} subtitle={`${v.createdAt} · ${v.status === "frozen" ? "已冻结" : "草稿"}`} selected={v.id === report.id} onClick={v.id === report.id ? undefined : () => setDiffVersion(v)} />
          ))}
        </SurfaceCard>
      </div>

      <EvidenceBundleDialog open={exportOpen} onOpenChange={setExportOpen} report={report} />
      <Dialog open={submitOpen} onOpenChange={setSubmitOpen} title="登记提交" size="md" footer={<><Button variant="ghost" onClick={() => setSubmitOpen(false)}>取消</Button><Button variant="primary" onClick={onSubmit}>登记</Button></>}>
        <div className="flex flex-col gap-3">
          <div className="flex flex-col"><Label>提交状态</Label><Select<SubmissionStatus> value={submitStatus} onChange={setSubmitStatus} options={SUBMISSION_STATUSES.map((s2) => ({ value: s2.value, label: s2.label }))} ariaLabel="提交状态" className={FIELD} popoverClassName={OPTION_HIT} /></div>
          <TextField label="外部编号" value={ticketId || report.submission.externalTicketId} onChange={(e) => setTicketId(e.target.value)} placeholder="BSRC-2026-xxxxx" className={FIELD} />
        </div>
      </Dialog>
      <Dialog open={feedbackOpen} onOpenChange={setFeedbackOpen} title="登记平台反馈" size="md" footer={<><Button variant="ghost" onClick={() => setFeedbackOpen(false)}>取消</Button><Button variant="primary" onClick={() => { const id = studio.addPlatformFeedback({ reportId: report.id, externalTicketId: report.submission.externalTicketId || "BSRC-DRAFT", platformAuthor: "平台管理员", rawText: "", kind: "needs_more_evidence" }); setFeedbackOpen(false); if (id) openRef({ kind: "feedback", id }); }}>创建</Button></>}>
        <p className="text-[13px] text-cx-fg-2">创建一条待处理的平台反馈，可在反馈详情里补全原文与附件。</p>
      </Dialog>
      {diffVersion ? <VersionDiffDialog a={report} b={diffVersion} onClose={() => setDiffVersion(null)} /> : null}
    </div>
  );
}

function VersionDiffDialog({ a, b, onClose }: { a: StudioReport; b: StudioReport; onClose: () => void }) {
  const rows: Array<[string, string, string]> = [
    ["准备度 / CVSS", `${a.readiness} · ${a.sections.cvssScore}`, `${b.readiness} · ${b.sections.cvssScore}`],
    ["证据", a.evidenceIds.join("、"), b.evidenceIds.join("、")],
    ["机制", a.sections.mechanism, b.sections.mechanism],
    ["已证实影响", a.sections.confirmedImpact, b.sections.confirmedImpact],
    ["提交", submissionStatusLabel(a.submission.status), submissionStatusLabel(b.submission.status)],
  ];
  return (
    <Dialog open onOpenChange={(o) => { if (!o) onClose(); }} title={`v${b.version} → v${a.version}`} size="lg">
      <div className="overflow-x-auto rounded-xl border border-cx-border-subtle">
        <table className="w-full border-collapse text-left text-[13px]">
          <thead><tr className="border-b border-cx-border-subtle bg-cx-bg-subtle"><th className="w-[120px] px-3 py-2 text-cx-fg-3">项</th><th className="px-3 py-2">{a.id}</th><th className="px-3 py-2 text-cx-fg-2">{b.id}</th></tr></thead>
          <tbody className="divide-y divide-cx-border-subtle">
            {rows.map(([k, va, vb]) => (
              <tr key={k} className={va !== vb ? "bg-cx-accent-soft/30" : ""}><td className="px-3 py-2 text-cx-fg-3">{k}</td><td className="px-3 py-2">{va}</td><td className="px-3 py-2 text-cx-fg-2">{vb}</td></tr>
            ))}
          </tbody>
        </table>
      </div>
    </Dialog>
  );
}

function SubmissionAndFeedbackTab({ active }: { active: boolean }) {
  const state = useStudioState();
  const ui = state.workspace.reports;
  const feedback = state.feedbacks.find((f) => f.id === ui.selectedFeedbackId);

  useViewing(active, feedback ? { kind: "feedback", id: feedback.id } : null);

  if (feedback) return <FeedbackDetail key={feedback.id} feedback={feedback} onBack={() => studio.setReportsUi({ selectedFeedbackId: "" })} />;

  return (
    <div className="mx-auto flex w-full max-w-[760px] flex-col gap-4 p-4 pb-10" data-testid="studio-submission-ledger">
      <div>
        <SectionHeader title="平台反馈" count={state.feedbacks.length} />
        <SurfaceCard>
          {state.feedbacks.map((f) => (
            <SurfaceRow
              key={f.id}
              icon="messageCircle"
              title={f.externalTicketId}
              subtitle={`${f.receivedAt} · ${f.status === "resolved" ? "已解决" : f.status === "in_progress" ? "处理中" : "待处理"}`}
              trailing={f.status !== "resolved" ? <StatusDot tone="warning" /> : undefined}
              onClick={() => studio.setReportsUi({ selectedFeedbackId: f.id })}
            />
          ))}
          {!state.feedbacks.length ? <SurfaceRow icon="checkCheck" title="暂无平台反馈" /> : null}
        </SurfaceCard>
      </div>
    </div>
  );
}

const FEEDBACK_KIND: Record<StudioPlatformFeedback["kind"], string> = {
  needs_more_evidence: "需要补充证据",
  fix_claim: "要求修正结论",
  scope_question: "范围问题",
  accepted_notice: "已确认接收",
  duplicate_notice: "判定重复",
};

function FeedbackDetail({ feedback, onBack }: { feedback: StudioPlatformFeedback; onBack: () => void }) {
  const state = useStudioState();
  const report = state.reports.find((r) => r.id === feedback.reportId);
  const task = feedback.followUpTaskId ? state.tasks.find((t) => t.id === feedback.followUpTaskId) : undefined;
  const ref: StudioRef = { kind: "feedback", id: feedback.id };
  const reportTitle = report ? displayTitle(report.title) : feedback.reportId;

  const createTask = () => {
    const id = studio.createTaskFromFeedback(feedback.id);
    if (id) toast({ title: "已创建跟进任务", tone: "success", action: { label: "查看", onClick: () => openRef({ kind: "task", id }) } });
  };

  return (
    <div className="flex min-h-0 flex-1 flex-col" data-testid="studio-feedback-detail">
      <SurfaceToolbar>
        <IconButton size="sm" icon="arrowLeft" label="返回" onClick={onBack} />
        <PathBreadcrumb path={`${reportTitle}/${feedback.externalTicketId}`} rootLabel="报告" lastIsFile onSelect={(dir) => { if (dir === "" || dir === reportTitle) { onBack(); } }} />
        <Menu ariaLabel="反馈操作" className={MENU_HIT} trigger={<IconButton size="sm" icon="more" label="更多" />}>
          {feedback.status !== "resolved" ? <MenuItem icon="check" onSelect={() => studio.createTaskFromFeedback(feedback.id)}>创建跟进并标记处理中</MenuItem> : null}
          <MenuItem icon="pin" onSelect={() => quoteToComposer(feedback.rawText || feedback.externalTicketId, ref)}>固定到对话</MenuItem>
        </Menu>
      </SurfaceToolbar>
      <div className="cx-scroll min-h-0 flex-1 overflow-y-auto">
        <div className="mx-auto flex w-full max-w-[760px] flex-col gap-4 p-4 pb-10">
          <SurfaceCard className="p-3">
            <div className="flex items-start gap-3">
              <div className="min-w-0 flex-1">
                <p className="truncate text-[15px] font-semibold text-cx-fg">{feedback.externalTicketId}</p>
                <p className="mt-0.5 truncate text-[12px] text-cx-fg-4">{FEEDBACK_KIND[feedback.kind]}</p>
              </div>
              <StudioBadge tone={feedback.status === "resolved" ? "success" : "warning"}>{feedback.status === "resolved" ? "已解决" : feedback.status === "in_progress" ? "处理中" : "待处理"}</StudioBadge>
            </div>
            <Dl className="mt-3" items={[
              { label: "平台", value: report?.submission.platformName || "平台" },
              { label: "反馈人", value: feedback.platformAuthor },
              { label: "接收时间", value: feedback.receivedAt },
              { label: "关联报告", value: report ? <button type="button" className="text-cx-accent hover:underline" onClick={() => studio.setReportsUi({ tab: "reports", view: "report", selectedReportId: report.id })}>{`${report.id} · v${report.version}`}</button> : feedback.reportId },
            ]} />
            <div className="mt-3 flex items-center gap-2">
              <Button size="sm" variant="primary" onClick={createTask} disabled={Boolean(task)}>{task ? `已建任务 ${task.id}` : "创建跟进任务"}</Button>
            </div>
          </SurfaceCard>

          <div>
            <SectionHeader title="原文" action={<button type="button" className="text-[12px] font-medium text-cx-accent hover:underline" onClick={() => sendAbout(ref, `请根据平台反馈 ${feedback.id}（${feedback.externalTicketId}）起草回复并说明需要的补充材料。`)}>让 Agent 起草回复</button>} />
            <blockquote className="rounded-xl bg-cx-bg-subtle px-4 py-3 text-[13px] leading-6 text-cx-fg-2">{feedback.rawText || "（待补全原文）"}</blockquote>
          </div>

          {feedback.attachments.length ? (
            <div>
              <SectionHeader title="附件" count={feedback.attachments.length} />
              <SurfaceCard>
                {feedback.attachments.map((att, i) => (
                  <SurfaceRow key={i} icon="paperclip" title={att.name} subtitle={[att.mediaType, att.summary].filter(Boolean).join(" · ")} onClick={att.evidenceId ? () => openRef({ kind: "evidence", id: att.evidenceId! }) : undefined} />
                ))}
              </SurfaceCard>
            </div>
          ) : null}
        </div>
      </div>
    </div>
  );
}

function RetestFollowUpTab() {
  const state = useStudioState();
  const [newOpen, setNewOpen] = useState(false);
  const [findingId, setFindingId] = useState("F-052");
  const [targetFixVersion, setTargetFixVersion] = useState("snapshot-v47 · gateway-2.15.2");
  const [verdict, setVerdict] = useState<RetestVerdict>("fixed");
  const [prereq, setPrereq] = useState("测试账号 tenant-a-tester 登录状态有效，目标服务已确认更新至待验证版本。");
  const [normalCtrl, setNormalCtrl] = useState("同租户正常请求 GET/POST 返回 200 OK，正常业务功能未受影响。");
  const [basis, setBasis] = useState("重放原 PoC 请求与编码变体，网关均返回 403 Forbidden 且未再返回跨租户字段。");

  return (
    <div className="mx-auto flex max-w-[800px] flex-col gap-5 px-4 pb-10 pt-3" data-testid="studio-retest-tab">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div>
          <h3 className="text-[16px] font-semibold text-cx-fg">后续复核记录（已解决 / 仍存在 / 无法判断 三态契约）</h3>
          <p className="text-[12.5px] text-cx-fg-3">
            复核先确认复现前提与正常业务对照；账号失效或超时只能归入「无法判断」，绝不伪报为「已解决」。
          </p>
        </div>
        <Button size="md" variant="primary" icon="plus" className={BTN_SM} onClick={() => setNewOpen(true)}>
          登记新复核记录…
        </Button>
      </div>

      <div className="flex flex-col gap-3">
        {state.retests.map((rt: StudioRetestRecord) => (
          <section key={rt.id} className="flex flex-col gap-3 rounded-2xl border border-cx-border-subtle bg-cx-elevated p-4" data-testid="studio-retest-card">
            <div className="flex flex-wrap items-center justify-between gap-2">
              <div className="flex flex-wrap items-center gap-1.5">
                <StudioBadge
                  tone={rt.result === "fixed" ? "success" : rt.result === "still_vulnerable" ? "danger" : "warning"}
                >
                  {retestVerdictLabel(rt.result)}
                </StudioBadge>
                <span className="text-[15px] font-semibold text-cx-fg">
                  {rt.id} · 发现 {rt.findingId} · 适用版本 {rt.targetFixVersion}
                </span>
              </div>
              <span className="text-[12.5px] text-cx-fg-3">复核人：{rt.reviewer} · {rt.reviewedAt}</span>
            </div>

            <div className="grid grid-cols-2 gap-3 rounded-xl bg-cx-bg-subtle p-3 text-[13px] leading-5 max-sm:grid-cols-1">
              <div>
                <div className="font-medium text-cx-fg">1. 前置条件与身份核验</div>
                <p className="mt-1 text-cx-fg-2">{rt.prerequisiteCheck}</p>
              </div>
              <div>
                <div className="font-medium text-cx-fg">2. 正常业务对照检查</div>
                <p className="mt-1 text-cx-fg-2">{rt.normalControlCheck}</p>
              </div>
            </div>

            <div className="text-[13.5px] leading-6 text-cx-fg">
              <strong>3. 复核依据与实测观察：</strong>{rt.basisAndEvidence}
            </div>

            <div className="flex flex-wrap items-center gap-1.5">
              <span className="text-[12.5px] text-cx-fg-3">关联报告、反馈与证据：</span>
              <RefLink target={{ kind: "clue", id: rt.findingId }} />
              <RefLink target={{ kind: "report", id: rt.baseReportId }} />
              {rt.newReportId ? <RefLink target={{ kind: "report", id: rt.newReportId }} /> : null}
              {rt.feedbackId ? <RefLink target={{ kind: "feedback", id: rt.feedbackId }} /> : null}
              <RefLinkList targets={rt.evidenceIds.map((id): StudioRef => ({ kind: "evidence", id }))} max={4} />
            </div>
          </section>
        ))}
      </div>

      <Dialog
        open={newOpen}
        onOpenChange={setNewOpen}
        title="登记后续复核记录"
        description="绑定发现、原冻结报告版本与待验证修复版本，记录前置条件、正常对照与三态复核结论。"
        icon="refresh"
        size="lg"
        footer={(
          <>
            <Button variant="ghost" className={BTN} onClick={() => setNewOpen(false)}>取消</Button>
            <Button
              variant="primary"
              className={BTN}
              onClick={() => {
                const rep = state.reports.find((r) => r.findingId === findingId) ?? state.reports[0];
                const id = studio.recordRetest({
                  findingId,
                  baseReportId: rep?.id ?? "R-052",
                  targetFixVersion,
                  result: verdict,
                  prerequisiteCheck: prereq,
                  normalControlCheck: normalCtrl,
                  basisAndEvidence: basis,
                  evidenceIds: ["EX-014", "EX-019"],
                });
                setNewOpen(false);
                toast({ title: `已保存复核记录 ${id}`, tone: "success" });
              }}
            >
              保存复核记录
            </Button>
          </>
        )}
      >
        <div className="flex flex-col gap-3">
          <div className="grid grid-cols-2 gap-3 max-sm:grid-cols-1">
            <TextField label="关联发现编号" value={findingId} onChange={(e) => setFindingId(e.target.value)} className={FIELD} />
            <TextField label="验证目标修复版本" value={targetFixVersion} onChange={(e) => setTargetFixVersion(e.target.value)} className={FIELD} />
          </div>
          <SegmentedControl<RetestVerdict>
            value={verdict}
            onChange={setVerdict}
            size="md"
            ariaLabel="三态复核结论"
            className={SEGMENTED_HIT}
            options={RETEST_VERDICTS.map((v) => ({ value: v.value, label: v.label }))}
          />
          <TextArea label="原复现前提与账号核验" value={prereq} onChange={(e) => setPrereq(e.target.value)} className="min-h-[68px] text-[13.5px]" />
          <TextArea label="正常业务对照结果" value={normalCtrl} onChange={(e) => setNormalCtrl(e.target.value)} className="min-h-[68px] text-[13.5px]" />
          <TextArea label="复核依据与实测观察" value={basis} onChange={(e) => setBasis(e.target.value)} className="min-h-[84px] text-[13.5px]" />
        </div>
      </Dialog>
    </div>
  );
}

export function ReportsWorkspace({ active, sheet = false }: { active: boolean; sheet?: boolean }) {
  const state = useStudioState();
  const ui = state.workspace.reports;
  const tab = ui.tab ?? "reports";
  const view = ui.view ?? "list";
  const scroll = useRememberedScroll("reports:list", active);
  const [query, setQuery] = useState("");
  const report = state.reports.find((r) => r.id === ui.selectedReportId);

  const confirmed = state.reports.reduce((sum, r) => sum + (r.submission.rewardStatus === "confirmed" || r.submission.rewardStatus === "paid" ? r.submission.rewardAmount ?? 0 : 0), 0);
  const paid = state.reports.reduce((sum, r) => sum + (r.submission.rewardStatus === "paid" ? r.submission.rewardAmount ?? 0 : 0), 0);
  const feedbackAlert = new Set(state.feedbacks.filter((f) => f.status !== "resolved").map((f) => f.reportId));

  useViewing(active && view === "list", null);

  if (view === "report" && (report || tab !== "reports")) {
    const crumb = tab === "feedbacks" ? "提交与反馈" : tab === "retests" ? "后续复核" : report?.id ?? "";
    return (
      <div className="flex h-full min-h-0 flex-col" data-testid="studio-reports-workspace">
        <div className="flex min-h-0 flex-1 flex-col">
          <SurfaceToolbar>
            <IconButton size="sm" icon="arrowLeft" label="返回报告列表" onClick={() => studio.setReportsUi({ view: "list", tab: "reports" })} />
            <PathBreadcrumb path={crumb} rootLabel="报告" lastIsFile onSelect={() => studio.setReportsUi({ view: "list", tab: "reports" })} />
            <Menu ariaLabel="报告视图" className={MENU_HIT} trigger={<IconButton size="sm" icon="more" label="更多" />}>
              <MenuItem icon="messageCircle" onSelect={() => studio.setReportsUi({ tab: "feedbacks" })}>提交与反馈台账</MenuItem>
              <MenuItem icon="refresh" onSelect={() => studio.setReportsUi({ tab: "retests" })}>后续复核记录</MenuItem>
              <MenuSeparator />
              <MenuItem icon="file" disabled={!report} onSelect={() => studio.setReportsUi({ tab: "reports" })}>报告详情</MenuItem>
            </Menu>
          </SurfaceToolbar>
          <div className="cx-scroll min-h-0 flex-1 overflow-y-auto">
            {tab === "feedbacks" ? <SubmissionAndFeedbackTab active={active} /> : tab === "retests" ? <RetestFollowUpTab /> : <ReportDeliveryTab active={active} />}
          </div>
        </div>
        <SheetAskBar sheet={sheet} target={report ? { kind: "report", id: report.id } : null} prefill={report ? `关于报告「${report.title}」：` : undefined} />
      </div>
    );
  }

  const groups: Array<{ label: string; items: StudioReport[] }> = [
    { label: "草稿", items: state.reports.filter((r) => r.status === "draft") },
    { label: "待提交", items: state.reports.filter((r) => r.status === "frozen" && r.submission.status === "not_submitted") },
    { label: "已提交", items: state.reports.filter((r) => r.submission.status !== "not_submitted" && r.status !== "superseded" && r.status !== "invalidated") },
    { label: "已结束", items: state.reports.filter((r) => r.status === "superseded" || r.status === "invalidated") },
  ].map((g) => ({ ...g, items: g.items.filter((r) => !query.trim() || r.title.toLowerCase().includes(query.trim().toLowerCase()) || r.id.includes(query.trim())) })).filter((g) => g.items.length);

  return (
    <div className="flex h-full min-h-0 flex-col" data-testid="studio-reports-workspace">
      <SurfaceToolbar>
        <SearchInput value={query} onValueChange={setQuery} placeholder="搜索报告" aria-label="搜索报告" size="sm" className="min-w-0 flex-1" />
        <Menu ariaLabel="更多" className={MENU_HIT} trigger={<IconButton size="sm" icon="more" label="更多" />}>
          <MenuItem icon="messageCircle" onSelect={() => studio.setReportsUi({ tab: "feedbacks", view: "report" })}>提交与反馈台账</MenuItem>
          <MenuItem icon="refresh" onSelect={() => studio.setReportsUi({ tab: "retests", view: "report" })}>后续复核记录</MenuItem>
          <MenuSeparator />
          <MenuItem icon="download" onSelect={() => toast({ title: "台账导出", description: "演示环境未生成文件。", tone: "neutral" })}>导出台账</MenuItem>
        </Menu>
      </SurfaceToolbar>
      <p className="px-4 pb-0 pt-2 text-[12px] text-cx-fg-4">已确认 ¥{confirmed.toLocaleString()} · 已到账 ¥{paid.toLocaleString()}</p>
      <div ref={scroll.ref} className="cx-scroll min-h-0 flex-1 overflow-y-auto p-4">
        {groups.length ? (
          <div className="flex flex-col gap-4">
            {groups.map((group) => (
              <div key={group.label}>
                <SectionHeader title={group.label} count={group.items.length} />
                <SurfaceCard>
                  {group.items.map((r) => (
                    <SurfaceRow
                      key={r.id}
                      icon="file"
                      title={displayTitle(r.title)}
                      subtitle={r.status === "draft" ? `v${r.version} · 缺 ${r.materialGaps?.length ?? 0} 项` : `${r.submission.externalTicketId || "未登记编号"} · ${submissionStatusLabel(r.submission.status)}${r.submission.rewardAmount ? ` ¥${r.submission.rewardAmount.toLocaleString()}` : ""}`}
                      trailing={feedbackAlert.has(r.id) ? <StatusDot tone="accent" /> : undefined}
                      onClick={() => studio.setReportsUi({ tab: "reports", view: "report", selectedReportId: r.id })}
                    />
                  ))}
                </SurfaceCard>
              </div>
            ))}
          </div>
        ) : (
          <EmptyState compact icon="file" title="还没有报告" description="从发现详情里起草报告，或让 AI 帮你生成。" />
        )}
      </div>
      <SheetAskBar sheet={sheet} target={null} />
    </div>
  );
}
