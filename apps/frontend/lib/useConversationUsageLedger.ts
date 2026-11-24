"use client";

import { useEffect, useState } from "react";
import { apiFetch } from "./serviceAuth";
import type { RunUsageTotals } from "./useRunUsageLedger";

type LedgerState = { threadId: string; totals: RunUsageTotals | null; loading: boolean; error: string };

export function useConversationUsageLedger(threadId: string, revision: string) {
  const [pricingRevision, setPricingRevision] = useState(0);
  const [state, setState] = useState<LedgerState>({ threadId, totals: null, loading: Boolean(threadId), error: "" });
  useEffect(() => {
    const invalidate = () => setPricingRevision(value => value + 1);
    const resume = () => { if (!document.hidden) invalidate(); };
    window.addEventListener("muteki:model-pricing", invalidate);
    window.addEventListener("focus", resume);
    document.addEventListener("visibilitychange", resume);
    return () => {
      window.removeEventListener("muteki:model-pricing", invalidate);
      window.removeEventListener("focus", resume);
      document.removeEventListener("visibilitychange", resume);
    };
  }, []);
  useEffect(() => {
    if (!threadId) return;
    const controller = new AbortController();
    const timer = window.setTimeout(async () => {
      setState(previous => ({ threadId, totals: previous.threadId === threadId ? previous.totals : null, loading: true, error: "" }));
      try {
        const params = new URLSearchParams({ thread_id: threadId, limit: "1" });
        const response = await apiFetch(`/api/usage?${params}`, { signal: controller.signal });
        if (!response.ok) throw new Error(`费用读取失败（${response.status}）`);
        const data = await response.json() as { totals: RunUsageTotals };
        if (!data.totals) throw new Error("费用接口返回了无效数据");
        if (!controller.signal.aborted) setState({ threadId, totals: data.totals, loading: false, error: "" });
      } catch (cause) {
        if (!controller.signal.aborted) setState(previous => ({ threadId, totals: previous.threadId === threadId ? previous.totals : null, loading: false, error: cause instanceof Error ? cause.message : "费用读取失败" }));
      }
    }, 200);
    return () => { controller.abort(); window.clearTimeout(timer); };
  }, [threadId, revision, pricingRevision]);
  return state.threadId === threadId ? state : { threadId, totals: null, loading: Boolean(threadId), error: "" };
}
