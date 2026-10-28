"use client";

import { Fragment, useEffect, useMemo, useRef, useState, type DragEvent, type ReactNode } from "react";
import { cn } from "@/lib/cn";
import { Icon, type IconName } from "@/components/Icon";
import {
  Button,
  Callout,
  Dialog,
  EmptyState,
  IconButton,
  Input,
  Label,
  Menu,
  MenuItem,
  SegmentedControl,
  Select,
  Switch,
  TextArea,
  TextField,
  toast,
  type Tone,
} from "@/components/chat/ui";
import { formatBytes } from "@/components/chat/surfaces/shared";
import { Dl } from "./Kit";
import { MENU_HIT } from "./parts";
import { SectionHeader, SurfaceToolbar } from "@/components/chat/surfaces/shared";
import { SurfaceCard, SurfaceRow } from "@/components/chat/surfaces/OverviewSurface";
import { PathBreadcrumb } from "@/components/chat/surfaces/PathBreadcrumb";
import { SOURCE_KINDS, sourceKindLabel, type SourceKind, type StudioConnector, type StudioRef } from "./model";
import { isSheetViewport, openRef } from "./nav";
import {
  BTN,
  BTN_SM,
  FIELD,
  Meta,
  OPTION_HIT,
  SEGMENTED_HIT,
  StudioBadge,
  StudioSectionHeader,
  displayTitle,
  useRememberedScroll,
  useViewing,
} from "./parts";
import {
  previewImport,
  readStudioState,
  refExists,
  studio,
  useStudioState,
  type ImportBatch,
  type ImportItem,
  type ImportResult,
} from "./store";
import {
  CSV_DELIMITERS,
  EMPTY_IMPORT_MAPPING,
  EMPTY_IMPORT_VALUES,
  IMPORT_ACCEPT,
  IMPORT_COMBINE_MODES,
  IMPORT_ENCODINGS,
  IMPORT_FIELDS,
  IMPORT_FORMATS,
  MAX_IMPORT_BYTES,
  combineImportDrafts,
  countImportOutcomes,
  decodeImportBytes,
  detectImportFormat,
  importFormatLabel,
  parseImport,
  resolveSourceKind,
  splitLines,
  splitPassages,
  toImportDraft,
  type ConflictPolicy,
  type CsvDelimiter,
  type ImportCombineMode,
  type ImportCounts,
  type ImportDraft,
  type ImportEncoding,
  type ImportField,
  type ImportFileError,
  type ImportFormat,
  type ImportMapping,
  type ImportOutcome,
  type ImportSettings,
  type ImportValues,
  type TxtMode,
} from "./importParsing";

const OUTCOMES: Record<ImportOutcome, { label: string; tone: Tone; icon: IconName }> = {
  created: { label: "新增", tone: "success", icon: "plus" },
  updated: { label: "更新", tone: "accent", icon: "refresh" },
  skipped: { label: "跳过", tone: "neutral", icon: "minus" },
  failed: { label: "失败", tone: "danger", icon: "circleAlert" },
};

const OUTCOME_ORDER: ImportOutcome[] = ["created", "updated", "skipped", "failed"];

const FORMAT_ICON: Record<ImportFormat, IconName> = { csv: "rows", json: "braces", txt: "file" };

const PREVIEW_LIMIT = 100;

type OutcomeFilterValue = "all" | ImportOutcome;

function countsText(counts: ImportCounts): string {
  const parts = OUTCOME_ORDER.filter((outcome) => counts[outcome]).map((outcome) => `${OUTCOMES[outcome].label} ${counts[outcome]} 条`);
  return parts.length ? parts.join("，") : "没有记录";
}

function formatImportTime(iso: string): string {
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return iso;
  const pad = (value: number) => String(value).padStart(2, "0");
  return `${date.getMonth() + 1}月${date.getDate()}日 ${pad(date.getHours())}:${pad(date.getMinutes())}`;
}

function clip(text: string, length: number): string {
  return text.length > length ? `${text.slice(0, length)}…` : text;
}

function OutcomeBadge({ outcome, className }: { outcome: ImportOutcome; className?: string }) {
  const meta = OUTCOMES[outcome];
  return <StudioBadge tone={meta.tone} icon={meta.icon} className={cn("shrink-0", className)}>{meta.label}</StudioBadge>;
}

function OutcomeFilter({ value, onChange, counts, total }: { value: OutcomeFilterValue; onChange: (value: OutcomeFilterValue) => void; counts: ImportCounts; total: number }) {
  const count = (n: number) => <span className="cx-tabular text-cx-fg-3">{n}</span>;
  return (
    <div className="max-w-full overflow-x-auto">
      <SegmentedControl<OutcomeFilterValue>
        value={value}
        onChange={onChange}
        ariaLabel="按结果筛选"
        size="md"
        className={SEGMENTED_HIT}
        options={[
          { value: "all", label: "全部", badge: count(total) },
          ...OUTCOME_ORDER.map((outcome) => ({
            value: outcome,
            label: OUTCOMES[outcome].label,
            badge: count(counts[outcome]),
            disabled: !counts[outcome] && value !== outcome,
          })),
        ]}
      />
    </div>
  );
}

function RawRecord({ text, className }: { text: string; className?: string }) {
  return (
    <pre className={cn("cx-scroll max-h-[200px] overflow-auto whitespace-pre-wrap break-all rounded-lg border border-cx-border-subtle bg-cx-bg-subtle px-3 py-2 font-cx-mono text-[12.5px] leading-5 text-cx-fg-2", className)}>
      {text || " "}
    </pre>
  );
}

// ---------------------------------------------------------------------------
// Choosing a file (with raw ArrayBuffer for multi-encoding support)
// ---------------------------------------------------------------------------

interface PickedFile {
  name: string;
  size: number;
  buffer: ArrayBuffer | null;
  text: string;
  detected: ImportFormat | null;
  error: string;
  warning: string;
}

async function readPicked(file: File, encoding: ImportEncoding = "utf-8"): Promise<PickedFile> {
  const detected = detectImportFormat(file.name, file.type);
  const base: PickedFile = { name: file.name, size: file.size, buffer: null, text: "", detected, error: "", warning: "" };
  if (!detected) return { ...base, error: "只支持 CSV、JSON、TXT 文件。" };
  if (file.size > MAX_IMPORT_BYTES) {
    return { ...base, error: `文件有 ${formatBytes(file.size)}，超过 ${formatBytes(MAX_IMPORT_BYTES)} 的上限。可以拆成几个文件分批导入。` };
  }
  try {
    const buffer = await file.arrayBuffer();
    const decoded = decodeImportBytes(buffer, encoding);
    return { ...base, buffer, text: decoded.text, error: decoded.error ?? "", warning: decoded.warning ?? "" };
  } catch {
    return { ...base, error: "无法读取这个文件，请确认它仍然存在并且没有被其他程序占用。" };
  }
}

function useFileInput(onFile: (file: File) => void) {
  const ref = useRef<HTMLInputElement | null>(null);
  const element = (
    <input
      ref={ref}
      type="file"
      accept={IMPORT_ACCEPT}
      className="hidden"
      tabIndex={-1}
      aria-hidden
      onChange={(event) => {
        const file = event.target.files?.[0];
        event.target.value = "";
        if (file) onFile(file);
      }}
    />
  );
  return { open: () => ref.current?.click(), element };
}

function droppedFile(event: DragEvent<HTMLElement>): File | null {
  const files = event.dataTransfer.files;
  if (files.length > 1) toast({ title: "一次导入一个文件", description: `已读取「${files[0]?.name ?? ""}」。`, tone: "warning" });
  return files[0] ?? null;
}

function FileDropZone({ onFile, busy }: { onFile: (file: File) => void; busy: boolean }) {
  const picker = useFileInput(onFile);
  const [over, setOver] = useState(false);
  return (
    <div
      onDragOver={(event) => {
        if (!event.dataTransfer.types.includes("Files")) return;
        event.preventDefault();
        setOver(true);
      }}
      onDragLeave={() => setOver(false)}
      onDrop={(event) => {
        event.preventDefault();
        setOver(false);
        const file = droppedFile(event);
        if (file) onFile(file);
      }}
      className={cn(
        "flex flex-col items-center gap-4 rounded-2xl border border-dashed px-6 py-10 text-center transition-colors",
        over ? "border-cx-accent-line bg-cx-accent-soft" : "border-cx-border bg-cx-bg-subtle",
      )}
      data-testid="studio-import-dropzone"
    >
      <span className="grid size-11 place-items-center rounded-2xl bg-cx-hover text-cx-fg-3">
        <Icon name="upload" size={20} />
      </span>
      <div className="flex flex-col gap-1">
        <p className="text-[15px] font-medium text-cx-fg">把文件拖到这里，或者选择文件</p>
        <p className="text-[13px] text-cx-fg-3">支持 CSV、JSON、TXT（可选 UTF-8 / GB18030 / GBK / Big5 / UTF-16 编码，支持多条记录合成为一份资料），最大 {formatBytes(MAX_IMPORT_BYTES)}。</p>
      </div>
      <Button variant="secondary" icon="folderOpen" className={BTN} loading={busy} onClick={picker.open}>选择文件</Button>
      {picker.element}
      <ul className="grid w-full max-w-[560px] grid-cols-3 gap-2 text-left max-sm:grid-cols-1">
        {[
          { icon: FORMAT_ICON.csv, title: "CSV", text: "每行一份或按唯一标识/整表合成为一份资料" },
          { icon: FORMAT_ICON.json, title: "JSON", text: "对象数组或嵌套数组，支持多对象合成一份" },
          { icon: FORMAT_ICON.txt, title: "TXT", text: "整份文件一份资产，或按空行分块" },
        ].map((item) => (
          <li key={item.title} className="flex items-start gap-2 rounded-xl bg-cx-bg px-3 py-2">
            <Icon name={item.icon} size={15} className="mt-0.5 shrink-0 text-cx-fg-3" />
            <span className="flex min-w-0 flex-col">
              <span className="text-[13px] font-medium text-cx-fg">{item.title}</span>
              <span className="text-[12.5px] leading-5 text-cx-fg-3">{item.text}</span>
            </span>
          </li>
        ))}
      </ul>
    </div>
  );
}

function ParseErrorCallout({ error, text }: { error: ImportFileError; text: string }) {
  const lineText = useMemo(() => (error.line ? splitLines(text)[error.line - 1] ?? "" : ""), [error.line, text]);
  const column = Math.max(1, error.column ?? 1);
  const from = Math.max(0, column - 1 - 40);
  const excerpt = lineText.slice(from, from + 80);
  return (
    <Callout tone="danger" title="文件解析失败" role="alert">
      <p>
        {error.message}
        {error.line ? ` 位置：第 ${error.line} 行${error.column ? `第 ${error.column} 列` : ""}。` : ""}
      </p>
      {error.line ? (
        <pre className="mt-2 overflow-x-auto rounded-lg bg-cx-bg px-2 py-1.5 font-cx-mono text-[12.5px] leading-5 text-cx-fg">
          {`${from ? "…" : ""}${excerpt}`}
          {error.column ? `\n${" ".repeat(column - 1 - from + (from ? 1 : 0))}^` : ""}
        </pre>
      ) : null}
    </Callout>
  );
}

function SettingCell({ label, hint, children }: { label: string; hint?: ReactNode; children: ReactNode }) {
  return (
    <div className="flex min-w-0 flex-col">
      <Label hint={hint}>{label}</Label>
      {children}
    </div>
  );
}

function PreviewRow({ draft, result, expanded, onToggle }: { draft: ImportDraft; result: ImportResult; expanded: boolean; onToggle: () => void }) {
  const passages = splitPassages(draft.values.body).length;
  const failed = result.outcome === "failed";
  return (
    <li className="rounded-xl border border-cx-border-subtle" data-testid="studio-import-preview-row">
      <button
        type="button"
        aria-expanded={expanded}
        onClick={onToggle}
        className="flex w-full items-start gap-3 rounded-xl px-3 py-2.5 text-left outline-none hover:bg-cx-hover focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]"
      >
        <OutcomeBadge outcome={result.outcome} className="mt-0.5" />
        <span className="flex min-w-0 flex-1 flex-col gap-0.5">
          <span className="truncate text-[14px] font-medium text-cx-fg">
            {draft.values.title || "（无标题）"}
            {draft.mergedCount && draft.mergedCount > 1 ? ` · 合成 ${draft.mergedCount} 条记录` : ""}
          </span>
          <Meta parts={[draft.location.label, draft.location.path, passages ? `${passages} 段` : null]} />
          <span className={cn("text-[13px] leading-5", failed ? "text-cx-danger" : "text-cx-fg-3")}>{result.reason}</span>
        </span>
        <Icon name="chevronDown" size={15} className={cn("mt-1 shrink-0 text-cx-fg-4 transition-transform", expanded && "rotate-180")} />
      </button>
      {expanded ? (
        <div className="flex flex-col gap-3 border-t border-cx-border-subtle px-3 py-3">
          <div className="flex flex-col gap-1">
            <span className="text-[12.5px] font-medium text-cx-fg-2">原始记录</span>
            <RawRecord text={draft.raw} />
          </div>
          <dl className="grid grid-cols-[88px_minmax(0,1fr)] gap-x-3 gap-y-1 text-[13px] leading-5">
            {IMPORT_FIELDS.map((field) => (
              <Fragment key={field.value}>
                <dt className="text-cx-fg-3">{field.label}</dt>
                <dd className="min-w-0 break-words text-cx-fg">{draft.values[field.value] ? clip(draft.values[field.value], 240) : <span className="text-cx-fg-4">空</span>}</dd>
              </Fragment>
            ))}
          </dl>
        </div>
      ) : null}
    </li>
  );
}

/**
 * Choose a local file, pick encoding, choose record-combine mode, set how its
 * fields map onto sources, and see every record's expected outcome before importing.
 */
export function ImportSourcesDialog({ open, onOpenChange, projectId }: { open: boolean; onOpenChange: (open: boolean) => void; projectId: string }) {
  const state = useStudioState();
  const [picked, setPicked] = useState<PickedFile | null>(null);
  const [busy, setBusy] = useState(false);
  const [encoding, setEncoding] = useState<ImportEncoding>("utf-8");
  const [combineMode, setCombineMode] = useState<ImportCombineMode>("per_row");
  const [combinedTitle, setCombinedTitle] = useState("");
  const [format, setFormat] = useState<ImportFormat>("csv");
  const [delimiter, setDelimiter] = useState<CsvDelimiter>("auto");
  const [header, setHeader] = useState(true);
  const [txtMode, setTxtMode] = useState<TxtMode>("whole");
  const [manual, setManual] = useState<{ key: string; mapping: ImportMapping } | null>(null);
  const [defaultKind, setDefaultKind] = useState<SourceKind>(SOURCE_KINDS[0]?.value ?? "field");
  const [defaultAuthor, setDefaultAuthor] = useState("我");
  const [conflict, setConflict] = useState<ConflictPolicy>("update");
  const [target, setTarget] = useState(projectId);
  const [filter, setFilter] = useState<OutcomeFilterValue>("all");
  const [expanded, setExpanded] = useState("");
  const loads = useRef(0);

  useEffect(() => {
    if (!open) return;
    loads.current += 1;
    setPicked(null);
    setBusy(false);
    setEncoding("utf-8");
    setCombineMode("per_row");
    setCombinedTitle("");
    setManual(null);
    setFilter("all");
    setExpanded("");
  }, [open]);

  useEffect(() => {
    if (open) setTarget(projectId);
  }, [open, projectId]);

  const onEncodingChange = (nextEncoding: ImportEncoding) => {
    setEncoding(nextEncoding);
    if (picked?.buffer) {
      const decoded = decodeImportBytes(picked.buffer, nextEncoding);
      setPicked({
        ...picked,
        text: decoded.text,
        error: decoded.error ?? "",
        warning: decoded.warning ?? "",
      });
    }
  };

  const choose = async (file: File) => {
    loads.current += 1;
    const load = loads.current;
    setBusy(true);
    const next = await readPicked(file, encoding);
    if (load !== loads.current) return;
    setBusy(false);
    setPicked(next);
    if (next.detected) setFormat(next.detected);
    setDelimiter("auto");
    setHeader(true);
    setTxtMode("whole");
    setCombineMode("per_row");
    setCombinedTitle(file.name.replace(/\.[^.]+$/, ""));
    setManual(null);
    setFilter("all");
    setExpanded("");
  };
  const onFile = (file: File) => { void choose(file); };
  const picker = useFileInput(onFile);

  const parsed = useMemo(
    () => (picked && !picked.error ? parseImport(picked.text, { format, fileName: picked.name, delimiter, header, txtMode }) : null),
    [picked, format, delimiter, header, txtMode],
  );
  const usable = parsed && !parsed.error ? parsed : null;
  const columnsKey = usable ? [format, txtMode, ...usable.columns].join("\u0001") : "";
  const mapping = manual && manual.key === columnsKey ? manual.mapping : usable?.suggested ?? EMPTY_IMPORT_MAPPING;
  const setField = (field: ImportField, column: string) => setManual({ key: columnsKey, mapping: { ...mapping, [field]: column } });

  const settings = useMemo((): ImportSettings => ({
    format,
    fileName: picked?.name ?? "",
    delimiter,
    header,
    txtMode,
    encoding,
    combineMode,
    combinedTitle,
    mapping,
    defaultKind,
    defaultAuthor,
    conflict,
  }), [combineMode, combinedTitle, conflict, defaultAuthor, defaultKind, delimiter, encoding, format, header, mapping, picked?.name, txtMode]);

  const rawDrafts = useMemo(() => (usable ? usable.records.map((record) => toImportDraft(record, mapping)) : []), [mapping, usable]);
  const drafts = useMemo(
    () => (format === "csv" || format === "json"
      ? combineImportDrafts(rawDrafts, { combineMode, combinedTitle, fileName: picked?.name })
      : rawDrafts),
    [combineMode, combinedTitle, format, picked?.name, rawDrafts],
  );
  const results = useMemo(() => previewImport(state, target, drafts, settings), [drafts, settings, state, target]);
  const counts = useMemo(() => countImportOutcomes(results), [results]);
  const rows = useMemo(
    () => drafts.flatMap((draft, index) => {
      const result = results[index];
      return result && (filter === "all" || result.outcome === filter) ? [{ draft, result }] : [];
    }),
    [drafts, filter, results],
  );

  const projectExists = state.projects.some((project) => project.id === target);
  const missingRequired = IMPORT_FIELDS.filter((field) => field.required && !mapping[field.value]);
  const canImport = Boolean(picked && usable && drafts.length && projectExists && !missingRequired.length);

  const commit = () => {
    if (!canImport || !picked) return;
    const done = studio.importFile({ projectId: target, fileName: picked.name, size: picked.size, text: picked.text, settings, drafts });
    onOpenChange(false);
    const scope = readStudioState().workspace.projectId;
    if (scope !== "all" && scope !== target) studio.setWorkspaceProject(target);
    studio.setLibraryUi({ importsOpen: true, importBatchId: done.batchId, sourceId: "", passageId: "", clueId: "" });
    toast({
      title: done.counts.failed ? `导入完成，${done.counts.failed} 条失败` : "导入完成",
      description: `${countsText(done.counts)}。支持在批次详情中整批撤销、复制分享链接或修正失败记录。`,
      tone: done.counts.failed ? "warning" : "success",
      action: { label: "整批撤销", onClick: () => { studio.undoImportBatch(done.batchId); } },
    });
  };

  const sample = rawDrafts[0]?.values ?? EMPTY_IMPORT_VALUES;
  const columnOptions = [{ value: "", label: "不导入" }, ...(usable?.columns ?? []).map((column) => ({ value: column, label: column }))];

  return (
    <Dialog
      open={open}
      onOpenChange={onOpenChange}
      title="导入文件"
      description="支持 CSV、JSON、TXT 导入，提供多编码解码（UTF-8 / GB18030 / GBK / Big5）与「多条记录合成为一份资料」能力。"
      icon="upload"
      size="xl"
      dismissable={!picked}
      testId="studio-import-dialog"
      footer={(
        <>
          {usable ? <span className="mr-auto min-w-0 text-[13px] text-cx-fg-3">预计{countsText(counts)}</span> : null}
          <Button variant="ghost" className={BTN} onClick={() => onOpenChange(false)}>取消</Button>
          <Button variant="primary" icon="upload" className={BTN} disabled={!canImport} onClick={commit}>
            {canImport ? `导入 ${drafts.length} 项` : "导入"}
          </Button>
        </>
      )}
    >
      {!picked ? <FileDropZone onFile={onFile} busy={busy} /> : (
        <div
          className="flex flex-col gap-5 pb-2"
          onDragOver={(event) => { if (event.dataTransfer.types.includes("Files")) event.preventDefault(); }}
          onDrop={(event) => {
            event.preventDefault();
            const file = droppedFile(event);
            if (file) onFile(file);
          }}
        >
          <div className="flex min-w-0 items-center gap-3 rounded-xl border border-cx-border-subtle px-3 py-2.5">
            <span className="grid size-10 shrink-0 place-items-center rounded-xl bg-cx-hover text-cx-fg-2">
              <Icon name={FORMAT_ICON[format]} size={17} />
            </span>
            <span className="flex min-w-0 flex-1 flex-col gap-0.5">
              <span className="truncate text-[15px] font-medium text-cx-fg">{picked.name}</span>
              <Meta className="flex-wrap" parts={[formatBytes(picked.size), `编码：${encoding.toUpperCase()}`, ...(usable?.notes ?? [])]} />
            </span>
            <Button variant="ghost" icon="refresh" className={BTN_SM} loading={busy} onClick={picker.open}>更换文件</Button>
            {picker.element}
          </div>

          {picked.error ? <Callout tone="danger" title="无法读取这个文件" role="alert">{picked.error}</Callout> : null}
          {picked.warning ? <Callout tone="warning">{picked.warning}</Callout> : null}
          {parsed?.error ? <ParseErrorCallout error={parsed.error} text={picked.text} /> : null}

          <section aria-label="读取方式" className="grid grid-cols-3 gap-3 max-md:grid-cols-2 max-sm:grid-cols-1">
            <SettingCell label="导入到项目">
              <Select value={projectExists ? target : null} onChange={setTarget} options={state.projects.map((project) => ({ value: project.id, label: displayTitle(project.title) }))} ariaLabel="导入到项目" className={FIELD} popoverClassName={OPTION_HIT} />
            </SettingCell>
            <SettingCell label="文件编码">
              <Select<ImportEncoding> value={encoding} onChange={onEncodingChange} options={IMPORT_ENCODINGS} ariaLabel="文件编码" className={FIELD} popoverClassName={OPTION_HIT} />
            </SettingCell>
            <SettingCell label="按此格式读取">
              <Select<ImportFormat> value={format} onChange={(value) => { setFormat(value); setFilter("all"); }} options={IMPORT_FORMATS} ariaLabel="文件格式" className={FIELD} popoverClassName={OPTION_HIT} />
            </SettingCell>
            {(format === "csv" || format === "json") ? (
              <SettingCell label="记录合成方式" hint="多条合成一份">
                <Select<ImportCombineMode>
                  value={combineMode}
                  onChange={setCombineMode}
                  options={IMPORT_COMBINE_MODES}
                  ariaLabel="记录合成方式"
                  className={FIELD}
                  popoverClassName={OPTION_HIT}
                />
              </SettingCell>
            ) : null}
            {combineMode === "single_source" && (format === "csv" || format === "json") ? (
              <SettingCell label="合成后的资料标题">
                <TextField
                  value={combinedTitle}
                  onChange={(e) => setCombinedTitle(e.target.value)}
                  placeholder="合并导入资料"
                  aria-label="合成后的资料标题"
                  className={FIELD}
                />
              </SettingCell>
            ) : null}
            {format === "csv" ? (
              <>
                <SettingCell label="分隔符">
                  <Select<CsvDelimiter> value={delimiter} onChange={setDelimiter} options={CSV_DELIMITERS} ariaLabel="分隔符" className={FIELD} popoverClassName={OPTION_HIT} />
                </SettingCell>
                <SettingCell label="表头">
                  <div className="flex h-[40px] items-center rounded-lg border border-cx-border px-3">
                    <Switch checked={header} onCheckedChange={setHeader} label="首行是列名" className="w-full" />
                  </div>
                </SettingCell>
              </>
            ) : null}
            {format === "txt" ? (
              <SettingCell label="拆分方式">
                <SegmentedControl<TxtMode>
                  value={txtMode}
                  onChange={setTxtMode}
                  ariaLabel="拆分方式"
                  size="md"
                  className={cn(SEGMENTED_HIT, "h-[40px] w-full [&>button]:flex-1")}
                  options={[{ value: "whole", label: "整份一份资产" }, { value: "blocks", label: "按空行分块" }]}
                />
              </SettingCell>
            ) : null}
            <SettingCell label="默认类型">
              <Select<SourceKind> value={defaultKind} onChange={setDefaultKind} options={SOURCE_KINDS.map((item) => ({ value: item.value, label: item.label }))} ariaLabel="默认类型" className={FIELD} popoverClassName={OPTION_HIT} />
            </SettingCell>
            <SettingCell label="默认作者">
              <TextField value={defaultAuthor} onChange={(event) => setDefaultAuthor(event.target.value)} aria-label="默认作者" placeholder="我" className={FIELD} />
            </SettingCell>
            <SettingCell label="已存在的资产">
              <SegmentedControl<ConflictPolicy>
                value={conflict}
                onChange={setConflict}
                ariaLabel="已存在的资产"
                size="md"
                className={cn(SEGMENTED_HIT, "h-[40px] w-full [&>button]:flex-1")}
                options={[{ value: "update", label: "更新" }, { value: "skip", label: "跳过" }]}
              />
            </SettingCell>
          </section>

          {usable ? (
            <section aria-label="字段映射" className="flex flex-col gap-2">
              <StudioSectionHeader title="字段映射" action={<span className="text-[13px] text-cx-fg-3">示例取自第 1 条记录</span>} />
              <div className="grid grid-cols-3 gap-3 max-md:grid-cols-2 max-sm:grid-cols-1">
                {IMPORT_FIELDS.map((field) => {
                  const missing = field.required && !mapping[field.value];
                  return (
                    <div key={field.value} className="flex min-w-0 flex-col">
                      <Label hint={field.required ? "必填" : undefined}>{field.label}</Label>
                      <Select<string>
                        value={mapping[field.value]}
                        onChange={(column) => setField(field.value, column)}
                        options={columnOptions}
                        ariaLabel={`${field.label}对应的字段`}
                        searchable={columnOptions.length > 8}
                        className={cn(FIELD, missing && "border-cx-danger")}
                        popoverClassName={OPTION_HIT}
                      />
                      <p className={cn("mt-1.5 truncate text-[12.5px]", missing ? "text-cx-danger" : "text-cx-fg-3")}>
                        {missing ? "请选择一个字段" : mapping[field.value] ? `示例：${clip(sample[field.value].replace(/\s+/g, " "), 60) || "（空）"}` : field.hint}
                      </p>
                    </div>
                  );
                })}
              </div>
            </section>
          ) : null}

          {usable ? (
            <section aria-label="预览" className="flex flex-col gap-2">
              <StudioSectionHeader
                title="逐条预览"
                count={drafts.length}
                action={combineMode !== "per_row" ? <StudioBadge tone="accent">已启用多条记录合成</StudioBadge> : undefined}
              />
              <OutcomeFilter value={filter} onChange={setFilter} counts={counts} total={drafts.length} />
              {counts.failed && !missingRequired.length ? (
                <p className="text-[13px] text-cx-fg-3">失败的记录不会进入资产库，会留在导入记录里，可以逐条修正后重试。</p>
              ) : null}
              {rows.length ? (
                <ul className="flex flex-col gap-1.5">
                  {rows.slice(0, PREVIEW_LIMIT).map(({ draft, result }) => (
                    <PreviewRow
                      key={draft.index}
                      draft={draft}
                      result={result}
                      expanded={expanded === String(draft.index)}
                      onToggle={() => setExpanded((current) => (current === String(draft.index) ? "" : String(draft.index)))}
                    />
                  ))}
                </ul>
              ) : (
                <EmptyState compact icon="filter" title="没有这类记录" />
              )}
              {rows.length > PREVIEW_LIMIT ? (
                <p className="text-[13px] text-cx-fg-3">只显示前 {PREVIEW_LIMIT} 条，导入后可以在导入记录里查看全部 {rows.length} 条。</p>
              ) : null}
            </section>
          ) : null}
        </div>
      )}
    </Dialog>
  );
}

// ---------------------------------------------------------------------------
// Correcting a failed record
// ---------------------------------------------------------------------------

function ImportFixDialog({ batch, itemId, onClose }: { batch: ImportBatch; itemId: string; onClose: () => void }) {
  const item = itemId ? batch.items.find((entry) => entry.id === itemId) : undefined;
  const [values, setValues] = useState<ImportValues>(EMPTY_IMPORT_VALUES);
  const [kind, setKind] = useState<"" | SourceKind>("");
  useEffect(() => {
    if (!item) return;
    setValues(item.values);
    setKind(resolveSourceKind(item.values.kind) ?? "");
  }, [item]);
  const set = (field: ImportField) => (event: { target: { value: string } }) => setValues((current) => ({ ...current, [field]: event.target.value }));
  const unknownKind = item && item.values.kind && resolveSourceKind(item.values.kind) === undefined ? item.values.kind : "";

  const save = () => {
    if (!item) return;
    const result = studio.fixImportItem(batch.id, item.id, { ...values, title: values.title.trim(), body: values.body.trim(), author: values.author.trim(), collectedAt: values.collectedAt.trim(), key: values.key.trim(), kind });
    if (!result) return;
    if (result.outcome === "failed") {
      toast({ title: "这条记录仍然无法导入", description: result.reason, tone: "danger" });
      return;
    }
    onClose();
    const sourceId = result.sourceId;
    toast({
      title: `已${OUTCOMES[result.outcome].label}`,
      description: result.reason,
      tone: "success",
      action: sourceId ? { label: "打开", onClick: () => openRef({ kind: "source", id: sourceId }) } : undefined,
    });
  };

  return (
    <Dialog
      open={Boolean(item)}
      onOpenChange={(next) => { if (!next) onClose(); }}
      title="修正记录"
      description="按导入时的设置重新导入这一条，其他记录不受影响。"
      icon="edit"
      size="lg"
      dismissable={false}
      testId="studio-import-fix"
      footer={(
        <>
          <Button variant="ghost" className={BTN} onClick={onClose}>取消</Button>
          <Button variant="primary" icon="retry" className={BTN} onClick={save}>保存并重试</Button>
        </>
      )}
    >
      {item ? (
        <div className="flex flex-col gap-4">
          <Callout tone="danger" title={[item.location.label, item.location.path].filter(Boolean).join(" · ")} role="alert">
            {item.reason}
          </Callout>
          <TextField label="标题" value={values.title} onChange={set("title")} className={FIELD} autoFocus />
          <TextArea label="正文" description="每行成为一个可引用的段落。" value={values.body} onChange={set("body")} className="min-h-[180px] text-[14px] leading-6" />
          <div className="grid grid-cols-2 gap-3 max-sm:grid-cols-1">
            <div className="flex min-w-0 flex-col">
              <Label>类型</Label>
              <Select<"" | SourceKind>
                value={kind}
                onChange={setKind}
                options={[{ value: "", label: `默认（${sourceKindLabel(batch.settings.defaultKind)}）` }, ...SOURCE_KINDS.map((entry) => ({ value: entry.value, label: entry.label }))]}
                ariaLabel="类型"
                className={FIELD}
                popoverClassName={OPTION_HIT}
              />
              {unknownKind ? <p className="mt-1.5 text-[12.5px] text-cx-fg-3">原值「{unknownKind}」无法识别，请重新选择。</p> : null}
            </div>
            <TextField label="作者" value={values.author} onChange={set("author")} placeholder={batch.settings.defaultAuthor || "我"} className={FIELD} />
            <TextField label="采集时间" value={values.collectedAt} onChange={set("collectedAt")} placeholder="留空时用导入当天" className={FIELD} />
            <TextField label="唯一标识" value={values.key} onChange={set("key")} placeholder="留空时按标题匹配" description="和已有资产的标识相同时，会更新那一份。" className={FIELD} />
          </div>
          <div className="flex flex-col gap-1">
            <span className="text-[12.5px] font-medium text-cx-fg-2">原始记录</span>
            <RawRecord text={item.raw} />
          </div>
        </div>
      ) : null}
    </Dialog>
  );
}

// ---------------------------------------------------------------------------
// Import history & Full Original File Viewer (No 1,500-line truncation!)
// ---------------------------------------------------------------------------

function downloadOriginal(batch: ImportBatch) {
  const type = batch.format === "json" ? "application/json" : batch.format === "csv" ? "text/csv" : "text/plain";
  const url = URL.createObjectURL(new Blob([batch.text], { type: `${type};charset=utf-8` }));
  const link = document.createElement("a");
  link.href = url;
  link.download = batch.fileName;
  document.body.appendChild(link);
  link.click();
  link.remove();
  window.setTimeout(() => URL.revokeObjectURL(url), 0);
}

function lineDomId(batchId: string, line: number) {
  return `studio-import-${batchId}-line-${line}`;
}

/**
 * Full original text viewer with line numbers, line search, jump-to-line, and
 * full file rendering (no 1,500-line cap). Failed records are tinted and annotated.
 */
function OriginalFile({ batch, focusId, focusLine, onFix }: { batch: ImportBatch; focusId: string; focusLine?: number; onFix: (itemId: string) => void }) {
  const lines = useMemo(() => splitLines(batch.text), [batch.text]);
  const focus = focusId ? batch.items.find((item) => item.id === focusId) : undefined;
  const [lineQuery, setLineQuery] = useState("");
  const [jumpInput, setJumpInput] = useState(focusLine ? String(focusLine) : "");

  const { failedLines, notes } = useMemo(() => {
    const marks = new Uint8Array(lines.length + 2);
    const byEnd = new Map<number, ImportItem[]>();
    batch.items.forEach((item) => {
      if (item.outcome !== "failed") return;
      for (let line = item.location.line; line <= item.location.endLine && line <= lines.length; line += 1) marks[line] = 1;
      byEnd.set(item.location.endLine, [...(byEnd.get(item.location.endLine) ?? []), item]);
    });
    return { failedLines: marks, notes: byEnd };
  }, [batch.items, lines.length]);

  useEffect(() => {
    const targetLine = focus?.location.line ?? focusLine;
    if (!targetLine) return;
    const frame = window.requestAnimationFrame(() => {
      document.getElementById(lineDomId(batch.id, targetLine))?.scrollIntoView({ block: "center", behavior: "smooth" });
    });
    return () => window.cancelAnimationFrame(frame);
  }, [batch.id, focus, focusLine]);

  const matchingLineNumbers = useMemo(() => {
    const q = lineQuery.trim().toLowerCase();
    const out: number[] = [];
    for (let line = 1; line <= lines.length; line += 1) {
      if (!q || (lines[line - 1] ?? "").toLowerCase().includes(q)) out.push(line);
    }
    return out;
  }, [lineQuery, lines]);

  const jumpTo = () => {
    const num = Math.max(1, Math.min(lines.length, Number(jumpInput) || 1));
    document.getElementById(lineDomId(batch.id, num))?.scrollIntoView({ block: "center", behavior: "smooth" });
  };

  return (
    <div className="flex flex-col gap-2" data-testid="studio-import-original">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <p className="text-[13px] text-cx-fg-3">
          完整原文件共 {lines.length.toLocaleString()} 行，{formatBytes(batch.size)}（无行数截断）。{batch.items.some((item) => item.outcome === "failed") ? "失败的记录标成红色，原因写在记录下方。" : "所有记录都已处理。"}
        </p>
        <div className="flex items-center gap-1.5">
          <Input
            size="md"
            value={lineQuery}
            onChange={(e) => setLineQuery(e.target.value)}
            placeholder="过滤原文件行…"
            aria-label="过滤原文件行"
            className="w-[180px] [&>input]:h-[34px] [&>input]:text-[13px]"
          />
          <Input
            size="md"
            value={jumpInput}
            onChange={(e) => setJumpInput(e.target.value)}
            onKeyDown={(e) => { if (e.key === "Enter") jumpTo(); }}
            placeholder="行号"
            aria-label="跳转到行号"
            className="w-[80px] [&>input]:h-[34px] [&>input]:text-[13px]"
          />
          <Button size="md" variant="secondary" className={BTN_SM} onClick={jumpTo}>跳转</Button>
        </div>
      </div>
      <div className="cx-scroll max-h-[540px] overflow-auto rounded-xl border border-cx-border-subtle bg-cx-bg-subtle py-2">
        <ol className="min-w-full">
          {matchingLineNumbers.map((line) => {
            const focused = Boolean((focus && line >= focus.location.line && line <= focus.location.endLine) || focusLine === line);
            return (
              <Fragment key={line}>
                <li
                  id={lineDomId(batch.id, line)}
                  className={cn(
                    "flex scroll-mt-24 gap-3 px-3",
                    failedLines[line] ? "bg-cx-danger-soft" : null,
                    focused && "bg-cx-accent-soft shadow-[inset_2px_0_0_var(--cx-accent)]",
                  )}
                >
                  <span className="cx-tabular w-10 shrink-0 select-none text-right font-cx-mono text-[12.5px] leading-6 text-cx-fg-4">{line}</span>
                  <span className="min-w-0 flex-1 whitespace-pre-wrap break-all font-cx-mono text-[13px] leading-6 text-cx-fg">{lines[line - 1] || " "}</span>
                </li>
                {(notes.get(line) ?? []).map((item) => (
                  <li key={item.id} className="flex items-start gap-2 bg-cx-danger-soft py-1.5 pl-[64px] pr-3">
                    <Icon name="circleAlert" size={14} className="mt-[3px] shrink-0 text-cx-danger" />
                    <span className="min-w-0 flex-1 text-[13px] leading-5 text-cx-fg">
                      <span className="font-medium">第 {item.index} 条失败：</span>{item.reason}
                    </span>
                    <Button variant="secondary" icon="edit" className={BTN_SM} onClick={() => onFix(item.id)}>修正</Button>
                  </li>
                ))}
              </Fragment>
            );
          })}
        </ol>
      </div>
    </div>
  );
}

function ImportItemRow({ item, onFix, onShowInFile }: { item: ImportItem; onFix: () => void; onShowInFile: () => void }) {
  const state = useStudioState();
  const target: StudioRef | null = item.sourceId ? { kind: "source", id: item.sourceId } : null;
  const linked = target && refExists(state, target) ? target : null;
  const failed = item.outcome === "failed";
  return (
    <SurfaceRow
      icon={failed ? "circleAlert" : item.outcome === "created" ? "plus" : item.outcome === "updated" ? "edit" : "minus"}
      title={item.values.title || "（无标题）"}
      subtitle={failed ? item.reason : [item.location.label, item.attempts > 1 ? `已重试 ${item.attempts - 1} 次` : null].filter(Boolean).join(" · ")}
      trailing={failed ? <button type="button" className="shrink-0 text-[12px] font-medium text-cx-accent hover:underline" onClick={onFix}>修正</button> : undefined}
      onClick={linked ? () => openRef(linked) : failed ? onShowInFile : undefined}
    />
  );
}

function ImportBatchView({ batch, active, onBack }: { batch: ImportBatch; active: boolean; onBack: () => void }) {
  const state = useStudioState();
  const ui = state.workspace.library;
  const [filter, setFilter] = useState<OutcomeFilterValue>(() => (batch.items.some((item) => item.outcome === "failed") ? "failed" : "all"));
  const [focusId, setFocusId] = useState("");
  const [fileOpen, setFileOpen] = useState(Boolean(ui.importFocusLine));
  const [fixId, setFixId] = useState("");
  const scroll = useRememberedScroll(`library-import:${batch.id}`, active);
  const counts = useMemo(() => countImportOutcomes(batch.items), [batch.items]);
  const visible = filter === "all" ? batch.items : batch.items.filter((item) => item.outcome === filter);
  const project = state.projects.find((item) => item.id === batch.projectId);
  const batchRef: StudioRef = { kind: "import", id: batch.id };

  useViewing(active, batchRef);

  const imported = useMemo(() => {
    const ids = new Set<string>();
    batch.items.forEach((item) => {
      if ((item.outcome === "created" || item.outcome === "updated") && item.sourceId && state.sources.some((source) => source.id === item.sourceId)) ids.add(item.sourceId);
    });
    return [...ids].map((id): StudioRef => ({ kind: "source", id }));
  }, [batch.items, state.sources]);

  const retryFailed = () => {
    const done = studio.retryImportItems(batch.id);
    const total = OUTCOME_ORDER.reduce((sum, outcome) => sum + done[outcome], 0);
    if (!total) return;
    toast({
      title: done.failed ? `重试了 ${total} 条，仍有 ${done.failed} 条失败` : `重试了 ${total} 条，全部成功`,
      description: `${countsText(done)}。`,
      tone: done.failed ? "warning" : "success",
    });
  };

  const citeImported = () => {
    studio.addContextMany(readStudioState().activeThreadId, imported);
    if (isSheetViewport()) toast({ title: "已带上对话", description: `${imported.length} 份导入的资产`, tone: "success", duration: 2400 });
  };

  const copyShareLink = () => {
    const params = new URLSearchParams(window.location.search);
    params.set("ws", "library");
    params.set("import", batch.id);
    if (ui.importFocusLine) params.set("line", String(ui.importFocusLine));
    const url = `${window.location.origin}${window.location.pathname}?${params.toString()}`;
    void navigator.clipboard?.writeText(url);
    toast({
      title: "已复制可分享的批次链接",
      description: `打开该链接会直接定位到导入批次「${batch.id} · ${batch.fileName}」。`,
      tone: "success",
    });
  };

  const undoBatch = () => {
    const restored = studio.undoImportBatch(batch.id);
    if (!restored) return;
    toast({
      title: `已整批撤销导入《${batch.fileName}》`,
      description: `已撤回本次批次涉及的 ${restored} 份资产改动，并记入处理记录。`,
      tone: "success",
    });
  };

  const showInFile = (itemId: string) => {
    setFocusId(itemId);
    setFileOpen(true);
  };

  return (
    <div className="flex min-h-0 flex-1 flex-col" data-testid="studio-import-batch">
      <SurfaceToolbar>
        <IconButton size="sm" icon="arrowLeft" label="返回" onClick={onBack} />
        <PathBreadcrumb path={`导入记录/${batch.id}`} rootLabel="资产" lastIsFile onSelect={(dir) => { if (dir === "") { studio.setLibraryUi({ importsOpen: false, importBatchId: "" }); } else if (dir === "导入记录") { studio.setLibraryUi({ importBatchId: "", importFocusLine: undefined }); } }} />
        <Menu
          ariaLabel="批次操作"
          className={MENU_HIT}
          trigger={<IconButton size="sm" icon="more" label="更多" />}
        >
          <MenuItem icon="share" onSelect={copyShareLink}>复制批次链接</MenuItem>
          <MenuItem icon="download" onSelect={() => downloadOriginal(batch)}>下载原文件</MenuItem>
          {imported.length ? (
            <MenuItem icon="at" onSelect={citeImported}>导入的资产带上对话（{imported.length}）</MenuItem>
          ) : null}
          {!batch.undoneAt ? <MenuItem icon="undo" danger onSelect={undoBatch}>整批撤销本次导入</MenuItem> : null}
        </Menu>
      </SurfaceToolbar>
      <div ref={scroll.ref} className="cx-scroll min-h-0 flex-1 overflow-y-auto">
        <div className="mx-auto flex max-w-[760px] flex-col gap-4 p-4 pb-10">
          <SurfaceCard className="p-3">
            <div className="flex items-start gap-3">
              <div className="min-w-0 flex-1">
                <p className="truncate text-[15px] font-semibold text-cx-fg">{batch.fileName}</p>
              </div>
              <StudioBadge tone={batch.undoneAt ? "warning" : counts.failed ? "danger" : "success"}>{batch.undoneAt ? "已撤销" : counts.failed ? `${counts.failed} 条失败` : "完成"}</StudioBadge>
            </div>
            <Dl className="mt-3" items={[
              { label: "编号", value: batch.id, mono: true },
              { label: "格式", value: importFormatLabel(batch.format) },
              { label: "大小", value: formatBytes(batch.size) },
              { label: "导入时间", value: formatImportTime(batch.importedAt) },
              { label: "项目", value: project?.title ?? "项目已不存在" },
              { label: "结果", value: `共 ${batch.items.length} 条：${countsText(counts)}` },
            ]} />
            {counts.failed ? (
              <Button size="sm" variant="primary" icon="retry" className="mt-3" onClick={retryFailed}>重试失败（{counts.failed}）</Button>
            ) : null}
          </SurfaceCard>
          {batch.connectorMeta ? (
            <Dl items={[
              { label: "来源", value: batch.connectorMeta.sourceSystem },
              { label: "查询", value: batch.connectorMeta.query, mono: true },
              { label: "完整性", value: batch.connectorMeta.completeness },
              { label: "回执", value: `${batch.connectorMeta.receiptId}（${batch.connectorMeta.costCredits} 点配额）` },
            ]} />
          ) : null}
          <div>
            <SectionHeader
              title="逐条结果"
              count={batch.items.length}
              action={(
                <SegmentedControl<OutcomeFilterValue>
                  value={filter}
                  onChange={setFilter}
                  size="sm"
                  ariaLabel="按结果筛选"
                  options={[{ value: "all" as OutcomeFilterValue, label: `全部 ${batch.items.length}` }, ...OUTCOME_ORDER.filter((o) => counts[o] > 0).map((o) => ({ value: o as OutcomeFilterValue, label: `${OUTCOMES[o].label} ${counts[o]}` }))]}
                />
              )}
            />
            <SurfaceCard>
              {visible.length ? visible.map((item) => (
                <ImportItemRow key={item.id} item={item} onFix={() => setFixId(item.id)} onShowInFile={() => showInFile(item.id)} />
              )) : <SurfaceRow icon={filter === "failed" ? "checkCircle" : "filter"} title={filter === "failed" ? "没有失败的记录" : "没有这类记录"} />}
            </SurfaceCard>
          </div>
          <div>
            <SectionHeader title="原文件" action={<button type="button" className="text-[12px] font-medium text-cx-accent hover:underline" onClick={() => setFileOpen((v) => !v)}>{fileOpen ? "收起" : "查看"}</button>} />
            {fileOpen ? <OriginalFile batch={batch} focusId={focusId} focusLine={ui.importFocusLine} onFix={setFixId} /> : null}
          </div>
        </div>
      </div>
      <ImportFixDialog batch={batch} itemId={fixId} onClose={() => setFixId("")} />
    </div>
  );
}

function BatchRow({ batch, showProject }: { batch: ImportBatch; showProject: boolean }) {
  const state = useStudioState();
  const counts = countImportOutcomes(batch.items);
  const open = () => studio.setLibraryUi({ importBatchId: batch.id });
  const batchRef: StudioRef = { kind: "import", id: batch.id };
  void batchRef;
  return (
    <SurfaceRow
      icon={batch.connectorMeta ? "plug" : FORMAT_ICON[batch.format]}
      onClick={open}
      title={<>{batch.fileName}{batch.undoneAt ? <span className="ml-1.5 text-[12px] text-cx-fg-4">已撤销</span> : null}</>}
      subtitle={[formatImportTime(batch.importedAt), showProject ? displayTitle(state.projects.find((project) => project.id === batch.projectId)?.title ?? "") : null, countsText(counts)].filter(Boolean).join(" · ")}
      trailing={counts.failed ? <span className="shrink-0 text-[12px] text-cx-danger">{counts.failed} 失败</span> : undefined}
    />
  );
}

/**
 * External Sources & Connectors Panel: shows capability manifests, quotas,
 * query syntax, and runs simulated connector sync into an ImportBatch.
 */
export function ConnectorsPanel({ projectId }: { projectId: string }) {
  const state = useStudioState();
  const [queries, setQueries] = useState<Record<string, string>>({});

  const triggerSync = (conn: StudioConnector) => {
    const targetProject = projectId === "all" ? state.projects[0]?.id ?? "" : projectId;
    const done = studio.runConnectorSync(conn.id, targetProject, queries[conn.id] ?? conn.defaultQuery);
    if (!done) return;
    studio.setLibraryUi({ importsOpen: true, importBatchId: done.batchId });
    toast({
      title: `已从「${conn.name}」同步资产`,
      description: `生成批次 ${done.batchId}：${countsText(done.counts)}。`,
      tone: "success",
    });
  };

  return (
    <div className="flex flex-col gap-3 px-3 pt-2" data-testid="studio-connectors-panel">
      <p className="text-[13px] text-cx-fg-3">连接器声明能力清单与配额；「同步拉取」会生成可追溯的导入批次。</p>
      <div className="flex flex-col divide-y divide-cx-border-subtle">
        {state.connectors.map((conn) => (
          <div key={conn.id} className="flex flex-col gap-1.5 py-3">
            <div className="flex flex-wrap items-center justify-between gap-2">
              <div className="flex min-w-0 items-center gap-2">
                <span className="truncate text-[14px] font-medium text-cx-fg">{conn.name}</span>
                <span className="font-cx-mono text-[12px] text-cx-fg-3">{conn.version}</span>
                <StudioBadge tone={conn.status === "ready" ? "success" : "warning"}>配额 {conn.quotaRemaining}/{conn.quotaTotal}</StudioBadge>
              </div>
              <Button size="md" variant="secondary" icon="refresh" className={BTN_SM} onClick={() => triggerSync(conn)}>
                同步拉取
              </Button>
            </div>
            <p className="text-[13px] text-cx-fg-2">{conn.description}</p>
            <div className="flex items-center gap-2">
              <Input
                size="md"
                value={queries[conn.id] ?? conn.defaultQuery}
                onChange={(e) => setQueries((prev) => ({ ...prev, [conn.id]: e.target.value }))}
                aria-label={`${conn.name}查询语句`}
                className="min-w-0 flex-1 font-cx-mono [&>input]:h-[32px] [&>input]:text-[12.5px]"
              />
            </div>
            <p className="text-[12px] text-cx-fg-3">
              {conn.completenessGuarantee}{conn.lastSyncedAt ? ` · 最近同步 ${conn.lastSyncedAt}` : ""} · 每次同步 -{conn.costPerSync} 配额
            </p>
          </div>
        ))}
      </div>
    </div>
  );
}

function ImportBatchList({ active, onImport, onClose }: { active: boolean; onImport: () => void; onClose: () => void }) {
  const state = useStudioState();
  const scope = state.workspace.projectId;
  const batches = state.imports.filter((batch) => scope === "all" || batch.projectId === scope);
  const scroll = useRememberedScroll("library-imports", active);
  return (
    <div className="flex min-h-0 flex-1 flex-col" data-testid="studio-import-history">
      <SurfaceToolbar>
        <IconButton size="sm" icon="arrowLeft" label="返回" onClick={onClose} />
        <PathBreadcrumb path="导入记录" rootLabel="资产" lastIsFile onSelect={() => studio.setLibraryUi({ importsOpen: false, importBatchId: "" })} />
        <span className="min-w-0 flex-1" />
        <Button size="sm" variant="secondary" icon="upload" onClick={onImport}>导入文件</Button>
      </SurfaceToolbar>
      <div ref={scroll.ref} className="cx-scroll min-h-0 flex-1 overflow-y-auto p-4">
        {batches.length ? (
          <SurfaceCard>
            {batches.map((batch) => <BatchRow key={batch.id} batch={batch} showProject={scope === "all"} />)}
          </SurfaceCard>
        ) : (
          <EmptyState
            compact
            icon="history"
            title={state.imports.length ? "这个项目还没有导入记录" : "还没有导入记录"}
            description="导入的文件和连接器同步批次都会留在这里。"
            action={<Button size="md" variant="secondary" icon="upload" onClick={onImport}>导入文件</Button>}
          />
        )}
      </div>
    </div>
  );
}

export function ImportHistory({ active, onImport }: { active: boolean; onImport: () => void }) {
  const state = useStudioState();
  const ui = state.workspace.library;
  const batch = ui.importBatchId ? state.imports.find((item) => item.id === ui.importBatchId) : undefined;
  if (batch) return <ImportBatchView key={batch.id} batch={batch} active={active} onBack={() => studio.setLibraryUi({ importBatchId: "", importFocusLine: undefined })} />;
  return <ImportBatchList active={active} onImport={onImport} onClose={() => studio.setLibraryUi({ importsOpen: false, importBatchId: "", importFocusLine: undefined })} />;
}

export function ImportStatusRow() {
  const state = useStudioState();
  const scope = state.workspace.projectId;
  const batches = state.imports.filter((batch) => scope === "all" || batch.projectId === scope);
  if (!batches.length) return null;
  const failed = batches.reduce((sum, batch) => sum + batch.items.filter((item) => item.outcome === "failed").length, 0);
  return (
    <button
      type="button"
      onClick={() => studio.setLibraryUi({ importsOpen: true, importBatchId: "" })}
      className="mx-4 mb-2 flex min-h-[44px] items-center gap-2 rounded-xl border border-cx-border-subtle px-3 py-1.5 text-left outline-none hover:bg-cx-hover focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]"
      data-testid="studio-import-status"
    >
      <Icon name="plug" size={15} className="shrink-0 text-cx-fg-3" />
      <span className="min-w-0 flex-1 truncate text-[13px] text-cx-fg-2">
        导入批次 · {batches.length} 个文件 · {state.connectors.length} 个外部连接器就绪
      </span>
      {failed ? <StudioBadge tone="danger">{failed} 条失败待修正</StudioBadge> : null}
      <Icon name="chevronRight" size={15} className="shrink-0 text-cx-fg-4" />
    </button>
  );
}

export function ImportOriginLink({ sourceId }: { sourceId: string }) {
  const state = useStudioState();
  const origin = state.importOrigins[sourceId];
  const batch = origin ? state.imports.find((item) => item.id === origin.batchId) : undefined;
  if (!batch) return null;
  return (
    <Button
      size="md"
      variant="ghost"
      icon="history"
      className={cn(BTN_SM, "-ml-3 max-w-full self-start")}
      onClick={() => studio.setLibraryUi({ importsOpen: true, importBatchId: batch.id, sourceId: "", passageId: "", clueId: "" })}
    >
      <span className="truncate">来自导入批次「{batch.id} · {batch.fileName}」</span>
    </Button>
  );
}
