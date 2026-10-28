"use client";

import { useCallback, useEffect, useMemo, useState } from "react";
import { Icon } from "@/components/Icon";
import {
  Badge, Empty, ErrorState, EvidenceList, KeyValues, Loading, Notice,
  RawDetails, ReviewShell, ReviewTabs, Section, downloadReport, errorMessage,
  formatDate, lines, newIdempotencyKey, postCommand, querySrc,
  type EvidenceReference, type OpenEvidence,
} from "./reviewPrimitives";
import styles from "./review.module.css";
import { byteText } from "./RequestWorkspaceTypes";

type Readiness = "GREEN" | "YELLOW" | "RED";
type Template = { template_id: string; platform?: string; template_version?: string; mapping_version?: string; source_url?: string; source_fetched_at?: string; field_constraints?: Record<string, unknown> };
type ReportVersion = { report_id: string; report_seq: number; created_at?: string; readiness?: Readiness; template_id?: string; supersedes?: string };
type Poc = { language?: string; entrypoint?: string; dependencies?: string[]; initialization?: string[]; steps?: string[]; success_assertion?: string; cleanup?: string[]; verified?: boolean; execution_evidence_refs?: EvidenceReference[] };
type ReportStructure = {
  title?: string; target_version?: string; program_scope?: string; attacker_role?: string; preconditions?: string[]; function_location?: string;
  mechanism?: string; normal_behavior?: string; actual_behavior?: string; reproduction_steps?: string[]; decisive_evidence_refs?: EvidenceReference[];
  confirmed_impact?: string[]; conditional_impact?: string[]; remediation?: string[]; counter_evidence?: string[]; limitations?: string[];
  restoration?: { status?: string; residual_objects?: string[]; reason?: string };
};
type PlatformStatus = { status?: string; platform_ref?: string; platform_reference?: string; bounty_status?: string; amount?: number | string; currency?: string; notes?: string; revision?: number; recorded_by?: string; recorded_at?: string; user_recorded_unverified?: boolean };
type Feedback = { feedback_id: string; source?: string; text: string; created_at?: string; attachments?: EvidenceReference[]; addressed?: string[]; unresolved?: string[]; diff?: ReportDiff[] };
type ReportDiff = { field: string; before?: unknown; after?: unknown; feedback_id?: string };
type Retest = { retest_id: string; report_id?: string; result?: "fixed" | "still_vulnerable" | "inconclusive"; repair_statement?: string; expected_target_version?: string; created_at?: string; finished_at?: string; evidence_refs?: EvidenceReference[]; gaps?: string[]; restoration?: { status?: string; residual_objects?: string[]; reason?: string }; idempotency_key?: string };

export type ReportView = {
  report_id: string; finding_id: string; report_seq: number; revision?: number; frozen?: boolean; stale_reason?: string;
  source_run_ids?: string[]; source_watermarks?: Record<string, unknown>; readiness?: Readiness;
  structure: ReportStructure; report_markdown?: string; poc?: Poc; template?: Template; available_templates?: Template[];
  versions?: ReportVersion[]; platform_status?: PlatformStatus; platform_history?: PlatformStatus[];
  feedback?: Feedback[]; retests?: Retest[]; diffs?: ReportDiff[];
  manifest?: { expected_entries?: number; actual_entries?: number; omissions?: string[]; failures?: string[]; partial?: boolean; attachments?: Array<Record<string, unknown>> };
};

export type ReportInspectorProps = {
  taskId: string;
  reportId: string;
  onOpenEvidence?: OpenEvidence;
  onOpenFinding?: (findingId: string) => void;
  onSelectReport?: (reportId: string) => void;
  onClose?: () => void;
  className?: string;
};

const readinessTone = (value?: Readiness) => value === "GREEN" ? "positive" : value === "YELLOW" ? "warning" : "danger";
const retestLabels = { fixed: "已验证修复（仅限本次路径与版本）", still_vulnerable: "仍然可利用", inconclusive: "无法确定" };
const platformStatuses = ["not_submitted", "submitted", "pending_info", "accepted", "duplicate", "rejected", "reward_confirmed", "reward_paid"];

function textList(value: unknown): string[] {
  if (value === undefined || value === null || value === "") return [];
  return (Array.isArray(value) ? value : [value]).map(item => typeof item === "string" ? item : JSON.stringify(item));
}

function reportFromPayload(payload: unknown): ReportView {
  if (payload && typeof payload === "object" && "report" in payload) {
    return reportFromPayload((payload as { report: unknown }).report);
  }
  const raw = payload as ReportView & {
    source_run_id?: string; frozen_at?: string; rendered_markdown?: string;
    review?: { readiness?: Readiness | { status?: Readiness } };
    document?: {
      title?: string; target_version?: string; scope?: string; attacker?: string; function?: string;
      preconditions?: string[]; mechanism?: string; normal_behavior?: string; actual_behavior?: string;
      steps?: Array<string | { action?: string }>; decisive_evidence?: EvidenceReference[];
      confirmed_impact?: unknown; conditional_impact?: unknown; remediation?: unknown;
      counterevidence?: unknown; limitations?: unknown; restoration?: ReportStructure["restoration"]; poc?: Poc;
    };
  };
  const document = raw.document;
  if (!document) return raw;
  const reviewReadiness = raw.review?.readiness;
  const readiness = typeof reviewReadiness === "object" ? reviewReadiness.status : reviewReadiness;
  return {
    ...raw,
    frozen: raw.frozen ?? Boolean(raw.frozen_at),
    source_run_ids: raw.source_run_ids || (raw.source_run_id ? [raw.source_run_id] : []),
    readiness: readiness || raw.readiness,
    report_markdown: raw.report_markdown || raw.rendered_markdown,
    poc: raw.poc || document.poc,
    structure: {
      title: document.title,
      target_version: document.target_version,
      program_scope: document.scope,
      attacker_role: document.attacker,
      preconditions: document.preconditions,
      function_location: document.function,
      mechanism: document.mechanism,
      normal_behavior: document.normal_behavior,
      actual_behavior: document.actual_behavior,
      reproduction_steps: (document.steps || []).map(step => typeof step === "string" ? step : step.action || JSON.stringify(step)),
      decisive_evidence_refs: document.decisive_evidence,
      confirmed_impact: textList(document.confirmed_impact),
      conditional_impact: textList(document.conditional_impact),
      remediation: textList(document.remediation),
      counter_evidence: textList(document.counterevidence),
      limitations: textList(document.limitations),
      restoration: document.restoration,
    },
  };
}

function DiffList({ diffs }: { diffs?: ReportDiff[] }) {
  if (!diffs?.length) return <Empty>尚无结构化版本差异。</Empty>;
  return <div className={styles.stack}>{diffs.map((diff, index) => <article className={styles.card} key={`${diff.field}-${index}`}>
    <h3>{diff.field}</h3><div className={styles.diffGrid}>
      <div className={styles.diffBefore}><h4>旧版本</h4><pre className={styles.codeBlock}>{JSON.stringify(diff.before ?? "—", null, 2)}</pre></div>
      <div className={styles.diffAfter}><h4>新版本</h4><pre className={styles.codeBlock}>{JSON.stringify(diff.after ?? "—", null, 2)}</pre></div>
    </div>
  </article>)}</div>;
}

export function ReportInspector({ taskId, reportId, onOpenEvidence, onOpenFinding, onSelectReport, onClose, className }: ReportInspectorProps) {
  const [report, setReport] = useState<ReportView | null>(null);
  const [rawReport, setRawReport] = useState<Record<string, any> | null>(null);
  const [selectedSeq, setSelectedSeq] = useState<number | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");
  const [message, setMessage] = useState("");
  const [tab, setTab] = useState("report");
  const [templateId, setTemplateId] = useState("");
  const [format, setFormat] = useState("zip");
  const [redact, setRedact] = useState(false);
  const [redactionValues, setRedactionValues] = useState("");
  const [partial, setPartial] = useState(false);
  const [busy, setBusy] = useState("");
  const [status, setStatus] = useState("submitted");
  const [platformRef, setPlatformRef] = useState("");
  const [amount, setAmount] = useState("");
  const [statusNotes, setStatusNotes] = useState("");
  const [feedbackSource, setFeedbackSource] = useState("manual");
  const [feedbackText, setFeedbackText] = useState("");
  const [repairStatement, setRepairStatement] = useState("");
  const [expectedVersion, setExpectedVersion] = useState("");
  const [identityRefs, setIdentityRefs] = useState("");
  const [authorizationRef, setAuthorizationRef] = useState("");
  const [retestKey, setRetestKey] = useState(() => newIdempotencyKey("src-retest"));

  const load = useCallback(async () => {
    setLoading(true); setError("");
    try {
      const raw = await querySrc<Record<string, any>>(taskId, "src.report.get", { report_id: reportId });
      setRawReport(raw);
      const findingId = String(raw.finding_id || "");
      const [versionRows, platformStatus, platformHistory, retestRows, templates, diff] = await Promise.all([
        querySrc<ReportVersion[]>(taskId, "src.report.list", { finding_id: findingId }),
        querySrc<PlatformStatus>(taskId, "src.feedback.platform_status", { report_id: reportId }),
        querySrc<PlatformStatus[]>(taskId, "src.feedback.history", { report_id: reportId }),
        querySrc<Retest[]>(taskId, "src.retest.recover"),
        querySrc<Template[]>(taskId, "src.report.templates"),
        querySrc<{ changes?: ReportDiff[]; feedback?: Array<Record<string, any>> }>(taskId, "src.feedback.diff", {
          old_report_id: raw.supersedes || reportId, new_report_id: reportId,
        }),
      ]);
      const feedback = await Promise.all((diff.feedback || []).map(async row => {
        let text = typeof row.text === "string" ? row.text : "";
        if (!text && row.text_ref) text = byteText(await querySrc(taskId, "src.artifact.read", { ref_data: row.text_ref }));
        return {
          ...row, text,
          created_at: row.created_at || row.received_at,
        } as Feedback;
      }));
      setReport(reportFromPayload({
        ...raw,
        versions: versionRows.map(row => ({
          ...row, created_at: row.created_at || (row as unknown as { frozen_at?: string }).frozen_at,
          readiness: (() => {
            const value = (row as unknown as { review?: { readiness?: Readiness | { status?: Readiness } } }).review?.readiness;
            return typeof value === "object" ? value.status : value;
          })(),
          template_id: (row as unknown as { template?: { template_id?: string } }).template?.template_id,
        })),
        available_templates: templates,
        platform_status: platformStatus,
        platform_history: platformHistory,
        retests: retestRows.filter(retest => retest.report_id === reportId),
        diffs: diff.changes || [], feedback,
      }));
      setSelectedSeq(Number(raw.report_seq || 1));
    } catch (cause) { setError(errorMessage(cause)); }
    finally { setLoading(false); }
  }, [taskId, reportId]);
  useEffect(() => { void load(); }, [load]);
  useEffect(() => { setSelectedSeq(null); setRetestKey(newIdempotencyKey("src-retest")); }, [reportId]);
  useEffect(() => { if (report?.template?.template_id) setTemplateId(report.template.template_id); }, [report?.template?.template_id]);

  const runAction = async (name: string, action: () => Promise<void>) => {
    setBusy(name); setError(""); setMessage("");
    try { await action(); }
    catch (cause) { setError(errorMessage(cause)); }
    finally { setBusy(""); }
  };

  const renderTemplate = () => runAction("template", async () => {
    if (!rawReport) throw new Error("冻结报告原文尚未恢复");
    const result = await postCommand<{ report_id?: string }>(taskId, "src.report.freeze", {
      finding_id: rawReport.finding_id,
      source_run_id: rawReport.source_run_id || rawReport.source_run_ids?.[0],
      document: rawReport.document,
      evidence_refs: rawReport.evidence_refs || rawReport.document?.decisive_evidence || [],
      claims: rawReport.claims,
      review: rawReport.review,
      cvss: rawReport.cvss,
      deduplication: rawReport.deduplication,
      template: templateId,
      plan_snapshot: rawReport.plan_snapshot,
      authorization_snapshot: rawReport.authorization_snapshot,
      source_watermarks: rawReport.source_watermarks,
      supersedes: rawReport.report_id,
      feedback_responses: rawReport.feedback_responses,
      idempotency_key: newIdempotencyKey("src-render"),
    });
    setMessage("已按所选模板生成新渲染版本；已冻结旧正文保持不变。");
    if (result.report_id && onSelectReport) onSelectReport(result.report_id);
    await load();
  });
  const exportEvidence = () => runAction("export", async () => {
    if (!report) return;
    if (redact && !lines(redactionValues).length) throw new Error("请逐项填写需要脱敏的原值；不会猜测或替换整个 SecretStore");
    await downloadReport(taskId, "src.submission.export", {
      report_id: report.report_id,
      format: format === "raw_http" ? "raw" : format,
      allow_partial: partial,
      redactions: redact ? lines(redactionValues).map(value => ({ value, replacement: "[已隐去]" })) : [],
    });
    setMessage(redact ? "已导出派生脱敏包；原始证据未被覆盖，部分 PoC 可能因此不能直接运行。" : "已按选定原值导出证据包。请核对 manifest 的完整性和缺项。 ");
  });
  const recordStatus = () => runAction("status", async () => {
    if (!report) return;
    const parsedAmount = amount.trim() ? Number(amount) : undefined;
    if (parsedAmount !== undefined && (!Number.isFinite(parsedAmount) || parsedAmount < 0)) {
      throw new Error("奖金金额必须是非负数字；无法确认时留空。 ");
    }
    await postCommand(taskId, "src.feedback.platform_status.record", {
      report_id: report.report_id, base_revision: report.platform_status?.revision ?? 0,
      actor_kind: "operator",
      status, platform_reference: platformRef, amount: parsedAmount, notes: statusNotes,
    });
    setMessage("人工平台状态已记录，未与平台自动核验，也没有自动提交报告。");
    await load();
  });
  const recordFeedback = () => runAction("feedback", async () => {
    if (!report) return;
    await postCommand(taskId, "src.feedback.record", {
      report_id: report.report_id,
      source: feedbackSource, text: feedbackText,
    });
    setFeedbackText(""); setMessage("平台反馈已作为研究输入保存；后续补证和报告差异仍沿原 Task/Run。 ");
    await load();
  });
  const requestRetest = () => runAction("retest", async () => {
    if (!report) return;
    await postCommand(taskId, "src.retest.request", {
      finding_id: report.finding_id, report_id: report.report_id,
      repair_statement: repairStatement, expected_target_version: expectedVersion,
      authorization_ref: authorizationRef,
      identity_refs: lines(identityRefs), idempotency_key: retestKey,
      budget: {},
    });
    setMessage("复测请求已登记。重复提交同一幂等键会返回同一记录，不会新建 Run 或重复非幂等动作。 ");
    await load();
  });

  const versions = useMemo(() => [...(report?.versions || [])].sort((a, b) => b.report_seq - a.report_seq), [report?.versions]);
  if (loading && !report) return <Loading label="正在恢复冻结报告、证据和版本状态…" />;
  if (error && !report) return <ErrorState message={error} onRetry={() => void load()} />;
  if (!report) return <Empty>报告不存在、已撤回或当前版本不可读。</Empty>;
  const structure = report.structure;

  return <div className={className}><ReviewShell
    title={structure.title || `报告 ${report.report_id}`}
    subtitle={`${report.report_id} · v${report.report_seq}${report.frozen ? " · 已冻结" : " · 草稿"}`}
    actions={<>
      {report.readiness ? <Badge tone={readinessTone(report.readiness)}>{report.readiness}</Badge> : null}
      {onOpenFinding ? <button type="button" className={styles.secondaryButton} onClick={() => onOpenFinding(report.finding_id)}><Icon name="target" size={14} />Finding</button> : null}
      {onClose ? <button type="button" className={styles.secondaryButton} onClick={onClose} aria-label="关闭报告"><Icon name="x" size={15} /></button> : null}
    </>}
  >
    {error ? <Notice tone="danger">{error}</Notice> : null}
    {message ? <Notice tone="success">{message}</Notice> : null}
    {report.stale_reason ? <Notice tone="warning">当前报告已有新证据或来源变化：{report.stale_reason}。冻结旧版仍可读，不会原位更新。</Notice> : null}
    <Notice>报告始终由人自行提交。平台接受、重复、拒绝和奖金只来自用户手工记录。</Notice>
    <div className={styles.toolbar}>
      <label className={styles.field}>报告版本<select value={selectedSeq ?? report.report_seq} onChange={event => {
        const seq = Number(event.target.value);
        setSelectedSeq(seq);
        const selected = versions.find(version => version.report_seq === seq);
        if (selected && onSelectReport) onSelectReport(selected.report_id);
      }}>{versions.length ? versions.map(version => <option key={version.report_id} value={version.report_seq}>v{version.report_seq} · {formatDate(version.created_at)} · {version.readiness || "未审阅"}</option>) : <option value={report.report_seq}>v{report.report_seq}</option>}</select></label>
      <label className={styles.field}>提交模板<select value={templateId} onChange={event => setTemplateId(event.target.value)}>{(report.available_templates?.length ? report.available_templates : report.template ? [report.template] : []).map(template => <option key={template.template_id} value={template.template_id}>{template.platform || template.template_id} · {template.template_version || "v1"}</option>)}</select></label>
      <button type="button" className={styles.secondaryButton} disabled={busy === "template" || !templateId} onClick={renderTemplate}><Icon name={busy === "template" ? "loader" : "refresh"} size={14} className={busy === "template" ? "pentest-spin" : undefined} />生成新渲染版本</button>
    </div>
    {report.template ? <div className={styles.metaLine}>模板 {report.template.template_id} · 映射 {report.template.mapping_version || "—"} · 规则快照 {formatDate(report.template.source_fetched_at)}{report.template.source_url ? <> · <a href={report.template.source_url} target="_blank" rel="noreferrer">来源<Icon name="arrowUpRight" size={11} /></a></> : null}</div> : null}
    <ReviewTabs label="报告工作区" active={tab} onChange={setTab} items={[
      { id: "report", label: "提交正文" },
      { id: "poc", label: "PoC 与证据" },
      { id: "diff", label: "反馈与差异", count: report.feedback?.length || 0 },
      { id: "retest", label: "修复复测", count: report.retests?.length || 0 },
      { id: "status", label: "人工平台状态" },
      { id: "export", label: "导出与完整性" },
    ]} />

    {tab === "report" ? <>
      <Section title="规范报告结构">
        <KeyValues items={[
          ["目标/版本", structure.target_version || "未固定"], ["计划范围", structure.program_scope || "—"],
          ["最低角色", structure.attacker_role || "—"], ["功能位置", structure.function_location || "—"],
          ["前置条件", structure.preconditions?.length ? <ul className={styles.list} key="pre">{structure.preconditions.map(item => <li key={item}>{item}</li>)}</ul> : "—"],
        ]} />
        <div className={styles.cardGrid}>
          <article className={styles.card}><h3>机制</h3><p>{structure.mechanism || "—"}</p></article>
          <article className={styles.card}><h3>正常行为</h3><p>{structure.normal_behavior || "—"}</p></article>
          <article className={styles.card}><h3>实际行为</h3><p>{structure.actual_behavior || "—"}</p></article>
        </div>
      </Section>
      <Section title="逐步复现" count={structure.reproduction_steps?.length || 0}>
        {structure.reproduction_steps?.length ? <ol className="pentest-report-steps">{structure.reproduction_steps.map((step, index) => <li key={index}><p>{step}</p></li>)}</ol> : <Empty>复现步骤缺失；这应阻止 GREEN。</Empty>}
      </Section>
      <Section title="影响、限制与恢复">
        <div className={styles.cardGrid}>
          <article className={styles.card}><h3>已证实业务影响</h3>{structure.confirmed_impact?.length ? <ul className={styles.list}>{structure.confirmed_impact.map(item => <li key={item}>{item}</li>)}</ul> : <Empty>未证实业务影响</Empty>}</article>
          <article className={styles.card}><h3>条件性影响（未直接验证）</h3>{structure.conditional_impact?.length ? <ul className={styles.list}>{structure.conditional_impact.map(item => <li key={item}>{item}</li>)}</ul> : <Empty>无条件性影响</Empty>}</article>
          <article className={styles.card}><h3>反证与限制</h3>{[...(structure.counter_evidence || []), ...(structure.limitations || [])].length ? <ul className={styles.list}>{[...(structure.counter_evidence || []), ...(structure.limitations || [])].map(item => <li key={item}>{item}</li>)}</ul> : <Empty>未登记反证或限制</Empty>}</article>
          <article className={styles.card}><h3>修复建议</h3>{structure.remediation?.length ? <ul className={styles.list}>{structure.remediation.map(item => <li key={item}>{item}</li>)}</ul> : <Empty>未提供修复建议</Empty>}</article>
          <article className={styles.card}><h3>恢复结果</h3><KeyValues items={[["状态", structure.restoration?.status || "unknown"], ["残留", structure.restoration?.residual_objects?.join("、") || "无已登记残留"], ["原因", structure.restoration?.reason || "—"]]} /></article>
        </div>
      </Section>
      <Section title="提交正文（原值预览）">
        {report.report_markdown ? <pre className={styles.codeBlock}>{report.report_markdown}</pre> : <Empty>当前模板没有可预览正文；请检查必填字段与缺证。</Empty>}
      </Section>
    </> : null}

    {tab === "poc" ? <>
      <Section title="可运行 PoC">
        {!report.poc ? <Empty>没有 PoC；未执行的脚本不能标为已验证。</Empty> : <>
          {report.poc.verified ? <Notice tone="success">该 PoC 有实际执行验证记录；仍可展开对应证据核对。</Notice> : <Notice tone="warning">PoC 未经过实际执行验证，不能据此标记 GREEN。</Notice>}
          <KeyValues items={[["语言/入口", `${report.poc.language || "—"} · ${report.poc.entrypoint || "—"}`], ["依赖", report.poc.dependencies?.join("、") || "—"], ["成功断言", report.poc.success_assertion || "未提供"], ["初始化", report.poc.initialization?.join("；") || "—"]]} />
          {report.poc.entrypoint ? <pre className={styles.codeBlock}>{report.poc.entrypoint}</pre> : null}
          {report.poc.steps?.length ? <ol className={styles.list}>{report.poc.steps.map(step => <li key={step}>{step}</li>)}</ol> : null}
          {report.poc.cleanup?.length ? <><h3>清理与恢复</h3><ul className={styles.list}>{report.poc.cleanup.map(item => <li key={item}>{item}</li>)}</ul></> : null}
          <EvidenceList refs={report.poc.execution_evidence_refs} onOpen={onOpenEvidence} empty="没有 PoC 执行证据" />
        </>}
      </Section>
      <Section title="决定性证据与附件">
        <EvidenceList refs={structure.decisive_evidence_refs} onOpen={onOpenEvidence} empty="未选择决定性证据" />
        <EvidenceList refs={report.manifest?.attachments as EvidenceReference[] | undefined} onOpen={onOpenEvidence} empty="manifest 未列出附件" />
      </Section>
    </> : null}

    {tab === "diff" ? <>
      <Section title="记录人工平台反馈">
        <div className={styles.toolbar}>
          <label className={styles.field}>来源<input value={feedbackSource} onChange={event => setFeedbackSource(event.target.value)} /></label>
          <label className={`${styles.field} ${styles.fieldGrow}`}>反馈原文<textarea value={feedbackText} onChange={event => setFeedbackText(event.target.value)} placeholder="粘贴平台反馈；正文只作为研究数据，不会授予执行权限" /></label>
        </div>
        <div className={styles.actions}><button type="button" className={styles.primaryButton} disabled={busy === "feedback" || !feedbackText.trim()} onClick={recordFeedback}>保存反馈并关联 v{report.report_seq}</button></div>
      </Section>
      <Section title="反馈与未决问题" count={report.feedback?.length || 0}>
        {!report.feedback?.length ? <Empty>尚未记录平台反馈。</Empty> : <div className={styles.stack}>{report.feedback.map(feedback => <article className={styles.card} key={feedback.feedback_id}>
          <div className={styles.metaLine}><Badge>{feedback.source || "manual"}</Badge><span>{formatDate(feedback.created_at)}</span></div><p>{feedback.text}</p>
          <EvidenceList refs={feedback.attachments} onOpen={onOpenEvidence} />
          {feedback.addressed?.length ? <><h4>已回应</h4><ul className={styles.list}>{feedback.addressed.map(item => <li key={item}>{item}</li>)}</ul></> : null}
          {feedback.unresolved?.length ? <><h4>仍未知</h4><ul className={styles.list}>{feedback.unresolved.map(item => <li key={item}>{item}</li>)}</ul></> : null}
        </article>)}</div>}
      </Section>
      <Section title="主张、步骤、证据、CVSS 与准备度差异"><DiffList diffs={report.diffs} /></Section>
    </> : null}

    {tab === "retest" ? <>
      <Section title="沿原 Run 请求单 Finding 复测">
        <Notice>复测使用当前计划/授权与原冻结 PoC，不为每条请求创建新 Run。目标前提或正常对照不成立时只能为 inconclusive。</Notice>
        <div className={styles.cardGrid}>
          <label className={styles.field}>修复说明<textarea value={repairStatement} onChange={event => setRepairStatement(event.target.value)} /></label>
          <label className={styles.field}>预期修复目标版本<input value={expectedVersion} onChange={event => setExpectedVersion(event.target.value)} /></label>
          <label className={styles.field}>当前授权快照引用<input value={authorizationRef} onChange={event => setAuthorizationRef(event.target.value)} placeholder="当前 ExecutionPolicy/授权快照 digest" required /></label>
          <label className={styles.field}>使用身份（每行一个）<textarea value={identityRefs} onChange={event => setIdentityRefs(event.target.value)} /></label>
        </div>
        <div className={styles.metaLine}>幂等键 <code>{retestKey}</code></div>
        <div className={styles.actions}><button type="button" className={styles.primaryButton} disabled={busy === "retest" || !repairStatement.trim() || !expectedVersion.trim() || !authorizationRef.trim()} onClick={requestRetest}>提交复测请求</button></div>
      </Section>
      <Section title="复测历史" count={report.retests?.length || 0}>
        {!report.retests?.length ? <Empty>尚无复测记录。</Empty> : <div className={styles.stack}>{report.retests.map(retest => <article className={styles.card} key={retest.retest_id}>
          <div className={styles.metaLine}><Badge tone={retest.result === "fixed" ? "positive" : retest.result === "still_vulnerable" ? "danger" : "warning"}>{retest.result ? retestLabels[retest.result] : "等待现场结果"}</Badge><span>{formatDate(retest.created_at)}</span></div>
          <KeyValues items={[["目标版本", retest.expected_target_version || "—"], ["修复说明", retest.repair_statement || "—"], ["完成时间", formatDate(retest.finished_at)], ["恢复", retest.restoration?.status || "—"]]} />
          <EvidenceList refs={retest.evidence_refs} onOpen={onOpenEvidence} empty="尚无现场证据" />
          {retest.gaps?.length ? <ul className={styles.list}>{retest.gaps.map(gap => <li key={gap}>{gap}</li>)}</ul> : null}
          {retest.restoration?.residual_objects?.length ? <p>残留：{retest.restoration.residual_objects.join("、")}。{retest.restoration.reason || ""}</p> : null}
          <RawDetails value={retest} label="复测记录详情" />
        </article>)}</div>}
      </Section>
    </> : null}

    {tab === "status" ? <>
      <Section title="当前冻结版本的人工状态">
        {report.platform_status?.status ? <><KeyValues items={[["状态", report.platform_status.status], ["平台编号", report.platform_status.platform_reference || report.platform_status.platform_ref || "—"], ["奖金", report.platform_status.amount !== undefined ? `${report.platform_status.amount} ${report.platform_status.currency || ""}` : "—"], ["记录人/时间", `${report.platform_status.recorded_by || "—"} · ${formatDate(report.platform_status.recorded_at)}`], ["备注", report.platform_status.notes || "—"]]} /><Notice>用户记录，未与平台核验。</Notice></> : <Empty>此冻结版本尚无人工平台状态。新报告版本不会继承旧版提交、接受或奖金。</Empty>}
      </Section>
      <Section title="手工记录新状态">
        <div className={styles.toolbar}>
          <label className={styles.field}>状态<select value={status} onChange={event => setStatus(event.target.value)}>{platformStatuses.map(value => <option key={value}>{value}</option>)}</select></label>
          <label className={styles.field}>平台编号<input value={platformRef} onChange={event => setPlatformRef(event.target.value)} /></label>
          <label className={styles.field}>金额（可选）<input value={amount} onChange={event => setAmount(event.target.value)} inputMode="decimal" /></label>
          <label className={`${styles.field} ${styles.fieldGrow}`}>备注<textarea value={statusNotes} onChange={event => setStatusNotes(event.target.value)} /></label>
        </div>
        <div className={styles.actions}><button type="button" className={styles.primaryButton} disabled={busy === "status"} onClick={recordStatus}>保存到 v{report.report_seq}（不自动提交）</button></div>
      </Section>
      <Section title="状态审计历史" count={report.platform_history?.length || 0}>
        {report.platform_history?.length ? <div className={styles.stack}>{report.platform_history.map((item, index) => <article className={styles.card} key={`${item.recorded_at}-${index}`}><KeyValues items={[["旧/新状态", item.status], ["平台编号", item.platform_reference || item.platform_ref || "—"], ["金额", item.amount ?? "—"], ["操作者与时间", `${item.recorded_by || "—"} · ${formatDate(item.recorded_at)}`], ["备注", item.notes || "—"]]} /></article>)}</div> : <Empty>尚无人工状态变化。</Empty>}
      </Section>
    </> : null}

    {tab === "export" ? <>
      <Section title="导出所选冻结版本">
        <div className={styles.toolbar}>
          <label className={styles.field}>格式<select value={format} onChange={event => setFormat(event.target.value)}><option value="zip">完整 ZIP 证据包</option><option value="raw_http">raw HTTP</option><option value="har">HAR</option><option value="jsonl">JSONL</option></select></label>
          <label className={styles.checkbox}><input type="checkbox" checked={redact} onChange={event => setRedact(event.target.checked)} />明确生成派生脱敏版本</label>
          <label className={styles.checkbox}><input type="checkbox" checked={partial} onChange={event => setPartial(event.target.checked)} />允许明确标记 partial 的包</label>
          <button type="button" className={styles.primaryButton} disabled={busy === "export"} onClick={exportEvidence}><Icon name={busy === "export" ? "loader" : "download"} size={14} className={busy === "export" ? "pentest-spin" : undefined} />{busy === "export" ? "正在校验并导出…" : "校验并导出"}</button>
        </div>
        {redact ? <label className={styles.field}>逐项脱敏原值（每行一个；只替换这些明确值）<textarea value={redactionValues} onChange={event => setRedactionValues(event.target.value)} placeholder="Cookie: session=…" /></label> : null}
        {redact ? <Notice tone="warning">脱敏只生成派生文件并记录替换规则；原始证据不变，但相关命令可能无法直接运行。</Notice> : <Notice>默认保留所选 Cookie、token、identifier 和请求响应原值，不扩大到整个 SecretStore。</Notice>}
        {!partial ? <Notice>缺项、hash 不符、撤回或权限失败时应导出失败，不静默生成“完整”包。</Notice> : null}
      </Section>
      <Section title="Manifest 与来源水位">
        <KeyValues items={[["预期条目", report.manifest?.expected_entries ?? "—"], ["实际条目", report.manifest?.actual_entries ?? "—"], ["部分包", report.manifest ? (report.manifest.partial ? "partial" : "complete") : "unknown"], ["来源 Run", report.source_run_ids?.join("、") || "—"]]} />
        {report.manifest?.omissions?.length ? <><h3>遗漏</h3><ul className={styles.list}>{report.manifest.omissions.map(item => <li key={item}>{item}</li>)}</ul></> : null}
        {report.manifest?.failures?.length ? <><h3>失败</h3><ul className={styles.list}>{report.manifest.failures.map(item => <li key={item}>{item}</li>)}</ul></> : null}
        <RawDetails value={{ manifest: report.manifest, source_watermarks: report.source_watermarks }} label="完整 manifest 与来源水位" />
      </Section>
    </> : null}
  </ReviewShell></div>;
}

export default ReportInspector;
