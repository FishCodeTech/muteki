"use client";

import { useEffect, useState } from "react";
import Link from "next/link";
import { useT } from "@/lib/i18n";
import { cn } from "@/lib/cn";
import { type RunReport } from "@/lib/reports";
import { reportErrorInfo } from "@/lib/reportErrors";
import { Icon } from "@muteki/ui";
import { Button, Callout } from "@muteki/ui";

export function useElapsedSeconds(startedAt: number | undefined): number {
  const [, tick] = useState(0);
  useEffect(() => {
    if (!startedAt) return;
    const timer = window.setInterval(() => tick((v) => v + 1), 1000);
    return () => window.clearInterval(timer);
  }, [startedAt]);
  return startedAt ? Math.max(0, Math.floor(Date.now() / 1000 - startedAt)) : 0;
}

export function fmtElapsed(totalSeconds: number): string {
  const m = Math.floor(totalSeconds / 60);
  const s = totalSeconds % 60;
  return `${String(m).padStart(2, "0")}:${String(s).padStart(2, "0")}`;
}

const PHASE_KEYS = new Set(["starting", "investigating", "writing", "explore", "moving", "prepared", "ready"]);

function progressText(t: (key: string, vars?: Record<string, string | number>) => string, generation: NonNullable<RunReport["generation"]>): string {
  const phase = generation.progress?.phase;
  if (phase && PHASE_KEYS.has(phase)) {
    const tool = generation.progress?.tool_name;
    return tool ? t(`reports.gen.phase.${phase}Tool`, { tool }) : t(`reports.gen.phase.${phase}`);
  }
  return t("reports.ai.generatingHint");
}

export function GenerationBanner({
  report,
  busy,
  className,
  onCancel,
  onRetry,
  onDismiss,
  onApplyPending,
  onViewDiff,
  onIgnorePending,
}: {
  report: RunReport;
  busy: boolean;
  className?: string;
  onCancel: () => void;
  onRetry: () => void;
  onDismiss: () => void;
  onApplyPending: () => void;
  onViewDiff: () => void;
  onIgnorePending: () => void;
}) {
  const t = useT();
  const generation = report.generation ?? null;
  const pending = report.pending_update ?? null;
  const elapsed = useElapsedSeconds(generation && generation.state === "running" ? generation.started_at : undefined);
  const [detailOpen, setDetailOpen] = useState(false);
  const failedInfo = generation?.state === "failed" ? reportErrorInfo(generation.error ?? null) : null;
  const failedTitle = generation?.state === "failed"
    ? failedInfo
      ? t(failedInfo.titleKey)
      : generation.error?.message || t("reports.gen.failed")
    : "";

  if (!generation && !pending) return null;

  return (
    <div className={cn("flex shrink-0 flex-col gap-2 px-5 pt-3", className)}>
      {generation && generation.state !== "failed" && (
        <Callout
          tone="running"
          role="status"
          title={
            generation.state === "queued"
              ? t("reports.gen.queued", { n: generation.queue_position ?? 1 })
              : t("reports.gen.running")
          }
          action={
            <Button size="xs" variant="outline" disabled={busy} onClick={onCancel}>
              {t("reports.ai.cancel")}
            </Button>
          }
        >
          <span className="flex flex-wrap items-center gap-x-2 gap-y-0.5" title={t("reports.gen.leaveOk")}>
            <Icon name="loader" size={13} className="cx-spin" />
            {generation.state === "running" && (
              <span className="tabular-nums">{fmtElapsed(elapsed)}</span>
            )}
            <span className="min-w-0 flex-1">{progressText(t, generation)}</span>
          </span>
        </Callout>
      )}

      {generation?.state === "failed" && (
        <Callout
          tone="danger"
          role="alert"
          title={failedTitle}
          action={
            <>
              {failedInfo?.link && (
                <Link href={failedInfo.link} className="underline underline-offset-2">
                  {t(failedInfo.linkKey ?? "reports.err.openSettings")}
                </Link>
              )}
              <Button size="xs" variant="outline" onClick={onRetry} disabled={busy}>
                {t("reports.gen.retry")}
              </Button>
              <Button size="xs" variant="ghost" onClick={onDismiss} disabled={busy}>
                {t("reports.gen.dismiss")}
              </Button>
            </>
          }
        >
          <span className="flex flex-col gap-1">
            {failedInfo?.hintKey && <span>{t(failedInfo.hintKey)}</span>}
            {generation.error?.message && (
              <span>
                <button
                  type="button"
                  className="inline-flex items-center gap-1 text-cx-fg-4 underline decoration-dotted underline-offset-2"
                  onClick={() => setDetailOpen((v) => !v)}
                >
                  <Icon name="chevronDown" size={11} className={cn("transition-transform", detailOpen && "rotate-180")} />
                  {t("reports.err.detail")}
                </button>
                {detailOpen && (
                  <code className="mt-1 block max-h-32 overflow-y-auto whitespace-pre-wrap break-all rounded-md bg-cx-hover p-2 font-cx-mono text-cx-2xs">
                    {generation.error.message}
                  </code>
                )}
              </span>
            )}
          </span>
        </Callout>
      )}

      {pending && (
        <Callout
          tone="accent"
          role="status"
          title={t("reports.pending.title")}
          action={
            <>
              <Button size="xs" variant="primary" onClick={onApplyPending} disabled={busy}>
                {t("reports.pending.apply")}
              </Button>
              <Button size="xs" variant="outline" onClick={onViewDiff}>
                {t("reports.pending.diff")}
              </Button>
              <Button size="xs" variant="ghost" onClick={onIgnorePending} disabled={busy}>
                {t("reports.pending.ignore")}
              </Button>
            </>
          }
        >
          {t("reports.pending.hint", { size: `${(pending.bytes / 1024).toFixed(1)} KB` })}
        </Callout>
      )}
    </div>
  );
}
