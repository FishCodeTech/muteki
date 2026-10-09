"use client";

import dynamic from "next/dynamic";
import { useEffect, useMemo, useRef, useState, type CSSProperties } from "react";
import { EngineLogo } from "./EngineLogo";
import { Button } from "./chat/ui/Button";
import { Callout, Skeleton } from "./chat/ui/Feedback";
import { SegmentedControl } from "./chat/ui/Tabs";
import { ProviderUsageChart } from "./usage/ProviderUsageChart";
import {
  countValue, engineColor, engineName, exactMoney, fetchUsageLimits, fetchUsageSummary, moneyValue, pricingLabel,
  slotLabel, statusLabel, timestampLabel, tokenValue, type UsageDays, type UsageLimits, type UsageMetric,
  type UsageProvider, type UsageScope, type UsageSummary, type UsageTotals,
} from "@/lib/providerUsage";
import { formatShare } from "@/lib/usageFormat";
import styles from "./UsageDashboard.module.css";

const UsageLimitsView = dynamic(() => import("./usage/UsageLimitsView").then(module => module.UsageLimitsView));
const UsageLedger = dynamic(() => import("./usage/UsageLedger").then(module => module.UsageLedger));
const STORAGE_KEY = "muteki.usage.v2";
const DEFAULT_ENGINES = ["codex", "claude", "cursor", "grok", "opencode", "pi", "kimi", "omp", "devin", "droid"];
const ROLES: Record<string, string> = { worker: "Worker", assistant: "对话 Agent", reason: "Reason", titler: "标题 / 摘要", dispatch: "任务列表标题" };
const PERIODS = [{ value: "1", label: "24 小时" }, { value: "7", label: "7 天" }, { value: "30", label: "30 天" }, { value: "90", label: "90 天" }];
type ChallengeOption = { id: string; label: string };
type Preferences = { metric: UsageMetric; days: UsageDays; scope: UsageScope };
type Resource = { key: string; loading: boolean; error: string; data: UsageSummary | UsageLimits | null };

function EngineName({ engine }: { engine: string }) {
  const known = DEFAULT_ENGINES.includes(engine);
  return <span className={styles.engineName}>{known ? <EngineLogo engine={engine} size={19} /> : <span className={styles.unknownEngine} aria-hidden="true">·</span>}<span>{engineName(engine)}</span></span>;
}

function Coverage({ totals }: { totals: UsageTotals }) {
  const records = totals.records;
  return <div className={styles.coverage}>
    <span>{countValue(records)} 条记录</span>
    {totals.historical_scope_unknown ? <span data-warning="true">历史口径未知 {countValue(totals.historical_scope_unknown)} 条 · 已排除原观测数值</span> : null}
    {records !== null && records > 0 && totals.missing !== null ? <span data-warning={totals.missing > 0}>Token 覆盖 {formatShare(Math.max(0, records - totals.missing) / records)}{totals.missing > 0 ? ` · ${countValue(totals.missing)} 条未上报` : ""}</span> : null}
    {records !== null && records > 0 && totals.unpriced !== null ? <span data-warning={totals.unpriced > 0}>定价覆盖 {formatShare(Math.max(0, records - totals.unpriced) / records)}{totals.unpriced > 0 ? ` · ${countValue(totals.unpriced)} 条未定价` : ""}</span> : null}
  </div>;
}

function TokenTotals({ totals }: { totals: UsageTotals }) {
  return <div className={styles.tokenTotals}>{([
    ["处理 Token", totals.total_tokens, `${countValue(totals.records)} 条记录`],
    ["未缓存输入", totals.uncached_input, ""], ["缓存读取", totals.cache_read, ""], ["缓存写入", totals.cache_write, ""],
    ["输出", totals.output, totals.reasoning !== null ? `含推理 ${tokenValue(totals.reasoning)}` : "推理明细未上报"],
  ] as [string, number | null, string][]).map(([label, value, note], index) => <div key={label} data-primary={index === 0}><span>{label}</span><strong title={value === null ? "未上报" : countValue(value)}>{tokenValue(value)}</strong>{note ? <small>{note}</small> : null}</div>)}</div>;
}

function Composition({ totals, metric }: { totals: UsageTotals; metric: "cost" | "tokens" }) {
  const parts = metric === "cost" ? [
    { label: "供应商上报", value: totals.reported_cost, color: "var(--cx-accent)" },
    { label: "API 估算", value: totals.estimated_cost, color: "color-mix(in oklab, var(--cx-accent) 45%, var(--cx-bg))" },
  ] : [
    { label: "未缓存输入", value: totals.uncached_input, color: "var(--cx-accent)" },
    { label: "缓存读取", value: totals.cache_read, color: "color-mix(in oklab, var(--cx-accent) 62%, var(--cx-bg))" },
    { label: "缓存写入", value: totals.cache_write, color: "var(--eng-omp, var(--cx-fg-3))" },
    { label: "输出", value: totals.output, color: "var(--eng-kimi, var(--cx-fg-2))" },
    ...(totals.unclassified_input !== null && totals.unclassified_input > 0 ? [{ label: "未细分输入", value: totals.unclassified_input, color: "var(--cx-fg-3)" }] : []),
  ];
  const known = parts.reduce((sum, part) => sum + (part.value ?? 0), 0);
  const format = metric === "cost" ? moneyValue : tokenValue;
  return <section className={styles.composition} aria-label={metric === "cost" ? "费用来源" : "Token 构成"}>
    <h2>{metric === "cost" ? "费用来源" : "Token 构成"}</h2>
    {known > 0 ? <div className={styles.compositionBar} aria-hidden="true">{parts.filter(part => part.value !== null && part.value > 0).map(part => <span key={part.label} style={{ width: `${part.value! / known * 100}%`, background: part.color }} />)}</div> : null}
    {metric === "tokens" && totals.unclassified_input !== null && totals.unclassified_input > 0 ? <p className={styles.unclassifiedInputNote}>未细分输入的缓存明细未上报，已计入处理 Token，不归入未缓存输入。</p> : null}
    <div className={styles.compositionLegend}>{parts.map(part => <span key={part.label}><i style={{ background: part.color }} aria-hidden="true" />{part.label}<b title={metric === "cost" ? exactMoney(part.value) : countValue(part.value)}>{format(part.value)}</b>{part.value !== null && known > 0 ? <small>{formatShare(part.value / known)}</small> : null}</span>)}</div>
  </section>;
}

function Breakdown({ data, metric }: { data: UsageSummary; metric: "cost" | "tokens" }) {
  const [group, setGroup] = useState(metric === "cost" ? "model" : "engine");
  const [page, setPage] = useState(0);
  const field = metric === "cost" ? "cost" : "total_tokens";
  const format = metric === "cost" ? moneyValue : tokenValue;
  const rows = useMemo(() => {
    if (group === "day") return data.series.map(row => ({ id: row.slot, label: slotLabel(row.slot), engine: "", value: row[field], tokens: row.total_tokens, cost: row.cost, note: "已知部分", total: null }));
    const values = group === "engine" ? data.providers : data.models;
    return values.map((row, index) => ({ id: `${row.engine}-${"model" in row ? row.model : ""}-${index}`, label: "model" in row ? row.model : engineName(row.engine), engine: row.engine, value: row[field], tokens: row.total_tokens, cost: row.cost, note: group === "engine" && (row as UsageProvider).message ? (row as UsageProvider).message! : metric === "cost" ? pricingLabel(row) : "status" in row ? statusLabel(row.status) : `${countValue(row.records)} 条记录`, total: row })).sort((left, right) => (right.value ?? -1) - (left.value ?? -1));
  }, [data, field, group, metric]);
  const total = data.totals[field];
  const lastPage = Math.max(0, Math.ceil(rows.length / 20) - 1);
  const currentPage = Math.min(page, lastPage);
  const visible = rows.slice(currentPage * 20, (currentPage + 1) * 20);
  return <section className={styles.breakdown}><div className={styles.sectionHeading}><h2>{metric === "cost" ? "费用明细" : "用量明细"}</h2><SegmentedControl value={group} onChange={value => { setGroup(value); setPage(0); }} ariaLabel="明细分组" options={[{ value: "engine", label: "引擎" }, { value: "model", label: "模型" }, { value: "day", label: "时间" }]} /></div>
    {rows.length ? <><div className={styles.tableScroll}><table><thead><tr><th className={styles.rankColumn}>#</th><th>{group === "model" ? "模型" : group === "day" ? "时间" : "引擎"}</th>{group === "model" ? <th>引擎</th> : null}<th className={styles.numeric}>{metric === "cost" ? "已定价费用" : "处理 Token"}</th><th className={styles.numeric}>占比</th><th className={styles.numeric}>{metric === "cost" ? "Token" : "费用"}</th><th>数据状态</th></tr></thead><tbody>{visible.map((row, index) => <tr key={row.id}><td className={styles.rankColumn}>{currentPage * 20 + index + 1}</td><th scope="row">{group === "engine" ? <EngineName engine={row.engine} /> : row.label}</th>{group === "model" ? <td><EngineName engine={row.engine} /></td> : null}<td className={styles.numeric}><strong title={metric === "cost" ? exactMoney(row.value) : countValue(row.value)}>{format(row.value)}</strong></td><td className={styles.numeric}>{row.value !== null && total !== null && total > 0 ? formatShare(row.value / total) : "—"}</td><td className={styles.numeric} title={metric === "tokens" ? exactMoney(row.cost) : countValue(row.tokens)}>{metric === "cost" ? tokenValue(row.tokens) : moneyValue(row.cost)}</td><td className={styles.rowNote}>{row.note}{row.total && (row.total.unpriced || row.total.missing) ? <small>{row.total.unpriced ? `${countValue(row.total.unpriced)} 条未定价` : ""}{row.total.unpriced && row.total.missing ? " · " : ""}{row.total.missing ? `${countValue(row.total.missing)} 条 Token 未上报` : ""}{row.total.historical_scope_unknown ? `（含 ${countValue(row.total.historical_scope_unknown)} 条历史口径未知）` : ""}</small> : null}</td></tr>)}</tbody></table></div>{rows.length > 20 ? <div className={styles.pagination}><span>{rows.length} 项 · 第 {currentPage + 1} / {lastPage + 1} 页</span><Button variant="outline" size="sm" disabled={currentPage === 0} onClick={() => setPage(currentPage - 1)}>上一页</Button><Button variant="outline" size="sm" disabled={currentPage === lastPage} onClick={() => setPage(currentPage + 1)}>下一页</Button></div> : null}</> : <p className={styles.loading}>这个范围暂无{group === "model" ? "模型" : "用量"}明细。</p>}
  </section>;
}

function SummaryContent({ data, metric, scope }: { data: UsageSummary; metric: "cost" | "tokens"; scope: UsageScope }) {
  const [providersExpanded, setProvidersExpanded] = useState(false);
  const visibleProviders = providersExpanded ? data.providers : data.providers.slice(0, 5);
  const chartEngines = useMemo(() => [...new Set(data.series.flatMap(row => Object.keys(row.providers)))].filter(engine => data.series.some(row => row.providers[engine]?.[metric === "cost" ? "cost" : "total_tokens"] != null)), [data.series, metric]);
  const chartTitle = `${data.window.resolution === "hour" || data.window.resolution === "hourly" ? "每小时" : "每日"}${metric === "cost" ? "费用" : " Token"}`;
  const failedSources = data.sources.filter(source => ["error", "partial", "unavailable"].includes(source.status));
  return <>
    {failedSources.length ? <div className={styles.sourceWarning}><Callout tone="warning" role="status">{failedSources.map(source => engineName(source.engine)).join("、")} 的数据未完整读取，当前显示可用记录。具体原因见下方数据来源。</Callout></div> : null}
    {data.cache_warning || data.pricing?.message ? <div className={styles.sourceWarning}><Callout tone="warning" role="status">{[data.cache_warning, data.pricing?.message].filter(Boolean).join(" ")}</Callout></div> : null}
    {metric === "cost" ? <div className={styles.costOverview}><section className={styles.costSidebar} aria-label="费用概览"><span className={styles.summaryLabel}>已定价费用</span><strong className={styles.heroValue} title={exactMoney(data.totals.cost)}>{moneyValue(data.totals.cost)}</strong><p className={styles.summaryNote}>{pricingLabel(data.totals)} · {countValue(data.totals.sessions)} 个会话</p>{data.totals.unpriced ? <p className={styles.warningNote}>{countValue(data.totals.unpriced)} 条未定价记录，未计入此金额</p> : null}
      <div className={styles.providerList}>{visibleProviders.map(provider => <div key={provider.engine} className={styles.providerItem} style={{ "--usage-engine": engineColor(provider.engine) } as CSSProperties}><div className={styles.providerTop}><EngineName engine={provider.engine} /><strong title={exactMoney(provider.cost)}>{provider.records === 0 && provider.cost === null ? "暂无记录" : moneyValue(provider.cost)}</strong></div><div className={styles.providerMeta}><span>{tokenValue(provider.total_tokens)} Token</span><span>{provider.records === 0 ? statusLabel(provider.status) : pricingLabel(provider)}</span></div>{provider.message || provider.historical_scope_unknown ? <small className={styles.providerMessage}>{provider.message}{provider.historical_scope_unknown ? ` · ${countValue(provider.historical_scope_unknown)} 条历史口径未知` : ""}</small> : null}</div>)}</div>{data.providers.length > 5 ? <Button variant="ghost" size="sm" className={styles.expandProviders} onClick={() => setProvidersExpanded(value => !value)} aria-expanded={providersExpanded}>{providersExpanded ? "收起引擎列表 ↑" : `展开全部 ${data.providers.length} 个引擎 ↓`}</Button> : null}
    </section><section className={styles.trend}><div className={styles.sectionHeading}><h2>{chartTitle}</h2><span>{data.window.time_zone}</span></div><ProviderUsageChart series={data.series} metric={metric} engines={chartEngines} /></section></div> : <><TokenTotals totals={data.totals} /><section className={styles.tokenTrend}><div className={styles.sectionHeading}><h2>{chartTitle}</h2><span>{data.window.time_zone}</span></div><ProviderUsageChart series={data.series} metric={metric} engines={chartEngines} /></section></>}
    <Coverage totals={data.totals} />
    <Composition totals={data.totals} metric={metric} />
    <Breakdown key={metric} data={data} metric={metric} />
    <details className={styles.sources}><summary>查看数据来源 <span>{data.sources.length} 个来源</span></summary><div>{data.pricing ? <div><span>模型价格表</span><span className={styles.sourceStatus}>{data.pricing.status === "fresh" ? "最新价格" : data.pricing.status === "cached" ? "缓存价格" : "价格不可用"}</span><p>{data.pricing.message || "用于无供应商上报金额的已知模型估算。"} {data.pricing.fetched_at !== null ? `价格更新于 ${timestampLabel(data.pricing.fetched_at)}。` : ""}{data.pricing.source ? <small className={styles.priceSource}>来源：{data.pricing.source}</small> : null}</p></div> : null}{data.cache_warning ? <div><span>历史扫描缓存</span><span className={styles.sourceStatus}>缓存异常</span><p>{data.cache_warning}</p></div> : null}{data.sources.map((source, index) => <div key={`${source.engine}-${index}`}><EngineName engine={source.engine} /><span className={styles.sourceStatus}>{statusLabel(source.status)}</span><p>{source.message || (source.status === "ok" ? "已读取当前范围内的记录。" : "当前来源没有可用记录。")}</p></div>)}</div></details>
    <p className={styles.footnote}>{data.totals.historical_scope_unknown ? "历史口径未知的原始观测仅供追溯，不计入 Token 和费用合计。 " : ""}{scope === "history" ? "引擎历史汇总本机原生记录与账户历史；各来源覆盖范围见上方说明。此范围可能包含 Muteki 内部调用，两种范围不能相加。" : "仅统计此 Muteki 实例归属明确的调用，包含辅助模型。"} {metric === "cost" ? "供应商上报与模型标价估算不代表实际账单。" : (data.totals.unclassified_input !== null && data.totals.unclassified_input > 0 ? "未缓存输入、缓存读取、缓存写入、未细分输入与输出互不重复；推理是输出的明细。" : "未缓存输入、缓存读取、缓存写入与输出互不重复；推理是输出的明细。")} 缺失数据以“—”或“未定价”显示。</p>
  </>;
}

export function UsageDashboard({ runId, competitionId, threadId, challenges = [] }: { runId?: string; competitionId?: string; threadId?: string; challenges?: ChallengeOption[] }) {
  const scoped = Boolean(runId || competitionId || threadId);
  const [preferences, setPreferences] = useState<Preferences>({ metric: scoped ? "tokens" : "limits", days: scoped ? "all" : "7", scope: scoped ? "muteki" : "history" });
  const [ready, setReady] = useState(false);
  const [startDate, setStartDate] = useState("");
  const [endDate, setEndDate] = useState("");
  const [workspace, setWorkspace] = useState("");
  const [generation, setGeneration] = useState("");
  const [model, setModel] = useState("");
  const [role, setRole] = useState("");
  const [engine, setEngine] = useState("");
  const [challengeId, setChallengeId] = useState("");
  const [ledgerOpen, setLedgerOpen] = useState(false);
  const [refresh, setRefresh] = useState(0);
  const [resource, setResource] = useState<Resource>({ key: "", loading: true, error: "", data: null });
  const lastRefresh = useRef(0);
  useEffect(() => {
    if (scoped) {
      setPreferences({ metric: "tokens", days: "all", scope: "muteki" });
    } else {
      try {
        const stored = JSON.parse(window.localStorage.getItem(STORAGE_KEY) || "null") as (Partial<Preferences> & { startDate?: string; endDate?: string }) | null;
        if (stored) {
          const allowedDays = ["1", "7", "30", "90", ...(stored.scope === "muteki" ? ["all", "custom"] : [])];
          const validDate = (value: unknown) => typeof value === "string" && /^\d{4}-\d{2}-\d{2}$/.test(value) && Number.isFinite(new Date(`${value}T00:00:00`).getTime());
          setStartDate(validDate(stored.startDate) ? stored.startDate! : "");
          setEndDate(validDate(stored.endDate) ? stored.endDate! : "");
          setPreferences({ metric: ["cost", "tokens", "limits"].includes(stored.metric || "") ? stored.metric! : "limits", days: allowedDays.includes(stored.days || "") ? stored.days! : "7", scope: stored.scope === "muteki" ? "muteki" : "history" });
        }
      } catch { /* Storage is optional; unavailable storage does not affect usage data. */ }
    }
    setReady(true);
  }, [scoped]);
  useEffect(() => {
    if (!ready || scoped) return;
    try { window.localStorage.setItem(STORAGE_KEY, JSON.stringify({ ...preferences, startDate, endDate })); } catch { /* Private browsing may disable persistence. */ }
  }, [preferences, ready, scoped, startDate, endDate]);
  const metric = scoped && preferences.metric === "limits" ? "tokens" : preferences.metric;
  const scope = scoped ? "muteki" : preferences.scope;
  const limits = metric === "limits";
  const timeZone = Intl.DateTimeFormat().resolvedOptions().timeZone;
  const invalidDates = preferences.days === "custom" && startDate && endDate && startDate > endDate;
  const query = useMemo(() => {
    const params = new URLSearchParams({ scope, days: preferences.days === "all" ? "90" : preferences.days === "custom" ? "30" : preferences.days, tz: timeZone });
    if (preferences.days === "all") params.set("start", "0");
    else if (preferences.days === "custom") {
      params.set("start", startDate ? String(new Date(`${startDate}T00:00:00`).getTime() / 1000) : "0");
      if (endDate) params.set("end", String(new Date(`${endDate}T23:59:59.999`).getTime() / 1000));
    }
    for (const [key, value] of Object.entries({ engine, run_id: runId, thread_id: threadId, competition_id: competitionId, ...(scope === "muteki" ? { challenge_id: challengeId, workspace_kind: workspace, generation, model, role } : {}) })) if (value) params.set(key, value);
    return params.toString();
  }, [scope, preferences.days, timeZone, startDate, endDate, engine, runId, threadId, competitionId, challengeId, workspace, generation, model, role]);
  const resourceKey = limits ? "limits" : query;
  useEffect(() => {
    if (!ready || (!limits && invalidDates)) return;
    const controller = new AbortController();
    let busy = false;
    const force = refresh !== lastRefresh.current;
    lastRefresh.current = refresh;
    const load = async (forceRefresh: boolean) => {
      if (busy || controller.signal.aborted) return;
      busy = true;
      setResource(previous => ({ key: resourceKey, data: previous.key === resourceKey ? previous.data : null, loading: true, error: "" }));
      try {
        const data = limits ? await fetchUsageLimits(forceRefresh, controller.signal) : await fetchUsageSummary(`${resourceKey}${forceRefresh ? "&refresh=true" : ""}`, controller.signal);
        if (!controller.signal.aborted) setResource({ key: resourceKey, data, loading: false, error: "" });
      } catch (cause) {
        if (!controller.signal.aborted) setResource(previous => ({ ...previous, loading: false, error: cause instanceof Error ? cause.message : "用量读取失败" }));
      } finally { busy = false; }
    };
    void load(force);
    const timer = limits ? null : window.setInterval(() => { if (!document.hidden) void load(false); }, scope === "muteki" ? 15_000 : 60_000);
    return () => { controller.abort(); if (timer !== null) window.clearInterval(timer); };
  }, [ready, resourceKey, limits, scope, refresh, invalidDates]);
  const data = resource.key === resourceKey && (limits || !invalidDates) ? resource.data : null;
  const summary = data && "totals" in data ? data : null;
  const limitData = data && "accounts" in data ? data : null;
  const ledgerParams = new URLSearchParams(query);
  if (summary && !["all", "custom"].includes(preferences.days)) {
    if (summary.window.since_ms !== null) ledgerParams.set("start", String(summary.window.since_ms / 1000));
    if (summary.window.until_ms !== null) ledgerParams.set("end", String(summary.window.until_ms / 1000));
  }
  const ledgerQuery = ledgerParams.toString();
  const availableEngines = [...new Set([...DEFAULT_ENGINES, ...(summary?.providers.map(provider => provider.engine) || [])])];
  const updatePreference = <Key extends keyof Preferences>(key: Key, value: Preferences[Key]) => setPreferences(previous => ({ ...previous, [key]: value }));
  const changeScope = (nextScope: UsageScope) => setPreferences(previous => ({ ...previous, scope: nextScope, days: nextScope === "history" && ["all", "custom"].includes(previous.days) ? "7" : previous.days }));
  return <section className={styles.root} data-scope={competitionId ? "competition" : runId ? "run" : threadId ? "thread" : "global"} aria-label="用量统计">
    <header className={styles.pageHeader}><div><span className={styles.eyebrow}>USAGE</span><h1>{competitionId ? "比赛 Worker 用量" : runId ? "任务用量" : threadId ? "对话用量" : "用量"}</h1></div><SegmentedControl<UsageMetric> value={metric} onChange={value => updatePreference("metric", value)} size="md" ariaLabel="用量视图" options={[{ value: "cost", label: "费用 Cost" }, { value: "tokens", label: "Token" }, ...(!scoped ? [{ value: "limits" as const, label: "额度 Limits" }] : [])]} /></header>
    <div className={styles.toolbar}>{!limits ? <><div className={styles.scopeControls}>{!scoped ? <label className={styles.inlineField}><span className={styles.srOnly}>统计范围</span><select value={scope} onChange={event => changeScope(event.target.value as UsageScope)}><option value="history">引擎历史</option><option value="muteki">Muteki 内部用量</option></select></label> : <span className={styles.scopeLabel}>{competitionId ? "仅本比赛 Worker" : runId ? "当前任务" : "当前对话"}</span>}<label className={styles.inlineField}><span className={styles.srOnly}>引擎筛选</span><select value={engine} onChange={event => setEngine(event.target.value)}><option value="">全部引擎</option>{availableEngines.map(engine => <option key={engine} value={engine}>{engineName(engine)}</option>)}</select></label></div><div className={styles.periodControls}><SegmentedControl<string> value={preferences.days} onChange={value => updatePreference("days", value as UsageDays)} size="md" ariaLabel="时间范围" options={[...PERIODS, ...(["all", "custom"].includes(preferences.days) ? [{ value: preferences.days, label: preferences.days === "all" ? "全部历史" : "自定义" }] : [])]} />{scope === "muteki" ? <label className={styles.inlineField}><span className={styles.srOnly}>其他时间范围</span><select value={["all", "custom"].includes(preferences.days) ? preferences.days : ""} onChange={event => { if (event.target.value) updatePreference("days", event.target.value as UsageDays); }}><option value="">更多时间</option><option value="all">全部历史</option><option value="custom">自定义</option></select></label> : null}</div></> : <p className={styles.toolbarNote}>订阅额度 · 独立于费用和 Token</p>}<Button variant="outline" icon="refresh" loading={resource.loading && ready} disabled={!ready || Boolean(invalidDates && !limits)} onClick={() => setRefresh(value => value + 1)}>{limits ? "刷新额度" : "刷新"}</Button></div>
    {!limits && scope === "muteki" ? <details className={styles.filters} open={preferences.days === "custom" ? true : undefined}><summary>筛选与归属{[workspace, generation, model, role, challengeId].filter(Boolean).length ? ` · ${[workspace, generation, model, role, challengeId].filter(Boolean).length} 项筛选` : ""}</summary><div>{preferences.days === "custom" ? <><label>开始日期<input type="date" value={startDate} onChange={event => setStartDate(event.target.value)} /></label><label>结束日期<input type="date" value={endDate} onChange={event => setEndDate(event.target.value)} /></label></> : null}{!scoped ? <label>工作区<select value={workspace} onChange={event => setWorkspace(event.target.value)}><option value="">全部工作区</option><option value="conversation">对话</option><option value="single-security-task">单任务</option><option value="competition">比赛</option></select></label> : null}{competitionId ? <label>题目<select value={challengeId} onChange={event => setChallengeId(event.target.value)}><option value="">全部题目</option>{challenges.map(challenge => <option key={challenge.id} value={challenge.id}>{challenge.label}</option>)}</select></label> : null}<label>模型<input value={model} placeholder="全部模型（精确匹配）" onChange={event => setModel(event.target.value)} /></label><label>角色<select value={role} onChange={event => setRole(event.target.value)}><option value="">全部角色</option>{Object.entries(ROLES).map(([value, label]) => <option key={value} value={value}>{label}</option>)}</select></label>{runId ? <label>执行批次<input type="number" min="0" step="1" value={generation} placeholder="全部执行" onChange={event => setGeneration(event.target.value)} /></label> : null}<Button size="sm" variant="ghost" onClick={() => { setWorkspace(""); setGeneration(""); setModel(""); setRole(""); setChallengeId(""); setEngine(""); }}>清除筛选</Button></div></details> : null}
    {!limits && invalidDates ? <Callout tone="danger" role="alert">结束日期不能早于开始日期。</Callout> : null}
    {resource.error && resource.key === resourceKey ? <Callout tone="danger" role="alert">{resource.error}{data ? "。当前保留上次成功结果。" : ""}</Callout> : null}
    {!data && !resource.error && (limits || !invalidDates) ? <div className={styles.skeleton} role="status" aria-label="正在读取用量"><span className={styles.srOnly}>正在读取{limits ? "额度" : "用量"}…</span><Skeleton className="h-8 w-44" /><Skeleton className="h-60 w-full rounded-lg" /><Skeleton className="h-32 w-full rounded-lg" /></div> : null}
    {summary && !limits ? <><div className={styles.contextBar}><p>{scope === "history" ? "原生记录与账户历史，按来源去重" : competitionId ? "包含失败、中断和重试；不含 Reason、标题和摘要" : "此 Muteki 实例的任务、对话及辅助调用"}</p><span>{summary.window.slots.length ? `${slotLabel(summary.window.slots[0])} — ${slotLabel(summary.window.slots[summary.window.slots.length - 1])} · ` : ""}更新于 {timestampLabel(summary.as_of)}</span></div><SummaryContent data={summary} metric={metric} scope={scope} />{scope === "muteki" ? <details className={styles.ledgerDisclosure} onToggle={event => setLedgerOpen(event.currentTarget.open)}><summary>查看调用账本与归属 <span>任务、题目、角色和执行批次</span></summary>{ledgerOpen ? <UsageLedger key={query} query={ledgerQuery} refresh={refresh} competitionId={competitionId} challenges={challenges} allowImport={!scoped} onImport={() => setRefresh(value => value + 1)} /> : null}</details> : null}</> : null}
    {limitData && limits ? <UsageLimitsView data={limitData} /> : null}
  </section>;
}
