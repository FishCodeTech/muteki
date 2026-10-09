import { apiFetch } from "./serviceAuth";
import { formatTokens, formatUsd } from "./usageFormat";

export type UsageMetric = "cost" | "tokens" | "limits";
export type UsageScope = "history" | "muteki";
export type UsageDays = "1" | "7" | "30" | "90" | "all" | "custom";
export type UsageTotals = {
  total_tokens: number | null; uncached_input: number | null; unclassified_input: number | null; cache_read: number | null;
  cache_write: number | null; output: number | null; reasoning: number | null;
  cost: number | null; reported_cost: number | null; estimated_cost: number | null;
  historical_scope_unknown: number | null; unpriced: number | null; records: number | null; sessions: number | null; missing: number | null;
};
export type UsageProvider = UsageTotals & { engine: string; status: string; message?: string };
export type UsageModel = UsageTotals & { engine: string; model: string };
export type UsageSeries = { slot: string; total_tokens: number | null; cost: number | null; providers: Record<string, { total_tokens: number | null; cost: number | null }> };
export type UsageSummary = {
  as_of: number | null;
  window: { days: number | null; resolution: string; time_zone: string; slots: string[]; since_ms: number | null; until_ms: number | null };
  totals: UsageTotals; providers: UsageProvider[]; models: UsageModel[]; series: UsageSeries[];
  sources: { engine: string; status: string; message: string }[];
  pricing: { status: string; source: string; fetched_at: number | null; message: string } | null;
  cache_warning: string;
};
export type UsageLimitWindow = { aggregate: boolean; id: string; kind: "session" | "weekly" | "monthly" | "other"; label: string; used_percent: number | null; reset_at: number | null; window_seconds: number | null };
export type UsageLimitAccount = {
  id: string; engine: string; label: string; status: string; windows: UsageLimitWindow[];
  updated_at: number | null; error: string | null; error_code: string | null; stale: boolean;
};
export type UsageLimits = { as_of: number | null; accounts: UsageLimitAccount[] };

const ENGINES: Record<string, string> = {
  codex: "Codex", claude: "Claude Code", cursor: "Cursor", grok: "Grok", opencode: "OpenCode",
  pi: "Pi", kimi: "Kimi", omp: "OMP", devin: "Devin", droid: "Droid", antigravity: "Antigravity",
  unknown: "辅助调用 / 未归属", reason: "辅助调用", dsh: "DeepSeek Harness",
};
export const UNSUPPORTED_LIMIT_ENGINES = ["pi", "kimi", "omp", "devin", "droid"] as const;
export const engineName = (engine: string) => ENGINES[engine] || engine || ENGINES.unknown;
export const engineColor = (engine: string) => Object.hasOwn(ENGINES, engine) && /^[a-z]+$/.test(engine)
  ? `var(--eng-${engine}, var(--cx-accent))` : "var(--cx-accent)";
export const finiteValue = (value: unknown): number | null => typeof value === "number" && Number.isFinite(value) && value >= 0 ? value : null;
export const tokenValue = (value: unknown) => { const number = finiteValue(value); return number === null ? "—" : formatTokens(number); };
export const moneyValue = (value: unknown) => { const number = finiteValue(value); return number === null ? "未定价" : number > 0 && number < .01 ? "<$0.01" : formatUsd(number); };
export const exactMoney = (value: unknown) => { const number = finiteValue(value); return number === null ? "未定价" : `$${number}`; };
export const countValue = (value: unknown) => { const number = finiteValue(value); return number === null ? "—" : number.toLocaleString(); };
export const pricingLabel = (totals: UsageTotals) => totals.reported_cost !== null && totals.estimated_cost !== null ? "上报 + API 估算"
  : totals.reported_cost !== null ? "供应商上报" : totals.estimated_cost !== null ? "API 估算" : "未定价";
export const statusLabel = (status: string) => ({ ok: "已读取", ready: "已读取", partial: "部分可用", empty: "暂无记录", unsupported: "暂未支持", unauthenticated: "未登录", disabled: "未开启", error: "读取失败", missing: "暂无记录", unavailable: "不可用", ledger: "Muteki 记录" }[status] || status || "未知");

function object(value: unknown): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new Error("用量接口返回了无效数据");
  return value as Record<string, unknown>;
}
const string = (value: unknown, fallback = "") => typeof value === "string" ? value : fallback;
function array(value: unknown): unknown[] {
  if (!Array.isArray(value)) throw new Error("用量接口返回的数据列表无效");
  return value;
}
function totals(value: unknown): UsageTotals {
  const row = object(value);
  return {
    total_tokens: finiteValue(row.total_tokens), uncached_input: finiteValue(row.uncached_input), unclassified_input: finiteValue(row.unclassified_input), cache_read: finiteValue(row.cache_read),
    cache_write: finiteValue(row.cache_write), output: finiteValue(row.output), reasoning: finiteValue(row.reasoning),
    cost: finiteValue(row.cost), reported_cost: finiteValue(row.reported_cost), estimated_cost: finiteValue(row.estimated_cost),
    historical_scope_unknown: finiteValue(row.historical_scope_unknown), unpriced: finiteValue(row.unpriced), records: finiteValue(row.records), sessions: finiteValue(row.sessions), missing: finiteValue(row.missing),
  };
}
async function read(path: string, signal?: AbortSignal): Promise<unknown> {
  const response = await apiFetch(path, { signal });
  if (!response.ok) {
    let detail = "";
    try { const body = object(await response.json()); detail = string(body.detail); } catch { /* The HTTP status remains the error when the body is not JSON. */ }
    throw new Error(detail || `用量读取失败（${response.status}）`);
  }
  return response.json();
}
export async function fetchUsageSummary(query: string, signal?: AbortSignal): Promise<UsageSummary> {
  const data = object(await read(`/api/usage/summary?${query}`, signal));
  const window = object(data.window);
  const pricing = data.pricing ? object(data.pricing) : null;
  return {
    as_of: finiteValue(data.as_of),
    window: { days: finiteValue(window.days), resolution: string(window.resolution), time_zone: string(window.time_zone), slots: array(window.slots).map(value => string(value)), since_ms: finiteValue(window.since_ms), until_ms: finiteValue(window.until_ms) },
    totals: totals(data.totals),
    providers: array(data.providers).map(value => { const row = object(value); return { ...totals(row), engine: string(row.engine, "unknown"), status: string(row.status, "unknown"), message: string(row.message) }; }),
    models: array(data.models).map(value => { const row = object(value); return { ...totals(row), engine: string(row.engine, "unknown"), model: string(row.model, "模型未上报") }; }),
    series: array(data.series).map(value => {
      const row = object(value);
      const providers = object(row.providers);
      return { slot: string(row.slot), total_tokens: finiteValue(row.total_tokens), cost: finiteValue(row.cost), providers: Object.fromEntries(Object.entries(providers).map(([engine, value]) => { const entry = object(value); return [engine, { total_tokens: finiteValue(entry.total_tokens), cost: finiteValue(entry.cost) }]; })) };
    }),
    pricing: pricing ? { status: string(pricing.status), source: string(pricing.source), fetched_at: finiteValue(pricing.fetched_at), message: string(pricing.message) } : null,
    cache_warning: string(data.cache_warning),
    sources: array(data.sources).map(value => { const row = object(value); return { engine: string(row.engine, "unknown"), status: string(row.status, "unknown"), message: string(row.message) }; }),
  };
}
export async function fetchUsageLimits(refresh: boolean, signal?: AbortSignal): Promise<UsageLimits> {
  const data = object(await read(`/api/usage/limits${refresh ? "?refresh=true" : ""}`, signal));
  return { as_of: finiteValue(data.as_of), accounts: array(data.accounts).map(value => {
    const account = object(value);
    return {
      id: string(account.id), engine: string(account.engine, "unknown"), label: string(account.label), status: string(account.status, "unknown"),
      updated_at: finiteValue(account.updated_at), error: string(account.error) || null, error_code: string(account.error_code) || null, stale: account.stale === true,
      windows: array(account.windows).map(value => {
        const window = object(value);
        const kind = string(window.kind);
        return { aggregate: window.aggregate === true, id: string(window.id), kind: (["session", "weekly", "monthly"].includes(kind) ? kind : "other") as UsageLimitWindow["kind"], label: string(window.label), used_percent: finiteValue(window.used_percent), reset_at: finiteValue(window.reset_at), window_seconds: finiteValue(window.window_seconds) };
      }),
    };
  }) };
}
export function slotLabel(slot: string): string {
  const match = /^(\d{4})-(\d{2})-(\d{2})(?:[ T](\d{2})(?::(\d{2}))?)?/.exec(slot);
  if (!match) return slot;
  return match[4] ? `${Number(match[2])}/${Number(match[3])} ${match[4]}:${match[5] || "00"}` : `${Number(match[2])}月${Number(match[3])}日`;
}
export function timestampLabel(value: number | null): string {
  return value === null ? "尚未更新" : new Date(value * 1000).toLocaleString("zh-CN", { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" });
}
