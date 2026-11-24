"use client";

import { useEffect, useMemo, useRef, useState } from "react";
import { useT } from "@/lib/i18n";
import { cn } from "@/lib/cn";
import {
  buildImportRows,
  filesFromEntry,
  rowMarkdown,
  IMPORT_MAX_BYTES,
  type ImportRow,
  type ImportStatus,
} from "@/lib/reportImport";
import { reportByteSize } from "@/lib/reportMarkdown";
import {
  createReport,
  createReportFolder,
  ReportError,
  type ReportFolder,
  type RunReport,
} from "@/lib/reports";
import { reportErrorText } from "@/lib/reportErrors";
import { folderDepth, folderPath, foldersById, pathLabel, REPORTS_MAX_DEPTH } from "@/lib/reportTree";
import { Icon } from "@muteki/ui";
import { Badge, Button, Callout, Checkbox, Dialog, Select, TextField, toast, type ListOption } from "@muteki/ui";
import type { RunSummary } from "@/lib/useRun";

interface Props {
  open: boolean;
  mode: "ctf" | "pentest";
  runs: RunSummary[];
  reports: RunReport[];
  folders: ReportFolder[];
  defaultFolderId: string | null;
  seed?: { files: { file: File; path: string }[]; entries: FileSystemEntry[] };
  onClose: () => void;
  onImported: () => void;
  onSelectReport: (report: RunReport) => void;
}

const STATUS_TONE: Record<ImportStatus, "neutral" | "accent" | "success" | "warning" | "danger" | "running"> = {
  pending: "neutral",
  exists: "warning",
  too_large: "danger",
  importing: "running",
  imported: "success",
  failed: "danger",
  skipped: "neutral",
};

export function ImportDialog({ open, mode, runs, reports, folders, defaultFolderId, seed, onClose, onImported, onSelectReport }: Props) {
  const t = useT();
  const [rows, setRows] = useState<ImportRow[]>([]);
  const [localFolders, setLocalFolders] = useState<ReportFolder[]>([]);
  const [busy, setBusy] = useState(false);
  const fileInput = useRef<HTMLInputElement>(null);
  const dirInput = useRef<HTMLInputElement>(null);

  const allFolders = useMemo(() => [...folders, ...localFolders], [folders, localFolders]);
  const byId = useMemo(() => foldersById(allFolders), [allFolders]);
  const folderOptions = useMemo<ListOption<string>[]>(() => {
    const opts: ListOption<string>[] = [{ value: "__root__", label: t("reports.import.root") }];
    for (const f of allFolders) {
      opts.push({ value: f.id, label: pathLabel(folderPath(f.id, byId), t("reports.import.root")) });
    }
    const newDirs = new Set<string>();
    for (const r of rows) if (r.dirParts.length) newDirs.add(r.dirParts.join("/"));
    for (const d of [...newDirs].sort()) {
      opts.push({ value: `__new__${d}`, label: `${t("reports.import.newPrefix")} ${d}` });
    }
    return opts;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [allFolders, byId, rows, t]);

  const runOptions = useMemo<ListOption<string>[]>(
    () => [
      { value: "__none__", label: t("reports.import.noRun") },
      ...runs.map((r) => ({ value: r.run_id, label: r.name || r.run_id })),
    ],
    [runs, t],
  );

  const patch = (key: string, update: Partial<ImportRow>) =>
    setRows((prev) => prev.map((r) => (r.key === key ? { ...r, ...update } : r)));

  const ingest = async (picked: { file: File; path: string }[], explicit: boolean, nonMarkdown?: { file: File; path: string }[]) => {
    const built = await buildImportRows(picked, {
      reports,
      runs,
      existingFolderId: defaultFolderId,
      explicit,
      nonMarkdown,
    });
    setRows((prev) => {
      const seen = new Set(prev.map((r) => r.key));
      return [...prev, ...built.filter((r) => !seen.has(r.key))];
    });
  };

  const seeded = useRef(false);
  useEffect(() => {
    if (!open || !seed || seeded.current) return;
    if (!seed.files.length && !seed.entries.length) return;
    seeded.current = true;
    void (async () => {
      const files = [...seed.files];
      for (const entry of seed.entries) {
        files.push(...(await filesFromEntry(entry)));
      }
      await ingest(files, false);
    })();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, seed]);
  useEffect(() => {
    if (!open) seeded.current = false;
  }, [open]);

  const checkedRows = rows.filter((r) => r.checked && r.status !== "exists" && r.status !== "skipped");
  const failures = rows.filter((r) => r.status === "failed" || r.status === "too_large");

  const folderSelectValue = (r: ImportRow) => (r.folderId ?? "__root__");

  const effectiveParts = (r: ImportRow): { parts: string[]; flatWarn: boolean } => {
    const baseDepth = r.folderId ? folderDepth(r.folderId, byId) : 0;
    const allowed = Math.max(0, REPORTS_MAX_DEPTH - baseDepth);
    const parts = [...r.dirParts];
    if (parts.length <= allowed) return { parts, flatWarn: false };
    const head = parts.slice(0, Math.max(0, allowed - 1));
    const tail = parts.slice(Math.max(0, allowed - 1)).join("-");
    return { parts: allowed === 0 ? [] : [...head, tail], flatWarn: true };
  };

  const ensureFolder = async (r: ImportRow, folderMap: Map<string, ReportFolder>): Promise<string | null> => {
    const { parts } = effectiveParts(r);
    let parentId = r.folderId;
    for (const name of parts) {
      const existing = [...folderMap.values()].find((f) => f.parent_id === parentId && f.name === name);
      if (existing) {
        parentId = existing.id;
        continue;
      }
      const made = await createReportFolder({ mode, name, parent_id: parentId });
      folderMap.set(made.id, made);
      setLocalFolders((prev) => (prev.some((f) => f.id === made.id) ? prev : [...prev, made]));
      parentId = made.id;
    }
    return parentId;
  };

  const importRow = async (r: ImportRow, folderMap: Map<string, ReportFolder>): Promise<RunReport | null> => {
    patch(r.key, { status: "importing", error: undefined });
    try {
      const folderId = await ensureFolder(r, folderMap);
      const markdown = await rowMarkdown(r, r.embedImages);
      const bytes = reportByteSize(markdown);
      if (bytes > IMPORT_MAX_BYTES) {
        patch(r.key, {
          status: "too_large",
          error: `${(bytes / 1e6).toFixed(1)} MB`,
          detail: r.embedImages ? "embed" : undefined,
        });
        return null;
      }
      const report = await createReport({
        ...(r.linkedRunId ? { run_id: r.linkedRunId } : { mode }),
        title: r.title.trim() || r.path,
        markdown,
        status: "draft",
        source: { kind: "import", ref: null },
        import_source: { path: r.path, sha256: r.sha256 },
        folder_id: folderId,
      });
      patch(r.key, { status: "imported", checked: false });
      return report;
    } catch (error) {
      if (error instanceof ReportError && error.code === "run.report.duplicate_import") {
        patch(r.key, { status: "exists", checked: false, error: undefined });
        return null;
      }
      patch(r.key, { status: "failed", error: reportErrorText(t, error) });
      return null;
    }
  };

  const commit = async (targets?: ImportRow[]) => {
    if (busy) return;
    const queue = (targets ?? checkedRows).filter((r) => r.status !== "exists" && r.status !== "skipped" && r.status !== "imported");
    if (!queue.length) return;
    setBusy(true);
    const folderMap = new Map(allFolders.map((f) => [f.id, f]));
    const results: (RunReport | null)[] = [];
    for (let i = 0; i < queue.length; i += 4) {
      results.push(...(await Promise.all(queue.slice(i, i + 4).map((r) => importRow(r, folderMap)))));
    }
    setBusy(false);
    const imported = results.filter(Boolean).length;
    const skipped = rows.filter((r) => r.status === "exists" || r.status === "skipped").length;
    const failed = queue.length - imported;
    toast({
      tone: failed ? "warning" : "success",
      title: t("reports.import.done", { imported, skipped, failed }),
      duration: failed ? 8000 : 4000,
    });
    const newReports = results.filter((x): x is RunReport => Boolean(x));
    onImported();
    const first = newReports[0];
    if (first && !failed) {
      onClose();
      onSelectReport(first);
    }
  };

  const checkedCount = rows.filter((r) => r.checked).length;

  return (
    <Dialog
      open={open}
      onOpenChange={(o) => !o && !busy && onClose()}
      title={t("reports.import.title")}
      icon="fileDown"
      dismissable={!busy}
      size="lg"
      footer={
        <>
          <div className="mr-auto flex items-center gap-2 text-cx-xs text-cx-fg-3">
            {failures.length > 0 && (
              <Button size="xs" variant="outline" onClick={() => void commit(failures.map((f) => (f.status === "too_large" && f.detail === "embed" ? { ...f, embedImages: false } : f)))} disabled={busy}>
                {t("reports.import.retryFailed")}
              </Button>
            )}
          </div>
          <Button variant="ghost" onClick={onClose} disabled={busy}>{t("dialog.cancel")}</Button>
          <Button variant="primary" loading={busy} disabled={!checkedCount} onClick={() => void commit()}>
            {t("reports.import.commit", { n: checkedCount })}
          </Button>
        </>
      }
    >
      <div className="flex flex-col gap-3">
        <div className="flex flex-wrap items-center gap-2">
          <Button size="sm" variant="outline" icon="fileDown" onClick={() => fileInput.current?.click()} disabled={busy}>
            {t("reports.import.pickFiles")}
          </Button>
          <Button size="sm" variant="outline" icon="folder" onClick={() => dirInput.current?.click()} disabled={busy}>
            {t("reports.import.pickFolder")}
          </Button>
          {checkedCount > 0 && (
            <>
              <Select<string>
                value={null}
                onChange={(v) => {
                  const fid = v === "__root__" ? null : v.startsWith("__new__") ? null : v;
                  setRows((prev) => prev.map((r) => (r.checked ? { ...r, folderId: fid } : r)));
                }}
                options={folderOptions}
                ariaLabel={t("reports.import.setFolder")}
                trigger={<Button size="sm" variant="outline" iconRight="chevronDown">{t("reports.import.setFolder")}</Button>}
              />
              <Button size="xs" variant="ghost" onClick={() => setRows((prev) => prev.map((r) => (r.checked ? { ...r, linkedRunId: null, suggested: false } : r)))}>
                {t("reports.import.clearRun")}
              </Button>
            </>
          )}
        </div>

        <input
          ref={fileInput}
          type="file"
          multiple
          accept=".md,.markdown"
          className="hidden"
          onChange={(e) => {
            const files = Array.from(e.target.files ?? []);
            e.target.value = "";
            void ingest(files.map((f) => ({ file: f, path: f.name })), true);
          }}
        />
        <input
          ref={dirInput}
          type="file"
          // @ts-expect-error non-standard attribute
          webkitdirectory=""
          className="hidden"
          onChange={(e) => {
            const files = Array.from(e.target.files ?? []);
            e.target.value = "";
            const md: { file: File; path: string }[] = [];
            const other: { file: File; path: string }[] = [];
            for (const f of files) {
              const rel = (f as File & { webkitRelativePath?: string }).webkitRelativePath || f.name;
              const parts = rel.split("/").filter(Boolean);
              const path = parts.length > 1 ? parts.slice(1).join("/") : parts.join("/");
              if (/\.(md|markdown)$/i.test(f.name) && !f.name.startsWith(".")) md.push({ file: f, path });
              else if (!f.name.startsWith(".") && !HIDDEN_RX.test(path)) other.push({ file: f, path });
            }
            void ingest(md, false, other);
          }}
        />

        {rows.length === 0 ? (
          <Callout tone="neutral" role="status">{t("reports.import.empty")}</Callout>
        ) : (
          <div className="flex min-h-0 flex-col gap-1" role="table" aria-label={t("reports.import.title")}>
            <div className="grid grid-cols-[auto_minmax(0,1.4fr)_minmax(0,1.2fr)_minmax(0,1fr)_minmax(0,1fr)_auto] items-center gap-2 px-1 text-cx-2xs font-medium uppercase tracking-wide text-cx-fg-4 max-md:hidden">
              <span />
              <span>{t("reports.import.colPath")}</span>
              <span>{t("reports.import.colTitle")}</span>
              <span>{t("reports.import.colRun")}</span>
              <span>{t("reports.import.colFolder")}</span>
              <span>{t("reports.import.colStatus")}</span>
            </div>
            {rows.map((r) => (
              <div
                key={r.key}
                className={cn(
                  "grid grid-cols-[auto_minmax(0,1.4fr)_minmax(0,1.2fr)_minmax(0,1fr)_minmax(0,1fr)_auto] items-center gap-2 rounded-lg border border-cx-border-subtle px-2 py-1.5 max-md:grid-cols-[auto_minmax(0,1fr)_auto]",
                  (r.status === "exists" || r.status === "skipped") && "opacity-55",
                )}
                role="row"
              >
                <Checkbox checked={r.checked} disabled={busy || r.status === "importing" || r.status === "imported"} onCheckedChange={(v) => patch(r.key, { checked: v })} />
                <span className="min-w-0 truncate font-cx-mono text-cx-xs text-cx-fg-3" title={r.path}>{r.path}</span>
                <TextField size="sm" value={r.title} aria-label={t("reports.import.colTitle")} disabled={busy} onChange={(e) => patch(r.key, { title: e.target.value })} />
                <Select<string>
                  value={r.linkedRunId ?? "__none__"}
                  onChange={(v) => patch(r.key, { linkedRunId: v === "__none__" ? null : v, suggested: false })}
                  options={runOptions}
                  searchable
                  ariaLabel={t("reports.import.colRun")}
                  className="min-w-0"
                  trigger={
                    <button type="button" className="flex h-7 min-w-0 items-center gap-1 rounded-lg border border-cx-border px-2 text-cx-xs text-cx-fg-2 hover:bg-cx-hover disabled:opacity-50" disabled={busy}>
                      <span className="truncate">{r.linkedRunId ? runs.find((x) => x.run_id === r.linkedRunId)?.name || r.linkedRunId : t("reports.import.noRun")}</span>
                      {r.suggested && <Badge tone="accent">{t("reports.import.suggested")}</Badge>}
                    </button>
                  }
                />
                <Select<string>
                  value={folderSelectValue(r)}
                  onChange={(v) => {
                    const fid = v === "__root__" || v.startsWith("__new__") ? null : v;
                    patch(r.key, { folderId: fid });
                  }}
                  options={folderOptions}
                  ariaLabel={t("reports.import.colFolder")}
                  className="min-w-0"
                  trigger={
                    <button type="button" className="flex h-7 min-w-0 items-center gap-1 rounded-lg border border-cx-border px-2 text-cx-xs text-cx-fg-2 hover:bg-cx-hover disabled:opacity-50" disabled={busy}>
                      <span className="truncate">{r.folderId ? pathLabel(folderPath(r.folderId, byId), t("reports.import.root")) : t("reports.import.root")}</span>
                      <Icon name="chevronDown" size={11} />
                    </button>
                  }
                />
                <span className="flex items-center gap-1.5">
                  <Badge tone={STATUS_TONE[r.status]} dot={r.status === "importing"}>
                    {t(`reports.import.status.${r.status}`, { n: r.missingImages.length })}
                  </Badge>
                  {r.missingImages.length > 0 && (
                    <span className="cursor-help text-cx-warning" title={r.missingImages.join("\n")}>
                      <Icon name="image" size={13} />
                    </span>
                  )}
                  {r.error && <span className="max-w-[9rem] truncate text-cx-2xs text-cx-danger" title={r.error}>{r.error}</span>}
                  {r.status === "too_large" && r.detail === "embed" && (
                    <Button size="xs" variant="ghost" onClick={() => void commit([{ ...r, embedImages: false }])} disabled={busy}>
                      {t("reports.import.noEmbed")}
                    </Button>
                  )}
                  {r.status === "failed" && (
                    <Button size="xs" variant="ghost" onClick={() => void commit([r])} disabled={busy}>{t("reports.gen.retry")}</Button>
                  )}
                </span>
              </div>
            ))}
          </div>
        )}
        {rows.some((r) => effectiveParts(r).flatWarn) && (
          <Callout tone="warning" role="status">{t("reports.import.flatWarn")}</Callout>
        )}
      </div>
    </Dialog>
  );
}

const HIDDEN_RX = /(^|\/)\.[^/]/;
