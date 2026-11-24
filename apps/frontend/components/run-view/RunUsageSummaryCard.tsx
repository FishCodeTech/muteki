"use client";

import { useState } from "react";
import {
  formatCostUsd,
  summarizeRunCost,
  summarizeTokenBreakdown,
  type RunUsageTotals,
  type TokenFallbackInput,
} from "@/lib/runInspectorMetrics";
import { formatTokens } from "@/lib/usageFormat";

export type RunUsageWorkerRow = RunUsageTotals & { name: string };

export function RunUsageSummaryCard({
  ledger,
  solve,
  auxiliary,
  workerGroups,
  error = false,
  fallbackUsage,
  fallbackTokens,
  en = false,
  showWorkerBreakdown = false,
}: {
  ledger?: RunUsageTotals | null;
  solve?: RunUsageTotals | null;
  auxiliary?: RunUsageTotals | null;
  workerGroups?: RunUsageWorkerRow[];
  error?: boolean;
  fallbackUsage?: TokenFallbackInput | null;
  fallbackTokens?: number | null;
  en?: boolean;
  showWorkerBreakdown?: boolean;
}) {
  const [showAllWorkers, setShowAllWorkers] = useState(false);

  const cost = summarizeRunCost(ledger, solve, auxiliary, en);
  const effectiveFallback: TokenFallbackInput | null =
    fallbackUsage ??
    (typeof fallbackTokens === "number" && Number.isFinite(fallbackTokens) && fallbackTokens > 0
      ? { total_tokens: fallbackTokens }
      : null);
  const tokens = summarizeTokenBreakdown(ledger, effectiveFallback);

  const namedWorkers = (workerGroups ?? [])
    .filter((row) => row.name && row.name !== "未上报" && row.records > 0)
    .slice()
    .sort((a, b) => (b.cost ?? -1) - (a.cost ?? -1) || (b.total_tokens ?? -1) - (a.total_tokens ?? -1));
  const visibleWorkers = showAllWorkers ? namedWorkers : namedWorkers.slice(0, 4);

  const costValue =
    error && !ledger ? (en ? "Load failed" : "读取失败") : cost.primary;
  const costSub =
    error && ledger ? (en ? "Stale data" : "刷新失败") : cost.secondary;
  const tokenSub =
    tokens.input != null || tokens.output != null
      ? `${en ? "In" : "输入"} ${tokens.input != null ? formatTokens(tokens.input) : "—"} · ${en ? "Out" : "输出"} ${tokens.output != null ? formatTokens(tokens.output) : "—"}`
      : null;

  return (
    <div className="rv-usage-card">
      <div className="rv-metric-grid">
        <div className="rv-metric-tile">
          <span className="rv-metric-label">{en ? "Cost" : "费用"}</span>
          <strong className={`rv-metric-value${cost.muted || (error && !ledger) ? " muted" : ""}`}>
            {costValue}
          </strong>
          {costSub ? <span className="rv-metric-sub">{costSub}</span> : null}
        </div>
        <div className="rv-metric-tile">
          <span className="rv-metric-label">Token</span>
          <strong className={`rv-metric-value${tokens.total == null ? " muted" : ""}`}>
            {tokens.total != null ? formatTokens(tokens.total) : "—"}
          </strong>
          {tokenSub ? <span className="rv-metric-sub">{tokenSub}</span> : null}
        </div>
      </div>

      {((tokens.cacheRead != null && tokens.cacheRead > 0) ||
        (tokens.reasoning != null && tokens.reasoning > 0)) && (
        <div className="rv-token-strip">
          {tokens.cacheRead != null && tokens.cacheRead > 0 ? (
            <span className="rv-token-chip">
              {en ? "Cache read" : "缓存读取"} {formatTokens(tokens.cacheRead)}
            </span>
          ) : null}
          {tokens.reasoning != null && tokens.reasoning > 0 ? (
            <span className="rv-token-chip">
              {en ? "Reasoning" : "思考"} {formatTokens(tokens.reasoning)}
            </span>
          ) : null}
        </div>
      )}

      {showWorkerBreakdown && namedWorkers.length > 0 ? (
        <div className="rv-cost-workers">
          {visibleWorkers.map((worker) => (
            <div className="rv-cost-worker-row" key={worker.name}>
              <span className="rv-cost-worker-name" title={worker.name}>
                {worker.name}
              </span>
              <span className={`rv-cost-worker-val${worker.cost == null ? " muted" : ""}`}>
                {worker.cost != null ? formatCostUsd(worker.cost) : en ? "Unpriced" : "未定价"}
                {worker.total_tokens != null ? ` · ${formatTokens(worker.total_tokens)}` : ""}
              </span>
            </div>
          ))}
          {namedWorkers.length > 4 ? (
            <button
              type="button"
              className="rv-files-more"
              onClick={() => setShowAllWorkers((prev) => !prev)}
            >
              {showAllWorkers
                ? en
                  ? "Show fewer"
                  : "收起"
                : en
                  ? `Show all ${namedWorkers.length}`
                  : `展开全部 ${namedWorkers.length} 个`}
            </button>
          ) : null}
        </div>
      ) : null}
    </div>
  );
}
