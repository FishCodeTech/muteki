"use client";

import { useEffect, useState } from "react";
import { apiFetch } from "./serviceAuth";
import { ctfDemoUsage, isCtfDemoRunId } from "./ctfDemoRun";
import { usageAmountDetails, type RunUsageTotals } from "./runInspectorMetrics";

export type { RunUsageTotals } from "./runInspectorMetrics";
export { usageAmountDetails } from "./runInspectorMetrics";

export type RunUsageSnapshot = {
  totals: RunUsageTotals;
  buckets?: { solve: RunUsageTotals; auxiliary: RunUsageTotals };
  groups?: { worker_id?: (RunUsageTotals & { name: string })[] };
};

export function useRunUsageLedger(
  runId: string,
  generation?: number,
  poll = true,
  refreshKey?: unknown,
) {
  const key = `${runId}:${generation ?? "all"}`;
  const [state, setState] = useState<{ key: string; data: RunUsageSnapshot | null; error: boolean }>({ key, data: null, error: false });
  useEffect(() => {
    if (isCtfDemoRunId(runId)) {
      setState({ key, data: ctfDemoUsage(runId), error: false });
      return undefined;
    }
    const controller = new AbortController();
    let busy = false;
    const read = async () => {
      if (busy || !runId) return;
      busy = true;
      try {
        const params = new URLSearchParams({ run_id: runId, limit: "1" });
        if (generation != null) params.set("generation", String(generation));
        const [data, solve, auxiliary] = await Promise.all(["", "solve", "auxiliary"].map(async scope => {
          const query = new URLSearchParams(params);
          if (scope) query.set("billing_scope", scope);
          const response = await apiFetch(`/api/usage?${query}`, { signal: controller.signal });
          if (!response.ok) throw new Error("Usage ledger unavailable");
          return await response.json() as RunUsageSnapshot;
        }));
        data.buckets = { solve: solve.totals, auxiliary: auxiliary.totals };
        if (!controller.signal.aborted) setState({ key, data, error: false });
      } catch {
        if (!controller.signal.aborted) setState(current => ({ key, data: current.key === key ? current.data : null, error: true }));
      } finally { busy = false; }
    };
    void read();
    const timer = poll ? window.setInterval(() => {
      if (!document.hidden) void read();
    }, 3000) : null;
    const onVisible = () => { if (!document.hidden) void read(); };
    document.addEventListener("visibilitychange", onVisible);
    return () => {
      controller.abort();
      if (timer != null) window.clearInterval(timer);
      document.removeEventListener("visibilitychange", onVisible);
    };
  }, [runId, generation, key, poll, refreshKey]);
  return state.key === key ? state : { key, data: null, error: false };
}

export function runUsageAmount(cost: RunUsageTotals, en = false): { amount: string; source: string } {
  const display = usageAmountDetails(cost, en);
  return { amount: cost.cost == null ? display.amount : `${display.source} ${display.amount}`, source: display.incomplete };
}

export function runUsageTokens(cost: RunUsageTotals | null | undefined, format: (value: number) => string, en = false): string {
  if (cost?.total_tokens == null) return en ? "Unreported" : "未上报";
  return `${format(cost.total_tokens)}${cost.token_coverage === "partial" ? (en ? " (partial)" : "（部分）") : ""}`;
}
