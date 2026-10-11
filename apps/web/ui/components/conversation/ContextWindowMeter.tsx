"use client";

/**
 * C27 — ContextWindowMeter
 *
 * Displays the current context-window occupancy reported by the active
 * Runtime.  Distinguishes window usage from cumulative token consumption:
 *   - limit=null → "上限未知" (never fabricated from cumulative totals)
 *   - a Runtime without the ``context_usage`` capability shows "此引擎不支持"
 *   - the compact button runs the Runtime's own ``/compact`` as a turn; its
 *     availability comes from the live command catalog
 */

import React, { useEffect, useRef, useState } from "react";
import { cn } from "@/lib/cn";
import { Icon } from "@/components/Icon";
import { Badge, Button, Popover, type Tone } from "@/components/chat/ui";
import type { ContextWindowState } from "@/lib/useConversation";
import { requestThreadCompact } from "@/lib/useConversation";
import { fetchComposerCapabilities, type ComposerCapabilityContext } from "@/lib/composerCapabilities";
import { compactNumber, relativeTime } from "../chat/composer/format";
import { ProgressRing } from "../chat/composer/ProgressRing";

const STATUS_LABEL: Record<ContextWindowState["compact_status"], string> = {
  idle: "",
  running: "压缩中…",
  done: "压缩完成",
  failed: "压缩失败",
  interrupted: "压缩已中断",
};

const STATUS_TONE: Record<ContextWindowState["compact_status"], Tone> = {
  idle: "neutral",
  running: "warning",
  done: "success",
  failed: "danger",
  interrupted: "neutral",
};

type Level = "unknown" | "ok" | "warn" | "danger";

function levelOf(pct: number | null): Level {
  if (pct === null) return "unknown";
  if (pct >= 95) return "danger";
  if (pct >= 80) return "warn";
  return "ok";
}

const LEVEL_TEXT: Record<Level, string> = {
  unknown: "text-cx-fg-4",
  ok: "text-cx-fg-3",
  warn: "text-cx-warning",
  danger: "text-cx-danger",
};

const LEVEL_BAR: Record<Level, string> = {
  unknown: "bg-cx-border-strong",
  ok: "bg-cx-accent",
  warn: "bg-cx-warning",
  danger: "bg-cx-danger",
};

export interface ContextWindowMeterProps {
  threadId: string;
  contextWindow: ContextWindowState | null | undefined;
  /** Runtime ``context_usage`` capability; undefined while descriptors load. */
  usageSupported?: boolean;
  /** Live command catalog of the thread's Runtime; resolves ``/compact``. */
  capabilityContext?: ComposerCapabilityContext;
  onRefresh?: () => void;
  /** "card" (default) renders the full panel; "ring" a compact toolbar gauge with a hover popover. */
  variant?: "card" | "ring";
  className?: string;
}

type CompactCommand =
  | { state: "loading" }
  | { state: "available" }
  | { state: "unavailable"; reason: string }
  | { state: "error"; reason: string };

function useCompactCommand(context: ComposerCapabilityContext | undefined): CompactCommand {
  const [command, setCommand] = useState<CompactCommand>({ state: "loading" });
  const key = context ? JSON.stringify(context) : "";
  useEffect(() => {
    if (!context) {
      setCommand({ state: "unavailable", reason: "当前对话还没有可用的 Agent Runtime" });
      return;
    }
    const controller = new AbortController();
    setCommand({ state: "loading" });
    fetchComposerCapabilities(context, "/", "compact", controller.signal).then((catalog) => {
      const item = catalog.items.find((row) => row.kind === "command" && row.name.toLowerCase() === "compact");
      if (!item) setCommand({ state: "unavailable", reason: "此引擎没有公布 /compact 命令" });
      else if (item.invocable === false) setCommand({ state: "unavailable", reason: item.reason || "/compact 当前不可执行" });
      else setCommand({ state: "available" });
    }).catch((err: unknown) => {
      if (controller.signal.aborted) return;
      setCommand({ state: "error", reason: err instanceof Error ? err.message : "命令目录加载失败" });
    });
    return () => controller.abort();
    // The serialized key carries every field of the context.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key]);
  return command;
}

function useCompaction(threadId: string, onRefresh?: () => void) {
  const [compacting, setCompacting] = useState(false);
  const [compactError, setCompactError] = useState("");
  const compact = async () => {
    setCompactError("");
    setCompacting(true);
    try {
      await requestThreadCompact(threadId);
      onRefresh?.();
    } catch (err) {
      setCompactError(err instanceof Error ? err.message : "压缩请求失败");
    } finally {
      setCompacting(false);
    }
  };
  return { compacting, compactError, compact };
}

function MeterDetails({
  contextWindow,
  usageSupported,
  capabilityContext,
  compacting,
  compactError,
  onCompact,
}: {
  contextWindow: ContextWindowState | null | undefined;
  usageSupported: boolean | undefined;
  capabilityContext: ComposerCapabilityContext | undefined;
  compacting: boolean;
  compactError: string;
  onCompact: () => void;
}) {
  const compactCommand = useCompactCommand(capabilityContext);
  const total = contextWindow?.total ?? null;
  const limit = contextWindow?.limit ?? null;
  const compactStatus = contextWindow?.compact_status ?? "idle";
  const compacted = contextWindow?.compacted ?? [];
  const zones = contextWindow?.zones ?? [];
  const pct = total != null && limit != null && limit > 0
    ? Math.min(100, Math.round((total / limit) * 100))
    : null;
  const level = levelOf(pct);
  const unsupported = usageSupported === false && total == null && limit == null;
  const running = compacting || compactStatus === "running";
  const compactReason = compactCommand.state === "unavailable" || compactCommand.state === "error"
    ? compactCommand.reason : "";

  return (
    <div className="flex flex-col gap-3">
      <div className="flex items-center justify-between gap-2">
        <h3 className="text-[13px] font-semibold text-cx-fg">上下文窗口</h3>
        <div className="flex items-center gap-2">
          {compactStatus !== "idle" ? (
            <Badge tone={STATUS_TONE[compactStatus]}>{STATUS_LABEL[compactStatus]}</Badge>
          ) : null}
          {contextWindow?.updated_at ? (
            <span className="text-[12px] text-cx-fg-4">{relativeTime(contextWindow.updated_at)}更新</span>
          ) : null}
        </div>
      </div>

      <div className="flex flex-col gap-1.5">
        <div className="flex items-baseline justify-between text-[12px]">
          <span className="cx-tabular font-cx-mono text-cx-fg">
            {total != null ? compactNumber(total) : "—"} <span className="text-cx-fg-4">tok</span>
          </span>
          <span className="cx-tabular text-cx-fg-3">
            {limit != null ? `上限 ${compactNumber(limit)}` : "上限未知"}
          </span>
        </div>
        <div className="h-1.5 w-full overflow-hidden rounded-full bg-cx-hover">
          {pct != null ? (
            <div
              className={cn("h-full rounded-full transition-[width] duration-500 ease-cx-out", LEVEL_BAR[level])}
              style={{ width: `${pct}%` }}
            />
          ) : null}
        </div>
        {pct != null ? (
          <p className={cn("text-[12px]", level === "ok" ? "text-cx-fg-3" : cn("font-medium", LEVEL_TEXT[level]))}>
            已占用 {pct}%{level === "danger" ? "，即将达到上限" : level === "warn" ? "，接近上限" : ""}
          </p>
        ) : null}
        {unsupported ? (
          <p className="text-[12px] text-cx-fg-4" data-testid="context-window-unsupported">此引擎不支持上报上下文用量</p>
        ) : total == null && limit == null ? (
          <p className="text-[12px] text-cx-fg-4">等待引擎在下一次回复后上报窗口用量</p>
        ) : null}
      </div>

      {zones.length > 0 ? (
        <dl className="flex flex-col gap-1 border-t border-cx-border-subtle pt-2.5">
          {zones.map((zone) => (
            <div key={zone.label} className="flex items-center justify-between text-[12px]">
              <dt className="capitalize text-cx-fg-3">{zone.label}</dt>
              <dd className="cx-tabular font-cx-mono text-cx-fg-2">{compactNumber(zone.tokens)}</dd>
            </div>
          ))}
        </dl>
      ) : null}

      <div className="flex flex-col gap-2 border-t border-cx-border-subtle pt-2.5">
          <Button
            size="sm"
            variant="secondary"
            icon="foldVertical"
            loading={running || compactCommand.state === "loading"}
            disabled={compactCommand.state !== "available" || running}
            title={compactReason || undefined}
            onClick={onCompact}
            className="w-full"
          >
            {running ? "压缩中…" : "压缩上下文（/compact）"}
          </Button>
          {compactReason ? (
            <p className={cn("text-[12px]", compactCommand.state === "error" ? "text-cx-danger" : "text-cx-fg-4")}>{compactReason}</p>
          ) : null}
          {compactError ? <p className="text-[12px] text-cx-danger">{compactError}</p> : null}
          {compacted.length > 0 ? (
            <div className="flex flex-col gap-1.5">
              <span className="text-[12px] font-medium text-cx-fg-4">压缩历史</span>
              {compacted.slice().reverse().map((rec, i) => (
                <div key={i} className="rounded-lg bg-cx-bg-subtle px-2.5 py-1.5 text-[12px] shadow-[0_0_0_1px_var(--cx-border-subtle)]">
                  <div className="flex items-center gap-1.5 text-[12px] text-cx-fg-3">
                    <span>{rec.trigger === "auto" ? "自动压缩" : "手动压缩"}</span>
                    <span className={rec.status === "failed" ? "text-cx-danger" : "text-cx-fg-4"}>
                      {rec.status === "failed" ? "失败"
                        : rec.status === "interrupted" ? "已中断"
                          : rec.confirmed === false ? "回合结束（引擎未上报压缩结果）" : "完成"}
                    </span>
                  </div>
                  {rec.summary ? <p className="mt-0.5 line-clamp-3 leading-snug text-cx-fg-2" title={rec.summary}>{rec.summary}</p> : null}
                  {rec.message ? <p className="mt-0.5 leading-snug text-cx-fg-3">{rec.message}</p> : null}
                  <div className="cx-tabular mt-0.5 flex items-center gap-2 font-cx-mono text-[12px] text-cx-fg-4">
                    {rec.tokens_before != null ? (
                      <span>{compactNumber(rec.tokens_before)} → {rec.tokens_after != null ? compactNumber(rec.tokens_after) : "?"}</span>
                    ) : null}
                    {rec.occurred_at ? <span>{relativeTime(rec.occurred_at)}</span> : null}
                  </div>
                </div>
              ))}
            </div>
          ) : null}
      </div>
    </div>
  );
}

const HOVER_OPEN_MS = 220;
const HOVER_CLOSE_MS = 180;

function ContextRingTrigger({
  contextWindow,
  usageSupported,
  children,
  className,
}: {
  contextWindow: ContextWindowState | null | undefined;
  usageSupported: boolean | undefined;
  children: React.ReactNode;
  className?: string;
}) {
  const [open, setOpen] = useState(false);
  const timer = useRef<number | undefined>(undefined);
  const total = contextWindow?.total ?? null;
  const limit = contextWindow?.limit ?? null;
  const pct = total != null && limit != null && limit > 0
    ? Math.min(100, Math.round((total / limit) * 100))
    : null;
  const level = levelOf(pct);
  const running = contextWindow?.compact_status === "running";

  useEffect(() => () => window.clearTimeout(timer.current), []);
  const schedule = (next: boolean, delay: number) => {
    window.clearTimeout(timer.current);
    timer.current = window.setTimeout(() => setOpen(next), delay);
  };
  const hoverHandlers = {
    onPointerEnter: (event: React.PointerEvent) => { if (event.pointerType === "mouse") schedule(true, open ? 0 : HOVER_OPEN_MS); },
    onPointerLeave: (event: React.PointerEvent) => { if (event.pointerType === "mouse") schedule(false, HOVER_CLOSE_MS); },
  };

  return (
    <Popover
      open={open}
      onOpenChange={(next) => { window.clearTimeout(timer.current); setOpen(next); }}
      placement="top-end"
      offset={8}
      initialFocus="none"
      ariaLabel="上下文窗口"
      className="w-[280px] p-3.5"
      trigger={(
        <button
          type="button"
          aria-label={pct != null ? `上下文窗口已占用 ${pct}%`
            : usageSupported === false && total == null ? "此引擎不支持上报上下文用量" : "上下文窗口用量"}
          data-testid="context-window-ring"
          data-level={level}
          {...hoverHandlers}
          className={cn(
            "cx-press relative grid size-8 shrink-0 place-items-center rounded-full hover:bg-cx-hover data-[state=open]:bg-cx-active",
            "outline-none focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--cx-focus)]",
            LEVEL_TEXT[level],
            className,
          )}
        >
          <ProgressRing value={running ? null : pct === null ? 0 : pct / 100} size={18} stroke={2.25} trackOpacity={level === "ok" ? 0.2 : 0.26} />
        </button>
      )}
    >
      <div {...hoverHandlers}>{children}</div>
    </Popover>
  );
}

export function ContextWindowMeter({
  threadId,
  contextWindow,
  usageSupported,
  capabilityContext,
  onRefresh,
  variant = "card",
  className,
}: ContextWindowMeterProps) {
  const { compacting, compactError, compact } = useCompaction(threadId, onRefresh);
  const details = (
    <MeterDetails
      contextWindow={contextWindow}
      usageSupported={usageSupported}
      capabilityContext={capabilityContext}
      compacting={compacting}
      compactError={compactError}
      onCompact={() => void compact()}
    />
  );

  switch (variant) {
    case "ring":
      return <ContextRingTrigger contextWindow={contextWindow} usageSupported={usageSupported} className={className}>{details}</ContextRingTrigger>;
    case "card":
      return (
        <div className={cn("rounded-xl bg-cx-elevated p-3.5 shadow-[0_0_0_1px_var(--cx-border)]", className)}>
          {details}
        </div>
      );
    default: {
      const exhaustive: never = variant;
      return exhaustive;
    }
  }
}
