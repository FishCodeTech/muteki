"use client";

import { useEffect, useState } from "react";
import { Button, Select, TextField } from "@/components/chat/ui";
import { useLang } from "@/lib/i18n";
import { allCredentialModels, type ConversationCredential } from "@/lib/useConversation";
import { invalidateModelPrices, priceKey, priceLabel, pricingRequest, readPricingPrefill, useModelPrices, type ModelPrice, type PriceIdentity, type PriceOverride, type PricingCatalog } from "@/lib/modelPricing";
import { SettingsNote, SettingsSection } from "./primitives";

const EMPTY = { input: "", output: "", cache_read: "", cache_write: "" };
const RATE_LABELS = { input: ["输入", "Input"], output: ["输出", "Output"], cache_read: ["缓存读取（可选）", "Cache read (optional)"], cache_write: ["缓存写入（可选）", "Cache write (optional)"] };
export function ModelPricingSettings({ credentials, searchParams }: { credentials: ConversationCredential[]; searchParams?: URLSearchParams }) {
  const { lang } = useLang(); const en = lang === "en";
  const t = (zh: string, english: string) => en ? english : zh;
  const [catalog, setCatalog] = useState<PricingCatalog | null>(null);
  const [busy, setBusy] = useState(false); const [error, setError] = useState(""); const [notice, setNotice] = useState("");
  const [engine, setEngine] = useState(""); const [model, setModel] = useState(""); const [credentialId, setCredentialId] = useState(""); const [provider, setProvider] = useState("");
  const [rates, setRates] = useState(EMPTY); const [resolved, setResolved] = useState<ModelPrice | null>(null); const [reference, setReference] = useState("");
  const [editing, setEditing] = useState(false);
  const [speed, setSpeed] = useState("");
  const prefill = readPricingPrefill(searchParams);
  const prefillKey = prefill ? priceKey(prefill) : "";
  useEffect(() => {
    if (!prefillKey) return;
    const [nextEngine, nextModel, nextCredential, nextProvider, nextSpeed] = JSON.parse(prefillKey) as string[];
    setEngine(nextEngine); setModel(nextModel); setCredentialId(nextCredential); setProvider(nextProvider); setSpeed(nextSpeed);
    setEditing(false); setRates(EMPTY); setError(""); setNotice("");
  }, [prefillKey]);
  const identity: PriceIdentity = { engine: engine.trim(), model: model.trim(), ...(credentialId ? { credential_id: credentialId } : {}), ...(provider.trim() ? { provider_id: provider.trim() } : {}), ...(speed ? { speed } : {}) };
  const identityKey = priceKey(identity);
  const autoDetect = Boolean(prefillKey && identityKey === prefillKey);
  const detection = useModelPrices([identity], autoDetect);
  const detected = detection.values.get(identityKey);
  useEffect(() => { if (autoDetect && detected) setResolved(detected); }, [autoDetect, detected]);
  useEffect(() => { setResolved(null); setReference(""); }, [identityKey]);
  useEffect(() => { let alive = true; pricingRequest<PricingCatalog>("").then(data => { if (alive) setCatalog(data); }).catch(cause => { if (alive) setError(String(cause.message || cause)); }); return () => { alive = false; }; }, []);
  const act = async (operation: () => Promise<void>) => {
    setBusy(true); setError(""); setNotice("");
    try { await operation(); } catch (cause) { setError(cause instanceof Error ? cause.message : String(cause)); } finally { setBusy(false); }
  };
  const reload = async () => {
    setCatalog(await pricingRequest<PricingCatalog>("")); invalidateModelPrices();
    if (identity.engine && identity.model) {
      const data = await pricingRequest<{ items: ModelPrice[] }>("/resolve", "POST", { items: [identity] });
      setResolved(data.items[0]);
    }
  };
  const validateIdentity = () => { if (!identity.engine || !identity.model) throw new Error(t("请填写引擎和完整模型 ID。", "Enter an engine and exact model ID.")); };
  const save = () => act(async () => {
    validateIdentity();
    const parsed = Object.fromEntries(Object.entries(rates).map(([key, value]) => [key, value.trim() === "" ? null : Number(value)])) as PriceOverride["rates"];
    if (parsed.input === null || parsed.output === null || Object.values(parsed).some(value => value !== null && (!Number.isFinite(value) || value < 0))) throw new Error(t("输入和输出价格必填；所有价格必须是大于等于 0 的数字。缓存留空表示未知。", "Input and output are required; prices must be finite numbers of zero or more. Blank cache prices mean unknown."));
    await pricingRequest("/override", "PUT", { ...identity, rates: parsed }); await reload(); setEditing(false); setNotice(t("自定义价格已保存。自动目录更新不会覆盖此价格。", "Custom price saved. Catalog updates will not replace it."));
  });
  const edit = (row: PriceOverride) => { setEngine(row.engine); setModel(row.model); setCredentialId(row.credential_id || ""); setProvider(row.provider_id || ""); setSpeed(row.speed || ""); setRates(Object.fromEntries(Object.entries(row.rates).map(([key, value]) => [key, value === null ? "" : String(value)])) as typeof EMPTY); setEditing(true); setError(""); setNotice(""); };
  const options = [{ value: "", label: t("整个引擎", "All engine credentials") }, ...credentials.filter(row => !engine || row.engine === engine).map(row => ({ value: row.id, label: row.label }))];
  if (credentialId && !options.some(row => row.value === credentialId)) options.push({ value: credentialId, label: credentialId });
  const suggested = credentials.filter(row => !engine || row.engine === engine).flatMap(allCredentialModels).map(row => row.id);
  return <SettingsSection anchor="model-pricing" title={t("模型价格", "Model pricing")} description={t("USD / 百万 Token。用于标价估算，不代表订阅实际账单；未知价格不按 0 计算。", "USD per million tokens. List-price estimates, not subscription charges; unknown prices are never zero.")} actions={<Button size="sm" variant="secondary" disabled={busy} onClick={() => void act(async () => { setCatalog(await pricingRequest<PricingCatalog>("?refresh=true")); invalidateModelPrices(); setNotice(t("已检查价格目录，请查看目录状态和更新时间。", "Catalog checked. See its status and update time.")); })}>{t("刷新目录", "Refresh catalog")}</Button>}>
    <div className="flex flex-col gap-4 p-5">
      {catalog ? <p className="break-words text-[12px] leading-5 text-cx-fg-3">{t("目录状态", "Catalog status")}：{catalog.meta.status === "fresh" ? t("最新", "Fresh") : catalog.meta.status === "cached" ? t("缓存", "Cached") : catalog.meta.status || t("未知", "Unknown")} · {catalog.catalog.length} {t("个模型", "models")}{catalog.meta.fetched_at ? ` · ${new Date(catalog.meta.fetched_at * 1000).toLocaleString()}` : ""}{catalog.meta.message ? ` · ${catalog.meta.message}` : ""}{catalog.meta.source ? <span className="block">{t("来源", "Source")}：{catalog.meta.source}</span> : null}</p> : !error ? <p role="status" className="text-[13px] text-cx-fg-3">{t("正在读取价格目录…", "Loading pricing catalog…")}</p> : null}
      {prefillKey ? <SettingsNote>{t("已带入这条用量记录的模型身份。请确认实际供应商后选择目录映射，或填写已核实的自定义单价。", "The usage record's model identity is filled in. Confirm the actual provider, then choose a catalog mapping or enter verified custom rates.")}</SettingsNote> : null}
      <form onSubmit={event => { event.preventDefault(); void save(); }} className="flex flex-col gap-4">
        <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
          <TextField label={t("引擎", "Engine")} required value={engine} placeholder="codex" list="pricing-engines" disabled={editing || busy} onChange={event => { setEngine(event.target.value); setCredentialId(""); }} />
          <datalist id="pricing-engines">{[...new Set(credentials.map(row => row.engine))].map(id => <option key={id} value={id} />)}</datalist>
          <TextField label={t("完整模型 ID", "Exact model ID")} required value={model} list="pricing-models" disabled={editing || busy} onChange={event => setModel(event.target.value)} />
          <datalist id="pricing-models">{[...new Set(suggested)].map(id => <option key={id} value={id} />)}</datalist>
          <div><p className="mb-1.5 text-[12.5px] font-medium text-cx-fg-2">{t("凭据范围", "Credential scope")}</p><Select ariaLabel={t("价格凭据范围", "Pricing credential scope")} value={credentialId} onChange={setCredentialId} disabled={editing || busy} options={options} className="w-full" /></div>
          <TextField label={t("供应商 ID（可选）", "Provider ID (optional)")} value={provider} disabled={editing || busy} onChange={event => setProvider(event.target.value)} description={t("仅在需要区分供应商时填写。", "Set only to distinguish a specific provider.")} />
        </div>
        {speed ? <p className="text-[12px] text-cx-fg-3">{t("适用服务档位", "Service tier")}: {speed}</p> : null}
        <div className="flex flex-wrap items-center gap-3"><Button size="sm" variant="secondary" disabled={busy || detection.loading || !engine.trim() || !model.trim()} onClick={() => void act(async () => { validateIdentity(); const data = await pricingRequest<{ items: ModelPrice[] }>("/resolve", "POST", { items: [identity] }); setResolved(data.items[0]); })}>{autoDetect && detection.loading ? t("正在检测价格…", "Detecting price…") : t("检测价格", "Detect price")}</Button>{resolved ? <span role="status" className="text-[12px] text-cx-fg-3">{priceLabel(resolved, "", false, en)}{resolved.reference_model ? ` · ${resolved.reference_model}` : ""}</span> : <span className="text-[12px] text-cx-fg-3">{t("按完整 ID 精确匹配，不会自动猜测相似模型。", "Exact IDs only; similar names are never applied automatically.")}</span>}</div>
        {resolved && !resolved.rates ? <p className="text-[12px] text-cx-fg-3">{resolved.missing_scope?.length ? t("已有仅适用于特定凭据或供应商的价格，请先指定上方的对应范围。", "Scoped prices exist. Select the matching credential or provider above.") : resolved.status === "ambiguous" ? t("存在多个同名目录模型，请确认下面的映射或填写自定义价格。", "Several catalog models share this name. Confirm a mapping below or enter custom prices.") : resolved.status === "subscription_unknown" ? t("这是订阅套餐占位价格，不能按免费计算。请填写已确认的自定义单价。", "This is subscription placeholder pricing, not a free tariff. Enter a verified custom rate.") : resolved.status === "unknown-tier" ? t("当前服务档位没有价格，请填写自定义价格。", "No price exists for this service tier. Enter a custom price.") : t("没有精确匹配价格，请确认目录映射或填写自定义价格。", "No exact price matched. Confirm a catalog mapping or enter custom prices.")}</p> : null}
        {resolved ? <div className="flex flex-col gap-3">
          <div><h3 className="text-[13px] font-medium text-cx-fg">{t("选择目录映射", "Choose a catalog mapping")}</h3><p className="mt-1 text-[12px] leading-5 text-cx-fg-3">{t("请选择实际提供模型和计费的供应商。同名第三方模型不会自动套用。", "Choose the provider that serves and bills this model. Same-name third-party entries are never applied automatically.")}</p></div>
          {resolved.candidates.length ? <div className="flex flex-col gap-2"><p className="text-[12px] text-cx-fg-3">{t(`共 ${resolved.candidates.length} 个同名目录候选，可滚动查看全部`, `${resolved.candidates.length} matching catalog entries. Scroll to view all.`)}</p><div className="flex max-h-44 flex-wrap gap-2 overflow-y-auto overscroll-contain rounded-sm p-1 focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2" role="group" tabIndex={0} aria-label={t("同名目录候选", "Matching catalog entries")}>{resolved.candidates.map(candidate => <Button key={candidate} size="sm" variant={reference === candidate ? "secondary" : "ghost"} disabled={busy} aria-pressed={reference === candidate} className="h-auto min-h-8 max-w-full whitespace-normal break-all text-left" onClick={() => setReference(candidate)}>{candidate}</Button>)}</div></div> : null}
          <div className="grid grid-cols-1 items-end gap-3 sm:grid-cols-[1fr_auto]"><TextField label={t("完整目录 ID（需确认）", "Exact catalog ID (confirm to apply)")} list="pricing-catalog-models" value={reference} onChange={event => setReference(event.target.value)} placeholder="provider/model" description={t("选择候选项或输入已核实的完整 ID，再确认映射。", "Choose an entry or enter a verified exact ID, then confirm the mapping.")} /><datalist id="pricing-catalog-models">{catalog?.catalog.map(row => <option key={row.model} value={row.model} />)}</datalist><Button size="sm" variant="secondary" disabled={busy || !reference.trim()} onClick={() => void act(async () => { await pricingRequest("/reference", "PUT", { ...identity, reference_model: reference.trim() }); await reload(); setNotice(t("目录映射已确认。", "Catalog mapping confirmed.")); })}>{t("确认映射", "Confirm mapping")}</Button></div>
        </div> : null}
        <h3 className="text-[13px] font-medium text-cx-fg">{t("或填写自定义单价", "Or enter custom rates")}</h3>
        <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">{(Object.keys(EMPTY) as (keyof typeof EMPTY)[]).map(key => <TextField key={key} label={`${RATE_LABELS[key][en ? 1 : 0]} · USD / 1M`} type="number" inputMode="decimal" min="0" step="any" required={key === "input" || key === "output"} value={rates[key]} disabled={busy} placeholder={key === "input" || key === "output" ? "0" : t("未知", "Unknown")} onChange={event => setRates(previous => ({ ...previous, [key]: event.target.value }))} />)}</div>
        <div className="flex flex-wrap gap-2"><Button type="submit" size="sm" disabled={busy}>{busy ? t("处理中…", "Working…") : editing ? t("保存修改", "Save changes") : t("保存自定义价格", "Save custom price")}</Button>{editing ? <Button size="sm" variant="ghost" disabled={busy} onClick={() => { setEditing(false); setRates(EMPTY); setModel(""); }}>{t("取消编辑", "Cancel edit")}</Button> : null}</div>
      </form>
      {error || (autoDetect && detection.error) ? <SettingsNote tone="danger">{error || detection.error}</SettingsNote> : null}{notice ? <SettingsNote>{notice}</SettingsNote> : null}
      <div className="border-t border-cx-border-subtle pt-4"><h3 className="mb-2 text-[13px] font-medium text-cx-fg">{t("已保存的自定义价格", "Saved custom prices")}</h3>{catalog?.overrides.length ? catalog.overrides.map(row => <div key={priceKey(row)} className="flex flex-wrap items-center gap-2 border-b border-cx-border-subtle py-3 last:border-0"><div className="min-w-0 flex-1 basis-52"><p className="break-words text-[13px] text-cx-fg">{row.engine} · {row.model}</p><p className="break-words text-[12px] text-cx-fg-3">{row.credential_id || t("整个引擎", "All credentials")}{row.provider_id ? ` · ${row.provider_id}` : ""}<br />{priceLabel({ ...row, status: "custom", candidates: [] }, "", false, en)} · {t("缓存读 / 写", "Cache read / write")} {row.rates.cache_read ?? "?"} / {row.rates.cache_write ?? "?"}</p></div><Button size="xs" variant="ghost" disabled={busy} onClick={() => edit(row)} aria-label={`${t("编辑", "Edit")} ${row.model}`}>{t("编辑", "Edit")}</Button><Button size="xs" variant="ghost" disabled={busy} onClick={() => void act(async () => { await pricingRequest("/override", "DELETE", row); await reload(); setNotice(t("自定义价格已删除，将重新按目录匹配。", "Custom price deleted; catalog matching applies again.")); })} aria-label={`${t("删除价格", "Delete price")} ${row.model}`}>{t("删除", "Delete")}</Button></div>) : <p className="text-[13px] text-cx-fg-3">{t("暂无自定义价格。已知模型仍使用自动目录。", "No custom prices. Known models use the automatic catalog.")}</p>}</div>
      {catalog?.references.length ? <div className="border-t border-cx-border-subtle pt-4"><h3 className="mb-2 text-[13px] font-medium text-cx-fg">{t("已确认的目录映射", "Confirmed catalog mappings")}</h3>{catalog.references.map(row => <div key={priceKey(row)} className="flex flex-wrap items-center gap-2 py-2"><p className="min-w-0 flex-1 break-words text-[12px] text-cx-fg-3">{row.engine} · {row.model} → {row.reference_model}{row.credential_id ? ` · ${row.credential_id}` : ""}{row.provider_id ? ` · ${row.provider_id}` : ""}</p><Button size="xs" variant="ghost" disabled={busy} onClick={() => void act(async () => { await pricingRequest("/reference", "DELETE", row); await reload(); setNotice(t("映射已删除。", "Mapping deleted.")); })}>{t("删除映射", "Delete mapping")}</Button></div>)}</div> : null}
    </div>
  </SettingsSection>;
}
