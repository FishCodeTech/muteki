"use client";

import { useEffect, useMemo, useState } from "react";
import { useT } from "@/lib/i18n";
import { cn } from "@/lib/cn";
import { fetchFollowupArtifact, type RunReport } from "@/lib/reports";
import { reportDiffPreview } from "@/lib/lineDiff";
import { Button, Callout, Dialog, Skeleton } from "@muteki/ui";

export function PendingUpdateDialog({
  report,
  busy,
  onClose,
  onApply,
  onIgnore,
}: {
  report: RunReport;
  busy: boolean;
  onClose: () => void;
  onApply: () => void;
  onIgnore: () => void;
}) {
  const t = useT();
  const [incoming, setIncoming] = useState<string | null>(null);
  const [error, setError] = useState("");

  useEffect(() => {
    const jobId = report.pending_update?.job_id;
    const runId = report.run_id;
    if (!jobId || !runId) {
      setError(t("reports.err.jobMissing"));
      return;
    }
    let cancelled = false;
    void fetchFollowupArtifact(runId, jobId).then(
      (markdown) => !cancelled && setIncoming(markdown),
      (err) => !cancelled && setError(err instanceof Error ? err.message : String(err)),
    );
    return () => {
      cancelled = true;
    };
  }, [report, t]);

  const rows = useMemo(
    () => (incoming === null ? [] : reportDiffPreview(report.markdown, incoming)),
    [report.markdown, incoming],
  );
  const stats = useMemo(() => {
    let add = 0;
    let del = 0;
    for (const row of rows ?? []) {
      if (row.kind === "add") add++;
      else if (row.kind === "del") del++;
    }
    return { add, del };
  }, [rows]);

  return (
    <Dialog
      open
      onOpenChange={(open) => !open && !busy && onClose()}
      title={t("reports.pending.diffTitle")}
      icon="sparkles"
      dismissable={!busy}
      size="lg"
      footer={
        <>
          <Button variant="ghost" onClick={onIgnore} disabled={busy}>
            {t("reports.pending.ignore")}
          </Button>
          <Button variant="primary" loading={busy} disabled={incoming === null} onClick={onApply}>
            {t("reports.pending.apply")}
          </Button>
        </>
      }
    >
      {error ? (
        <Callout tone="danger" role="alert">{error}</Callout>
      ) : incoming === null ? (
        <div className="flex flex-col gap-2 py-2">
          <Skeleton className="h-4 w-2/3" />
          <Skeleton className="h-4 w-1/2" />
          <Skeleton className="h-4 w-3/4" />
        </div>
      ) : rows === null ? (
        <Callout tone="warning" role="status">{t("reports.pending.diffTooLarge")}</Callout>
      ) : (
        <div className="flex min-h-0 flex-col gap-2">
          <div className="flex items-center gap-3 text-cx-xs text-cx-fg-3">
            <span className="text-cx-success">+{stats.add}</span>
            <span className="text-cx-danger">−{stats.del}</span>
            <span className="text-cx-fg-4">{t("reports.pending.diffHint")}</span>
          </div>
          <div
            className="cx-scroll max-h-[55vh] overflow-y-auto rounded-lg border border-cx-border-subtle bg-cx-bg-subtle py-2 font-cx-mono text-cx-xs leading-5"
            role="region"
            aria-label={t("reports.pending.diffTitle")}
          >
            {rows.map((row, i) =>
              row.kind === "gap" ? (
                <div key={i} className="px-3 py-1 text-center text-cx-fg-4">
                  ⋯ {row.count} ⋯
                </div>
              ) : (
                <div
                  key={i}
                  className={cn(
                    "whitespace-pre-wrap break-all px-3",
                    row.kind === "add" && "bg-[color-mix(in_srgb,var(--cx-success)_14%,transparent)] text-cx-success",
                    row.kind === "del" && "bg-[color-mix(in_srgb,var(--cx-danger)_12%,transparent)] text-cx-danger",
                    row.kind === "same" && "text-cx-fg-3",
                  )}
                >
                  <span className="mr-2 inline-block w-3 select-none opacity-60">
                    {row.kind === "add" ? "+" : row.kind === "del" ? "−" : " "}
                  </span>
                  {row.text}
                </div>
              ),
            )}
          </div>
        </div>
      )}
    </Dialog>
  );
}
