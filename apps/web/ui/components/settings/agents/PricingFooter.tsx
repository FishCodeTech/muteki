"use client";

import { useEffect, useMemo, useState } from "react";
import { Button, Collapse, IconButton, Select, TextField, Tooltip, type ListOption } from "@/components/chat/ui";
import { Icon } from "@/components/Icon";
import { cn } from "@/lib/cn";
import { useLang } from "@/lib/i18n";
import { invalidateModelPrices, priceKey, pricingRequest, useModelPrices, type PriceIdentity, type PricingCatalog } from "@/lib/modelPricing";
import { PriceEditor, priceCellView } from "./PriceEditor";

const modelKey = (item: PriceIdentity) => `${item.engine}\u0000${item.model}`;

/** Price catalog status plus the saved prices and mappings that do not belong to any listed model. */
export function PricingFooter({ catalog, error, onCatalog, knownModels, engines, defaultEngine, extra }: {
  catalog: PricingCatalog | null;
  error: string;
  onCatalog: (catalog: PricingCatalog) => void;
  /** `engine\0model` of every model offered by a credential. */
  knownModels: ReadonlySet<string>;
  engines: { value: string; label: string }[];
  /** Engine preselected for a manually added price. */
  defaultEngine?: string;
  /** A model identity that arrived through a link but is not in any list. */
  extra: PriceIdentity | null;
}) {
  const { lang } = useLang(); const en = lang === "en";
  const t = (zh: string, english: string) => en ? english : zh;
  const [open, setOpen] = useState(false);
  const [expanded, setExpanded] = useState("");
  const [drafts, setDrafts] = useState<PriceIdentity[]>([]);
  const [engine, setEngine] = useState("");
  const [model, setModel] = useState("");
  const [refreshing, setRefreshing] = useState(false);
  const [refreshError, setRefreshError] = useState("");

  const extraKey = extra ? priceKey(extra) : "";
  useEffect(() => {
    if (!extra) return;
    setDrafts(current => current.some(item => priceKey(item) === extraKey) ? current : [...current, extra]);
    setExpanded(extraKey); setOpen(true);
  }, [extraKey]); // eslint-disable-line react-hooks/exhaustive-deps

  const saved = useMemo(() => {
    const byKey = new Map<string, PriceIdentity>();
    for (const entry of [...(catalog?.overrides || []), ...(catalog?.references || [])]) {
      const identity: PriceIdentity = { engine: entry.engine, model: entry.model, credential_id: entry.credential_id, provider_id: entry.provider_id, speed: entry.speed };
      if (!knownModels.has(modelKey(identity))) byKey.set(priceKey(identity), identity);
    }
    for (const draft of drafts) if (!byKey.has(priceKey(draft))) byKey.set(priceKey(draft), draft);
    return [...byKey.values()];
  }, [catalog, drafts, knownModels]);
  const pricing = useModelPrices(saved);

  const refreshCatalog = async () => {
    setRefreshing(true); setRefreshError("");
    try { onCatalog(await pricingRequest<PricingCatalog>("?refresh=true")); invalidateModelPrices(); }
    catch (cause) { setRefreshError(cause instanceof Error ? cause.message : String(cause)); }
    finally { setRefreshing(false); }
  };
  const addDraft = () => {
    const identity: PriceIdentity = { engine: engine || defaultEngine || engines[0]?.value || "", model: model.trim() };
    if (!identity.engine || !identity.model) return;
    setDrafts(current => current.some(item => priceKey(item) === priceKey(identity)) ? current : [...current, identity]);
    setExpanded(priceKey(identity)); setModel("");
  };

  const meta = catalog?.meta;
  const status = meta?.status === "fresh" ? t("最新", "Fresh") : meta?.status === "cached" ? t("缓存", "Cached") : meta?.status || t("未知", "Unknown");
  const line = catalog
    ? `${t("价格目录", "Price catalog")} · ${catalog.catalog.length.toLocaleString()} ${t("个模型", "models")} · ${status}${meta?.fetched_at ? ` · ${new Date(meta.fetched_at * 1000).toLocaleString(en ? "en-US" : "zh-CN", { hour12: false, month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit" })}` : ""}`
    : error ? t("价格目录读取失败", "Price catalog unavailable") : t("正在读取价格目录…", "Loading price catalog…");
  const detail = [t("USD / 百万 Token，仅用于标价估算，不代表订阅实际账单；未知价格不按 0 计算。", "USD per million tokens. List-price estimates, not subscription bills; unknown prices are never zero."), meta?.message, meta?.source ? `${t("来源", "Source")}：${meta.source}` : ""].filter(Boolean).join("\n");
  const engineOptions: ListOption[] = engines.map(item => ({ value: item.value, label: item.label }));

  return (
    <section data-settings-anchor="model-pricing" id="setting-model-pricing" aria-label={t("模型价格", "Model pricing")} className="flex flex-col gap-2">
      <div className="flex flex-wrap items-center gap-2 px-1 text-[12px] text-cx-fg-3">
        <Icon name="database" size={14} className="shrink-0" />
        <span className="min-w-0 truncate" role="status">{line}</span>
        <Tooltip content={<span className="block max-w-[320px] whitespace-pre-wrap break-words">{detail}</span>}><span tabIndex={0} aria-label={t("价格目录说明", "About the price catalog")} className="grid size-5 place-items-center rounded-md outline-none hover:text-cx-fg focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]"><Icon name="info" size={13} /></span></Tooltip>
        <Button size="xs" variant="outline" icon="refresh" loading={refreshing} onClick={() => void refreshCatalog()}>{t("刷新目录", "Refresh")}</Button>
        <span className="flex-1" />
        <button type="button" aria-expanded={open} onClick={() => setOpen(value => !value)}
          className="inline-flex items-center gap-1 rounded-md px-1 py-0.5 text-cx-fg-3 outline-none hover:text-cx-fg focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]">
          <Icon name="chevronDown" size={13} className={cn("transition-transform", open && "rotate-180")} />
          {t("其它已保存的价格", "Other saved prices")}{saved.length ? `（${saved.length}）` : ""}
        </button>
      </div>
      {error || refreshError ? <p role="alert" className="px-1 text-[12px] text-cx-danger">{refreshError || error}</p> : null}
      <Collapse open={open}>
        <div className="cx-settings-card">
          {saved.length ? saved.map(identity => {
            const key = priceKey(identity);
            const view = priceCellView(pricing.values.get(key), pricing.loading, pricing.error, en);
            return (
              <div key={key} className="border-b border-cx-border-subtle last:border-b-0">
                <div className="flex flex-wrap items-center gap-2 px-4 py-2.5">
                  <div className="min-w-0 flex-1 basis-52">
                    <p className="truncate text-[12.5px] text-cx-fg"><code>{identity.model}</code></p>
                    <p className="truncate text-[11.5px] text-cx-fg-3">{identity.engine}{identity.credential_id ? ` · ${identity.credential_id}` : ""}{identity.provider_id ? ` · ${identity.provider_id}` : ""}</p>
                  </div>
                  <span className={cn("cx-tabular text-[12px]", view.tone === "warn" ? "text-cx-warning" : "text-cx-fg-2")}>{view.text}{view.source ? ` · ${view.source}` : ""}</span>
                  <IconButton icon="pencil" label={`${t("编辑", "Edit")} ${identity.model}`} onClick={() => setExpanded(expanded === key ? "" : key)} />
                </div>
                {expanded === key ? <PriceEditor identity={identity} price={pricing.values.get(key)} catalog={catalog} onClose={() => setExpanded("")} /> : null}
              </div>
            );
          }) : <p className="px-4 py-4 text-center text-[12.5px] text-cx-fg-3">{t("没有不属于当前模型列表的价格。", "No saved prices outside the current model lists.")}</p>}
          <form className="flex flex-wrap items-end gap-2 border-t border-cx-border-subtle bg-cx-bg-subtle px-4 py-3" onSubmit={event => { event.preventDefault(); addDraft(); }}>
            <div className="min-w-[140px]"><p className="mb-1.5 text-[12.5px] font-medium text-cx-fg-2">{t("引擎", "Engine")}</p>
              <Select ariaLabel={t("引擎", "Engine")} value={engine || defaultEngine || engines[0]?.value || ""} onChange={setEngine} options={engineOptions} className="w-full" /></div>
            <TextField label={t("完整模型 ID", "Exact model ID")} value={model} onChange={event => setModel(event.target.value)} containerClassName="min-w-[200px] flex-1" />
            <Button type="submit" size="sm" variant="secondary" icon="plus" disabled={!model.trim()}>{t("手动添加价格", "Add a price")}</Button>
          </form>
        </div>
      </Collapse>
    </section>
  );
}
