"use client";

import { useEffect, useState } from "react";
import { apiFetch } from "./serviceAuth";

export type PriceIdentity = { engine: string; model: string; credential_id?: string; provider_id?: string; speed?: string };
export type PriceRates = { input: number | null; output: number | null; cache_read: number | null; cache_write: number | null };
export type ModelPrice = PriceIdentity & { status: "custom" | "catalog" | "missing" | "ambiguous" | "unknown-tier" | "subscription_unknown"; rates: PriceRates | null; candidates: string[]; reference_model?: string; missing_scope?: ("credential_id" | "provider_id")[] };
export type PriceOverride = PriceIdentity & { rates: PriceRates };
export type PriceReference = PriceIdentity & { reference_model: string };
export type PricingCatalog = { catalog: { model: string; rates: PriceRates | null; tiers: string[]; status?: string }[]; overrides: PriceOverride[]; references: PriceReference[]; meta: { status?: string; source?: string; fetched_at?: number | null; message?: string } };
const priceModel = (item: PriceIdentity) => item.model.trim().toLowerCase();
// Speedless saved prices and model-table rows refer to the standard tier.
const priceSpeed = (item: PriceIdentity) => item.speed?.trim().toLowerCase() || "standard";
export const priceKey = (item: PriceIdentity) => JSON.stringify([item.engine, priceModel(item), item.credential_id || "", item.provider_id || "", priceSpeed(item)]);
export async function pricingRequest<T>(path: string, method = "GET", body?: unknown): Promise<T> {
  const response = await apiFetch(`/api/usage/pricing${path}`, { method, ...(body === undefined ? {} : { headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) }) });
  if (!response.ok) {
    const data = await response.json().catch(() => null);
    throw new Error(typeof data?.detail === "string" ? data.detail : `价格服务请求失败 (HTTP ${response.status})`);
  }
  return response.json() as Promise<T>;
}
const PRICE_CACHE_TTL_MS = 60_000;
let prices = new Map<string, { promise: Promise<ModelPrice>; expiresAt: number }>();
let pending: { item: PriceIdentity; resolve: (price: ModelPrice) => void; reject: (error: unknown) => void }[] = [];
let scheduled = false;
function resolvePrice(item: PriceIdentity): Promise<ModelPrice> {
  item = { ...item, credential_id: item.credential_id || undefined, provider_id: item.provider_id || undefined, speed: item.speed || undefined };
  const key = priceKey(item);
  const cached = prices.get(key);
  if (cached && cached.expiresAt > Date.now()) return cached.promise;
  const result = new Promise<ModelPrice>((resolve, reject) => {
    pending.push({ item, resolve, reject });
    if (scheduled) return;
    scheduled = true;
    queueMicrotask(() => {
      const batch = pending; pending = []; scheduled = false;
      for (let offset = 0; offset < batch.length; offset += 1000) {
        const chunk = batch.slice(offset, offset + 1000);
        void pricingRequest<{ items: ModelPrice[] }>("/resolve", "POST", { items: chunk.map(row => row.item) }).then(data => {
          chunk.forEach((row, index) => { if (data.items[index]) row.resolve({ ...data.items[index], ...row.item }); else { prices.delete(priceKey(row.item)); row.reject(new Error("价格响应缺失")); } });
        }).catch(error => chunk.forEach(row => { prices.delete(priceKey(row.item)); row.reject(error); }));
      }
    });
  });
  prices.set(key, { promise: result, expiresAt: Date.now() + PRICE_CACHE_TTL_MS });
  return result;
}
export function invalidateModelPrices() { prices = new Map(); window.dispatchEvent(new Event("muteki:model-pricing")); }
export function useModelPrices(items: PriceIdentity[], enabled = true) {
  const serialized = JSON.stringify(items);
  const [revision, setRevision] = useState(0);
  const [state, setState] = useState<{ values: Map<string, ModelPrice>; error: string; loading: boolean }>({ values: new Map(), error: "", loading: false });
  useEffect(() => { const update = () => setRevision(value => value + 1); window.addEventListener("muteki:model-pricing", update); return () => window.removeEventListener("muteki:model-pricing", update); }, []);
  useEffect(() => {
    if (!enabled) return;
    let alive = true;
    const requested = JSON.parse(serialized) as PriceIdentity[];
    setState({ values: new Map(), error: "", loading: requested.length > 0 });
    void Promise.all(requested.map(resolvePrice)).then(rows => { if (alive) setState({ values: new Map(rows.map(row => [priceKey(row), row])), error: "", loading: false }); }).catch(error => { if (alive) setState({ values: new Map(), error: error instanceof Error ? error.message : String(error), loading: false }); });
    return () => { alive = false; };
  }, [serialized, enabled, revision]);
  return state;
}
/** Catalog, saved custom prices and confirmed mappings; reloads whenever any price changes. */
export function usePricingCatalog() {
  const [catalog, setCatalog] = useState<PricingCatalog | null>(null);
  const [error, setError] = useState("");
  const [revision, setRevision] = useState(0);
  useEffect(() => { const update = () => setRevision(value => value + 1); window.addEventListener("muteki:model-pricing", update); return () => window.removeEventListener("muteki:model-pricing", update); }, []);
  useEffect(() => {
    let alive = true;
    pricingRequest<PricingCatalog>("").then(data => { if (alive) { setCatalog(data); setError(""); } }).catch(cause => { if (alive) setError(cause instanceof Error ? cause.message : String(cause)); });
    return () => { alive = false; };
  }, [revision]);
  return { catalog, error, setCatalog };
}
/** The most specific saved entry that applies to this identity (an engine-wide entry also applies to scoped rows). */
export function matchPriceScope<T extends PriceIdentity>(entries: T[] | undefined, identity: PriceIdentity): T | null {
  const hits = (entries || []).filter(entry => entry.engine === identity.engine && priceModel(entry) === priceModel(identity)
    && priceSpeed(entry) === priceSpeed(identity)
    && (!entry.credential_id || entry.credential_id === identity.credential_id)
    && (!entry.provider_id || entry.provider_id === identity.provider_id));
  const weight = (entry: PriceIdentity) => Number(Boolean(entry.credential_id)) + Number(Boolean(entry.provider_id));
  const specificity = Math.max(...hits.map(weight));
  const best = hits.filter(entry => weight(entry) === specificity);
  return best.length === 1 ? best[0] : null;
}
export function priceLabel(price?: ModelPrice, error = "", loading = false, en = false): string {
  if (!price) return error ? (en ? "Price unavailable" : "价格读取失败") : loading ? (en ? "Loading prices…" : "正在读取价格…") : (en ? "Price unknown" : "价格未知");
  if (price.missing_scope?.length && !price.rates) return en ? "Credential/provider scope required" : "需要凭据或供应商范围";
  const source = price.status === "custom" ? (en ? "Custom" : "自定义") : price.status === "catalog" ? (en ? "Catalog" : "目录") : "";
  if (!price.rates) return ({ missing: en ? "Price unknown" : "价格未知", ambiguous: en ? "Mapping required" : "需要确认映射", "unknown-tier": en ? "Tier price unknown" : "此服务档位价格未知", subscription_unknown: en ? "Subscription plan; token rate unknown" : "订阅套餐，Token 单价未知", custom: "", catalog: "" })[price.status];
  const rate = (value: number | null) => value === null ? "?" : `$${Number(value.toPrecision(12))}`;
  return `${en ? "In" : "入"} ${rate(price.rates.input)} · ${en ? "Out" : "出"} ${rate(price.rates.output)}/1M · ${source}${price.reference_model ? (en ? " mapping" : "映射") : ""}`;
}

export type PricingIssue = Partial<PriceIdentity> & {
  reason: string;
  worker_id?: string;
  role?: string;
  count?: number;
  candidates?: string[];
  missing_scope?: string[];
};

const PREFILL_FIELDS = ["engine", "model", "credential_id", "provider_id", "speed"] as const;
export function pricingSettingsHref(identity: Partial<PriceIdentity>): string {
  const params = new URLSearchParams();
  for (const field of PREFILL_FIELDS) {
    const value = identity[field];
    if (typeof value === "string" && value.trim()) params.set(`pricing_${field}`, value.trim());
  }
  return `/settings/agents${params.size ? `?${params}` : ""}#setting-model-pricing`;
}

export function readPricingPrefill(params?: URLSearchParams): PriceIdentity | null {
  const engine = params?.get("pricing_engine")?.trim();
  const model = params?.get("pricing_model")?.trim();
  if (!engine || !model) return null;
  const identity: PriceIdentity = { engine, model };
  for (const field of PREFILL_FIELDS) {
    const value = params?.get(`pricing_${field}`)?.trim();
    if (value) identity[field] = value;
  }
  return identity;
}

export function pricingIssueMessage(issue: PricingIssue, en = false): string {
  const text: Record<string, [string, string]> = {
    ambiguous_model: ["存在多个同名目录模型，尚未确认适用的价格映射。", "Several catalog models share this name. Confirm the applicable price mapping."],
    missing_provider: ["这条记录未保存供应商身份，无法确定适用的目录价格。", "This record has no provider identity to select the applicable catalog price."],
    missing_price: ["没有匹配的目录价格或已保存的自定义单价。", "No matching catalog price or saved custom rate is available."],
    missing_tokens: ["Token 明细未完整上报，无法按单价独立核算。", "Token details were not fully reported, so the amount cannot be independently calculated from rates."],
    historical_scope_unknown: ["历史用量口径无法确认，已排除其金额。", "The historical usage scope is unknown, so its amount is excluded."],
    unknown_tier: ["当前服务档位没有可用单价。", "No price is available for this service tier."],
    subscription_unknown: ["订阅套餐未提供 Token 单价，不能按免费计费。", "The subscription plan has no token rate and cannot be treated as free."],
  };
  return text[issue.reason]?.[en ? 1 : 0] || (en ? "Pricing information is incomplete. Check the model and pricing scope." : "价格信息不完整，请确认模型和定价范围。");
}
