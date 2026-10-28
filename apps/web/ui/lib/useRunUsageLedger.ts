"use client";

import { useEffect, useState } from "react";
import { apiFetch } from "./serviceAuth";
import type { PricingIssue } from "./modelPricing";

export type RunUsageTotals = {
  records: number;
  total_tokens: number | null;
  input_tokens?: number | null;
  output_tokens?: number | null;
  cost: number | null;
  reported_cost: number | null;
  estimated_cost: number | null;
  unpriced: number;
  token_coverage?: string;
  pricing_coverage?: Record<string, number>;
  pricing_issues?: PricingIssue[];
};

type RunUsageSnapshot = {
  totals: RunUsageTotals;
  groups?: { worker_id?: (RunUsageTotals & { name: string })[] };
};

export function useRunUsageLedger(runId: string, generation?: number) {
  const key = `${runId}:${generation ?? "all"}`;
  const [state, setState] = useState<{ key: string; data: RunUsageSnapshot | null; error: boolean }>({ key, data: null, error: false });
  useEffect(() => {
    const controller = new AbortController();
    let busy = false;
    const read = async () => {
      if (busy || !runId) return;
      busy = true;
      try {
        const params = new URLSearchParams({ run_id: runId, limit: "1" });
        if (generation != null) params.set("generation", String(generation));
        const response = await apiFetch(`/api/usage?${params}`, { signal: controller.signal });
        if (!response.ok) throw new Error("Usage ledger unavailable");
        const data: RunUsageSnapshot = await response.json();
        if (!controller.signal.aborted) setState({ key, data, error: false });
      } catch {
        if (!controller.signal.aborted) setState(current => ({ key, data: current.key === key ? current.data : null, error: true }));
      } finally { busy = false; }
    };
    void read();
    const timer = window.setInterval(read, 3000);
    return () => { controller.abort(); window.clearInterval(timer); };
  }, [runId, generation, key]);
  return state.key === key ? state : { key, data: null, error: false };
}

export function runUsageAmount(cost: RunUsageTotals, en = false): { amount: string; source: string } {
  const display = usageAmountDetails(cost, en);
  return { amount: cost.cost == null ? display.amount : `${display.source} ${display.amount}`, source: display.incomplete };
}

/** Shared ledger amount presentation. The ledger has already priced each model and deduplicated cache tokens. */
export function usageAmountDetails(cost: RunUsageTotals, en = false): { amount: string; source: string; incomplete: string } {
  const sources = cost.pricing_coverage ?? {};
  const labels: Record<string, string> = en
    ? { provider_reported: "Reported", catalog: "Catalog estimate", custom: "Custom estimate", driver_estimate: "Tool estimate", captured_estimate: "Historical estimate" }
    : { provider_reported: "上报金额", catalog: "目录估算", custom: "自定义估算", driver_estimate: "工具估算", captured_estimate: "历史估算" };
  const source = Object.entries(labels).filter(([key]) => sources[key] > 0).map(([, label]) => label).join(" + ")
    || (cost.reported_cost != null && cost.estimated_cost != null ? (en ? "Reported + estimate" : "上报 + 估算")
      : cost.estimated_cost != null ? (en ? "Estimate" : "估算") : cost.reported_cost != null ? (en ? "Reported" : "上报金额") : "");
  const amount = cost.cost == null
    ? (cost.records === 0 ? (en ? "No usage data" : "费用暂无数据") : (en ? "Unpriced" : "未定价"))
    : cost.cost > 0 && cost.cost < 0.0001 ? "<$0.0001" : `$${cost.cost.toFixed(cost.cost < 1 ? 4 : 2)}`;
  return { amount, source, incomplete: cost.cost != null && cost.unpriced > 0 ? (en ? "Partly unpriced" : "部分未定价") : "" };
}

export function runUsageTokens(cost: RunUsageTotals | null | undefined, format: (value: number) => string, en = false): string {
  if (cost?.total_tokens == null) return en ? "Unreported" : "未上报";
  return `${format(cost.total_tokens)}${cost.token_coverage === "partial" ? (en ? " (partial)" : "（部分）") : ""}`;
}
