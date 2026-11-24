"use client";
import { type IconName } from "@muteki/ui";


import { useEffect, useMemo, useRef, useState } from "react";

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
} from "@muteki/ui";
import {
  RETEST_VERDICTS,
  REWARD_STATUSES,
  SUBMISSION_STATUSES,
  retestVerdictLabel,
  rewardStatusLabel,
  submissionStatusLabel,
  type ReportTemplateId,
  type RewardStatus,
  type RetestVerdict,
  type StudioPlatformFeedback,
  type StudioRef,
  type StudioReport,
  type StudioRetestRecord,
  type SubmissionStatus,
} from "./model";
import { openRef, openWorkspace } from "./nav";
import {
  RefLink,
  RefLinkList,
  SheetAskBar,
  StudioBadge,
  askAi,
  displayTitle,
  dragRef,
  useRememberedScroll,
  useViewing,
} from "./parts";
import { postCommand, querySrc } from "../src/reviewPrimitives";
import { getFeedbackMeta } from "./real/reports";
import { ObjectLifecycleSection } from "./ClueBoard";
import { REPORT_TEMPLATES, afterDomainMutation, attachedTaskId, hasAttachedDomains, readStudioState, studio, useStudioState } from "./store";
import { quoteToComposer } from "./parts";
import { SectionHeader, SurfaceToolbar } from "@/components/chat/surfaces/shared";
import { SurfaceCard, SurfaceRow } from "@/components/chat/surfaces/OverviewSurface";
import { PathBreadcrumb } from "@/components/chat/surfaces/PathBreadcrumb";
import { TodoList, type TodoItem } from "@muteki/ui/agent";
import { MessageMarkdown } from "@/components/chat/markdown/MessageMarkdown";
import { Dl, sendAbout } from "./Kit";

const CURRENCY_SYMBOLS: Record<string, string> = {
  CNY: "¥",
  RMB: "¥",
  USD: "$",
  EUR: "€",
  GBP: "£",
  HKD: "HK$",
};

const PRESET_CURRENCIES = ["CNY", "USD", "EUR", "HKD"] as const;

function formatMoney(amount: number | undefined, currency = "CNY"): string {
  if (amount == null || !Number.isFinite(amount)) return "未登记金额";
  const code = (currency || "CNY").trim().toUpperCase();
  const sym = CURRENCY_SYMBOLS[code];
  return sym ? `${sym}${amount.toLocaleString()} ${code}` : `${amount.toLocaleString()} ${code}`;
}

function decodeBase64Utf8(data?: string): string {
  if (!data) return "";
  try {
    const raw = atob(data);
    const bytes = new Uint8Array(raw.length);
    for (let i = 0; i < raw.length; i += 1) bytes[i] = raw.charCodeAt(i);
    return new TextDecoder("utf-8", { fatal: false }).decode(bytes);
  } catch {
    return "";
  }
}

function buildReportMarkdown(report: StudioReport, redact = false): string {
  const mask = (text: string) => (redact ? redactTextCredentials(text) : text);
  const tpl = REPORT_TEMPLATES.find((t) => t.id === report.templateId) ?? REPORT_TEMPLATES[0];
  const isEn = report.templateId === "hackerone-en-v1" || report.templateId === "bugcrowd-en-v1";
  const isBi = report.templateId === "bilingual-src-v1";
  if (isEn) {
    return [
      `# ${report.title}`,
      "",
      `- **Report ID & Version**: ${report.id} (v${report.version} · ${report.status === "frozen" ? `Frozen at ${report.frozenAt}` : "Draft"})`,
      `- **Template**: ${tpl.name} (v${tpl.version})`,
      `- **Finding & Target Version**: ${report.findingId} · ${report.targetVersion}`,
      `- **Readiness**: ${report.readiness} · **CVSS 3.1**: ${report.sections.cvssScore} (\`${report.sections.cvssVector}\`)`,
      `- **Redaction Mode**: ${redact ? "Redacted Derivative (replace credentials before replay)" : "Original Unredacted Evidence"}`,
      "",
      "## 1. Asset Scope & Preconditions",
      report.sections.scopeAndTarget,
      "",
      "**Attacker Minimum Role & Preconditions**:",
      report.sections.preconditions,
      "",
      "## 2. Root Cause & Expected vs Observed Behavior",
      report.sections.mechanism,
      "",
      "**Expected vs Observed**:",
      report.sections.normalVsActual,
      "",
      "## 3. Steps to Reproduce",
      ...report.sections.steps,
      "",
      "## 4. Reproducible PoC",
      "```bash",
      mask(report.sections.pocScript),
      "```",
      "",
      "## 5. Impact",
      `- **Confirmed Impact**: ${report.sections.confirmedImpact}`,
      `- **Conditional Impact**: ${report.sections.conditionalImpact}`,
      ...(report.sections.counterEvidence ? [`- **Counter-evidence & Exclusions**: ${report.sections.counterEvidence}`] : []),
      "",
      "## 6. Supporting Evidence Artifacts",
      ...report.evidenceIds.map((id) => `- \`artifacts/${id}\``),
      "",
      "## 7. Concrete Remediation & Scope Limitations",
      report.sections.remediation,
      "",
      `> Limitations & Cleanup: ${report.sections.limitations}`,
    ].join("\n");
  }
  return [
    `# ${report.title}`,
    "",
    `- **${isBi ? "报告编号与版本 / Report ID" : "报告编号与版本"}**：${report.id} (v${report.version} · ${report.status === "frozen" ? `已冻结于 ${report.frozenAt}` : "草稿"})`,
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
    ...(report.sections.counterEvidence ? [`- **反证与排除项**：${report.sections.counterEvidence}`] : []),
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

function redactTextCredentials(text: string): string {
  return text
    .replace(/(Bearer\s+)[A-Za-z0-9._~+/=-]+/gi, "$1[REDACTED]")
    .replace(/^((?:authorization|proxy-authorization|cookie|cookie2|set-cookie|x-token|x-auth-token|x-api-key|satoken)\s*:\s*).+$/gim, "$1[REDACTED]");
}

function redactValueDeep(value: unknown): unknown {
  if (typeof value === "string") return redactTextCredentials(value);
  if (Array.isArray(value)) return value.map(redactValueDeep);
  if (value && typeof value === "object") {
    const out: Record<string, unknown> = {};
    for (const [k, v] of Object.entries(value as Record<string, unknown>)) {
      if (/^(authorization|proxy-authorization|cookie|cookie2|set-cookie|x-token|x-auth-token|x-api-key|satoken)$/i.test(k)) {
        out[k] = "[REDACTED]";
      } else {
        out[k] = redactValueDeep(v);
      }
    }
    return out;
  }
  return value;
}

function EvidenceBundleDialog({ open, onOpenChange, report }: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  report: StudioReport;
}) {
  const state = useStudioState();
  const [format, setFormat] = useState<"markdown" | "json" | "manifest" | "docx" | "docx_per_finding">("markdown");
  const [redact, setRedact] = useState(false);
  const [remoteExport, setRemoteExport] = useState<{ key: string; text: string; b64?: string; error?: string }>({ key: "", text: "" });

  const evidences = useMemo(
    () => report.evidenceIds.map((id) => state.evidences.find((e) => e.id === id)).filter((e): e is NonNullable<typeof e> => Boolean(e)),
    [report.evidenceIds, state.evidences],
  );

  const exportKey = `${report.id}:${report.version}:${report.status}:${format}:${redact ? "1" : "0"}`;
  const [exportState, setExportState] = useState<{ key: string; loading: boolean }>({ key: "", loading: false });
  useEffect(() => {
    const taskId = attachedTaskId();
    if (!open || !hasAttachedDomains() || !taskId || report.status !== "frozen" || format === "manifest") {
      return;
    }
    let cancelled = false;
    setExportState({ key: exportKey, loading: true });
    void postCommand<{
      markdown?: string;
      document?: unknown;
      canonical?: unknown;
      bytes?: { data?: string; size?: number };
      embedded_images?: number;
      retested?: boolean;
      docx_count?: number;
    }>(taskId, "src.submission.export", {
      report_id: report.id,
      format: format === "markdown" ? "md" : format,
      allow_partial: true,
      redact_auth: redact,
      ...(format === "docx_per_finding" ? { all_frozen_reports: true } : {}),
    })
      .then((res) => {
        if (cancelled) return;
        const b64 = format === "docx" || format === "docx_per_finding" ? String(res.bytes?.data ?? "") : undefined;
        let body = "";
        if (format === "markdown") {
          body = String(res.markdown ?? decodeBase64Utf8(res.bytes?.data));
          if (!body) throw new Error("服务端未返回 Markdown 导出正文");
        } else if (format === "docx") {
          body = `OOXML .docx (${res.bytes?.size ?? 0} bytes · 嵌图 ${res.embedded_images ?? 0} 张 · ${res.retested ? "已复测" : "未复测标注"})`;
        } else if (format === "docx_per_finding") {
          body = `一洞一 Word 批量包 (${res.docx_count ?? 1} 份 .docx · ${res.bytes?.size ?? 0} bytes)`;
        } else {
          const decoded = decodeBase64Utf8(res.bytes?.data);
          body = decoded || JSON.stringify(res.canonical ?? res.document ?? res, null, 2);
        }
        setRemoteExport({ key: exportKey, text: body, b64 });
      })
      .catch((err) => {
        if (!cancelled) {
          setRemoteExport({ key: exportKey, text: "", error: err instanceof Error ? err.message : String(err) });
        }
      })
      .finally(() => {
        if (!cancelled) setExportState({ key: exportKey, loading: false });
      });
    return () => { cancelled = true; };
  }, [exportKey, format, open, redact, report.id, report.status]);

  const manifestObj = useMemo(() => ({
    schema: "muteki-src-evidence-bundle-manifest.v1",
    report_id: report.id,
    version: report.version,
    status: report.status,
    template_id: report.templateId,
    redacted_derivative: redact,
    completeness: evidences.length !== report.evidenceIds.length || evidences.some((e) => e.completeness !== "complete") ? "partial" : "complete",
    exported_at: new Date().toISOString(),
    entries: [
      { path: "report.md", media_type: "text/markdown", category: redact ? "derived_redacted" : "original", purpose: "提交正文" },
      { path: "report.json", media_type: "application/json", category: redact ? "derived_redacted" : "original", purpose: "规范结构化报告" },
      ...evidences.map((ev) => ({
        path: `artifacts/${ev.id}`,
        evidence_id: ev.id,
        title: ev.title,
        media_type: ev.mediaType,
        size: ev.size,
        artifact_id: ev.artifactId || ev.id,
        digest_verified: ev.digestVerified,
        completeness: ev.completeness,
        unreadable_reason: ev.unreadableReason ?? null,
      })),
    ],
  }), [evidences, redact, report]);

  const requiresRemoteExport = hasAttachedDomains() && report.status === "frozen" && format !== "manifest";
  const remoteError = remoteExport.key === exportKey ? remoteExport.error : undefined;
  const exportLoading = requiresRemoteExport && exportState.key === exportKey && exportState.loading;
  const exportBlocked = requiresRemoteExport && (exportLoading || Boolean(remoteError) || remoteExport.key !== exportKey || !remoteExport.text);

  const previewText = useMemo(() => {
    if (requiresRemoteExport) {
      if (remoteExport.key === exportKey && remoteExport.text) return remoteExport.text;
      if (remoteError) return `服务端导出失败：${remoteError}`;
      return "正在从服务端生成并校验导出内容…";
    }
    if (format === "docx" || format === "docx_per_finding") return "仅已连接可信后端并冻结报告后可生成 OOXML .docx";
    if (format === "markdown") return redact ? redactTextCredentials(buildReportMarkdown(report, true)) : buildReportMarkdown(report, false);
    if (format === "json") {
      const safeReport = redact ? redactValueDeep(report) : report;
      return JSON.stringify({ report: safeReport, redacted: redact, manifest: manifestObj }, null, 2);
    }
    return JSON.stringify(manifestObj, null, 2);
  }, [exportKey, format, manifestObj, redact, remoteError, remoteExport, report, requiresRemoteExport]);

  const download = () => {
    if (exportBlocked) {
      toast({
        title: exportLoading ? "正在生成导出内容" : "导出受阻",
        description: remoteError || "必须使用服务端冻结导出结果后才可下载，避免本地绕过脱敏",
        tone: "warning",
      });
      return;
    }
    if (format === "docx" || format === "docx_per_finding") {
      if (remoteExport.key !== exportKey || !remoteExport.b64) {
        toast({ title: "尚未就绪", description: "OOXML .docx 需要已冻结报告并由后端实时生成", tone: "warning" });
        return;
      }
      const raw = atob(remoteExport.b64);
      const arr = new Uint8Array(raw.length);
      for (let i = 0; i < raw.length; i++) arr[i] = raw.charCodeAt(i);
      const mime = format === "docx_per_finding"
        ? "application/zip"
        : "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
      const url = URL.createObjectURL(new Blob([arr], { type: mime }));
      const link = document.createElement("a");
      link.href = url;
      link.download = format === "docx_per_finding"
        ? `${report.id}-per-finding-docx.zip`
        : `${report.id}-v${report.version}.docx`;
      document.body.appendChild(link);
      link.click();
      link.remove();
      window.setTimeout(() => URL.revokeObjectURL(url), 0);
      toast({ title: `已导出 ${report.id} (${format === "docx_per_finding" ? "一洞一 Word" : "DOCX"})`, tone: "success" });
      return;
    }
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
      title={`导出报告与证据包 · ${report.id} (v${report.version})`}
      icon="package"
      size="xl"
      testId="studio-report-export-dialog"
      footer={(
        <>
          <Button
            variant="secondary"
            icon="copy"
            disabled={exportBlocked}
            onClick={async () => {
              if (!navigator.clipboard?.writeText) {
                toast({ title: "当前浏览器不支持剪贴板写入", tone: "danger" });
                return;
              }
              try {
                await navigator.clipboard.writeText(previewText);
                toast({ title: "已复制到剪贴板", tone: "success" });
              } catch (err) {
                toast({ title: "复制失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
              }
            }}
          >
            复制全文
          </Button>
          <Button variant="ghost" onClick={() => onOpenChange(false)}>关闭</Button>
          <Button variant="primary" icon="download" disabled={exportBlocked} onClick={download}>
            下载 {format === "markdown" ? "Markdown" : format === "docx" ? "Word (.docx)" : format === "docx_per_finding" ? "一洞一 Word (.zip)" : format === "json" ? "规范 JSON" : "证据包 Manifest"}
          </Button>
        </>
      )}
    >
      <div className="flex flex-col gap-3">
        {remoteError ? (
          <Callout tone="danger" title="服务端导出失败（已阻止降级导出）">{remoteError}</Callout>
        ) : null}
        <div className="flex flex-wrap items-center justify-between gap-2">
          <SegmentedControl<"markdown" | "json" | "manifest" | "docx" | "docx_per_finding">
            value={format}
            onChange={setFormat}
            size="md"
            ariaLabel="导出格式"
            options={[
              { value: "markdown", label: "Markdown 报告正文" },
              { value: "docx", label: "Word (.docx)" },
              { value: "docx_per_finding", label: "一洞一 Word (.zip)" },
              { value: "json", label: "结构化 JSON" },
              { value: "manifest", label: `证据包清单 (${evidences.length + 2} 项)` },
            ]}
          />
          <Switch
            checked={redact}
            onCheckedChange={setRedact}
            label="脱敏导出"
          />
        </div>
        <pre className="cx-scroll max-h-[380px] overflow-auto whitespace-pre-wrap rounded-xl border border-cx-border-subtle bg-cx-bg-subtle p-3.5 font-cx-mono text-cx-xs leading-5 text-cx-fg">
          {previewText}
        </pre>
      </div>
    </Dialog>
  );
}


const LEDGER_STATUS_TO_SERVER: Record<SubmissionStatus, string> = {
  not_submitted: "not_submitted",
  submitted: "submitted",
  needs_info: "pending_info",
  accepted: "accepted",
  duplicate: "duplicate",
  rejected: "rejected",
};

const FEEDBACK_BACKEND_KINDS = [
  { value: "supplement_request", label: "补充材料请求 (supplement_request)" },
  { value: "duplicate_dispute", label: "重复争议 (duplicate_dispute)" },
  { value: "retest_notice", label: "复测通知 (retest_notice)" },
  { value: "triage_question", label: "审核提问 (triage_question)" },
  { value: "general", label: "一般反馈 (general)" },
] as const;

type BackendFeedbackKind = (typeof FEEDBACK_BACKEND_KINDS)[number]["value"];

const BACKEND_KIND_TO_UI: Record<BackendFeedbackKind, StudioPlatformFeedback["kind"]> = {
  supplement_request: "needs_more_evidence",
  duplicate_dispute: "duplicate_notice",
  retest_notice: "fix_claim",
  triage_question: "scope_question",
  general: "accepted_notice",
};

function SubmissionLedgerDialog({
  open,
  onOpenChange,
  report,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  report: StudioReport;
}) {
  const state = useStudioState();
  const srcTaskId = attachedTaskId(state);
  const sub = report.submission;
  const initialCurr = (sub.rewardCurrency || "CNY").trim().toUpperCase();
  const isPreset = (PRESET_CURRENCIES as readonly string[]).includes(initialCurr);

  const [status, setStatus] = useState<SubmissionStatus>(sub.status);
  const [platformName, setPlatformName] = useState(sub.platformName || "");
  const [ticketId, setTicketId] = useState(sub.externalTicketId || "");
  const [rewardStatus, setRewardStatus] = useState<RewardStatus>(sub.rewardStatus);
  const [amountText, setAmountText] = useState(sub.rewardAmount != null ? String(sub.rewardAmount) : "");
  const [currencyChoice, setCurrencyChoice] = useState<string>(isPreset ? initialCurr : "CUSTOM");
  const [customCurrency, setCustomCurrency] = useState<string>(isPreset ? "" : initialCurr);
  const [paidAt, setPaidAt] = useState(sub.paidAt || "");
  const [notes, setNotes] = useState(sub.notes || "");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const wasOpenRef = useRef(false);

  useEffect(() => {
    const justOpened = open && !wasOpenRef.current;
    wasOpenRef.current = open;
    if (!justOpened) return;
    const curr = (report.submission.rewardCurrency || "CNY").trim().toUpperCase();
    const preset = (PRESET_CURRENCIES as readonly string[]).includes(curr);
    setStatus(report.submission.status);
    setPlatformName(report.submission.platformName || "");
    setTicketId(report.submission.externalTicketId || "");
    setRewardStatus(report.submission.rewardStatus);
    setAmountText(report.submission.rewardAmount != null ? String(report.submission.rewardAmount) : "");
    setCurrencyChoice(preset ? curr : "CUSTOM");
    setCustomCurrency(preset ? "" : curr);
    setPaidAt(report.submission.paidAt || "");
    setNotes(report.submission.notes || "");
    setError("");
  }, [open, report]);

  const finalCurrency = (currencyChoice === "CUSTOM" ? customCurrency : currencyChoice).trim().toUpperCase() || "CNY";

  const handleSave = async () => {
    setError("");
    const trimmedAmount = amountText.trim();
    const parsedAmount = trimmedAmount === "" ? undefined : Number(trimmedAmount);
    if (parsedAmount !== undefined && (!Number.isFinite(parsedAmount) || parsedAmount < 0)) {
      setError("奖金金额必须为非负数字。");
      return;
    }
    if (!srcTaskId) {
      studio.updateSubmissionLedger(report.id, {
        status,
        platformName: platformName.trim(),
        externalTicketId: ticketId.trim(),
        rewardStatus,
        rewardAmount: parsedAmount,
        paidAt: paidAt.trim() || undefined,
        notes: notes.trim(),
      });
      onOpenChange(false);
      toast({ title: "已更新提交与奖金台账", tone: "success" });
      return;
    }
    if (report.status === "draft") {
      setError("当前为草稿版本，请先冻结报告后再登记后端提交与奖金台账。");
      return;
    }
    setBusy(true);
    try {
      const currentLedger = await querySrc<{ revision?: number }>(srcTaskId, "src.feedback.platform_status", {
        report_id: report.id,
      });
      const backendStatus =
        rewardStatus === "paid"
          ? "reward_paid"
          : rewardStatus === "confirmed"
          ? "reward_confirmed"
          : rewardStatus === "reward_pending"
          ? "reward_pending"
          : LEDGER_STATUS_TO_SERVER[status] ?? "submitted";
      await postCommand(srcTaskId, "src.platform.status.record", {
        report_id: report.id,
        status: backendStatus,
        actor_id: "local-user",
        actor_kind: "operator",
        base_revision: Number(currentLedger.revision ?? 0),
        platform_reference: ticketId.trim(),
        platform: platformName.trim() || null,
        ...(parsedAmount !== undefined ? { amount: parsedAmount } : {}),
        currency: finalCurrency,
        paid_at: paidAt.trim() || null,
        notes: notes.trim(),
      });
      await studio.reloadDomain("reports");
      onOpenChange(false);
      toast({ title: "已更新提交与奖金台账", tone: "success" });
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog
      open={open}
      onOpenChange={onOpenChange}
      title="编辑提交与奖金台账"
      description={`报告 ${report.id} (v${report.version}) 的提交状态、平台工单、币种与奖金核销记录`}
      size="lg"
      footer={
        <div className="flex items-center justify-end gap-2">
          <Button variant="ghost" disabled={busy} onClick={() => onOpenChange(false)}>取消</Button>
          <Button variant="primary" disabled={busy || (Boolean(srcTaskId) && report.status === "draft")} onClick={() => { void handleSave(); }}>
            {busy ? "保存中…" : "保存台账"}
          </Button>
        </div>
      }
    >
      <div className="space-y-3">
        {srcTaskId && report.status === "draft" ? (
          <Callout tone="warning">
            草稿报告尚未冻结，后端提交与奖金台账仅登记于已冻结版本；请先冻结报告。
          </Callout>
        ) : null}
        {srcTaskId && report.status !== "draft" && report.readiness !== "GREEN" ? (
          <Callout tone="warning">
            当前复核准备度为 {report.readiness}；如需同步更新台账备注或状态，请确认事实与证据完整。
          </Callout>
        ) : null}
        <div className="grid gap-3 sm:grid-cols-2">
          <div className="flex flex-col">
            <Label>提交状态</Label>
            <Select<SubmissionStatus>
              value={status}
              onChange={setStatus}
              options={SUBMISSION_STATUSES.map((s2) => ({ value: s2.value, label: s2.label }))}
              ariaLabel="提交状态"
            />
          </div>
          <div className="flex flex-col">
            <Label>奖励状态</Label>
            <Select<RewardStatus>
              value={rewardStatus}
              onChange={setRewardStatus}
              options={REWARD_STATUSES.map((r) => ({ value: r.value, label: r.label }))}
              ariaLabel="奖励状态"
            />
          </div>
        </div>
        <div className="grid gap-3 sm:grid-cols-2">
          <TextField label="平台名称" value={platformName} onChange={(e) => setPlatformName(e.target.value)} placeholder="如：HackerOne / Bugcrowd / 补天 / SRC" />
          <TextField label="外部工单编号" value={ticketId} onChange={(e) => setTicketId(e.target.value)} placeholder="如：H1-123456 / BSRC-2026-001" />
        </div>
        <div className="grid gap-3 sm:grid-cols-3">
          <TextField label="奖金金额" value={amountText} onChange={(e) => setAmountText(e.target.value)} placeholder="例如 500" />
          <div className="flex flex-col">
            <Label>币种</Label>
            <Select<string>
              value={currencyChoice}
              onChange={setCurrencyChoice}
              options={[
                { value: "CNY", label: "CNY (¥)" },
                { value: "USD", label: "USD ($)" },
                { value: "EUR", label: "EUR (€)" },
                { value: "HKD", label: "HKD (HK$)" },
                { value: "CUSTOM", label: "自定义币种…" },
              ]}
              ariaLabel="奖金币种"
            />
          </div>
          <TextField label="到账时间" value={paidAt} onChange={(e) => setPaidAt(e.target.value)} placeholder="YYYY-MM-DD 或 ISO 时间" />
        </div>
        {currencyChoice === "CUSTOM" ? (
          <TextField label="自定义币种代码" value={customCurrency} onChange={(e) => setCustomCurrency(e.target.value)} placeholder="如：JPY / GBP / SGD" />
        ) : null}
        <TextArea label="台账备注" value={notes} onChange={(e) => setNotes(e.target.value)} placeholder="记录平台裁决说明、评级分歧或奖金结算细节" />
        {error ? <Callout tone="danger">{error}</Callout> : null}
      </div>
    </Dialog>
  );
}

function RecordFeedbackDialog({
  open,
  onOpenChange,
  defaultReportId,
  defaultFindingId,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  defaultReportId?: string;
  defaultFindingId?: string;
}) {
  const state = useStudioState();
  const srcTaskId = attachedTaskId(state);
  const frozenReports = state.reports.filter((r) => r.status !== "draft");
  const allReports = frozenReports.length ? frozenReports : state.reports;

  const [bindType, setBindType] = useState<"report" | "finding">(defaultReportId ? "report" : defaultFindingId ? "finding" : allReports.length ? "report" : "finding");
  const [selectedReportId, setSelectedReportId] = useState(defaultReportId || allReports[0]?.id || "");
  const [selectedFindingId, setSelectedFindingId] = useState(defaultFindingId || state.clues[0]?.id || "");
  const [kind, setKind] = useState<BackendFeedbackKind>("supplement_request");
  const [ticketId, setTicketId] = useState("");
  const [author, setAuthor] = useState("platform");
  const [rawText, setRawText] = useState("");
  const [linksText, setLinksText] = useState("");
  const [notes, setNotes] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const wasOpenRef = useRef(false);

  useEffect(() => {
    const justOpened = open && !wasOpenRef.current;
    wasOpenRef.current = open;
    if (!justOpened) return;
    setError("");
    setBindType(defaultReportId ? "report" : defaultFindingId ? "finding" : allReports.length ? "report" : "finding");
    const initRep = defaultReportId || allReports[0]?.id || "";
    setSelectedReportId(initRep);
    const repObj = state.reports.find((r) => r.id === initRep);
    setSelectedFindingId(defaultFindingId || repObj?.findingId || state.clues[0]?.id || "");
    setTicketId(repObj?.submission.externalTicketId || "");
    setAuthor("platform");
    setKind("supplement_request");
    setRawText("");
    setLinksText("");
    setNotes("");
  }, [allReports, defaultFindingId, defaultReportId, open, state.clues, state.reports]);

  const submitFeedback = async () => {
    if (!rawText.trim()) {
      setError("请填写平台反馈正文。");
      return;
    }
    const reportId = bindType === "report" ? selectedReportId : "";
    const findingId = bindType === "finding"
      ? selectedFindingId
      : (state.reports.find((r) => r.id === reportId)?.findingId || selectedFindingId || "");
    if (!reportId && !findingId) {
      setError("请选择要绑定的报告或发现。");
      return;
    }
    const links = linksText.split(/[\n,]+/).map((s) => s.trim()).filter(Boolean);

    if (!srcTaskId) {
      const id = studio.addPlatformFeedback({
        reportId: reportId || allReports[0]?.id || "",
        externalTicketId: ticketId.trim() || "待补充工单编号",
        platformAuthor: author.trim() || "platform",
        kind: BACKEND_KIND_TO_UI[kind],
        rawText: rawText.trim(),
      });
      onOpenChange(false);
      openRef({ kind: "feedback", id });
      toast({ title: "已登记平台反馈", tone: "success" });
      return;
    }

    setBusy(true);
    setError("");
    try {
      const saved = await postCommand<{ feedback_id?: string }>(srcTaskId, "src.feedback.record", {
        ...(reportId ? { report_id: reportId } : {}),
        ...(findingId ? { finding_id: findingId } : {}),
        kind,
        text: rawText.trim(),
        ...(notes.trim() ? { notes: notes.trim() } : {}),
        ...(links.length ? { links } : {}),
        source: author.trim() || "platform",
        platform_reference: ticketId.trim(),
      });
      await studio.reloadDomain("reports");
      onOpenChange(false);
      toast({ title: "已登记平台反馈", tone: "success" });
      if (saved.feedback_id) openRef({ kind: "feedback", id: saved.feedback_id });
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog
      open={open}
      onOpenChange={onOpenChange}
      title="登记平台反馈"
      size="lg"
      footer={
        <div className="flex items-center justify-end gap-2">
          <Button variant="ghost" disabled={busy} onClick={() => onOpenChange(false)}>取消</Button>
          <Button variant="primary" disabled={busy || !rawText.trim()} onClick={() => { void submitFeedback(); }}>
            {busy ? "保存中…" : "登记反馈"}
          </Button>
        </div>
      }
    >
      <div className="space-y-3">
        <div className="grid gap-3 sm:grid-cols-2">
          <div className="flex flex-col">
            <Label>绑定对象类型</Label>
            <SegmentedControl<"report" | "finding">
              value={bindType}
              onChange={setBindType}
              ariaLabel="绑定对象类型"
              options={[
                { value: "report", label: "绑定到报告 (report_id)" },
                { value: "finding", label: "绑定到发现 (finding_id)" },
              ]}
            />
          </div>
          <div className="flex flex-col">
            <Label>反馈类型</Label>
            <Select<BackendFeedbackKind>
              value={kind}
              onChange={setKind}
              options={FEEDBACK_BACKEND_KINDS.map((k) => ({ value: k.value, label: k.label }))}
              ariaLabel="反馈类型"
            />
          </div>
        </div>

        {bindType === "report" ? (
          <div className="flex flex-col">
            <Label>选择报告</Label>
            <Select<string>
              value={selectedReportId}
              onChange={setSelectedReportId}
              options={allReports.map((r) => ({ value: r.id, label: `${r.id} · v${r.version} · ${displayTitle(r.title)}` }))}
              ariaLabel="绑定报告"
            />
          </div>
        ) : (
          <div className="flex flex-col">
            <Label>选择发现</Label>
            <Select<string>
              value={selectedFindingId}
              onChange={setSelectedFindingId}
              options={state.clues.map((c) => ({ value: c.id, label: `${c.id} · ${displayTitle(c.title)}` }))}
              ariaLabel="绑定发现"
            />
          </div>
        )}

        <div className="grid gap-3 sm:grid-cols-2">
          <TextField label="外部工单编号" value={ticketId} onChange={(e) => setTicketId(e.target.value)} placeholder="如：BSRC-2026-001" />
          <TextField label="来源 / 审核人" value={author} onChange={(e) => setAuthor(e.target.value)} placeholder="如：platform / triage-reviewer" />
        </div>
        <TextArea label="反馈正文（必填）" rows={3} value={rawText} onChange={(e) => setRawText(e.target.value)} placeholder="粘贴平台审核回复原文、补件要求或争议理由" />
        <TextArea label="相关链接（可选，每行或逗号分隔）" rows={2} value={linksText} onChange={(e) => setLinksText(e.target.value)} placeholder="https://platform.example.com/tickets/123" />
        <TextArea label="内部研判备注（可选）" rows={2} value={notes} onChange={(e) => setNotes(e.target.value)} placeholder="补充研判背景或需要优先核查的细节" />
        {error ? <Callout tone="danger">{error}</Callout> : null}
      </div>
    </Dialog>
  );
}

function ReportDeliveryTab({ active }: { active: boolean }) {
  const state = useStudioState();
  const ui = state.workspace.reports;
  const report = state.reports.find((r) => r.id === ui.selectedReportId) ?? state.reports[0];
  const [editing, setEditing] = useState(false);
  const [exportOpen, setExportOpen] = useState(false);
  const [ledgerOpen, setLedgerOpen] = useState(false);
  const [feedbackOpen, setFeedbackOpen] = useState(false);
  const [diffVersion, setDiffVersion] = useState<StudioReport | null>(null);

  useViewing(active, report ? { kind: "report", id: report.id } : null);

  if (!report) return <EmptyState icon="file" title="还没有报告" />;
  const reportRef: StudioRef = { kind: "report", id: report.id };
  const finding = state.clues.find((c) => c.id === report.findingId);
  const feedbacks = state.feedbacks.filter((f) => f.reportId === report.id);
  const retests = state.retests.filter((rt) => rt.baseReportId === report.id || rt.newReportId === report.id);
  const versions = state.reports.filter((r) => r.findingId === report.findingId).sort((a, b) => b.version - a.version);
  const gaps = report.materialGaps ?? [];
  const freezeGaps = report.target?.trim() ? gaps : ["请填写授权范围内的目标 URL", ...gaps];
  const checklist = report.strangerChecklist ?? [];
  const passedCount = checklist.filter((c) => c.passed).length;
  const lifecycle = report.status === "frozen" ? "已冻结" : report.status === "superseded" ? "已被新版替代" : report.status === "invalidated" ? "已作废" : "草稿";
  const lifecycleTone = report.status === "frozen" ? "success" : report.status === "draft" ? "accent" : "neutral";
  const needsMore = feedbacks.some((f) => f.status !== "resolved" && (f.kind === "needs_more_evidence" || f.kind === "fix_claim"));
  const evidenceRefs: StudioRef[] = Array.from(
    new Set([report.evidenceExchangeId, ...report.evidenceIds].filter((id): id is string => Boolean(id))),
  ).map((id) => ({ kind: "evidence", id }));

  const onFreeze = async () => {
    if (freezeGaps.length) { toast({ title: "无法冻结", description: freezeGaps[0], tone: "danger" }); return; }
    const res = studio.freezeOrForkReport(report.id, "freeze");
    const sync = await afterDomainMutation();
    if (!sync.ok || (!hasAttachedDomains() && !res.ok)) {
      toast({ title: "无法冻结", description: sync.error || res.error, tone: "danger" });
      return;
    }
    toast({ title: `已冻结报告 ${report.id} (v${report.version})`, tone: "success" });
  };
  const onFork = async () => {
    const res = studio.freezeOrForkReport(report.id, "new_version");
    const sync = await afterDomainMutation();
    if (!sync.ok) { toast({ title: "派生修订草稿失败", description: sync.error, tone: "danger" }); return; }
    const mappedId = sync.outcome?.createdId || res.newReportId;
    if (mappedId) toast({ title: `已派生修订草稿 ${mappedId}`, tone: "success" });
  };

  const primary = report.status === "draft" ? (
    <Button size="sm" variant="primary" disabled={freezeGaps.length > 0} tooltip={freezeGaps.length ? freezeGaps[0] : undefined} onClick={onFreeze}>冻结此版本</Button>
  ) : needsMore ? (
    <Button size="sm" variant="primary" onClick={onFork}>派生新版本</Button>
  ) : (
    <Button size="sm" variant="primary" onClick={() => setFeedbackOpen(true)}>登记反馈</Button>
  );

  const checkItems: TodoItem[] = [
    ...checklist.map((c, i) => ({ id: `chk-${i}`, title: c.item, status: (c.passed ? "completed" : "pending") as TodoItem["status"], detail: c.note })),
    ...gaps.map((g, i) => ({ id: `gap-${i}`, title: g, status: "pending" as const })),
  ];

  const markdown = [
    `### 目标范围与前提\n${report.sections.scopeAndTarget}\n\n${report.sections.preconditions}`,
    `### 漏洞机制\n${report.sections.mechanism}\n\n${report.sections.normalVsActual}`,
    `### 复现步骤\n${report.sections.steps.map((s2, i) => `${i + 1}. ${s2}`).join("\n")}\n\n\`\`\`\n${report.sections.pocScript}\n\`\`\``,
    `### 影响与修复\n**已证实影响：**${report.sections.confirmedImpact}\n\n**条件性影响：**${report.sections.conditionalImpact}\n\n**修复建议：**${report.sections.remediation}`,
    ...(report.sections.counterEvidence ? [`### 反证与排除项\n${report.sections.counterEvidence}`] : []),
  ].join("\n\n");

  const events: Array<{ icon: IconName; title: string; subtitle: string; onClick?: () => void }> = [
    ...(report.submission.status !== "not_submitted" ? [{ icon: "send" as IconName, title: `提交 · ${submissionStatusLabel(report.submission.status)}`, subtitle: `${report.submission.platformName || "平台"} ${report.submission.externalTicketId || ""} ${report.submission.submittedAt ?? ""}`.trim() }] : []),
    ...(report.submission.rewardStatus !== "none" ? [{ icon: "trophy" as IconName, title: `奖励 · ${rewardStatusLabel(report.submission.rewardStatus)}`, subtitle: formatMoney(report.submission.rewardAmount, report.submission.rewardCurrency) }] : []),
    ...feedbacks.map((f): { icon: IconName; title: string; subtitle: string; onClick: () => void } => ({ icon: "messageCircle", title: f.externalTicketId || f.id, subtitle: `${f.receivedAt} · ${f.rawText}`, onClick: () => studio.setReportsUi({ tab: "feedbacks", selectedFeedbackId: f.id }) })),
    ...retests.map((rt) => ({ icon: "refresh" as IconName, title: `复测 ${rt.targetFixVersion}`, subtitle: `${retestVerdictLabel(rt.result)} · ${rt.basisAndEvidence}` })),
  ];

  return (
    <div className="mx-auto flex w-full max-w-[760px] flex-col gap-4 p-4 pb-10" data-testid="studio-report-delivery">
      <SurfaceCard className="p-3" {...dragRef(reportRef)}>
        <div className="flex items-start gap-3">
          <div className="min-w-0 flex-1">
            <p className="truncate text-cx-md font-semibold text-cx-fg">{displayTitle(report.title)}</p>
          </div>
          <StudioBadge tone={lifecycleTone}>{lifecycle}</StudioBadge>
        </div>
        <Dl className="mt-3" items={[
          { label: "编号", value: report.id, mono: true },
          { label: "发现", value: finding ? <RefLink target={{ kind: "clue", id: finding.id }} /> : report.findingId },
          { label: "目标 URL", value: report.target || "待填写" },
          { label: "原始证据", value: evidenceRefs.length ? <RefLinkList targets={evidenceRefs} max={4} /> : "未关联" },
          {
            label: "计划与范围",
            value: (
              <span className="flex flex-wrap items-center gap-1.5">
                {report.planRef ? <RefLink target={{ kind: "plan", id: report.planRef }} /> : <span className="text-cx-fg-4">未绑定计划</span>}
                {report.scopeRef ? (
                  <button
                    type="button"
                    className="inline-flex h-[32px] items-center rounded-lg border border-cx-border-subtle bg-cx-bg px-2.5 text-cx-xs text-cx-fg-2 hover:bg-cx-hover hover:text-cx-fg"
                    onClick={() => { studio.setTasksUi({ tab: "scope" }); openWorkspace("tasks"); }}
                  >
                    授权快照 {report.scopeRef}
                  </button>
                ) : null}
              </span>
            ),
          },
          { label: "模板", value: report.status === "draft" ? <Select<ReportTemplateId> value={report.templateId} onChange={(templateId) => studio.updateReportDraft(report.id, { templateId })} size="sm" options={REPORT_TEMPLATES.map((t) => ({ value: t.id, label: t.name }))} ariaLabel="切换模板" className="min-w-[160px]" /> : (REPORT_TEMPLATES.find((template) => template.id === report.templateId)?.name ?? report.templateId) },
          { label: "版本", value: `v${report.version}${report.supersedesReportId ? ` · 演进自 ${report.supersedesReportId}` : ""}` },
          { label: "复核准备度", value: report.readiness === "GREEN" ? "GREEN · 已复核" : `${report.readiness} · 待复核` },
          { label: "提交台账", value: `${submissionStatusLabel(report.submission.status)}${report.submission.platformName ? ` · ${report.submission.platformName}` : ""}${report.submission.externalTicketId ? ` · ${report.submission.externalTicketId}` : ""}` },
        ]} />
        <div className="mt-3 flex flex-wrap items-center gap-2">
          {primary}
          <Button size="sm" variant="secondary" onClick={() => setLedgerOpen(true)}>
            编辑提交与奖金台账
          </Button>
          {report.status !== "draft" ? (
            <Button size="sm" variant="secondary" onClick={() => setFeedbackOpen(true)}>登记反馈</Button>
          ) : null}
          <Button size="sm" variant="secondary" icon="download" onClick={() => setExportOpen(true)}>
            导出报告与证据包…
          </Button>
        </div>
      </SurfaceCard>

      {report.draftSummary || report.draftReviewNote ? (
        <SurfaceCard className="space-y-2 p-3 text-cx-sm">
          {report.draftSummary ? <p><strong>草稿摘要：</strong>{report.draftSummary}</p> : null}
          {report.draftReviewNote ? <p><strong>复核备注：</strong>{report.draftReviewNote}</p> : null}
        </SurfaceCard>
      ) : null}

      {report.status === "frozen" && report.readiness !== "GREEN" ? (
        <Callout tone="warning">这份冻结报告的复核准备度为 {report.readiness}；安全影响仍需验证，提交前请核对正文与原始证据。</Callout>
      ) : null}

      {checkItems.length ? (
        <div className="flex flex-col gap-1.5">
          <TodoList
            items={checkItems}
            collapseOnComplete
            title={`冻结前检查 ${passedCount}/${checklist.length + gaps.length}`}
          />
          {gaps.length ? (
            <div className="flex justify-end px-1">
              <button
                type="button"
                className="text-cx-xs font-medium text-cx-accent hover:underline"
                onClick={() => sendAbout(reportRef, `报告 ${report.id} 还缺 ${gaps.length} 项材料，请帮我补齐。`)}
              >
                让 Agent 补齐
              </button>
            </div>
          ) : null}
        </div>
      ) : null}

      {attachedTaskId() ? <ObjectLifecycleSection taskId={attachedTaskId()} /> : null}

      <div>
        <SectionHeader title="正文" action={report.status === "draft" ? <button type="button" className="text-cx-xs font-medium text-cx-accent hover:underline" onClick={() => setEditing((v) => !v)}>{editing ? "完成" : "编辑"}</button> : undefined} />
        {editing && report.status === "draft" ? (
          <div className="flex flex-col gap-3">
            <TextArea label="目标 URL" value={report.target ?? ""} onChange={(e) => studio.updateReportDraft(report.id, { target: e.target.value })} className="min-h-[56px]" />
            <TextArea label="目标范围与前提" value={report.sections.scopeAndTarget} onChange={(e) => studio.updateReportDraft(report.id, { sections: { scopeAndTarget: e.target.value } })} className="min-h-[70px]" />
            <TextArea label="漏洞机制" value={report.sections.mechanism} onChange={(e) => studio.updateReportDraft(report.id, { sections: { mechanism: e.target.value } })} className="min-h-[88px]" />
            <TextArea label="复现步骤（每行一步）" value={report.sections.steps.join("\n")} onChange={(e) => studio.updateReportDraft(report.id, { sections: { steps: e.target.value.split("\n") } })} className="min-h-[110px]" />
            <TextArea label="PoC 脚本" value={report.sections.pocScript} onChange={(e) => studio.updateReportDraft(report.id, { sections: { pocScript: e.target.value } })} className="min-h-[90px] font-cx-mono text-cx-xs" />
            <TextArea label="已证实影响" value={report.sections.confirmedImpact} onChange={(e) => studio.updateReportDraft(report.id, { sections: { confirmedImpact: e.target.value } })} className="min-h-[70px]" />
            <TextArea label="反证与排除项" value={report.sections.counterEvidence ?? ""} onChange={(e) => studio.updateReportDraft(report.id, { sections: { counterEvidence: e.target.value } })} placeholder="记录替代解释、边界条件、不成立场景或人工排除理由" className="min-h-[70px]" />
            <TextArea label="修复建议" value={report.sections.remediation} onChange={(e) => studio.updateReportDraft(report.id, { sections: { remediation: e.target.value } })} className="min-h-[70px]" />
          </div>
        ) : (
          <div className="rounded-2xl border border-cx-border-subtle bg-cx-elevated p-4" data-testid="studio-report-preview">
            <MessageMarkdown text={markdown} />
          </div>
        )}
      </div>

      <div>
        <SectionHeader
          title="进展与奖金台账"
          count={events.length}
          action={<button type="button" className="text-cx-xs font-medium text-cx-accent hover:underline" onClick={() => setLedgerOpen(true)}>编辑提交与奖金台账</button>}
        />
        <SurfaceCard>
          {events.length ? events.map((ev, i) => <SurfaceRow key={i} icon={ev.icon} title={ev.title} subtitle={ev.subtitle} onClick={ev.onClick} />) : <SurfaceRow icon="history" title="暂无提交或反馈" />}
          <div className="border-t border-cx-border-subtle px-3 py-2.5">
            <Dl items={[
              { label: "奖励状态", value: rewardStatusLabel(report.submission.rewardStatus) },
              { label: "金额与币种", value: formatMoney(report.submission.rewardAmount, report.submission.rewardCurrency) },
              ...(report.submission.paidAt ? [{ label: "到账时间", value: report.submission.paidAt }] : []),
              { label: "备注", value: report.submission.notes || "—" },
            ]} />
          </div>
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
      <SubmissionLedgerDialog open={ledgerOpen} onOpenChange={setLedgerOpen} report={report} />
      <RecordFeedbackDialog
        open={feedbackOpen}
        onOpenChange={setFeedbackOpen}
        defaultReportId={report.id}
        defaultFindingId={report.findingId}
      />
      {diffVersion ? <VersionDiffDialog a={report} b={diffVersion} onClose={() => setDiffVersion(null)} /> : null}
    </div>
  );
}

type ServerReportDiffResult = {
  changed_count?: number;
  fields?: Array<{ field?: string; changed?: boolean; before?: unknown; after?: unknown }>;
  changes?: Array<{ field?: string; path?: string; before?: unknown; after?: unknown }>;
  feedback?: Array<{ feedback_id?: string; kind?: string; text?: string; platform_reference?: string }>;
};

function formatDiffVal(value: unknown): string {
  if (value == null || value === "") return "—";
  if (typeof value === "string") return value;
  try {
    return JSON.stringify(value, null, 2);
  } catch {
    return String(value);
  }
}

function VersionDiffDialog({ a, b, onClose }: { a: StudioReport; b: StudioReport; onClose: () => void }) {
  const state = useStudioState();
  const srcTaskId = attachedTaskId(state);
  const [serverDiff, setServerDiff] = useState<ServerReportDiffResult | null>(null);
  const [feedbackDiff, setFeedbackDiff] = useState<ServerReportDiffResult | null>(null);
  const [diffError, setDiffError] = useState("");
  const [loading, setLoading] = useState(false);

  const canQueryServer = Boolean(srcTaskId && a.status !== "draft" && b.status !== "draft");
  useEffect(() => {
    if (!canQueryServer || !srcTaskId) {
      setServerDiff(null);
      setFeedbackDiff(null);
      setDiffError("");
      return;
    }
    let alive = true;
    setLoading(true);
    setDiffError("");
    const tasks: Array<Promise<void>> = [
      querySrc<ServerReportDiffResult>(srcTaskId, "src.workbench.report.diff", {
        from_report_id: b.id,
        to_report_id: a.id,
      }).then((res) => { if (alive) setServerDiff(res); }),
    ];
    if (a.findingId && a.findingId === b.findingId) {
      tasks.push(
        querySrc<ServerReportDiffResult>(srcTaskId, "src.feedback.diff", {
          old_report_id: b.id,
          new_report_id: a.id,
        }).then((res) => { if (alive) setFeedbackDiff(res); }),
      );
    }
    void Promise.all(tasks)
      .catch((err) => {
        if (alive) setDiffError(err instanceof Error ? err.message : String(err));
      })
      .finally(() => {
        if (alive) setLoading(false);
      });
    return () => { alive = false; };
  }, [a.findingId, a.id, b.findingId, b.id, canQueryServer, srcTaskId]);

  const rows: Array<[string, string, string]> = [
    ["准备度 / CVSS", `${a.readiness} · ${a.sections.cvssScore}`, `${b.readiness} · ${b.sections.cvssScore}`],
    ["证据", a.evidenceIds.join("、"), b.evidenceIds.join("、")],
    ["机制", a.sections.mechanism, b.sections.mechanism],
    ["已证实影响", a.sections.confirmedImpact, b.sections.confirmedImpact],
    ["反证与排除项", a.sections.counterEvidence ?? "", b.sections.counterEvidence ?? ""],
    ["提交", submissionStatusLabel(a.submission.status), submissionStatusLabel(b.submission.status)],
  ];
  const serverFieldChanges: Array<{ field?: string; path?: string; before?: unknown; after?: unknown }> = [
    ...(serverDiff?.fields?.filter((f) => f.changed) ?? []),
    ...(feedbackDiff?.changes ?? []),
  ];

  return (
    <Dialog open onOpenChange={(o) => { if (!o) onClose(); }} title={`v${b.version} → v${a.version}`} size="lg">
      <div className="space-y-4">
        {loading ? <p className="text-cx-xs text-cx-fg-3">正在比对服务端冻结版本差异…</p> : null}
        {diffError ? <Callout tone="warning">服务端差异查询失败，以下展示前端对比：{diffError}</Callout> : null}
        {serverFieldChanges.length > 0 ? (
          <div className="overflow-x-auto rounded-xl border border-cx-border-subtle">
            <table className="w-full border-collapse text-left text-cx-xs">
              <thead>
                <tr className="border-b border-cx-border-subtle bg-cx-bg-subtle">
                  <th className="w-[140px] px-3 py-2 text-cx-fg-3">服务端变更字段</th>
                  <th className="px-3 py-2">{a.id} (新)</th>
                  <th className="px-3 py-2 text-cx-fg-2">{b.id} (旧)</th>
                </tr>
              </thead>
              <tbody className="divide-y divide-cx-border-subtle">
                {serverFieldChanges.map((item, idx) => (
                  <tr key={`${item.field ?? item.path}-${idx}`} className="bg-cx-accent-soft/20">
                    <td className="px-3 py-2 font-cx-mono text-cx-fg-3">{item.path || item.field}</td>
                    <td className="whitespace-pre-wrap break-all px-3 py-2">{formatDiffVal(item.after)}</td>
                    <td className="whitespace-pre-wrap break-all px-3 py-2 text-cx-fg-2">{formatDiffVal(item.before)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        ) : null}
        {(feedbackDiff?.feedback?.length ?? 0) > 0 ? (
          <SurfaceCard className="p-3">
            <SectionHeader title="跨版本平台反馈响应" count={feedbackDiff?.feedback?.length} />
            <div className="space-y-1.5 text-cx-xs">
              {(feedbackDiff?.feedback ?? []).map((fb, idx) => (
                <div key={fb.feedback_id || idx} className="rounded-lg border border-cx-border-subtle bg-cx-bg px-2.5 py-2">
                  <span className="font-medium text-cx-fg">{fb.platform_reference || fb.feedback_id}</span>
                  <span className="ml-2 text-cx-fg-3">{fb.kind}</span>
                  {fb.text ? <p className="mt-1 text-cx-fg-2">{fb.text}</p> : null}
                </div>
              ))}
            </div>
          </SurfaceCard>
        ) : null}
        <div className="overflow-x-auto rounded-xl border border-cx-border-subtle">
          <table className="w-full border-collapse text-left text-cx-sm">
            <thead><tr className="border-b border-cx-border-subtle bg-cx-bg-subtle"><th className="w-[120px] px-3 py-2 text-cx-fg-3">项</th><th className="px-3 py-2">{a.id}</th><th className="px-3 py-2 text-cx-fg-2">{b.id}</th></tr></thead>
            <tbody className="divide-y divide-cx-border-subtle">
              {rows.map(([k, va, vb]) => (
                <tr key={k} className={va !== vb ? "bg-cx-accent-soft/30" : ""}><td className="px-3 py-2 text-cx-fg-3">{k}</td><td className="whitespace-pre-wrap px-3 py-2">{va || "—"}</td><td className="whitespace-pre-wrap px-3 py-2 text-cx-fg-2">{vb || "—"}</td></tr>
              ))}
            </tbody>
          </table>
        </div>
      </div>
    </Dialog>
  );
}

function SubmissionAndFeedbackTab({ active }: { active: boolean }) {
  const state = useStudioState();
  const ui = state.workspace.reports;
  const feedback = state.feedbacks.find((f) => f.id === ui.selectedFeedbackId);
  const [recordOpen, setRecordOpen] = useState(false);

  useViewing(active, feedback ? { kind: "feedback", id: feedback.id } : null);

  if (feedback) return <FeedbackDetail key={feedback.id} feedback={feedback} onBack={() => studio.setReportsUi({ selectedFeedbackId: "" })} />;

  return (
    <div className="mx-auto flex w-full max-w-[760px] flex-col gap-4 p-4 pb-10" data-testid="studio-submission-ledger">
      <div>
        <SectionHeader
          title="平台反馈"
          count={state.feedbacks.length}
          action={<Button size="sm" variant="primary" icon="plus" onClick={() => setRecordOpen(true)}>登记平台反馈</Button>}
        />
        <SurfaceCard>
          {state.feedbacks.map((f) => {
            const meta = getFeedbackMeta(f.id);
            return (
              <SurfaceRow
                key={f.id}
                icon="messageCircle"
                title={f.externalTicketId || f.id}
                subtitle={[meta?.backendKind || FEEDBACK_KIND[f.kind], f.reportId || f.findingId, f.receivedAt, f.status === "resolved" ? "已解决" : f.status === "in_progress" ? "处理中" : "待处理"].filter(Boolean).join(" · ")}
                trailing={f.status !== "resolved" ? <StatusDot tone="warning" /> : undefined}
                onClick={() => studio.setReportsUi({ selectedFeedbackId: f.id })}
              />
            );
          })}
          {!state.feedbacks.length ? <SurfaceRow icon="checkCheck" title="暂无平台反馈" /> : null}
        </SurfaceCard>
      </div>
      <RecordFeedbackDialog open={recordOpen} onOpenChange={setRecordOpen} />
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

const BACKEND_FEEDBACK_KIND_LABEL: Record<string, string> = {
  supplement_request: "补充材料请求",
  duplicate_dispute: "重复争议",
  retest_notice: "复测通知",
  triage_question: "审核提问",
  general: "一般反馈",
};

function FeedbackDetail({ feedback, onBack }: { feedback: StudioPlatformFeedback; onBack: () => void }) {
  const state = useStudioState();
  const report = state.reports.find((r) => r.id === feedback.reportId);
  const finding = state.clues.find((c) => c.id === feedback.findingId);
  const task = feedback.followUpTaskId ? state.tasks.find((t) => t.id === feedback.followUpTaskId) : undefined;
  const ref: StudioRef = { kind: "feedback", id: feedback.id };
  const reportTitle = report ? displayTitle(report.title) : (feedback.reportId || feedback.findingId || feedback.id);
  const meta = getFeedbackMeta(feedback.id);
  const kindDisplay = (meta?.backendKind && BACKEND_FEEDBACK_KIND_LABEL[meta.backendKind]) || FEEDBACK_KIND[feedback.kind];

  const createTask = async () => {
    const localId = studio.createTaskFromFeedback(feedback.id);
    if (hasAttachedDomains()) {
      const sync = await afterDomainMutation();
      if (!sync.ok) {
        toast({ title: "派发补件待办失败", description: sync.error, tone: "danger" });
        return;
      }
      await studio.reloadDomain("tasks");
      const createdId = sync.outcome?.createdId || localId;
      toast({
        title: "已派发补件待办",
        tone: "success",
        action: createdId ? { label: "查看", onClick: () => openRef({ kind: "task", id: createdId }) } : undefined,
      });
      return;
    }
    if (localId) toast({ title: "已派发补件待办", tone: "success", action: { label: "查看", onClick: () => openRef({ kind: "task", id: localId }) } });
  };

  const bringToChat = () => {
    const threadId = readStudioState().activeThreadId;
    studio.addContext(threadId, ref);
    if (feedback.reportId) studio.addContext(threadId, { kind: "report", id: feedback.reportId });
    if (feedback.findingId) studio.addContext(threadId, { kind: "clue", id: feedback.findingId });
    const prompt = [
      `请分析平台反馈「${feedback.externalTicketId || feedback.id}」（${kindDisplay}）：`,
      feedback.rawText ? `正文：\n${feedback.rawText}` : "",
      meta?.notes ? `内部备注：${meta.notes}` : "",
      meta?.links.length ? `相关链接：${meta.links.join(", ")}` : "",
      "请对照关联报告与证据，判断需要的补充材料、反证说明或复测步骤，并给出可执行回复建议。",
    ].filter(Boolean).join("\n\n");
    if (hasAttachedDomains()) {
      const accepted = askAi(ref, prompt);
      if (accepted) toast({ title: "已带入主对话分析", tone: "success" });
    } else {
      sendAbout(ref, prompt);
    }
  };

  return (
    <div className="flex min-h-0 flex-1 flex-col" data-testid="studio-feedback-detail">
      <SurfaceToolbar>
        <IconButton size="sm" icon="arrowLeft" label="返回" onClick={onBack} />
        <PathBreadcrumb path={`${reportTitle}/${feedback.externalTicketId || feedback.id}`} rootLabel="报告" lastIsFile onSelect={(dir) => { if (dir === "" || dir === reportTitle) { onBack(); } }} />
        <Menu ariaLabel="反馈操作" trigger={<IconButton size="sm" icon="more" label="更多" />}>
          {feedback.status !== "resolved" ? <MenuItem icon="check" onSelect={() => { void createTask(); }}>派发补件待办</MenuItem> : null}
          <MenuItem icon="messageCircle" onSelect={bringToChat}>带入主对话分析</MenuItem>
          <MenuItem icon="pin" onSelect={() => quoteToComposer(feedback.rawText || feedback.externalTicketId, ref)}>固定到对话</MenuItem>
        </Menu>
      </SurfaceToolbar>
      <div className="cx-scroll min-h-0 flex-1 overflow-y-auto">
        <div className="mx-auto flex w-full max-w-[760px] flex-col gap-4 p-4 pb-10">
          <SurfaceCard className="p-3">
            <div className="flex items-start gap-3">
              <div className="min-w-0 flex-1">
                <p className="truncate text-cx-md font-semibold text-cx-fg">{feedback.externalTicketId || feedback.id}</p>
                <p className="mt-0.5 truncate text-cx-xs text-cx-fg-4">{kindDisplay}</p>
              </div>
              <StudioBadge tone={feedback.status === "resolved" ? "success" : "warning"}>{feedback.status === "resolved" ? "已解决" : feedback.status === "in_progress" ? "处理中" : "待处理"}</StudioBadge>
            </div>
            <Dl className="mt-3" items={[
              { label: "平台", value: report?.submission.platformName || "平台" },
              { label: "反馈人", value: feedback.platformAuthor },
              { label: "接收时间", value: feedback.receivedAt },
              ...(feedback.reportId ? [{ label: "关联报告", value: report ? <RefLink target={{ kind: "report", id: report.id }} /> : feedback.reportId }] : []),
              ...(feedback.findingId ? [{ label: "关联发现", value: finding ? <RefLink target={{ kind: "clue", id: finding.id }} /> : feedback.findingId }] : []),
            ]} />
            <div className="mt-3 flex flex-wrap items-center gap-2">
              <Button size="sm" variant="primary" onClick={() => { void createTask(); }} disabled={Boolean(task)}>
                {task ? `已建任务 ${task.id}` : "派发补件待办"}
              </Button>
              <Button size="sm" variant="secondary" onClick={bringToChat}>
                带入主对话分析
              </Button>
              {task ? (
                <Button size="sm" variant="ghost" onClick={() => openRef({ kind: "task", id: task.id })}>查看待办</Button>
              ) : null}
            </div>
          </SurfaceCard>

          <div>
            <SectionHeader title="原文" action={<button type="button" className="text-cx-xs font-medium text-cx-accent hover:underline" onClick={bringToChat}>让 Agent 起草回复</button>} />
            <blockquote className="whitespace-pre-wrap rounded-xl bg-cx-bg-subtle px-4 py-3 text-cx-sm leading-6 text-cx-fg-2">{feedback.rawText || "（待补全原文）"}</blockquote>
          </div>

          {meta?.notes ? (
            <div>
              <SectionHeader title="内部备注" />
              <SurfaceCard className="p-3 text-cx-sm text-cx-fg-2 whitespace-pre-wrap">{meta.notes}</SurfaceCard>
            </div>
          ) : null}

          {meta?.links.length ? (
            <div>
              <SectionHeader title="相关链接" count={meta.links.length} />
              <SurfaceCard>
                {meta.links.map((link, idx) => (
                  <SurfaceRow key={idx} icon="link" title={link} />
                ))}
              </SurfaceCard>
            </div>
          ) : null}

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
  const [findingId, setFindingId] = useState("");
  const [targetFixVersion, setTargetFixVersion] = useState("");
  const [basis, setBasis] = useState("");
  const [completeId, setCompleteId] = useState("");
  const [verdict, setVerdict] = useState<RetestVerdict>("inconclusive");
  const [preconditionsValid, setPreconditionsValid] = useState(false);
  const [targetVersionConfirmed, setTargetVersionConfirmed] = useState(false);
  const [normalControlWorks, setNormalControlWorks] = useState(false);
  const [evidenceJson, setEvidenceJson] = useState("[]");
  const [selectedEvidencePick, setSelectedEvidencePick] = useState("");
  const [receiptIds, setReceiptIds] = useState("");

  const frozenFindings = useMemo(
    () => state.clues.filter((c) => state.reports.some((r) => r.findingId === c.id && r.status === "frozen")),
    [state.clues, state.reports],
  );
  const effectiveFindingId = findingId || frozenFindings[0]?.id || "";

  return (
    <div className="mx-auto flex max-w-[800px] flex-col gap-5 px-4 pb-10 pt-3" data-testid="studio-retest-tab">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <h3 className="text-cx-md font-semibold text-cx-fg">后续复核记录</h3>
        <Button size="sm" variant="primary" icon="plus" onClick={() => setNewOpen(true)}>
          提交复测请求…
        </Button>
      </div>

      <div className="flex flex-col gap-3">
        {state.retests.map((rt: StudioRetestRecord) => (
          <section key={rt.id} className="flex flex-col gap-3 rounded-2xl border border-cx-border-subtle bg-cx-elevated p-4" data-testid="studio-retest-card">
            <div className="flex flex-wrap items-center justify-between gap-2">
              <div className="flex flex-wrap items-center gap-1.5">
                <StudioBadge
                  tone={rt.status && rt.status !== "completed" ? "warning" : rt.result === "fixed" ? "success" : rt.result === "still_vulnerable" ? "danger" : "warning"}
                >
                  {rt.status && rt.status !== "completed" ? `待复核 · ${rt.status}` : retestVerdictLabel(rt.result)}
                </StudioBadge>
                <span className="text-cx-md font-semibold text-cx-fg">
                  {rt.id} · 发现 {rt.findingId} · 适用版本 {rt.targetFixVersion}
                </span>
              </div>
              <span className="text-cx-xs text-cx-fg-3">{rt.status && rt.status !== "completed" ? `请求时间：${rt.reviewedAt}` : `复核人：${rt.reviewer} · ${rt.reviewedAt}`}</span>
            </div>

            {rt.status && rt.status !== "completed" ? null : <div className="grid grid-cols-2 gap-3 rounded-xl bg-cx-bg-subtle p-3 text-cx-sm leading-5 max-sm:grid-cols-1">
              <div>
                <div className="font-medium text-cx-fg">1. 前置条件与身份核验</div>
                <p className="mt-1 text-cx-fg-2">{rt.prerequisiteCheck}</p>
              </div>
              <div>
                <div className="font-medium text-cx-fg">2. 正常业务对照检查</div>
                <p className="mt-1 text-cx-fg-2">{rt.normalControlCheck}</p>
              </div>
            </div>}

            <div className="text-cx-sm leading-6 text-cx-fg">
              <strong>{rt.status && rt.status !== "completed" ? "修复说明与复测目标：" : "3. 复核依据与实测观察："}</strong>{rt.basisAndEvidence}
            </div>

            <div className="flex flex-wrap items-center gap-1.5">
              <span className="text-cx-xs text-cx-fg-3">关联报告、反馈与证据：</span>
              <RefLink target={{ kind: "clue", id: rt.findingId }} />
              <RefLink target={{ kind: "report", id: rt.baseReportId }} />
              {rt.newReportId ? <RefLink target={{ kind: "report", id: rt.newReportId }} /> : null}
              {rt.feedbackId ? <RefLink target={{ kind: "feedback", id: rt.feedbackId }} /> : null}
              <RefLinkList targets={rt.evidenceIds.map((id): StudioRef => ({ kind: "evidence", id }))} max={4} />
            </div>
            {rt.status && rt.status !== "completed" ? <Button size="sm" variant="secondary" onClick={() => setCompleteId(rt.id)}>凭回执登记结论…</Button> : null}
          </section>
        ))}
      </div>

      <Dialog
        open={newOpen}
        onOpenChange={setNewOpen}
        title="提交复测请求"
        icon="refresh"
        size="lg"
        footer={(
          <>
            <Button variant="ghost" onClick={() => setNewOpen(false)}>取消</Button>
            <Button
              variant="primary"
              disabled={!effectiveFindingId.trim() || !targetFixVersion.trim() || !basis.trim() || !state.reports.some((r) => r.findingId === effectiveFindingId && r.status === "frozen")}
              onClick={() => {
                void (async () => {
                  const rep = state.reports.find((r) => r.findingId === effectiveFindingId && r.status === "frozen");
                  if (!rep) return;
                  const id = studio.recordRetest({
                    findingId: effectiveFindingId,
                    baseReportId: rep.id,
                    targetFixVersion,
                    result: "inconclusive",
                    prerequisiteCheck: "",
                    normalControlCheck: "",
                    basisAndEvidence: basis,
                    evidenceIds: [],
                  });
                  const sync = await afterDomainMutation();
                  if (!sync.ok) {
                    toast({ title: "提交复测失败", description: sync.error, tone: "danger" });
                    return;
                  }
                  setNewOpen(false);
                  toast({ title: `已提交复测请求 ${sync.outcome?.createdId || id}`, tone: "success" });
                })();
              }}
            >
              提交复测请求
            </Button>
          </>
        )}
      >
        <div className="flex flex-col gap-3">
          <div className="grid grid-cols-2 gap-3 max-sm:grid-cols-1">
            {frozenFindings.length ? (
              <div className="flex flex-col">
                <Label>关联发现</Label>
                <Select<string>
                  value={effectiveFindingId}
                  onChange={setFindingId}
                  ariaLabel="关联发现"
                  options={frozenFindings.map((c) => ({ value: c.id, label: `${c.id} · ${displayTitle(c.title)}` }))}
                />
              </div>
            ) : (
              <TextField label="关联发现编号" value={findingId} onChange={(e) => setFindingId(e.target.value)} />
            )}
            <TextField label="验证目标修复版本" value={targetFixVersion} onChange={(e) => setTargetFixVersion(e.target.value)} />
          </div>
          <TextArea label="修复说明与复测目标" value={basis} onChange={(e) => setBasis(e.target.value)} className="min-h-[84px]" />
        </div>
      </Dialog>
      <Dialog
        open={Boolean(completeId)}
        onOpenChange={(open) => { if (!open) setCompleteId(""); }}
        title="登记复测结论"
        icon="refresh"
        size="lg"
        footer={<>
          <Button variant="ghost" onClick={() => setCompleteId("")}>取消</Button>
          <Button variant="primary" onClick={() => {
            void (async () => {
              let evidenceRefs: Record<string, unknown>[];
              try {
                const parsed: unknown = JSON.parse(evidenceJson);
                if (!Array.isArray(parsed) || !parsed.every((item) => item && typeof item === "object" && !Array.isArray(item))) throw new Error("证据引用须为 JSON 对象数组");
                evidenceRefs = parsed as Record<string, unknown>[];
              } catch (error) {
                toast({ title: "证据引用格式错误", description: error instanceof Error ? error.message : String(error), tone: "danger" });
                return;
              }
              const executionReceiptIds = receiptIds.split(/\r?\n/).map((id) => id.trim()).filter(Boolean);
              if (verdict !== "inconclusive" && (!preconditionsValid || !targetVersionConfirmed || !normalControlWorks || !evidenceRefs.length || !executionReceiptIds.length)) {
                toast({ title: "结论缺少核验条件", description: "请确认前提、目标版本和正常对照，并填写本次执行回执与现场证据。", tone: "danger" });
                return;
              }
              studio.completeRetest({
                retestId: completeId, preconditionsValid, targetVersionConfirmed,
                normalControlWorks: normalControlWorks ? true : null,
                originalPathImpact: verdict === "inconclusive" ? null : verdict === "still_vulnerable",
                evidenceRefs, executionReceiptIds, outcomeUnknown: verdict === "inconclusive",
              });
              const sync = await afterDomainMutation();
              if (!sync.ok) {
                toast({ title: "提交复测结论失败", description: sync.error, tone: "danger" });
                return;
              }
              setCompleteId("");
              toast({ title: "复测结论已提交核验", tone: "success" });
            })();
          }}>提交结论</Button>
        </>}
      >
        <div className="flex flex-col gap-3">
          <SegmentedControl<RetestVerdict> value={verdict} onChange={setVerdict} size="md" ariaLabel="三态复核结论" options={RETEST_VERDICTS.map((v) => ({ value: v.value, label: v.label }))} />
          <Switch checked={preconditionsValid} onCheckedChange={setPreconditionsValid} label="原复现前提与身份已核验" />
          <Switch checked={targetVersionConfirmed} onCheckedChange={setTargetVersionConfirmed} label="目标修复版本已核验" />
          <Switch checked={normalControlWorks} onCheckedChange={setNormalControlWorks} label="正常业务对照正常" />
          {state.evidences.length ? (
            <div className="flex items-end gap-2">
              <div className="flex flex-1 flex-col">
                <Label>附加证据</Label>
                <Select<string>
                  value={selectedEvidencePick}
                  onChange={(evId) => {
                    setSelectedEvidencePick(evId);
                    const ev = state.evidences.find((e) => e.id === evId);
                    if (!ev) return;
                    try {
                      const arr = JSON.parse(evidenceJson);
                      const next = Array.isArray(arr) ? arr : [];
                      const item = { evidence_id: ev.id, artifact_id: ev.artifactId || ev.id, kind: ev.kind };
                      setEvidenceJson(JSON.stringify([...next, item], null, 2));
                    } catch {
                      setEvidenceJson(JSON.stringify([{ evidence_id: ev.id, artifact_id: ev.artifactId || ev.id, kind: ev.kind }], null, 2));
                    }
                  }}
                  placeholder="选择已有证据追加…"
                  ariaLabel="附加证据"
                  options={state.evidences.map((e) => ({ value: e.id, label: `${e.id} · ${displayTitle(e.title)}` }))}
                />
              </div>
            </div>
          ) : null}
          <TextArea label="本次执行回执 ID（每行一个）" value={receiptIds} onChange={(event) => setReceiptIds(event.target.value)} className="min-h-[72px]" />
          <TextArea label="现场工件完整引用（JSON 数组）" value={evidenceJson} onChange={(event) => setEvidenceJson(event.target.value)} className="min-h-[110px] font-cx-mono text-cx-xs" />
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
  const [draftOpen, setDraftOpen] = useState(false);
  const [draftFindingId, setDraftFindingId] = useState("");
  const [draftTitle, setDraftTitle] = useState("");
  const [draftTemplate, setDraftTemplate] = useState<ReportTemplateId>("general_src");
  const report = state.reports.find((r) => r.id === ui.selectedReportId);

  const confirmedByCurrency = new Map<string, number>();
  const paidByCurrency = new Map<string, number>();
  for (const r of state.reports) {
    const curr = (r.submission.rewardCurrency || "CNY").trim().toUpperCase();
    const amt = r.submission.rewardAmount ?? 0;
    if (!amt) continue;
    if (r.submission.rewardStatus === "confirmed" || r.submission.rewardStatus === "paid") {
      confirmedByCurrency.set(curr, (confirmedByCurrency.get(curr) ?? 0) + amt);
    }
    if (r.submission.rewardStatus === "paid") {
      paidByCurrency.set(curr, (paidByCurrency.get(curr) ?? 0) + amt);
    }
  }
  const formatTotals = (map: Map<string, number>) =>
    map.size ? [...map.entries()].map(([curr, amt]) => formatMoney(amt, curr)).join(" + ") : "¥0 CNY";
  const feedbackAlert = new Set(state.feedbacks.filter((f) => f.status !== "resolved").map((f) => f.reportId));

  const exportLedger = () => {
    const content = JSON.stringify({
      exportedAt: new Date().toISOString(),
      reports: state.reports,
      feedbacks: state.feedbacks,
      retests: state.retests,
    }, null, 2);
    const url = URL.createObjectURL(new Blob([content], { type: "application/json;charset=utf-8" }));
    const link = document.createElement("a");
    link.href = url;
    link.download = "src-report-ledger.json";
    document.body.appendChild(link);
    link.click();
    link.remove();
    window.setTimeout(() => URL.revokeObjectURL(url), 0);
    toast({ title: "已导出报告台账", tone: "success" });
  };

  useViewing(active && view === "list", null);

  if (view === "report" && (report || tab !== "reports")) {
    const crumb = tab === "feedbacks" ? "提交与反馈" : tab === "retests" ? "后续复核" : report?.id ?? "";
    return (
      <div className="flex h-full min-h-0 flex-col" data-testid="studio-reports-workspace">
        <div className="flex min-h-0 flex-1 flex-col">
          <SurfaceToolbar>
            <IconButton size="sm" icon="arrowLeft" label="返回报告列表" onClick={() => studio.setReportsUi({ view: "list", tab: "reports" })} />
            <PathBreadcrumb path={crumb} rootLabel="报告" lastIsFile onSelect={() => studio.setReportsUi({ view: "list", tab: "reports" })} />
            <Menu ariaLabel="报告视图" trigger={<IconButton size="sm" icon="more" label="更多" />}>
              <MenuItem icon="messageCircle" onSelect={() => studio.setReportsUi({ tab: "feedbacks" })}>提交与反馈台账</MenuItem>
              <MenuItem icon="refresh" onSelect={() => studio.setReportsUi({ tab: "retests" })}>后续复核记录</MenuItem>
              <MenuSeparator />
              <MenuItem icon="file" disabled={!report} onSelect={() => studio.setReportsUi({ tab: "reports" })}>报告详情</MenuItem>
            </Menu>
          </SurfaceToolbar>
          {state.domainErrors?.reports ? (
            <Callout tone="danger" className="mx-4 mt-2" action={<Button size="xs" variant="secondary" onClick={() => { void studio.reloadDomain("reports"); }}>重试加载</Button>}>
              报告与反馈域加载异常：{state.domainErrors.reports}
            </Callout>
          ) : null}
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
        <IconButton size="sm" icon="plus" label="新建报告草稿" onClick={() => {
          setDraftFindingId(state.clues[0]?.id ?? "");
          setDraftTitle("");
          setDraftOpen(true);
        }} />
        <Menu ariaLabel="更多" trigger={<IconButton size="sm" icon="more" label="更多" />}>
          <MenuItem icon="messageCircle" onSelect={() => studio.setReportsUi({ tab: "feedbacks", view: "report" })}>提交与反馈台账</MenuItem>
          <MenuItem icon="refresh" onSelect={() => studio.setReportsUi({ tab: "retests", view: "report" })}>后续复核记录</MenuItem>
          <MenuSeparator />
          <MenuItem icon="download" onSelect={exportLedger}>导出台账</MenuItem>
        </Menu>
      </SurfaceToolbar>
      {state.domainErrors?.reports ? (
        <Callout tone="danger" className="mx-4 mt-2" action={<Button size="xs" variant="secondary" onClick={() => { void studio.reloadDomain("reports"); }}>重试加载</Button>}>
          报告与反馈域加载异常：{state.domainErrors.reports}
        </Callout>
      ) : null}
      <p className="px-4 pb-0 pt-2 text-cx-xs text-cx-fg-4">已确认 {formatTotals(confirmedByCurrency)} · 已到账 {formatTotals(paidByCurrency)}</p>
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
                      subtitle={r.status === "draft" ? `v${r.version} · ${r.readiness} 待复核${r.materialGaps?.length ? ` · 缺 ${r.materialGaps.length} 项` : ""}` : `v${r.version} · ${r.readiness} · ${r.submission.externalTicketId || "未登记编号"} · ${submissionStatusLabel(r.submission.status)}${r.submission.rewardAmount != null ? ` ${formatMoney(r.submission.rewardAmount, r.submission.rewardCurrency)}` : ""}`}
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
      <Dialog
        open={draftOpen}
        onOpenChange={setDraftOpen}
        title="新建报告草稿"
        size="md"
        footer={(
          <>
            <Button variant="ghost" onClick={() => setDraftOpen(false)}>取消</Button>
            <Button
              variant="primary"
              disabled={!draftFindingId && state.clues.length === 0}
              onClick={() => {
                void (async () => {
                  const targetFinding = draftFindingId || state.clues[0]?.id || "";
                  if (!targetFinding) return;
                  const localId = studio.createReportDraft(targetFinding, {
                    title: draftTitle.trim() || undefined,
                    templateId: draftTemplate,
                  });
                  const sync = await afterDomainMutation();
                  if (!sync.ok) {
                    toast({ title: "创建报告草稿失败", description: sync.error, tone: "danger" });
                    return;
                  }
                  const createdId = readStudioState().workspace.reports.selectedReportId || sync.outcome?.createdId || localId;
                  setDraftOpen(false);
                  openRef({ kind: "report", id: createdId });
                  toast({ title: `已创建报告草稿 ${createdId}`, tone: "success" });
                })();
              }}
            >
              创建
            </Button>
          </>
        )}
      >
        <div className="flex flex-col gap-3">
          {state.clues.length ? (
            <div className="flex flex-col">
              <Label>关联发现</Label>
              <Select<string>
                value={draftFindingId || state.clues[0]?.id || ""}
                onChange={setDraftFindingId}
                ariaLabel="关联发现"
                options={state.clues.map((c) => ({ value: c.id, label: `${c.id} · ${displayTitle(c.title)}` }))}
              />
            </div>
          ) : null}
          <TextField label="报告标题" value={draftTitle} onChange={(e) => setDraftTitle(e.target.value)} placeholder="留空则沿用发现标题" />
          <div className="flex flex-col">
            <Label>报告模板</Label>
            <Select<ReportTemplateId>
              value={draftTemplate}
              onChange={setDraftTemplate}
              ariaLabel="报告模板"
              options={REPORT_TEMPLATES.map((t) => ({ value: t.id, label: t.name }))}
            />
          </div>
        </div>
      </Dialog>
      <SheetAskBar sheet={sheet} target={null} />
    </div>
  );
}
