"use client";

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Button } from "@heroui/react";
import { srcFetch as apiFetch } from "@/lib/srcFetch";
import { Icon } from "@/components/Icon";
import { RequestEditor } from "./RequestEditor";
import { RequestCompare } from "./RequestCompare";
import { IdentityObjectPanel, ProtocolEvidencePanel } from "./RequestProtocolPanels";
import type {
  ApprovalRecord, ComparisonResult, ExchangePage, ExecutionReceipt, HttpExchange,
  IdentityContext, OastInteraction, ProtocolStream, RequestDraft, TestObject,
  WorkspaceEndpoints,
} from "./RequestWorkspaceTypes";
import { byteText, errorMessage, receiptOutput, textBase64 } from "./RequestWorkspaceTypes";

class WorkspaceError extends Error {
  status: number;
  code: string;
  payload: unknown;
  constructor(message: string, status = 0, code = "", payload: unknown = null) {
    super(message); this.status = status; this.code = code; this.payload = payload;
  }
}

type Tab = "requests" | "compare" | "identity" | "protocol";
type Notice = { tone: "info" | "warning" | "danger"; text: string } | null;

function normalizeTestObject(value: TestObject & { record_id?: string; after_state?: Record<string, unknown> }): TestObject {
  const restoration = value.restoration;
  return {
    ...value,
    experiment_id: value.experiment_id || value.record_id,
    current_state: value.current_state || value.after_state,
    restoration: restoration && typeof restoration === "object" ? restoration.status : restoration,
    restoration_detail: restoration && typeof restoration === "object" ? restoration : undefined,
    reason: value.reason || (restoration && typeof restoration === "object" ? restoration.reason : ""),
  };
}

export interface RequestWorkspaceProps {
  taskId: string;
  initialExchangeId?: string;
  onExchangeChange?: (exchangeId: string) => void;
  endpoints?: WorkspaceEndpoints;
  className?: string;
}

export function RequestWorkspace({ taskId, initialExchangeId = "", onExchangeChange, endpoints = {}, className = "" }: RequestWorkspaceProps) {
  const paths = useMemo(() => ({
    exchanges: endpoints.exchanges || `/api/src/tasks/${encodeURIComponent(taskId)}/exchanges`,
    identities: endpoints.identities || `/api/src/tasks/${encodeURIComponent(taskId)}/identities`,
    objects: endpoints.objects || `/api/src/tasks/${encodeURIComponent(taskId)}/objects`,
    streams: endpoints.streams || `/api/src/tasks/${encodeURIComponent(taskId)}/protocol-streams`,
    oast: endpoints.oast || `/api/src/tasks/${encodeURIComponent(taskId)}/oast/interactions`,
    artifacts: endpoints.artifacts || `/api/src/tasks/${encodeURIComponent(taskId)}/artifacts`,
  }), [taskId, endpoints]);
  const [tab, setTab] = useState<Tab>("requests");
  const [items, setItems] = useState<HttpExchange[]>([]);
  const [page, setPage] = useState<ExchangePage>({ items: [] });
  const [selectedId, setSelectedId] = useState(initialExchangeId);
  const [selected, setSelected] = useState<HttpExchange | null>(null);
  const [draft, setDraft] = useState<RequestDraft | null>(null);
  const [rawRequest, setRawRequest] = useState("");
  const [rawResponse, setRawResponse] = useState("");
  const [dirty, setDirty] = useState(false);
  const [conflict, setConflict] = useState(false);
  const [approval, setApproval] = useState<ApprovalRecord | null>(null);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);
  const [busyId, setBusyId] = useState("");
  const [error, setError] = useState("");
  const [notice, setNotice] = useState<Notice>(null);
  const [filters, setFilters] = useState({ method: "", source: "", context: "", text: "" });
  const [cursor, setCursor] = useState("");
  const [baselineId, setBaselineId] = useState("");
  const [experimentId, setExperimentId] = useState("");
  const [comparison, setComparison] = useState<ComparisonResult | null>(null);
  const [identities, setIdentities] = useState<IdentityContext[]>([]);
  const [objects, setObjects] = useState<TestObject[]>([]);
  const [streams, setStreams] = useState<ProtocolStream[]>([]);
  const [interactions, setInteractions] = useState<OastInteraction[]>([]);
  const idempotencyRef = useRef("");
  const loadSequence = useRef(0);

  const json = useCallback(async <T,>(path: string, init?: RequestInit): Promise<T> => {
    const response = await apiFetch(path, init);
    const text = await response.text();
    let body: unknown = null;
    if (text) {
      try { body = JSON.parse(text); } catch { body = text; }
    }
    if (!response.ok) {
      const message = errorMessage(body, `请求失败（HTTP ${response.status}）`);
      const code = body && typeof body === "object" && "error" in body
        ? String((body as { error?: { code?: string } }).error?.code || "") : "";
      throw new WorkspaceError(message, response.status, code, body);
    }
    return body as T;
  }, []);
  const command = useCallback(async <T,>(name: string, payload: Record<string, unknown>): Promise<T> => {
    const body = await json<unknown>(`${paths.exchanges.split("/exchanges")[0]}/commands/${name}`, {
      method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(payload),
    });
    if (body && typeof body === "object" && "error" in body && (body as { error?: unknown }).error) {
      throw new WorkspaceError(errorMessage(body, "命令执行失败"), 409, String((body as { error?: { code?: string } }).error?.code || ""), body);
    }
    return receiptOutput<T>(body);
  }, [json, paths.exchanges]);
  const query = useCallback(async <T,>(name: string, payload: Record<string, unknown>): Promise<T> => {
    return json<T>(`${paths.exchanges.split("/exchanges")[0]}/queries/${name}`, {
      method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(payload),
    });
  }, [json, paths.exchanges]);
  const selectExchange = useCallback((exchangeId: string) => {
    setSelectedId(exchangeId);
    onExchangeChange?.(exchangeId);
  }, [onExchangeChange]);

  const loadList = useCallback(async () => {
    setError("");
    try {
      const result = await query<ExchangePage | HttpExchange[]>("src.exchange.query", {
        filters: {
          ...(filters.method ? { method: filters.method } : {}),
          ...(filters.source ? { source_kind: filters.source } : {}),
          ...(filters.context ? { context_id: filters.context } : {}),
          ...(filters.text ? { body_text: filters.text } : {}),
        }, cursor, limit: 50,
      });
      const normalized: ExchangePage = Array.isArray(result) ? { items: result } : result;
      const rows = (normalized.items || []).map(row => {
        const record = row as HttpExchange & { payload?: HttpExchange };
        return record.payload ? { ...record.payload, revision: record.revision } : record;
      });
      setItems(rows); setPage(normalized);
      if (!selectedId && rows[0]) selectExchange(rows[0].exchange_id);
      if (selectedId && rows.length && !rows.some(row => row.exchange_id === selectedId)) {
        setNotice({ tone: "info", text: "当前选中请求不在此筛选页，已保留其完整证据与深链。" });
      }
      if (!baselineId && rows[0]) setBaselineId(rows[0].exchange_id);
      if (!experimentId && rows[1]) setExperimentId(rows[1].exchange_id);
    } catch (cause) { setError(errorMessage(cause, "请求索引读取失败")); }
    finally { setLoading(false); }
  }, [baselineId, cursor, experimentId, filters, query, selectExchange, selectedId]);

  const loadSelected = useCallback(async () => {
    if (!selectedId) { setSelected(null); setDraft(null); setRawRequest(""); setRawResponse(""); return; }
    const sequence = ++loadSequence.current;
    setBusy(true); setError(""); setApproval(null);
    try {
      const body = await query<{
        exchange: HttpExchange; record_revision: number; request_raw?: unknown; response_raw?: unknown;
        approval?: ApprovalRecord;
      }>("src.exchange.read", { exchange_id: selectedId });
      const exchange = body.exchange;
      const draftRows = await query<Array<{ payload: RequestDraft; revision: number }>>("src.research.records", { kind: "request_draft", limit: 500 });
      const draftRow = draftRows.filter(row => row.payload.source_exchange_id === selectedId)
        .sort((a, b) => b.payload.revision - a.payload.revision)[0];
      let currentDraft = draftRow?.payload || null;
      let draftRaw = "";
      if (currentDraft) {
        const draftBody = await query<{ draft: RequestDraft; raw_request?: unknown }>("src.exchange.draft.read", { draft_id: currentDraft.draft_id });
        currentDraft = draftBody.draft;
        draftRaw = byteText(draftBody.raw_request);
      }
      const requestText = draftRaw || byteText(body.request_raw);
      const responseText = byteText(body.response_raw);
      let approval = body.approval || null;
      if (!approval && exchange.decision_ref) {
        const approvalPage = await query<{ records?: Array<{ payload: ApprovalRecord }> }>("src.record.list", { kind: "execution_approval", limit: 500 });
        approval = (approvalPage.records || []).find(row => row.payload.invocation_id === exchange.decision_ref)?.payload || null;
      }
      if (sequence !== loadSequence.current) return;
      setSelected({ ...exchange, revision: body.record_revision }); setDraft(currentDraft); setRawRequest(requestText); setRawResponse(responseText);
      setDirty(false); setConflict(false); idempotencyRef.current = "";
      setApproval(approval);
    } catch (cause) { setError(errorMessage(cause, "请求证据读取失败")); }
    finally { if (sequence === loadSequence.current) setBusy(false); }
  }, [query, selectedId]);

  useEffect(() => { setLoading(true); setCursor(""); setSelectedId(initialExchangeId); }, [taskId, initialExchangeId]);
  useEffect(() => { void loadList(); }, [loadList]);
  useEffect(() => { void loadSelected(); }, [loadSelected]);
  useEffect(() => {
    if (tab === "compare" || tab === "requests") return;
    let active = true;
    const load = async () => {
      try {
        if (tab === "identity") {
          const [identityRows, objectRows] = await Promise.all([
            query<Array<{ payload: IdentityContext }>>("src.research.records", { kind: "browser_identity", limit: 500 }),
            query<Array<{ payload: TestObject }>>("src.research.records", { kind: "object_experiment", limit: 500 }),
          ]);
          if (active) {
            setIdentities(identityRows.map(row => ({ ...row.payload, identity_revision: row.payload.identity_revision || 1 })));
            setObjects(objectRows.map(row => normalizeTestObject(row.payload)));
          }
        } else {
          const [streamRows, interactionRows] = await Promise.all([
            query<Array<{ payload: ProtocolStream }>>("src.research.records", { kind: "protocol_stream", limit: 500 }),
            query<Array<{ payload: OastInteraction }>>("src.research.records", { kind: "oast_interaction", limit: 500 }),
          ]);
          const enriched = await Promise.all(interactionRows.map(async row => {
            const events = await query<{ items?: OastInteraction["events"] }>("src.oast.events", { interaction_id: row.payload.interaction_id, limit: 500 });
            return { ...row.payload, events: events.items || [] };
          }));
          if (active) { setStreams(streamRows.map(row => row.payload)); setInteractions(enriched); }
        }
      } catch (cause) { if (active) setError(errorMessage(cause, "身份或协议证据读取失败")); }
    };
    void load();
    return () => { active = false; };
  }, [query, tab]);

  const saveDraft = async () => {
    if (!selected || !rawRequest) return;
    setBusy(true); setNotice(null);
    if (!idempotencyRef.current) idempotencyRef.current = crypto.randomUUID();
    try {
      const task = await query<{ binding: { principal_id: string } }>("src.task.get", {});
      const output = await command<RequestDraft | { draft: RequestDraft }>("src.exchange.draft.save", {
        source_exchange_id: selected.exchange_id,
        base_revision: draft?.revision ?? selected.revision ?? 1,
        raw_request_b64: textBase64(rawRequest), editor: task.binding.principal_id,
        idempotency_key: idempotencyRef.current,
      });
      const saved = "draft" in output ? output.draft : output;
      setDraft(saved); setRawRequest(saved.raw_request || rawRequest); setDirty(false); setConflict(false);
      idempotencyRef.current = "";
      setNotice({ tone: "info", text: `草稿已保存为 v${saved.revision}；尚未发送网络请求。` });
    } catch (cause) {
      if (cause instanceof WorkspaceError && cause.code === "version_conflict") {
        setConflict(true); setNotice({ tone: "danger", text: "草稿版本冲突，未覆盖服务端 revision。" });
      } else setNotice({ tone: "danger", text: errorMessage(cause, "草稿保存失败") });
    } finally { setBusy(false); }
  };

  const executeDraft = async () => {
    if (!selected || dirty || conflict) return;
    setBusy(true); setNotice(null);
    if (!idempotencyRef.current) idempotencyRef.current = crypto.randomUUID();
    try {
      let activeDraft = draft;
      if (!activeDraft) {
        const task = await query<{ binding: { principal_id: string } }>("src.task.get", {});
        const savedOut = await command<RequestDraft | { draft: RequestDraft }>("src.exchange.draft.save", {
          source_exchange_id: selected.exchange_id,
          base_revision: selected.revision ?? 1,
          raw_request_b64: textBase64(rawRequest), editor: task.binding.principal_id,
          idempotency_key: `init-${idempotencyRef.current}`,
        });
        activeDraft = "draft" in savedOut ? savedOut.draft : savedOut;
        setDraft(activeDraft);
      }
      const policy = await query<{ version: number }>("src.policy.current", {});
      const output = await command<{
        exchange?: HttpExchange; exchange_id?: string; receipt?: ExecutionReceipt;
        execution_state?: string; approval?: ApprovalRecord;
      }>("src.exchange.draft.execute", {
        draft_id: activeDraft.draft_id, draft_revision: activeDraft.revision,
        identity_ref: selected.context_id, credential_ref: String(selected.metadata?.credential_ref || ""),
        context_id: selected.context_id, identity_revision: selected.identity_revision,
        browser_generation: selected.browser_generation,
        source_run_id: selected.source_run_id || undefined, actor_ref: "human",
        expected_side_effect: "由当前请求与业务对象决定",
        authorization_version: policy.version,
        idempotency_key: idempotencyRef.current,
        budget: { max_requests: 1 }, wait_for_approval: true,
      });
      idempotencyRef.current = "";
      const nextId = output.exchange_id || output.exchange?.exchange_id;
      if (nextId) selectExchange(nextId);
      await Promise.all([loadList(), loadSelected()]);
      if (output.approval) setApproval(output.approval);
      const state = output.execution_state || output.receipt?.dispatch_state || output.exchange?.execution_state;
      setNotice({
        tone: state === "outcome_unknown" || state === "waiting_approval" ? "warning" : "info",
        text: state === "outcome_unknown" ? "执行结果未知，未自动重发；请先核对服务端状态。" : state === "waiting_approval" ? "请求尚未发送，请在原请求卡片完成审批。" : `执行状态：${state || "已提交"}`,
      });
    } catch (cause) { setNotice({ tone: "danger", text: errorMessage(cause, "请求执行失败") }); }
    finally { setBusy(false); }
  };

  const decideApproval = async (decision: "approve_once" | "deny" | "approve_task_scope") => {
    if (!approval) return;
    setBusy(true);
    try {
      const snapshotTarget = String(approval.request_snapshot?.arguments?.url || selected?.observed_url || selected?.declared_target || "");
      const snapshotIdentity = String(approval.request_snapshot?.identity_ref || selected?.context_id || "");
      await command("src.approval.decide", {
        approval_id: approval.approval_id, decision, base_revision: approval.revision,
        scope: decision === "approve_task_scope" ? {
          resource_selector: { url: { target: snapshotTarget } },
          actions: ["request"], identity_constraints: { identity_ref: snapshotIdentity },
        } : undefined,
      });
      setApproval(null);
      await Promise.all([loadSelected(), loadList()]);
      setNotice({ tone: "info", text: decision === "deny" ? "已拒绝；该请求不会下发。" : "审批已记录，正在读取原调用的真实执行回执。" });
    } catch (cause) { setNotice({ tone: "danger", text: errorMessage(cause, "审批决定失败") }); }
    finally { setBusy(false); }
  };

  const compare = async (ignoreRules: Record<string, unknown>) => {
    setBusy(true); setComparison(null);
    try {
      const result = await query<ComparisonResult>("src.exchange.compare", {
        baseline_id: baselineId, experiment_id: experimentId,
        ignore_rules: ignoreRules,
      });
      setComparison(result);
    } catch (cause) { setNotice({ tone: "danger", text: errorMessage(cause, "对照失败") }); }
    finally { setBusy(false); }
  };

  const verifyIdentity = async (identity: IdentityContext, expected: Record<string, unknown>) => {
    setBusyId(identity.context_id);
    try {
      const result = await command<IdentityContext>("src.identity.verify", {
        context_id: identity.context_id, condition_version: identity.identity_revision || 0,
        browser_generation: identity.browser_generation, expected,
        observed: identity.observed || {},
        evidence_refs: (identity.evidence_refs || []).map(id => ({ artifact_id: id })),
      });
      setIdentities(current => current.map(row => row.context_id === identity.context_id ? { ...row, ...result } : row));
      setNotice({ tone: result.status === "verified" ? "info" : "warning", text: `身份 ${identity.context_id}：${result.status || "identity_unverified"}` });
    } catch (cause) { setNotice({ tone: "danger", text: errorMessage(cause, "身份核验失败") }); }
    finally { setBusyId(""); }
  };

  const restoreObject = async (object: TestObject) => {
    setBusyId(object.object_id);
    try {
      const result = await command<TestObject>("src.object.experiment.restore", {
        experiment_id: object.experiment_id, status: object.restoration || "unknown",
        readback: object.current_state || {}, fencing_token: Number(object.lease?.fencing_token || 0),
        residual_objects: object.residual_objects || [], unrestored_fields: object.unrestored_fields || [],
        reason: object.reason || "恢复读回尚未确认最终状态", evidence_refs: [],
      });
      const normalized = normalizeTestObject(result);
      setObjects(current => current.map(row => row.object_id === object.object_id ? normalized : row));
      const restorationStatus = normalized.restoration_detail?.status || normalized.restoration || "unknown";
      setNotice({ tone: restorationStatus === "restored" ? "info" : "warning", text: `恢复结果：${restorationStatus}${normalized.reason ? ` · ${normalized.reason}` : ""}` });
    } catch (cause) { setNotice({ tone: "danger", text: errorMessage(cause, "对象恢复失败") }); }
    finally { setBusyId(""); }
  };

  const history = useMemo(() => selected ? items.filter(row => row.replay_of === selected.exchange_id || row.exchange_id === selected.replay_of) : [], [items, selected]);
  const tabs: Array<[Tab, string]> = [["requests", "请求"], ["compare", "基线对照"], ["identity", "身份与对象"], ["protocol", "WS / SSE / OAST"]];
  return <div className={`grid min-w-0 gap-4 ${className}`} aria-label="SRC 请求工作区">
    <div className="flex flex-wrap items-center gap-1" role="tablist" aria-label="请求工作区视图">
      {tabs.map(([id, label]) => <Button key={id} size="sm" variant={tab === id ? "secondary" : "ghost"} onPress={() => setTab(id)} aria-pressed={tab === id}>{label}</Button>)}
      <span className="flex-1" />
      {page.snapshot ? <span className="text-xs text-cx-fg-3">snapshot {page.snapshot}</span> : null}
      <Button size="sm" variant="ghost" onPress={() => { void loadList(); if (selectedId) void loadSelected(); }} aria-label="刷新请求证据"><Icon name="refresh" size={14} />刷新</Button>
    </div>
    {error ? <div className="workspace-home-error" role="alert"><span>{error}</span><Button size="sm" variant="ghost" onPress={() => { setError(""); void loadList(); }}>重试</Button></div> : null}
    {notice ? <p className={`pentest-notice pentest-notice--${notice.tone === "danger" ? "danger" : notice.tone === "warning" ? "warning" : "info"}`} role="status">{notice.text}</p> : null}
    {page.partial_reason || page.complete === false ? <p className="pentest-notice pentest-notice--warning" role="status"><Icon name="alert" size={14} />当前索引或分页不完整：{page.partial_reason || "来源水位落后，不能把当前结果当作全部。"}</p> : null}
    {page.watermarks?.length ? <details className="text-xs text-cx-fg-3"><summary>来源水位与可达性（{page.watermarks.length}）</summary><div className="mt-2 grid gap-1">{page.watermarks.map((row, index) => <code key={index} className="break-all">{JSON.stringify(row)}</code>)}</div></details> : null}
    {tab === "requests" ? <div className="grid min-w-0 gap-4 xl:grid-cols-[310px_1fr]">
      <aside className="min-w-0 grid content-start gap-3" aria-label="请求证据列表">
        <form className="grid gap-2" onSubmit={event => { event.preventDefault(); setCursor(""); void loadList(); }}>
          <div className="grid grid-cols-2 gap-2">
            <input aria-label="HTTP 方法" placeholder="方法" value={filters.method} onChange={event => setFilters(current => ({ ...current, method: event.target.value.toUpperCase() }))} className="rounded-md border border-cx-line bg-cx-bg px-2 py-2 text-xs" />
            <select aria-label="来源" value={filters.source} onChange={event => setFilters(current => ({ ...current, source: event.target.value }))} className="rounded-md border border-cx-line bg-cx-bg px-2 py-2 text-xs"><option value="">全部来源</option><option value="http_tool">HTTP 工具</option><option value="browser">浏览器</option><option value="proxy">代理</option><option value="import">导入</option></select>
          </div>
          <input aria-label="身份" placeholder="身份 context" value={filters.context} onChange={event => setFilters(current => ({ ...current, context: event.target.value }))} className="rounded-md border border-cx-line bg-cx-bg px-2 py-2 text-xs" />
          <div className="flex gap-2"><input aria-label="检索请求" placeholder="URL / path / body" value={filters.text} onChange={event => setFilters(current => ({ ...current, text: event.target.value }))} className="min-w-0 flex-1 rounded-md border border-cx-line bg-cx-bg px-2 py-2 text-xs" /><Button size="sm" variant="secondary" type="submit"><Icon name="search" size={13} />检索</Button></div>
        </form>
        <div className="grid max-h-[680px] gap-1 overflow-auto" aria-busy={loading}>
          {items.map(row => <button type="button" key={row.exchange_id} onClick={() => selectExchange(row.exchange_id)} className={`rounded-md border p-2 text-left ${selectedId === row.exchange_id ? "border-cx-accent" : "border-cx-line"}`}>
            <span className="flex items-center gap-2"><strong className="text-xs">{row.method}</strong><span className="text-xs text-cx-fg-3">{row.status_code ?? "—"}</span><span className="ml-auto text-[11px] text-cx-fg-3">{row.capture_state}</span></span>
            <span className="mt-1 block break-all text-xs text-cx-fg-2">{row.observed_url || row.declared_target || "observed_target=unknown"}</span>
            <span className="mt-1 block text-[11px] text-cx-fg-3">{row.context_id} · {row.source_kind} · {new Date(row.started_at).toLocaleString()}</span>
          </button>)}
          {!loading && !items.length ? <div className="workspace-home-empty">当前筛选没有请求证据；索引失败与空结果会分别显示。</div> : null}
          {loading ? <div className="workspace-home-empty" role="status">正在读取请求与来源水位…</div> : null}
        </div>
        <div className="flex items-center gap-2 text-xs"><span className="flex-1 text-cx-fg-3">{page.total ?? items.length} 条</span>{page.next_cursor ? <Button size="sm" variant="outline" onPress={() => setCursor(page.next_cursor || "")}>下一页</Button> : null}</div>
      </aside>
      {selected ? <div className="grid min-w-0 content-start gap-4">
        <RequestEditor key={selected.exchange_id} exchange={selected} draft={draft} rawRequest={rawRequest} rawResponse={rawResponse}
          onRawChange={value => { setRawRequest(value); setDirty(true); setConflict(false); idempotencyRef.current = ""; }}
          onSave={() => void saveDraft()} onExecute={() => void executeDraft()} onReload={() => void loadSelected()} onDecide={decision => void decideApproval(decision)}
          busy={busy} dirty={dirty} conflict={conflict} approval={approval} />
        <section aria-label="重放与执行历史" className="grid gap-2 border-t border-cx-line pt-3"><h3 className="text-sm font-medium">重放与执行历史</h3>
          {history.length ? history.map(row => <button type="button" key={row.exchange_id} onClick={() => selectExchange(row.exchange_id)} className="flex flex-wrap items-center gap-2 rounded-md border border-cx-line p-2 text-left text-xs"><span>{row.exchange_id}</span><span className="pentest-chip pentest-chip--neutral">{row.execution_state}</span><span className="text-cx-fg-3">replay_of {row.replay_of || "原请求"}</span></button>) : <p className="text-xs text-cx-fg-3">当前筛选页没有关联重放；原请求始终不可变，执行后才会产生新 exchange。</p>}
        </section>
      </div> : !loading ? <div className="workspace-home-empty">选择一条请求证据以读取完整原文和草稿。</div> : null}
    </div> : null}
    {tab === "compare" ? <RequestCompare exchanges={items} baselineId={baselineId} experimentId={experimentId} onBaselineChange={setBaselineId} onExperimentChange={setExperimentId} onCompare={rules => void compare(rules)} result={comparison} busy={busy} /> : null}
    {tab === "identity" ? <IdentityObjectPanel identities={identities} objects={objects} onVerify={(identity, expected) => void verifyIdentity(identity, expected)} onRestore={object => void restoreObject(object)} busyId={busyId} /> : null}
    {tab === "protocol" ? <ProtocolEvidencePanel streams={streams} interactions={interactions} /> : null}
  </div>;
}
