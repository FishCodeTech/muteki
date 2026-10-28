"use client";

import { useCallback, useEffect, useMemo, useState } from "react";
import { Icon } from "@/components/Icon";
import {
  Badge, Empty, ErrorState, EvidenceList, KeyValues, Loading, Notice,
  RawDetails, ReviewShell, ReviewTabs, Section, errorMessage,
  formatDate, lines, newIdempotencyKey, postCommand, querySrc, type EvidenceReference, type OpenEvidence,
} from "./reviewPrimitives";
import styles from "./review.module.css";

type ClaimStatus = "observed" | "supported_inference" | "unverified";
type Readiness = "GREEN" | "YELLOW" | "RED";
type ValidationType = "artifact_review" | "live_reproduction" | "repair_retest";

type Claim = {
  claim_id: string;
  version?: number;
  digest?: string;
  text: string;
  status: ClaimStatus;
  source_run_id?: string;
  evidence_refs?: EvidenceReference[];
  counter_evidence_refs?: EvidenceReference[];
  limitations?: string[];
  missing_proof?: Array<string | { code?: string; reason?: string; action?: string }>;
  identity_refs?: string[];
  target_version?: string;
};

type Review = {
  review_id: string;
  validation_type: ValidationType;
  readiness: Readiness;
  reasons?: string[];
  missing?: Array<{ code?: string; reason?: string; action?: string } | string>;
  reviewer?: string;
  target_version?: string;
  identity_refs?: string[];
  evidence_snapshot?: EvidenceReference[];
  executed_steps?: string[];
  skipped_steps?: Array<{ step?: string; reason?: string } | string>;
  created_at?: string;
  stale_reason?: string;
};

type CvssMetric = { metric?: string; value?: string; rationale?: string; evidence_refs?: EvidenceReference[] };
type Cvss = { vector: string; score?: number; severity?: string; calculation_version?: string; metrics?: CvssMetric[] };
type DedupCandidate = { report_id?: string; finding_id?: string; title?: string; reason?: string; source_run_id?: string };
type Deduplication = { verdict?: "extend_existing" | "independent" | "undetermined"; reason?: string; complete?: boolean; candidates?: DedupCandidate[] };

export type FindingView = {
  finding_id: string;
  title: string;
  summary?: string;
  revision?: number;
  source_run_id?: string;
  affected_assets?: string[];
  target_version?: string;
  claims?: Claim[];
  reviews?: Review[];
  readiness?: Readiness;
  readiness_reason?: string;
  stale_reason?: string;
  cvss?: Cvss;
  deduplication?: Deduplication;
  report_ids?: string[];
};

export type FindingInspectorProps = {
  taskId: string;
  findingId: string;
  initialFinding?: FindingView;
  onOpenEvidence?: OpenEvidence;
  onOpenReport?: (reportId: string) => void;
  onClose?: () => void;
  className?: string;
};

const claimLabels: Record<ClaimStatus, string> = {
  observed: "实际观察", supported_inference: "有证据支持的推论", unverified: "未验证",
};
const readinessLabels: Record<Readiness, string> = { GREEN: "GREEN · 可提交", YELLOW: "YELLOW · 缺关键证据", RED: "RED · 不支持提交" };
const readinessTone = (value?: Readiness) => value === "GREEN" ? "positive" : value === "YELLOW" ? "warning" : "danger";
const validationLabels: Record<ValidationType, string> = {
  artifact_review: "Artifact 内容与 hash 审阅",
  live_reproduction: "独立端到端现场复现",
  repair_retest: "修复复测",
};

function findingFromPayload(payload: unknown): FindingView {
  if (payload && typeof payload === "object" && "finding" in payload) {
    return findingFromPayload((payload as { finding: unknown }).finding);
  }
  const raw = payload as FindingView & {
    review_status?: { state?: string; stale_reasons?: string[] };
    deduplications?: Array<{
      outcome?: Deduplication["verdict"]; reasons?: string[]; history_complete?: boolean;
      comparisons?: Array<{ report_id?: string; outcome?: string; reason?: string }>;
    }>;
  };
  const reviewState = raw.review_status?.state;
  const dedup = (raw.deduplication || raw.deduplications?.at(-1)) as (Deduplication & {
    outcome?: Deduplication["verdict"]; reasons?: string[]; history_complete?: boolean;
    comparisons?: Array<{ report_id?: string; outcome?: string; reason?: string }>;
  }) | undefined;
  const normalizedDedup: Deduplication | undefined = dedup ? {
    verdict: dedup.outcome ?? dedup.verdict,
    complete: dedup.history_complete ?? dedup.complete,
    reason: dedup.reasons?.join("；") ?? dedup.reason,
    candidates: dedup.comparisons?.map(item => ({
      report_id: item.report_id, title: item.report_id, reason: item.reason,
    })) ?? dedup.candidates,
  } : undefined;
  return {
    ...raw,
    title: raw.title || raw.claims?.[0]?.text || raw.finding_id,
    readiness: reviewState === "GREEN" || reviewState === "YELLOW" || reviewState === "RED" ? reviewState : raw.readiness,
    stale_reason: raw.stale_reason || raw.review_status?.stale_reasons?.join("；"),
    deduplication: normalizedDedup,
  };
}

function missingText(value: Review["missing"]): string {
  return (value || []).map(item => typeof item === "string" ? item : [item.reason || item.code, item.action].filter(Boolean).join(" | ")).join("\n");
}

export function FindingInspector({ taskId, findingId, initialFinding, onOpenEvidence, onOpenReport, onClose, className }: FindingInspectorProps) {
  const [finding, setFinding] = useState<FindingView | null>(initialFinding || null);
  const [loading, setLoading] = useState(!initialFinding);
  const [error, setError] = useState("");
  const [tab, setTab] = useState("claims");
  const [validationType, setValidationType] = useState<ValidationType>("artifact_review");
  const [readiness, setReadiness] = useState<Readiness>("YELLOW");
  const [reasons, setReasons] = useState("");
  const [missing, setMissing] = useState("");
  const [executed, setExecuted] = useState("");
  const [skipped, setSkipped] = useState("");
  const [targetVersion, setTargetVersion] = useState("");
  const [identityRefs, setIdentityRefs] = useState("");
  const [alternatives, setAlternatives] = useState("");
  const [executionReceipts, setExecutionReceipts] = useState("");
  const [independentContext, setIndependentContext] = useState(false);
  const [retestId, setRetestId] = useState("");
  const [operatorId, setOperatorId] = useState("");
  const [reviewer, setReviewer] = useState("");
  const [saving, setSaving] = useState(false);
  const [saveMessage, setSaveMessage] = useState("");

  const load = useCallback(async () => {
    setLoading(true); setError("");
    try {
      const [claims, reviews, status, deduplications, reports, task] = await Promise.all([
        querySrc<Claim[]>(taskId, "src.finding.claims", { finding_id: findingId }),
        querySrc<Review[]>(taskId, "src.finding.reviews", { finding_id: findingId }),
        querySrc<{ state?: string; review?: { readiness?: Readiness; cvss?: Cvss }; stale_reasons?: string[] }>(taskId, "src.finding.status", { finding_id: findingId }),
        querySrc<Deduplication[]>(taskId, "src.finding.deduplications", { finding_id: findingId }),
        querySrc<Array<{ report_id: string }>>(taskId, "src.report.list", { finding_id: findingId }),
        querySrc<{ binding: { principal_id: string } }>(taskId, "src.task.get"),
      ]);
      setOperatorId(task.binding.principal_id);
      const payload: FindingView = {
        finding_id: findingId,
        title: claims[0]?.text || findingId,
        summary: claims[0]?.text,
        revision: Math.max(1, ...claims.map(claim => claim.version || 1)),
        source_run_id: claims[0]?.source_run_id,
        target_version: claims[0]?.target_version,
        affected_assets: [],
        claims, reviews,
        readiness: status.review?.readiness,
        stale_reason: status.stale_reasons?.join("；"),
        cvss: status.review?.cvss,
        deduplication: deduplications.at(-1),
        report_ids: reports.map(report => report.report_id),
      };
      setFinding(findingFromPayload(payload));
    } catch (cause) { setError(errorMessage(cause)); }
    finally { setLoading(false); }
  }, [taskId, findingId]);
  useEffect(() => { void load(); }, [load]);

  const latestReview = finding?.reviews?.at(-1);
  const effectiveReadiness = latestReview?.readiness || finding?.readiness;
  useEffect(() => {
    if (!finding) return;
    setReadiness(latestReview?.readiness || finding.readiness || "YELLOW");
    setReasons((latestReview?.reasons || []).join("\n"));
    setMissing(missingText(latestReview?.missing));
    setTargetVersion(latestReview?.target_version || finding.target_version || "");
    setReviewer(latestReview?.reviewer || operatorId);
  }, [finding, latestReview, operatorId]);

  const saveReview = async () => {
    if (!finding) return;
    setSaving(true); setSaveMessage(""); setError("");
    try {
      const cvss = finding.cvss ? {
        vector: finding.cvss.vector,
        metric_reasons: Object.fromEntries((finding.cvss.metrics || []).map(metric => [metric.metric, {
          value: metric.value, rationale: metric.rationale || "", evidence_refs: metric.evidence_refs || [],
        }])),
        base_score: finding.cvss.score,
        temporal_score: (finding.cvss as { temporal_score?: number }).temporal_score ?? finding.cvss.score,
        environmental_score: (finding.cvss as { environmental_score?: number }).environmental_score ?? finding.cvss.score,
      } : undefined;
      await postCommand(taskId, "src.finding.review.record", {
        finding_id: finding.finding_id,
        report_id: finding.report_ids?.[0],
        idempotency_key: newIdempotencyKey("src-review"),
        validation_type: validationType,
        readiness,
        claim_versions: (finding.claims || []).map(claim => ({ claim_id: claim.claim_id, version: claim.version || 1, digest: claim.digest || "" })),
        reasons: lines(reasons),
        missing: lines(missing).map(line => {
          const [reason, ...action] = line.split("|");
          return { code: reason.trim(), reason: reason.trim(), action: action.join("|").trim() };
        }),
        alternative_explanations: lines(alternatives).map(line => {
          const [explanation, ...assessment] = line.split("|");
          const assessmentText = assessment.join("|").trim();
          return {
            explanation: explanation.trim(), assessment: assessmentText,
            status: assessmentText ? "addressed" : "unresolved",
            evidence_refs: (finding.claims || []).flatMap(claim => claim.counter_evidence_refs || []),
          };
        }),
        evidence_snapshot: (finding.claims || []).flatMap(claim => [...(claim.evidence_refs || []), ...(claim.counter_evidence_refs || [])]),
        executed_steps: lines(executed),
        skipped_steps: lines(skipped).map(line => {
          const [step, ...reason] = line.split("|");
          return { step: step.trim(), reason: reason.join("|").trim() };
        }),
        reviewer: reviewer.trim(),
        target_version: targetVersion,
        identity_refs: lines(identityRefs),
        independent_context: independentContext,
        execution_receipt_ids: lines(executionReceipts),
        retest_id: validationType === "repair_retest" ? retestId : "",
        cvss,
      });
      setSaveMessage("审阅已保存为新版本；历史记录未被改写。");
      await load();
    } catch (cause) { setError(errorMessage(cause)); }
    finally { setSaving(false); }
  };

  const claimCounts = useMemo(() => ({
    evidence: finding?.claims?.reduce((total, claim) => total + (claim.evidence_refs?.length || 0), 0) || 0,
    counter: finding?.claims?.reduce((total, claim) => total + (claim.counter_evidence_refs?.length || 0), 0) || 0,
  }), [finding]);

  if (loading && !finding) return <Loading label="正在读取 Finding、主张和审阅记录…" />;
  if (error && !finding) return <ErrorState message={error} onRetry={() => void load()} />;
  if (!finding) return <Empty>当前 Finding 不可读或已被撤回。</Empty>;

  return <div className={className}><ReviewShell
    title={finding.title}
    subtitle={`Finding ${finding.finding_id}${finding.source_run_id ? ` · ${finding.source_run_id}` : ""}`}
    actions={<>
      {effectiveReadiness ? <Badge tone={readinessTone(effectiveReadiness)}>{readinessLabels[effectiveReadiness]}</Badge> : null}
      {onClose ? <button type="button" className={styles.secondaryButton} onClick={onClose} aria-label="关闭 Finding"><Icon name="x" size={15} /></button> : null}
    </>}
  >
    {error ? <Notice tone="danger">{error}</Notice> : null}
    {saveMessage ? <Notice tone="success">{saveMessage}</Notice> : null}
    {finding.stale_reason || latestReview?.stale_reason ? <Notice tone="warning">结论已陈旧：{finding.stale_reason || latestReview?.stale_reason}。旧审阅仍保留，需要重新核对。</Notice> : null}
    {finding.summary ? <p>{finding.summary}</p> : null}
    <KeyValues items={[
      ["目标版本", finding.target_version || "未固定"],
      ["受影响资产", finding.affected_assets?.length ? finding.affected_assets.join("、") : "—"],
      ["报告版本", finding.report_ids?.length ? finding.report_ids.map(id => <button type="button" className={styles.textButton} key={id} onClick={() => onOpenReport?.(id)}>{id}</button>) : "尚未冻结"],
    ]} />
    <ReviewTabs label="Finding 检查视图" active={tab} onChange={setTab} items={[
      { id: "claims", label: "主张与反证", count: finding.claims?.length || 0 },
      { id: "reviews", label: "准备度与审阅", count: finding.reviews?.length || 0 },
      { id: "cvss", label: "CVSS 3.1" },
      { id: "dedup", label: "去重判断" },
    ]} />

    {tab === "claims" ? <>
      <Section title="原子主张" count={finding.claims?.length || 0}>
        {!finding.claims?.length ? <Empty>尚无主张记录。工具输出、模型推论和未验证影响必须分开登记。</Empty> : null}
        <div className="pentest-report-proof-list">{(finding.claims ?? []).map(claim => <article className="pentest-report-proof" key={claim.claim_id}>
          <header className="pentest-report-proof-heading"><strong>{claim.text}</strong><Badge tone={claim.status === "observed" ? "positive" : claim.status === "unverified" ? "warning" : "accent"}>{claimLabels[claim.status]}</Badge></header>
          <div className="pentest-report-proof-body">
            <div className={styles.metaLine}><code>{claim.claim_id}</code>{claim.target_version ? <span>目标 {claim.target_version}</span> : null}{claim.identity_refs?.length ? <span>身份 {claim.identity_refs.join("、")}</span> : null}</div>
            <h4>支持证据</h4><EvidenceList refs={claim.evidence_refs} onOpen={onOpenEvidence} empty="没有支持证据" />
            <h4>反证与最强替代解释</h4><EvidenceList refs={claim.counter_evidence_refs} onOpen={onOpenEvidence} empty="未登记反证；审阅时仍需核对替代解释" />
            {claim.limitations?.length ? <><h4>限制</h4><ul className={styles.list}>{claim.limitations.map(item => <li key={item}>{item}</li>)}</ul></> : null}
            {claim.missing_proof?.length ? <><h4>缺失证明</h4><ul className={styles.list}>{claim.missing_proof.map((item, index) => <li key={index}>{typeof item === "string" ? item : item.reason || item.code || item.action}</li>)}</ul></> : null}
          </div>
        </article>)}</div>
      </Section>
      <div className={styles.metaLine}>支持证据 {claimCounts.evidence} 项 · 反证 {claimCounts.counter} 项。证据数量本身不决定准备度。</div>
    </> : null}

    {tab === "reviews" ? <>
      <Section title="新增审阅版本">
        {validationType === "artifact_review" ? <Notice>Artifact review 只核对已保存内容和 hash，不会被显示成现场独立复现。</Notice> : null}
        {validationType === "live_reproduction" ? <Notice tone="warning">只有实际执行了端到端步骤才选择现场复现；未执行步骤必须列入下方，不得用文字声明代替。</Notice> : null}
        {validationType === "repair_retest" ? <Notice tone="warning">修复复测必须保留正常业务对照；前提或目标版本不成立时应为 inconclusive，而不是 fixed。</Notice> : null}
        <div className={styles.toolbar}>
          <label className={styles.field}>验证类型<select value={validationType} onChange={event => setValidationType(event.target.value as ValidationType)}>{Object.entries(validationLabels).map(([value, label]) => <option value={value} key={value}>{label}</option>)}</select></label>
          <label className={styles.field}>准备度<select value={readiness} onChange={event => setReadiness(event.target.value as Readiness)}>{Object.entries(readinessLabels).map(([value, label]) => <option value={value} key={value}>{label}</option>)}</select></label>
          <label className={styles.field}>目标版本<input value={targetVersion} onChange={event => setTargetVersion(event.target.value)} placeholder="构建、部署或观测版本" /></label>
          <label className={styles.field}>Reviewer（当前操作者）<input required value={reviewer} onChange={event => setReviewer(event.target.value)} placeholder={operatorId || "当前用户"} /></label>
        </div>
        <div className={styles.cardGrid}>
          <label className={styles.field}>结论理由（每行一项）<textarea value={reasons} onChange={event => setReasons(event.target.value)} /></label>
          <label className={styles.field}>精确缺口与下一动作（每行“缺口 | 动作”）<textarea value={missing} onChange={event => setMissing(event.target.value)} /></label>
          <label className={styles.field}>实际执行步骤（每行一步）<textarea value={executed} onChange={event => setExecuted(event.target.value)} /></label>
          <label className={styles.field}>未执行步骤（每行“步骤 | 原因”）<textarea value={skipped} onChange={event => setSkipped(event.target.value)} /></label>
          <label className={styles.field}>使用身份（每行一个身份引用）<textarea value={identityRefs} onChange={event => setIdentityRefs(event.target.value)} /></label>
          <label className={styles.field}>替代解释与处理（每行“解释 | 核对结论”）<textarea value={alternatives} onChange={event => setAlternatives(event.target.value)} /></label>
          {validationType === "live_reproduction" ? <>
            <label className={styles.field}>现场执行回执 ID（每行一个）<textarea value={executionReceipts} onChange={event => setExecutionReceipts(event.target.value)} placeholder="只有已完成且结果已知的真实回执" /></label>
            <label className={styles.checkbox}><input type="checkbox" checked={independentContext} onChange={event => setIndependentContext(event.target.checked)} />确认审阅上下文未参与原探索</label>
          </> : null}
          {validationType === "repair_retest" ? <label className={styles.field}>已完成复测记录 ID<input value={retestId} onChange={event => setRetestId(event.target.value)} /></label> : null}
        </div>
        <Notice>Reviewer 为当前操作者，不表示独立复核；只有用户明确确认实际审阅上下文未参与原探索时，才记录 independent_context。</Notice>
        <div className={styles.actions}><button type="button" className={styles.primaryButton} disabled={saving || !lines(reasons).length || !reviewer.trim()} onClick={() => void saveReview()}><Icon name={saving ? "loader" : "check"} size={14} className={saving ? "pentest-spin" : undefined} />{saving ? "正在保存…" : "保存为新审阅版本"}</button></div>
      </Section>
      <Section title="审阅历史" count={finding.reviews?.length || 0}>
        {!finding.reviews?.length ? <Empty>尚无审阅记录。内部 accepted 不等于 GREEN 或平台接受。</Empty> : null}
        <div className={styles.stack}>{finding.reviews?.map(review => <article className={styles.card} key={review.review_id}>
          <div className={styles.metaLine}><Badge tone={readinessTone(review.readiness)}>{review.readiness}</Badge><strong>{validationLabels[review.validation_type]}</strong><span>{formatDate(review.created_at)}</span></div>
          <KeyValues items={[["审阅者", review.reviewer || "—"], ["目标版本", review.target_version || "—"], ["使用身份", review.identity_refs?.join("、") || "—"]]} />
          {review.stale_reason ? <Notice tone="warning">已陈旧：{review.stale_reason}</Notice> : null}
          {review.reasons?.length ? <ul className={styles.list}>{review.reasons.map(reason => <li key={reason}>{reason}</li>)}</ul> : null}
          {review.missing?.length ? <><h4>缺口与下一动作</h4><ul className={styles.list}>{review.missing.map((item, index) => <li key={index}>{typeof item === "string" ? item : <>{item.reason || item.code}{item.action ? ` · 下一步：${item.action}` : ""}</>}</li>)}</ul></> : null}
          {review.executed_steps?.length ? <><h4>实际执行</h4><ol className={styles.list}>{review.executed_steps.map(step => <li key={step}>{step}</li>)}</ol></> : null}
          {review.skipped_steps?.length ? <><h4>未执行及原因</h4><ul className={styles.list}>{review.skipped_steps.map((item, index) => <li key={index}>{typeof item === "string" ? item : `${item.step || "步骤"}：${item.reason || "未说明"}`}</li>)}</ul></> : null}
          <EvidenceList refs={review.evidence_snapshot} onOpen={onOpenEvidence} empty="未固定审阅证据快照" />
          <RawDetails value={review} label="审阅记录详情" />
        </article>)}</div>
      </Section>
    </> : null}

    {tab === "cvss" ? <Section title="CVSS 3.1 向量与逐项依据">
      {!finding.cvss ? <Empty>尚未保存 CVSS 评估。空向量不会自动计算风险或业务影响。</Empty> : <>
        <KeyValues items={[["向量", <code key="vector">{finding.cvss.vector}</code>], ["分数", finding.cvss.score ?? "—"], ["等级", finding.cvss.severity || "—"], ["计算版本", finding.cvss.calculation_version || "—"]]} />
        {finding.cvss.metrics?.length ? <div className={styles.cardGrid}>{finding.cvss.metrics.map((metric, index) => <article className={styles.card} key={`${metric.metric}-${index}`}><h3>{metric.metric || "指标"} · {metric.value || "—"}</h3><p>{metric.rationale || "未填写理由"}</p><EvidenceList refs={metric.evidence_refs} onOpen={onOpenEvidence} /></article>)}</div> : <Empty>未提供逐指标理由。</Empty>}
        <RawDetails value={finding.cvss} label="完整 CVSS 输入" />
      </>}
    </Section> : null}

    {tab === "dedup" ? <Section title="根因、信任边界与修复点比较">
      {!finding.deduplication ? <Empty>尚未进行去重比较。标题相似或不同都不能单独证明重复或独立。</Empty> : <>
        <KeyValues items={[["判断", finding.deduplication.verdict || "undetermined"], ["历史完整性", finding.deduplication.complete === false ? "不完整，不能排除重复" : "已核对可读历史"], ["理由", finding.deduplication.reason || "—"]]} />
        {finding.deduplication.complete === false ? <Notice tone="warning">历史检索缺页、来源不可读或水位落后，因此重复性未知，不能当作独立新发现。</Notice> : null}
        {finding.deduplication.candidates?.length ? <div className={styles.stack}>{finding.deduplication.candidates.map((candidate, index) => <article className={styles.card} key={`${candidate.report_id}-${index}`}><h3>{candidate.title || candidate.finding_id || candidate.report_id}</h3><p>{candidate.reason || "未提供比较理由"}</p><div className={styles.metaLine}><code>{candidate.report_id}</code>{candidate.source_run_id ? <span>{candidate.source_run_id}</span> : null}</div>{candidate.report_id && onOpenReport ? <button type="button" className={styles.textButton} onClick={() => onOpenReport(candidate.report_id!)}>打开候选报告<Icon name="arrowUpRight" size={12} /></button> : null}</article>)}</div> : <Empty>没有候选报告；这不等同于完成全历史去重。</Empty>}
        <RawDetails value={finding.deduplication} label="完整去重记录" />
      </>}
    </Section> : null}
  </ReviewShell></div>;
}

export default FindingInspector;
