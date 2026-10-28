"use client";

import { useCallback, useEffect, useMemo, useState, type CSSProperties, type KeyboardEvent } from "react";
import { cn } from "@/lib/cn";
import { Icon, type IconName } from "@/components/Icon";
import {
  Button,
  Checkbox,
  CopyButton,
  Dialog,
  EmptyState,
  IconButton,
  Label,
  Menu,
  MenuItem,
  MenuLabel,
  MenuSeparator,
  SearchInput,
  Select,
  Switch,
  TextArea,
  TextField,
  toast,
} from "@/components/chat/ui";
import { SectionHeader, SurfaceToolbar, relativeTime } from "@/components/chat/surfaces/shared";
import { SurfaceCard, SurfaceRow } from "@/components/chat/surfaces/OverviewSurface";
import { PathBreadcrumb } from "@/components/chat/surfaces/PathBreadcrumb";
import {
  ASSET_IMPORTANCE_LEVELS,
  SOURCE_KINDS,
  categoryLabel,
  importanceLabel,
  sourceKindLabel,
  statusLabel,
  type AssetImportance,
  type AssetVersionSnapshot,
  type SourceKind,
  type StudioPassage,
  type StudioRef,
  type StudioSource,
} from "./model";
import { openRef } from "./nav";
import {
  AttachButton,
  FIELD,
  HighlightedText,
  MENU_HIT,
  NewMark,
  OPTION_HIT,
  SheetAskBar,
  StudioBadge,
  askAi,
  dragRef,
  quoteToComposer,
  useRememberedScroll,
  useViewing,
  citeRef,
} from "./parts";
import { Dl, sendAbout } from "./Kit";
import { ImportHistory, ImportOriginLink, ImportSourcesDialog, ImportStatusRow } from "./ImportViews";
import {
  EMPTY_SOURCE_FILTER,
  SOURCE_STATUSES,
  countLabel,
  findSourceDuplicates,
  formatTime,
  mergeOf,
  displayTitle,
  pickSourceFilter,
  RUN_STATE_LABEL,
  sameFilter,
  sourceStatusLabel,
  type FilterTimeRange,
  type FindingsPresence,
  type MergeRecord,
  type SourceFilter,
} from "./records";
import {
  BatchExportDialog,
  MergeInfo,
  VirtualRecordList,
  BatchLinkDialog,
  BatchTagDialog,
  DuplicatesView,
  FilterChips,
  FilterField,
  FilterSelect,
  HistoryView,
  MergeDialog,
  MergeMark,
  RecordHistory,
  batchScope,
  reportChange,
  revertEntry,
  scopedHistory,
  useChecklist,
  type FilterChip,
} from "./RecordTools";
import { readStudioState, studio, useStudioState, type ImportOrigin, type LibraryUi, type StudioState } from "./store";

const KIND_ICON: Record<SourceKind, IconName> = {
  interview: "messageCircle",
  field: "pencilLine",
  archive: "archive",
  minutes: "rows",
};

const FINDINGS_PRESENCE: Array<{ value: FindingsPresence; label: string }> = [
  { value: "all", label: "不限" },
  { value: "with", label: "有发现" },
  { value: "open", label: "有待核实的发现" },
  { value: "without", label: "还没有发现" },
];

const TIME_RANGES: Array<{ value: FilterTimeRange; label: string }> = [
  { value: "all", label: "不限时间" },
  { value: "24h", label: "最近观察 (10月27日)" },
  { value: "7d", label: "近 7 天观察" },
  { value: "30d", label: "早期基线" },
  { value: "stale", label: "待刷新" },
];

interface ClueCounts {
  total: number;
  open: number;
}

function useClueCounts() {
  const state = useStudioState();
  return useMemo(() => {
    const counts = new Map<string, ClueCounts>();
    state.clues.forEach((clue) => {
      if (mergeOf(state.merges, "clue", clue.id)) return;
      const assets = clue.affectedSourceIds?.length ? clue.affectedSourceIds : [clue.sourceId];
      assets.forEach((assetId) => {
        const entry = counts.get(assetId) ?? { total: 0, open: 0 };
        entry.total += 1;
        if (clue.status !== "verified") entry.open += 1;
        counts.set(assetId, entry);
      });
    });
    return counts;
  }, [state.clues, state.merges]);
}

function sourceMatches(
  source: StudioSource,
  filter: SourceFilter,
  counts: Map<string, ClueCounts>,
  merges: MergeRecord[],
  origins: Record<string, ImportOrigin>,
  withReportAssets: Set<string>,
  runAssets: Map<string, Set<string>>,
  activeProjectId: string,
): boolean {
  const lifecycle = filter.lifecycle ?? "active";
  const isUnlinked = Boolean(source.unlinkedTaskIds?.includes(activeProjectId));
  if (lifecycle === "active" && (source.archived || isUnlinked)) return false;
  if (lifecycle === "archived" && !source.archived) return false;
  if (lifecycle === "unlinked" && !isUnlinked) return false;

  if (!filter.showMerged && mergeOf(merges, "source", source.id)) return false;
  if (filter.kind !== "all" && source.kind !== filter.kind) return false;
  if (filter.status !== "all" && source.status !== filter.status) return false;
  if (filter.importance && filter.importance !== "all" && (source.importance ?? "normal") !== filter.importance) return false;
  if (filter.tag && !(source.tags ?? []).includes(filter.tag)) return false;

  const clues = counts.get(source.id);
  if (filter.findings === "with" && !clues?.total) return false;
  if (filter.findings === "open" && !clues?.open) return false;
  if (filter.findings === "without" && clues?.total) return false;

  if (filter.report === "with" && !withReportAssets.has(source.id)) return false;
  if (filter.report === "without" && withReportAssets.has(source.id)) return false;
  if (filter.runId && !runAssets.get(filter.runId)?.has(source.id)) return false;

  if (filter.conflict === "with" && !(source.conflicts?.length)) return false;
  if (filter.conflict === "unresolved" && !(source.conflicts?.some((c) => !c.resolvedBy || c.resolvedBy === "rule"))) return false;

  if (filter.timeRange && filter.timeRange !== "all") {
    if (filter.timeRange === "24h" && !source.collectedAt.includes("10 月 27 日") && source.collectedAt !== "刚刚") return false;
    if (filter.timeRange === "7d" && !source.collectedAt.includes("10 月")) return false;
    if (filter.timeRange === "30d" && !source.collectedAt.includes("09 月")) return false;
    if (filter.timeRange === "stale" && !source.lastFailedAt) return false;
  }

  if (filter.origin !== "all") {
    const origin = origins[source.id];
    if (filter.origin === "manual" ? origin : filter.origin === "imported" ? !origin : origin?.batchId !== filter.origin) return false;
  }
  const q = filter.query.trim().toLowerCase();
  if (!q) return true;
  return source.title.toLowerCase().includes(q)
    || source.author.toLowerCase().includes(q)
    || (source.canonicalKey ?? "").toLowerCase().includes(q)
    || (source.aliases ?? []).some((a) => a.toLowerCase().includes(q))
    || (source.tags ?? []).some((t) => t.toLowerCase().includes(q))
    || source.passages.some((passage) => passage.text.toLowerCase().includes(q));
}

function originLabel(state: StudioState, origin: string): string {
  if (origin === "manual") return "手动录入";
  if (origin === "imported") return "文件导入";
  const batch = state.imports.find((item) => item.id === origin);
  return batch ? `导入：${batch.fileName}` : "导入批次已不存在";
}

function sourceChips(state: StudioState, filter: SourceFilter): FilterChip[] {
  const set = (patch: Partial<SourceFilter>) => () => studio.setLibraryUi(patch);
  const chips: FilterChip[] = [];
  if (filter.kind !== "all") chips.push({ key: "kind", label: `类型：${sourceKindLabel(filter.kind)}`, onRemove: set({ kind: "all" }) });
  if (filter.status !== "all") chips.push({ key: "status", label: sourceStatusLabel(filter.status), onRemove: set({ status: "all" }) });
  if (filter.importance && filter.importance !== "all") chips.push({ key: "importance", label: `重要性：${importanceLabel(filter.importance)}`, onRemove: set({ importance: "all" }) });
  if (filter.timeRange && filter.timeRange !== "all") {
    chips.push({ key: "timeRange", label: TIME_RANGES.find((t) => t.value === filter.timeRange)?.label ?? filter.timeRange, onRemove: set({ timeRange: "all" }) });
  }
  if (filter.findings !== "all") {
    chips.push({ key: "findings", label: FINDINGS_PRESENCE.find((item) => item.value === filter.findings)?.label ?? "", onRemove: set({ findings: "all" }) });
  }
  if (filter.report && filter.report !== "all") chips.push({ key: "report", label: filter.report === "with" ? "已关联报告" : "未关联报告", onRemove: set({ report: "all" }) });
  if (filter.runId) chips.push({ key: "runId", label: `工作记录：${filter.runId}`, onRemove: set({ runId: "" }) });
  if (filter.tag) chips.push({ key: "tag", label: `标签：${filter.tag}`, onRemove: set({ tag: "" }) });
  if (filter.conflict && filter.conflict !== "all") chips.push({ key: "conflict", label: filter.conflict === "with" ? "有字段冲突" : "待裁决冲突", onRemove: set({ conflict: "all" }) });
  if (filter.lifecycle && filter.lifecycle !== "active") {
    const lbl = filter.lifecycle === "archived" ? "已归档资产" : filter.lifecycle === "unlinked" ? "已解除任务关联" : "含已归档/解除关联";
    chips.push({ key: "lifecycle", label: lbl, onRemove: set({ lifecycle: "active" }) });
  }
  if (filter.origin !== "all") chips.push({ key: "origin", label: originLabel(state, filter.origin), onRemove: set({ origin: "all" }) });
  if (filter.showMerged) chips.push({ key: "merged", label: "含已归并的", onRemove: set({ showMerged: false }) });
  return chips;
}

function readerTarget(ui: LibraryUi, source: StudioSource): StudioRef {
  if (ui.clueId) return { kind: "clue", id: ui.clueId };
  if (ui.passageId && source.passages.some((passage) => passage.id === ui.passageId)) return { kind: "passage", id: ui.passageId };
  return { kind: "source", id: source.id };
}

function openSource(sourceId: string) {
  studio.setLibraryUi({ sourceId, passageId: "", clueId: "" });
}

function closeReader() {
  studio.setLibraryUi({ sourceId: "", passageId: "", clueId: "" });
}

/** Display name without the "A-xxxx · " id prefix; the id shows separately in mono. */
export function assetTitle(source: StudioSource): string {
  return displayTitle(source.canonicalKey ?? source.title);
}

function CollectSourceDialog({ open, onOpenChange, projectId }: { open: boolean; onOpenChange: (open: boolean) => void; projectId: string }) {
  const state = useStudioState();
  const [title, setTitle] = useState("");
  const [canonicalKey, setCanonicalKey] = useState("");
  const [aliasesText, setAliasesText] = useState("");
  const [importance, setImportance] = useState<AssetImportance>("normal");
  const [kind, setKind] = useState<SourceKind>(SOURCE_KINDS[0]?.value ?? "field");
  const [text, setText] = useState("");
  const [target, setTarget] = useState(projectId);
  useEffect(() => {
    if (!open) return;
    setTitle("");
    setCanonicalKey("");
    setAliasesText("");
    setImportance("normal");
    setText("");
    setTarget(projectId);
  }, [open, projectId]);
  const canSave = Boolean(title.trim() && text.trim() && target);
  const save = () => {
    if (!canSave) return;
    const aliases = aliasesText.split(/[,，\s]+/).map((s) => s.trim()).filter(Boolean);
    const id = studio.addSource({
      projectId: target,
      title: title.trim(),
      kind,
      text,
      author: "我",
      canonicalKey: canonicalKey.trim() || title.trim(),
      aliases,
      importance,
    });
    onOpenChange(false);
    openSource(id);
    toast({
      title: "已录入资产库",
      description: "已记入处理记录（支持一键撤销），并自动保存在当前工作台。",
      tone: "success",
      action: {
        label: "撤销",
        onClick: () => {
          const latest = readStudioState().history[0];
          if (latest) revertEntry(latest.id);
        },
      },
    });
  };
  return (
    <Dialog
      open={open}
      onOpenChange={onOpenChange}
      title="录入资产"
      description="登记资产规范标识、别名、重要性与观测正文，每一行会成为一个可引用的段落。"
      icon="book"
      size="md"
      footer={(
        <>
          <Button variant="ghost" onClick={() => onOpenChange(false)}>取消</Button>
          <Button variant="primary" disabled={!canSave} onClick={save}>加入资产库</Button>
        </>
      )}
    >
      <div className="flex flex-col gap-4">
        <TextField label="资产标题" value={title} onChange={(event) => setTitle(event.target.value)} placeholder="例如：A-1285 · gateway-north.example.invalid" className={FIELD} autoFocus />
        <div className="grid grid-cols-2 gap-3 max-sm:grid-cols-1">
          <TextField label="规范标识 (Canonical Key)" value={canonicalKey} onChange={(e) => setCanonicalKey(e.target.value)} placeholder="留空时同标题" className={FIELD} />
          <TextField label="别名 (逗号分隔)" value={aliasesText} onChange={(e) => setAliasesText(e.target.value)} placeholder="例如：old-gw.example.invalid" className={FIELD} />
        </div>
        <div className="grid grid-cols-3 gap-3 max-sm:grid-cols-1">
          <div className="flex flex-col">
            <Label>所属项目</Label>
            <Select value={target} onChange={setTarget} options={state.projects.map((project) => ({ value: project.id, label: displayTitle(project.title) }))} ariaLabel="所属项目" className={FIELD} popoverClassName={OPTION_HIT} />
          </div>
          <div className="flex flex-col">
            <Label>类型</Label>
            <Select<SourceKind> value={kind} onChange={setKind} options={SOURCE_KINDS.map((item) => ({ value: item.value, label: item.label }))} ariaLabel="资产类型" className={FIELD} popoverClassName={OPTION_HIT} />
          </div>
          <div className="flex flex-col">
            <Label>重要性</Label>
            <Select<AssetImportance> value={importance} onChange={setImportance} options={ASSET_IMPORTANCE_LEVELS} ariaLabel="重要性" className={FIELD} popoverClassName={OPTION_HIT} />
          </div>
        </div>
        <TextArea label="正文与观测说明" value={text} onChange={(event) => setText(event.target.value)} placeholder="每行一段。之后可以在原文里选中文字记为发现。" className="min-h-[140px] text-[14px] leading-6" />
      </div>
    </Dialog>
  );
}

/** Batch actions on checked sources: status, kind, tags, links, export, archive, merge, delete (all undoable!). */
function SourceBatchActions({ checked, matchingIds, filterText }: { checked: string[]; matchingIds: string[]; filterText: string }) {
  const state = useStudioState();
  const [mergeIds, setMergeIds] = useState<string[]>([]);
  const [mergeOpen, setMergeOpen] = useState(false);
  const [tagOpen, setTagOpen] = useState(false);
  const [linkOpen, setLinkOpen] = useState(false);
  const [exportOpen, setExportOpen] = useState(false);
  const byId = useMemo(() => new Map(state.sources.map((source) => [source.id, source])), [state.sources]);
  const changing = (differs: (source: StudioSource) => boolean) => checked.filter((id) => {
    const source = byId.get(id);
    return source ? differs(source) : false;
  }).length;
  const scope = batchScope("source", checked.length, filterText);
  const subject = `勾选的 ${countLabel("source", checked.length)}`;

  const onDeleteBatch = () => {
    const entryId = studio.batchDeleteRecords("source", checked, scope);
    reportChange(entryId, "没有可删除的资产。");
  };

  return (
    <>
      <Menu
        ariaLabel="移到"
        className={MENU_HIT}
        trigger={<Button size="md" variant="secondary" iconRight="chevronDown">移到</Button>}
      >
        <MenuLabel>{subject}移到</MenuLabel>
        {SOURCE_STATUSES.map((item) => {
          const count = changing((source) => source.status !== item.value);
          return (
            <MenuItem
              key={item.value}
              disabled={!count}
              hint={count ? `${count} 份会改` : "都已是"}
              onSelect={() => reportChange(studio.setSourceStatus(checked, item.value, scope), `勾选的资产都已经是「${item.label}」。`)}
            >
              {item.label}
            </MenuItem>
          );
        })}
      </Menu>
      <Menu
        ariaLabel="改类型"
        className={MENU_HIT}
        trigger={<Button size="md" variant="secondary" iconRight="chevronDown">改类型</Button>}
      >
        <MenuLabel>{subject}的类型改为</MenuLabel>
        {SOURCE_KINDS.map((item) => {
          const count = changing((source) => source.kind !== item.value);
          return (
            <MenuItem
              key={item.value}
              icon={KIND_ICON[item.value]}
              disabled={!count}
              hint={count ? `${count} 份会改` : "都已是"}
              onSelect={() => reportChange(studio.setSourceKind(checked, item.value, scope), `勾选的资产都已经是「${item.label}」。`)}
            >
              {item.label}
            </MenuItem>
          );
        })}
      </Menu>
      <Button size="md" variant="secondary" icon="hash" onClick={() => setTagOpen(true)}>
        标签…
      </Button>
      <Button size="md" variant="secondary" icon="link" onClick={() => setLinkOpen(true)}>
        关联…
      </Button>
      <Button size="md" variant="secondary" icon="download" onClick={() => setExportOpen(true)}>
        导出…
      </Button>
      <Button
        size="md"
        variant="secondary"
        icon="listCollapse"
        disabled={checked.length < 2}
        tooltip={checked.length < 2 ? "勾选两份或更多重复的资产后归并" : undefined}
        onClick={() => { setMergeIds(checked); setMergeOpen(true); }}
      >
        归并…
      </Button>
      <Button
        size="md"
        variant="ghost"
        icon="archive"
        onClick={() => reportChange(studio.toggleAssetArchive(checked, true, scope), "勾选资产都已归档。")}
      >
        归档
      </Button>
      <Button
        size="md"
        variant="ghost"
        icon="trash"
        className={"text-cx-danger hover:text-cx-danger"}
        onClick={onDeleteBatch}
      >
        删除
      </Button>
      <MergeDialog open={mergeOpen} onOpenChange={setMergeOpen} kind="source" ids={mergeIds} />
      <BatchTagDialog open={tagOpen} onOpenChange={setTagOpen} kind="source" ids={checked} scope={scope} />
      <BatchLinkDialog open={linkOpen} onOpenChange={setLinkOpen} kind="source" ids={checked} scope={scope} />
      <BatchExportDialog open={exportOpen} onOpenChange={setExportOpen} kind="source" checkedIds={checked} matchingIds={matchingIds} />
    </>
  );
}

// ---------------------------------------------------------------------------
// Complete Asset Dossier & Reader (Passages, Attributes/Conflicts, Versions/Diff, Relations/Reuse)
// ---------------------------------------------------------------------------

function passageDomId(passageId: string) {
  return `studio-passage-${passageId}`;
}

interface TextSelection {
  text: string;
  passageId: string;
  top: number;
  left: number;
}

function usePassageSelection(root: HTMLElement | null, enabled: boolean) {
  const [selection, setSelection] = useState<TextSelection | null>(null);
  useEffect(() => {
    if (!enabled || !root) {
      setSelection(null);
      return;
    }
    let frame = 0;
    const read = () => {
      const current = window.getSelection();
      if (!current || current.isCollapsed || !current.rangeCount) { setSelection(null); return; }
      const range = current.getRangeAt(0);
      const host = (node: Node) => (node instanceof Element ? node : node.parentElement)?.closest<HTMLElement>("[data-passage-text]") ?? null;
      const start = host(range.startContainer);
      if (!start || start !== host(range.endContainer) || !root.contains(start)) { setSelection(null); return; }
      const text = current.toString().trim();
      const passageId = start.dataset.passageText ?? "";
      if (!text || !passageId) { setSelection(null); return; }
      const rect = range.getBoundingClientRect();
      const base = root.getBoundingClientRect();
      const above = rect.top - base.top - 52;
      setSelection({
        text,
        passageId,
        top: above >= 0 ? above : rect.bottom - base.top + 8,
        left: Math.min(Math.max(rect.left - base.left + rect.width / 2, 170), Math.max(170, base.width - 170)),
      });
    };
    const onChange = () => {
      window.cancelAnimationFrame(frame);
      frame = window.requestAnimationFrame(read);
    };
    document.addEventListener("selectionchange", onChange);
    return () => {
      window.cancelAnimationFrame(frame);
      document.removeEventListener("selectionchange", onChange);
    };
  }, [enabled, root]);
  return selection;
}

function clearTextSelection() {
  window.getSelection()?.removeAllRanges();
}

function recordFinding(source: StudioSource, passage: StudioPassage, quote: string) {
  const id = studio.addClue({ projectId: source.projectId, sourceId: source.id, passageId: passage.id, quote, category: "detail" });
  studio.setLibraryUi({ passageId: passage.id, clueId: id });
  toast({
    title: "已记为发现",
    description: `归为「${categoryLabel("detail")}」，可以随时改或撤销。`,
    tone: "success",
    action: { label: "查看", onClick: () => openRef({ kind: "clue", id }) },
  });
}

function SelectionToolbar({ selection, source }: { selection: TextSelection; source: StudioSource }) {
  const passage = source.passages.find((item) => item.id === selection.passageId);
  if (!passage) return null;
  const owner: StudioRef = { kind: "passage", id: passage.id };
  const run = (action: () => void) => () => { action(); clearTextSelection(); };
  const style: CSSProperties = { top: selection.top, left: selection.left };
  return (
    <div
      role="toolbar"
      aria-label="选中文字的操作"
      style={style}
      onMouseDown={(event) => event.preventDefault()}
      onKeyDown={(event: KeyboardEvent<HTMLDivElement>) => { if (event.key === "Escape") clearTextSelection(); }}
      className="absolute z-20 flex -translate-x-1/2 items-center gap-1 rounded-2xl border border-cx-border bg-cx-elevated p-1 shadow-cx-md"
      data-testid="studio-selection-toolbar"
    >
      <Button
        size="md"
        variant="primary"
        icon="sparkles"
        onClick={run(() => {
          quoteToComposer(selection.text, owner, { silent: true });
          askAi(null);
        })}
      >
        问 AI
      </Button>
      <Button size="md" variant="ghost" icon="quote" onClick={run(() => quoteToComposer(selection.text, owner))}>引用</Button>
      <Button size="md" variant="ghost" icon="bookmark" onClick={run(() => recordFinding(source, passage, selection.text))}>记为发现</Button>
    </div>
  );
}

function SourceRow({ source, counts, checked, selecting, onToggle }: {
  source: StudioSource;
  counts?: ClueCounts;
  checked: boolean;
  selecting: boolean;
  onToggle: (checked: boolean, range: boolean) => void;
}) {
  const target: StudioRef = { kind: "source", id: source.id };
  const unresolved = (source.conflicts ?? []).filter((conf) => !conf.resolvedBy || conf.resolvedBy === "rule").length;
  const signal = unresolved ? <span className="shrink-0 text-[12px] text-cx-warning">冲突 {unresolved}</span>
    : counts?.total ? <span className="shrink-0 text-[12px] text-cx-fg-3">{counts.total} 发现</span> : null;
  const lead = (
    <span className="relative grid size-8 shrink-0 place-items-center">
      <span className={cn("absolute inset-0 grid place-items-center rounded-lg bg-cx-hover text-cx-fg-3", (selecting || checked) && "hidden", "group-hover/row:hidden")}>
        <Icon name={KIND_ICON[source.kind]} size={15} />
      </span>
      <span className={cn("absolute inset-0 place-items-center", (selecting || checked) ? "grid" : "hidden group-hover/row:grid")}>
        <Checkbox
          checked={checked}
          aria-label={`勾选「${source.title}」`}
          onCheckedChange={(next) => onToggle(Boolean(next), false)}
          className="[&>button]:size-5"
        />
      </span>
    </span>
  );
  return (
    <span className="group/row block" {...dragRef(target)}>
      <SurfaceRow
        icon={KIND_ICON[source.kind]}
        leading={lead}
        selected={checked}
        title={(
          <>
            {assetTitle(source)}
            <NewMark target={target} />
            <MergeMark kind="source" id={source.id} />
          </>
        )}
        subtitle={[
          sourceKindLabel(source.kind),
          source.lastObservedAt ? `最近观察 ${relativeTime(source.lastObservedAt) || source.lastObservedAt}` : relativeTime(source.collectedAt) || source.collectedAt,
          source.archived ? "已归档" : null,
        ].filter(Boolean).join(" · ")}
        trailing={signal}
        onClick={() => openSource(source.id)}
      />
    </span>
  );
}

function SourceList({ active, onCollect, onImport }: { active: boolean; onCollect: () => void; onImport: () => void }) {
  const state = useStudioState();
  const counts = useClueCounts();
  const ui = state.workspace.library;
  const projectId = state.workspace.projectId;
  const scroll = useRememberedScroll("library-list", active);
  const [exportOpen, setExportOpen] = useState(false);
  const [filterOpen, setFilterOpen] = useState(false);
  const [connectorOpen, setConnectorOpen] = useState(false);

  const filter = useMemo<SourceFilter>(() => pickSourceFilter(ui), [ui]);
  const isEmpty = sameFilter(filter, EMPTY_SOURCE_FILTER);
  const chips = sourceChips(state, filter);
  const describe = [filter.query.trim() ? `搜索「${filter.query.trim()}」` : "", ...chips.map((chip) => chip.label)].filter(Boolean).join("、");

  const withReportAssets = useMemo(() => {
    const ids = new Set<string>();
    const clueById = new Map(state.clues.map((c) => [c.id, c]));
    state.reports.forEach((rep) => {
      const c = clueById.get(rep.findingId);
      if (c) (c.affectedSourceIds ?? [c.sourceId]).forEach((id) => ids.add(id));
    });
    return ids;
  }, [state.clues, state.reports]);

  const runAssets = useMemo(() => {
    const map = new Map<string, Set<string>>();
    state.workRuns.forEach((r) => {
      const set = new Set<string>();
      r.linkedRefs.forEach((ref) => { if (ref.kind === "source") set.add(ref.id); });
      map.set(r.id, set);
    });
    state.clues.forEach((c) => {
      if (!c.sourceRunId) return;
      const set = map.get(c.sourceRunId) ?? new Set<string>();
      (c.affectedSourceIds ?? [c.sourceId]).forEach((id) => set.add(id));
      map.set(c.sourceRunId, set);
    });
    return map;
  }, [state.clues, state.workRuns]);

  const scoped = useMemo(
    () => state.sources.filter((source) => projectId === "all" || source.projectId === projectId),
    [projectId, state.sources],
  );
  const matching = useMemo(
    () => scoped.filter((source) => sourceMatches(source, filter, counts, state.merges, state.importOrigins, withReportAssets, runAssets, projectId)),
    [counts, filter, projectId, runAssets, scoped, state.importOrigins, state.merges, withReportAssets],
  );
  const ordered = useMemo(
    () => SOURCE_STATUSES.flatMap((group) => matching.filter((source) => source.status === group.value)),
    [matching],
  );
  const orderedIds = useMemo(() => ordered.map((source) => source.id), [ordered]);

  const setChecked = useCallback((ids: string[]) => studio.setLibraryUi({ checked: ids }), []);
  const checklist = useChecklist(ui.checked, orderedIds, setChecked);

  const duplicates = useMemo(
    () => findSourceDuplicates(state.sources, state.merges, state.dismissedDuplicates, state.undeterminedDuplicates).filter((group) => projectId === "all" || group.projectId === projectId),
    [projectId, state.dismissedDuplicates, state.merges, state.sources, state.undeterminedDuplicates],
  );
  const history = useMemo(() => scopedHistory(state.history, "source", state.sources, projectId), [projectId, state.history, state.sources]);

  const batches = state.imports.filter((batch) => projectId === "all" || batch.projectId === projectId);
  const originOptions = [
    { value: "all", label: "不限" },
    { value: "manual", label: "手动录入" },
    { value: "imported", label: "文件导入" },
    ...batches.map((batch) => ({ value: batch.id, label: batch.fileName, description: `导入于 ${formatTime(batch.importedAt)}` })),
  ];
  const tagOptions = useMemo(() => {
    const tags = new Set<string>();
    scoped.forEach((s) => (s.tags ?? []).forEach((t) => tags.add(t)));
    if (filter.tag) tags.add(filter.tag);
    return [{ value: "", label: "不限" }, ...Array.from(tags).sort().map((t) => ({ value: t, label: t }))];
  }, [filter.tag, scoped]);
  const selecting = ui.checked.length > 0;

  return (
    <section aria-label="资产列表" className="flex min-h-0 flex-1 flex-col">
      <SurfaceToolbar>
        <SearchInput
          value={ui.query}
          onValueChange={(query) => studio.setLibraryUi({ query, view: "list" })}
          placeholder="搜索资产"
          aria-label="搜索资产"
          size="sm"
          className="min-w-0 flex-1"
        />
        <Menu
          open={filterOpen}
          onOpenChange={setFilterOpen}
          ariaLabel="筛选资产"
          className={MENU_HIT}
          trigger={(
            <span className="relative inline-flex">
              <IconButton size="sm" icon="filter" label="筛选" />
              {chips.length ? <span className="pointer-events-none absolute right-1 top-1 size-1.5 rounded-full bg-cx-accent" /> : null}
            </span>
          )}
        >
          <MenuLabel>快速筛选</MenuLabel>
          <MenuItem icon="star" checked={filter.importance === "core"} onSelect={() => studio.setLibraryUi({ importance: filter.importance === "core" ? "all" : "core", view: "list" })}>核心</MenuItem>
          <MenuItem icon="gitCompare" checked={filter.conflict === "with" || filter.conflict === "unresolved"} onSelect={() => studio.setLibraryUi({ conflict: filter.conflict === "with" || filter.conflict === "unresolved" ? "all" : "unresolved", view: "list" })}>有冲突</MenuItem>
          <MenuItem icon="refresh" checked={filter.timeRange === "stale"} onSelect={() => studio.setLibraryUi({ timeRange: filter.timeRange === "stale" ? "all" : "stale", view: "list" })}>待刷新</MenuItem>
          <MenuItem icon="archive" checked={filter.lifecycle === "archived"} onSelect={() => studio.setLibraryUi({ lifecycle: filter.lifecycle === "archived" ? "active" : "archived", view: "list" })}>已归档</MenuItem>
          <MenuSeparator />
          <div className="flex flex-col gap-3 p-3">
            <FilterSelect<"all" | SourceKind> label="类型" value={filter.kind} onChange={(kind) => studio.setLibraryUi({ kind })} options={[{ value: "all", label: "不限" }, ...SOURCE_KINDS.map((item) => ({ value: item.value, label: item.label }))]} />
            <FilterSelect<"all" | StudioSource["status"]> label="整理状态" value={filter.status} onChange={(status) => studio.setLibraryUi({ status })} options={[{ value: "all", label: "不限" }, ...SOURCE_STATUSES]} />
            <FilterSelect<FilterTimeRange> label="观察时间" value={filter.timeRange ?? "all"} onChange={(timeRange) => studio.setLibraryUi({ timeRange })} options={TIME_RANGES} />
            <FilterSelect<FindingsPresence> label="关联发现" value={filter.findings} onChange={(findings) => studio.setLibraryUi({ findings })} options={FINDINGS_PRESENCE} />
            <FilterSelect<string> label="标签" value={filter.tag ?? ""} onChange={(tag) => studio.setLibraryUi({ tag })} options={tagOptions} searchable={tagOptions.length > 6} />
            <FilterSelect<string> label="来源" value={filter.origin} onChange={(origin) => studio.setLibraryUi({ origin })} options={originOptions} searchable={originOptions.length > 8} />
            <FilterField label="已归并的资产">
              <Switch checked={filter.showMerged} onCheckedChange={(showMerged) => studio.setLibraryUi({ showMerged })} label="也列出已归并的" />
            </FilterField>
            {!isEmpty ? (
              <Button size="sm" variant="ghost" className="self-start" onClick={() => studio.setLibraryUi({ ...EMPTY_SOURCE_FILTER })}>清除全部筛选</Button>
            ) : null}
          </div>
        </Menu>
        <Menu
          ariaLabel="新建或导入"
          className={MENU_HIT}
          trigger={<IconButton size="sm" icon="plus" label="新建或导入" />}
        >
          <MenuItem icon="upload" onSelect={onImport}>导入文件</MenuItem>
          <MenuItem icon="plug" onSelect={() => setConnectorOpen(true)}>连接外部来源</MenuItem>
          <MenuItem icon="pencilLine" onSelect={onCollect}>新建资产</MenuItem>
        </Menu>
        <Menu
          ariaLabel="更多"
          className={MENU_HIT}
          trigger={<IconButton size="sm" icon="more" label="更多" />}
        >
          <MenuItem icon="copy" disabled={!duplicates.length} onSelect={() => studio.setLibraryUi({ view: "duplicates" })}>可能重复{duplicates.length ? ` ${duplicates.length}` : ""}</MenuItem>
          <MenuItem icon="history" onSelect={() => studio.setLibraryUi({ importsOpen: true })}>导入记录</MenuItem>
          <MenuItem icon="listChecks" onSelect={() => studio.setLibraryUi({ view: "history" })}>处理记录</MenuItem>
          <MenuSeparator />
          <MenuItem icon="download" onSelect={() => setExportOpen(true)}>导出</MenuItem>
        </Menu>
      </SurfaceToolbar>
      {state.assetLoad && !state.assetLoad.complete ? (
        <p className="border-b border-cx-sep px-3 py-1.5 text-[12px] text-cx-fg-3" data-testid="asset-load-notice">
          仅加载了前 {state.assetLoad.loaded} 项 / 共 {state.assetLoad.total} 项（列表分页上限，其余资产仍在服务端）
        </p>
      ) : null}
      {ui.view === "list" && chips.length ? <FilterChips chips={chips} onClear={() => studio.setLibraryUi({ ...EMPTY_SOURCE_FILTER, query: ui.query })} /> : null}
      <ImportStatusRow />
      {ui.view === "duplicates" ? (
        <DuplicatesView kind="source" groups={duplicates} active={active} onOpen={openSource} />
      ) : ui.view === "history" ? (
        <HistoryView kind="source" entries={history} active={active} />
      ) : (
        <div ref={scroll.ref} className="cx-scroll min-h-0 flex-1 overflow-y-auto p-4">
          {ordered.length ? (
            <>
              <VirtualRecordList
                items={ordered}
                scrollNode={scroll.node.current}
                estimateSize={60}
                ariaLabel="资产虚拟滚动列表"
                renderItem={(source) => (
                  <SourceRow
                    key={source.id}
                    source={source}
                    counts={counts.get(source.id)}
                    checked={checklist.isChecked(source.id)}
                    selecting={selecting}
                    onToggle={(next, range) => checklist.toggle(source.id, next, range)}
                  />
                )}
              />
              {selecting ? (
                <div className="sticky bottom-3 z-10 mx-auto mt-2 flex w-fit items-center gap-1 rounded-2xl border border-cx-border bg-cx-elevated px-3 py-1.5 shadow-cx-md" data-testid="studio-source-bulkbar">
                  <span className="mr-1 text-[12px] font-medium text-cx-fg-2">已选 {ui.checked.length}</span>
                  <SourceBatchActions checked={ui.checked} matchingIds={orderedIds} filterText={describe} />
                  <IconButton size="sm" icon="x" label="清除选择" onClick={() => setChecked([])} />
                </div>
              ) : null}
            </>
          ) : (
            <EmptyState
              compact
              icon="book"
              title={scoped.length ? "没有符合条件的资产" : "还没有资产"}
              description={scoped.length ? "换个关键词或筛选条件试试。" : "录入一份笔记或摘录，或者导入 CSV、JSON、TXT 文件。"}
              action={scoped.length ? (
                isEmpty ? undefined : <Button size="md" variant="secondary" onClick={() => studio.setLibraryUi({ ...EMPTY_SOURCE_FILTER })}>清除筛选和搜索</Button>
              ) : (
                <>
                  <Button size="md" variant="secondary" icon="plus" onClick={onCollect}>录入资产</Button>
                  <Button size="md" variant="secondary" icon="upload" onClick={onImport}>导入文件</Button>
                </>
              )}
            />
          )}
        </div>
      )}
      <BatchExportDialog open={exportOpen} onOpenChange={setExportOpen} kind="source" checkedIds={ui.checked} matchingIds={orderedIds} />
      <ConnectorsSheet open={connectorOpen} onOpenChange={setConnectorOpen} />
    </section>
  );
}

function ConnectorsSheet({ open, onOpenChange }: { open: boolean; onOpenChange: (open: boolean) => void }) {
  const state = useStudioState();
  const connectors = state.connectors ?? [];
  return (
    <Dialog open={open} onOpenChange={onOpenChange} title="连接外部来源" description="后端连接器清单（真实 manifests）。" size="md">
      <div className="flex flex-col gap-1">
        {connectors.length ? connectors.map((c) => (
          <SurfaceRow key={c.id} icon="plug" title={c.name} subtitle={[c.version, c.capabilities.slice(0, 3).join("/")].filter(Boolean).join(" · ")} />
        )) : <p className="p-2 text-[13px] text-cx-fg-3">后端没有返回连接器清单。</p>}
      </div>
    </Dialog>
  );
}

// ---------------------------------------------------------------------------
// Asset drill-in — header card + stacked sections (conflicts, 发现, 关系, 版本, 原文)
// ---------------------------------------------------------------------------

function ConflictRow({ source, conf }: { source: StudioSource; conf: NonNullable<StudioSource["conflicts"]>[number] }) {
  const [open, setOpen] = useState(false);
  const pending = !conf.resolvedBy || conf.resolvedBy === "rule";
  return (
    <div className="rounded-xl px-2 py-1.5" data-testid={`studio-conflict-${conf.id}`}>
      <button type="button" onClick={() => setOpen((v) => !v)} className="flex w-full items-center gap-3 text-left">
        <span className="grid size-8 shrink-0 place-items-center rounded-lg bg-cx-hover text-cx-fg-3"><Icon name="gitCompare" size={15} /></span>
        <span className="flex min-w-0 flex-1 flex-col">
          <span className="truncate text-[13px] font-medium text-cx-fg">{conf.fieldLabel}</span>
          <span className="truncate text-[12px] text-cx-fg-4">
            {pending ? "待裁决" : conf.resolvedBy === "manual" ? "人工已裁决" : "AI 建议已采纳"} · 当前 {conf.selectedValue}
          </span>
        </span>
        {pending ? <span className="shrink-0 text-[12px] text-cx-warning">待裁决</span> : null}
        <Icon name={open ? "chevronDown" : "chevronRight"} size={14} className="shrink-0 text-cx-fg-4" />
      </button>
      {open ? (
        <div className="mt-2 flex flex-col gap-1 pl-11">
          {conf.candidates.map((cand, idx) => {
            const active = cand.value === conf.selectedValue;
            return (
              <div key={idx} className="flex items-center justify-between gap-2 rounded-lg px-2 py-1.5 text-[13px] hover:bg-cx-hover">
                <div className="flex min-w-0 flex-1 flex-col">
                  <span className={cn("truncate", active ? "font-medium text-cx-fg" : "text-cx-fg-2")}>{cand.value}</span>
                  <span className="truncate text-[12px] text-cx-fg-4">{cand.source} · {cand.observedAt}</span>
                </div>
                <button
                  type="button"
                  disabled={active}
                  className={cn("shrink-0 text-[12px] font-medium", active ? "cursor-default text-cx-fg-4" : "text-cx-accent hover:underline")}
                  onClick={() => {
                    studio.resolveAssetConflict(source.id, conf.id, cand.value, `人工裁决选用「${cand.source}」（${cand.observedAt}）的值`);
                    toast({ title: `已选用「${cand.source}」的字段值`, tone: "success" });
                  }}
                >
                  {active ? "当前值" : "选用"}
                </button>
              </div>
            );
          })}
        </div>
      ) : null}
    </div>
  );
}

function VersionDiffDialog({ source, version, onClose }: { source: StudioSource; version: AssetVersionSnapshot; onClose: () => void }) {
  const versions = source.versions ?? [];
  const idx = versions.findIndex((v) => v.version === version.version);
  const prev = versions[idx + 1];
  if (!prev) return null;
  const keys = Array.from(new Set([...Object.keys(prev.attributes ?? {}), ...Object.keys(version.attributes ?? {})]));
  const lines = [
    `正文预览`,
    ...keys.map((key) => `${key}: ${prev.attributes[key] ?? "—"} → ${version.attributes[key] ?? "—"}`),
    ``,
    `正文 A: ${prev.bodyPreview}`,
    `正文 B: ${version.bodyPreview}`,
  ].join("\n");
  return (
    <Dialog open onOpenChange={(o) => { if (!o) onClose(); }} title={`${prev.label} → ${version.label}`} description={`${version.createdAt} · ${version.sourceLabel}`} size="lg">
      <pre className="cx-scroll max-h-[50vh] overflow-auto whitespace-pre-wrap rounded-xl bg-cx-bg-subtle p-3 font-cx-mono text-[12px] leading-5 text-cx-fg-2">{lines}</pre>
      <div className="flex items-center justify-between gap-2 pt-3">
        <span className="text-[12px] text-cx-fg-4">{version.summary}</span>
        <CopyButton text={lines} label="复制差异" />
      </div>
    </Dialog>
  );
}

function SourceReader({ source, active }: { source: StudioSource; active: boolean }) {
  const state = useStudioState();
  const ui = state.workspace.library;
  const scroll = useRememberedScroll(`library-reader:${source.id}`, active);
  const [content, setContent] = useState<HTMLElement | null>(null);
  const selection = usePassageSelection(content, active);
  const [diffVer, setDiffVer] = useState<AssetVersionSnapshot | null>(null);

  const clues = useMemo(
    () => state.clues.filter((clue) => clue.sourceId === source.id || clue.affectedSourceIds?.includes(source.id)),
    [source.id, state.clues],
  );
  const listedClues = useMemo(() => clues.filter((clue) => !mergeOf(state.merges, "clue", clue.id)), [clues, state.merges]);
  const activeClue = ui.clueId ? clues.find((clue) => clue.id === ui.clueId) : undefined;
  const focusPassageId = ui.passageId || activeClue?.passageId || "";
  const relations = source.relations ?? [];
  const relatedRuns = state.workRuns.filter((r) => r.linkedRefs.some((ref) => ref.kind === "source" && ref.id === source.id));
  const versions = source.versions ?? [];
  const conflicts = source.conflicts ?? [];
  const unresolvedConflicts = conflicts.filter((conf) => !conf.resolvedBy || conf.resolvedBy === "rule").length;
  const workspaceProject = state.workspace.projectId;
  const readerProject = workspaceProject === "all" ? source.projectId : workspaceProject;
  const isUnlinked = Boolean(source.unlinkedTaskIds?.includes(readerProject));

  useViewing(active, readerTarget(ui, source));
  useEffect(() => { studio.markSeen({ kind: "source", id: source.id }); }, [source.id]);
  useEffect(() => { if (activeClue) studio.markSeen({ kind: "clue", id: activeClue.id }); }, [activeClue]);
  useEffect(() => {
    if (workspaceProject !== "all" && workspaceProject !== source.projectId) studio.setWorkspaceProject(source.projectId);
  }, [source.projectId, workspaceProject]);
  useEffect(() => {
    if (!focusPassageId || !active) return;
    const frame = window.requestAnimationFrame(() => {
      document.getElementById(passageDomId(focusPassageId))?.scrollIntoView({ block: "center", behavior: "smooth" });
    });
    return () => window.cancelAnimationFrame(frame);
  }, [active, focusPassageId, ui.clueId]);

  const selectQuote = (id: string) => {
    const clue = clues.find((item) => item.id === id);
    if (clue) studio.setLibraryUi({ passageId: clue.passageId, clueId: clue.id });
  };
  const sourceRef: StudioRef = { kind: "source", id: source.id };
  const backPath = ui.importsOpen ? "导入记录" : ui.view === "duplicates" ? "可能重复" : ui.view === "history" ? "处理记录" : "";

  useEffect(() => {
    if (!active) return;
    const onKey = (event: globalThis.KeyboardEvent) => { if (event.key === "Escape") closeReader(); };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [active]);

  return (
    <div className="flex min-h-0 flex-1 flex-col" data-testid="studio-source-reader">
      <SurfaceToolbar>
        <IconButton size="sm" icon="arrowLeft" label="返回" onClick={closeReader} />
        <PathBreadcrumb path={`${backPath ? `${backPath}/` : ""}${source.id}`} rootLabel="资产" lastIsFile onSelect={closeReader} />
        <Menu ariaLabel="资产操作" className={MENU_HIT} trigger={<IconButton size="sm" icon="more" label="更多" />}>
          <MenuItem icon="pin" onSelect={() => citeRef(sourceRef)}>固定到对话</MenuItem>
          <MenuItem icon="archive" onSelect={() => {
            studio.toggleAssetArchive(source.id, !source.archived, source.archived ? "恢复归档资产" : "人工归档");
            toast({ title: source.archived ? "已恢复资产" : "已归档资产", tone: "success" });
          }}>{source.archived ? "恢复资产" : "归档"}</MenuItem>
          <MenuItem icon="split" onSelect={() => {
            studio.toggleTaskUnlinkAsset(source.id, readerProject);
            toast({ title: isUnlinked ? "已重新关联到当前任务" : "已解除与本任务的关联", tone: "success" });
          }}>{isUnlinked ? "恢复本任务关联" : "解除本任务关联"}</MenuItem>
          <MenuItem icon="upload" onSelect={() => {
            const ver = source.versions?.[0]?.version ?? source.pinnedVersion ?? 46;
            studio.publishOrPinAssetVersion(source.id, ver);
            toast({ title: `已发布固定版本 v${ver}`, tone: "success" });
          }}>发布固定版本</MenuItem>
        </Menu>
      </SurfaceToolbar>
      <div ref={scroll.ref} className="cx-scroll min-h-0 flex-1 overflow-y-auto">
        <article ref={setContent} className="relative mx-auto flex w-full max-w-[760px] flex-col gap-4 p-4 pb-10">
          <SurfaceCard className="p-3">
            <div className="flex items-start gap-3">
              <div className="min-w-0 flex-1">
                <p className="truncate text-[15px] font-semibold text-cx-fg">{assetTitle(source)}</p>
              </div>
              <StudioBadge tone={source.status === "filed" ? "accent" : "neutral"}>{sourceStatusLabel(source.status)}</StudioBadge>
            </div>
            <Dl className="mt-3" items={[
              {
                label: "类型",
                value: (
                  <Select<SourceKind> value={source.kind} onChange={(kind) => studio.setSourceKind(source.id, kind)} size="sm" ariaLabel="资产类型" className="h-7 min-w-[130px] text-[13px]" popoverClassName={OPTION_HIT} options={SOURCE_KINDS.map((item) => ({ value: item.value, label: item.label }))} />
                ),
              },
              {
                label: "重要性",
                value: (
                  <Select<AssetImportance> value={source.importance ?? "normal"} onChange={(imp) => studio.setSourceImportance(source.id, imp)} size="sm" ariaLabel="资产重要性" className="h-7 min-w-[130px] text-[13px]" popoverClassName={OPTION_HIT} options={ASSET_IMPORTANCE_LEVELS} />
                ),
              },
              {
                label: "整理状态",
                value: (
                  <Select<StudioSource["status"]> value={source.status} onChange={(status) => studio.setSourceStatus(source.id, status)} size="sm" ariaLabel="整理状态" className="h-7 min-w-[130px] text-[13px]" popoverClassName={OPTION_HIT} options={SOURCE_STATUSES.map((item) => ({ value: item.value, label: item.label }))} />
                ),
              },
              { label: "规范标识", value: displayTitle(source.canonicalKey ?? source.title), mono: true, copy: source.canonicalKey ?? source.title },
              { label: "别名", value: (source.aliases ?? []).join("、") || "无" },
              { label: "首次观察", value: source.firstSeenAt ?? source.collectedAt },
              { label: "最近观察", value: source.lastObservedAt ?? source.collectedAt },
              ...(state.importOrigins[source.id] ? [{ label: "来源批次", value: <ImportOriginLink sourceId={source.id} /> }] : []),
            ]} />
          </SurfaceCard>

          <MergeInfo kind="source" id={source.id} />

          {conflicts.length ? (
            <div>
              <SectionHeader title="字段冲突" count={unresolvedConflicts || conflicts.length} action={<button type="button" className="text-[12px] font-medium text-cx-accent hover:underline" onClick={() => sendAbout(sourceRef, `请分析资产 ${source.id} 的字段冲突并给出裁决建议。`)}>让 Agent 裁决</button>} />
              <SurfaceCard>
                {conflicts.map((conf) => <ConflictRow key={conf.id} source={source} conf={conf} />)}
              </SurfaceCard>
            </div>
          ) : null}

          {listedClues.length ? (
            <div>
              <SectionHeader title="发现" count={listedClues.length} />
              <SurfaceCard>
                {listedClues.map((clue) => (
                  <SurfaceRow key={clue.id} icon="bookmark" title={displayTitle(clue.title)} subtitle={[categoryLabel(clue.category), statusLabel(clue.status)].join(" · ")} onClick={() => openRef({ kind: "clue", id: clue.id })} />
                ))}
              </SurfaceCard>
            </div>
          ) : null}

          {relations.length || relatedRuns.length ? (
            <div>
              <SectionHeader title="关系" count={relations.length + relatedRuns.length} />
              <SurfaceCard>
                {relations.map((rel) => (
                  <SurfaceRow key={rel.id} icon="link" title={rel.targetLabel} subtitle={`${rel.relationType} · ${{ observed: "观察", derived: "派生", inferred: "推断" }[rel.origin]} · ${rel.observedAt}`} onClick={rel.targetKind === "source" ? () => openSource(rel.targetId) : undefined} />
                ))}
                {relatedRuns.map((r) => (
                  <SurfaceRow key={r.id} icon="terminal" title={displayTitle(r.title)} subtitle={`工作记录 · ${RUN_STATE_LABEL[r.state]}`} onClick={() => openRef({ kind: "run", id: r.id })} />
                ))}
              </SurfaceCard>
            </div>
          ) : null}

          {versions.length ? (
            <div>
              <SectionHeader title="版本" count={versions.length} action={versions.length > 1 ? undefined : undefined} />
              <SurfaceCard>
                {versions.map((v, idx) => (
                  <SurfaceRow
                    key={v.version}
                    icon="history"
                    title={`${v.label}${v.version === source.pinnedVersion ? " · 当前固定" : ""}`}
                    subtitle={`${v.createdAt} · ${v.sourceLabel}`}
                    onClick={idx < versions.length - 1 ? () => setDiffVer(v) : undefined}
                  />
                ))}
              </SurfaceCard>
            </div>
          ) : null}

          <div>
            <SectionHeader title="原文" count={source.passages.length} action={source.passages.length ? <span className="text-[12px] text-cx-fg-4">选中文字可问 AI 或记为发现</span> : undefined} />
            {source.passages.length ? (
              <ol className="flex flex-col gap-1">
                {source.passages.map((passage) => {
                  const passageClues = clues.filter((clue) => clue.passageId === passage.id);
                  const focused = passage.id === focusPassageId;
                  return (
                    <li
                      key={passage.id}
                      id={passageDomId(passage.id)}
                      className={cn("group relative flex scroll-mt-32 gap-3 rounded-xl py-2.5 pl-1 pr-2 transition-colors", focused ? "bg-cx-accent-soft" : "hover:bg-cx-hover")}
                      data-focused={focused || undefined}
                    >
                      <button
                        type="button"
                        className="cx-tabular h-[32px] w-[32px] shrink-0 rounded-lg text-center text-[12px] text-cx-fg-4 outline-none hover:bg-cx-active hover:text-cx-fg focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]"
                        aria-label={`定位到第 ${passage.index} 段`}
                        aria-pressed={focused}
                        onClick={() => studio.setLibraryUi({ passageId: focused && !ui.clueId ? "" : passage.id, clueId: "" })}
                        {...dragRef({ kind: "passage", id: passage.id })}
                      >
                        {passage.index}
                      </button>
                      <p data-passage-text={passage.id} className="min-w-0 flex-1 select-text pt-0.5 text-[15px] leading-7 text-cx-fg">
                        <HighlightedText text={passage.text} quotes={passageClues} activeQuote={activeClue?.id} onSelectQuote={selectQuote} />
                      </p>
                      <AttachButton target={{ kind: "passage", id: passage.id }} small reveal className={cn("shrink-0", focused && "opacity-100")} />
                    </li>
                  );
                })}
              </ol>
            ) : <EmptyState compact icon="book" title="这份资产没有正文" />}
          </div>

          <RecordHistory target={sourceRef} />
          {selection ? <SelectionToolbar selection={selection} source={source} /> : null}
        </article>
      </div>
      {diffVer ? <VersionDiffDialog source={source} version={diffVer} onClose={() => setDiffVer(null)} /> : null}
    </div>
  );
}

export function LibraryWorkspace({ active, sheet = false }: { active: boolean; sheet?: boolean }) {
  const state = useStudioState();
  const ui = state.workspace.library;
  const [collectOpen, setCollectOpen] = useState(false);
  const [importOpen, setImportOpen] = useState(false);
  const source = ui.sourceId ? state.sources.find((item) => item.id === ui.sourceId) : undefined;
  const showImports = ui.importsOpen && !ui.sourceId;
  const projectId = state.workspace.projectId;
  const fallbackProject = projectId === "all"
    ? state.threads[state.activeThreadId]?.projectId ?? state.projects[0]?.id ?? ""
    : projectId;

  useViewing(active && !ui.sourceId && !ui.importsOpen, null);
  const openCollect = useCallback(() => setCollectOpen(true), []);
  const openImport = useCallback(() => setImportOpen(true), []);

  return (
    <div className="flex h-full min-h-0 flex-col" data-testid="studio-library-workspace">
      <div className={cn("flex min-h-0 flex-1 flex-col", (ui.sourceId || ui.importsOpen) && "hidden")}>
        <SourceList active={active && !ui.sourceId && !ui.importsOpen} onCollect={openCollect} onImport={openImport} />
      </div>
      {showImports ? <ImportHistory active={active} onImport={openImport} /> : null}
      {ui.sourceId ? (
        source ? <SourceReader key={source.id} source={source} active={active} /> : (
          <div className="grid min-h-0 flex-1 place-items-center p-6">
            <EmptyState
              icon="book"
              title="资产不存在"
              description="链接指向的资产不在这个浏览器保存的内容里。"
              action={<Button size="md" variant="secondary" onClick={closeReader}>返回资产列表</Button>}
            />
          </div>
        )
      ) : null}
      <SheetAskBar
        sheet={sheet}
        target={source ? readerTarget(ui, source) : null}
        prefill={source ? `关于《${source.title}》：` : undefined}
      />
      <CollectSourceDialog open={collectOpen} onOpenChange={setCollectOpen} projectId={fallbackProject} />
      <ImportSourcesDialog open={importOpen} onOpenChange={setImportOpen} projectId={fallbackProject} />
    </div>
  );
}
