"use client";

import { useState } from "react";
import { Button } from "@heroui/react";
import { Icon } from "@/components/Icon";
import type { ComparisonResult, HttpExchange } from "./RequestWorkspaceTypes";
import { exchangeLabel } from "./RequestWorkspaceTypes";

function pretty(value: unknown): string {
  if (value == null) return "尚无差异结果。";
  return typeof value === "string" ? value : JSON.stringify(value, null, 2);
}

export function RequestCompare({
  exchanges,
  baselineId,
  experimentId,
  onBaselineChange,
  onExperimentChange,
  onCompare,
  result,
  busy,
}: {
  exchanges: HttpExchange[];
  baselineId: string;
  experimentId: string;
  onBaselineChange: (value: string) => void;
  onExperimentChange: (value: string) => void;
  onCompare: (ignoreRules: Record<string, unknown>) => void;
  result?: ComparisonResult | null;
  busy: boolean;
}) {
  const [timestamps, setTimestamps] = useState(false);
  const [nonces, setNonces] = useState(false);
  const [view, setView] = useState<"raw" | "normalized">("normalized");
  const baseline = exchanges.find(row => row.exchange_id === baselineId);
  const experiment = exchanges.find(row => row.exchange_id === experimentId);
  return <section aria-label="基线与实验对照" className="grid gap-4">
    <div className="grid gap-3 lg:grid-cols-2">
      {([
        ["基线", baselineId, onBaselineChange, baseline],
        ["实验", experimentId, onExperimentChange, experiment],
      ] as const).map(([label, value, onChange, selected]) => <div key={label} className="grid min-w-0 gap-2 rounded-md border border-cx-line p-3">
        <label className="grid min-w-0 gap-1 text-xs font-medium">{label}<select value={value} onChange={event => onChange(event.target.value)} className="min-w-0 rounded-md border border-cx-line bg-cx-bg px-2 py-2">
          <option value="">选择 exchange</option>
          {exchanges.map(row => <option value={row.exchange_id} key={row.exchange_id}>{exchangeLabel(row)} · {row.exchange_id}</option>)}
        </select></label>
        {selected ? <dl className="grid grid-cols-[90px_1fr] gap-1 text-xs text-cx-fg-3"><dt>身份</dt><dd>{selected.context_id} · v{selected.identity_revision}</dd><dt>目标</dt><dd className="break-all">{selected.observed_url || selected.declared_target}</dd><dt>时间</dt><dd>{new Date(selected.started_at).toLocaleString()}</dd><dt>来源</dt><dd>{selected.source_run_id || selected.actor_ref}</dd></dl> : null}
      </div>)}
    </div>
    <div className="flex flex-wrap items-center gap-3 text-xs">
      <label className="flex items-center gap-2"><input type="checkbox" checked={timestamps} onChange={event => setTimestamps(event.target.checked)} />忽略时间戳</label>
      <label className="flex items-center gap-2"><input type="checkbox" checked={nonces} onChange={event => setNonces(event.target.checked)} />忽略 nonce</label>
      <span className="text-cx-fg-3">忽略规则仅作用于派生视图，原始字节仍可读取。</span>
      <Button size="sm" variant="primary" isPending={busy} isDisabled={busy || !baselineId || !experimentId || baselineId === experimentId} onPress={() => onCompare({ version: 1, ignore_timestamps: timestamps, ignore_nonce: nonces })}><Icon name="fileDiff" size={14} />比较</Button>
    </div>
    {result ? <>
      {result.non_single_variable || (baseline && experiment && (baseline.context_id !== experiment.context_id || baseline.identity_revision !== experiment.identity_revision)) ? <p className="pentest-notice pentest-notice--warning" role="status"><Icon name="alert" size={14} />身份、对象或目标版本同时变化，这不是严格的单变量对照。</p> : null}
      {result.warnings?.map((warning, index) => <p className="pentest-notice pentest-notice--warning" key={index} role="status">{warning}</p>)}
      <div className="flex items-center gap-2" role="group" aria-label="差异显示方式">
        <Button size="sm" variant={view === "normalized" ? "secondary" : "ghost"} onPress={() => setView("normalized")}>降噪/结构化</Button>
        <Button size="sm" variant={view === "raw" ? "secondary" : "ghost"} onPress={() => setView("raw")}>原始差异</Button>
        <span className="text-xs text-cx-fg-3">diff 只证明差异，不自行判定漏洞。</span>
      </div>
      {view === "raw" ? <pre className="max-h-[560px] overflow-auto rounded-md border border-cx-line bg-cx-bg p-3 font-mono text-xs whitespace-pre-wrap">{pretty(result.raw_diff || result.request_diff)}</pre> : <div className="grid gap-3 lg:grid-cols-2">
        <DiffBlock title="请求/Header" value={result.header_diff ?? result.request_diff} />
        <DiffBlock title="响应/Body" value={result.body_diff ?? result.response_diff} />
      </div>}
      <pre className="max-h-[420px] overflow-auto rounded-md border border-cx-line bg-cx-bg p-3 font-mono text-xs whitespace-pre-wrap">{pretty(result.normalized_diff)}</pre>
    </> : <div className="workspace-home-empty">选择两个真实 exchange 后执行对照；不会用格式化文本替换原始证据。</div>}
  </section>;
}

function DiffBlock({ title, value }: { title: string; value: unknown }) {
  return <article className="min-w-0"><h3 className="mb-2 text-sm font-medium">{title}</h3><pre className="max-h-[480px] overflow-auto rounded-md border border-cx-line bg-cx-bg p-3 font-mono text-xs whitespace-pre-wrap">{pretty(value)}</pre></article>;
}
