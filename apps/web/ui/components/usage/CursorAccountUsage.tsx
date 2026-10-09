"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { EngineLogo } from "@/components/EngineLogo";
import { Badge, Button, Callout, IconButton, SegmentedControl, Skeleton, Tooltip } from "@/components/chat/ui";
import { cn } from "@/lib/cn";
import {
  CURSOR_USAGE_DAYS, displayLimitWindows, fetchCursorAccountUsage, writeUsageSettings,
  type CursorAccountUsage, type CursorLimitWindow, type CursorUsageDays, type CursorUsageError, type CursorUsageTotals,
} from "@/lib/cursorAccountUsage";
import { useLang } from "@/lib/i18n";
import { formatCountdown, formatShare, formatTokens, formatUsd } from "@/lib/usageFormat";
import { UsageAreaChart } from "./UsageAreaChart";

type Metric = "cost" | "tokens";
type LoadState =
  | { status: "loading" }
  | { status: "ready"; data: CursorAccountUsage }
  | { status: "failed"; message: string };

const SOURCE_LABEL: Record<NonNullable<CursorAccountUsage["source"]>, [string, string]> = {
  keychain: ["macOS 钥匙串", "macOS Keychain"],
  auth_file: ["auth.json", "auth.json"],
  env: ["CURSOR_AUTH_TOKEN", "CURSOR_AUTH_TOKEN"],
};

const DAY_LABELS: Record<CursorUsageDays, [string, string]> = {
  1: ["24 小时", "Past 24h"],
  7: ["7 天", "7 days"],
  30: ["30 天", "30 days"],
  90: ["90 天", "90 days"],
};

const TOKEN_SEGMENTS: Array<{ key: keyof Pick<CursorUsageTotals, "uncached_input" | "cache_read" | "cache_write" | "output">; label: [string, string]; mix: number }> = [
  { key: "uncached_input", label: ["未缓存输入", "Uncached input"], mix: 60 },
  { key: "cache_read", label: ["缓存读取", "Cache read"], mix: 30 },
  { key: "cache_write", label: ["缓存写入", "Cache write"], mix: 72 },
  { key: "output", label: ["输出", "Output"], mix: 100 },
];

const segmentColor = (mix: number) => `color-mix(in oklab, var(--cx-fg) ${mix}%, transparent)`;

function slotLabel(slot: string, en: boolean): string {
  if (slot.length > 10) return slot.slice(11);
  const [, month, day] = slot.split("-").map(Number);
  if (en) return new Date(2000, month - 1, day).toLocaleDateString("en-US", { month: "short", day: "numeric" });
  return `${month}月${day}日`;
}

export function CursorAccountUsagePanel() {
  const { lang } = useLang();
  const en = lang === "en";
  const t = (zh: string, english: string) => en ? english : zh;
  const pick = (pair: [string, string]) => en ? pair[1] : pair[0];
  const [days, setDays] = useState<CursorUsageDays>(30);
  const [metric, setMetric] = useState<Metric>("cost");
  const [state, setState] = useState<LoadState>({ status: "loading" });
  const [refreshing, setRefreshing] = useState(false);
  const [enabling, setEnabling] = useState(false);
  const [enableError, setEnableError] = useState("");
  const [nowMs, setNowMs] = useState(() => Date.now());
  const request = useRef<AbortController | null>(null);

  const load = useCallback(async (refresh: boolean) => {
    request.current?.abort();
    const controller = new AbortController();
    request.current = controller;
    if (refresh) setRefreshing(true);
    else setState({ status: "loading" });
    try {
      const data = await fetchCursorAccountUsage(days, { refresh, signal: controller.signal });
      if (!controller.signal.aborted) { setState({ status: "ready", data }); setNowMs(Date.now()); }
    } catch (cause) {
      if (!controller.signal.aborted) setState({ status: "failed", message: cause instanceof Error ? cause.message : String(cause) });
    } finally {
      if (!controller.signal.aborted) setRefreshing(false);
    }
  }, [days]);

  useEffect(() => {
    void load(false);
    return () => request.current?.abort();
  }, [load]);

  const enable = async () => {
    setEnabling(true);
    setEnableError("");
    try {
      await writeUsageSettings({ cursor_account_usage_enabled: true });
      await load(true);
    } catch (cause) {
      setEnableError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setEnabling(false);
    }
  };

  const data = state.status === "ready" ? state.data : null;

  return <section aria-label={t("Cursor 账号用量", "Cursor account usage")} className="flex flex-col gap-5 rounded-xl border border-cx-border bg-cx-bg p-5">
    <header className="flex flex-wrap items-center justify-between gap-3">
      <div className="flex min-w-0 items-center gap-2.5">
        <EngineLogo engine="cursor" size={18} />
        <h2 className="text-[15px] font-medium text-cx-fg">{t("Cursor 账号用量", "Cursor account usage")}</h2>
        {data?.source ? <Badge tone="neutral" icon="lock">{pick(SOURCE_LABEL[data.source])}</Badge> : null}
        {data?.account ? <span className="font-cx-mono text-[12px] text-cx-fg-4" title={t("账号指纹（sub 的 SHA-256 前缀）", "Account fingerprint (SHA-256 prefix of sub)")}>#{data.account}</span> : null}
      </div>
      <div className="flex flex-wrap items-center gap-2">
        <SegmentedControl<Metric> size="sm" ariaLabel={t("指标", "Metric")} value={metric} onChange={setMetric}
          options={[{ value: "cost", label: t("费用", "Cost") }, { value: "tokens", label: "Token" }]} />
        <SegmentedControl<string> size="sm" ariaLabel={t("时间范围", "Time range")} value={String(days)} onChange={value => setDays(Number(value) as CursorUsageDays)}
          options={CURSOR_USAGE_DAYS.map(value => ({ value: String(value), label: pick(DAY_LABELS[value]) }))} />
        <IconButton icon="refresh" label={t("刷新", "Refresh")} disabled={refreshing || state.status === "loading"} onClick={() => void load(true)} />
      </div>
    </header>

    {state.status === "loading" ? <PanelSkeleton /> : null}
    {state.status === "failed" ? <Callout tone="danger" role="alert">{t(`无法读取 Cursor 账号用量：${state.message}`, `Could not read Cursor account usage: ${state.message}`)}</Callout> : null}

    {data?.error ? <ErrorCallout error={data.error} en={en} enabling={enabling} onEnable={() => void enable()} onRetry={() => void load(true)} /> : null}
    {enableError ? <Callout tone="danger" role="alert">{enableError}</Callout> : null}

    {data && !data.error ? <>
      <LimitsSection data={data} nowMs={nowMs} en={en} />
      <HistorySection data={data} metric={metric} en={en} />
      <p className="text-[12px] leading-5 text-cx-fg-4">
        {t("数据来自 cursor.com 账号仪表盘，包含桌面端、CLI 与后台 Agent 的全部用量，金额为 Cursor 上报值。Muteki 账本里的 Cursor 调用也包含在内，不要与上方账本合计相加。",
          "From the cursor.com account dashboard: desktop, CLI and background agents, with amounts as Cursor reports them. Muteki's own Cursor calls are included, so do not add this to the ledger totals above.")}
      </p>
    </> : null}
  </section>;
}

function PanelSkeleton() {
  return <div className="flex flex-col gap-4" aria-hidden>
    <div className="grid gap-3 md:grid-cols-2"><Skeleton className="h-20 rounded-lg" /><Skeleton className="h-20 rounded-lg" /></div>
    <div className="grid gap-6 lg:grid-cols-[minmax(0,16rem)_minmax(0,1fr)]"><Skeleton className="h-24 rounded-lg" /><Skeleton className="h-56 rounded-lg" /></div>
  </div>;
}

function ErrorCallout({ error, en, enabling, onEnable, onRetry }: {
  error: CursorUsageError; en: boolean; enabling: boolean; onEnable: () => void; onRetry: () => void;
}) {
  if (error.action === "enable_cursor_keychain") {
    return <Callout tone="accent" icon="lock" title={en ? "Cursor account usage is off" : "Cursor 账号用量未开启"}
      action={<Button size="sm" variant="primary" loading={enabling} onClick={onEnable}>{en ? "Enable" : "开启"}</Button>}>
      {en
        ? "Muteki reads your existing Cursor CLI login from the macOS Keychain to show account history and monthly limits. macOS shows an access prompt on the Mac running the Muteki service; the token never leaves the service."
        : "开启后，Muteki 会从 macOS 钥匙串读取已有的 Cursor CLI 登录，用来显示账号历史和月度额度。macOS 会在运行 Muteki 服务的那台 Mac 上弹出授权；登录凭据只在服务端使用，不会发给浏览器。"}
    </Callout>;
  }
  return <Callout tone="warning" role="alert" action={<Button size="sm" variant="outline" icon="refresh" onClick={onRetry}>{en ? "Retry" : "重试"}</Button>}>
    {error.message}
  </Callout>;
}

function LimitsSection({ data, nowMs, en }: { data: CursorAccountUsage; nowMs: number; en: boolean }) {
  if (data.limits_error) {
    return <p className="text-[13px] text-cx-fg-3" role="status">{en ? "Monthly limits: " : "月度额度："}{data.limits_error.message}</p>;
  }
  if (!data.limits) return null;
  const windows = displayLimitWindows(data.limits.windows);
  return <div className="flex flex-col gap-2.5">
    <div className="flex items-baseline justify-between gap-3">
      <h3 className="text-[13px] font-medium text-cx-fg">{en ? "Monthly limits" : "本月额度"}</h3>
      {data.limits.resets_at ? <span className="text-[12px] tabular-nums text-cx-fg-3">
        {en ? "Resets " : "重置于 "}{new Date(data.limits.resets_at * 1000).toLocaleString(en ? "en-US" : "zh-CN", { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" })}
      </span> : null}
    </div>
    <div className={cn("grid gap-3", windows.length > 1 && "md:grid-cols-2")}>
      {windows.map(window => <LimitCard key={window.id} window={window} nowMs={nowMs} en={en} />)}
    </div>
  </div>;
}

function LimitCard({ window, nowMs, en }: { window: CursorLimitWindow; nowMs: number; en: boolean }) {
  const remaining = Math.round(100 - window.used_percent);
  return <div className="grid gap-3 rounded-lg border border-cx-border-subtle p-4 sm:grid-cols-[9rem_minmax(0,1fr)] sm:items-center">
    <div>
      <Tooltip content={en ? window.description[1] : window.description[0]}>
        <span className="cursor-help text-[12.5px] text-cx-fg-3 underline decoration-dotted underline-offset-2">{en ? window.label[1] : window.label[0]}</span>
      </Tooltip>
      <div className="mt-0.5 flex items-baseline gap-1.5">
        <span className="text-[28px] font-semibold leading-8 tabular-nums text-cx-fg">{remaining}%</span>
        <span className="text-[12.5px] text-cx-fg-3">{en ? "left" : "剩余"}</span>
      </div>
    </div>
    <div className="relative h-6 overflow-hidden rounded-md bg-cx-hover text-cx-fg"
      role="meter" aria-valuemin={0} aria-valuemax={100} aria-valuenow={remaining} aria-label={`${en ? window.label[1] : window.label[0]} ${remaining}%`}>
      <div className="absolute inset-y-0 left-0 bg-current opacity-35" style={{ width: `${remaining}%` }} />
      <div className="absolute inset-y-0 right-0 opacity-25"
        style={{ width: `${100 - remaining}%`, backgroundImage: "repeating-linear-gradient(135deg, currentColor 0 1px, transparent 1px 5px)" }} />
      <span className="absolute left-1.5 top-1/2 -translate-y-1/2 rounded-sm bg-cx-bg/85 px-1.5 text-[11px] tabular-nums text-cx-fg">{remaining}%</span>
      {window.resets_at ? <span className="absolute right-1.5 top-1/2 -translate-y-1/2 rounded-sm bg-cx-bg/85 px-1.5 text-[11px] tabular-nums text-cx-fg-2">↻ {formatCountdown(window.resets_at, nowMs)}</span> : null}
    </div>
  </div>;
}

function HistorySection({ data, metric, en }: { data: CursorAccountUsage; metric: Metric; en: boolean }) {
  if (data.history_error) return <Callout tone="warning" role="alert">{data.history_error.message}</Callout>;
  if (!data.history) return null;
  const { totals, models, series } = data.history;
  if (totals.events === 0) {
    return <p className="rounded-lg border border-dashed border-cx-border px-4 py-6 text-center text-[13px] text-cx-fg-3">{en ? "No Cursor usage in this window." : "这个时间范围内没有 Cursor 用量。"}</p>;
  }
  const costMode = metric === "cost";
  const hourly = data.window.resolution === "hour";
  const totalCost = totals.reported_cost ?? 0;
  const value = (row: CursorUsageTotals) => costMode ? row.reported_cost ?? 0 : row.total_tokens;
  const grand = costMode ? totalCost : totals.total_tokens;
  const sorted = costMode ? models : [...models].sort((a, b) => b.total_tokens - a.total_tokens);
  const peak = Math.max(0, ...sorted.map(value));
  const chartTitle = costMode
    ? (hourly ? (en ? "Hourly cost" : "每小时费用") : (en ? "Daily cost" : "每日费用"))
    : (hourly ? (en ? "Hourly processed tokens" : "每小时处理 Token") : (en ? "Daily processed tokens" : "每日处理 Token"));

  return <div className="flex flex-col gap-6">
    <div className="grid gap-6 lg:grid-cols-[minmax(0,16rem)_minmax(0,1fr)]">
      <div className="flex flex-col gap-1">
        <span className="text-[34px] font-semibold leading-10 tabular-nums text-cx-fg">
          {costMode ? (totals.reported_cost == null ? (en ? "Not reported" : "未上报") : formatUsd(totalCost)) : formatTokens(totals.total_tokens)}
        </span>
        <span className="text-[12.5px] text-cx-fg-3">
          {en ? `${totals.sessions} sessions · ${totals.events} requests` : `${totals.sessions} 个会话 · ${totals.events} 次请求`}
          {costMode ? (en ? " · reported by Cursor" : " · Cursor 上报") : ""}
        </span>
        {costMode && totals.unpriced_events > 0 ? <span className="text-[12px] text-cx-fg-4">
          {en ? `${totals.unpriced_events} requests reported no amount and are excluded.` : `${totals.unpriced_events} 次请求未上报金额，未计入总额。`}
        </span> : null}
      </div>
      <div className="flex min-w-0 flex-col gap-2">
        <h3 className="text-[13px] font-medium text-cx-fg">{chartTitle}</h3>
        <UsageAreaChart ariaLabel={chartTitle} format={costMode ? formatUsd : formatTokens}
          points={series.map(slot => ({ label: slotLabel(slot.slot, en), value: costMode ? slot.reported_cost : slot.total_tokens }))} />
      </div>
    </div>

    <div className="grid grid-cols-2 gap-4 md:grid-cols-5">
      {([
        [en ? "Processed tokens" : "处理 Token", totals.total_tokens],
        [en ? "Cache read" : "缓存读取", totals.cache_read],
        [en ? "Uncached input" : "未缓存输入", totals.uncached_input],
        [en ? "Cache write" : "缓存写入", totals.cache_write],
        [en ? "Output" : "输出", totals.output],
      ] as const).map(([label, amount]) => <div key={label}>
        <div className="text-[12px] text-cx-fg-3">{label}</div>
        <div className="text-[15px] font-medium tabular-nums text-cx-fg">{formatTokens(amount)}</div>
      </div>)}
    </div>

    <ShareBar title={en ? "Tokens by type" : "Token 构成"} en={en} total={totals.total_tokens}
      segments={TOKEN_SEGMENTS.map(segment => ({ label: en ? segment.label[1] : segment.label[0], value: totals[segment.key], color: segmentColor(segment.mix) }))} />

    <div className="flex flex-col gap-2">
      <h3 className="text-[13px] font-medium text-cx-fg">{en ? "By model" : "按模型"}</h3>
      <div className="overflow-x-auto">
        <table className="w-full min-w-[480px] text-[13px]">
          <thead>
            <tr className="border-b border-cx-border-subtle text-left text-[12px] text-cx-fg-3">
              <th className="w-8 py-2 font-normal">#</th>
              <th className="py-2 font-normal">{en ? "Model" : "模型"}</th>
              <th className="py-2 text-right font-normal">{en ? "Cost" : "金额"}</th>
              <th className="hidden py-2 text-right font-normal sm:table-cell">{en ? "Share" : "占比"}</th>
              <th className="py-2 text-right font-normal">Token</th>
            </tr>
          </thead>
          <tbody>
            {sorted.map((row, index) => <tr key={row.model} className="border-b border-cx-border-subtle last:border-b-0 hover:bg-cx-hover">
              <td className="py-2.5 tabular-nums text-cx-fg-4">{index + 1}</td>
              <td className="py-2.5">
                <div className="font-cx-mono text-[12.5px] text-cx-fg">{row.model}</div>
                <div className="mt-1.5 h-0.5 max-w-48 rounded-full bg-cx-fg" style={{ width: `${peak > 0 ? Math.max(4, (value(row) / peak) * 100) : 4}%` }} />
              </td>
              <td className="py-2.5 text-right tabular-nums text-cx-fg">{row.reported_cost == null ? (en ? "Not reported" : "未上报") : formatUsd(row.reported_cost)}</td>
              <td className="hidden py-2.5 text-right tabular-nums text-cx-fg-3 sm:table-cell">{grand > 0 ? formatShare(value(row) / grand) : "—"}</td>
              <td className="py-2.5 text-right tabular-nums text-cx-fg-2">{formatTokens(row.total_tokens)}</td>
            </tr>)}
          </tbody>
        </table>
      </div>
    </div>
  </div>;
}

function ShareBar({ title, segments, total, en }: {
  title: string; total: number; en: boolean;
  segments: Array<{ label: string; value: number; color: string }>;
}) {
  const visible = segments.filter(segment => segment.value > 0);
  if (total <= 0 || visible.length === 0) return null;
  return <div className="flex flex-col gap-2">
    <h3 className="text-[13px] font-medium text-cx-fg">{title}</h3>
    <div className="flex h-2 gap-0.5" role="img" aria-label={visible.map(segment => `${segment.label} ${formatShare(segment.value / total)}`).join(en ? ", " : "，")}>
      {visible.map(segment => <Tooltip key={segment.label} content={`${segment.label} · ${formatTokens(segment.value)} · ${formatShare(segment.value / total)}`}>
        <div className="min-w-1 rounded-[2px] first:rounded-l-full last:rounded-r-full" style={{ flex: `${segment.value} 1 0`, backgroundColor: segment.color }} />
      </Tooltip>)}
    </div>
    <div className="flex flex-wrap gap-x-4 gap-y-1 text-[12px]">
      {visible.map(segment => <span key={segment.label} className="inline-flex items-center gap-1.5">
        <span className="size-2 rounded-[2px]" style={{ backgroundColor: segment.color }} />
        <span className="text-cx-fg-3">{segment.label}</span>
        <span className="tabular-nums text-cx-fg">{formatTokens(segment.value)}</span>
      </span>)}
    </div>
  </div>;
}
