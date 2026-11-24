"use client";

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { cn } from "@/lib/cn";
import { Icon } from "@muteki/ui";
import {
  Button,
  Callout,
  Checkbox,
  Collapse,
  Dialog,
  EmptyState,
  IconButton,
  Input,
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
  useContextMenu,
} from "@muteki/ui";
import { CodeBlock } from "@muteki/ui/agent";
import { TodoList, type TodoItem } from "@muteki/ui/agent";
import { SectionHeader, SurfaceToolbar } from "@/components/chat/surfaces/shared";
import { SurfaceCard, SurfaceRow } from "@/components/chat/surfaces/OverviewSurface";
import { PathBreadcrumb } from "@/components/chat/surfaces/PathBreadcrumb";
import { STUDIO_TEAM } from "./constants";
import {
  CLUE_CATEGORIES,
  CLUE_STATUSES,
  EVIDENCE_KIND_LABELS,
  FINDING_SEVERITIES,
  READINESS_LEVELS,
  categoryLabel,
  severityLabel,
  statusLabel,
  statusTone,
  type ClueCategory,
  type ClueStatus,
  type EvidenceKind,
  type FindingClaimItem,
  type FindingSeverity,
  type ReadinessLevel,
  type StudioClue,
  type StudioEvidence,
  type StudioRef,
} from "./model";
import { openRef, openRefInReader } from "./nav";
import {
  HighlightedText,
  NewMark,
  RefLinkList,
  SheetAskBar,
  StudioBadge,
  askAi,
  displayTitle,
  dragRef,
  quoteToComposer,
  useRememberedScroll,
  useViewing,
} from "./parts";
import {
  EMPTY_CLUE_FILTER,
  countLabel,
  findClueDuplicates,
  mergeOf,
  mergesInto,
  pickClueFilter,
  sameFilter,
  type ClueFilter,
  type MergeRecord,
} from "./records";
import {
  BatchExportDialog,
  BatchLinkDialog,
  BatchTagDialog,
  DuplicatesView,
  FilterChips,
  FilterField,
  FilterSelect,
  HistoryView,
  MergeDialog,
  MergeInfo,
  MergeMark,
  RecordHistory,
  batchScope,
  reportChange,
  scopedHistory,
  useChecklist,
  type FilterChip,
} from "./RecordTools";
import { Dl, sendAbout } from "./Kit";
import { postCommand, querySrc } from "../src/reviewPrimitives";
import { useSrcResource } from "../src/ui";
import { getEvidencePagination, loadMoreFindingEvidences, readBrowserScreenshotImage } from "./real/findings";
import { afterDomainMutation, attachedTaskId, findPassage, hasAttachedDomains, readStudioState, studio, useStudioState, type StudioState } from "./store";

function matches(clue: StudioClue, query: string, sourceTitle: string): boolean {
  if (!query) return true;
  const q = query.toLowerCase();
  return clue.title.toLowerCase().includes(q)
    || clue.quote.toLowerCase().includes(q)
    || sourceTitle.toLowerCase().includes(q)
    || (clue.impactSummary ?? "").toLowerCase().includes(q)
    || (clue.reviewer ?? "").toLowerCase().includes(q)
    || clue.tags.some((tag) => tag.toLowerCase().includes(q));
}

function clueMatches(
  clue: StudioClue,
  filter: ClueFilter,
  sourceTitle: string,
  merges: MergeRecord[],
  withTask: Set<string>,
  withReport: Map<string, "frozen" | "draft">,
  keepMerged: boolean,
): boolean {
  if (!filter.showMerged && !keepMerged && mergeOf(merges, "clue", clue.id)) return false;
  if (filter.category !== "all" && clue.category !== filter.category) return false;
  if (filter.severity && filter.severity !== "all" && clue.severity !== filter.severity) return false;
  if (filter.readiness && filter.readiness !== "all" && (clue.readiness ?? "YELLOW") !== filter.readiness) return false;
  if (filter.tag && !clue.tags.includes(filter.tag)) return false;
  if (filter.sourceId && clue.sourceId !== filter.sourceId && !(clue.affectedSourceIds?.includes(filter.sourceId))) return false;
  if (filter.task === "with" && !withTask.has(clue.id)) return false;
  if (filter.task === "without" && withTask.has(clue.id)) return false;
  if (filter.report && filter.report !== "all") {
    const repState = withReport.get(clue.id);
    if (filter.report === "with" && !repState) return false;
    if (filter.report === "frozen" && repState !== "frozen") return false;
    if (filter.report === "without" && repState) return false;
  }
  if (filter.runId && clue.sourceRunId !== filter.runId) return false;
  if (filter.stale === "stale" && !clue.stale) return false;
  if (filter.stale === "fresh" && clue.stale) return false;
  if (filter.timeRange && filter.timeRange !== "all") {
    if (filter.timeRange === "stale") {
      if (!clue.stale) return false;
    } else {
      const ts = Date.parse(clue.reviewedAt || "");
      const days = filter.timeRange === "24h" ? 1 : filter.timeRange === "7d" ? 7 : 30;
      if (!ts || Date.now() - ts > days * 24 * 3600 * 1000) return false;
    }
  }
  return matches(clue, filter.query.trim(), sourceTitle);
}

function clueChips(state: StudioState, filter: ClueFilter): FilterChip[] {
  const set = (patch: Partial<ClueFilter>) => () => studio.setClueFilter(patch);
  const chips: FilterChip[] = [];
  if (filter.category !== "all") chips.push({ key: "category", label: `分类：${categoryLabel(filter.category)}`, onRemove: set({ category: "all" }) });
  if (filter.severity && filter.severity !== "all") chips.push({ key: "severity", label: `重要性：${severityLabel(filter.severity)}`, onRemove: set({ severity: "all" }) });
  if (filter.readiness && filter.readiness !== "all") chips.push({ key: "readiness", label: `准备度：${filter.readiness}`, onRemove: set({ readiness: "all" }) });
  if (filter.report && filter.report !== "all") {
    const lbl = filter.report === "frozen" ? "已有冻结报告" : filter.report === "with" ? "已关联报告" : "尚无报告";
    chips.push({ key: "report", label: lbl, onRemove: set({ report: "all" }) });
  }
  if (filter.runId) chips.push({ key: "runId", label: `工作记录：${filter.runId}`, onRemove: set({ runId: "" }) });
  if (filter.stale && filter.stale !== "all") chips.push({ key: "stale", label: filter.stale === "stale" ? "陈旧待复核" : "状态新鲜", onRemove: set({ stale: "all" }) });
  if (filter.timeRange && filter.timeRange !== "all") chips.push({ key: "timeRange", label: `时间：${filter.timeRange}`, onRemove: set({ timeRange: "all" }) });
  if (filter.tag) chips.push({ key: "tag", label: `标签：${filter.tag}`, onRemove: set({ tag: "" }) });
  if (filter.sourceId) {
    const title = state.sources.find((source) => source.id === filter.sourceId)?.title ?? "已不存在的资产";
    chips.push({ key: "source", label: `出处：《${title}》`, onRemove: set({ sourceId: "" }) });
  }
  if (filter.task !== "all") chips.push({ key: "task", label: filter.task === "with" ? "已有任务" : "还没有任务", onRemove: set({ task: "all" }) });
  if (filter.showMerged) chips.push({ key: "merged", label: "含已归并的", onRemove: set({ showMerged: false }) });
  return chips;
}

export function createTaskFromClues(clues: StudioClue[]) {
  if (!clues.length) return;
  const state = readStudioState();
  const first = clues[0];
  const id = studio.addTask({
    projectId: first.projectId,
    title: clues.length === 1 ? `核实发现：${first.title}` : `核实 ${clues.length} 条发现（${first.title} 等）`,
    assignee: STUDIO_TEAM[1] ?? STUDIO_TEAM[0] ?? "",
    links: [
      ...clues.map((clue) => ({ kind: "clue" as const, id: clue.id })),
      ...Array.from(new Set(clues.flatMap((clue) => clue.affectedSourceIds ?? [clue.sourceId]))).map((sourceId) => ({ kind: "source" as const, id: sourceId })),
    ],
    threadId: state.activeThreadId,
    originKind: "finding",
  });
  toast({
    title: "已创建任务",
    description: "可以在任务里一键交给 AI 推进，也可以在处理记录中撤销。",
    tone: "success",
    action: { label: "查看任务", onClick: () => openRef({ kind: "task", id }) },
  });
}

const EXCERPT_RADIUS = 90;

function quoteWindow(text: string, quote: string): string {
  const start = text.indexOf(quote);
  if (start < 0 || text.length <= quote.length + EXCERPT_RADIUS * 2) return text;
  const from = Math.max(0, start - EXCERPT_RADIUS);
  const to = Math.min(text.length, start + quote.length + EXCERPT_RADIUS);
  return `${from > 0 ? "…" : ""}${text.slice(from, to)}${to < text.length ? "…" : ""}`;
}

function Evidence({ clue }: { clue: StudioClue }) {
  const state = useStudioState();
  const [full, setFull] = useState(false);
  const hit = findPassage(state, clue.passageId);
  if (!hit) {
    return <blockquote className="rounded-2xl bg-cx-bg-subtle px-4 py-3 text-cx-md leading-7 text-cx-fg">{clue.quote}</blockquote>;
  }
  const excerpt = quoteWindow(hit.passage.text, clue.quote);
  const folded = excerpt !== hit.passage.text;
  const passageRef = { kind: "passage" as const, id: hit.passage.id };
  return (
    <section aria-label="原文" className="flex flex-col gap-2 rounded-2xl border border-cx-border-subtle bg-cx-bg-subtle p-4" data-testid="studio-clue-evidence">
      <p className="text-cx-md leading-7 text-cx-fg-2">
        <HighlightedText text={full ? hit.passage.text : excerpt} quotes={[clue]} activeQuote={clue.id} />
      </p>
      <div className="flex flex-wrap items-center gap-1">
        <span className="min-w-0 flex-1 truncate text-cx-sm text-cx-fg-3">《{hit.source.title}》第 {hit.passage.index} 段</span>
        {folded ? (
          <Button size="md" variant="ghost" aria-expanded={full} onClick={() => setFull((value) => !value)}>
            {full ? "只看摘录" : "展开全段"}
          </Button>
        ) : null}
        <Button size="md" variant="ghost" icon="quote" onClick={() => quoteToComposer(hit.passage.text, passageRef)}>引用</Button>
        <Button size="md" variant="ghost" icon="book" onClick={() => openRefInReader({ kind: "clue", id: clue.id })}>读原文</Button>
      </div>
    </section>
  );
}

function MergedMember({ clueId }: { clueId: string }) {
  const state = useStudioState();
  const clue = state.clues.find((item) => item.id === clueId);
  if (!clue) return <span className="text-cx-sm text-cx-fg-3">发现已不存在</span>;
  const hit = findPassage(state, clue.passageId);
  return (
    <div className="flex flex-col gap-1 rounded-xl bg-cx-bg-subtle px-3 py-2" {...dragRef({ kind: "clue", id: clue.id })}>
      <span className="text-cx-base leading-6 text-cx-fg">{clue.title}</span>
      <span className="text-cx-sm leading-5 text-cx-fg-2">「{clue.quote}」</span>
      <div className="flex flex-wrap items-center gap-1">
        <span className="min-w-0 flex-1 truncate text-cx-sm text-cx-fg-3">
          {hit ? `《${hit.source.title}》第 ${hit.passage.index} 段` : "原文缺失"} · {statusLabel(clue.status)}
        </span>
        <Button size="md" variant="ghost" icon="book" onClick={() => openRefInReader({ kind: "clue", id: clue.id })}>读原文</Button>
        <Button size="md" variant="ghost" iconRight="arrowRight" onClick={() => studio.setFindingsUi({ selectedId: clue.id })}>详情</Button>
      </div>
    </div>
  );
}

function ClueBatchActions({ checked, matchingIds, filterText }: { checked: string[]; matchingIds: string[]; filterText: string }) {
  const state = useStudioState();
  const [mergeIds, setMergeIds] = useState<string[]>([]);
  const [mergeOpen, setMergeOpen] = useState(false);
  const [tagOpen, setTagOpen] = useState(false);
  const [linkOpen, setLinkOpen] = useState(false);
  const [exportOpen, setExportOpen] = useState(false);
  const byId = useMemo(() => new Map(state.clues.map((clue) => [clue.id, clue])), [state.clues]);
  const picked = checked.flatMap((id) => (byId.get(id) ? [byId.get(id) as StudioClue] : []));
  const changing = (differs: (clue: StudioClue) => boolean) => picked.filter(differs).length;
  const scope = batchScope("clue", checked.length, filterText);
  const subject = `勾选的 ${countLabel("clue", checked.length)}`;

  return (
    <>
      <Menu
        ariaLabel="标为"
        trigger={<Button size="md" variant="secondary" iconRight="chevronDown">标为</Button>}
      >
        <MenuLabel>{subject}标为</MenuLabel>
        {CLUE_STATUSES.map((item) => {
          const count = changing((clue) => clue.status !== item.value);
          return (
            <MenuItem
              key={item.value}
              disabled={!count}
              hint={count ? `${count} 条会改` : "都已是"}
              onSelect={() => reportChange(studio.setClueStatus(checked, item.value, scope), `勾选的发现都已经是「${item.label}」。`)}
            >
              {item.label}
            </MenuItem>
          );
        })}
      </Menu>
      <Menu
        ariaLabel="归为"
        trigger={<Button size="md" variant="secondary" iconRight="chevronDown">归为</Button>}
      >
        <MenuLabel>{subject}归为</MenuLabel>
        {CLUE_CATEGORIES.map((item) => {
          const count = changing((clue) => clue.category !== item.value);
          return (
            <MenuItem
              key={item.value}
              disabled={!count}
              hint={count ? `${count} 条会改` : "都已是"}
              onSelect={() => reportChange(studio.setClueCategory(checked, item.value, scope), `勾选的发现都已经归为「${item.label}」。`)}
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
        tooltip={checked.length < 2 ? "勾选两条或更多重复的发现后归并" : undefined}
        onClick={() => { setMergeIds(checked); setMergeOpen(true); }}
      >
        归并…
      </Button>
      <Button size="md" variant="ghost" icon="listTodo" disabled={!picked.length} onClick={() => createTaskFromClues(picked)}>
        转为任务
      </Button>
      <Button
        size="md"
        variant="ghost"
        icon="trash"
        className={"text-cx-danger hover:text-cx-danger"}
        onClick={() => reportChange(studio.batchDeleteRecords("clue", checked, scope), "没有可删除的发现。")}
      >
        删除
      </Button>
      <MergeDialog open={mergeOpen} onOpenChange={setMergeOpen} kind="clue" ids={mergeIds} onMerged={(primaryId) => studio.setFindingsUi({ selectedId: primaryId })} />
      <BatchTagDialog open={tagOpen} onOpenChange={setTagOpen} kind="clue" ids={checked} scope={scope} />
      <BatchLinkDialog open={linkOpen} onOpenChange={setLinkOpen} kind="clue" ids={checked} scope={scope} />
      <BatchExportDialog open={exportOpen} onOpenChange={setExportOpen} kind="clue" checkedIds={checked} matchingIds={matchingIds} />
    </>
  );
}

function ClueActions({ clue, showOpen = false }: { clue: StudioClue; showOpen?: boolean }) {
  const target: StudioRef = { kind: "clue", id: clue.id };
  return (
    <>
      {showOpen ? <><MenuItem icon="bookmark" onSelect={() => studio.setFindingsUi({ selectedId: clue.id })}>打开发现</MenuItem><MenuSeparator /></> : null}
      <MenuItem icon="pin" onSelect={() => quoteToComposer(clue.title, target)}>固定到对话</MenuItem>
      <MenuItem icon="listTodo" onSelect={() => createTaskFromClues([clue])}>转为任务</MenuItem>
      <MenuItem icon="listCollapse" onSelect={() => studio.setFindingsUi({ view: "duplicates" })}>归并候选…</MenuItem>
      <MenuSeparator />
      {clue.status !== "verified" ? <MenuItem icon="checkCircle" onSelect={() => reportChange(studio.setClueStatus(clue.id, "verified"), "已经是已复核。")}>标为已复核</MenuItem> : null}
      <MenuItem icon="listChecks" onSelect={() => studio.setFindingsUi({ view: "history" })}>处理记录</MenuItem>
    </>
  );
}

export function ClueDetail({ clue, onBack, backLabel = "全部发现" }: { clue: StudioClue; onBack?: () => void; backLabel?: string }) {
  const ctx = useContextMenu();
  const actions = <ClueActions clue={clue} />;
  const state = useStudioState();
  const [newClaimText, setNewClaimText] = useState("");
  const [newClaimKind, setNewClaimKind] = useState<FindingClaimItem["kind"]>("observed");
  const [addClaimOpen, setAddClaimOpen] = useState(false);
  const [dossierOpen, setDossierOpen] = useState(false);
  const [draftVersion, setDraftVersion] = useState(clue.targetVersion ?? "");
  const [draftImpact, setDraftImpact] = useState(clue.impactSummary ?? "");
  const [draftCondImpact, setDraftCondImpact] = useState(clue.conditionalImpact ?? "");
  useEffect(() => {
    setDraftVersion(clue.targetVersion ?? "");
    setDraftImpact(clue.impactSummary ?? "");
    setDraftCondImpact(clue.conditionalImpact ?? "");
    setDossierOpen(false);
  }, [clue.id, clue.targetVersion, clue.impactSummary, clue.conditionalImpact]);
  const tasks = state.tasks.filter((task) => task.links.some((link) => link.kind === "clue" && link.id === clue.id));
  const reports = state.reports.filter((r) => r.findingId === clue.id);
  const retests = state.retests.filter((rt) => rt.findingId === clue.id);
  const target = { kind: "clue" as const, id: clue.id };

  const sources = useMemo(() => {
    const memberIds = mergesInto(state.merges, "clue", clue.id).flatMap((merge) => merge.memberIds);
    const ownAssets = clue.affectedSourceIds?.length ? clue.affectedSourceIds : [clue.sourceId];
    const ids = [...ownAssets, ...memberIds.flatMap((id) => {
      const m = state.clues.find((item) => item.id === id);
      return m ? (m.affectedSourceIds ?? [m.sourceId]) : [];
    })].filter(Boolean);
    return Array.from(new Set(ids)).map((id): StudioRef => ({ kind: "source", id }));
  }, [clue.affectedSourceIds, clue.id, clue.sourceId, state.clues, state.merges]);

  const evidenceRefs = useMemo(
    () => (clue.supportingEvidenceIds ?? []).map((id): StudioRef => ({ kind: "evidence", id })),
    [clue.supportingEvidenceIds],
  );

  const report = reports[0];
  const draftReport = async () => {
    if (report) {
      openRef({ kind: "report", id: report.id });
      return;
    }
    const localId = studio.createReportDraft(clue.id, { title: `${clue.title}（草稿）` });
    const sync = await afterDomainMutation();
    if (!sync.ok) {
      toast({ title: "创建报告草稿失败", description: sync.error, tone: "danger" });
      return;
    }
    const targetId = readStudioState().workspace.reports.selectedReportId || localId;
    openRef({ kind: "report", id: targetId });
  };
  const markReviewed = async () => {
    studio.updateFindingDossier(clue.id, {
      stale: false,
      staleReason: undefined,
      reviewedAt: new Date().toISOString(),
      reviewer: hasAttachedDomains() ? "local-user" : "演示操作员",
    }, "复核确认");
    const sync = await afterDomainMutation();
    if (!sync.ok) {
      toast({ title: "确认复核失败", description: sync.error, tone: "danger" });
      return;
    }
    toast({ title: "已确认复核", tone: "success" });
  };

  const gaps = (clue.missingMaterials ?? []).map((g) => {
    if (typeof g === "string") return g;
    if (g && typeof g === "object") {
      const o = g as Record<string, unknown>;
      return String(o.action ?? o.message ?? o.title ?? o.code ?? "");
    }
    return String(g ?? "");
  }).filter(Boolean);
  const present = (clue.claims ?? []).length + evidenceRefs.length;
  const readinessItems: TodoItem[] = [
    ...evidenceRefs.map((ref, i) => ({ id: `ev-${i}`, title: `证据 ${ref.id}`, status: "completed" as const })),
    ...(clue.claims ?? []).map((c) => ({ id: c.id, title: c.text.slice(0, 40), status: clue.status === "verified" && c.evidenceIds.length ? "completed" as const : "pending" as const })),
    ...gaps.map((g, i) => ({ id: `gap-${i}`, title: g, status: "pending" as const })),
    { id: "review", title: "完成原件、反证与安全影响复核", status: clue.readiness === "GREEN" ? "completed" as const : "pending" as const },
  ];

  return (
    <div className="flex min-h-0 flex-1 flex-col" data-testid="studio-clue-detail" onContextMenu={ctx.onContextMenu} onKeyDown={ctx.onKeyDown}>
      <SurfaceToolbar>
        {onBack ? <IconButton size="sm" icon="arrowLeft" label="返回" onClick={onBack} /> : null}
        <PathBreadcrumb path={`${backLabel === "全部发现" ? "" : `${backLabel}/`}${displayTitle(clue.title)}`} rootLabel="发现" lastIsFile onSelect={onBack} />
        <Menu ariaLabel="发现操作" trigger={<IconButton size="sm" icon="more" label="更多" />}>
          {actions}
        </Menu>
      </SurfaceToolbar>
      {ctx.render(actions, `发现 ${clue.title} 操作`)}
      <div className="cx-scroll min-h-0 flex-1 overflow-y-auto">
        <div className="mx-auto flex w-full max-w-[760px] flex-col gap-4 p-4 pb-10">
          <SurfaceCard className="p-3">
            <div className="flex items-start gap-3">
              <div className="min-w-0 flex-1">
                <p className="truncate text-cx-md font-semibold text-cx-fg">{displayTitle(clue.title)}</p>
              </div>
              <StudioBadge tone={statusTone(clue.status)}>{statusLabel(clue.status)}</StudioBadge>
            </div>
            <Dl className="mt-3" items={[
              {
                label: "严重度",
                value: (
                  <Select<FindingSeverity | "unrated"> value={clue.severity ?? "unrated"} onChange={(next) => { if (next !== "unrated") studio.setClueSeverity(clue.id, next); }} size="sm" ariaLabel="严重度" className="min-w-[110px]" options={[{ value: "unrated", label: "未定级" }, ...FINDING_SEVERITIES.map((item) => ({ value: item.value, label: item.label }))]} />
                ),
              },
              { label: "CVSS", value: clue.cvssVector || clue.cvssScore !== undefined ? `${clue.cvssScore ?? ""} ${clue.cvssVector ?? ""}`.trim() : "未评估", mono: true, copy: clue.cvssVector },
              { label: "资产", value: <RefLinkList targets={sources} max={3} /> },
              { label: "复核", value: `${clue.reviewer ?? "未复核"}${clue.reviewedAt ? ` · ${clue.reviewedAt}` : ""}` },
              { label: "适用版本", value: clue.targetVersion ?? "未指定", mono: true },
              {
                label: "分类",
                value: (
                  <Select<ClueCategory> value={clue.category} onChange={(next) => studio.setClueCategory(clue.id, next)} size="sm" ariaLabel="分类" className="min-w-[110px]" options={CLUE_CATEGORIES.map((item) => ({ value: item.value, label: item.label }))} />
                ),
              },
              {
                label: "准备度",
                value: (
                  <Select<ReadinessLevel> value={clue.readiness ?? "YELLOW"} onChange={(next) => { void (async () => { studio.setClueReadiness(clue.id, next); const res = await afterDomainMutation(); if (!res.ok) toast({ title: "更新准备度失败", description: res.error, tone: "danger" }); })(); }} size="sm" ariaLabel="准备度" className="min-w-[110px]" options={READINESS_LEVELS.map((item) => ({ value: item.value, label: item.label }))} />
                ),
              },
              {
                label: "状态",
                value: (
                  <Select<ClueStatus> value={clue.status} onChange={(next) => studio.setClueStatus(clue.id, next)} size="sm" ariaLabel="状态" className="min-w-[110px]" options={CLUE_STATUSES.map((item) => ({ value: item.value, label: item.label }))} />
                ),
              },
            ]} />
            <div className="mt-3 flex flex-wrap items-center gap-2">
              <Button size="sm" variant="primary" icon="file" onClick={() => { void draftReport(); }}>{report ? "打开报告" : "起草报告"}</Button>
              <Button size="sm" variant="secondary" icon="pencilLine" onClick={() => setDossierOpen((v) => !v)}>{dossierOpen ? "收起档案" : "编辑档案"}</Button>
              {(clue.readiness ?? "YELLOW") === "DRAFT" || gaps.length > 0 ? (
                <Button
                  size="sm"
                  variant="secondary"
                  icon="plus"
                  onClick={() => askAi(target, `请为发现「${clue.title}」(${clue.id}) 补抓缺失证据：${gaps.length ? gaps.join("、") : "正常账号对照包与量化影响范围"}。完成后更新核查并升级准备度。`)}
                >
                  补齐对照与量化证据
                </Button>
              ) : null}
            </div>
            {dossierOpen ? (
              <div className="mt-3 flex flex-col gap-2 border-t border-cx-border-subtle pt-3">
                <Input size="sm" value={draftVersion} onChange={(e) => setDraftVersion(e.target.value)} placeholder="适用版本 / 目标范围版本" aria-label="适用版本" />
                <TextArea value={draftImpact} onChange={(e) => setDraftImpact(e.target.value)} placeholder="已证实安全影响" aria-label="已证实影响" className="min-h-[60px]" />
                <TextArea value={draftCondImpact} onChange={(e) => setDraftCondImpact(e.target.value)} placeholder="条件性影响或前提" aria-label="条件性影响" className="min-h-[52px]" />
                <div className="flex justify-end gap-2">
                  <Button size="sm" variant="ghost" onClick={() => setDossierOpen(false)}>取消</Button>
                  <Button size="sm" variant="primary" onClick={() => { void (async () => {
                    studio.updateFindingDossier(clue.id, { targetVersion: draftVersion.trim(), impactSummary: draftImpact.trim(), conditionalImpact: draftCondImpact.trim() }, "更新发现档案");
                    const res = await afterDomainMutation();
                    if (!res.ok) { toast({ title: "保存档案失败", description: res.error, tone: "danger" }); return; }
                    setDossierOpen(false);
                    toast({ title: "已更新发现档案", tone: "success" });
                  })(); }}>保存</Button>
                </div>
              </div>
            ) : null}
          </SurfaceCard>

          {clue.stale ? (
            <Callout tone="warning" action={<Button size="sm" variant="secondary" onClick={markReviewed}>已复核</Button>}>
              资料已更新，结论需复核{clue.staleReason ? `：${clue.staleReason}` : ""}
            </Callout>
          ) : null}

          <MergeInfo kind="clue" id={clue.id} memberDetail={(memberId) => <MergedMember clueId={memberId} />} />

          {readinessItems.length ? (
            <div className="flex flex-col gap-1.5">
              <TodoList
                items={readinessItems}
                collapseOnComplete
                title={`交付准备 ${present}/${present + gaps.length}`}
              />
              {gaps.length ? (
                <div className="flex justify-end px-1">
                  <button
                    type="button"
                    className="text-cx-xs font-medium text-cx-accent hover:underline"
                    onClick={() => sendAbout(target, `发现 ${clue.id} 还缺 ${gaps.length} 项材料，请帮我补齐。`)}
                  >
                    让 Agent 补齐
                  </button>
                </div>
              ) : null}
            </div>
          ) : null}

          <div>
            <SectionHeader
              title="主张"
              count={clue.claims?.length ?? 0}
              action={<button type="button" className="text-cx-xs font-medium text-cx-accent hover:underline" onClick={() => setAddClaimOpen((v) => !v)}>添加主张</button>}
            />
            <SurfaceCard>
              {(["observed", "inferred", "unverified"] as const).map((kind) => {
                const claims = (clue.claims ?? []).filter((claim) => claim.kind === kind);
                if (!claims.length) return null;
                return (
                  <div key={kind}>
                    <p className="px-2 pb-0.5 pt-1.5 text-cx-xs font-medium text-cx-fg-4">{kind === "observed" ? "观察" : kind === "inferred" ? "推断" : "未验证"}</p>
                    {claims.map((claim) => (
                      <SurfaceRow key={claim.id} icon={kind === "observed" ? "check" : kind === "inferred" ? "sparkles" : "help"} title={claim.text} subtitle={[claim.note, claim.evidenceIds.length ? `${claim.evidenceIds.length} 条证据` : null].filter(Boolean).join(" · ")} />
                    ))}
                  </div>
                );
              })}
              {!(clue.claims ?? []).length ? <SurfaceRow icon="quote" title="还没有主张" subtitle="添加观察、推断或未验证的主张" /> : null}
            </SurfaceCard>
            {addClaimOpen ? (
              <div className="mt-2 flex flex-wrap items-center gap-2">
                <Select<FindingClaimItem["kind"]>
                  value={newClaimKind}
                  onChange={setNewClaimKind}
                  size="sm"
                  ariaLabel="主张类别"
                  className="h-8 min-w-[110px] text-cx-sm"
                  options={[
                    { value: "observed", label: "观察" },
                    { value: "inferred", label: "推断" },
                    { value: "unverified", label: "未验证" },
                  ]}
                />
                <Input size="sm" value={newClaimText} onChange={(e) => setNewClaimText(e.target.value)} placeholder="补充一条主张…" aria-label="新主张内容" className="min-w-[160px] flex-1" />
                <Button size="sm" variant="secondary" disabled={!newClaimText.trim()} onClick={() => {
                  studio.addFindingClaim(clue.id, { text: newClaimText.trim(), kind: newClaimKind, evidenceIds: [] });
                  setNewClaimText("");
                  setAddClaimOpen(false);
                  toast({ title: "已添加主张", tone: "success" });
                }}>添加</Button>
              </div>
            ) : null}
          </div>

          <div>
            <SectionHeader title="证据" count={evidenceRefs.length} />
            <SurfaceCard>
              {evidenceRefs.map((ref) => {
                const ev = state.evidences.find((e) => e.id === ref.id);
                return (
                  <SurfaceRow key={ref.id} icon="file" title={ev ? displayTitle(ev.title) : ref.id} subtitle={ev ? `${EVIDENCE_KIND_LABELS[ev.kind]} · ${ev.capturedAt}` : "证据缺失"} onClick={() => studio.setFindingsUi({ evidenceId: ref.id, evidenceParent: clue.id })} />
                );
              })}
            </SurfaceCard>
          </div>

          <div>
            <SectionHeader title="原文" />
            <Evidence key={clue.id} clue={clue} />
          </div>

          <div>
            <SectionHeader title="影响" />
            <p className="px-1 text-cx-sm leading-6 text-cx-fg">{clue.impactSummary ?? "尚未评估安全影响。"}</p>
            {clue.conditionalImpact ? <p className="px-1 pt-1 text-cx-xs leading-5 text-cx-fg-4">{clue.conditionalImpact}</p> : null}
          </div>

          {attachedTaskId() ? <ObjectLifecycleSection taskId={attachedTaskId()} /> : null}

          <div>
            <SectionHeader title="关联" count={reports.length + retests.length + tasks.length} />
            <SurfaceCard>
              {reports.map((r) => <SurfaceRow key={r.id} icon="file" title={displayTitle(r.title ?? r.id)} subtitle="报告" onClick={() => openRef({ kind: "report", id: r.id })} />)}
              {retests.map((rt) => <SurfaceRow key={rt.id} icon="refresh" title={`复测 ${rt.targetFixVersion}`} subtitle={rt.result === "fixed" ? "已解决" : rt.result === "still_vulnerable" ? "仍存在" : "无法判断"} />)}
              {tasks.map((task) => <SurfaceRow key={task.id} icon="listTodo" title={task.title} subtitle="任务" onClick={() => openRef({ kind: "task", id: task.id })} />)}
              {!reports.length && !retests.length && !tasks.length ? <SurfaceRow icon="link" title="暂无关联" /> : null}
            </SurfaceCard>
          </div>

          <RecordHistory target={target} />
        </div>
      </div>
    </div>
  );
}

export function ObjectLifecycleSection({ taskId }: { taskId: string }) {
  const [rows, setRows] = useState<Array<Record<string, unknown>>>([]);
  const [loadError, setLoadError] = useState("");
  const [busyId, setBusyId] = useState("");
  const [openAdd, setOpenAdd] = useState(false);
  const [objType, setObjType] = useState("order");
  const [objId, setObjId] = useState("");
  const [ctxId, setCtxId] = useState("default");
  const [owner, setOwner] = useState("test-account");

  const load = useCallback(async () => {
    if (!taskId) return;
    setLoadError("");
    try {
      const res = await querySrc<{ records?: Array<Record<string, unknown>> }>(taskId, "src.record.list", {
        kind: "object_experiment",
        limit: 100,
      });
      setRows(res.records ?? []);
    } catch (err) {
      setLoadError(err instanceof Error ? err.message : String(err));
    }
  }, [taskId]);

  useEffect(() => {
    void load();
  }, [load]);

  const restore = async (expId: string, fencingToken?: number) => {
    setBusyId(expId);
    try {
      await postCommand(taskId, "src.object.experiment.restore", {
        experiment_id: expId,
        status: "restored",
        readback: { verified: true },
        release: true,
        ...(typeof fencingToken === "number" && fencingToken > 0 ? { fencing_token: fencingToken } : {}),
      });
      await load();
      toast({ title: `已恢复测试对象 ${expId}`, tone: "success" });
    } catch (err) {
      toast({ title: "恢复失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
    } finally {
      setBusyId("");
    }
  };

  const registerBegin = async () => {
    if (!objId.trim()) return;
    setBusyId("new");
    try {
      await postCommand(taskId, "src.object.experiment.begin", {
        context_id: ctxId.trim() || "default",
        object_type: objType.trim() || "object",
        object_id: objId.trim(),
        owner: owner.trim() || "test-account",
        initial_state: { created: true },
        expected_change: { modified: true },
        restoration: { restored: true },
      });
      setOpenAdd(false);
      setObjId("");
      await load();
      toast({ title: "已登记测试对象实验", tone: "success" });
    } catch (err) {
      toast({ title: "登记失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
    } finally {
      setBusyId("");
    }
  };

  const uncleaned = rows.filter((r) => {
    const p = r.payload as Record<string, unknown> | undefined;
    const st = String((p?.restoration as Record<string, unknown> | undefined)?.status ?? "unknown");
    return st !== "restored" && st !== "not_applicable";
  }).length;

  return (
    <div data-testid="studio-object-lifecycle">
      <SectionHeader
        title="测试对象账本"
        count={rows.length}
        action={(
          <span className="flex items-center gap-2">
            {uncleaned > 0 ? <StudioBadge tone="warning">未清理 {uncleaned}</StudioBadge> : null}
            <button type="button" className="text-cx-xs font-medium text-cx-accent hover:underline" onClick={() => setOpenAdd(true)}>登记对象</button>
          </span>
        )}
      />
      {loadError ? (
        <Callout tone="danger" className="mb-2" action={<Button size="xs" variant="secondary" onClick={() => { void load(); }}>重试</Button>}>
          读取测试对象账本失败：{loadError}
        </Callout>
      ) : null}
      <SurfaceCard>
        {rows.length ? rows.map((r) => {
          const expId = String(r.natural_key || r.record_id || "");
          const p = (r.payload as Record<string, unknown>) ?? {};
          const rest = (p.restoration as Record<string, unknown>) ?? {};
          const lease = (p.lease as Record<string, unknown>) ?? {};
          const status = String(rest.status ?? "unknown");
          const fencing = typeof lease.fencing_token === "number" ? lease.fencing_token : undefined;
          const needsRestore = status !== "restored" && status !== "not_applicable";
          return (
            <SurfaceRow
              key={expId}
              icon="shield"
              title={`${String(p.object_type ?? "object")}:${String(p.object_id ?? expId)}`}
              subtitle={`${String(p.context_id ?? "default")} · ${expId}${fencing ? ` · token=${fencing}` : ""}`}
              trailing={(
                <span className="flex items-center gap-1.5">
                  <StudioBadge tone={status === "restored" ? "success" : "warning"}>{status}</StudioBadge>
                  {needsRestore ? (
                    <Button
                      size="sm"
                      variant="secondary"
                      disabled={busyId === expId}
                      onClick={(e) => {
                        e.stopPropagation();
                        void restore(expId, fencing);
                      }}
                    >
                      标记已恢复
                    </Button>
                  ) : null}
                </span>
              )}
            />
          );
        }) : <SurfaceRow icon="shield" title="暂无测试对象记录" />}
      </SurfaceCard>
      <Dialog
        open={openAdd}
        onOpenChange={setOpenAdd}
        title="登记实验测试对象"
        size="md"
        footer={(
          <>
            <Button variant="ghost" onClick={() => setOpenAdd(false)}>取消</Button>
            <Button variant="primary" disabled={busyId === "new" || !objId.trim()} onClick={() => { void registerBegin(); }}>确认</Button>
          </>
        )}
      >
        <div className="flex flex-col gap-3">
          <TextField label="对象类型" value={objType} onChange={(e) => setObjType(e.target.value)} placeholder="order" />
          <TextField label="对象标识" value={objId} onChange={(e) => setObjId(e.target.value)} placeholder="order-1001" />
          <TextField label="身份 Context" value={ctxId} onChange={(e) => setCtxId(e.target.value)} placeholder="default" />
          <TextField label="归属账号" value={owner} onChange={(e) => setOwner(e.target.value)} placeholder="test-account" />
        </div>
      </Dialog>
    </div>
  );
}


function EvidenceActions({ evidence, showOpen = false }: { evidence: StudioEvidence; showOpen?: boolean }) {
  return (
    <>
      {showOpen ? <><MenuItem icon="file" onSelect={() => studio.setFindingsUi({ evidenceId: evidence.id })}>打开证据</MenuItem><MenuSeparator /></> : null}
      {evidence.kind === "screenshot" && !evidence.imageReviewedBy ? (
        <MenuItem icon="eye" onSelect={() => { void (async () => {
          studio.markEvidenceImageReviewed(evidence.id, hasAttachedDomains() ? "local-user" : undefined);
          const sync = await afterDomainMutation();
          if (!sync.ok) { toast({ title: "签署原图审阅失败", description: sync.error, tone: "danger" }); return; }
          toast({ title: "已签署原图审阅记录", tone: "success" });
        })(); }}>标记已审阅</MenuItem>
      ) : null}
      <MenuItem icon="pin" onSelect={() => quoteToComposer(evidence.title, { kind: "evidence", id: evidence.id })}>固定到对话</MenuItem>
    </>
  );
}

function useEvidenceText(taskId: string, ref: Record<string, unknown> | undefined, inline?: string) {
  const result = useSrcResource(async () => {
    const raw = await querySrc<{ data: string } | [{ data: string }]>(taskId, "src.artifact.read", { ref_data: ref });
    const envelope = Array.isArray(raw) ? raw[0] : raw;
    return new TextDecoder().decode(Uint8Array.from(atob(envelope.data), (c) => c.charCodeAt(0)));
  }, [taskId, ref?.artifact_id, ref?.sha256], { enabled: Boolean(taskId && ref) });
  if (!taskId || !ref) return inline ?? "无";
  if (result.error) return `原文读取失败：${result.error}`;
  return result.data ?? "正在读取原文…";
}

function EvidenceDetail({ evidence, parent, onBack }: { evidence: StudioEvidence; parent?: StudioClue; onBack?: () => void }) {
  const ctx = useContextMenu();
  const actions = <EvidenceActions evidence={evidence} />;
  const state = useStudioState();
  const [diffMode, setDiffMode] = useState<"denoised" | "raw">("raw");
  const [replayOpen, setReplayOpen] = useState(false);
  const [replayBody, setReplayBody] = useState('{"tenant_id":"tenant-b-target","format":"summary","dry_run":true}');
  const [shot, setShot] = useState<{ id: string; url: string; readId: string; error: string }>({ id: "", url: "", readId: "", error: "" });
  const [judgment, setJudgment] = useState("");
  const counterpart = evidence.baselineExchangeId ? state.evidences.find((e) => e.id === evidence.baselineExchangeId) : undefined;

  const taskId = attachedTaskId() ?? "";
  const requestText = useEvidenceText(taskId, evidence.requestArtifact, evidence.requestRaw);
  const responseText = useEvidenceText(taskId, evidence.responseArtifact, evidence.responseRaw);
  const baselineText = useEvidenceText(taskId, counterpart?.responseArtifact, counterpart?.responseRaw);

  useEffect(() => {
    const taskId = attachedTaskId();
    if (evidence.kind !== "screenshot" || !hasAttachedDomains() || !taskId) {
      setShot({ id: evidence.id, url: "", readId: "", error: "" });
      return;
    }
    let cancelled = false;
    setShot({ id: evidence.id, url: "", readId: "", error: "" });
    void readBrowserScreenshotImage(taskId, evidence.id, "local-user")
      .then((res) => {
        if (!cancelled) setShot({ id: evidence.id, url: res.dataUrl, readId: res.readId, error: "" });
      })
      .catch((err) => {
        if (!cancelled) setShot({ id: evidence.id, url: "", readId: "", error: err instanceof Error ? err.message : String(err) });
      });
    return () => { cancelled = true; };
  }, [evidence.id, evidence.kind]);

  const stripNoise = (raw: string) =>
    raw.split(/\r?\n/).filter((line) => !/^(Date|X-Trace-Id|X-Request-Id):/i.test(line.trim())).join("\n");

  const completenessLabel = evidence.completeness === "complete" ? "完整" : evidence.completeness === "partial" ? "部分截断" : evidence.completeness === "revoked" ? "已撤回" : "不可用";
  const blocked = evidence.completeness === "revoked" || evidence.completeness === "partial";
  const crumbPath = parent ? `${parent.title}/${evidence.id}` : evidence.id;

  return (
    <div className="flex min-h-0 flex-1 flex-col" data-testid="studio-evidence-detail" onContextMenu={ctx.onContextMenu} onKeyDown={ctx.onKeyDown}>
      <SurfaceToolbar>
        {onBack ? <IconButton size="sm" icon="arrowLeft" label="返回" onClick={onBack} /> : null}
        <PathBreadcrumb path={crumbPath} rootLabel="发现" lastIsFile onSelect={onBack} />
        <Menu ariaLabel="证据操作" trigger={<IconButton size="sm" icon="more" label="更多" />}>
          {actions}
        </Menu>
      </SurfaceToolbar>
      {ctx.render(actions, `证据 ${evidence.title} 操作`)}
      <div className="cx-scroll min-h-0 flex-1 overflow-y-auto">
        <div className="mx-auto flex w-full max-w-[760px] flex-col gap-4 p-4 pb-10">
          <SurfaceCard className="p-3">
            <div className="flex items-start gap-3">
              <div className="min-w-0 flex-1">
                <p className="truncate text-cx-md font-semibold text-cx-fg">{displayTitle(evidence.title)}</p>
              </div>
              <span className="flex shrink-0 items-center gap-2">
                {evidence.artifactId ? (
                  <Button size="sm" variant="ghost" icon="check" onClick={() => studio.verifyEvidenceDigest(evidence.id)} aria-label="读取验证">
                    {evidence.digestVerified === "verified" ? "可读取" : evidence.digestVerified === "mismatch" ? "无法读取" : "读取验证"}
                  </Button>
                ) : null}
                <StudioBadge tone={evidence.completeness === "complete" ? "success" : evidence.completeness === "partial" ? "warning" : "danger"}>{completenessLabel}</StudioBadge>
              </span>
            </div>
            <Dl className="mt-3" items={[
              { label: "编号", value: evidence.id, mono: true },
              { label: "类型", value: EVIDENCE_KIND_LABELS[evidence.kind] },
              { label: "采集身份", value: evidence.actorRef, mono: true },
              { label: "时间", value: evidence.capturedAt },
              ...(evidence.artifactId ? [{ label: "工件编号", value: evidence.artifactId, mono: true, copy: evidence.artifactId }] : []),
              { label: "来源", value: `${evidence.sourceKind} · ${evidence.version}` },
            ]} />
          </SurfaceCard>

          {blocked ? (
            <Callout tone={evidence.completeness === "revoked" ? "danger" : "warning"}>
              {evidence.completeness === "revoked" ? "此证据已被撤回，不应再被报告引用。" : "此证据内容被部分截断。"}
            </Callout>
          ) : null}

          {evidence.kind === "screenshot" ? (
            <div>
              <SectionHeader title="截图" action={evidence.imageReviewedBy ? <span className="text-cx-xs text-cx-fg-4">已审阅 · {evidence.imageReviewedBy}</span> : undefined} />
              <div className="overflow-hidden rounded-xl border border-cx-border bg-cx-bg-subtle" data-testid="studio-evidence-screenshot">
                <div className="flex items-center gap-2 border-b border-cx-border-subtle bg-cx-hover px-3 py-1.5 text-cx-xs text-cx-fg-3">
                  <Icon name="globe" size={13} />
                  <span className="truncate font-cx-mono">{evidence.captureUrl || evidence.title}</span>
                  <span className="ml-auto shrink-0">{evidence.dimensions}</span>
                </div>
                {shot.id === evidence.id && shot.url ? (
                  <div className="bg-cx-bg p-3">
                    <img src={shot.url} alt={evidence.title} className="max-h-[420px] w-full rounded-lg border border-cx-border-subtle object-contain" />
                  </div>
                ) : shot.id === evidence.id && shot.error ? (
                  <p className="p-3 text-cx-xs text-cx-danger">{shot.error}</p>
                ) : null}
                {evidence.visualPreviewData ? (
                  <div className="flex flex-col gap-3 p-4">
                    <p className="text-cx-sm font-medium text-cx-fg">{evidence.visualPreviewData.caption}</p>
                    <CodeBlock code={evidence.visualPreviewData.domHighlight} language="html" className="max-h-[160px]" />
                    <div className="grid grid-cols-2 gap-2 text-cx-xs max-sm:grid-cols-1">
                      <div className="rounded-lg bg-cx-bg p-2.5">
                        <div className="font-medium text-cx-fg-3">控制台</div>
                        <code className="mt-1 block truncate font-cx-mono text-cx-fg">{evidence.visualPreviewData.consoleLog}</code>
                      </div>
                      <div className="rounded-lg bg-cx-bg p-2.5">
                        <div className="font-medium text-cx-fg-3">网络请求</div>
                        <code className="mt-1 block truncate font-cx-mono text-cx-fg">{evidence.visualPreviewData.networkSummary}</code>
                      </div>
                    </div>
                  </div>
                ) : null}
                {!evidence.imageReviewedBy ? (
                  <div className="flex flex-wrap items-center gap-2 border-t border-cx-border-subtle bg-cx-bg p-3">
                    <Input size="sm" value={judgment} onChange={(e) => setJudgment(e.target.value)} placeholder="原图核验结论" aria-label="原图核验结论" className="min-w-[200px] flex-1" />
                    <Button size="sm" variant="primary" icon="check" onClick={() => { void (async () => {
                      studio.markEvidenceImageReviewed(evidence.id, hasAttachedDomains() ? "local-user" : "人工复核", judgment.trim() || "人工已核验原图", shot.readId || undefined);
                      const sync = await afterDomainMutation();
                      if (!sync.ok) { toast({ title: "签署审阅失败", description: sync.error, tone: "danger" }); return; }
                      toast({ title: "已签署原图审阅记录", tone: "success" });
                    })(); }}>签署原图审阅</Button>
                  </div>
                ) : null}
              </div>
            </div>
          ) : null}

          {evidence.kind === "http_exchange" ? (
            <div>
              <SectionHeader title="报文" action={<span className="font-cx-mono text-cx-xs text-cx-fg-4">{evidence.method} → {evidence.statusCode}</span>} />
              <CodeBlock code={`# 请求\n${requestText}\n\n# 响应\n${responseText}`} language="http" className="max-h-[320px]" />
              {counterpart || evidence.diffBefore ? (
                <div className="mt-3">
                  <SectionHeader title="对比" action={(
                    <Select<"denoised" | "raw"> value={diffMode} onChange={setDiffMode} size="sm" ariaLabel="差异模式" className="min-w-[120px]" options={[{ value: "raw", label: "原始" }, { value: "denoised", label: "隐藏时间/链路头" }]} />
                  )} />
                  <CodeBlock
                    code={`# 之前\n${counterpart ? (diffMode === "denoised" ? stripNoise(baselineText) : baselineText) : evidence.diffBefore ?? ""}\n\n# 当前\n${diffMode === "denoised" ? stripNoise(responseText) : responseText}`}
                    language="http"
                    className="max-h-[240px]"
                  />
                </div>
              ) : null}
              <Collapse open={replayOpen} className="mt-2">
                <div className="flex flex-wrap items-center gap-2 p-1">
                  <Input size="sm" value={replayBody} onChange={(e) => setReplayBody(e.target.value)} aria-label="重放请求体" className="min-w-[220px] flex-1 font-cx-mono" />
                  <Button size="sm" variant="secondary" icon="play" onClick={() => { void (async () => {
                    const id = studio.replayHttpEvidence(evidence.id, replayBody);
                    const sync = await afterDomainMutation();
                    if (!sync.ok) { toast({ title: "重放失败", description: sync.error, tone: "danger" }); return; }
                    if (id) toast({ title: `已生成新证据 ${id}`, tone: "success" });
                  })(); }}>执行重放</Button>
                </div>
              </Collapse>
              <button type="button" className="mt-2 text-cx-xs font-medium text-cx-accent hover:underline" onClick={() => setReplayOpen((v) => !v)}>{replayOpen ? "收起重放实验" : "重放实验…"}</button>
            </div>
          ) : null}

          {evidence.content && evidence.kind !== "http_exchange" ? (
            <div>
              <SectionHeader title="原文" />
              <CodeBlock code={evidence.content} className="max-h-[300px]" />
            </div>
          ) : null}

          <div>
            <SectionHeader title="关联" count={evidence.linkedClueIds.length + evidence.linkedSourceIds.length} />
            <SurfaceCard>
              {evidence.linkedClueIds.map((id) => {
                const c = state.clues.find((x) => x.id === id);
                return <SurfaceRow key={id} icon="bookmark" title={c ? displayTitle(c.title) : id} subtitle="发现" onClick={() => openRef({ kind: "clue", id })} />;
              })}
              {evidence.linkedSourceIds.map((id) => {
                const s2 = state.sources.find((x) => x.id === id);
                return <SurfaceRow key={id} icon="book" title={displayTitle(s2?.title ?? id)} subtitle="资产" onClick={() => openRef({ kind: "source", id })} />;
              })}
              {!evidence.linkedClueIds.length && !evidence.linkedSourceIds.length ? <SurfaceRow icon="link" title="暂无关联" /> : null}
            </SurfaceCard>
          </div>
        </div>
      </div>
    </div>
  );
}

function EvidenceRow({ evidence }: { evidence: StudioEvidence }) {
  const ctx = useContextMenu();
  const actions = <EvidenceActions evidence={evidence} showOpen />;
  const status = evidence.completeness === "revoked" ? <span className="text-cx-xs text-cx-danger">已撤回</span> : evidence.completeness === "partial" ? <span className="text-cx-xs text-cx-warning">截断</span> : null;
  return (
    <div onContextMenu={ctx.onContextMenu} onKeyDown={ctx.onKeyDown}>
      <SurfaceRow icon="file" title={displayTitle(evidence.title)} subtitle={[EVIDENCE_KIND_LABELS[evidence.kind], evidence.capturedAt].join(" · ")}
        trailing={<span className="flex items-center gap-2">{status}<Menu ariaLabel={`证据 ${evidence.title} 操作`} trigger={<IconButton size="sm" icon="more" label={`证据 ${evidence.title} 操作`} />}>{actions}</Menu></span>}
        onClick={() => studio.setFindingsUi({ evidenceId: evidence.id })} />
      {ctx.render(actions, `证据 ${evidence.title} 操作`)}
    </div>
  );
}

function EvidencesListView({ active }: { active: boolean }) {
  const state = useStudioState();
  const ui = state.workspace.findings;
  const srcTaskId = attachedTaskId(state);
  const scroll = useRememberedScroll("findings-evidences", active);
  const [kindFilter, setKindFilter] = useState<"all" | EvidenceKind>("all");
  const [loadingMore, setLoadingMore] = useState(false);
  const [loadMoreError, setLoadMoreError] = useState("");
  const evidences = useMemo(() => state.evidences.filter((e) => kindFilter === "all" || e.kind === kindFilter), [kindFilter, state.evidences]);
  const selected = state.evidences.find((e) => e.id === ui.evidenceId);
  const backToFindings = () => studio.setFindingsUi({ view: "list", selectedId: "", evidenceId: "", evidenceParent: "" });
  const pagination = srcTaskId ? getEvidencePagination(srcTaskId) : null;
  const domainError = state.domainErrors?.findings;

  useViewing(active, selected ? { kind: "evidence", id: selected.id } : null);

  if (selected) {
    const parent = ui.evidenceParent ? state.clues.find((c) => c.id === ui.evidenceParent) : state.clues.find((c) => (c.supportingEvidenceIds ?? []).includes(selected.id));
    return <EvidenceDetail evidence={selected} parent={parent} onBack={() => studio.setFindingsUi({ evidenceId: "", evidenceParent: "" })} />;
  }

  const handleLoadMore = async () => {
    if (!srcTaskId || loadingMore) return;
    setLoadingMore(true);
    setLoadMoreError("");
    try {
      await loadMoreFindingEvidences(srcTaskId, () => studio.reloadDomain("findings"));
    } catch (err) {
      setLoadMoreError(err instanceof Error ? err.message : String(err));
    } finally {
      setLoadingMore(false);
    }
  };

  return (
    <div className="flex min-h-0 flex-1 flex-col" data-testid="studio-evidences-list">
      <SurfaceToolbar>
        <IconButton size="sm" icon="arrowLeft" label="返回" onClick={backToFindings} />
        <PathBreadcrumb path="全部证据" rootLabel="发现" lastIsFile onSelect={backToFindings} />
        <span className="min-w-0 flex-1" />
        <Select<"all" | EvidenceKind>
          value={kindFilter}
          onChange={setKindFilter}
          size="sm"
          options={[{ value: "all", label: "全部类型" }, ...Object.entries(EVIDENCE_KIND_LABELS).map(([k, lbl]) => ({ value: k as EvidenceKind, label: lbl }))]}
          ariaLabel="筛选证据类型"
          className="min-w-[130px]"
        />
      </SurfaceToolbar>
      <div ref={scroll.ref} className="cx-scroll min-h-0 flex-1 overflow-y-auto p-4 space-y-3">
        {domainError ? (
          <Callout tone="danger" action={<Button size="xs" variant="secondary" onClick={() => { void studio.reloadDomain("findings"); }}>重试加载</Button>}>
            证据加载异常：{domainError}
          </Callout>
        ) : null}
        <SurfaceCard>
          {evidences.map((ev) => <EvidenceRow key={ev.id} evidence={ev} />)}
          {!evidences.length ? <SurfaceRow icon="file" title="没有此类证据" /> : null}
        </SurfaceCard>
        {pagination ? (
          <div className="flex flex-wrap items-center justify-between gap-2 px-1 text-cx-xs text-cx-fg-3">
            <span>HTTP / 导入交换记录：已加载 {pagination.loaded} / {pagination.total} 条</span>
            {pagination.hasMore ? (
              <Button size="sm" variant="secondary" disabled={loadingMore || pagination.loadingMore} onClick={() => { void handleLoadMore(); }}>
                {loadingMore || pagination.loadingMore ? "加载中…" : "继续加载更早证据"}
              </Button>
            ) : null}
          </div>
        ) : null}
        {loadMoreError ? (
          <Callout tone="danger" action={<Button size="xs" variant="secondary" onClick={() => { void handleLoadMore(); }}>重试</Button>}>
            继续加载失败：{loadMoreError}
          </Callout>
        ) : null}
      </div>
    </div>
  );
}

function ClueRow({ clue, checked, selecting, onToggle }: {
  clue: StudioClue;
  checked: boolean;
  selecting: boolean;
  onToggle: (checked: boolean, range: boolean) => void;
}) {
  const ctx = useContextMenu();
  const shiftRef = useRef(false);
  const actions = <ClueActions clue={clue} showOpen />;
  const missing = (clue.missingMaterials ?? []).length;
  const target: StudioRef = { kind: "clue", id: clue.id };
  const lead = (
    <span className="relative grid size-8 shrink-0 place-items-center" onPointerDownCapture={(e) => { shiftRef.current = e.shiftKey; }}>
      <span className={cn("absolute inset-0 grid place-items-center rounded-lg bg-cx-hover text-cx-fg-3", (selecting || checked) && "hidden", "group-hover/row:hidden")}>
        <Icon name="bookmark" size={15} />
      </span>
      <span className={cn("absolute inset-0 place-items-center", (selecting || checked) ? "grid" : "hidden group-hover/row:grid")}>
        <Checkbox checked={checked} aria-label={`勾选「${clue.title}」`} onCheckedChange={(next) => onToggle(Boolean(next), shiftRef.current)} className="[&>button]:size-5" />
      </span>
    </span>
  );
  return (
    <span className="group/row block" {...dragRef(target)} onContextMenu={ctx.onContextMenu} onKeyDown={ctx.onKeyDown}>
      <SurfaceRow
        icon="bookmark"
        leading={lead}
        selected={checked}
        title={<>{displayTitle(clue.title)}<NewMark target={target} /><MergeMark kind="clue" id={clue.id} /></>}
        subtitle={[severityLabel(clue.severity), categoryLabel(clue.category), missing ? `缺 ${missing} 项材料` : null].filter(Boolean).join(" · ")}
        trailing={<span className="flex items-center gap-2">{clue.stale ? <span className="text-cx-xs text-cx-warning">陈旧</span> : null}<Menu ariaLabel={`发现 ${clue.title} 操作`} trigger={<IconButton size="sm" icon="more" label={`发现 ${clue.title} 操作`} />}>{actions}</Menu></span>}
        onClick={() => studio.setFindingsUi({ selectedId: clue.id })}
      />
      {ctx.render(actions, `发现 ${clue.title} 操作`)}
    </span>
  );
}

export function FindingsWorkspace({ active, sheet = false }: { active: boolean; sheet?: boolean }) {
  const state = useStudioState();
  const { projectId, clueFilter: rawFilter, findings: ui } = state.workspace;
  const filter = useMemo(() => pickClueFilter(rawFilter), [rawFilter]);
  const scroll = useRememberedScroll("findings", active);

  const sourceTitles = useMemo(() => new Map(state.sources.map((source) => [source.id, source.title])), [state.sources]);
  const withTask = useMemo(() => {
    const ids = new Set<string>();
    state.tasks.forEach((task) => task.links.forEach((link) => { if (link.kind === "clue") ids.add(link.id); }));
    return ids;
  }, [state.tasks]);
  const withReport = useMemo(() => {
    const map = new Map<string, "frozen" | "draft">();
    state.reports.forEach((rep) => {
      if (rep.status === "frozen" || !map.has(rep.findingId)) map.set(rep.findingId, rep.status === "frozen" ? "frozen" : "draft");
    });
    return map;
  }, [state.reports]);

  const scoped = useMemo(() => {
    if (ui.focusIds) {
      const ids = new Set(ui.focusIds);
      return state.clues.filter((clue) => ids.has(clue.id));
    }
    return state.clues.filter((clue) => projectId === "all" || clue.projectId === projectId);
  }, [projectId, state.clues, ui.focusIds]);
  const keepMerged = Boolean(ui.focusIds);
  const searched = useMemo(
    () => scoped.filter((clue) => clueMatches(clue, filter, sourceTitles.get(clue.sourceId) ?? "", state.merges, withTask, withReport, keepMerged)),
    [filter, keepMerged, scoped, sourceTitles, state.merges, withReport, withTask],
  );
  const visible = useMemo(() => searched.filter((clue) => filter.status === "all" || clue.status === filter.status), [filter.status, searched]);
  const groups = useMemo(
    () => CLUE_STATUSES.map((item) => ({ ...item, items: visible.filter((clue) => clue.status === item.value) })).filter((group) => group.items.length),
    [visible],
  );
  const ordered = useMemo(() => groups.flatMap((group) => group.items), [groups]);
  const matchingIds = useMemo(() => ordered.map((clue) => clue.id), [ordered]);
  const selected = state.clues.find((clue) => clue.id === ui.selectedId);

  const isEmpty = sameFilter(filter, EMPTY_CLUE_FILTER);
  const chips = clueChips(state, filter);
  const describe = [filter.query.trim() ? `搜索「${filter.query.trim()}」` : "", ...chips.map((chip) => chip.label)].filter(Boolean).join("、");

  const duplicates = useMemo(
    () => findClueDuplicates(state.clues, state.merges, state.dismissedDuplicates, state.undeterminedDuplicates).filter((group) => projectId === "all" || group.projectId === projectId),
    [projectId, state.clues, state.dismissedDuplicates, state.merges, state.undeterminedDuplicates],
  );
  const history = useMemo(() => scopedHistory(state.history, "clue", state.clues, projectId), [projectId, state.clues, state.history]);

  const tagOptions = useMemo(() => {
    const tags = new Set<string>();
    scoped.forEach((clue) => clue.tags.forEach((tag) => tags.add(tag)));
    if (filter.tag) tags.add(filter.tag);
    return [{ value: "", label: "不限" }, ...Array.from(tags).sort().map((tag) => ({ value: tag, label: tag }))];
  }, [filter.tag, scoped]);
  const sourceOptions = useMemo(() => {
    const ids = new Set(scoped.map((clue) => clue.sourceId));
    if (filter.sourceId) ids.add(filter.sourceId);
    return [{ value: "", label: "不限" }, ...Array.from(ids).map((id) => ({ value: id, label: sourceTitles.get(id) ?? "已不存在的资产" }))];
  }, [filter.sourceId, scoped, sourceTitles]);

  const setChecked = useCallback((ids: string[]) => studio.setFindingsUi({ checked: ids }), []);
  const checklist = useChecklist(ui.checked, matchingIds, setChecked);
  const selecting = ui.checked.length > 0;

  useViewing(active && ui.view !== "evidences", selected ? { kind: "clue", id: selected.id } : null);
  useEffect(() => { if (selected) studio.markSeen({ kind: "clue", id: selected.id }); }, [selected]);

  const backLabel = ui.view === "duplicates" ? "可能重复" : ui.view === "history" ? "处理记录" : "全部发现";

  if (ui.view === "evidences" || ui.evidenceId) {
    return (
      <div className="flex h-full min-h-0 flex-col" data-testid="studio-findings-workspace">
        <EvidencesListView active={active} />
        <SheetAskBar sheet={sheet} target={null} />
      </div>
    );
  }

  if (selected) {
    return (
      <div className="flex h-full min-h-0 flex-col" data-testid="studio-findings-workspace">
        <ClueDetail clue={selected} onBack={() => studio.setFindingsUi({ selectedId: "" })} backLabel={backLabel} />
        <SheetAskBar sheet={sheet} target={{ kind: "clue", id: selected.id }} prefill={`关于发现「${selected.title}」：`} />
      </div>
    );
  }

  return (
    <div className="flex h-full min-h-0 flex-col" data-testid="studio-findings-workspace">
      <SurfaceToolbar>
        <SearchInput
          value={filter.query}
          onValueChange={(query) => { studio.setClueFilter({ query }); if (ui.view !== "list") studio.setFindingsUi({ view: "list" }); }}
          placeholder="搜索发现"
          aria-label="搜索发现"
          size="sm"
          className="min-w-0 flex-1"
        />
        <Menu
          ariaLabel="筛选发现"
          trigger={(
            <span className="relative inline-flex">
              <IconButton size="sm" icon="filter" label="筛选" />
              {chips.length ? <span className="pointer-events-none absolute right-1 top-1 size-1.5 rounded-full bg-cx-accent" /> : null}
            </span>
          )}
        >
          <div className="flex flex-col gap-3 p-3">
            <FilterSelect<"all" | ClueStatus> label="核对状态" value={filter.status} onChange={(status) => studio.setClueFilter({ status })} options={[{ value: "all", label: "不限" }, ...CLUE_STATUSES]} />
            <FilterSelect<"all" | FindingSeverity> label="严重度" value={filter.severity ?? "all"} onChange={(severity) => studio.setClueFilter({ severity })} options={[{ value: "all", label: "不限" }, ...FINDING_SEVERITIES]} />
            <FilterSelect<"all" | ReadinessLevel> label="交付准备度" value={filter.readiness ?? "all"} onChange={(readiness) => studio.setClueFilter({ readiness })} options={[{ value: "all", label: "不限" }, ...READINESS_LEVELS]} />
            <FilterSelect<"all" | ClueCategory> label="分类" value={filter.category} onChange={(category) => studio.setClueFilter({ category })} options={[{ value: "all", label: "不限" }, ...CLUE_CATEGORIES]} />
            <FilterSelect<ClueFilter["report"]> label="关联报告" value={filter.report ?? "all"} onChange={(report) => studio.setClueFilter({ report })} options={[{ value: "all", label: "不限" }, { value: "with", label: "已有报告" }, { value: "frozen", label: "已有冻结报告" }, { value: "without", label: "尚无报告" }]} />
            <FilterSelect<ClueFilter["stale"]> label="陈旧" value={filter.stale ?? "all"} onChange={(stale) => studio.setClueFilter({ stale })} options={[{ value: "all", label: "不限" }, { value: "stale", label: "陈旧待复核" }, { value: "fresh", label: "状态新鲜" }]} />
            <FilterSelect<string> label="出处资产" value={filter.sourceId} onChange={(sourceId) => studio.setClueFilter({ sourceId })} options={sourceOptions} searchable={sourceOptions.length > 8} />
            <FilterSelect<string> label="标签" value={filter.tag} onChange={(tag) => studio.setClueFilter({ tag })} options={tagOptions} searchable={tagOptions.length > 8} />
            <FilterField label="已归并的发现">
              <Switch checked={filter.showMerged} onCheckedChange={(showMerged) => studio.setClueFilter({ showMerged })} label="也列出已归并的" />
            </FilterField>
            {!isEmpty ? <Button size="sm" variant="ghost" className="self-start" onClick={() => studio.resetClueFilter()}>清除全部筛选</Button> : null}
          </div>
        </Menu>
        <Menu
          ariaLabel="更多"
          trigger={<IconButton size="sm" icon="more" label="更多" />}
        >
          <MenuItem icon="copy" disabled={!duplicates.length} onSelect={() => studio.setFindingsUi({ view: "duplicates" })}>可能重复{duplicates.length ? ` ${duplicates.length}` : ""}</MenuItem>
          <MenuItem icon="file" onSelect={() => studio.setFindingsUi({ view: "evidences" })}>全部证据</MenuItem>
          <MenuItem icon="listChecks" onSelect={() => studio.setFindingsUi({ view: "history" })}>处理记录</MenuItem>
        </Menu>
      </SurfaceToolbar>
      {state.domainErrors?.findings ? (
        <Callout tone="danger" className="mx-4 mt-2" action={<Button size="xs" variant="secondary" onClick={() => { void studio.reloadDomain("findings"); }}>重试加载</Button>}>
          发现与证据加载异常：{state.domainErrors.findings}
        </Callout>
      ) : null}
      {ui.view === "list" && chips.length ? <FilterChips chips={chips} onClear={() => studio.setClueFilter({ ...EMPTY_CLUE_FILTER, query: filter.query })} /> : null}
      {ui.focusIds ? (
        <Callout tone="neutral" className="mx-4 mt-2" action={<Button size="sm" variant="ghost" onClick={() => studio.setFindingsUi({ focusIds: null, focusLabel: "" })}>显示全部</Button>}>
          {ui.focusLabel || "选定的发现"} · {scoped.length} 条
        </Callout>
      ) : null}
      {ui.view === "duplicates" ? (
        <DuplicatesView kind="clue" groups={duplicates} active={active} onOpen={(id) => studio.setFindingsUi({ selectedId: id })} />
      ) : ui.view === "history" ? (
        <HistoryView kind="clue" entries={history} active={active} />
      ) : (
        <div ref={scroll.ref} className="cx-scroll min-h-0 flex-1 overflow-y-auto p-4">
          {groups.length ? (
            <div className="flex flex-col gap-4">
              {groups.map((group) => (
                <div key={group.value}>
                  <SectionHeader title={group.label} count={group.items.length} />
                  <SurfaceCard>
                    {group.items.map((clue) => (
                      <ClueRow key={clue.id} clue={clue} checked={checklist.isChecked(clue.id)} selecting={selecting} onToggle={(next, range) => checklist.toggle(clue.id, next, range)} />
                    ))}
                  </SurfaceCard>
                </div>
              ))}
              {selecting ? (
                <div className="sticky bottom-3 z-(--z-raised) mx-auto flex w-fit max-w-full flex-wrap items-center gap-1 rounded-2xl ring-1 ring-cx-ring bg-cx-elevated px-3 py-1.5 shadow-cx-md">
                  <span className="mr-1 text-cx-xs font-medium text-cx-fg-2">已选 {ui.checked.length}</span>
                  <ClueBatchActions checked={ui.checked} matchingIds={matchingIds} filterText={describe} />
                  <IconButton size="sm" icon="x" label="清除选择" onClick={() => setChecked([])} />
                </div>
              ) : null}
            </div>
          ) : (
            <EmptyState
              compact
              icon="bookmark"
              title={scoped.length ? "没有符合条件的发现" : "还没有发现"}
              description={scoped.length ? "换个关键词或筛选条件试试。" : "在资产原文里选中文字记为发现，或让 AI 帮你提取。"}
              action={scoped.length && !isEmpty ? <Button size="md" variant="secondary" onClick={() => studio.resetClueFilter()}>清除筛选和搜索</Button> : undefined}
            />
          )}
        </div>
      )}
      <SheetAskBar sheet={sheet} target={null} />
    </div>
  );
}
