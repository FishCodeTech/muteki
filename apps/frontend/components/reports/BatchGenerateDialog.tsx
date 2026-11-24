"use client";

import { useEffect, useMemo, useState } from "react";
import { useT } from "@/lib/i18n";
import { cn } from "@/lib/cn";
import {
  batchReports,
  fetchReportCandidates,
  getGenerationSettings,
  putGenerationSettings,
  type ReportCandidate,
  type WriteupTemplate,
} from "@/lib/reports";
import { reportErrorText as errText } from "@/lib/reportErrors";
import { Icon } from "@muteki/ui";
import { Badge, Button, Callout, Checkbox, Dialog, Select, Skeleton, toast } from "@muteki/ui";

type Group = "import" | "generate" | "unavailable";

function classify(c: ReportCandidate): { group: Group; reason?: string } {
  if (c.unavailable_reason === "running") return { group: "unavailable", reason: "running" };
  if (c.unavailable_reason === "draft") return { group: "unavailable", reason: "draft" };
  if (c.active_job) return { group: "unavailable", reason: "generating" };
  if (c.latest_writeup && (c.report_ids?.length ?? 0) > 0) return { group: "unavailable", reason: "hasReport" };
  if (c.latest_writeup) return { group: "import" };
  return { group: "generate" };
}

export function BatchGenerateDialog({ open, onClose }: { open: boolean; onClose: () => void }) {
  const t = useT();
  const [candidates, setCandidates] = useState<ReportCandidate[] | null>(null);
  const [error, setError] = useState("");
  const [checked, setChecked] = useState<Set<string>>(new Set());
  const [maxConcurrent, setMaxConcurrent] = useState(3);
  const [templates, setTemplates] = useState<WriteupTemplate[]>([]);
  const [templateId, setTemplateId] = useState("");
  const [settingsBusy, setSettingsBusy] = useState(false);
  const [starting, setStarting] = useState(false);

  useEffect(() => {
    if (!open) return;
    setCandidates(null);
    setError("");
    void fetchReportCandidates("ctf").then(
      (list) => {
        setCandidates(list);
        setChecked(new Set(list.filter((c) => classify(c).group !== "unavailable").map((c) => c.run_id)));
      },
      (err) => setError(err instanceof Error ? err.message : String(err)),
    );
    void getGenerationSettings().then(
      (s) => {
        setMaxConcurrent(s.max_concurrent);
        setTemplates(s.templates);
        setTemplateId(s.default_template_id || s.templates[0]?.id || "");
      },
      () => {},
    );
  }, [open]);

  const groups = useMemo(() => {
    const out: Record<Group, ReportCandidate[]> = { import: [], generate: [], unavailable: [] };
    const reasons = new Map<string, string>();
    for (const c of candidates ?? []) {
      const { group, reason } = classify(c);
      out[group].push(c);
      if (reason) reasons.set(c.run_id, reason);
    }
    return { out, reasons };
  }, [candidates]);

  const selectedImport = groups.out.import.filter((c) => checked.has(c.run_id));
  const selectedGenerate = groups.out.generate.filter((c) => checked.has(c.run_id));
  const total = selectedImport.length + selectedGenerate.length;

  const toggle = (runId: string, group: Group) => {
    if (group === "unavailable") return;
    setChecked((prev) => {
      const next = new Set(prev);
      if (next.has(runId)) next.delete(runId);
      else next.add(runId);
      return next;
    });
  };

  const bump = async (delta: number) => {
    const next = Math.min(8, Math.max(1, maxConcurrent + delta));
    if (next === maxConcurrent || settingsBusy) return;
    setSettingsBusy(true);
    try {
      const s = await putGenerationSettings(next);
      setMaxConcurrent(s.max_concurrent);
    } catch (err) {
      toast({ tone: "danger", title: errText(t, err) });
    } finally {
      setSettingsBusy(false);
    }
  };

  const start = async () => {
    if (starting || !total) return;
    setStarting(true);
    onClose();
    const failures: string[] = [];
    try {
      const [imports, gens] = await Promise.all([
        selectedImport.length ? batchReports("import_writeups", selectedImport.map((c) => c.run_id)) : Promise.resolve([]),
        selectedGenerate.length
          ? batchReports("generate", selectedGenerate.map((c) => c.run_id), undefined, templateId ? { templateId } : undefined)
          : Promise.resolve([]),
      ]);
      for (const r of [...imports, ...gens]) {
        if (!r.ok) {
          const name = (candidates ?? []).find((c) => c.run_id === r.run_id)?.name || r.run_id;
          failures.push(`${name}: ${r.error ? errText(t, { code: r.error.code, message: r.error.message }) : r.skipped ?? "?"}`);
        }
      }
      toast({
        tone: failures.length ? "warning" : "success",
        title: t("reports.batch.done", { n: total - failures.length, total }),
        description: failures.length ? failures.join(" · ") : undefined,
        duration: failures.length ? 9000 : 4000,
      });
    } catch (err) {
      toast({ tone: "danger", title: errText(t, err) });
    } finally {
      setStarting(false);
    }
  };

  const row = (c: ReportCandidate, group: Group) => {
    const disabled = group === "unavailable";
    const reason = groups.reasons.get(c.run_id);
    return (
      <label
        key={c.run_id}
        className={cn(
          "flex items-center gap-2.5 rounded-lg px-2 py-1.5 text-cx-sm",
          disabled ? "opacity-55" : "cursor-pointer hover:bg-cx-hover",
        )}
      >
        <Checkbox
          checked={checked.has(c.run_id)}
          disabled={disabled}
          onCheckedChange={() => toggle(c.run_id, group)}
        />
        <span className="min-w-0 flex-1 truncate font-medium text-cx-fg">{c.name || c.run_id}</span>
        {c.category && <Badge>{c.category}</Badge>}
        <Badge tone={c.solved ? "success" : "neutral"} dot={c.solved}>
          {c.solved ? t("reports.filter.solved") : t("reports.filter.unsolved")}
        </Badge>
        {reason && <span className="text-cx-xs text-cx-fg-4">{t(`reports.batch.unavailable.${reason}`)}</span>}
      </label>
    );
  };

  return (
    <Dialog
      open={open}
      onOpenChange={(o) => !o && !starting && onClose()}
      title={t("reports.batch.title")}
      icon="sparkles"
      dismissable={!starting}
      size="lg"
      footer={
        <>
          <div className="mr-auto flex items-center gap-1.5" title={t("reports.batch.concurrencyHint")}>
            <span className="text-cx-xs text-cx-fg-3">{t("reports.batch.concurrency")}</span>
            <Button size="xs" variant="outline" disabled={settingsBusy || maxConcurrent <= 1} onClick={() => void bump(-1)} aria-label="减少并发数">
              −
            </Button>
            <span className="w-6 text-center tabular-nums text-cx-sm font-medium">{maxConcurrent}</span>
            <Button size="xs" variant="outline" disabled={settingsBusy || maxConcurrent >= 8} onClick={() => void bump(1)} aria-label="增加并发数">
              +
            </Button>
            <span className="hidden text-cx-xs text-cx-fg-4 sm:inline">{t("reports.batch.concurrencyHint")}</span>
          </div>
          <Button variant="ghost" onClick={onClose} disabled={starting}>
            {t("dialog.cancel")}
          </Button>
          <Button variant="primary" loading={starting} disabled={!total} onClick={() => void start()}>
            {t("reports.batch.start", { n: total })}
          </Button>
        </>
      }
    >
      {error ? (
        <Callout tone="danger" role="alert">{error}</Callout>
      ) : candidates === null ? (
        <div className="flex flex-col gap-2 py-2">
          <Skeleton className="h-4 w-1/3" />
          <Skeleton className="h-4 w-2/3" />
          <Skeleton className="h-4 w-1/2" />
        </div>
      ) : (
        <div className="flex flex-col gap-4">
          {groups.out.import.length > 0 && (
            <section>
              <h3 className="mb-1 flex items-center gap-2 text-cx-xs font-semibold text-cx-fg-2">
                <Icon name="fileDown" size={13} className="text-cx-fg-3" />
                {t("reports.batch.groupImport", { n: groups.out.import.length })}
              </h3>
              <div className="flex flex-col">{groups.out.import.map((c) => row(c, "import"))}</div>
            </section>
          )}
          {groups.out.generate.length > 0 && (
            <section>
              <div className="mb-1.5 flex flex-wrap items-center justify-between gap-2">
                <h3 className="flex items-center gap-2 text-cx-xs font-semibold text-cx-fg-2">
                  <Icon name="sparkles" size={13} className="text-cx-fg-3" />
                  {t("reports.batch.groupGenerate", { n: groups.out.generate.length })}
                  <span className="font-normal text-cx-warning">{t("reports.batch.quotaHint")}</span>
                </h3>
                {templates.length > 0 && (
                  <div className="flex items-center gap-2">
                    <span className="text-cx-xs text-cx-fg-3">生成模板</span>
                    <Select
                      size="sm"
                      value={templateId}
                      onChange={setTemplateId}
                      ariaLabel="生成模板"
                      className="max-w-[220px]"
                      options={templates.map((tpl) => ({
                        value: tpl.id,
                        label: tpl.name,
                        description: tpl.description,
                      }))}
                    />
                  </div>
                )}
              </div>
              <div className="flex flex-col">{groups.out.generate.map((c) => row(c, "generate"))}</div>
            </section>
          )}
          {groups.out.unavailable.length > 0 && (
            <section>
              <h3 className="mb-1 text-cx-xs font-semibold text-cx-fg-3">
                {t("reports.batch.groupUnavailable", { n: groups.out.unavailable.length })}
              </h3>
              <div className="flex flex-col">{groups.out.unavailable.map((c) => row(c, "unavailable"))}</div>
            </section>
          )}
          {!candidates.length && <p className="py-4 text-center text-cx-fg-3">{t("reports.batch.empty")}</p>}
        </div>
      )}
    </Dialog>
  );
}
