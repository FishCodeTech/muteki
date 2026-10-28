"use client";

import React, { useEffect, useMemo, useRef, useState } from "react";
import { cn } from "@/lib/cn";
import { Popover, Tooltip } from "@/components/chat/ui";
import type { ContextWindowState, ConversationStatistics } from "@/lib/useConversation";
import { compactNumber, formatDuration } from "../chat/composer/format";
import { useConversationUsageLedger } from "@/lib/useConversationUsageLedger";
import { runUsageTokens, usageAmountDetails, type RunUsageTotals } from "@/lib/useRunUsageLedger";
import { pricingIssueMessage } from "@/lib/modelPricing";

function buildStatItems(statistics?: ConversationStatistics, totals?: RunUsageTotals | null): string[] {
  if (!statistics || statistics.turn_count < 1) return [];
  const result: string[] = [];
  const cadence = [
    `${statistics.turn_count} 轮`,
    statistics.step_count > 0 ? `${statistics.step_count} 步` : "",
  ].filter(Boolean).join(" · ");
  result.push(cadence);

  if (statistics.llm_duration_ms) {
    const label = statistics.llm_duration_source === "reported" ? "LLM" : "其他耗时（估算）";
    result.push(`${label} ${formatDuration(statistics.llm_duration_ms)}`);
  } else if (statistics.total_duration_ms) {
    result.push(`耗时 ${formatDuration(statistics.total_duration_ms)}`);
  }

  if (statistics.tool_duration_ms) {
    const label = statistics.tool_duration_source === "reported" ? "工具累计（原生上报）" : "工具累计（可能含等待）";
    result.push(`${label} ${formatDuration(statistics.tool_duration_ms)}`);
  }

  if (statistics.average_ttft_ms || statistics.tokens_per_second) {
    const timing = [
      statistics.average_ttft_ms
        ? `首 token 平均 ${formatDuration(statistics.average_ttft_ms)}`
        : "",
      statistics.tokens_per_second
        ? `${Math.round(statistics.tokens_per_second)} tok/s`
        : "",
    ].filter(Boolean).join(" · ");
    result.push(timing);
  }

  if (statistics.cache_hit_rate !== undefined) {
    result.push(`缓存命中 ${Math.round(statistics.cache_hit_rate * 100)}%`);
  }

  if (totals && totals.records > 0) {
    result.push(`处理 Token ${runUsageTokens(totals, compactNumber)}`);
    const parts = [
      totals.input_tokens != null ? `输入 ${compactNumber(totals.input_tokens)} tok（含缓存）` : "",
      totals.output_tokens != null ? `输出 ${compactNumber(totals.output_tokens)} tok` : "",
    ].filter(Boolean);
    if (parts.length) result.push(parts.join(" · "));
    return result;
  }
  const coverage = statistics.token_coverage ?? (
    statistics.input_tokens === undefined && statistics.output_tokens === undefined
      ? "missing"
      : statistics.input_tokens === undefined || statistics.output_tokens === undefined
        ? "partial"
        : "complete"
  );
  if (coverage === "missing") {
    if (statistics.turn_count >= 1) {
      result.push("Token 未上报");
    }
  } else if (
    statistics.input_tokens !== undefined
    || statistics.output_tokens !== undefined
  ) {
    const parts = [
      statistics.input_tokens !== undefined
        ? `输入 ${compactNumber(statistics.input_tokens)} tok`
        : "",
      statistics.output_tokens !== undefined
        ? `输出 ${compactNumber(statistics.output_tokens)} tok`
        : "",
      coverage === "partial" ? "数据不完整" : "",
    ].filter(Boolean);
    result.push(parts.join(" · "));
  }
  return result;
}

function contextChip(contextWindow?: ContextWindowState | null): { label: string; warn: boolean } | null {
  if (!contextWindow) return null;
  const { total, limit } = contextWindow;
  if (total == null || limit == null || limit <= 0) return null;
  const pct = Math.min(100, Math.round((total / limit) * 100));
  return { label: `ctx ${pct}%`, warn: pct >= 80 };
}

function StatsChipList({
  items,
  ctxChip,
  className,
}: {
  items: string[];
  ctxChip: { label: string; warn: boolean } | null;
  className?: string;
}) {
  return (
    <div className={cn("cx-tabular flex items-center font-cx-mono text-[12px] text-cx-fg-4", className)}>
      {items.map((item, index) => (
        <React.Fragment key={`${index}-${item}`}>
          {index > 0 ? <span aria-hidden="true" className="mx-1.5 shrink-0 text-cx-border-strong">·</span> : null}
          <span className="shrink-0">{item}</span>
        </React.Fragment>
      ))}
      {ctxChip ? (
        <>
          {items.length > 0 ? <span aria-hidden="true" className="mx-1.5 shrink-0 text-cx-border-strong">·</span> : null}
          <span className={cn("shrink-0", ctxChip.warn && "font-medium text-cx-warning")} title="当前上下文窗口占用率">
            {ctxChip.label}
          </span>
        </>
      ) : null}
    </div>
  );
}

/** Compact "tokens · duration" summary for the composer context strip; full list on hover. */
function InlineStats({ statistics, items, totals, costLabel }: { statistics: ConversationStatistics; items: string[]; totals: RunUsageTotals | null; costLabel: string }) {
  const coverage = statistics.token_coverage;
  const hasTokenFields =
    statistics.input_tokens !== undefined
    || statistics.output_tokens !== undefined;
  const tokens = (statistics.input_tokens ?? 0) + (statistics.output_tokens ?? 0);
  const tokenLabel = totals && totals.records > 0 ? totals.total_tokens == null ? "Token 未上报" : `${runUsageTokens(totals, compactNumber)} tok` : coverage === "missing"
    ? "Token 未上报"
    : hasTokenFields
      ? `${compactNumber(tokens)} tok${coverage === "partial" ? "·不完整" : ""}`
      : "";
  // Duration and turn count stay in the hover list; the strip only carries cost and volume.
  const parts = [costLabel, tokenLabel].filter(Boolean);
  return (
    <Tooltip
      placement="top-end"
      content={(
        <span className="flex flex-col gap-0.5 py-0.5">
          {items.map((item) => <span key={item} className="cx-tabular whitespace-normal break-words">{item}</span>)}
        </span>
      )}
    >
      <span
        tabIndex={0}
        role="note"
        aria-label={`会话统计：${items.join("，")}`}
        data-testid="c39-stats-inline"
        className="cx-tabular inline-flex h-7 min-w-0 cursor-default items-center gap-1 rounded-full px-1.5 text-[11px] text-cx-fg-4 outline-none hover:bg-cx-hover hover:text-cx-fg-3 focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)] sm:gap-1.5 sm:px-2 sm:text-[12px]"
      >
        {parts.map((part, index) => (
          <React.Fragment key={part}>
            {index > 0 ? <span aria-hidden="true" className="text-cx-border-strong">·</span> : null}
            <span className="whitespace-nowrap">{part}</span>
          </React.Fragment>
        ))}
      </span>
    </Tooltip>
  );
}

export function ConversationStatsBar({
  threadId = "",
  usageRevision,
  running = false,
  statistics,
  contextWindow,
  variant = "bar",
  className,
}: {
  threadId?: string;
  usageRevision?: number;
  running?: boolean;
  statistics?: ConversationStatistics;
  /** C27: context-window state for the ctx% chip */
  contextWindow?: ContextWindowState | null;
  /** "bar" (default) is the full cadence rail; "inline" a compact summary for the context strip. */
  variant?: "bar" | "inline";
  className?: string;
}) {
  const railRef = useRef<HTMLDivElement>(null);
  const [clipped, setClipped] = useState(false);
  const [menuOpen, setMenuOpen] = useState(false);

  const revision = [statistics?.turn_count, statistics?.step_count, statistics?.input_tokens, statistics?.output_tokens, statistics?.token_coverage, usageRevision, running].join(":");
  const ledger = useConversationUsageLedger(threadId, revision);
  const amount = ledger.totals ? usageAmountDetails(ledger.totals) : null;
  const costLabel = !threadId ? "" : amount ? `${ledger.totals?.estimated_cost != null && ledger.totals.cost != null ? "≈" : ""}${amount.amount}${amount.incomplete ? "*" : ""}` : ledger.error ? "费用不可用" : "费用…";
  const items = useMemo(() => {
    const result = buildStatItems(statistics, ledger.totals);
    const amount = ledger.totals ? usageAmountDetails(ledger.totals) : null;
    if (threadId) {
      result.push(amount ? `费用 ${amount.amount}${amount.source ? ` · ${amount.source}` : ""}${amount.incomplete ? ` · ${amount.incomplete}` : ""}` : ledger.error || "正在读取费用…");
      if (ledger.totals?.unpriced) result.push(`${ledger.totals.unpriced} 条记录未定价，未计入金额`);
      for (const issue of ledger.totals?.pricing_issues?.slice(0, 2) ?? []) result.push(pricingIssueMessage(issue));
      if (ledger.error && ledger.totals) result.push(`${ledger.error}，保留上次成功结果`);
    }
    return result;
  }, [statistics, ledger.totals, ledger.error, threadId]);
  const ctxChip = useMemo(() => contextChip(contextWindow), [contextWindow]);
  const fullText = items.join("  |  ");

  useEffect(() => {
    const element = railRef.current;
    if (!element) return;
    const measure = () => setClipped(element.scrollWidth > element.clientWidth + 1);
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(element);
    return () => observer.disconnect();
  }, [fullText]);

  if (variant === "inline") {
    if (!statistics || !items.length) return null;
    return <InlineStats statistics={statistics} items={items} totals={ledger.totals} costLabel={costLabel} />;
  }

  if (!items.length && !ctxChip) return null;

  return (
    <div className={cn("mt-1.5 px-3", className)} data-testid="c39-stats-bar">
      <div
        ref={railRef}
        aria-label={`会话统计：${fullText}${ctxChip ? ` · ${ctxChip.label}` : ""}`}
        title={clipped ? fullText : undefined}
        className="hidden min-h-0 overflow-hidden whitespace-nowrap min-[900px]:flex"
        data-testid="c39-stats-rail"
      >
        <StatsChipList items={items} ctxChip={ctxChip} />
      </div>

      <div className="flex items-center justify-end min-[900px]:hidden">
        <Popover
          open={menuOpen}
          onOpenChange={setMenuOpen}
          placement="top-end"
          ariaLabel="会话统计"
          className="w-[min(92vw,320px)] p-3"
          trigger={(
            <button
              type="button"
              className="cx-press inline-flex h-7 min-h-[44px] items-center gap-1.5 rounded-lg px-2 text-[12px] font-medium text-cx-fg-3 hover:bg-cx-hover hover:text-cx-fg"
              data-testid="c39-stats-more"
            >
              统计
              {ctxChip ? <span className="cx-tabular font-cx-mono text-[12px] text-cx-fg-4">{ctxChip.label}</span> : null}
            </button>
          )}
        >
          <div data-testid="c39-stats-popover">
            <div className="mb-2 text-[12px] font-semibold text-cx-fg">会话统计</div>
            <StatsChipList items={items} ctxChip={ctxChip} className="flex-wrap gap-y-1" />
          </div>
        </Popover>
      </div>
    </div>
  );
}

/** Shared chip builder for Info drawer overview (narrow alternate path). */
export function conversationStatsSummaryItems(
  statistics?: ConversationStatistics,
  contextWindow?: ContextWindowState | null,
): string[] {
  const items = buildStatItems(statistics);
  const chip = contextChip(contextWindow);
  return chip ? [...items, chip.label] : items;
}
