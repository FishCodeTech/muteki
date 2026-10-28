"use client";

import { useEffect, useMemo, useState } from "react";
import { Badge, Button, Callout, Collapse, Select, TextField, type ListOption } from "@/components/chat/ui";
import { Icon } from "@/components/Icon";
import { cn } from "@/lib/cn";
import { useLang } from "@/lib/i18n";
import {
  invalidateModelPrices, matchPriceScope, pricingRequest,
  type ModelPrice, type PriceIdentity, type PriceOverride, type PriceRates, type PricingCatalog,
} from "@/lib/modelPricing";

const FIELDS = ["input", "output", "cache_read", "cache_write"] as const;
type Fields = Record<(typeof FIELDS)[number], string>;
const LABELS: Record<(typeof FIELDS)[number], [string, string]> = {
  input: ["输入", "Input"], output: ["输出", "Output"], cache_read: ["缓存读取（选填）", "Cache read (optional)"], cache_write: ["缓存写入（选填）", "Cache write (optional)"],
};
const toFields = (rates?: PriceRates | null): Fields => Object.fromEntries(FIELDS.map(key => [key, rates?.[key] == null ? "" : String(rates[key])])) as Fields;
const money = (value: number | null | undefined) => value == null ? "?" : `$${Number(value.toPrecision(12))}`;

export type PriceCellView = { text: string; tone: "ok" | "warn" | "mute"; source?: string };

/** Short, row-sized summary of a resolved price. */
export function priceCellView(price: ModelPrice | undefined, loading: boolean, error: string, en: boolean): PriceCellView {
  const t = (zh: string, english: string) => en ? english : zh;
  if (!price) return { text: error ? t("价格读取失败", "Price unavailable") : loading ? "…" : t("价格未知", "Price unknown"), tone: "mute" };
  if (price.rates) {
    const source = price.status === "custom" ? t("自定义", "Custom") : price.reference_model ? t("映射", "Mapped") : t("目录", "Catalog");
    return { text: `${money(price.rates.input)} / ${money(price.rates.output)}`, tone: "ok", source };
  }
  if (price.missing_scope?.length) return { text: t("需要指定凭据或供应商", "Scope required"), tone: "warn" };
  if (price.status === "ambiguous") return { text: t("需要确认映射", "Mapping required"), tone: "warn" };
  if (price.status === "subscription_unknown") return { text: t("订阅套餐 · 单价未知", "Subscription · rate unknown"), tone: "warn" };
  if (price.status === "unknown-tier") return { text: t("此档位无价格", "No tier price"), tone: "warn" };
  return { text: t("未获取到价格 · 手动输入", "No price found · enter manually"), tone: "warn" };
}

const MAX_SUGGESTIONS = 5;

/** Search box over catalog model IDs: shows the best few matches and narrows as you type; only an exact ID can be confirmed. */
function CatalogPicker({ candidates, catalog, value, onChange, busy, onConfirm, t }: {
  candidates: string[]; catalog: PricingCatalog | null; value: string; onChange: (value: string) => void; busy: boolean; onConfirm: () => void;
  t: (zh: string, english: string) => string;
}) {
  const source = useMemo(() => candidates.length ? candidates : catalog?.catalog.map(row => row.model) ?? [], [candidates, catalog]);
  const known = useMemo(() => new Set(source), [source]);
  const query = value.trim();
  const terms = query.toLowerCase().split(/\s+/).filter(Boolean);
  const matches = candidates.length || terms.length ? source.filter(model => terms.every(term => model.toLowerCase().includes(term))) : [];
  const shown = matches.slice(0, MAX_SUGGESTIONS);
  const hidden = matches.length - shown.length;
  const exact = known.has(query);
  return (
    <div className="flex flex-col gap-2">
      <div className="flex items-end gap-2">
        <TextField label={t("映射到目录模型", "Map to a catalog model")} value={value} autoComplete="off" spellCheck={false}
          placeholder={t(`输入供应商或模型名搜索，如 openrouter deepseek（共 ${source.length.toLocaleString()} 个）`, `Search by provider or model, e.g. openrouter deepseek (${source.length.toLocaleString()})`)}
          onChange={event => onChange(event.target.value)} containerClassName="min-w-0 flex-1"
          onKeyDown={event => {
            if (event.key !== "Enter") return;
            event.preventDefault();
            if (exact) onConfirm(); else if (shown[0]) onChange(shown[0]);
          }} />
        <Button size="sm" variant="secondary" disabled={busy || !exact} onClick={onConfirm}>{t("确认映射", "Confirm")}</Button>
      </div>
      {shown.length ? (
        <ul role="listbox" aria-label={t("匹配的目录模型", "Matching catalog models")} className="flex flex-col gap-1">
          {shown.map(model => (
            <li key={model} role="option" aria-selected={model === query}>
              <button type="button" disabled={busy} onClick={() => onChange(model)}
                className={cn("w-full break-all rounded-lg border px-2.5 py-1.5 text-left font-mono text-[12px] outline-none transition-colors focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]",
                  model === query ? "border-cx-accent bg-cx-elevated text-cx-fg" : "border-cx-border bg-cx-elevated text-cx-fg-2 hover:border-cx-border-strong hover:text-cx-fg")}>{model}</button>
            </li>
          ))}
        </ul>
      ) : <p className="text-[12px] text-cx-fg-3">{terms.length ? t("没有匹配的目录模型，换个关键词试试。", "No catalog model matches. Try another keyword.") : t("输入关键词开始搜索目录。", "Type to search the catalog.")}</p>}
      {hidden > 0 ? <p className="text-[12px] text-cx-fg-3">{t(`还有 ${hidden.toLocaleString()} 个匹配，继续输入以缩小范围。`, `${hidden.toLocaleString()} more match. Keep typing to narrow down.`)}</p> : null}
      {query && !exact && shown.length ? <p className="text-[12px] text-cx-fg-3">{t("点选一项或输入完整 ID 后才能确认。", "Pick one or enter the exact ID to confirm.")}</p> : null}
    </div>
  );
}

export function PriceEditor({ identity, price, catalog, credentialLabel, onClose }: {
  identity: PriceIdentity; price?: ModelPrice; catalog: PricingCatalog | null; credentialLabel?: string; onClose: () => void;
}) {
  const { lang } = useLang(); const en = lang === "en";
  const t = (zh: string, english: string) => en ? english : zh;
  const override = matchPriceScope(catalog?.overrides, identity);
  const mapping = matchPriceScope(catalog?.references, identity);
  const hasPrice = Boolean(price?.rates);
  const [customize, setCustomize] = useState(false);
  const [rates, setRates] = useState<Fields>(() => toFields(override?.rates ?? price?.rates));
  const [scope, setScope] = useState<"engine" | "credential">("engine");
  const [provider, setProvider] = useState("");
  const [reference, setReference] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [advanced, setAdvanced] = useState(false);
  const ratesKey = JSON.stringify(override?.rates ?? price?.rates ?? null);
  useEffect(() => { setRates(toFields(override?.rates ?? price?.rates)); }, [ratesKey]); // eslint-disable-line react-hooks/exhaustive-deps

  const ambiguous = price?.status === "ambiguous" && !hasPrice;
  const showForm = Boolean(override) || customize || (!hasPrice && !ambiguous);
  const status = price?.status;
  const hint = !price ? "" : price.missing_scope?.length
    ? t("已有仅适用于特定凭据或供应商的价格，请在“高级”里指定对应范围。", "Scoped prices exist. Pick the matching credential or provider under Advanced.")
    : status === "ambiguous" ? t("存在多个同名目录模型。请选择实际计费的供应商并确认映射，或直接填写自定义价格。", "Several catalog models share this name. Choose the billing provider and confirm, or enter custom rates.")
    : status === "subscription_unknown" ? t("订阅套餐没有 Token 单价，不能按免费计算。请填写已确认的单价。", "Subscription plan has no token rate and is never free. Enter a verified rate.")
    : status === "unknown-tier" ? t("当前服务档位没有价格，请填写自定义价格。", "No price for this service tier. Enter a custom price.")
    : !hasPrice ? t("未找到该模型的精确价格，不会自动套用相似模型。请填写已核实的单价（USD / 百万 Token）。", "No exact price was found and similar names are never applied. Enter a verified rate (USD per million tokens).") : "";

  const act = async (operation: () => Promise<void>) => {
    setBusy(true); setError("");
    try { await operation(); invalidateModelPrices(); onClose(); } catch (cause) { setError(cause instanceof Error ? cause.message : String(cause)); } finally { setBusy(false); }
  };
  const scopedIdentity = (): PriceIdentity => ({
    engine: identity.engine, model: identity.model, ...(identity.speed ? { speed: identity.speed } : {}),
    ...(scope === "credential" && identity.credential_id ? { credential_id: identity.credential_id } : {}),
    ...(provider.trim() ? { provider_id: provider.trim() } : {}),
  });
  const save = () => act(async () => {
    const parsed = Object.fromEntries(FIELDS.map(key => [key, rates[key].trim() === "" ? null : Number(rates[key])])) as PriceRates;
    if (parsed.input === null || parsed.output === null || Object.values(parsed).some(value => value !== null && (!Number.isFinite(value) || value < 0))) {
      throw new Error(t("输入和输出价格必填；所有价格必须是大于等于 0 的数字。缓存留空表示未知。", "Input and output are required; prices must be numbers of zero or more. Blank cache prices mean unknown."));
    }
    await pricingRequest("/override", "PUT", { ...(override ? { engine: override.engine, model: override.model, credential_id: override.credential_id, provider_id: override.provider_id, speed: override.speed } : scopedIdentity()), rates: parsed });
    setCustomize(false);
  });
  const restore = (row: PriceOverride) => act(async () => { await pricingRequest("/override", "DELETE", row); setCustomize(false); });
  const confirmMapping = () => act(async () => { await pricingRequest("/reference", "PUT", { ...scopedIdentity(), reference_model: reference.trim() }); setReference(""); });
  const deleteMapping = () => act(async () => { if (mapping) await pricingRequest("/reference", "DELETE", mapping); });

  const credentialOptions: ListOption<"engine" | "credential">[] = [{ value: "engine", label: t("整个引擎", "Whole engine") }, ...(identity.credential_id ? [{ value: "credential" as const, label: credentialLabel || identity.credential_id }] : [])];
  const mappingForm = (
    <CatalogPicker candidates={price?.candidates || []} catalog={catalog} value={reference} onChange={setReference} busy={busy} onConfirm={() => void confirmMapping()} t={t} />
  );

  return (
    <div className="flex flex-col gap-3 border-t border-cx-border-subtle bg-cx-bg-subtle px-4 py-3" data-price-editor>
      {hint ? <p className="flex items-start gap-1.5 text-[12.5px] leading-5 text-cx-fg-2"><Icon name="alert" size={14} className="mt-0.5 shrink-0 text-cx-warning" />{hint}</p> : null}
      {hasPrice && !override ? (
        <div className="flex flex-wrap items-center gap-x-4 gap-y-1 text-[12.5px] text-cx-fg-2">
          {FIELDS.map(key => <span key={key}>{LABELS[key][en ? 1 : 0].replace(/（.*）| \(.*\)/, "")} <b className="font-medium text-cx-fg">{money(price!.rates![key])}</b></span>)}
          <span className="text-cx-fg-3">USD / 1M</span>
          {mapping ? <Badge tone="neutral">{t("映射到", "Mapped to")} {mapping.reference_model}</Badge> : null}
        </div>
      ) : null}
      {status === "ambiguous" && !override ? mappingForm : null}
      {showForm ? (
        <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
          {FIELDS.map(key => (
            <TextField key={key} label={`${LABELS[key][en ? 1 : 0]} · USD / 1M`} type="number" inputMode="decimal" min="0" step="any" disabled={busy}
              required={key === "input" || key === "output"} value={rates[key]} placeholder={key === "input" || key === "output" ? "0" : t("未知", "Unknown")}
              onChange={event => setRates(previous => ({ ...previous, [key]: event.target.value }))} />
          ))}
        </div>
      ) : null}
      {showForm || mapping || hasPrice ? (
        <div>
          <button type="button" aria-expanded={advanced} onClick={() => setAdvanced(value => !value)}
            className="inline-flex items-center gap-1 rounded-md text-[12px] text-cx-fg-3 outline-none hover:text-cx-fg focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]">
            <Icon name="chevronDown" size={12} className={cn("transition-transform", advanced && "rotate-180")} />{t("高级", "Advanced")}
          </button>
          <Collapse open={advanced}>
            <div className="mt-2 flex flex-col gap-3">
              {!override ? (
                <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
                  <div><p className="mb-1.5 text-[12.5px] font-medium text-cx-fg-2">{t("适用范围", "Applies to")}</p>
                    <Select ariaLabel={t("价格适用范围", "Price scope")} value={scope} onChange={setScope} options={credentialOptions} className="w-full" /></div>
                  <TextField label={t("供应商 ID（选填）", "Provider ID (optional)")} value={provider} onChange={event => setProvider(event.target.value)} />
                </div>
              ) : <p className="text-[12px] text-cx-fg-3">{t("范围", "Scope")}：{override.credential_id || t("整个引擎", "Whole engine")}{override.provider_id ? ` · ${override.provider_id}` : ""}</p>}
              {identity.speed ? <p className="text-[12px] text-cx-fg-3">{t("服务档位", "Service tier")}：{identity.speed}</p> : null}
              {status !== "ambiguous" && !mapping ? mappingForm : null}
            </div>
          </Collapse>
        </div>
      ) : null}
      {error ? <Callout tone="danger" icon="alert" role="alert">{error}</Callout> : null}
      <div className="flex flex-wrap items-center gap-2">
        {showForm ? <Button size="sm" variant="primary" icon="check" loading={busy} onClick={() => void save()}>{t("保存价格", "Save price")}</Button> : <Button size="sm" variant="outline" disabled={busy} onClick={() => setCustomize(true)}>{ambiguous ? t("手动输入价格", "Enter a price manually") : t("改为自定义价格", "Use a custom price")}</Button>}
        {override ? <Button size="sm" variant="outline" disabled={busy} onClick={() => void restore(override)}>{t("恢复目录价", "Restore catalog price")}</Button> : null}
        {mapping ? <Button size="sm" variant="outline" disabled={busy} onClick={() => void deleteMapping()}>{t("删除映射", "Delete mapping")}</Button> : null}
        <span className="flex-1" />
        <Button size="sm" variant="ghost" disabled={busy} onClick={onClose}>{t("收起", "Close")}</Button>
      </div>
    </div>
  );
}
