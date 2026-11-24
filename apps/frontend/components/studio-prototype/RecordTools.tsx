"use client";

import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { useVirtualizer } from "@muteki/ui/engines/virtual";
import { cn } from "@/lib/cn";
import { Icon, type IconName } from "@muteki/ui";
import {
  Button,
  Callout,
  Dialog,
  EmptyState,
  IconButton,
  Label,
  Menu,
  MenuItem,
  SegmentedControl,
  Select,
  TextArea,
  TextField,
  toast,
} from "@muteki/ui";
import {
  assetTypeLabel,
  categoryLabel,
  sameRef,
  readinessLabel,
  severityLabel,
  sourceKindLabel,
  statusLabel,
  type StudioRef,
} from "./model";
import {
  RefLinkList,
  StudioBadge,
  displayTitle,
  StudioSectionHeader,
  useRememberedScroll,
} from "./parts";
import { SectionHeader } from "@/components/chat/surfaces/shared";
import { SurfaceCard, SurfaceRow } from "@/components/chat/surfaces/OverviewSurface";
import { cleanDemoText,
  RECORD_NOUN,
  buildAiMergeSuggestions,
  countLabel,
  formatTime,
  historyFor,
  mergeOf,
  mergesInto,
  sourceStatusLabel,
  type AiMergeSuggestion,
  type DuplicateGroup,
  type HistoryAction,
  type HistoryEntry,
  type RecordKind,
} from "./records";
import {
  afterDomainMutation,
  canRevert,
  findPassage,
  hasAttachedDomains,
  readStudioState,
  refLabel,
  refUsage,
  studio,
  useStudioState,
  type StudioState,
} from "./store";



const PAGE_SIZE = 150;

export function useProgressive(total: number, resetKey: string) {
  const [page, setPage] = useState({ key: resetKey, limit: PAGE_SIZE });
  if (page.key !== resetKey) setPage({ key: resetKey, limit: PAGE_SIZE });
  const limit = page.key === resetKey ? page.limit : PAGE_SIZE;
  const more = useCallback(() => setPage((current) => ({ key: current.key, limit: current.limit + PAGE_SIZE })), []);
  return { limit, remaining: Math.max(0, total - limit), more };
}

export function MoreRows({ remaining, onMore, unit }: { remaining: number; onMore: () => void; unit: string }) {
  const ref = useRef<HTMLDivElement | null>(null);
  useEffect(() => {
    const node = ref.current;
    if (!node || !remaining || typeof IntersectionObserver === "undefined") return;
    const observer = new IntersectionObserver((entries) => {
      if (entries.some((entry) => entry.isIntersecting)) onMore();
    });
    observer.observe(node);
    return () => observer.disconnect();
  }, [onMore, remaining]);
  if (!remaining) return null;
  return (
    <div ref={ref} className="flex justify-center py-3">
      <Button size="sm" variant="ghost" onClick={onMore}>
        再显示 {Math.min(remaining, PAGE_SIZE)} {unit}，还有 {remaining} {unit}
      </Button>
    </div>
  );
}

export function VirtualRecordList<T>({
  items,
  scrollNode,
  estimateSize = 68,
  renderItem,
  ariaLabel,
}: {
  items: T[];
  scrollNode: HTMLDivElement | null;
  estimateSize?: number;
  renderItem: (item: T, index: number) => ReactNode;
  ariaLabel?: string;
}) {
  const virtualizer = useVirtualizer({
    count: items.length,
    getScrollElement: () => scrollNode,
    estimateSize: () => estimateSize,
    overscan: 6,
    useAnimationFrameWithResizeObserver: true,
    directDomUpdates: true,
    directDomUpdatesMode: "position",
  });
  const virtualItems = virtualizer.getVirtualItems();

  if (!virtualItems.length || !scrollNode?.clientHeight) {
    return (
      <div role={ariaLabel ? "list" : undefined} aria-label={ariaLabel}>
        {items.slice(0, 14).map((item, index) => (
          <div key={index}>{renderItem(item, index)}</div>
        ))}
      </div>
    );
  }

  return (
    <div
      ref={virtualizer.containerRef}
      role={ariaLabel ? "list" : undefined}
      aria-label={ariaLabel}
      style={{ width: "100%", position: "relative" }}
      data-testid="studio-virtual-list"
    >
      {virtualItems.map((virtualRow) => {
        const item = items[virtualRow.index];
        if (!item) return null;
        return (
          <div
            key={virtualRow.key}
            data-index={virtualRow.index}
            ref={virtualizer.measureElement}
            style={{
              position: "absolute",
              left: 0,
              width: "100%",
            }}
          >
            {renderItem(item, virtualRow.index)}
          </div>
        );
      })}
    </div>
  );
}


export function useChecklist(checked: string[], ordered: string[], apply: (ids: string[]) => void) {
  const anchor = useRef("");
  const set = useMemo(() => new Set(checked), [checked]);
  const toggle = useCallback((id: string, next: boolean, range: boolean) => {
    let ids = [id];
    const pivot = anchor.current || checked[checked.length - 1] || "";
    if (range && pivot && pivot !== id) {
      const from = ordered.indexOf(pivot);
      const to = ordered.indexOf(id);
      if (from >= 0 && to >= 0) ids = ordered.slice(Math.min(from, to), Math.max(from, to) + 1);
    }
    anchor.current = id;
    const touched = new Set(ids);
    apply(next ? [...checked, ...ids.filter((item) => !set.has(item))] : checked.filter((item) => !touched.has(item)));
  }, [apply, checked, ordered, set]);
  const isChecked = useCallback((id: string) => set.has(id), [set]);
  return { isChecked, toggle };
}

export function batchScope(kind: RecordKind, count: number, filterText: string): string {
  return `勾选的 ${countLabel(kind, count)}${filterText ? `（当时的筛选：${filterText}）` : ""}`;
}


export function revertEntry(entryId: string) {
  const entry = readStudioState().history.find((item) => item.id === entryId);
  const outcome = studio.revertHistory(entryId);
  if (hasAttachedDomains()) {
    void afterDomainMutation().then((res) => {
      if (!res.ok) return;
      toast({ title: "已撤销", description: entry?.summary || "已同步服务器状态", tone: "success" });
    });
    return;
  }
  if (!entry || !outcome) {
    toast({ title: "无法撤销", description: "这条记录已经撤销过，或者涉及的记录已经不存在。", tone: "warning" });
    return;
  }
  let description = `${outcome.restored} 项恢复成原来的值。`;
  if (entry.action === "merge") description = `${outcome.restored} 条记录重新单独列出，内容没有变化。`;
  else if (entry.action === "import") description = `已撤回本次导入涉及的 ${outcome.restored} 份资产。`;
  else if (entry.action === "delete") description = `已完整恢复被删除的 ${outcome.restored} 项记录。`;
  else if (entry.createdRefs?.length && !entry.changes?.length) description = `已撤销新建的 ${outcome.restored} 项记录。`;
  else if (entry.dismissKey) description = "这组记录重新列为可能重复。";
  else if (entry.undeterminedKey) description = "已清除无法判断标记，恢复为待裁决候选。";
  else if (outcome.kept) description = `${outcome.restored} 项恢复成原来的值；${outcome.kept} 项之后又改过，保持现在的值。`;
  toast({ title: "已撤销", description, tone: outcome.kept ? "warning" : "success" });
}

export function reportChange(entryId: string | null, unchanged: string) {
  if (!entryId) {
    toast({ title: "没有需要修改的记录", description: unchanged });
    return;
  }
  if (hasAttachedDomains()) {
    void afterDomainMutation().then((res) => {
      if (!res.ok) return;
      const serverEntry = readStudioState().history[0];
      toast({
        title: serverEntry?.summary ?? "已同步",
        tone: "success",
      });
    });
    return;
  }
  const entry = readStudioState().history.find((item) => item.id === entryId);
  toast({
    title: entry?.summary ?? "已修改",
    description: entry?.scope ? `范围：${entry.scope}。已记入处理记录。` : "已记入处理记录。",
    tone: "success",
    duration: 6000,
    action: { label: "撤销", onClick: () => revertEntry(entryId) },
  });
}

export function undoMergeWithFeedback(mergeId: string) {
  const merge = readStudioState().merges.find((item) => item.id === mergeId);
  const ok = studio.undoMerge(mergeId);
  if (hasAttachedDomains()) {
    void afterDomainMutation().then((res) => {
      if (res.ok) toast({ title: "已撤销归并", tone: "success" });
    });
    return;
  }
  if (!ok || !merge) {
    toast({ title: "无法撤销归并", description: "这次归并已经撤销过了。", tone: "warning" });
    return;
  }
  toast({ title: "已撤销归并", description: `${countLabel(merge.kind, merge.memberIds.length)}重新单独列出，内容没有变化。`, tone: "success" });
}


export interface FilterChip {
  key: string;
  label: string;
  onRemove: () => void;
}

export function FilterField({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="flex flex-col">
      <Label>{label}</Label>
      {children}
    </div>
  );
}

export function FilterSelect<V extends string>({ label, value, onChange, options, searchable }: {
  label: string;
  value: V;
  onChange: (value: V) => void;
  options: Array<{ value: V; label: string; description?: string }>;
  searchable?: boolean;
}) {
  return (
    <FilterField label={label}>
      <Select<V>
        value={value}
        onChange={onChange}
        options={options}
        ariaLabel={label}
        searchable={searchable}
      />
    </FilterField>
  );
}

export function FilterChips({ chips, onClear }: { chips: FilterChip[]; onClear: () => void }) {
  if (!chips.length) return null;
  return (
    <div className="flex shrink-0 flex-wrap items-center gap-1.5 px-4 py-2" aria-label="当前筛选" data-testid="studio-filter-chips">
      {chips.map((chip) => (
        <button
          key={chip.key}
          type="button"
          onClick={chip.onRemove}
          aria-label={`去掉筛选：${chip.label}`}
          className="inline-flex h-[32px] max-w-[260px] items-center gap-1 rounded-lg bg-cx-accent-soft px-2.5 text-cx-sm text-cx-fg outline-none hover:bg-cx-active focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]"
        >
          <span className="min-w-0 truncate">{chip.label}</span>
          <Icon name="x" size={13} className="shrink-0 text-cx-fg-3" />
        </button>
      ))}
      {chips.length > 1 ? <Button size="sm" variant="ghost" onClick={onClear}>清除筛选</Button> : null}
    </div>
  );
}


export function BatchTagDialog({ open, onOpenChange, kind, ids, scope }: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  kind: RecordKind;
  ids: string[];
  scope: string;
}) {
  const [mode, setMode] = useState<"add" | "remove" | "replace">("add");
  const [input, setInput] = useState("");
  useEffect(() => {
    if (!open) return;
    setMode("add");
    setInput("");
  }, [open]);

  const tags = input.split(/[,，\s]+/).map((t) => t.trim()).filter(Boolean);
  const canApply = ids.length > 0 && (tags.length > 0 || mode === "replace");

  const submit = () => {
    if (!canApply) return;
    const entryId = studio.batchEditTags(kind, ids, mode, tags, scope);
    onOpenChange(false);
    reportChange(entryId, "勾选记录的标签没有变化。");
  };

  return (
    <Dialog
      open={open}
      onOpenChange={onOpenChange}
      title={`批量编辑标签 · ${countLabel(kind, ids.length)}`}
      description="修改勾选记录的标签，操作可随时在处理记录中一键撤销。"
      icon="hash"
      size="sm"
      footer={(
        <>
          <Button variant="ghost" onClick={() => onOpenChange(false)}>取消</Button>
          <Button variant="primary" disabled={!canApply} onClick={submit}>应用标签变更</Button>
        </>
      )}
    >
      <div className="flex flex-col gap-3">
        <SegmentedControl<"add" | "remove" | "replace">
          value={mode}
          onChange={setMode}
          size="md"
          ariaLabel="标签编辑模式"
          options={[
            { value: "add", label: "追加标签" },
            { value: "remove", label: "移除标签" },
            { value: "replace", label: "替换全部标签" },
          ]}
        />
        <TextField
          label="标签（多个标签用空格或逗号分隔）"
          value={input}
          onChange={(e) => setInput(e.target.value)}
          placeholder="例如：核心网关 重点复核 v46已确认"
          autoFocus
        />
      </div>
    </Dialog>
  );
}

export function BatchLinkDialog({ open, onOpenChange, kind, ids, scope }: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  kind: RecordKind;
  ids: string[];
  scope: string;
}) {
  const state = useStudioState();
  const [targetType, setTargetType] = useState<"task" | "run" | "report">("task");
  const options = useMemo(() => {
    if (targetType === "task") return state.tasks.map((t) => ({ value: t.id, label: t.title, description: `${t.assignee} · ${t.due}` }));
    if (targetType === "run") return state.workRuns.map((r) => ({ value: r.id, label: r.title, description: `G${r.generation} · ${r.state}` }));
    return state.reports.map((rep) => ({ value: rep.id, label: rep.title, description: `v${rep.version} · ${rep.status}` }));
  }, [state.reports, state.tasks, state.workRuns, targetType]);
  const [targetId, setTargetId] = useState("");
  useEffect(() => {
    setTargetId((current) =>
      (open && (!current || !options.some((o) => o.value === current)))
        ? (options[0]?.value ?? "")
        : current);
  }, [options, open]);

  const submit = () => {
    if (!targetId) return;
    const entryId = studio.batchLinkRecords(kind, ids, { kind: targetType, id: targetId }, scope);
    onOpenChange(false);
    reportChange(entryId, "勾选的记录都已经关联在该对象上了。");
  };

  return (
    <Dialog
      open={open}
      onOpenChange={onOpenChange}
      title={`批量关联 · ${countLabel(kind, ids.length)}`}
      description="将勾选记录批量关联到跟进任务、外部工作记录 (Run) 或交付报告。"
      icon="link"
      size="md"
      footer={(
        <>
          <Button variant="ghost" onClick={() => onOpenChange(false)}>取消</Button>
          <Button variant="primary" disabled={!targetId} onClick={submit}>确认关联</Button>
        </>
      )}
    >
      <div className="flex flex-col gap-3">
        <SegmentedControl<"task" | "run" | "report">
          value={targetType}
          onChange={setTargetType}
          size="md"
          ariaLabel="关联目标类型"
          options={[
            { value: "task", label: "跟进任务" },
            { value: "run", label: "工作记录" },
            { value: "report", label: "交付报告" },
          ]}
        />
        <FilterSelect<string>
          label="选择目标对象"
          value={targetId}
          onChange={setTargetId}
          options={options}
          searchable={options.length > 6}
        />
      </div>
    </Dialog>
  );
}

export function BatchExportDialog({ open, onOpenChange, kind, checkedIds, matchingIds }: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  kind: RecordKind;
  checkedIds: string[];
  matchingIds: string[];
}) {
  const state = useStudioState();
  const [rangeMode, setRangeMode] = useState<"checked" | "matching">(checkedIds.length ? "checked" : "matching");
  const [format, setFormat] = useState<"csv" | "json" | "jsonl" | "markdown">("json");

  useEffect(() => {
    if (open) setRangeMode(checkedIds.length ? "checked" : "matching");
  }, [checkedIds.length, open]);

  const targetIds = rangeMode === "checked" && checkedIds.length ? checkedIds : matchingIds;

  const content = useMemo(() => {
    const idSet = new Set(targetIds);
    const csv = (value: unknown) => {
      const text = String(value ?? "");
      return /[",\n\r]/.test(text) ? `"${text.replace(/"/g, '""')}"` : text;
    };
    if (kind === "source") {
      const rows = state.sources.filter((s) => idSet.has(s.id));
      if (format === "csv") {
        const header = "id,canonical_key,asset_type,kind,status,importance,author,collected_at,pinned_version,tags";
        const lines = rows.map((s) =>
          [
            csv(s.id),
            csv(s.canonicalKey ?? s.title),
            csv(assetTypeLabel(s.assetType, s.kind)),
            csv(sourceKindLabel(s.kind)),
            csv(sourceStatusLabel(s.status)),
            csv(s.importance ?? "normal"),
            csv(s.author ?? ""),
            csv(s.collectedAt),
            csv(s.pinnedVersion == null ? "" : `v${s.pinnedVersion}`),
            csv((s.tags ?? []).join(";")),
          ].join(","),
        );
        return [header, ...lines].join("\n");
      }
      if (format === "jsonl") return rows.map((s) => JSON.stringify(s)).join("\n");
      if (format === "markdown") {
        return [
          `# 资产导出清单（共 ${rows.length} 份）`,
          `> 导出快照时间：${new Date().toISOString()}`,
          "",
          ...rows.map((s) => `- **${s.title}** (${assetTypeLabel(s.assetType, s.kind)} · ${sourceStatusLabel(s.status)}${s.pinnedVersion == null ? "" : ` · v${s.pinnedVersion}`}) — 标签：${(s.tags ?? []).join("、") || "无"}`),
        ].join("\n");
      }
      return JSON.stringify({ total: rows.length, exportedAt: new Date().toISOString(), assets: rows }, null, 2);
    }

    const rows = state.clues.filter((c) => idSet.has(c.id));
    if (format === "csv") {
      const header = "id,title,severity,readiness,status,category,source_id,target_version,tags";
      const lines = rows.map((c) =>
        [
          csv(c.id),
          csv(c.title),
          csv(c.severity ?? ""),
          csv(c.readiness ?? "RED"),
          csv(statusLabel(c.status)),
          csv(categoryLabel(c.category)),
          csv(c.sourceId),
          csv(c.targetVersion ?? ""),
          csv((c.tags ?? []).join(";")),
        ].join(","),
      );
      return [header, ...lines].join("\n");
    }
    if (format === "jsonl") return rows.map((c) => JSON.stringify(c)).join("\n");
    if (format === "markdown") {
      return [
        `# 发现导出清单（共 ${rows.length} 条）`,
        `> 导出快照时间：${new Date().toISOString()}`,
        "",
        ...rows.map((c) => `- **${c.title}** [${c.severity ? severityLabel(c.severity) : "未定级"} · ${c.readiness ?? "RED"} · ${statusLabel(c.status)}]\n  - 关联资产：${(c.affectedSourceIds ?? [c.sourceId]).join("、")}\n  - 引文：「${c.quote}」`),
      ].join("\n");
    }
    return JSON.stringify({ total: rows.length, exportedAt: new Date().toISOString(), findings: rows }, null, 2);
  }, [format, kind, state.clues, state.sources, targetIds]);

  const download = () => {
    const ext = format === "markdown" ? "md" : format;
    const mime = format === "csv" ? "text/csv" : format === "markdown" ? "text/markdown" : "application/json";
    const url = URL.createObjectURL(new Blob([content], { type: `${mime};charset=utf-8` }));
    const link = document.createElement("a");
    link.href = url;
    link.download = `src-${kind}-export-${targetIds.length}.${ext}`;
    document.body.appendChild(link);
    link.click();
    link.remove();
    window.setTimeout(() => URL.revokeObjectURL(url), 0);
    onOpenChange(false);
    toast({ title: `已导出 ${countLabel(kind, targetIds.length)} (${format.toUpperCase()})`, tone: "success" });
  };

  return (
    <Dialog
      open={open}
      onOpenChange={onOpenChange}
      title={`批量导出${RECORD_NOUN[kind].noun}`}
      description="支持导出已勾选记录或当前筛选的全部完整结果（跨分页全量导出，含来源与版本快照）。"
      icon="download"
      size="lg"
      footer={(
        <>
          <Button
            variant="secondary"
            icon="copy"
            onClick={() => {
              void navigator.clipboard?.writeText(content);
              toast({ title: "已复制导出内容到剪贴板", tone: "success" });
            }}
          >
            复制内容
          </Button>
          <Button variant="ghost" onClick={() => onOpenChange(false)}>取消</Button>
          <Button variant="primary" icon="download" disabled={!targetIds.length} onClick={download}>
            下载 {format.toUpperCase()} ({targetIds.length})
          </Button>
        </>
      )}
    >
      <div className="flex flex-col gap-3">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <SegmentedControl<"checked" | "matching">
            value={rangeMode}
            onChange={setRangeMode}
            size="md"
            ariaLabel="导出范围"
            options={[
              { value: "checked", label: `已勾选 (${checkedIds.length})`, disabled: !checkedIds.length },
              { value: "matching", label: `筛选出的全部 (${matchingIds.length})` },
            ]}
          />
          <SegmentedControl<"csv" | "json" | "jsonl" | "markdown">
            value={format}
            onChange={setFormat}
            size="md"
            ariaLabel="导出格式"
            options={[
              { value: "json", label: "JSON" },
              { value: "jsonl", label: "JSONL" },
              { value: "csv", label: "CSV" },
              { value: "markdown", label: "Markdown" },
            ]}
          />
        </div>
        <pre className="cx-scroll max-h-[280px] overflow-auto rounded-xl border border-cx-border-subtle bg-cx-bg-subtle p-3 font-cx-mono text-cx-xs leading-5 text-cx-fg-2">
          {content.slice(0, 3200)}
          {content.length > 3200 ? `\n\n/* … 预览显示前 3,200 字符，下载包含全部 ${content.length.toLocaleString()} 字符 … */` : ""}
        </pre>
      </div>
    </Dialog>
  );
}


export interface RecordSummary {
  id: string;
  projectId: string;
  title: string;
  meta: string;
  rank: number;
}

export function describeRecord(state: StudioState, kind: RecordKind, id: string): RecordSummary | null {
  if (kind === "source") {
    const source = state.sources.find((item) => item.id === id);
    if (!source) return null;
    const clues = state.clues.filter((clue) => clue.sourceId === id).length;
    return {
      id,
      projectId: source.projectId,
      title: source.title,
      meta: [assetTypeLabel(source.assetType, source.kind), source.author, `${source.passages.length} 段`, clues ? `${clues} 条发现` : "", sourceStatusLabel(source.status)].filter(Boolean).join(" · "),
      rank: (source.status === "filed" ? 1000 : 0) + clues * 10 + source.passages.length,
    };
  }
  const clue = state.clues.find((item) => item.id === id);
  if (!clue) return null;
  const hit = findPassage(state, clue.passageId);
  const tasks = state.tasks.filter((task) => task.links.some((link) => link.kind === "clue" && link.id === id)).length;
  return {
    id,
    projectId: clue.projectId,
    title: clue.title,
    meta: [hit ? `《${hit.source.title}》第 ${hit.passage.index} 段` : "原文缺失", categoryLabel(clue.category), statusLabel(clue.status)].join(" · "),
    rank: (clue.status === "verified" ? 1000 : 0) + tasks * 10 + clue.tags.length,
  };
}

function usageText(state: StudioState, ref: StudioRef): string {
  const usage = refUsage(state, ref);
  const parts = [
    usage.tasks ? `${usage.tasks} 个任务` : "",
    usage.reports ? `${usage.reports} 份报告` : "",
    usage.threads ? `${usage.threads} 个对话` : "",
    usage.replies ? `${usage.replies} 条回复` : "",
  ].filter(Boolean);
  return parts.length ? `被 ${parts.join("、")}引用` : "";
}

export function MergeMark({ kind, id }: { kind: RecordKind; id: string }) {
  const state = useStudioState();
  if (mergeOf(state.merges, kind, id)) return <StudioBadge icon="listCollapse">已归并</StudioBadge>;
  const count = mergesInto(state.merges, kind, id).reduce((sum, merge) => sum + merge.memberIds.length, 0);
  return count ? <StudioBadge icon="listCollapse">含 {count} {RECORD_NOUN[kind].unit}归并</StudioBadge> : null;
}


export function MergeDialog({ open, onOpenChange, kind, ids, detected = "", onMerged }: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  kind: RecordKind;
  ids: string[];
  detected?: string;
  onMerged?: (primaryId: string) => void;
}) {
  const state = useStudioState();
  const { noun, unit } = RECORD_NOUN[kind];
  const [frozenIds, setFrozenIds] = useState<string[]>(ids);
  const [submitted, setSubmitted] = useState(false);
  const [primaryId, setPrimaryId] = useState("");
  const [reason, setReason] = useState("");
  const [error, setError] = useState("");
  const idsKey = ids.join(",");

  useEffect(() => {
    if (!open) return;
    const current = readStudioState();
    const list = idsKey ? idsKey.split(",") : [];
    setFrozenIds(list);
    setSubmitted(false);
    const best = list
      .map((id) => describeRecord(current, kind, id))
      .filter((item): item is RecordSummary => Boolean(item))
      .sort((a, b) => b.rank - a.rank)[0];
    setPrimaryId(best?.id ?? "");
    setReason(detected);
    setError("");
  }, [detected, idsKey, kind, open]);

  const activeIds = open && !submitted ? ids : frozenIds;
  const records = useMemo(
    () => activeIds.map((id) => describeRecord(state, kind, id)).filter((item): item is RecordSummary => Boolean(item)),
    [activeIds, kind, state],
  );

  const members = records.filter((item) => item.id !== primaryId);
  const projects = new Set(records.map((item) => item.projectId));
  const already = !open || submitted ? undefined : records.find((item) => mergeOf(state.merges, kind, item.id));
  const blocker = !open || submitted
    ? ""
    : records.length < 2
      ? `至少要有两${unit}${noun}才能归并。`
      : projects.size > 1
        ? `勾选的${noun}来自不同项目，只能归并同一项目里的${noun}。`
        : already
          ? `「${already.title}」已经归并到别的${noun}里。请先在它的主记录里撤销那次归并，或者取消勾选它。`
          : "";
  const canMerge = !blocker && Boolean(primaryId) && Boolean(reason.trim());

  const submit = () => {
    if (!canMerge) return;
    setSubmitted(true);
    const result = studio.mergeRecords({ kind, primaryId, memberIds: members.map((item) => item.id), reason, detected });
    if ("error" in result) {
      setSubmitted(false);
      setError(result.error);
      return;
    }
    onOpenChange(false);
    onMerged?.(primaryId);
    toast({
      title: `已把 ${countLabel(kind, members.length)}归并到「${refLabel(readStudioState(), { kind, id: primaryId })}」`,
      description: "出处、原文和引用都保留着，可以在主记录或处理记录里撤销。",
      tone: "success",
      duration: 8000,
      action: { label: "撤销", onClick: () => undoMergeWithFeedback(result.id) },
    });
  };

  return (
    <Dialog
      open={open}
      onOpenChange={onOpenChange}
      title={`归并重复的${noun}`}
      description={`其余${noun}收进主记录，不再单独出现在列表、筛选和 @ 提及里。`}
      icon="listCollapse"
      size="lg"
      testId="studio-merge-dialog"
      footer={(
        <>
          <Button variant="ghost" onClick={() => onOpenChange(false)}>取消</Button>
          <Button variant="primary" disabled={!canMerge} onClick={submit}>
            归并 {members.length} {unit}{noun}
          </Button>
        </>
      )}
    >
      <div className="flex flex-col gap-4">
        {blocker ? <Callout tone="warning" icon="alert">{blocker}</Callout> : null}
        <FilterField label="保留为主记录">
          <Select<string>
            value={primaryId || null}
            onChange={setPrimaryId}
            options={records.map((item) => ({ value: item.id, label: item.title, description: item.meta }))}
            ariaLabel="主记录"
          />
        </FilterField>
        <section aria-label={`收进主记录的${noun}`} className="flex flex-col gap-1">
          <StudioSectionHeader title={`收进主记录的${noun}`} count={members.length} />
          <ul className="cx-scroll flex max-h-[220px] flex-col divide-y divide-cx-border-subtle overflow-y-auto rounded-xl border border-cx-border-subtle">
            {members.map((item) => {
              const usage = usageText(state, { kind, id: item.id });
              return (
                <li key={item.id} className="flex flex-col gap-0.5 px-3 py-2">
                  <span className="truncate text-cx-base text-cx-fg">{item.title}</span>
                  <span className="text-cx-sm leading-5 text-cx-fg-3">{item.meta}{usage ? ` · ${usage}` : ""}</span>
                </li>
              );
            })}
          </ul>
          <p className="text-cx-sm leading-5 text-cx-fg-3">
            {kind === "source"
              ? "它们的原文、发现、导入来源，以及任务和对话里的引用都原样保留，在主记录的「归并进来的资产」里可以打开原文。"
              : "它们的引文、出处段落、标签，以及任务和对话里的引用都原样保留，主记录的详情会列出它们的原文。"}
          </p>
        </section>
        <TextArea
          label="归并原因"
          value={reason}
          onChange={(event) => { setReason(event.target.value); setError(""); }}
          placeholder="例如：同一主资产上的同一网关鉴权配置问题"
          description="必填。会显示在主记录和处理记录里，撤销时也看得到。"
          className="min-h-[88px] text-cx-base leading-6"
        />
        {error ? <Callout tone="danger" icon="circleAlert">{error}</Callout> : null}
      </div>
    </Dialog>
  );
}

export function MergeInfo({ kind, id, memberDetail }: {
  kind: RecordKind;
  id: string;
  memberDetail?: (memberId: string) => ReactNode;
}) {
  const state = useStudioState();
  const into = mergeOf(state.merges, kind, id);
  const merged = mergesInto(state.merges, kind, id);
  if (!into && !merged.length) return null;
  const { noun, unit } = RECORD_NOUN[kind];
  const total = merged.reduce((sum, merge) => sum + merge.memberIds.length, 0);
  return (
    <div className="flex flex-col gap-3" data-testid="studio-merge-info">
      {into ? (
        <Callout
          tone="accent"
          icon="listCollapse"
          title={`已归并到「${refLabel(state, { kind, id: into.primaryId })}」`}
          action={(
            <div className="flex flex-wrap gap-1.5">
              <RefLinkList targets={[{ kind, id: into.primaryId }]} />
              <Button size="sm" variant="ghost" icon="undo" onClick={() => undoMergeWithFeedback(into.id)}>撤销归并</Button>
            </div>
          )}
        >
          归并原因：{into.reason}（{formatTime(into.at)}）。它的内容和引用都保留着，只是不再单独列出。
        </Callout>
      ) : null}
      {merged.length ? (
        <section aria-label={`归并进来的${noun}`} className="flex flex-col gap-2 rounded-2xl border border-cx-border-subtle p-3" data-testid="studio-merged-members">
          <StudioSectionHeader title={`归并进来的${noun}`} count={total} />
          {merged.map((merge) => (
            <div key={merge.id} className="flex flex-col gap-1.5">
              <p className="text-cx-sm leading-5 text-cx-fg-2">
                归并原因：{merge.reason}
                <span className="text-cx-fg-3"> · {merge.memberIds.length} {unit} · {formatTime(merge.at)}</span>
              </p>
              {merge.detected && merge.detected !== merge.reason ? (
                <p className="text-cx-sm leading-5 text-cx-fg-3">重复检查发现：{merge.detected}</p>
              ) : null}
              {memberDetail ? (
                <div className="flex flex-col gap-1.5">{merge.memberIds.map((memberId) => <div key={memberId}>{memberDetail(memberId)}</div>)}</div>
              ) : (
                <RefLinkList targets={merge.memberIds.map((memberId) => ({ kind, id: memberId }))} max={4} />
              )}
              <div>
                <Button size="sm" variant="ghost" icon="undo" onClick={() => undoMergeWithFeedback(merge.id)}>撤销这次归并</Button>
              </div>
            </div>
          ))}
        </section>
      ) : null}
    </div>
  );
}


const DUPLICATE_RULES: Record<RecordKind, string> = {
  source: "同一项目里标题相同、正文相同或共享后端网关指纹的资产",
  clue: "同一项目里引文相同、标题相同、摘自同一段原文，或共同关联主资产且属同类问题的发现",
};

function UndeterminedDialog({ open, onOpenChange, group }: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  group: DuplicateGroup | null;
}) {
  const [reason, setReason] = useState("");
  const [missingNote, setMissingNote] = useState("");
  useEffect(() => {
    if (!open || !group) return;
    setReason(group.undetermined?.reason ?? `候选存在相似特征（${group.reasons.join("；")}），但尚不能确认是否同一根因修复点`);
    setMissingNote(group.undetermined?.missingNote ?? "需补充后端网关路由配置或双身份对照响应后再决定归并或保持独立");
  }, [group, open]);

  const submit = () => {
    if (!group) return;
    const entryId = studio.markDuplicatesUndetermined(group.kind, group.ids, reason, missingNote);
    onOpenChange(false);
    if (entryId) {
      toast({
        title: "已标为「无法判断」",
        description: "保留在「无法判断」筛选下，待补齐材料后可随时重新对照归并。",
        tone: "warning",
        action: { label: "撤销", onClick: () => revertEntry(entryId) },
      });
    }
  };

  return (
    <Dialog
      open={open}
      onOpenChange={onOpenChange}
      title="标为无法判断"
      description="现有证据不足以断定是同一根因还是独立问题时，记录原因与待补材料，不强行合并或排除。"
      icon="help"
      size="md"
      footer={(
        <>
          <Button variant="ghost" onClick={() => onOpenChange(false)}>取消</Button>
          <Button variant="primary" onClick={submit}>保存「无法判断」状态</Button>
        </>
      )}
    >
      <div className="flex flex-col gap-3">
        <TextArea
          label="无法判断的原因"
          value={reason}
          onChange={(e) => setReason(e.target.value)}
          className="min-h-[80px] text-cx-base"
        />
        <TextArea
          label="还需要补充什么材料才能判断？"
          value={missingNote}
          onChange={(e) => setMissingNote(e.target.value)}
          className="min-h-[80px] text-cx-base"
        />
      </div>
    </Dialog>
  );
}

function DuplicateCompareDialog({ open, onOpenChange, group, onMerge, onUndetermined, onDismiss }: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  group: DuplicateGroup | null;
  onMerge: (group: DuplicateGroup, primaryId: string) => void;
  onUndetermined: (group: DuplicateGroup) => void;
  onDismiss: (group: DuplicateGroup) => void;
}) {
  const state = useStudioState();
  const [selectedPrimary, setSelectedPrimary] = useState("");
  const mergeRef = useRef<HTMLButtonElement | null>(null);
  useEffect(() => {
    if (open && group?.ids.length) setSelectedPrimary(group.ids[0]);
  }, [group, open]);

  if (!group) return null;
  const { kind, ids } = group;
  const normalize = (id: string, value: string) => cleanDemoText(value.replace(new RegExp(`^${id.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}\\s*·\\s*`), ""));

  return (
    <Dialog
      open={open}
      onOpenChange={onOpenChange}
      title="对照审阅"
      description={group.reasons.join("；")}
      icon="columns"
      size="lg"
      testId="studio-duplicate-compare-dialog"
      initialFocusRef={mergeRef}
      footer={(
        <>
          <Button variant="ghost" icon="eyeOff" onClick={() => { onOpenChange(false); onDismiss(group); }}>
            不是重复
          </Button>
          <Button variant="secondary" icon="help" onClick={() => { onOpenChange(false); onUndetermined(group); }}>
            无法判断…
          </Button>
          <Button
            variant="primary"
            icon="listCollapse"
            ref={mergeRef}
            onClick={() => {
              onOpenChange(false);
              onMerge(group, selectedPrimary || ids[0]);
            }}
          >
            归并到「{selectedPrimary || ids[0]}」
          </Button>
        </>
      )}
    >
      <div className="flex flex-col gap-4">
        <div>
          <SectionHeader title="主记录" />
          <SurfaceCard>
            {ids.map((id) => {
              const primary = (selectedPrimary || ids[0]) === id;
              const c = kind === "clue" ? state.clues.find((x) => x.id === id) : null;
              const src = kind === "source" ? state.sources.find((x) => x.id === id) : null;
              const title = displayTitle(c?.title ?? src?.title ?? id);
              const subtitle = c
                ? [id, statusLabel(c.status), c.readiness ? readinessLabel(c.readiness) : null, c.supportingEvidenceIds?.length ? `${c.supportingEvidenceIds.length} 份证据` : null].filter(Boolean).join(" · ")
                : [id, src ? assetTypeLabel(src.assetType, src.kind) : null, src?.lastObservedAt ? `最近观察 ${src.lastObservedAt}` : null].filter(Boolean).join(" · ");
              return (
                <SurfaceRow
                  key={id}
                  icon={kind === "clue" ? "bookmark" : "book"}
                  title={title}
                  subtitle={subtitle}
                  selected={primary}
                  trailing={primary ? <span className="shrink-0 text-cx-xs font-medium text-cx-accent">主记录</span> : <span />}
                  onClick={() => setSelectedPrimary(id)}
                />
              );
            })}
          </SurfaceCard>
        </div>
        {(() => {
          const fields: Array<{ label: string; values: Map<string, string> }> = [];
          const primary = selectedPrimary || ids[0];
          if (kind === "clue") {
            const get = (id: string) => state.clues.find((x) => x.id === id);
            fields.push(
              { label: "标题", values: new Map(ids.map((id) => [id, normalize(id, get(id)?.title ?? "—")])) },
              { label: "状态 / 准备度", values: new Map(ids.map((id) => { const c = get(id); return [id, c ? [statusLabel(c.status), c.readiness, c.severity ? severityLabel(c.severity) : null].filter(Boolean).join(" · ") : "—"]; })) },
              { label: "关联资产", values: new Map(ids.map((id) => [id, (get(id)?.affectedSourceIds ?? [get(id)?.sourceId]).filter(Boolean).join("、") || "—"])) },
              { label: "原文引文", values: new Map(ids.map((id) => [id, `「${normalize(id, get(id)?.quote ?? "—")}」`])) },
              { label: "支撑证据与版本", values: new Map(ids.map((id) => { const c = get(id); return [id, [(c?.supportingEvidenceIds ?? []).join("、") || "无", c?.targetVersion || "版本未指定"].join(" · ")]; })) },
              { label: "影响与复核人", values: new Map(ids.map((id) => { const c = get(id); return [id, `${c?.impactSummary ?? "—"}（复核人：${c?.reviewer ?? "未指派"}）`]; })) },
            );
          } else {
            const get = (id: string) => state.sources.find((x) => x.id === id);
            fields.push(
              { label: "资产标题 / 标识", values: new Map(ids.map((id) => [id, normalize(id, get(id)?.title ?? "—")])) },
              { label: "类型与状态", values: new Map(ids.map((id) => { const s2 = get(id); return [id, s2 ? `${assetTypeLabel(s2.assetType, s2.kind)} · ${sourceStatusLabel(s2.status)}` : "—"]; })) },
              { label: "别名与网关指纹", values: new Map(ids.map((id) => { const s2 = get(id); return [id, `别名：${(s2?.aliases ?? []).join("、") || "无"}；技术栈：${s2?.attributes?.["技术栈指纹"] ?? "—"}`]; })) },
              { label: "首段摘要", values: new Map(ids.map((id) => [id, get(id)?.passages[0]?.text ?? "—"])) },
            );
          }
          const diff = fields.filter((f) => new Set([...f.values.values()]).size > 1);
          const same = fields.filter((f) => new Set([...f.values.values()]).size <= 1).map((f) => f.label);
          return (
            <>
              <div className="flex flex-col gap-4">
                {diff.length ? diff.map((f) => (
                  <div key={f.label}>
                    <SectionHeader title={f.label} />
                    <SurfaceCard>
                      {ids.map((id) => (
                        <SurfaceRow key={id} icon={kind === "clue" ? "bookmark" : "book"} title={<span className="whitespace-normal">{f.values.get(id) ?? "—"}</span>} subtitle={id} selected={id === primary} />
                      ))}
                    </SurfaceCard>
                  </div>
                )) : (
                  <SurfaceCard><SurfaceRow icon="check" title="所有对照维度一致" /></SurfaceCard>
                )}
              </div>
              {same.length ? <p className="text-cx-xs text-cx-fg-4">相同：{same.join("、")}</p> : null}
            </>
          );
        })()}
      </div>
    </Dialog>
  );
}

export function DuplicatesView({ kind, groups, active, onOpen }: {
  kind: RecordKind;
  groups: DuplicateGroup[];
  active: boolean;
  onOpen: (id: string) => void;
}) {
  const state = useStudioState();
  const scroll = useRememberedScroll(`${kind}-duplicates`, active);
  const [filterMode, setFilterMode] = useState<"all" | "pending" | "undetermined">("all");
  const [merging, setMerging] = useState<DuplicateGroup | null>(null);
  const [mergeOpen, setMergeOpen] = useState(false);
  const [comparing, setComparing] = useState<DuplicateGroup | null>(null);
  const [compareOpen, setCompareOpen] = useState(false);
  const [undeterminedGroup, setUndeterminedGroup] = useState<DuplicateGroup | null>(null);
  const [undeterminedOpen, setUndeterminedOpen] = useState(false);
  const { noun } = RECORD_NOUN[kind];

  const aiSuggestions = useMemo(
    () => buildAiMergeSuggestions(groups.filter((g) => !g.undetermined), state.sources, state.clues),
    [groups, state.clues, state.sources],
  );

  const filteredGroups = useMemo(() => {
    if (filterMode === "pending") return groups.filter((g) => !g.undetermined);
    if (filterMode === "undetermined") return groups.filter((g) => Boolean(g.undetermined));
    return groups;
  }, [filterMode, groups]);

  const page = useProgressive(filteredGroups.length, `${kind}:${filterMode}`);

  const dismiss = (group: DuplicateGroup) => {
    const entryId = studio.dismissDuplicates(kind, group.ids);
    if (!entryId) return;
    toast({
      title: "已标为保持独立（不是重复）",
      description: "这组记录不再列出；之后有新的相似记录加入时会重新提示。",
      action: { label: "撤销", onClick: () => revertEntry(entryId) },
    });
  };

  const adoptAiMerge = (suggestion: AiMergeSuggestion) => {
    const res = studio.mergeRecords({
      kind: suggestion.kind,
      primaryId: suggestion.primaryId,
      memberIds: suggestion.memberIds,
      reason: suggestion.rationale,
      detected: "AI 归并建议逐项采纳",
    });
    if ("error" in res) {
      toast({ title: "无法采纳归并", description: res.error, tone: "danger" });
      return;
    }
    toast({
      title: `已采纳 AI 建议：归并到「${suggestion.primaryId}」`,
      description: "来源、原文和引用全部保留，可随时撤销。",
      tone: "success",
      action: { label: "撤销", onClick: () => undoMergeWithFeedback(res.id) },
    });
  };

  const adoptAllAiMerges = () => {
    let count = 0;
    aiSuggestions.slice(0, 5).forEach((s) => {
      const res = studio.mergeRecords({
        kind: s.kind,
        primaryId: s.primaryId,
        memberIds: s.memberIds,
        reason: s.rationale,
        detected: "AI 归并建议批量采纳",
      });
      if (!("error" in res)) count += 1;
    });
    if (count) {
      toast({ title: `已批量采纳 ${count} 组 AI 归并建议`, description: "可在「处理记录」中逐组查看或撤销。", tone: "success" });
    }
  };

  const undeterminedCount = groups.filter((g) => Boolean(g.undetermined)).length;
  const pendingCount = groups.length - undeterminedCount;

  return (
    <div ref={scroll.ref} className="cx-scroll min-h-0 flex-1 overflow-y-auto px-4 pb-4" data-testid={`studio-${kind}-duplicates`}>
      <div className="flex flex-wrap items-center justify-between gap-2 py-2">
        <p className="text-cx-sm leading-5 text-cx-fg-3">
          {DUPLICATE_RULES[kind]}
        </p>
        <SegmentedControl<"all" | "pending" | "undetermined">
          value={filterMode}
          onChange={setFilterMode}
          size="md"
          ariaLabel="筛选重复候选状态"
          options={[
            { value: "all", label: "全部", badge: <span className="cx-tabular text-cx-fg-3">{groups.length}</span> },
            { value: "pending", label: "待裁决", badge: <span className="cx-tabular text-cx-fg-3">{pendingCount}</span> },
            { value: "undetermined", label: "无法判断", badge: <span className="cx-tabular text-cx-fg-3">{undeterminedCount}</span> },
          ]}
        />
      </div>

      {aiSuggestions.length > 0 && filterMode !== "undetermined" ? (
        <section
          aria-label="AI 归并建议"
          className="flex flex-col gap-1 rounded-2xl bg-cx-elevated p-1.5 shadow-cx-sm"
          data-testid="studio-ai-merge-suggestions"
        >
          <div className="flex items-center justify-between gap-2 px-3 pb-1 pt-2.5">
            <span className="text-cx-xs font-medium text-cx-fg-2">AI 归并建议 {aiSuggestions.length} 组</span>
            {aiSuggestions.length > 1 ? (
              <button type="button" className="text-cx-xs font-medium text-cx-accent hover:underline" onClick={adoptAllAiMerges}>全部采纳</button>
            ) : null}
          </div>
          {aiSuggestions.slice(0, 3).map((sug) => {
            const grp = groups.find((g) => g.key === sug.groupKey);
            return (
              <SurfaceRow
                key={sug.id}
                icon="sparkles"
                title={`保留 ${sug.primaryId} · 并入 ${sug.memberIds.join("、")}`}
                subtitle={sug.rationale}
                trailing={(
                  <span className="flex items-center gap-2">
                    {grp ? <button type="button" className="text-cx-xs text-cx-fg-3 hover:text-cx-fg" onClick={() => { setComparing(grp); setCompareOpen(true); }}>对照</button> : null}
                    <button type="button" className="text-cx-xs font-medium text-cx-accent hover:underline" onClick={() => adoptAiMerge(sug)}>采纳</button>
                  </span>
                )}
              />
            );
          })}
        </section>
      ) : null}

      {filteredGroups.length ? (
        <>
          <div className="flex flex-col gap-4">
            {filteredGroups.slice(0, page.limit).map((group) => (
              <div key={group.key} data-testid="studio-duplicate-group">
                <SectionHeader
                  title={`${group.reasons.join("、")}`}
                  count={countLabel(kind, group.ids.length)}
                  action={(
                    <Menu ariaLabel="重复组操作" trigger={<IconButton size="sm" icon="more" label="更多" />}>
                      <MenuItem icon="columns" onSelect={() => { setComparing(group); setCompareOpen(true); }}>对照审阅…</MenuItem>
                      <MenuItem icon="listCollapse" onSelect={() => { setMerging(group); setMergeOpen(true); }}>归并…</MenuItem>
                      <MenuItem icon="help" onSelect={() => { setUndeterminedGroup(group); setUndeterminedOpen(true); }}>{group.undetermined ? "修改无法判断说明…" : "无法判断…"}</MenuItem>
                      <MenuItem icon="eyeOff" onSelect={() => dismiss(group)}>不是重复</MenuItem>
                    </Menu>
                  )}
                />
                {group.undetermined ? (
                  <Callout tone="warning" title={`无法判断原因（${group.undetermined.by}）`} className="mb-1.5">
                    {group.undetermined.reason} · 待补：{group.undetermined.missingNote}
                  </Callout>
                ) : null}
                <SurfaceCard>
                  {group.ids.map((id) => {
                    const record = describeRecord(state, kind, id);
                    if (!record) return null;
                    return <SurfaceRow key={id} icon="copy" title={displayTitle(record.title)} subtitle={record.meta} onClick={() => onOpen(id)} />;
                  })}
                </SurfaceCard>
              </div>
            ))}
          </div>
          <MoreRows remaining={page.remaining} onMore={page.more} unit="组" />
        </>
      ) : (
        <EmptyState
          compact
          icon="copy"
          title={filterMode === "undetermined" ? "没有标为「无法判断」的候选组" : `没有可能重复的${noun}`}
          description={`${DUPLICATE_RULES[kind]}会列在这里。已标为不是重复的组不再出现。`}
        />
      )}
      <MergeDialog
        open={mergeOpen}
        onOpenChange={setMergeOpen}
        kind={kind}
        ids={merging?.ids ?? []}
        detected={merging?.reasons.join("；") ?? ""}
      />
      <DuplicateCompareDialog
        open={compareOpen}
        onOpenChange={setCompareOpen}
        group={comparing}
        onMerge={(grp) => { setMerging(grp); setMergeOpen(true); }}
        onUndetermined={(grp) => { setUndeterminedGroup(grp); setUndeterminedOpen(true); }}
        onDismiss={dismiss}
      />
      <UndeterminedDialog
        open={undeterminedOpen}
        onOpenChange={setUndeterminedOpen}
        group={undeterminedGroup}
      />
    </div>
  );
}


const ACTION_ICON: Record<HistoryAction, IconName> = {
  create: "plus",
  import: "upload",
  status: "checkCircle",
  category: "layers",
  merge: "listCollapse",
  unmerge: "split",
  dismiss: "eyeOff",
  undetermined: "help",
  revert: "undo",
  adopt: "sparkles",
  task: "listTodo",
  delete: "trash",
  tag: "hash",
  link: "link",
  archive: "archive",
  conflict: "gitCompare",
  report: "file",
  approval: "shield",
  monitor: "activity",
  retest: "refresh",
  plan: "compass",
};

export function HistoryRow({ entry, compact = false }: { entry: HistoryEntry; compact?: boolean }) {
  const state = useStudioState();
  const undoable = canRevert(state, entry);
  const refs = useMemo(
    () => entry.refs.filter((ref, index, list) => list.findIndex((item) => sameRef(item, ref)) === index),
    [entry.refs],
  );
  const meta = [formatTime(entry.at), entry.scope, entry.revertedBy ? "已撤销" : ""].filter(Boolean).join(" · ");
  return (
    <li className="flex gap-3 rounded-xl px-3 py-2.5 hover:bg-cx-hover" data-testid="studio-history-entry">
      <Icon name={ACTION_ICON[entry.action] ?? "history"} size={16} className="mt-1 shrink-0 text-cx-fg-3" />
      <div className="flex min-w-0 flex-1 flex-col gap-1">
        <p className={cn("break-words text-cx-base leading-6", entry.revertedBy ? "text-cx-fg-3 line-through decoration-cx-fg-4" : "text-cx-fg")}>{entry.summary}</p>
        <p className="break-words text-cx-sm leading-5 text-cx-fg-3">{meta}</p>
        {!compact && refs.length ? <RefLinkList targets={refs} max={3} /> : null}
      </div>
      {undoable ? (
        <Button size="sm" variant="ghost" icon="undo" className="shrink-0 self-start" onClick={() => revertEntry(entry.id)}>撤销</Button>
      ) : null}
    </li>
  );
}

export function HistoryView({ kind, entries, active }: { kind: RecordKind; entries: HistoryEntry[]; active: boolean }) {
  const scroll = useRememberedScroll(`${kind}-history`, active);
  const [undoableOnly, setUndoableOnly] = useState(false);
  const state = useStudioState();
  const shown = useMemo(() => (undoableOnly ? entries.filter((entry) => canRevert(state, entry)) : entries), [entries, state, undoableOnly]);
  const page = useProgressive(shown.length, `${kind}:${undoableOnly}`);
  return (
    <div ref={scroll.ref} className="cx-scroll min-h-0 flex-1 overflow-y-auto px-2 pb-4" data-testid={`studio-${kind}-history`}>
      {entries.length ? (
        <>
          <div className="flex flex-wrap items-center gap-2 px-3 py-2">
            <p className="min-w-0 flex-1 text-cx-sm leading-5 text-cx-fg-3">
              完整保留全部处理记录（无 2,000 条截断）。新建资料、文件导入、新建任务、批量修改、批量删除、标签编辑、归并和重复裁决均可一键撤销。
            </p>
            <Button size="sm" variant={undoableOnly ? "soft" : "ghost"} aria-pressed={undoableOnly} onClick={() => setUndoableOnly((value) => !value)}>
              只看可撤销的
            </Button>
          </div>
          {shown.length ? (
            <ol className="flex flex-col">
              {shown.slice(0, page.limit).map((entry) => <HistoryRow key={entry.id} entry={entry} />)}
            </ol>
          ) : (
            <EmptyState compact icon="undo" title="没有可以撤销的记录" />
          )}
          <MoreRows remaining={page.remaining} onMore={page.more} unit="条" />
        </>
      ) : (
        <EmptyState compact icon="history" title="还没有处理记录" description={`录入、导入、批量修改、删除和归并${RECORD_NOUN[kind].noun}都会记在这里，可以查看和撤销。`} />
      )}
    </div>
  );
}

const DETAIL_HISTORY = 4;

export function RecordHistory({ target }: { target: StudioRef }) {
  const state = useStudioState();
  const entries = historyFor(state.history, target);
  const [all, setAll] = useState(false);
  if (!entries.length) return null;
  const shown = all ? entries : entries.slice(0, DETAIL_HISTORY);
  return (
    <section aria-label="处理记录" className="flex flex-col gap-1" data-testid="studio-record-history">
      <StudioSectionHeader title="处理记录" count={entries.length} />
      <ol className="-mx-3 flex flex-col">
        {shown.map((entry) => <HistoryRow key={entry.id} entry={entry} compact />)}
      </ol>
      {entries.length > DETAIL_HISTORY ? (
        <Button size="sm" variant="ghost" className="self-start" onClick={() => setAll((value) => !value)}>
          {all ? "收起" : `显示全部 ${entries.length} 条`}
        </Button>
      ) : null}
    </section>
  );
}

export function scopedHistory(history: HistoryEntry[], kind: RecordKind, records: Array<{ id: string; projectId: string }>, projectId: string): HistoryEntry[] {
  const projects = new Map(records.map((record) => [record.id, record.projectId]));
  return history.filter((entry) => entry.refs.some((ref) => (
    ref.kind === kind && (projectId === "all" || !projects.has(ref.id) || projects.get(ref.id) === projectId)
  )));
}
