"use client";

import { useCallback, useEffect, useState } from "react";
import { Icon } from "@/components/Icon";
import {
  Badge, Empty, ErrorState, EvidenceList, KeyValues, Loading, Notice,
  RawDetails, ReviewShell, ReviewTabs, Section, errorMessage,
  formatDate, querySrc, type EvidenceReference, type OpenEvidence,
} from "./reviewPrimitives";
import styles from "./review.module.css";

type PlanSnapshot = {
  plan_id: string; version: number; created_at?: string; question?: string; rationale?: string;
  known_gaps?: string[]; proposed_actions?: string[]; evidence_refs?: EvidenceReference[];
  authorization?: { source_refs?: EvidenceReference[]; allowed_targets?: string[]; excluded_targets?: string[]; identities?: string[]; conditions?: string[]; unknown?: string[] };
  superseded_by?: string;
};
type ResearchMaterial = {
  material_id: string; name?: string; source_url?: string; source_commit?: string; version?: string; digest?: string;
  distilled_version?: string; selected_fragments?: Array<{ location?: string; text?: string; source_ref?: EvidenceReference }>;
  capability_mappings?: Array<{ required?: string; capability?: string; version?: string; status?: string; reason?: string }>;
  referenced_at?: string; readable?: boolean; stale_reason?: string;
};
type ApiOperation = { operation_id?: string; method?: string; path?: string; server?: string; parameters?: Array<{ name?: string; in?: string; required?: boolean; original_name?: string }>; security?: string[]; schema_refs?: string[]; source_location?: string; runtime_exchange_ids?: string[] };
type ApiComparison = { comparison_id?: string; kind?: string; method?: string; path?: string; field?: string; declared?: unknown; observed?: unknown; environment?: string; observed_at?: string; exchange_ids?: string[] };
type OpenApiDocument = { document_id: string; artifact?: EvidenceReference; version?: string; digest?: string; source_digest?: string; operations?: ApiOperation[]; security_schemes?: Record<string, unknown> | Array<Record<string, unknown>>; unresolved_refs?: string[]; errors?: Array<{ location?: string; message?: string }>; comparisons?: ApiComparison[] };
type SourceSink = { flow_id: string; source?: string; transformations?: string[]; transforms?: unknown; propagation?: string[]; sink?: string; file?: string; module?: string; function?: string; location?: string; execution_context?: string; required_interaction?: string; static_evidence_refs?: EvidenceReference[]; runtime_evidence_refs?: EvidenceReference[]; frame_origin?: string; csp?: string; status?: string };
type RuntimeAction = { action_id: string; label?: string; method?: string; path?: string; parameters?: Array<{ name?: string; in?: string; original_name?: string }>; identity_ref?: string; tenant?: string; object_ref?: string; before_state?: string; after_state?: string; exchange_ids?: string[]; browser_generation?: number; evidence_refs?: EvidenceReference[] };
type PermissionQuestion = { question_id: string; subject?: string; object?: string; action?: string; state?: string; expected_boundary?: string; actual_evidence_refs?: EvidenceReference[]; missing?: string[]; origin?: string; status?: string };
type BusinessModel = { roles?: string[]; accounts?: string[]; tenants?: string[]; object_types?: string[]; state_transitions?: string[]; sensitive_actions?: string[]; constraints?: Array<{ text?: string; origin?: string; evidence_refs?: EvidenceReference[] }>; questions?: PermissionQuestion[] };
type ResearchCase = { case_id: string; kind?: "case" | "counterexample"; title: string; problem_pattern?: string; prerequisites?: string[]; effective_experiment?: string; root_cause?: string; decisive_evidence_refs?: EvidenceReference[]; repair_boundary?: string; invalid_conditions?: string[]; exclusion_evidence_refs?: EvidenceReference[]; source_report_id?: string; version?: number; readable?: boolean; stale_reason?: string; match_reason?: string };

export type ResearchView = {
  research_id?: string; title?: string; current_plan_id?: string; plans?: PlanSnapshot[]; materials?: ResearchMaterial[];
  openapi?: OpenApiDocument[]; source_to_sink?: SourceSink[]; runtime_actions?: RuntimeAction[];
  business?: BusinessModel; cases?: ResearchCase[]; run_ids?: string[];
  watermarks?: Array<Record<string, unknown>>;
};

export type ResearchInspectorProps = {
  taskId: string;
  researchId?: string;
  onOpenEvidence?: OpenEvidence;
  onOpenExchange?: (exchangeId: string) => void;
  onOpenRun?: (runId: string) => void;
  onClose?: () => void;
  className?: string;
};

function text(value: unknown): string {
  if (value === undefined || value === null || value === "") return "";
  if (typeof value === "string") return value;
  if (typeof value === "object") {
    const row = value as Record<string, unknown>;
    return String(row.text || row.name || row.id || row.location || JSON.stringify(value));
  }
  return String(value);
}

function refs(values: unknown): EvidenceReference[] {
  if (!Array.isArray(values)) return [];
  return values.map(value => typeof value === "string" ? { id: value } : value as EvidenceReference);
}

function researchFromPayload(payload: unknown): ResearchView {
  if (payload && typeof payload === "object" && "research" in payload) {
    return researchFromPayload((payload as { research: unknown }).research);
  }
  const raw = payload as Omit<ResearchView, "plans" | "materials" | "openapi" | "source_to_sink" | "runtime_actions" | "business"> & {
    plans?: Array<PlanSnapshot & { gaps?: string[]; actions?: string[] }>;
    materials?: Array<Omit<ResearchMaterial, "capability_mappings"> & {
      content_digest?: string; distillation_version?: string; registered_at?: string;
      capability_mappings?: Array<{ required?: string; capability?: string; required_version?: string; actual_version?: string; version?: string; status?: string; available?: boolean; code?: string; reason?: string }>;
    }>;
    openapi?: Array<Omit<OpenApiDocument, "unresolved_refs"> & {
      declaration?: { servers?: Array<{ url?: string }>; operations?: ApiOperation[]; security_schemes?: Array<Record<string, unknown>> };
      unresolved_refs?: Array<{ location?: string; ref?: string; message?: string } | string>;
    }>;
    source_to_sink?: Array<Omit<SourceSink, "source" | "sink"> & {
      source?: unknown; sink?: unknown; static_refs?: unknown; runtime_refs?: unknown; transforms?: unknown;
    }>;
    runtime_actions?: Array<RuntimeAction & { action?: unknown; exchange_id?: string; observed_url?: string; object_refs?: { tenant?: string } }>;
    business?: Omit<BusinessModel, "roles" | "accounts" | "tenants" | "object_types" | "state_transitions" | "sensitive_actions" | "constraints" | "questions"> & {
      roles?: unknown[]; accounts?: unknown[]; tenants?: unknown[]; object_types?: unknown[];
      state_transitions?: unknown[]; sensitive_actions?: unknown[];
      constraints?: Array<{ text?: string; provenance?: string; origin?: string; source_refs?: unknown; evidence_refs?: EvidenceReference[] }>;
      questions?: Array<Omit<PermissionQuestion, "question_id" | "subject" | "object" | "action" | "missing" | "actual_evidence_refs"> & {
        question_id?: string; question?: string; actor?: unknown; object_context?: unknown; action?: unknown;
        subject?: string; object?: string; missing?: string[]; actual_evidence_refs?: EvidenceReference[];
        missing_comparisons?: Array<Record<string, unknown>>; source_refs?: unknown; evidence_refs?: unknown;
      }>;
    };
  };
  return {
    ...raw,
    plans: raw.plans?.map(plan => ({ ...plan, known_gaps: plan.known_gaps || plan.gaps, proposed_actions: plan.proposed_actions || plan.actions })),
    materials: raw.materials?.map(material => ({
      ...material,
      digest: material.digest || material.content_digest,
      distilled_version: material.distilled_version || material.distillation_version,
      referenced_at: material.referenced_at || material.registered_at,
      capability_mappings: material.capability_mappings?.map(mapping => ({
        ...mapping,
        required: mapping.required || mapping.capability,
        version: mapping.version || mapping.actual_version || mapping.required_version,
        status: mapping.status || (mapping.available === false || mapping.code === "capability_unavailable" ? "capability_unavailable" : "available"),
        reason: mapping.reason || mapping.code,
      })),
    })),
    openapi: raw.openapi?.map(document => {
      const declaration = document.declaration;
      return {
        ...document,
        digest: document.digest || document.source_digest,
        operations: document.operations || declaration?.operations?.map(operation => ({ ...operation, server: operation.server || declaration?.servers?.[0]?.url })),
        security_schemes: document.security_schemes || declaration?.security_schemes,
        unresolved_refs: document.unresolved_refs?.map(item => typeof item === "string" ? item : `${item.location || "$"}: ${item.ref || item.message || "未解析引用"}`),
      };
    }),
    source_to_sink: raw.source_to_sink?.map((flow): SourceSink => ({
      ...flow,
      source: text(flow.source), sink: text(flow.sink),
      transformations: flow.transformations || (Array.isArray(flow.transforms) ? flow.transforms.map(text) : []),
      static_evidence_refs: flow.static_evidence_refs || refs(flow.static_refs),
      runtime_evidence_refs: flow.runtime_evidence_refs || refs(flow.runtime_refs),
    })),
    runtime_actions: raw.runtime_actions?.map(action => ({
      ...action,
      label: action.label || text(action.action),
      path: action.path || action.observed_url,
      exchange_ids: action.exchange_ids || (action.exchange_id ? [action.exchange_id] : []),
      tenant: action.tenant || action.object_refs?.tenant,
    })),
    business: raw.business ? {
      ...raw.business,
      roles: raw.business.roles?.map(text), accounts: raw.business.accounts?.map(text),
      tenants: raw.business.tenants?.map(text), object_types: raw.business.object_types?.map(text),
      state_transitions: raw.business.state_transitions?.map(text), sensitive_actions: raw.business.sensitive_actions?.map(text),
      constraints: raw.business.constraints?.map(constraint => ({
        ...constraint, origin: constraint.origin || constraint.provenance,
        evidence_refs: constraint.evidence_refs || refs(constraint.source_refs),
      })),
      questions: raw.business.questions?.map(question => ({
        ...question,
        question_id: question.question_id || text(question.question),
        subject: question.subject || text(question.actor),
        object: question.object || text(question.object_context),
        action: text(question.action),
        origin: question.origin,
        missing: question.missing || question.missing_comparisons?.map(text),
        actual_evidence_refs: question.actual_evidence_refs || refs(question.evidence_refs),
      })),
    } : undefined,
  };
}

function ExchangeLinks({ ids, onOpen }: { ids?: string[]; onOpen?: (id: string) => void }) {
  if (!ids?.length) return <span className={styles.muted}>无实际 exchange</span>;
  return <div className={styles.evidenceList}>{ids.map(id => onOpen ? <button type="button" key={id} onClick={() => onOpen(id)}><Icon name="arrowUpRight" size={12} />{id}</button> : <code key={id}>{id}</code>)}</div>;
}

export function ResearchInspector({ taskId, researchId, onOpenEvidence, onOpenExchange, onOpenRun, onClose, className }: ResearchInspectorProps) {
  const [research, setResearch] = useState<ResearchView | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");
  const [tab, setTab] = useState("plan");
  const [planId, setPlanId] = useState("");

  const load = useCallback(async () => {
    setLoading(true); setError("");
    try {
      const [records, cases, task] = await Promise.all([
        querySrc<Array<{ record_id: string; kind: string; revision: number; payload: Record<string, any> }>>(taskId, "src.research.records", { kind: "", limit: 1000 }),
        querySrc<Array<Record<string, any>>>(taskId, "src.knowledge.search", { query: "" }),
        querySrc<{ title?: string; watermarks?: Array<Record<string, unknown>> }>(taskId, "src.task.get"),
      ]);
      const plans = records.filter(row => row.kind === "research_plan").map(row => ({
        ...row.payload, plan_id: row.payload.plan_id || row.record_id,
        version: row.payload.version || row.revision,
      } as PlanSnapshot));
      const currentPlanId = researchId || plans.at(-1)?.plan_id || plans[0]?.plan_id || "";
      const authRows = records.filter(row => row.kind === "authorization_snapshot");
      const latestAuth = authRows.at(-1)?.payload;
      const withAuthorization = plans.map(plan => {
        const matchedAuth = (plan as { authorization_ref?: string }).authorization_ref
          ? authRows.find(row => row.record_id === (plan as { authorization_ref?: string }).authorization_ref)?.payload
          : (plan.plan_id === currentPlanId ? latestAuth : undefined);
        return matchedAuth ? { ...plan, authorization: matchedAuth } : plan;
      });
      const payload: ResearchView = {
        research_id: currentPlanId || undefined,
        title: task.title || "研究资料与业务线索",
        current_plan_id: currentPlanId || undefined,
        plans: withAuthorization,
        materials: records.filter(row => row.kind === "skill_material").map(row => ({ ...row.payload, material_id: row.record_id, name: row.payload.name || row.payload.source_url })),
        openapi: records.filter(row => row.kind === "research_document").map(row => ({ ...row.payload, document_id: row.record_id })),
        source_to_sink: records.filter(row => row.kind === "dataflow").map(row => ({ ...row.payload, flow_id: row.payload.flow_id || row.record_id })),
        runtime_actions: records.filter(row => row.kind === "page_action").map(row => ({ ...row.payload, action_id: row.payload.action_id || row.record_id })),
        business: records.find(row => row.kind === "business_model")?.payload,
        cases: cases.map(item => ({
          ...item, case_id: item.case_id || item.entry_id,
          title: item.title || item.case_id || item.entry_id,
          prerequisites: item.prerequisites || Object.values(item.business_preconditions || {}),
          effective_experiment: item.effective_experiment || item.applicability?.effective_experiment,
          decisive_evidence_refs: item.decisive_evidence_refs || item.evidence || [],
        })),
        watermarks: task.watermarks || [],
      };
      const value = researchFromPayload(payload);
      setResearch(value);
      setPlanId(current => current || value.current_plan_id || value.plans?.[0]?.plan_id || "");
    } catch (cause) { setError(errorMessage(cause)); }
    finally { setLoading(false); }
  }, [taskId, researchId]);
  useEffect(() => { void load(); }, [load]);

  if (loading && !research) return <Loading label="正在读取计划、研究资料和运行证据…" />;
  if (error && !research) return <ErrorState message={error} onRetry={() => void load()} />;
  if (!research) return <Empty>研究资料不存在或当前版本不可读。</Empty>;
  const plan = research.plans?.find(item => item.plan_id === planId) || research.plans?.[0];
  const apiOperations = research.openapi?.reduce((total, document) => total + (document.operations?.length || 0), 0) || 0;
  const apiComparisons = research.openapi?.flatMap(document => document.comparisons || []) || [];

  return <div className={className}><ReviewShell
    title={research.title || "研究资料与业务线索"}
    subtitle="文档声明、静态线索、实际运行和受控验证分层展示"
    actions={<>
      {research.run_ids?.length ? research.run_ids.map(runId => <button type="button" className={styles.secondaryButton} key={runId} onClick={() => onOpenRun?.(runId)}><Icon name="play" size={13} />{runId}</button>) : null}
      {onClose ? <button type="button" className={styles.secondaryButton} onClick={onClose} aria-label="关闭研究资料"><Icon name="x" size={15} /></button> : null}
    </>}
  >
    {error ? <Notice tone="danger">{error}</Notice> : null}
    {research.watermarks?.some(item => item.state && item.state !== "ready") ? <Notice tone="warning">部分来源索引落后或失败；当前结果不是“没有资料”，请查看各来源水位和完整原文。</Notice> : null}
    <ReviewTabs label="研究资料视图" active={tab} onChange={setTab} items={[
      { id: "plan", label: "计划与方法包", count: research.plans?.length || 0 },
      { id: "api", label: "OpenAPI 与对照", count: apiOperations },
      { id: "runtime", label: "静态与运行态", count: (research.source_to_sink?.length || 0) + (research.runtime_actions?.length || 0) },
      { id: "business", label: "业务权限", count: research.business?.questions?.length || 0 },
      { id: "cases", label: "案例与反例", count: research.cases?.length || 0 },
    ]} />

    {tab === "plan" ? <>
      <Section title="版本化研究计划">
        {research.plans?.length ? <div className={styles.toolbar}><label className={styles.field}>计划版本<select value={plan?.plan_id || ""} onChange={event => setPlanId(event.target.value)}>{[...research.plans].sort((a, b) => b.version - a.version).map(item => <option key={item.plan_id} value={item.plan_id}>v{item.version} · {formatDate(item.created_at)}</option>)}</select></label></div> : null}
        {!plan ? <Empty>尚无研究计划。普通公开资料研究可以在没有 Pentest Run 的情况下结束。</Empty> : <article className={styles.card}>
          <div className={styles.metaLine}><Badge>计划 v{plan.version}</Badge><span>{formatDate(plan.created_at)}</span>{plan.superseded_by ? <span>由 {plan.superseded_by} 取代</span> : null}</div>
          <KeyValues items={[["当前问题", plan.question || "—"], ["选择依据", plan.rationale || "—"]]} />
          {plan.known_gaps?.length ? <><h3>已知缺口</h3><ul className={styles.list}>{plan.known_gaps.map(item => <li key={item}>{item}</li>)}</ul></> : null}
          {plan.proposed_actions?.length ? <><h3>拟执行动作</h3><ul className={styles.list}>{plan.proposed_actions.map(item => <li key={item}>{item}</li>)}</ul></> : null}
          <h3>计划依据</h3><EvidenceList refs={plan.evidence_refs} onOpen={onOpenEvidence} />
          {plan.authorization ? <><h3>授权快照</h3><KeyValues items={[["允许目标", plan.authorization.allowed_targets?.join("、") || "未明确"], ["排除项", plan.authorization.excluded_targets?.join("、") || "—"], ["身份", plan.authorization.identities?.join("、") || "—"], ["条件", plan.authorization.conditions?.join("；") || "—"], ["未知项", plan.authorization.unknown?.join("；") || "—"]]} /><EvidenceList refs={plan.authorization.source_refs} onOpen={onOpenEvidence} /><Notice>计划接受目标不等于每种业务动作已获授权；执行仍以当前策略为准。</Notice></> : null}
          <RawDetails value={plan} label="完整计划快照" />
        </article>}
      </Section>
      <Section title="按需研究材料与能力映射" count={research.materials?.length || 0}>
        {!research.materials?.length ? <Empty>尚未选用外部方法包；不会批量注入系统 prompt。</Empty> : <div className={styles.stack}>{research.materials.map(material => <article className={styles.card} key={material.material_id}>
          <div className={styles.metaLine}><strong>{material.name || material.material_id}</strong>{material.readable === false ? <Badge tone="danger">不可读</Badge> : null}{material.stale_reason ? <Badge tone="warning">已陈旧</Badge> : null}</div>
          <KeyValues items={[["源版本", material.source_commit || material.version || "—"], ["内容 digest", <code key="digest">{material.digest || "—"}</code>], ["提炼版本", material.distilled_version || "—"], ["引用时间", formatDate(material.referenced_at)]]} />
          {material.source_url ? <a href={material.source_url} target="_blank" rel="noreferrer">打开完整来源<Icon name="arrowUpRight" size={12} /></a> : null}
          {material.stale_reason ? <Notice tone="warning">{material.stale_reason}</Notice> : null}
          {material.selected_fragments?.length ? <><h4>实际选用片段</h4>{material.selected_fragments.map((fragment, index) => <details className={styles.rawDetails} key={`${fragment.location}-${index}`}><summary>{fragment.location || `片段 ${index + 1}`}</summary><p>{fragment.text || "未提供摘要；请从来源读取完整原文。"}</p><EvidenceList refs={fragment.source_ref ? [fragment.source_ref] : []} onOpen={onOpenEvidence} /></details>)}</> : null}
          {material.capability_mappings?.length ? <><h4>能力映射</h4><div className={styles.cardGrid}>{material.capability_mappings.map((mapping, index) => <div className={styles.card} key={index}><div className={styles.metaLine}><strong>{mapping.required || "能力"}</strong><Badge tone={mapping.status === "capability_unavailable" ? "danger" : "positive"}>{mapping.status || "available"}</Badge></div><p>{mapping.capability || "未映射"} · {mapping.version || "版本未知"}</p>{mapping.reason ? <p>{mapping.reason}</p> : null}</div>)}</div></> : null}
          <RawDetails value={material} label="材料版本与权限详情" />
        </article>)}</div>}
      </Section>
    </> : null}

    {tab === "api" ? <>
      <Section title="OpenAPI 声明" count={research.openapi?.length || 0}>
        {!research.openapi?.length ? <Empty>尚未导入 OpenAPI/Swagger 原件。</Empty> : <div className={styles.stack}>{research.openapi.map(document => <article className={styles.card} key={document.document_id}>
          <div className={styles.metaLine}><strong>{document.document_id}</strong><Badge>{document.version || "版本未知"}</Badge><code>{document.digest?.slice(0, 16)}…</code></div>
          <EvidenceList refs={document.artifact ? [document.artifact] : []} onOpen={onOpenEvidence} empty="原始文档不可读" />
          {document.unresolved_refs?.length ? <Notice tone="warning">未解析 $ref：{document.unresolved_refs.join("、")}</Notice> : null}
          {document.errors?.length ? <><h4>解析错误</h4><ul className={styles.list}>{document.errors.map((error, index) => <li key={index}>{error.location || "未知位置"}：{error.message || "未知错误"}</li>)}</ul></> : null}
          {document.operations?.length ? <div className={styles.stack}>{document.operations.map((operation, index) => <div className={styles.card} key={operation.operation_id || index}>
            <div className={styles.metaLine}><Badge>{operation.method || "?"}</Badge><code>{operation.path || "—"}</code><span>{operation.server || "—"}</span></div>
            {operation.parameters?.length ? <ul className={styles.list}>{operation.parameters.map((parameter, parameterIndex) => <li key={parameterIndex}>{parameter.original_name || parameter.name} · {parameter.in || "位置未知"}{parameter.required ? " · 必填" : ""}</li>)}</ul> : <span className={styles.muted}>无声明参数</span>}
            {operation.security?.length ? <p>认证：{operation.security.join("、")}</p> : null}
            {operation.schema_refs?.length ? <p>Schema/$ref：{operation.schema_refs.join("、")}</p> : null}
            <div className={styles.metaLine}><span>{operation.source_location || "无源码定位"}</span></div>
            <ExchangeLinks ids={operation.runtime_exchange_ids} onOpen={onOpenExchange} />
          </div>)}</div> : <Empty>没有解析出操作；请查看原文件和错误位置。</Empty>}
          <RawDetails value={document.security_schemes || []} label="Security schemes" />
        </article>)}</div>}
      </Section>
      <Section title="文档与真实请求对照" count={apiComparisons.length}>
        <Notice>新增、缺失或不一致只用于选择研究问题，不自动证明漏洞。</Notice>
        {!apiComparisons.length ? <Empty>尚无声明与运行请求差异。</Empty> : <div className={styles.stack}>{apiComparisons.map((comparison, index) => <article className={styles.card} key={comparison.comparison_id || index}>
          <div className={styles.metaLine}><Badge tone={comparison.kind === "inconsistent" ? "warning" : "accent"}>{comparison.kind || "inconsistent"}</Badge><strong>{comparison.method || ""} {comparison.path || ""}</strong><span>{comparison.field || ""}</span></div>
          <div className={styles.diffGrid}><div className={styles.diffBefore}><h4>文档声明</h4><pre className={styles.codeBlock}>{JSON.stringify(comparison.declared ?? "—", null, 2)}</pre></div><div className={styles.diffAfter}><h4>实际观察</h4><pre className={styles.codeBlock}>{JSON.stringify(comparison.observed ?? "—", null, 2)}</pre></div></div>
          <div className={styles.metaLine}><span>{comparison.environment || "环境未知"} · {formatDate(comparison.observed_at)}</span></div><ExchangeLinks ids={comparison.exchange_ids} onOpen={onOpenExchange} />
        </article>)}</div>}
      </Section>
    </> : null}

    {tab === "runtime" ? <>
      <Section title="客户端 source-to-sink" count={research.source_to_sink?.length || 0}>
        {!research.source_to_sink?.length ? <Empty>尚无客户端数据流记录。字符串或 sink 命中不等于真实执行可达。</Empty> : <div className={styles.stack}>{research.source_to_sink.map(flow => <article className={styles.card} key={flow.flow_id}>
          <div className={styles.metaLine}><strong>{flow.source || "未知 source"} → {flow.sink || "未知 sink"}</strong><Badge tone={flow.runtime_evidence_refs?.length ? "positive" : "warning"}>{flow.status || (flow.runtime_evidence_refs?.length ? "runtime" : "static")}</Badge></div>
          <KeyValues items={[["文件/模块", [flow.file, flow.module, flow.function].filter(Boolean).join(" · ") || "—"], ["定位", flow.location || "—"], ["执行上下文", flow.execution_context || "—"], ["所需交互", flow.required_interaction || "—"], ["frame/origin", flow.frame_origin || "—"], ["CSP/沙箱", flow.csp || "—"]]} />
          {flow.transformations?.length ? <p>转换/编码：{flow.transformations.join(" → ")}</p> : null}
          {flow.propagation?.length ? <p>传播路径：{flow.propagation.join(" → ")}</p> : null}
          <h4>静态定位证据</h4><EvidenceList refs={flow.static_evidence_refs} onOpen={onOpenEvidence} empty="无静态原件" />
          <h4>实际运行验证</h4><EvidenceList refs={flow.runtime_evidence_refs} onOpen={onOpenEvidence} empty="尚无运行验证；不得将静态候选当作已成立" />
        </article>)}</div>}
      </Section>
      <Section title="页面动作 → 参数与实际请求" count={research.runtime_actions?.length || 0}>
        {!research.runtime_actions?.length ? <Empty>尚无页面/客户端动作记录。</Empty> : <div className={styles.stack}>{research.runtime_actions.map(action => <article className={styles.card} key={action.action_id}>
          <div className={styles.metaLine}><strong>{action.label || action.action_id}</strong><Badge>{action.method || "ACTION"}</Badge><code>{action.path || ""}</code></div>
          {action.parameters?.length ? <ul className={styles.list}>{action.parameters.map((parameter, index) => <li key={index}>{parameter.original_name || parameter.name} · {parameter.in || "位置未知"}</li>)}</ul> : <Empty>无参数实例</Empty>}
          <KeyValues items={[["身份/租户", `${action.identity_ref || "未验证"} / ${action.tenant || "未知"}`], ["对象", action.object_ref || "—"], ["前置状态", action.before_state || "—"], ["后续状态", action.after_state || "—"], ["浏览器 generation", action.browser_generation ?? "—"]]} />
          <ExchangeLinks ids={action.exchange_ids} onOpen={onOpenExchange} /><EvidenceList refs={action.evidence_refs} onOpen={onOpenEvidence} />
        </article>)}</div>}
      </Section>
    </> : null}

    {tab === "business" ? <>
      <Section title="业务权限模型">
        {!research.business ? <Empty>尚无业务模型；不机械要求固定账号数或角色×端点覆盖。</Empty> : <>
          <KeyValues items={[["角色", research.business.roles?.join("、") || "—"], ["账号", research.business.accounts?.join("、") || "—"], ["租户", research.business.tenants?.join("、") || "—"], ["对象类型", research.business.object_types?.join("、") || "—"], ["状态迁移", research.business.state_transitions?.join("；") || "—"], ["敏感动作", research.business.sensitive_actions?.join("、") || "—"]]} />
          {research.business.constraints?.length ? <><h3>预期权限约束</h3><div className={styles.stack}>{research.business.constraints.map((constraint, index) => <article className={styles.card} key={index}><div className={styles.metaLine}><Badge>{constraint.origin || "来源未知"}</Badge><p>{constraint.text || "—"}</p></div><EvidenceList refs={constraint.evidence_refs} onOpen={onOpenEvidence} /></article>)}</div></> : null}
        </>}
      </Section>
      <Section title="可解释的权限疑问" count={research.business?.questions?.length || 0}>
        {!research.business?.questions?.length ? <Empty>尚无权限研究问题。接口可达、HTTP 200 或按钮隐藏不单独证明权限问题。</Empty> : <div className={styles.stack}>{research.business.questions.map(question => <article className={styles.card} key={question.question_id}>
          <div className={styles.metaLine}><strong>{question.subject || "未知主体"} → {question.action || "未知动作"} → {question.object || "未知对象"}</strong><Badge tone={question.status === "verified" ? "positive" : "warning"}>{question.status || "待研究"}</Badge></div>
          <KeyValues items={[["对象状态", question.state || "—"], ["预期边界", question.expected_boundary || "—"], ["来源", question.origin || "—"], ["缺失对照", question.missing?.join("；") || "—"]]} />
          <EvidenceList refs={question.actual_evidence_refs} onOpen={onOpenEvidence} empty="尚无实际服务端效果证据" />
        </article>)}</div>}
      </Section>
    </> : null}

    {tab === "cases" ? <Section title="案例与误报反例" count={research.cases?.length || 0}>
      <Notice>案例只提供候选研究方法，不生成本目标 Fact、自动定级或替代当前验证。</Notice>
      {!research.cases?.length ? <Empty>尚未发布可复用案例或反例。</Empty> : <div className={styles.stack}>{research.cases.map(item => <article className={styles.card} key={item.case_id}>
        <div className={styles.metaLine}><Badge tone={item.kind === "counterexample" ? "warning" : "accent"}>{item.kind === "counterexample" ? "反例" : "案例"}</Badge><strong>{item.title}</strong>{item.readable === false ? <Badge tone="danger">不可读</Badge> : null}{item.stale_reason ? <Badge tone="warning">已失效/陈旧</Badge> : null}</div>
        <KeyValues items={[["问题模式", item.problem_pattern || "—"], ["匹配理由", item.match_reason || "—"], ["真实成立原因", item.root_cause || "—"], ["有效区分实验", item.effective_experiment || "—"], ["修复边界", item.repair_boundary || "—"], ["来源报告/版本", `${item.source_report_id || "—"}${item.version ? ` · v${item.version}` : ""}`]]} />
        {item.prerequisites?.length ? <><h4>业务前提</h4><ul className={styles.list}>{item.prerequisites.map(value => <li key={value}>{value}</li>)}</ul></> : null}
        {item.invalid_conditions?.length ? <><h4>失效条件</h4><ul className={styles.list}>{item.invalid_conditions.map(value => <li key={value}>{value}</li>)}</ul></> : null}
        {item.stale_reason ? <Notice tone="warning">{item.stale_reason}</Notice> : null}
        <h4>决定性证据</h4><EvidenceList refs={item.decisive_evidence_refs} onOpen={onOpenEvidence} />
        <h4>排除反例的证据</h4><EvidenceList refs={item.exclusion_evidence_refs} onOpen={onOpenEvidence} empty="未提供反证" />
        <RawDetails value={item} label="案例完整来源与适用条件" />
      </article>)}</div>}
    </Section> : null}
  </ReviewShell></div>;
}

export default ResearchInspector;
