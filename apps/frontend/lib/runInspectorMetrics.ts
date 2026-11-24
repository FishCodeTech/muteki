import type { PricingIssue } from "./modelPricing";

export type RunUsageTotals = {
  records: number;
  total_tokens: number | null;
  input_tokens?: number | null;
  output_tokens?: number | null;
  cache_read_tokens?: number | null;
  reasoning_tokens?: number | null;
  cost: number | null;
  reported_cost: number | null;
  estimated_cost: number | null;
  unpriced: number;
  token_coverage?: string;
  pricing_coverage?: Record<string, number>;
  pricing_issues?: PricingIssue[];
};

export type ContextWindowTone = "normal" | "warn" | "danger" | "unknown";

export type ContextWindowSummary = {
  used: number | null;
  limit: number | null;
  ratio: number | null;
  percent: number | null;
  tone: ContextWindowTone;
};

export type RunCostSummary = {
  primary: string;
  secondary: string | null;
  muted: boolean;
  partial: boolean;
};

export type TokenFallbackInput = {
  input_tokens?: number | null;
  output_tokens?: number | null;
  cached_input_tokens?: number | null;
  total_tokens?: number | null;
};

export type TokenBreakdownSummary = {
  input: number | null;
  output: number | null;
  cacheRead: number | null;
  reasoning: number | null;
  total: number | null;
  hasAny: boolean;
};

function nonNegNumber(value: unknown): number | null {
  return typeof value === "number" && Number.isFinite(value) && value >= 0 ? value : null;
}

export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return "0 B";
  if (bytes < 1024) return `${Math.round(bytes)} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let value = bytes / 1024;
  let unitIdx = 0;
  while (value >= 1024 && unitIdx < units.length - 1) {
    value /= 1024;
    unitIdx += 1;
  }
  return `${value.toFixed(1).replace(/\.0$/, "")} ${units[unitIdx]}`;
}

export function formatCostUsd(amount: number): string {
  if (!Number.isFinite(amount) || amount <= 0) return "$0.00";
  if (amount < 0.0001) return "<$0.0001";
  return `$${amount.toFixed(amount < 1 ? 4 : 2)}`;
}

export function usageAmountDetails(
  cost: RunUsageTotals,
  en = false,
): { amount: string; source: string; incomplete: string } {
  const sources = cost.pricing_coverage ?? {};
  const labels: Record<string, string> = en
    ? {
        provider_reported: "Reported",
        catalog: "Catalog estimate",
        custom: "Custom estimate",
        driver_estimate: "Tool estimate",
        captured_estimate: "Historical estimate",
      }
    : {
        provider_reported: "上报金额",
        catalog: "目录估算",
        custom: "自定义估算",
        driver_estimate: "工具估算",
        captured_estimate: "历史估算",
      };
  const source =
    Object.entries(labels)
      .filter(([key]) => sources[key] > 0)
      .map(([, label]) => label)
      .join(" + ")
    || (cost.reported_cost != null && cost.estimated_cost != null
      ? en
        ? "Reported + estimate"
        : "上报 + 估算"
      : cost.estimated_cost != null
        ? en
          ? "Estimate"
          : "估算"
        : cost.reported_cost != null
          ? en
            ? "Reported"
            : "上报金额"
          : "");
  const amount =
    cost.cost == null
      ? cost.records === 0
        ? en
          ? "No usage data"
          : "费用暂无数据"
        : en
          ? "Unpriced"
          : "未定价"
      : formatCostUsd(cost.cost);
  return {
    amount,
    source,
    incomplete:
      cost.cost != null && cost.unpriced > 0 ? (en ? "Partly unpriced" : "部分未定价") : "",
  };
}

export function summarizeContextWindow(
  usedTokens: number | null | undefined,
  limit: number | null | undefined,
): ContextWindowSummary {
  const used = nonNegNumber(usedTokens);
  const cap = typeof limit === "number" && Number.isFinite(limit) && limit > 0 ? limit : null;
  const ratio = used != null && cap != null ? Math.max(0, used / cap) : null;
  const percent = ratio != null ? Math.min(100, Math.round(ratio * 100)) : null;
  const tone: ContextWindowTone =
    ratio == null ? "unknown" : ratio >= 0.85 ? "danger" : ratio >= 0.7 ? "warn" : "normal";
  return { used, limit: cap, ratio, percent, tone };
}

export function summarizeRunCost(
  ledger: RunUsageTotals | null | undefined,
  solve: RunUsageTotals | null | undefined,
  auxiliary: RunUsageTotals | null | undefined,
  en = false,
): RunCostSummary {
  if (!ledger || ledger.records === 0) {
    return {
      primary: en ? "No usage data" : "暂无数据",
      secondary: null,
      muted: true,
      partial: false,
    };
  }
  if (solve && solve.records > 0 && solve.cost == null) {
    const auxCost =
      auxiliary && auxiliary.cost != null
        ? en
          ? `Aux ${formatCostUsd(auxiliary.cost)}`
          : `辅助调用 ${formatCostUsd(auxiliary.cost)}`
        : null;
    return {
      primary: en ? "Unpriced" : "未定价",
      secondary: auxCost,
      muted: true,
      partial: Boolean(auxiliary && auxiliary.cost != null),
    };
  }
  if (ledger.cost != null) {
    const details = usageAmountDetails(ledger, en);
    const auxNote =
      solve?.cost != null && auxiliary?.cost != null && auxiliary.cost > 0
        ? en
          ? `Aux ${formatCostUsd(auxiliary.cost)}`
          : `含辅助 ${formatCostUsd(auxiliary.cost)}`
        : "";
    const secondary =
      [details.source, details.incomplete, auxNote].filter(Boolean).join(" · ") || null;
    return {
      primary: details.amount,
      secondary,
      muted: false,
      partial: ledger.unpriced > 0,
    };
  }
  return {
    primary: en ? "Unpriced" : "未定价",
    secondary: null,
    muted: true,
    partial: false,
  };
}

export function summarizeTokenBreakdown(
  ledger: RunUsageTotals | null | undefined,
  fallback?: TokenFallbackInput | null,
): TokenBreakdownSummary {
  const hasLedger = Boolean(
    ledger &&
      ledger.records > 0 &&
      (nonNegNumber(ledger.input_tokens) != null ||
        nonNegNumber(ledger.output_tokens) != null ||
        nonNegNumber(ledger.total_tokens) != null ||
        nonNegNumber(ledger.cache_read_tokens) != null ||
        nonNegNumber(ledger.reasoning_tokens) != null),
  );
  const input = hasLedger
    ? nonNegNumber(ledger?.input_tokens)
    : nonNegNumber(fallback?.input_tokens);
  const output = hasLedger
    ? nonNegNumber(ledger?.output_tokens)
    : nonNegNumber(fallback?.output_tokens);
  const cacheRead = hasLedger
    ? nonNegNumber(ledger?.cache_read_tokens)
    : nonNegNumber(fallback?.cached_input_tokens);
  const reasoning = hasLedger ? nonNegNumber(ledger?.reasoning_tokens) : null;
  const rawTotal = hasLedger
    ? nonNegNumber(ledger?.total_tokens)
    : nonNegNumber(fallback?.total_tokens);
  const total =
    rawTotal ?? (input != null || output != null ? (input ?? 0) + (output ?? 0) : null);
  const hasAny =
    input != null || output != null || cacheRead != null || reasoning != null || total != null;
  return { input, output, cacheRead, reasoning, total, hasAny };
}

export function shortWorkspaceLabel(workspaceRoot: string): string {
  const parts = workspaceRoot
    .trim()
    .replace(/[\\/]+$/, "")
    .split(/[\\/]+/)
    .filter(Boolean);
  if (parts.length <= 2) return parts.join("/") || workspaceRoot;
  return parts.slice(-2).join("/");
}
