"use client";

import { useCallback, useEffect, useMemo, useState } from "react";
import { cn } from "@/lib/cn";
import { Icon } from "@/components/Icon";
import {
  Button,
  Callout,
  Checkbox,
  CodeBlock,
  Collapse,
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
  toast,
} from "@/components/chat/ui";
import { TodoList, type TodoItem } from "@/components/agentui/agents/todo-list";
import { SectionHeader, SurfaceToolbar } from "@/components/chat/surfaces/shared";
import { SurfaceCard, SurfaceRow } from "@/components/chat/surfaces/OverviewSurface";
import { PathBreadcrumb } from "@/components/chat/surfaces/PathBreadcrumb";
import { STUDIO_TEAM } from "./demoData";
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
  MENU_HIT,
  NewMark,
  OPTION_HIT,
  RefLinkList,
  SheetAskBar,
  StudioBadge,
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
import { findPassage, readStudioState, studio, useStudioState, type StudioState } from "./store";

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
  if (filter.severity && filter.severity !== "all" && (clue.severity ?? "high") !== filter.severity) return false;
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
    if (filter.timeRange === "24h" && !clue.reviewedAt?.includes("10-27")) return false;
    if (filter.timeRange === "stale" && !clue.stale) return false;
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
    return <blockquote className="rounded-2xl bg-cx-bg-subtle px-4 py-3 text-[15px] leading-7 text-cx-fg">{clue.quote}</blockquote>;
  }
  const excerpt = quoteWindow(hit.passage.text, clue.quote);
  const folded = excerpt !== hit.passage.text;
  const passageRef = { kind: "passage" as const, id: hit.passage.id };
  return (
    <section aria-label="原文" className="flex flex-col gap-2 rounded-2xl border border-cx-border-subtle bg-cx-bg-subtle p-4" data-testid="studio-clue-evidence">
      <p className="text-[15px] leading-7 text-cx-fg-2">
        <HighlightedText text={full ? hit.passage.text : excerpt} quotes={[clue]} activeQuote={clue.id} />
      </p>
      <div className="flex flex-wrap items-center gap-1">
        <span className="min-w-0 flex-1 truncate text-[13px] text-cx-fg-3">《{hit.source.title}》第 {hit.passage.index} 段</span>
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
  if (!clue) return <span className="text-[13px] text-cx-fg-3">发现已不存在</span>;
  const hit = findPassage(state, clue.passageId);
  return (
    <div className="flex flex-col gap-1 rounded-xl bg-cx-bg-subtle px-3 py-2" {...dragRef({ kind: "clue", id: clue.id })}>
      <span className="text-[14px] leading-6 text-cx-fg">{clue.title}</span>
      <span className="text-[13px] leading-5 text-cx-fg-2">「{clue.quote}」</span>
      <div className="flex flex-wrap items-center gap-1">
        <span className="min-w-0 flex-1 truncate text-[13px] text-cx-fg-3">
          {hit ? `《${hit.source.title}》第 ${hit.passage.index} 段` : "原文缺失"} · {statusLabel(clue.status)}
        </span>
        <Button size="md" variant="ghost" icon="book" onClick={() => openRefInReader({ kind: "clue", id: clue.id })}>读原文</Button>
        <Button size="md" variant="ghost" iconRight="arrowRight" onClick={() => studio.setFindingsUi({ selectedId: clue.id })}>详情</Button>
      </div>
    </div>
  );
}

/**
 * Complete Finding Dossier:
 * - Importance & CVSS 3.1 basis
 * - Delivery readiness & Stale reason
 * - Confirmed & Conditional Impact
 * - Multiple linked assets (add/remove)
 * - Observed vs Inferred vs Unverified claims table
 * - Supporting evidence & Counter-evidence
 * - Missing materials
 * - Applicable target version, Reviewer & Review time
 */
/** Batch actions on checked findings: status, category, severity, tags, link, export, merge, task, delete. */
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
        className={MENU_HIT}
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
        className={MENU_HIT}
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

export function ClueDetail({ clue, onBack, backLabel = "全部发现" }: { clue: StudioClue; onBack?: () => void; backLabel?: string }) {
  const state = useStudioState();
  const [newClaimText, setNewClaimText] = useState("");
  const [newClaimKind, setNewClaimKind] = useState<FindingClaimItem["kind"]>("observed");
  const [addClaimOpen, setAddClaimOpen] = useState(false);
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
    () => (clue.supportingEvidenceIds ?? ["EX-014"]).map((id): StudioRef => ({ kind: "evidence", id })),
    [clue.supportingEvidenceIds],
  );

  const report = reports[0];
  const draftReport = () => sendAbout(target, report ? `打开报告 ${report.id}` : `请根据发现 ${clue.id}（${clue.title}）起草一份报告。`);
  const markReviewed = () => {
    studio.updateFindingDossier(clue.id, {
      stale: false,
      staleReason: undefined,
      reviewedAt: "2026-10-27 11:00",
      reviewer: "人工复核组 · 林工",
    }, "人工完成最新快照下的陈旧复核确认");
    toast({ title: "已确认复核", tone: "success" });
  };

  const gaps = clue.missingMaterials ?? [];
  const present = (clue.claims ?? []).length + evidenceRefs.length;
  const readinessItems: TodoItem[] = [
    ...evidenceRefs.map((ref, i) => ({ id: `ev-${i}`, title: `证据 ${ref.id}`, status: "completed" as const })),
    ...(clue.claims ?? []).map((c) => ({ id: c.id, title: c.text.slice(0, 40), status: "completed" as const })),
    ...gaps.map((g, i) => ({ id: `gap-${i}`, title: g, status: "pending" as const })),
  ];

  return (
    <div className="flex min-h-0 flex-1 flex-col" data-testid="studio-clue-detail">
      <SurfaceToolbar>
        {onBack ? <IconButton size="sm" icon="arrowLeft" label="返回" onClick={onBack} /> : null}
        <PathBreadcrumb path={`${backLabel === "全部发现" ? "" : `${backLabel}/`}${displayTitle(clue.title)}`} rootLabel="发现" lastIsFile onSelect={onBack} />
        <Menu ariaLabel="发现操作" className={MENU_HIT} trigger={<IconButton size="sm" icon="more" label="更多" />}>
          <MenuItem icon="pin" onSelect={() => quoteToComposer(clue.title, target)}>固定到对话</MenuItem>
          <MenuItem icon="listTodo" onSelect={() => createTaskFromClues([clue])}>转为任务</MenuItem>
          <MenuItem icon="listCollapse" onSelect={() => studio.setFindingsUi({ view: "duplicates" })}>归并候选…</MenuItem>
          <MenuSeparator />
          {clue.status !== "verified" ? (
            <MenuItem icon="checkCircle" onSelect={() => reportChange(studio.setClueStatus(clue.id, "verified"), "已经是已复核。")}>标为已复核</MenuItem>
          ) : null}
          <MenuItem icon="listChecks" onSelect={() => studio.setFindingsUi({ view: "history" })}>处理记录</MenuItem>
        </Menu>
      </SurfaceToolbar>
      <div className="cx-scroll min-h-0 flex-1 overflow-y-auto">
        <div className="mx-auto flex w-full max-w-[760px] flex-col gap-4 p-4 pb-10">
          <SurfaceCard className="p-3">
            <div className="flex items-start gap-3">
              <div className="min-w-0 flex-1">
                <p className="truncate text-[15px] font-semibold text-cx-fg">{displayTitle(clue.title)}</p>
              </div>
              <StudioBadge tone={statusTone(clue.status)}>{statusLabel(clue.status)}</StudioBadge>
            </div>
            <Dl className="mt-3" items={[
              {
                label: "严重度",
                value: (
                  <Select<FindingSeverity> value={clue.severity ?? "high"} onChange={(next) => studio.setClueSeverity(clue.id, next)} size="sm" ariaLabel="严重度" className="h-7 min-w-[110px] text-[13px]" popoverClassName={OPTION_HIT} options={FINDING_SEVERITIES.map((item) => ({ value: item.value, label: item.label }))} />
                ),
              },
              { label: "CVSS", value: `${clue.cvssScore ?? ""} ${clue.cvssVector ?? "CVSS:3.1/AV:N/AC:L/PR:L/UI:N/S:U/C:H/I:L/A:N"}`, mono: true, copy: clue.cvssVector ?? "CVSS:3.1/AV:N/AC:L/PR:L/UI:N/S:U/C:H/I:L/A:N" },
              { label: "资产", value: <RefLinkList targets={sources} max={3} /> },
              { label: "复核", value: `${clue.reviewer ?? "未复核"}${clue.reviewedAt ? ` · ${clue.reviewedAt}` : ""}` },
              { label: "适用版本", value: clue.targetVersion ?? "snapshot-v46", mono: true },
              {
                label: "分类",
                value: (
                  <Select<ClueCategory> value={clue.category} onChange={(next) => studio.setClueCategory(clue.id, next)} size="sm" ariaLabel="分类" className="h-7 min-w-[110px] text-[13px]" popoverClassName={OPTION_HIT} options={CLUE_CATEGORIES.map((item) => ({ value: item.value, label: item.label }))} />
                ),
              },
              {
                label: "状态",
                value: (
                  <Select<ClueStatus> value={clue.status} onChange={(next) => studio.setClueStatus(clue.id, next)} size="sm" ariaLabel="状态" className="h-7 min-w-[110px] text-[13px]" popoverClassName={OPTION_HIT} options={CLUE_STATUSES.map((item) => ({ value: item.value, label: item.label }))} />
                ),
              },
            ]} />
            <div className="mt-3 flex items-center gap-2">
              <Button size="sm" variant="primary" icon="file" onClick={draftReport}>{report ? "打开报告" : "起草报告"}</Button>
            </div>
          </SurfaceCard>

          {clue.stale ? (
            <Callout tone="warning" action={<Button size="sm" variant="secondary" onClick={markReviewed}>已复核</Button>}>
              资料已更新，结论需复核{clue.staleReason ? `：${clue.staleReason}` : ""}
            </Callout>
          ) : null}

          <MergeInfo kind="clue" id={clue.id} memberDetail={(memberId) => <MergedMember clueId={memberId} />} />

          <div>
            {readinessItems.length ? (
              <TodoList
                items={readinessItems}
                collapseOnComplete
                title={(
                  <span className="flex w-full items-center justify-between gap-2">
                    <span>交付准备 {present}/{present + gaps.length}</span>
                    {gaps.length ? <button type="button" className="text-[12px] font-medium text-cx-accent hover:underline" onClick={(e) => { e.stopPropagation(); sendAbout(target, `发现 ${clue.id} 还缺 ${gaps.length} 项材料，请帮我补齐。`); }}>让 Agent 补齐</button> : null}
                  </span>
                )}
              />
            ) : null}
          </div>

          <div>
            <SectionHeader
              title="主张"
              count={clue.claims?.length ?? 0}
              action={<button type="button" className="text-[12px] font-medium text-cx-accent hover:underline" onClick={() => setAddClaimOpen((v) => !v)}>添加主张</button>}
            />
            <SurfaceCard>
              {(["observed", "inferred", "unverified"] as const).map((kind) => {
                const claims = (clue.claims ?? []).filter((claim) => claim.kind === kind);
                if (!claims.length) return null;
                return (
                  <div key={kind}>
                    <p className="px-2 pb-0.5 pt-1.5 text-[12px] font-medium text-cx-fg-4">{kind === "observed" ? "观察" : kind === "inferred" ? "推断" : "未验证"}</p>
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
                  className="h-7 min-w-[110px] text-[13px]"
                  popoverClassName={OPTION_HIT}
                  options={[
                    { value: "observed", label: "观察" },
                    { value: "inferred", label: "推断" },
                    { value: "unverified", label: "未验证" },
                  ]}
                />
                <Input size="sm" value={newClaimText} onChange={(e) => setNewClaimText(e.target.value)} placeholder="补充一条主张…" aria-label="新主张内容" className="min-w-[160px] flex-1" />
                <Button size="sm" variant="secondary" disabled={!newClaimText.trim()} onClick={() => {
                  studio.addFindingClaim(clue.id, { text: newClaimText.trim(), kind: newClaimKind, evidenceIds: ["EX-014"] });
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
            <p className="px-1 text-[13px] leading-6 text-cx-fg">{clue.impactSummary ?? "已在演示沙箱通过对照报文确认配置边界不一致。"}</p>
            {clue.conditionalImpact ? <p className="px-1 pt-1 text-[12px] leading-5 text-cx-fg-4">{clue.conditionalImpact}</p> : null}
          </div>

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

// ---------------------------------------------------------------------------
// Evidence drill-in
// ---------------------------------------------------------------------------

function EvidenceDetail({ evidence, parent, onBack }: { evidence: StudioEvidence; parent?: StudioClue; onBack?: () => void }) {
  const state = useStudioState();
  const [diffMode, setDiffMode] = useState<"denoised" | "raw">("denoised");
  const [replayOpen, setReplayOpen] = useState(false);
  const [replayBody, setReplayBody] = useState('{"tenant_id":"tenant-b-target","format":"summary","dry_run":true}');
  const target: StudioRef = { kind: "evidence", id: evidence.id };
  const counterpart = evidence.baselineExchangeId ? state.evidences.find((e) => e.id === evidence.baselineExchangeId) : undefined;

  const stripNoise = (raw: string) =>
    raw.split(/\r?\n/).filter((line) => !/^(Date|X-Trace-Id|ETag):/i.test(line.trim())).join("\n");

  const completenessLabel = evidence.completeness === "complete" ? "完整" : evidence.completeness === "partial" ? "部分截断" : evidence.completeness === "revoked" ? "已撤回" : "不可用";
  const blocked = evidence.completeness === "revoked" || evidence.completeness === "partial";
  const crumbPath = parent ? `${parent.title}/${evidence.id}` : evidence.id;

  return (
    <div className="flex min-h-0 flex-1 flex-col" data-testid="studio-evidence-detail">
      <SurfaceToolbar>
        {onBack ? <IconButton size="sm" icon="arrowLeft" label="返回" onClick={onBack} /> : null}
        <PathBreadcrumb path={crumbPath} rootLabel="发现" lastIsFile onSelect={onBack} />
        <Menu ariaLabel="证据操作" className={MENU_HIT} trigger={<IconButton size="sm" icon="more" label="更多" />}>
          {evidence.kind === "screenshot" && !evidence.imageReviewedBy ? (
            <MenuItem icon="eye" onSelect={() => { studio.markEvidenceImageReviewed(evidence.id); toast({ title: "已签署原图审阅记录", tone: "success" }); }}>标记已审阅</MenuItem>
          ) : null}
          <MenuItem icon="pin" onSelect={() => quoteToComposer(evidence.title, target)}>固定到对话</MenuItem>
        </Menu>
      </SurfaceToolbar>
      <div className="cx-scroll min-h-0 flex-1 overflow-y-auto">
        <div className="mx-auto flex w-full max-w-[760px] flex-col gap-4 p-4 pb-10">
          <SurfaceCard className="p-3">
            <div className="flex items-start gap-3">
              <div className="min-w-0 flex-1">
                <p className="truncate text-[15px] font-semibold text-cx-fg">{displayTitle(evidence.title)}</p>
              </div>
              <span className="flex shrink-0 items-center gap-2">
                {evidence.sha256 ? (
                  <Button size="sm" variant="ghost" icon="check" onClick={() => studio.verifyEvidenceDigest(evidence.id)} aria-label="校验摘要">
                    {evidence.digestVerified === "verified" ? "已校验" : evidence.digestVerified === "mismatch" ? "摘要不符" : "校验摘要"}
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
              { label: "SHA-256", value: `${evidence.sha256.slice(0, 16)}…`, mono: true, copy: evidence.sha256 },
              { label: "来源", value: `${evidence.sourceKind} · ${evidence.version}` },
            ]} />
          </SurfaceCard>

          {blocked ? (
            <Callout tone={evidence.completeness === "revoked" ? "danger" : "warning"}>
              {evidence.completeness === "revoked" ? "此证据已被撤回，不应再被报告引用。" : "此证据内容被部分截断。"}
            </Callout>
          ) : null}

          {evidence.kind === "screenshot" && evidence.visualPreviewData ? (
            <div>
              <SectionHeader title="截图" action={evidence.imageReviewedBy ? <span className="text-[12px] text-cx-fg-4">已审阅 · {evidence.imageReviewedBy}</span> : undefined} />
              <div className="overflow-hidden rounded-xl border border-cx-border bg-cx-bg-subtle" data-testid="studio-evidence-screenshot">
                <div className="flex items-center gap-2 border-b border-cx-border-subtle bg-cx-hover px-3 py-1.5 text-[12px] text-cx-fg-3">
                  <Icon name="globe" size={13} />
                  <span className="truncate font-cx-mono">{evidence.captureUrl}</span>
                  <span className="ml-auto shrink-0">{evidence.dimensions}</span>
                </div>
                <div className="flex flex-col gap-3 p-4">
                  <p className="text-[13px] font-medium text-cx-fg">{evidence.visualPreviewData.caption}</p>
                  <CodeBlock code={evidence.visualPreviewData.domHighlight} language="html" className="max-h-[160px]" />
                  <div className="grid grid-cols-2 gap-2 text-[12.5px] max-sm:grid-cols-1">
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
              </div>
            </div>
          ) : null}

          {evidence.kind === "http_exchange" ? (
            <div>
              <SectionHeader title="报文" action={<span className="font-cx-mono text-[12px] text-cx-fg-4">{evidence.method} → {evidence.statusCode}</span>} />
              <CodeBlock code={`# 请求\n${evidence.requestRaw || "无"}\n\n# 响应\n${evidence.responseRaw || "无"}`} language="http" className="max-h-[320px]" />
              {counterpart || evidence.diffBefore ? (
                <div className="mt-3">
                  <SectionHeader title="对比" action={(
                    <Select<"denoised" | "raw"> value={diffMode} onChange={setDiffMode} size="sm" ariaLabel="差异模式" className="h-7 min-w-[90px] text-[12px]" options={[{ value: "denoised", label: "降噪" }, { value: "raw", label: "原始" }]} />
                  )} />
                  <CodeBlock
                    code={`# 之前\n${counterpart ? (diffMode === "denoised" ? stripNoise(counterpart.responseRaw ?? "") : counterpart.responseRaw ?? "") : evidence.diffBefore ?? ""}\n\n# 当前\n${diffMode === "denoised" ? stripNoise(evidence.responseRaw ?? "") : evidence.responseRaw ?? ""}`}
                    language="http"
                    className="max-h-[240px]"
                  />
                </div>
              ) : null}
              <Collapse open={replayOpen} className="mt-2">
                <div className="flex flex-wrap items-center gap-2 p-1">
                  <Input size="sm" value={replayBody} onChange={(e) => setReplayBody(e.target.value)} aria-label="重放请求体" className="min-w-[220px] flex-1 font-cx-mono" />
                  <Button size="sm" variant="secondary" icon="play" onClick={() => {
                    const id = studio.replayHttpEvidence(evidence.id, replayBody);
                    if (id) toast({ title: `已生成新证据 ${id}`, tone: "success" });
                  }}>执行重放</Button>
                </div>
              </Collapse>
              <button type="button" className="mt-2 text-[12px] font-medium text-cx-accent hover:underline" onClick={() => setReplayOpen((v) => !v)}>{replayOpen ? "收起重放实验" : "重放实验…"}</button>
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

function EvidencesListView({ active }: { active: boolean }) {
  const state = useStudioState();
  const ui = state.workspace.findings;
  const scroll = useRememberedScroll("findings-evidences", active);
  const [kindFilter, setKindFilter] = useState<"all" | EvidenceKind>("all");
  const evidences = useMemo(() => state.evidences.filter((e) => kindFilter === "all" || e.kind === kindFilter), [kindFilter, state.evidences]);
  const selected = state.evidences.find((e) => e.id === ui.evidenceId);

  useViewing(active, selected ? { kind: "evidence", id: selected.id } : null);

  if (selected) {
    const parent = ui.evidenceParent ? state.clues.find((c) => c.id === ui.evidenceParent) : state.clues.find((c) => (c.supportingEvidenceIds ?? []).includes(selected.id));
    return <EvidenceDetail evidence={selected} parent={parent} onBack={() => studio.setFindingsUi({ evidenceId: "", evidenceParent: "" })} />;
  }

  return (
    <div className="flex min-h-0 flex-1 flex-col" data-testid="studio-evidences-list">
      <SurfaceToolbar>
        <IconButton size="sm" icon="arrowLeft" label="返回" onClick={() => studio.setFindingsUi({ view: "list" })} />
        <PathBreadcrumb path="全部证据" rootLabel="发现" lastIsFile onSelect={() => studio.setFindingsUi({ view: "list" })} />
        <span className="min-w-0 flex-1" />
        <Select<"all" | EvidenceKind>
          value={kindFilter}
          onChange={setKindFilter}
          size="sm"
          options={[{ value: "all", label: "全部类型" }, ...Object.entries(EVIDENCE_KIND_LABELS).map(([k, lbl]) => ({ value: k as EvidenceKind, label: lbl }))]}
          ariaLabel="筛选证据类型"
          className="h-7 min-w-[130px] text-[13px]"
          popoverClassName={OPTION_HIT}
        />
      </SurfaceToolbar>
      <div ref={scroll.ref} className="cx-scroll min-h-0 flex-1 overflow-y-auto p-4">
        <SurfaceCard>
          {evidences.map((ev) => (
            <SurfaceRow
              key={ev.id}
              icon="file"
              title={displayTitle(ev.title)}
              subtitle={[EVIDENCE_KIND_LABELS[ev.kind], ev.capturedAt].join(" · ")}
              trailing={ev.completeness === "revoked" ? <span className="shrink-0 text-[12px] text-cx-danger">已撤回</span> : ev.completeness === "partial" ? <span className="shrink-0 text-[12px] text-cx-warning">截断</span> : undefined}
              onClick={() => studio.setFindingsUi({ evidenceId: ev.id })}
            />
          ))}
          {!evidences.length ? <SurfaceRow icon="file" title="没有此类证据" /> : null}
        </SurfaceCard>
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
  const missing = (clue.missingMaterials ?? []).length;
  const target: StudioRef = { kind: "clue", id: clue.id };
  const lead = (
    <span className="relative grid size-8 shrink-0 place-items-center">
      <span className={cn("absolute inset-0 grid place-items-center rounded-lg bg-cx-hover text-cx-fg-3", (selecting || checked) && "hidden", "group-hover/row:hidden")}>
        <Icon name="bookmark" size={15} />
      </span>
      <span className={cn("absolute inset-0 place-items-center", (selecting || checked) ? "grid" : "hidden group-hover/row:grid")}>
        <Checkbox checked={checked} aria-label={`勾选「${clue.title}」`} onCheckedChange={(next) => onToggle(Boolean(next), false)} className="[&>button]:size-5" />
      </span>
    </span>
  );
  return (
    <span className="group/row block" {...dragRef(target)}>
      <SurfaceRow
        icon="bookmark"
        leading={lead}
        selected={checked}
        title={<>{displayTitle(clue.title)}<NewMark target={target} /><MergeMark kind="clue" id={clue.id} /></>}
        subtitle={[severityLabel(clue.severity ?? "high"), categoryLabel(clue.category), missing ? `缺 ${missing} 项材料` : null].filter(Boolean).join(" · ")}
        trailing={clue.stale ? <span className="shrink-0 text-[12px] text-cx-warning">陈旧</span> : undefined}
        onClick={() => studio.setFindingsUi({ selectedId: clue.id })}
      />
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
          className={MENU_HIT}
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
          className={MENU_HIT}
          trigger={<IconButton size="sm" icon="more" label="更多" />}
        >
          <MenuItem icon="copy" disabled={!duplicates.length} onSelect={() => studio.setFindingsUi({ view: "duplicates" })}>可能重复{duplicates.length ? ` ${duplicates.length}` : ""}</MenuItem>
          <MenuItem icon="file" onSelect={() => studio.setFindingsUi({ view: "evidences" })}>全部证据</MenuItem>
          <MenuItem icon="listChecks" onSelect={() => studio.setFindingsUi({ view: "history" })}>处理记录</MenuItem>
        </Menu>
      </SurfaceToolbar>
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
                <div className="sticky bottom-3 z-10 mx-auto flex w-fit items-center gap-1 rounded-2xl border border-cx-border bg-cx-elevated px-3 py-1.5 shadow-cx-md">
                  <span className="mr-1 text-[12px] font-medium text-cx-fg-2">已选 {ui.checked.length}</span>
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
