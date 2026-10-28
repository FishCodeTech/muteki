"use client";

import { useEffect, useMemo, useState } from "react";
import { Button } from "@heroui/react";
import { Icon } from "@/components/Icon";
import type { ApprovalRecord, HttpExchange, RequestDraft } from "./RequestWorkspaceTypes";
import { exchangeLabel } from "./RequestWorkspaceTypes";

function rawParts(raw: string) {
  const separator = raw.includes("\r\n\r\n") ? "\r\n\r\n" : "\n\n";
  const index = raw.indexOf(separator);
  const head = index >= 0 ? raw.slice(0, index) : raw;
  const body = index >= 0 ? raw.slice(index + separator.length) : "";
  const lines = head.split(/\r?\n/);
  return { requestLine: lines.shift() || "", headers: lines.join("\n"), body };
}

function headerLines(headers?: Array<[string, string]>): string {
  return (headers || []).map(([name, value]) => `${name}: ${value}`).join("\n");
}

export function RequestEditor({
  exchange,
  draft,
  rawRequest,
  rawResponse,
  onRawChange,
  onSave,
  onExecute,
  onReload,
  onDecide,
  busy,
  dirty,
  conflict,
  approval,
}: {
  exchange: HttpExchange;
  draft?: RequestDraft | null;
  rawRequest: string;
  rawResponse: string;
  onRawChange: (value: string) => void;
  onSave: () => void;
  onExecute: () => void;
  onReload: () => void;
  onDecide: (decision: "approve_once" | "deny" | "approve_task_scope") => void;
  busy: boolean;
  dirty: boolean;
  conflict: boolean;
  approval?: ApprovalRecord | null;
}) {
  const [view, setView] = useState<"raw" | "structured" | "response">("raw");
  const initial = useMemo(() => rawParts(rawRequest), [rawRequest]);
  const [method, setMethod] = useState(exchange.method);
  const [target, setTarget] = useState(exchange.observed_url || exchange.declared_target || "");
  const [headers, setHeaders] = useState(headerLines(exchange.request_headers) || initial.headers);
  const [body, setBody] = useState(initial.body);
  const [saveScope, setSaveScope] = useState(false);

  useEffect(() => {
    const parsed = rawParts(rawRequest);
    const lineParts = parsed.requestLine.split(" ");
    setMethod(lineParts[0] || exchange.method);
    setTarget(lineParts[1] || draft?.target_url || exchange.observed_url || exchange.declared_target || "");
    setHeaders(parsed.headers || headerLines(exchange.request_headers));
    setBody(parsed.body);
  }, [exchange.exchange_id, exchange.method, exchange.observed_url, exchange.declared_target, exchange.request_headers, draft?.draft_id, draft?.revision, draft?.target_url, rawRequest]);

  const applyStructured = () => {
    const normalizedHeaders = headers.split(/\r?\n/).map(line => line.trimEnd()).filter(line => line.trim());
    const requestTarget = target.trim() || "/";
    const raw = [`${method.trim().toUpperCase()} ${requestTarget} HTTP/1.1`, ...normalizedHeaders, "", body].join("\r\n");
    onRawChange(raw);
    setView("raw");
  };

  return <section aria-label="请求编辑与执行" className="flex min-w-0 flex-1 flex-col gap-3">
    <div className="flex flex-wrap items-center gap-2">
      <div className="min-w-0 flex-1">
        <h2 className="truncate text-base font-semibold" title={exchangeLabel(exchange)}>{exchangeLabel(exchange)}</h2>
        <p className="mt-1 text-xs text-cx-fg-3">
          {exchange.source_kind} · {exchange.context_id} · identity v{exchange.identity_revision} · browser g{exchange.browser_generation}
        </p>
      </div>
      <span className={`pentest-chip ${exchange.capture_state === "complete" ? "pentest-chip--positive" : "pentest-chip--warning"}`}>{exchange.capture_state}</span>
      <span className={`pentest-chip ${exchange.execution_state === "completed" ? "pentest-chip--positive" : exchange.execution_state === "outcome_unknown" ? "pentest-chip--warning" : "pentest-chip--neutral"}`}>{exchange.execution_state}</span>
    </div>
    <div className="flex flex-wrap gap-x-4 gap-y-1 text-[11px] text-cx-fg-3" aria-label="请求与响应 Artifact 完整性">
      <span>request: <code title={exchange.request_raw_ref.sha256}>{exchange.request_raw_ref.sha256}</code> · {exchange.request_raw_ref.size} bytes</span>
      <span>response: {exchange.response_raw_ref ? <><code title={exchange.response_raw_ref.sha256}>{exchange.response_raw_ref.sha256}</code> · {exchange.response_raw_ref.size} bytes</> : "unavailable"}</span>
    </div>
    {exchange.capture_limitations?.length ? <p className="pentest-notice pentest-notice--warning" role="status"><Icon name="alert" size={14} />{exchange.capture_limitations.join("；")}</p> : null}
    {conflict ? <div className="pentest-notice pentest-notice--danger" role="alert"><Icon name="circleAlert" size={14} /><span>服务端草稿已被其他人或 Agent 修改。当前编辑未覆盖服务端版本；请重新读取后再保存。</span><Button size="sm" variant="outline" onPress={onReload}>读取服务端版本</Button></div> : null}
    <div className="flex flex-wrap items-center gap-1" role="tablist" aria-label="请求视图">
      {([ ["raw", "原始请求"], ["structured", "结构化编辑"], ["response", "响应"] ] as const).map(([id, label]) => <Button key={id} size="sm" variant={view === id ? "secondary" : "ghost"} onPress={() => setView(id)} aria-pressed={view === id}>{label}</Button>)}
      <span className="flex-1" />
      <span className="text-xs text-cx-fg-3">draft v{draft?.revision || 0}{dirty ? " · 未保存" : ""}</span>
    </div>
    {view === "raw" ? <label className="grid gap-1 text-xs text-cx-fg-2">原始请求（编辑只产生新草稿，不修改原证据）
      <textarea aria-label="原始请求" value={rawRequest} onChange={event => onRawChange(event.target.value)} spellCheck={false} className="min-h-[300px] w-full resize-y rounded-md border border-cx-line bg-cx-bg p-3 font-mono text-xs leading-5 text-cx-fg" />
    </label> : null}
    {view === "structured" ? <div className="grid gap-3 rounded-md border border-cx-line p-3">
      <div className="grid gap-3 sm:grid-cols-[140px_1fr]">
        <label className="grid gap-1 text-xs">方法<input value={method} onChange={event => setMethod(event.target.value.toUpperCase())} className="rounded-md border border-cx-line bg-cx-bg px-2 py-2 font-mono" /></label>
        <label className="grid gap-1 text-xs">精确目标<input value={target} onChange={event => setTarget(event.target.value)} className="rounded-md border border-cx-line bg-cx-bg px-2 py-2 font-mono" /></label>
      </div>
      <label className="grid gap-1 text-xs">Header（每行一个，允许重复名称和原始大小写）<textarea value={headers} onChange={event => setHeaders(event.target.value)} spellCheck={false} className="min-h-[130px] rounded-md border border-cx-line bg-cx-bg p-2 font-mono text-xs" /></label>
      <label className="grid gap-1 text-xs">Body<textarea value={body} onChange={event => setBody(event.target.value)} spellCheck={false} className="min-h-[150px] rounded-md border border-cx-line bg-cx-bg p-2 font-mono text-xs" /></label>
      <div><Button size="sm" variant="secondary" onPress={applyStructured}><Icon name="check" size={13} />应用到新草稿</Button></div>
      <p className="text-xs text-cx-fg-3">结构化编辑生成派生草稿；原 exchange 和原始字节保持不变。</p>
    </div> : null}
    {view === "response" ? <div className="grid gap-2">
      <div className="flex flex-wrap gap-2 text-xs text-cx-fg-3"><span>HTTP {exchange.status_code ?? "—"}</span><span>{exchange.protocol || "协议未知"}</span><span>{exchange.response_raw_ref ? `${exchange.response_raw_ref.size} bytes` : "无响应 Artifact"}</span></div>
      <pre aria-label="原始响应" className="min-h-[260px] overflow-auto rounded-md border border-cx-line bg-cx-bg p-3 font-mono text-xs leading-5 whitespace-pre-wrap">{rawResponse || "没有可显示的响应原文；请查看捕获完整性和 Artifact 元数据。"}</pre>
    </div> : null}
    <div className="flex flex-wrap items-center gap-2 border-t border-cx-line pt-3">
      <Button size="sm" variant="secondary" isPending={busy} isDisabled={busy || !dirty} onPress={onSave}><Icon name="check" size={14} />保存新 revision</Button>
      <Button size="sm" variant="primary" isPending={busy} isDisabled={busy || dirty || conflict} onPress={onExecute}><Icon name="send" size={14} />执行此 revision</Button>
      <Button size="sm" variant="ghost" isDisabled={busy} onPress={onReload}><Icon name="refresh" size={14} />重新读取</Button>
    </div>
    {approval?.status === "pending" ? <div className="grid gap-3 rounded-md border border-cx-line p-3" role="alert">
      <div className="flex items-start gap-2"><Icon name="shield" size={17} /><div><strong>执行前需要审批</strong><p className="mt-1 text-xs text-cx-fg-3">绑定输入 {approval.input_digest} · policy v{approval.policy_version} · 至 {new Date(approval.expires_at).toLocaleString()}</p></div></div>
      <dl className="grid gap-1 text-xs"><dt>命中规则</dt><dd>{approval.matched_rule_refs?.join("、") || "新增主动测试目标"}</dd><dt>身份</dt><dd>{approval.request_snapshot?.identity_ref || exchange.context_id}</dd></dl>
      <label className="flex items-start gap-2 text-xs"><input type="checkbox" checked={saveScope} onChange={event => setSaveScope(event.target.checked)} /><span>同时保存为本 Task 范围授权，仅限当前目标、动作和身份；不是全域名或全部脚本通行。</span></label>
      <div className="flex flex-wrap gap-2"><Button size="sm" variant="primary" onPress={() => onDecide(saveScope ? "approve_task_scope" : "approve_once")}>{saveScope ? "批准并保存范围" : "仅批准此次"}</Button><Button size="sm" variant="outline" onPress={() => onDecide("deny")}>拒绝</Button></div>
    </div> : null}
    {exchange.execution_state === "outcome_unknown" ? <p className="pentest-notice pentest-notice--warning" role="status"><Icon name="alert" size={14} />请求可能已产生副作用，最终结果未知。系统不会自动重发；请先核对服务端状态或只读回查。</p> : null}
  </section>;
}
