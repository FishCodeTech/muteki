"use client";

import { useSyncExternalStore } from "react";
import type {
  ConversationMessage,
  ConversationThread,
  ConversationTurn,
  ConversationView,
  ThreadPlanSnapshot,
} from "@/lib/useConversation";
import { cleanDemoText, displayTitle } from "./records";
import { buildDemoData, REPORT_TEMPLATES, STUDIO_DEFAULT_PROJECT_ID, STUDIO_DEFAULT_THREAD_ID, STUDIO_TEAM, STUDIO_THREAD_SEEDS } from "./demoData";
import {
  CLUE_CATEGORIES,
  SOURCE_KINDS,
  TASK_STATUSES,
  categoryLabel,
  readinessLabel,
  retestVerdictLabel,
  sameRef,
  severityLabel,
  sourceKindLabel,
  statusLabel,
  submissionStatusLabel,
  type AssetImportance,
  type ClueCategory,
  type ClueStatus,
  type FindingClaimItem,
  type FindingSeverity,
  type ReadinessLevel,
  type RetestVerdict,
  type SourceKind,
  type StudioApproval,
  type StudioChangeRecord,
  type StudioClue,
  type StudioConnector,
  type StudioEnvironmentStatus,
  type StudioEvidence,
  type StudioGrant,
  type StudioMonitor,
  type StudioPassage,
  type StudioPlanQuestion,
  type StudioPlanVersion,
  type StudioPlatformFeedback,
  type StudioProject,
  type StudioRef,
  type StudioReport,
  type StudioRetestRecord,
  type StudioScopeMaterial,
  type StudioScopeSnapshot,
  type StudioSource,
  type StudioSubmissionLedger,
  type StudioTask,
  type StudioWorkRun,
  type TaskStatus,
} from "./model";
import {
  countImportOutcomes,
  resolveSourceKind,
  splitPassages,
  type ImportCounts,
  type ImportDraft,
  type ImportFormat,
  type ImportOutcome,
  type ImportSettings,
  type ImportValues,
} from "./importParsing";
import {
  EMPTY_CLUE_FILTER,
  EMPTY_SOURCE_FILTER,
  RECORD_NOUN,
  buildAiMergeSuggestions,
  countLabel,
  duplicateKey,
  findClueDuplicates,
  mergeOf,
  sourceStatusLabel,
  type ClueFilter,
  type FieldChange,
  type HistoryEntry,
  type MergeRecord,
  type RecordKind,
  type RecordView,
  type SavedFilter,
  type SourceFilter,
  type TagChange,
  type UndeterminedDuplicate,
} from "./records";

export { EMPTY_CLUE_FILTER, type ClueFilter } from "./records";
export { REPORT_TEMPLATES };

/**
 * Single store shared by the conversation preview and the workspaces in the
 * conversation's right panel. Persistence saves `PersistedState` in this
 * browser's IndexedDB and also supports simulated server sync and JSON archive export/import.
 */

export interface StudioThreadRecord {
  id: string;
  projectId: string;
  title: string;
  createdAt: string;
  updatedAt: string;
  messages: ConversationMessage[];
  turns: ConversationTurn[];
  plan: ThreadPlanSnapshot | null;
  seq: number;
}

export type SuggestionKind =
  | "recategorize"
  | "finding"
  | "task"
  | "merge"
  | "plan_update"
  | "report_draft"
  | "conflict_resolve"
  | "retest_record";

/**
 * A change a simulated reply proposes. Nothing changes until the user adopts
 * it; `result` then points at the object it changed or created, so a
 * second adoption is a no-op.
 */
export interface StudioSuggestion {
  id: string;
  kind: SuggestionKind;
  title: string;
  rationale?: string;
  clueId?: string;
  from?: ClueCategory;
  to?: ClueCategory;
  sourceId?: string;
  passageId?: string;
  quote?: string;
  category?: ClueCategory;
  /** For `merge` suggestions: */
  mergeKind?: RecordKind;
  primaryId?: string;
  memberIds?: string[];
  /** For `conflict_resolve` suggestions: */
  conflictId?: string;
  conflictValue?: string;
  /** For `plan_update` suggestions: */
  planFocusQuestion?: string;
  planJudgement?: string;
  /** For `report_draft` suggestions: */
  reportId?: string;
  /** For `retest_record` suggestions: */
  retestVerdict?: RetestVerdict;
  result?: StudioRef;
}

/** Progress of a task handed to the simulated assistant. */
export interface TaskRun {
  threadId: string;
  turnId: string;
  steps: string[];
  step: number;
  done: boolean;
}

export type FindingsGroup = "category" | "status" | "source";

export interface LibraryUi extends SourceFilter {
  /** Checked sources: the scope of batch actions, and chat context while the library is visible. */
  checked: string[];
  view: RecordView;
  /** Source open in the reader; empty shows the list. */
  sourceId: string;
  passageId: string;
  clueId: string;
  /** Active sub-tab in the asset reader. */
  readerTab?: "passages" | "attributes" | "versions" | "relations";
  /** Shows the import history instead of the list; the reader still opens over it. */
  importsOpen: boolean;
  /** Import batch open in the history; empty lists all batches. */
  importBatchId: string;
  /** Optional 1-based line focused in the import batch original file view (for shareable links). */
  importFocusLine?: number;
}

export interface ImportResult {
  outcome: ImportOutcome;
  /** Why the record got its outcome, in words shown to the user. */
  reason: string;
  /** Source the record created, updated or matched. */
  sourceId?: string;
}

export interface ImportItem extends ImportDraft, ImportResult {
  id: string;
  /** How many times the record has been applied, the first import included. */
  attempts: number;
}

/** Snapshot of a source before an import batch updated it, so the batch can be undone cleanly. */
export interface ImportPreviousSource {
  id: string;
  existed: boolean;
  previous?: StudioSource;
  previousOrigin?: ImportOrigin;
}

/** One imported file or connector sync: its original text, settings and every record's result. */
export interface ImportBatch {
  id: string;
  projectId: string;
  fileName: string;
  format: ImportFormat;
  size: number;
  importedAt: string;
  text: string;
  settings: ImportSettings;
  items: ImportItem[];
  undoneAt?: string;
  previousSources?: ImportPreviousSource[];
  connectorMeta?: {
    connectorId: string;
    sourceSystem: string;
    query: string;
    completeness: "complete" | "partial" | "quota_limited";
    costCredits: number;
    receiptId: string;
  };
}

/** Import identity of a source, so a later import of the same record updates it. */
export interface ImportOrigin {
  batchId: string;
  /** Value of the mapped unique-key column; "" when the source was matched by title. */
  key: string;
}

export interface FindingsUi {
  selectedId: string;
  groupBy: FindingsGroup;
  /** Checked findings: the scope of batch actions, and chat context while findings are visible. */
  checked: string[];
  view: RecordView;
  /** Restricts the list to these clues (e.g. the ones a reply cited). */
  focusIds: string[] | null;
  focusLabel: string;
  /** Collapsed groups as `${groupBy}:${key}`; collapsing never unchecks. */
  collapsed: string[];
  /** Selected standalone evidence id when viewing `evidences`. */
  evidenceId?: string;
  /** Finding the evidence drill-in was entered from (breadcrumb parent). */
  evidenceParent?: string;
}

export type TasksSubTab = "overview" | "todo" | "plan" | "runs" | "monitors" | "scope" | "env" | "links";

export interface TasksUi {
  highlightId: string;
  query: string;
  showDone: boolean;
  tab?: TasksSubTab;
  selectedRunId?: string;
  selectedPlanId?: string;
  selectedMonitorId?: string;
  selectedChangeId?: string;
  selectedApprovalId?: string;
}

export type ReportsSubTab = "reports" | "feedbacks" | "retests";

export interface ReportsUi {
  tab: ReportsSubTab;
  /** "list" shows the grouped report list; "report" drills into selectedReportId. */
  view?: "list" | "report";
  selectedReportId: string;
  compareReportId: string;
  selectedFeedbackId: string;
  selectedRetestId: string;
}

export type VisibleWorkspace = "library" | "findings" | "tasks" | "reports" | null;
export type RuntimeSimulationMode = "local_rules" | "simulated_agent_mcp";

export interface StudioState {
  projects: StudioProject[];
  sources: StudioSource[];
  clues: StudioClue[];
  tasks: StudioTask[];
  planVersions: StudioPlanVersion[];
  planQuestions: StudioPlanQuestion[];
  scopeMaterials: StudioScopeMaterial[];
  evidences: StudioEvidence[];
  workRuns: StudioWorkRun[];
  reports: StudioReport[];
  feedbacks: StudioPlatformFeedback[];
  retests: StudioRetestRecord[];
  changes: StudioChangeRecord[];
  monitors: StudioMonitor[];
  scopeSnapshots: StudioScopeSnapshot[];
  approvals: StudioApproval[];
  grants: StudioGrant[];
  environmentStatus: StudioEnvironmentStatus;
  connectors: StudioConnector[];
  threads: Record<string, StudioThreadRecord>;
  threadOrder: string[];
  /** Which host mounted this state ("demo" | task:<id>); runtime-only, not persisted. */
  hostTag: string;
  /** Objects pinned as chat context, per thread. Only explicit actions add to it. */
  context: Record<string, StudioRef[]>;
  /** Clues cited by the latest simulated reply, per thread. */
  findings: Record<string, string[]>;
  /** Clues cited by each simulated reply, keyed by `turnKey`; complete, not only the summarized ones. */
  turnFindings: Record<string, string[]>;
  /** All objects (reports, evidences, runs, changes, feedbacks, etc.) cited by a reply, keyed by `turnKey`. */
  turnRefs: Record<string, StudioRef[]>;
  /** Changes proposed by each simulated reply, keyed by `turnKey`. */
  suggestions: Record<string, StudioSuggestion[]>;
  /** What each proposal kind covers, keyed by `turnKey`. */
  suggestionScopes: Record<string, Partial<Record<SuggestionKind, string>>>;
  /** Per reply: requested operations the local rules found nothing to change for, with the reason. */
  suggestionEmpty: Record<string, Partial<Record<SuggestionKind, string>>>;
  /** Task a reply worked on, keyed by `turnKey`. */
  turnTasks: Record<string, string>;
  /** Task waiting in the composer to be handed to the assistant, per thread. */
  handoff: Record<string, string>;
  taskRuns: Record<string, TaskRun>;
  activeThreadId: string;
  /** Object the visible workspace shows; offered as automatic context. */
  viewing: StudioRef | null;
  /** The user removed `viewing` from the next message's context. */
  viewingExcluded: boolean;
  /** The user removed the checked findings from the next message's context. */
  selectionExcluded: boolean;
  /** Objects added in this session; workspaces mark them as new until opened. */
  recent: StudioRef[];
  /** Objects changed by adopted suggestions or imports; marked as updated until opened. */
  updated: StudioRef[];
  /** Local file imports & connector batches, newest first. */
  imports: ImportBatch[];
  /** Keyed by source id; only sources an import created or updated have one. */
  importOrigins: Record<string, ImportOrigin>;
  /** Duplicate merges, undone ones included, oldest first. */
  merges: MergeRecord[];
  /** What was done to sources, findings, tasks, reports and approvals, newest first (no hard 2000 truncation). */
  history: HistoryEntry[];
  savedFilters: SavedFilter[];
  /** Duplicate groups the user marked as not duplicates, as `duplicateKey`. */
  dismissedDuplicates: string[];
  /** Duplicate groups the user marked as "无法判断" (undetermined), keyed by `duplicateKey`. */
  undeterminedDuplicates: Record<string, UndeterminedDuplicate>;
  /** Simulated runtime mode: local rules vs simulated Agent/MCP tool-call pipeline. */
  runtimeMode: RuntimeSimulationMode;
  /** Last real-domain effect that failed (optimistic state was rolled back). */
  effectError?: { message: string; code: string; domain: string; at: string } | null;
  /** Partial-load notice for the asset library: set when the server returned fewer than the total. */
  assetLoad?: { loaded: number; total: number; complete: boolean } | null;
  /** Simulated cloud/server sync status for cross-device persistence. */
  serverSync: {
    lastSyncedAt: string;
    syncRev: number;
    deviceName: string;
  };
  /** Scope and per-workspace view state; kept while workspaces are switched or closed. */
  workspace: {
    projectId: string;
    clueFilter: ClueFilter;
    library: LibraryUi;
    findings: FindingsUi;
    tasks: TasksUi;
    reports: ReportsUi;
  };
}

export const DEFAULT_THREAD_ID = STUDIO_DEFAULT_THREAD_ID;
export const NEW_THREAD_TITLE = "新的研究对话";
export const SIMULATION_LABEL = "模拟环境";
export const SIMULATION_DESCRIPTION =
  "资产、发现、任务、报告、证据与监控保存在本地浏览器（并支持模拟云端快照同步与存档导出导入）；AI 回复与 MCP/Pentest 推进均使用可检查的本地演示数据。";
export const LOCAL_MODEL_LABEL = "本地模拟";
export const AGENT_MCP_MODEL_LABEL = "Agent + MCP 模拟";
export const PLAN_STEPS = ["读取对话与关联对象上下文", "检索原文段落与独立证据", "对照计划、范围与同源归并规则", "生成可追溯回复与回写建议"];
export const AGENT_MCP_STEPS = [
  "AgentCapabilityGateway 核验 Task 绑定与策略版本",
  "调用 MCP: src.asset.query & src.exchange.compare",
  "调用 Pentest / 证据索引核对主张与冲突",
  "生成结构化结论与逐项可采纳回写建议",
];
/** Clues the reply text details; the cited list and the proposals cover all of them. */
export const REPLY_SUMMARY_LIMIT = 5;

const LOCAL_RUNTIME_SNAPSHOT: NonNullable<ConversationTurn["runtime_snapshot"]> = {
  adapter_id: "local-simulation",
  instance_id: "demo",
  model: LOCAL_MODEL_LABEL,
};
const STEP_MS = 650;
const RECENT_LIMIT = 60;
const FINDING_SUGGESTION_LIMIT = 6;
const TIME_PATTERN = /\d+\s*[年月日号时点]|[一二三四五六七八九十]+[月日号点]|凌晨|清晨|上午|中午|下午|傍晚|晚上|夜里|当天|次日|前一天|第二天/;
const RECATEGORIZE_PATTERN = /归类|分类|归为|归到|改为|改成|重新整理/;
const FINDING_PATTERN = /提取|记为发现|新发现|创建发现|找出|摘出|整理出/;
const TASK_PATTERN = /任务|核实|跟进|核对|待办/;
const MERGE_PATTERN = /归并|重复|合并|同源|同根因|去重/;
const PLAN_PATTERN = /计划|方案|目标|问题清单|修订/;
const REPORT_PATTERN = /报告|交付|草稿|冻结|补证|模板/;
const CONFLICT_PATTERN = /冲突|不一致|裁决|归属/;
const RETEST_PATTERN = /复测|复核|已解决|修复|仍存在/;

const EMPTY_LIBRARY_UI: LibraryUi = {
  ...EMPTY_SOURCE_FILTER,
  checked: [],
  view: "list",
  sourceId: "",
  passageId: "",
  clueId: "",
  readerTab: "passages",
  importsOpen: false,
  importBatchId: "",
  importFocusLine: undefined,
};
const IMPORT_TITLE_LIMIT = 200;
const EMPTY_FINDINGS_UI: FindingsUi = {
  selectedId: "F-052",
  groupBy: "category",
  checked: [],
  view: "list",
  focusIds: null,
  focusLabel: "",
  collapsed: [],
  evidenceId: "EX-014",
};
const INTERRUPTED_REPLY = "页面在这条回复完成前刷新或关闭了，回复没有生成，也没有修改任何数据。可以重新发送。";
const EMPTY_TASKS_UI: TasksUi = {
  highlightId: "",
  query: "",
  showDone: false,
  tab: "overview",
  selectedRunId: "demo-run-046",
  selectedPlanId: "PLAN-046",
  selectedMonitorId: "MON-001",
  selectedChangeId: "CHG-046-A",
  selectedApprovalId: "APP-001",
};
const EMPTY_REPORTS_UI: ReportsUi = {
  tab: "reports",
  selectedReportId: "R-052",
  compareReportId: "R-052-v1",
  selectedFeedbackId: "FB-001",
  selectedRetestId: "RETEST-002",
};

export function turnKey(threadId: string, turnId: string): string {
  return `${threadId}/${turnId}`;
}

/** Message id of a simulated reply; the conversation's `?message=` deep link accepts it. */
export function replyMessageId(turnId: string): string {
  return `${turnId}-reply`;
}

function emptyThread(id: string, projectId: string, title: string, at: string): StudioThreadRecord {
  return { id, projectId, title, createdAt: at, updatedAt: at, messages: [], turns: [], plan: null, seq: 0 };
}

function pushExchange(record: StudioThreadRecord, turnId: string, userText: string, at: string, modelLabel: string): StudioThreadRecord {
  const seq = record.seq + 1;
  return {
    ...record,
    messages: [
      ...record.messages,
      { message_id: `${turnId}-user`, turn_id: turnId, role: "user", kind: "message", text: userText, stream_seq: seq, created_at: at },
    ],
    turns: [
      ...record.turns,
      {
        turn_id: turnId,
        kind: "user",
        status: "running",
        text: userText,
        runtime_snapshot: { ...LOCAL_RUNTIME_SNAPSHOT, model: modelLabel },
        created_at: at,
      },
    ],
    seq,
    updatedAt: at,
  };
}

function dedupeRefs(refs: StudioRef[]): StudioRef[] {
  const out: StudioRef[] = [];
  refs.forEach((ref) => { if (!out.some((item) => sameRef(item, ref))) out.push(ref); });
  return out;
}

function sentences(text: string): string[] {
  return text.replace(/([。！？；])/g, "$1\n").split("\n").map((part) => part.trim()).filter((part) => part.length >= 8);
}

function categoryInText(text: string): ClueCategory | null {
  return CLUE_CATEGORIES.find((item) => text.includes(item.label))?.value ?? null;
}

/** Category a recategorize request moves `clue` to, or null when it stays. */
function recategorizeTarget(clue: StudioClue, target: ClueCategory | null): ClueCategory | null {
  const next = target && target !== "timeline" ? target : TIME_PATTERN.test(clue.quote) ? "timeline" : null;
  return next && next !== clue.category ? next : null;
}

type SuggestionInput = Pick<
  StudioState,
  | "clues"
  | "tasks"
  | "sources"
  | "merges"
  | "dismissedDuplicates"
  | "undeterminedDuplicates"
  | "reports"
  | "feedbacks"
  | "changes"
  | "planVersions"
>;

/** Context refs with every referenced task/run/report/feedback/change expanded with its linked objects. */
function expandRefs(current: Pick<StudioState, "tasks" | "workRuns" | "reports" | "feedbacks" | "changes">, refs: StudioRef[]): StudioRef[] {
  return dedupeRefs(
    refs.flatMap((ref): StudioRef[] => {
      if (ref.kind === "task") {
        return [ref, ...(current.tasks.find((task) => task.id === ref.id)?.links ?? [])];
      }
      if (ref.kind === "run") {
        const run = current.workRuns.find((item) => item.id === ref.id);
        return run ? [ref, ...run.linkedRefs, ...run.artifactRefs] : [ref];
      }
      if (ref.kind === "report") {
        const report = current.reports.find((item) => item.id === ref.id);
        return report
          ? [
              ref,
              { kind: "clue", id: report.findingId },
              ...report.evidenceIds.map((id): StudioRef => ({ kind: "evidence", id })),
            ]
          : [ref];
      }
      if (ref.kind === "feedback") {
        const fb = current.feedbacks.find((item) => item.id === ref.id);
        return fb
          ? [ref, { kind: "report", id: fb.reportId }, { kind: "clue", id: fb.findingId }]
          : [ref];
      }
      if (ref.kind === "change") {
        const chg = current.changes.find((item) => item.id === ref.id);
        return chg
          ? [
              ref,
              ...chg.affectedAssetIds.map((id): StudioRef => ({ kind: "source", id })),
              ...chg.affectedFindingIds.map((id): StudioRef => ({ kind: "clue", id })),
            ]
          : [ref];
      }
      return [ref];
    }),
  );
}

function passagesFor(
  current: Pick<StudioState, "clues" | "tasks" | "sources" | "workRuns" | "reports" | "feedbacks" | "changes">,
  refs: StudioRef[],
  clues: StudioClue[],
): Array<{ source: StudioSource; passage: StudioPassage }> {
  const out: Array<{ source: StudioSource; passage: StudioPassage }> = [];
  const add = (source: StudioSource | undefined, passage: StudioPassage | undefined) => {
    if (source && passage && !out.some((item) => item.passage.id === passage.id)) out.push({ source, passage });
  };
  for (const ref of expandRefs(current, refs)) {
    if (ref.kind === "source") {
      const source = current.sources.find((item) => item.id === ref.id);
      source?.passages.forEach((passage) => add(source, passage));
    }
    if (ref.kind === "passage") {
      const source = current.sources.find((item) => item.passages.some((passage) => passage.id === ref.id));
      add(source, source?.passages.find((passage) => passage.id === ref.id));
    }
  }
  if (out.length) return out;
  clues.forEach((clue) => {
    const source = current.sources.find((item) => item.id === clue.sourceId);
    add(source, source?.passages.find((passage) => passage.id === clue.passageId));
  });
  return out;
}

function buildSuggestions(
  current: SuggestionInput & Pick<StudioState, "workRuns">,
  text: string,
  clues: StudioClue[],
  refs: StudioRef[],
  prefix: string,
): {
  items: StudioSuggestion[];
  scopes: Partial<Record<SuggestionKind, string>>;
  empty: Partial<Record<SuggestionKind, string>>;
} {
  const items: StudioSuggestion[] = [];
  const scopes: Partial<Record<SuggestionKind, string>> = {};
  const empty: Partial<Record<SuggestionKind, string>> = {};
  const wantsRecategorize = RECATEGORIZE_PATTERN.test(text);
  const wantsFindings = FINDING_PATTERN.test(text);
  const wantsMerge = MERGE_PATTERN.test(text) || refs.some((r) => r.kind === "clue" && (r.id === "F-052" || r.id === "F-058" || r.id === "F-064"));
  const wantsPlan = PLAN_PATTERN.test(text) || refs.some((r) => r.kind === "plan" || r.kind === "change");
  const wantsReport = REPORT_PATTERN.test(text) || refs.some((r) => r.kind === "report" || r.kind === "feedback");
  const wantsConflict = CONFLICT_PATTERN.test(text) || refs.some((r) => r.kind === "source");
  const wantsRetest = RETEST_PATTERN.test(text) || refs.some((r) => r.kind === "feedback");

  if (wantsRecategorize) {
    const target = categoryInText(text);
    clues.forEach((clue) => {
      const to = recategorizeTarget(clue, target);
      if (to) items.push({ id: `${prefix}:cat:${clue.id}`, kind: "recategorize", title: clue.title, clueId: clue.id, from: clue.category, to });
    });
    const moved = items.filter((item) => item.kind === "recategorize").length;
    const scope = `${refs.length ? "本条消息带上的" : "当前项目中"} ${clues.length} 条发现`;
    scopes.recategorize = `范围：${scope}，其中 ${moved} 条需要改动分类`;
    if (!moved && refs.length && !clues.length) {
      empty.recategorize = "本条消息带上的对象里还没有已记录的发现，没有可以归类的内容。";
    } else if (!moved) {
      const checked = refs.length ? scope : "当前项目中的全部发现";
      empty.recategorize = target && target !== "timeline"
        ? `按本地规则检查了${checked}，相关发现已经是「${categoryLabel(target)}」，没有需要变更的分类。`
        : `按本地规则检查了${checked}，没有需要变更的分类。本地规则只会把含日期或时间的原文建议归入「${categoryLabel("timeline")}」；要改到其他分类，请在消息里写明目标分类（例如“归类为${categoryLabel(CLUE_CATEGORIES.find((item) => item.value !== "timeline")?.value ?? "detail")}”），或在发现工作区勾选后用「归为」批量修改。`;
    }
  }

  if (wantsFindings) {
    const passages = passagesFor(current, refs, clues);
    for (const { source, passage } of passages) {
      const known = current.clues.filter((clue) => clue.passageId === passage.id).map((clue) => clue.quote);
      for (const sentence of sentences(passage.text)) {
        if (items.filter((item) => item.kind === "finding").length >= FINDING_SUGGESTION_LIMIT) break;
        if (known.some((quote) => quote.includes(sentence) || sentence.includes(quote))) continue;
        items.push({
          id: `${prefix}:find:${passage.id}:${items.length}`,
          kind: "finding",
          title: sentence.replace(/[，。“”！？；]/g, " ").trim().slice(0, 18),
          sourceId: source.id,
          passageId: passage.id,
          quote: sentence,
          category: TIME_PATTERN.test(sentence) ? "timeline" : "detail",
        });
      }
    }
    const count = items.filter((item) => item.kind === "finding").length;
    scopes.finding = `范围：${passages.length} 个段落中还没有记录为发现的句子，列出前 ${count} 句`;
    if (!count) {
      empty.finding = passages.length
        ? `涉及的 ${passages.length} 个段落里，可提取的句子都已经记录为发现，没有新的建议。`
        : "这条消息没有带上原文段落或资产，本地规则无法从中提取发现；可以先在资产库打开原文，或带上相关段落再发送。";
    }
  }

  // AI Merge suggestions
  if (wantsMerge) {
    const dupGroups = findClueDuplicates(current.clues, current.merges, current.dismissedDuplicates, current.undeterminedDuplicates);
    const aiMerges = buildAiMergeSuggestions(dupGroups, current.sources, current.clues);
    const relevantMerges = clues.length
      ? aiMerges.filter((m) => [m.primaryId, ...m.memberIds].some((id) => clues.some((c) => c.id === id)))
      : aiMerges.slice(0, 3);
    relevantMerges.slice(0, 3).forEach((m) => {
      items.push({
        id: `${prefix}:merge:${m.groupKey}`,
        kind: "merge",
        title: `归并同源发现：保留 ${m.primaryId}，并入 ${m.memberIds.join("、")}`,
        rationale: m.rationale,
        mergeKind: "clue",
        primaryId: m.primaryId,
        memberIds: m.memberIds,
      });
    });
    if (relevantMerges.length) {
      scopes.merge = `范围：识别出 ${relevantMerges.length} 组同根因候选，支持逐项采纳归并`;
    }
  }

  // Conflict resolution suggestion when referencing an asset with unresolved conflicts
  if (wantsConflict) {
    const refSourceIds = expandRefs(current, refs).filter((r) => r.kind === "source").map((r) => r.id);
    const targetSources = refSourceIds.length
      ? current.sources.filter((s) => refSourceIds.includes(s.id))
      : current.sources.slice(0, 5);
    for (const src of targetSources) {
      for (const conf of src.conflicts ?? []) {
        if (conf.resolvedBy !== "ai" && items.filter((i) => i.kind === "conflict_resolve").length < 2) {
          items.push({
            id: `${prefix}:conf:${src.id}:${conf.id}`,
            kind: "conflict_resolve",
            title: `裁决资产 ${src.id} 字段冲突「${conf.fieldLabel}」`,
            rationale: `采用最新带身份观测值「${conf.candidates[0]?.value ?? conf.selectedValue}」（来源：${conf.candidates[0]?.source ?? "IMPORT-046"}）`,
            sourceId: src.id,
            conflictId: conf.id,
            conflictValue: conf.candidates[0]?.value ?? conf.selectedValue,
          });
        }
      }
    }
  }

  // Report draft / update suggestion when referencing findings or feedback
  if (wantsReport && clues.length) {
    const targetClue = clues[0];
    items.push({
      id: `${prefix}:report:${targetClue.id}`,
      kind: "report_draft",
      title: `为 ${targetClue.id} 生成/更新交付报告草稿（关联最新对照证据）`,
      rationale: `汇总 ${targetClue.id} 的主张表、原值 PoC 与证据快照，同步至「报告与交付」工作区`,
      clueId: targetClue.id,
    });
    scopes.report_draft = `范围：基于发现 ${targetClue.id} 及上下文证据生成可编辑报告版本`;
  }

  // Plan update suggestion when referencing changes, plans, or feedback
  if (wantsPlan) {
    items.push({
      id: `${prefix}:plan:next`,
      kind: "plan_update",
      title: `修订长期任务计划（基于当前对话上下文生成新计划快照）`,
      rationale: `将本次讨论识别的材料缺口、变化记录与下一步验证动作写入新的计划版本`,
      planFocusQuestion: text.trim().slice(0, 60) || "推进高风险发现补证与跨租户边界核验",
      planJudgement: `已结合 ${refs.length || clues.length} 个关联对象更新研究判断与待办优先级。`,
    });
  }

  // Retest suggestion when referencing feedback or retest keywords
  if (wantsRetest && clues.length) {
    const targetClue = clues[0];
    items.push({
      id: `${prefix}:retest:${targetClue.id}`,
      kind: "retest_record",
      title: `登记 ${targetClue.id} 后续复核记录（snapshot-v47 对照核验）`,
      rationale: `记录前置条件核验、正常业务对照与复测结论，写回报告与发现档案`,
      clueId: targetClue.id,
      retestVerdict: targetClue.id === "F-001" ? "fixed" : "still_vulnerable",
    });
  }

  const wantsTasks = TASK_PATTERN.test(text);
  if ((!wantsRecategorize && !wantsFindings) || wantsTasks) {
    let open = 0;
    clues.forEach((clue) => {
      if (clue.status === "verified") return;
      open += 1;
      const tracked = current.tasks.some((task) => task.links.some((link) => link.kind === "clue" && link.id === clue.id));
      if (!tracked && items.filter((i) => i.kind === "task").length < 4) {
        items.push({ id: `${prefix}:task:${clue.id}`, kind: "task", title: `核实发现：${clue.title}`, clueId: clue.id });
      }
    });
    const proposed = items.filter((item) => item.kind === "task").length;
    if (proposed) scopes.task = `范围：引用的 ${clues.length} 条发现中有 ${open} 条未核实，其中 ${proposed} 条还没有任务`;
    else if (wantsTasks) {
      empty.task = !clues.length
        ? `${refs.length ? "本条消息带上的对象里" : "当前项目里"}还没有已记录的发现，没有需要建任务核实的内容。`
        : open
          ? `引用的 ${clues.length} 条发现中有 ${open} 条未核实，但都已有研究任务跟进，不需要新建任务。`
          : `引用的 ${clues.length} 条发现都已核实，不需要新建核实任务。`;
    }
  }
  return { items, scopes, empty };
}

function initialState(): StudioState {
  const data = buildDemoData();
  const demoBatch: ImportBatch = {
    id: "IMPORT-046",
    projectId: STUDIO_DEFAULT_PROJECT_ID,
    fileName: "asset-snapshot-v46.csv",
    format: "csv",
    size: 84320,
    importedAt: "2026-10-27T10:42:00+08:00",
    text: [
      "asset_id,canonical_key,asset_type,owner,environment,title,body",
      "A-0001,subdomain-1.example.invalid,subdomain,演示资料组,offline-demo,A-0001 · subdomain-1.example.invalid,子域名 1 · snapshot-v46。离线导入的虚构资产。",
      "A-0887,api-887.example.invalid,api,核心平台网关组,offline-demo,A-0887 · api-887.example.invalid,API 服务 887 · 承载 /api/v2/tenant/export 多租户导出端点。",
      "A-0890,api-890.example.invalid,api,核心平台网关组,offline-demo,A-0890 · api-890.example.invalid,API 服务 890 · 与 A-0887 共享 GW-CLUSTER-NORTH-01 网关配置。",
      "A-1284,cdn-1284.example.invalid,cdn,演示资料组,offline-demo,A-1284 · cdn-1284.example.invalid,第 46 版快照新增节点，转发兼容导出路径。",
      "A-ERR-1,,unknown_type,演示资料组,offline-demo,,这一条故意留空标题与未识别类型，用于演示逐条失败修正与重试",
    ].join("\n"),
    settings: {
      format: "csv",
      fileName: "asset-snapshot-v46.csv",
      delimiter: ",",
      header: true,
      txtMode: "whole",
      encoding: "utf-8",
      combineMode: "per_row",
      mapping: {
        key: "asset_id",
        title: "title",
        body: "body",
        kind: "asset_type",
        author: "owner",
        collectedAt: "",
      },
      defaultKind: "field",
      defaultAuthor: "演示资料组",
      conflict: "update",
    },
    items: [
      {
        id: "IMPORT-046-r1",
        index: 1,
        location: { label: "第 2 行", line: 2, endLine: 2 },
        raw: "A-0001,subdomain-1.example.invalid,subdomain,演示资料组,offline-demo,A-0001 · subdomain-1.example.invalid,子域名 1 · snapshot-v46。",
        values: { key: "A-0001", title: "A-0001 · subdomain-1.example.invalid", body: "子域名 1 · snapshot-v46。", kind: "subdomain", author: "演示资料组", collectedAt: "2026-10-27" },
        parseError: "",
        outcome: "updated",
        reason: "按唯一标识「A-0001」匹配到已有资产，更新了正文与 v46 观测记录。",
        sourceId: "A-0001",
        attempts: 1,
      },
      {
        id: "IMPORT-046-r2",
        index: 2,
        location: { label: "第 3 行", line: 3, endLine: 3 },
        raw: "A-0887,api-887.example.invalid,api,核心平台网关组,offline-demo,A-0887 · api-887.example.invalid,API 服务 887 · 承载 /api/v2/tenant/export 多租户导出端点。",
        values: { key: "A-0887", title: "A-0887 · api-887.example.invalid", body: "API 服务 887 · 承载 /api/v2/tenant/export 多租户导出端点。", kind: "api", author: "核心平台网关组", collectedAt: "2026-10-27" },
        parseError: "",
        outcome: "updated",
        reason: "按唯一标识「A-0887」匹配到已有资产，更新了 v46 网关路由描述。",
        sourceId: "A-0887",
        attempts: 1,
      },
      {
        id: "IMPORT-046-r3",
        index: 3,
        location: { label: "第 4 行", line: 4, endLine: 4 },
        raw: "A-0890,api-890.example.invalid,api,核心平台网关组,offline-demo,A-0890 · api-890.example.invalid,API 服务 890 · 与 A-0887 共享网关。",
        values: { key: "A-0890", title: "A-0890 · api-890.example.invalid", body: "API 服务 890 · 与 A-0887 共享 GW-CLUSTER-NORTH-01 网关配置。", kind: "api", author: "核心平台网关组", collectedAt: "2026-10-27" },
        parseError: "",
        outcome: "created",
        reason: "新建资产，共 3 段。",
        sourceId: "A-0890",
        attempts: 1,
      },
      {
        id: "IMPORT-046-r4",
        index: 4,
        location: { label: "第 5 行", line: 5, endLine: 5 },
        raw: "A-1284,cdn-1284.example.invalid,cdn,演示资料组,offline-demo,A-1284 · cdn-1284.example.invalid,第 46 版快照新增节点。",
        values: { key: "A-1284", title: "A-1284 · cdn-1284.example.invalid", body: "第 46 版快照新增节点，转发兼容导出路径。", kind: "cdn", author: "演示资料组", collectedAt: "2026-10-27" },
        parseError: "",
        outcome: "created",
        reason: "新建资产，共 3 段。",
        sourceId: "A-1284",
        attempts: 1,
      },
      {
        id: "IMPORT-046-r5",
        index: 5,
        location: { label: "第 6 行", line: 6, endLine: 6 },
        raw: "A-ERR-1,,unknown_type,演示资料组,offline-demo,,这一条故意留空标题与未识别类型",
        values: { key: "A-ERR-1", title: "", body: "这一条故意留空标题与未识别类型，用于演示逐条失败修正与重试", kind: "unknown_type", author: "演示资料组", collectedAt: "" },
        parseError: "",
        outcome: "failed",
        reason: "标题为空：这条记录的「title」没有内容；且类型「unknown_type」无法识别。",
        attempts: 1,
      },
    ],
    connectorMeta: {
      connectorId: "conn-scopesentry",
      sourceSystem: "CSV 快照 + ScopeSentry 增量同步",
      query: "project=\"baidu-src-demo\" && snapshot=\"v46\"",
      completeness: "complete",
      costCredits: 10,
      receiptId: "RCPT-IMP-046",
    },
  };

  const base: StudioState = {
    ...data,
    hostTag: "demo",
    threads: {},
    threadOrder: [],
    context: {},
    findings: {},
    turnFindings: {},
    turnRefs: {},
    suggestions: {},
    suggestionScopes: {},
    suggestionEmpty: {},
    turnTasks: {},
    handoff: {},
    taskRuns: {},
    activeThreadId: DEFAULT_THREAD_ID,
    viewing: null,
    viewingExcluded: false,
    selectionExcluded: false,
    recent: [],
    updated: [],
    imports: [demoBatch],
    importOrigins: {
      "A-0001": { batchId: "IMPORT-046", key: "A-0001" },
      "A-0887": { batchId: "IMPORT-046", key: "A-0887" },
      "A-0890": { batchId: "IMPORT-046", key: "A-0890" },
      "A-1284": { batchId: "IMPORT-046", key: "A-1284" },
    },
    merges: [],
    history: [
      {
        id: "h-seed-1",
        at: "2026-10-27T10:45:00+08:00",
        action: "report",
        summary: "基于平台反馈 FB-001 创建报告修订草稿「R-052 (v2)」，关联对照证据 EX-019 与截图 IMG-003",
        scope: "不自动继承 R-052-v1 的平台提交与奖励状态",
        refs: [
          { kind: "report", id: "R-052" },
          { kind: "feedback", id: "FB-001" },
          { kind: "evidence", id: "EX-019" },
          { kind: "clue", id: "F-052" },
        ],
      },
      {
        id: "h-seed-2",
        at: "2026-10-27T10:42:00+08:00",
        action: "import",
        summary: "导入《asset-snapshot-v46.csv》：新增 2 条，更新 2 条，失败 1 条（可修正重试或整批撤销）",
        refs: [
          { kind: "import", id: "IMPORT-046" },
          { kind: "source", id: "A-0887" },
          { kind: "source", id: "A-1284" },
        ],
        batchId: "IMPORT-046",
      },
      {
        id: "h-seed-3",
        at: "2026-10-27T10:20:00+08:00",
        action: "retest",
        summary: "完成发现 F-001 修复复测（RETEST-001）：在 snapshot-v47 下确认为「已解决 (Fixed)」",
        refs: [
          { kind: "clue", id: "F-001" },
          { kind: "report", id: "R-001" },
          { kind: "feedback", id: "FB-002" },
        ],
      },
    ],
    savedFilters: [
      {
        id: "sf-core-conflicts",
        scope: "library",
        name: "核心资产且有待裁决冲突",
        createdAt: "2026-10-27T10:00:00+08:00",
        filter: { ...EMPTY_SOURCE_FILTER, importance: "core", conflict: "with" },
      },
      {
        id: "sf-green-critical",
        scope: "findings",
        name: "严重等级 · 可直接交付 (GREEN)",
        createdAt: "2026-10-27T10:05:00+08:00",
        filter: { ...EMPTY_CLUE_FILTER, severity: "critical", readiness: "GREEN" },
      },
      {
        id: "sf-stale-findings",
        scope: "findings",
        name: "资料有更新需复核 (陈旧发现)",
        createdAt: "2026-10-27T10:10:00+08:00",
        filter: { ...EMPTY_CLUE_FILTER, stale: "stale" },
      },
    ],
    dismissedDuplicates: [],
    undeterminedDuplicates: {
      [duplicateKey("clue", ["F-089", "F-095"])]: {
        key: duplicateKey("clue", ["F-089", "F-095"]),
        kind: "clue",
        projectId: STUDIO_DEFAULT_PROJECT_ID,
        ids: ["F-089", "F-095"],
        reason: "两处发现均涉及 A-0892 的会话超时配置，但一处位于 Web 端 Cookie 刷新，另一处位于移动端 Token 续期",
        missingNote: "需补充后端网关会话存储 Redis Key 前缀对照，确认是否共用同一套 Session 管理器后再决定归并或独立。",
        at: "2026-10-27T10:30:00+08:00",
        by: "人工复核组 · 林工",
      },
    },
    runtimeMode: "local_rules",
    serverSync: {
      lastSyncedAt: "2026-10-27T10:42:00+08:00",
      syncRev: 46,
      deviceName: "MacBook Pro · 离线研究工作台",
    },
    workspace: {
      projectId: STUDIO_DEFAULT_PROJECT_ID,
      clueFilter: EMPTY_CLUE_FILTER,
      library: EMPTY_LIBRARY_UI,
      findings: EMPTY_FINDINGS_UI,
      tasks: EMPTY_TASKS_UI,
      reports: EMPTY_REPORTS_UI,
    },
  };

  for (const seed of STUDIO_THREAD_SEEDS) {
    const createdAt = seed.view.thread.created_at ?? "2026-10-27T10:42:00+08:00";
    base.threads[seed.id] = {
      ...emptyThread(seed.id, seed.projectId, seed.view.thread.title, createdAt),
      messages: seed.view.messages,
      turns: seed.view.turns,
      seq: seed.view.watermark,
      updatedAt: seed.view.thread.updated_at ?? createdAt,
    };
    base.threadOrder.push(seed.id);
    base.context[seed.id] = seed.context;
    base.findings[seed.id] = seed.findings;
    const lastReply = [...seed.view.messages].reverse().find((message) => message.role === "assistant" && message.turn_id);
    if (lastReply?.turn_id && seed.findings.length) {
      const key = turnKey(seed.id, lastReply.turn_id);
      const clues = seed.findings.flatMap((id) => data.clues.filter((clue) => clue.id === id));
      const built = buildSuggestions(base, "归并 报告 冲突", clues, seed.context, key);
      base.turnFindings[key] = seed.findings;
      base.turnRefs[key] = seed.context;
      base.suggestions[key] = built.items;
      base.suggestionScopes[key] = built.scopes;
    }
  }
  return base;
}

/** A clean workbench bound to a real SRC task: one project, one empty thread, no demo records. */
function emptyTaskState(input: { taskId: string; threadId: string; title: string; goal: string }): StudioState {
  const at = new Date().toISOString();
  const projectId = `task-${input.taskId}`;
  const threadId = input.threadId;
  const base = initialState();
  return {
    ...base,
    hostTag: `task:${input.taskId}`,
    projects: [{ id: projectId, title: input.title || "SRC 研究", summary: input.goal || "", company: "", longTermGoal: input.goal || "" }],
    sources: [],
    clues: [],
    tasks: [],
    planVersions: [],
    planQuestions: [],
    scopeMaterials: [],
    evidences: [],
    workRuns: [],
    reports: [],
    feedbacks: [],
    retests: [],
    changes: [],
    monitors: [],
    scopeSnapshots: [],
    approvals: [],
    grants: [],
    connectors: [],
    threads: { [threadId]: emptyThread(threadId, projectId, input.title || "SRC 研究", at) },
    threadOrder: [threadId],
    context: { [threadId]: [] },
    findings: { [threadId]: [] },
    activeThreadId: threadId,
    imports: [],
    importOrigins: {},
    merges: [],
    history: [],
    savedFilters: [],
    dismissedDuplicates: [],
    undeterminedDuplicates: {},
    serverSync: { lastSyncedAt: "", syncRev: 0, deviceName: "云端工作台" },
    workspace: {
      projectId,
      clueFilter: EMPTY_CLUE_FILTER,
      library: EMPTY_LIBRARY_UI,
      findings: EMPTY_FINDINGS_UI,
      tasks: EMPTY_TASKS_UI,
      reports: EMPTY_REPORTS_UI,
    },
  };
}

let state: StudioState = initialState();
const serverState = state;
const listeners = new Set<() => void>();
let localCounter = 0;

function setState(update: (current: StudioState) => StudioState) {
  const next = update(state);
  if (next === state) return;
  state = next;
  listeners.forEach((listener) => listener());
}

function subscribe(listener: () => void) {
  listeners.add(listener);
  return () => { listeners.delete(listener); };
}

export function useStudioState(): StudioState {
  return useSyncExternalStore(subscribe, () => state, () => serverState);
}

export function readStudioState(): StudioState {
  return state;
}

export function subscribeStudio(listener: () => void): () => void {
  return subscribe(listener);
}

let sessionTag = "";

function nextLocalId(prefix: string): string {
  if (!sessionTag) sessionTag = Math.random().toString(36).slice(2, 7) || "x";
  localCounter += 1;
  return `${prefix}-${sessionTag}-${localCounter}`;
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

/** The part of the state saved in the browser; the rest lasts one page session. */
export type PersistedState = Pick<
  StudioState,
  | "projects" | "sources" | "clues" | "tasks"
  | "planVersions" | "planQuestions" | "scopeMaterials"
  | "evidences" | "workRuns" | "reports" | "feedbacks" | "retests"
  | "changes" | "monitors" | "scopeSnapshots" | "approvals" | "grants" | "environmentStatus" | "connectors"
  | "threads" | "threadOrder" | "activeThreadId"
  | "context" | "findings" | "turnFindings" | "turnRefs" | "suggestions" | "suggestionScopes" | "suggestionEmpty" | "turnTasks" | "handoff" | "taskRuns"
  | "imports" | "importOrigins"
  | "merges" | "history" | "savedFilters" | "dismissedDuplicates" | "undeterminedDuplicates"
  | "runtimeMode" | "serverSync"
  | "workspace"
>;

function savedField<K extends keyof PersistedState>(saved: Partial<PersistedState>, current: StudioState, key: K): PersistedState[K] {
  const value = saved[key];
  if (value === undefined || value === null) return current[key];
  const kind = (item: unknown) => (Array.isArray(item) ? "array" : typeof item);
  if (kind(value) !== kind(current[key])) throw new Error(`保存的数据里「${String(key)}」的格式不对。`);
  return value as PersistedState[K];
}

function settleInterrupted(record: StudioThreadRecord, at: string): StudioThreadRecord {
  const running = record.turns.filter((turn) => turn.status === "running");
  if (!running.length) return record;
  let seq = record.seq;
  const messages = [...record.messages];
  running.forEach((turn) => {
    const id = replyMessageId(turn.turn_id);
    if (messages.some((message) => message.message_id === id)) return;
    seq += 1;
    messages.push({ message_id: id, turn_id: turn.turn_id, role: "assistant", kind: "message", text: INTERRUPTED_REPLY, stream_seq: seq, created_at: at });
  });
  const stalePlan = record.plan && running.some((turn) => turn.turn_id === record.plan?.turn_id);
  return {
    ...record,
    messages,
    seq,
    plan: stalePlan ? null : record.plan,
    turns: record.turns.map((turn) => (turn.status === "running" ? { ...turn, status: "completed", completed_at: at } : turn)),
  };
}

export function restoreStudioState(saved: Partial<PersistedState>, options: { initial: boolean }) {
  setState((current) => {
    const get = <K extends keyof PersistedState>(key: K) => savedField(saved, current, key);
    const at = new Date().toISOString();
    const savedProjects = get("projects");
    const projectIds = new Set(savedProjects.map((project) => project.id));
    const projects = [...savedProjects, ...current.projects.filter((project) => !projectIds.has(project.id))];
    const savedThreads = get("threads");
    const threads: Record<string, StudioThreadRecord> = { ...current.threads };
    Object.entries(savedThreads).forEach(([id, record]) => {
      threads[id] = options.initial ? settleInterrupted(record, at) : record;
    });
    const savedOrder = get("threadOrder").filter((id) => threads[id]);
    const threadOrder = [...savedOrder, ...Object.keys(threads).filter((id) => !savedOrder.includes(id))];
    const taskRuns = get("taskRuns");
    const workspace = get("workspace");
    const nextWorkspace = options.initial && saved.workspace ? {
      projectId: workspace.projectId === "all" || projects.some((project) => project.id === workspace.projectId) ? workspace.projectId : current.workspace.projectId,
      clueFilter: { ...EMPTY_CLUE_FILTER, ...workspace.clueFilter },
      library: { ...EMPTY_LIBRARY_UI, ...workspace.library },
      findings: { ...EMPTY_FINDINGS_UI, ...workspace.findings, focusIds: null, focusLabel: "" },
      tasks: { ...EMPTY_TASKS_UI, ...workspace.tasks },
      reports: { ...EMPTY_REPORTS_UI, ...(workspace.reports ?? {}) },
    } : current.workspace;
    const activeThreadId = options.initial ? get("activeThreadId") : current.activeThreadId;
    return {
      ...current,
      projects,
      sources: get("sources"),
      clues: get("clues"),
      tasks: get("tasks"),
      planVersions: get("planVersions"),
      planQuestions: get("planQuestions"),
      scopeMaterials: get("scopeMaterials"),
      evidences: get("evidences"),
      workRuns: get("workRuns"),
      reports: get("reports"),
      feedbacks: get("feedbacks"),
      retests: get("retests"),
      changes: get("changes"),
      monitors: get("monitors"),
      scopeSnapshots: get("scopeSnapshots"),
      approvals: get("approvals"),
      grants: get("grants"),
      environmentStatus: get("environmentStatus"),
      connectors: get("connectors"),
      threads,
      threadOrder,
      activeThreadId: threads[activeThreadId] ? activeThreadId : current.activeThreadId,
      context: get("context"),
      findings: get("findings"),
      turnFindings: get("turnFindings"),
      turnRefs: get("turnRefs"),
      suggestions: get("suggestions"),
      suggestionScopes: get("suggestionScopes"),
      suggestionEmpty: get("suggestionEmpty"),
      turnTasks: get("turnTasks"),
      handoff: get("handoff"),
      taskRuns: options.initial ? Object.fromEntries(Object.entries(taskRuns).filter(([, run]) => run.done)) : taskRuns,
      imports: get("imports"),
      importOrigins: get("importOrigins"),
      merges: get("merges"),
      history: get("history"),
      savedFilters: get("savedFilters"),
      dismissedDuplicates: get("dismissedDuplicates"),
      undeterminedDuplicates: get("undeterminedDuplicates"),
      runtimeMode: get("runtimeMode"),
      serverSync: get("serverSync"),
      workspace: nextWorkspace,
      viewing: options.initial ? null : current.viewing,
    };
  });
}

// ---------------------------------------------------------------------------
// History & Undo
// ---------------------------------------------------------------------------

function withHistory(current: StudioState, entry: Omit<HistoryEntry, "id" | "at"> & { id?: string }): StudioState {
  const record: HistoryEntry = { ...entry, id: entry.id ?? nextLocalId("h"), at: new Date().toISOString() };
  // Keep full processing history without truncating at 2,000 entries.
  return { ...current, history: [record, ...current.history] };
}

function recordTitle(current: Pick<StudioState, "sources" | "clues">, kind: RecordKind, id: string): string {
  if (kind === "source") return current.sources.find((source) => source.id === id)?.title ?? "已移除的资产";
  return current.clues.find((clue) => clue.id === id)?.title ?? "已移除的发现";
}

function clueField(clue: StudioClue, field: FieldChange["field"]): string | undefined {
  if (field === "status") return clue.status;
  if (field === "category") return clue.category;
  if (field === "severity") return clue.severity ?? "high";
  if (field === "readiness") return clue.readiness ?? "YELLOW";
  return undefined;
}

function sourceField(source: StudioSource, field: FieldChange["field"]): string | undefined {
  if (field === "status") return source.status;
  if (field === "kind") return source.kind;
  if (field === "importance") return source.importance ?? "normal";
  if (field === "archived") return String(Boolean(source.archived));
  return undefined;
}

function plannedChanges(current: StudioState, kind: RecordKind, ids: string[], field: FieldChange["field"], value: string): FieldChange[] {
  const wanted = new Set(ids);
  const out: FieldChange[] = [];
  const add = (id: string, from: string | undefined) => {
    if (from !== undefined && from !== value) out.push({ kind, id, field, from, to: value });
  };
  if (kind === "clue") current.clues.forEach((clue) => { if (wanted.has(clue.id)) add(clue.id, clueField(clue, field)); });
  else current.sources.forEach((source) => { if (wanted.has(source.id)) add(source.id, sourceField(source, field)); });
  return out;
}

function stillApplied(current: StudioState, changes: FieldChange[]): FieldChange[] {
  const clues = new Map(current.clues.map((clue) => [clue.id, clue]));
  const sources = new Map(current.sources.map((source) => [source.id, source]));
  return changes.filter((change) => {
    const value = change.kind === "clue"
      ? (clues.get(change.id) ? clueField(clues.get(change.id) as StudioClue, change.field) : undefined)
      : (sources.get(change.id) ? sourceField(sources.get(change.id) as StudioSource, change.field) : undefined);
    return value === change.to;
  });
}

function applyChanges(current: StudioState, changes: FieldChange[], pick: "from" | "to"): StudioState {
  const byId = (kind: RecordKind) => {
    const map = new Map<string, FieldChange[]>();
    changes.forEach((change) => {
      if (change.kind !== kind) return;
      map.set(change.id, [...(map.get(change.id) ?? []), change]);
    });
    return map;
  };
  const clueChanges = byId("clue");
  const sourceChanges = byId("source");
  return {
    ...current,
    clues: clueChanges.size ? current.clues.map((clue) => (clueChanges.get(clue.id) ?? []).reduce((next, change): StudioClue => (
      change.field === "status" ? { ...next, status: change[pick] as ClueStatus }
        : change.field === "category" ? { ...next, category: change[pick] as ClueCategory }
          : change.field === "severity" ? { ...next, severity: change[pick] as FindingSeverity }
            : change.field === "readiness" ? { ...next, readiness: change[pick] as ReadinessLevel }
              : next
    ), clue)) : current.clues,
    sources: sourceChanges.size ? current.sources.map((source) => (sourceChanges.get(source.id) ?? []).reduce((next, change): StudioSource => (
      change.field === "status" ? { ...next, status: change[pick] as StudioSource["status"] }
        : change.field === "kind" ? { ...next, kind: change[pick] as SourceKind }
          : change.field === "importance" ? { ...next, importance: change[pick] as AssetImportance }
            : change.field === "archived" ? { ...next, archived: change[pick] === "true" }
              : next
    ), source)) : current.sources,
  };
}

function changeSummary(current: StudioState, kind: RecordKind, field: FieldChange["field"], value: string, changes: FieldChange[]): string {
  const verb = kind === "clue"
    ? field === "status"
      ? `标为「${statusLabel(value as ClueStatus)}」`
      : field === "category"
        ? `归为「${categoryLabel(value as ClueCategory)}」`
        : field === "severity"
          ? `等级改为「${severityLabel(value as FindingSeverity)}」`
          : `准备度改为「${readinessLabel(value as ReadinessLevel)}」`
    : field === "status"
      ? `移到「${sourceStatusLabel(value as StudioSource["status"])}」`
      : field === "kind"
        ? `类型改为「${sourceKindLabel(value as SourceKind)}」`
        : field === "archived"
          ? (value === "true" ? "已归档" : "已恢复归档")
          : `重要性改为「${value}」`;
  const subject = changes.length === 1 ? `${RECORD_NOUN[kind].noun}「${recordTitle(current, kind, changes[0].id)}」` : countLabel(kind, changes.length);
  return `${subject}${verb}`;
}

function setRecordField(kind: RecordKind, ids: string | string[], field: FieldChange["field"], value: string, scope?: string): string | null {
  let entryId: string | null = null;
  setState((current) => {
    const changes = plannedChanges(current, kind, Array.isArray(ids) ? ids : [ids], field, value);
    if (!changes.length) return current;
    const id = nextLocalId("h");
    entryId = id;
    return withHistory(applyChanges(current, changes, "to"), {
      id,
      action: field === "status" ? "status" : field === "archived" ? "archive" : "category",
      summary: changeSummary(current, kind, field, value, changes),
      scope,
      refs: changes.map((change) => ({ kind, id: change.id })),
      changes,
    });
  });
  return entryId;
}

function undoMergeIn(current: StudioState, mergeId: string): { state: StudioState; entryId: string } | null {
  const merge = current.merges.find((item) => item.id === mergeId && !item.undoneAt);
  if (!merge) return null;
  const entryId = nextLocalId("h");
  const at = new Date().toISOString();
  const next: StudioState = {
    ...current,
    merges: current.merges.map((item) => (item.id === mergeId ? { ...item, undoneAt: at } : item)),
    history: current.history.map((entry) => (entry.action === "merge" && entry.mergeId === mergeId && !entry.revertedBy ? { ...entry, revertedBy: entryId } : entry)),
  };
  return {
    entryId,
    state: withHistory(next, {
      id: entryId,
      action: "unmerge",
      summary: `撤销归并：${countLabel(merge.kind, merge.memberIds.length)}从「${recordTitle(current, merge.kind, merge.primaryId)}」中拆出，重新单独列出`,
      scope: `当初的归并原因：${merge.reason}`,
      refs: [merge.primaryId, ...merge.memberIds].map((id) => ({ kind: merge.kind, id })),
      mergeId,
    }),
  };
}

function undoImportBatchIn(current: StudioState, batchId: string): { state: StudioState; restoredCount: number } | null {
  const batch = current.imports.find((item) => item.id === batchId && !item.undoneAt);
  if (!batch) return null;
  const entryId = nextLocalId("h");
  const at = new Date().toISOString();
  const prevSnapshots = batch.previousSources ?? [];
  const createdIds = new Set<string>();
  const restoreMap = new Map<string, StudioSource>();
  const originRestore = { ...current.importOrigins };

  if (prevSnapshots.length) {
    prevSnapshots.forEach((snap) => {
      if (!snap.existed) {
        createdIds.add(snap.id);
        delete originRestore[snap.id];
      } else if (snap.previous) {
        restoreMap.set(snap.id, snap.previous);
        if (snap.previousOrigin) originRestore[snap.id] = snap.previousOrigin;
        else delete originRestore[snap.id];
      }
    });
  } else {
    batch.items.forEach((item) => {
      if (item.outcome === "created" && item.sourceId) {
        createdIds.add(item.sourceId);
        delete originRestore[item.sourceId];
      }
    });
  }

  const nextSources = current.sources
    .filter((s) => !createdIds.has(s.id))
    .map((s) => restoreMap.get(s.id) ?? s);

  const restoredCount = createdIds.size + restoreMap.size || batch.items.filter((i) => i.outcome === "created" || i.outcome === "updated").length;
  const next: StudioState = {
    ...current,
    sources: nextSources,
    importOrigins: originRestore,
    imports: current.imports.map((b) => (b.id === batchId ? { ...b, undoneAt: at } : b)),
    history: current.history.map((entry) => (entry.action === "import" && entry.batchId === batchId && !entry.revertedBy ? { ...entry, revertedBy: entryId } : entry)),
  };
  return {
    restoredCount,
    state: withHistory(next, {
      id: entryId,
      action: "revert",
      summary: `整批撤销导入《${batch.fileName}》：撤回 ${restoredCount} 份新增或更新的资产`,
      refs: [{ kind: "import", id: batchId }],
      batchId,
    }),
  };
}

/** Whether "撤销" can still undo the entry. Supports create, import, task, delete, tag, merge, dismiss, undetermined, and field changes! */
export function canRevert(current: StudioState, entry: HistoryEntry): boolean {
  if (entry.revertedBy) return false;
  if (entry.action === "merge") return Boolean(entry.mergeId && current.merges.some((merge) => merge.id === entry.mergeId && !merge.undoneAt));
  if (entry.action === "import" && entry.batchId) return Boolean(current.imports.some((b) => b.id === entry.batchId && !b.undoneAt));
  if (entry.dismissKey) return current.dismissedDuplicates.includes(entry.dismissKey);
  if (entry.undeterminedKey) return Boolean(current.undeterminedDuplicates[entry.undeterminedKey]);
  if (entry.createdRefs?.length) {
    return entry.createdRefs.some((ref) => refExists(current, ref));
  }
  if (entry.deletedSnapshot) {
    return Boolean(
      (entry.deletedSnapshot.sources?.length ?? 0) +
      (entry.deletedSnapshot.clues?.length ?? 0) +
      (entry.deletedSnapshot.tasks?.length ?? 0) > 0
    );
  }
  if (entry.tagChanges?.length) return true;
  return Boolean(entry.changes?.length);
}

/** Tasks, conversations, reports and replies that point at `ref`; merges keep all of them. */
export function refUsage(current: StudioState, ref: StudioRef): { tasks: number; threads: number; replies: number; reports: number } {
  return {
    tasks: current.tasks.filter((task) => task.links.some((link) => sameRef(link, ref))).length,
    threads: Object.values(current.context).filter((list) => list.some((item) => sameRef(item, ref))).length,
    replies: ref.kind === "clue" ? Object.values(current.turnFindings).filter((ids) => ids.includes(ref.id)).length : 0,
    reports: ref.kind === "clue"
      ? current.reports.filter((r) => r.findingId === ref.id).length
      : ref.kind === "evidence"
        ? current.reports.filter((r) => r.evidenceIds.includes(ref.id)).length
        : 0,
  };
}

export function importCountsText(counts: ImportCounts): string {
  const labels: Record<ImportOutcome, string> = { created: "新增", updated: "更新", skipped: "跳过", failed: "失败" };
  const parts = (Object.keys(labels) as ImportOutcome[]).filter((outcome) => counts[outcome]).map((outcome) => `${labels[outcome]} ${counts[outcome]} 条`);
  return parts.length ? parts.join("，") : "没有记录";
}

function updateThread(threadId: string, fn: (record: StudioThreadRecord) => StudioThreadRecord) {
  setState((current) => {
    const record = current.threads[threadId];
    if (!record) return current;
    return { ...current, threads: { ...current.threads, [threadId]: fn(record) } };
  });
}

function withRef(list: StudioRef[], ref: StudioRef): StudioRef[] {
  return [ref, ...list.filter((item) => !sameRef(item, ref))].slice(0, RECENT_LIMIT);
}

// ---------------------------------------------------------------------------
// Lookups (All 11 StudioRefKind objects!)
// ---------------------------------------------------------------------------

export function findSource(current: StudioState, id: string): StudioSource | undefined {
  return current.sources.find((source) => source.id === id);
}

export function findPassage(current: Pick<StudioState, "sources">, id: string) {
  for (const source of current.sources) {
    const passage = source.passages.find((item) => item.id === id);
    if (passage) return { source, passage };
  }
  return undefined;
}

export function refLabel(current: StudioState, ref: StudioRef): string {
  return displayTitle(rawRefLabel(current, ref));
}
function rawRefLabel(current: StudioState, ref: StudioRef): string {
  if (ref.kind === "source") return findSource(current, ref.id)?.title ?? "资产已移除";
  if (ref.kind === "passage") {
    const hit = findPassage(current, ref.id);
    return hit ? `${hit.source.title} · 第 ${hit.passage.index} 段` : "段落已移除";
  }
  if (ref.kind === "clue") return current.clues.find((clue) => clue.id === ref.id)?.title ?? "发现已移除";
  if (ref.kind === "task") return current.tasks.find((task) => task.id === ref.id)?.title ?? "任务已移除";
  if (ref.kind === "report") return current.reports.find((report) => report.id === ref.id)?.title ?? `报告 ${ref.id}`;
  if (ref.kind === "evidence") return current.evidences.find((ev) => ev.id === ref.id)?.title ?? `证据 ${ref.id}`;
  if (ref.kind === "import") {
    const batch = current.imports.find((item) => item.id === ref.id);
    return batch ? `${batch.id} · ${batch.fileName}` : `导入批次 ${ref.id}`;
  }
  if (ref.kind === "run") return current.workRuns.find((run) => run.id === ref.id)?.title ?? `工作记录 ${ref.id}`;
  if (ref.kind === "change") return current.changes.find((chg) => chg.id === ref.id)?.title ?? `变化记录 ${ref.id}`;
  if (ref.kind === "feedback") {
    const fb = current.feedbacks.find((item) => item.id === ref.id);
    return fb ? `${fb.id} · ${fb.externalTicketId} 平台反馈` : `反馈 ${ref.id}`;
  }
  if (ref.kind === "plan") {
    const plan = current.planVersions.find((item) => item.id === ref.id);
    return plan ? `${plan.id} (v${plan.version}) · 任务计划` : `计划 ${ref.id}`;
  }
  return ref.id;
}

export function refExists(current: StudioState, ref: StudioRef): boolean {
  if (ref.kind === "source") return Boolean(findSource(current, ref.id));
  if (ref.kind === "passage") return Boolean(findPassage(current, ref.id));
  if (ref.kind === "clue") return current.clues.some((clue) => clue.id === ref.id);
  if (ref.kind === "task") return current.tasks.some((task) => task.id === ref.id);
  if (ref.kind === "report") return current.reports.some((report) => report.id === ref.id);
  if (ref.kind === "evidence") return current.evidences.some((ev) => ev.id === ref.id);
  if (ref.kind === "import") return current.imports.some((batch) => batch.id === ref.id);
  if (ref.kind === "run") return current.workRuns.some((run) => run.id === ref.id);
  if (ref.kind === "change") return current.changes.some((chg) => chg.id === ref.id);
  if (ref.kind === "feedback") return current.feedbacks.some((fb) => fb.id === ref.id);
  if (ref.kind === "plan") return current.planVersions.some((plan) => plan.id === ref.id);
  return false;
}

/** Where a reference's original text lives: source plus optional passage and clue. */
export function refTarget(current: StudioState, ref: StudioRef): { sourceId: string; passageId?: string; clueId?: string } | null {
  if (ref.kind === "source") return findSource(current, ref.id) ? { sourceId: ref.id } : null;
  if (ref.kind === "passage") {
    const hit = findPassage(current, ref.id);
    return hit ? { sourceId: hit.source.id, passageId: ref.id } : null;
  }
  if (ref.kind === "clue") {
    const clue = current.clues.find((item) => item.id === ref.id);
    return clue ? { sourceId: clue.sourceId, passageId: clue.passageId, clueId: clue.id } : null;
  }
  return null;
}

export function refProjectId(current: StudioState, ref: StudioRef): string | undefined {
  if (ref.kind === "source") return findSource(current, ref.id)?.projectId;
  if (ref.kind === "passage") return findPassage(current, ref.id)?.source.projectId;
  if (ref.kind === "clue") return current.clues.find((clue) => clue.id === ref.id)?.projectId;
  if (ref.kind === "task") return current.tasks.find((task) => task.id === ref.id)?.projectId;
  if (ref.kind === "report") return current.reports.find((r) => r.id === ref.id)?.projectId;
  if (ref.kind === "evidence") return current.evidences.find((e) => e.id === ref.id)?.projectId;
  if (ref.kind === "import") return current.imports.find((b) => b.id === ref.id)?.projectId;
  if (ref.kind === "run") return current.workRuns.find((r) => r.id === ref.id)?.projectId;
  if (ref.kind === "change") return current.changes.find((c) => c.id === ref.id)?.projectId;
  if (ref.kind === "feedback") return current.feedbacks.find((f) => f.id === ref.id)?.projectId;
  if (ref.kind === "plan") return current.planVersions.find((p) => p.id === ref.id)?.projectId;
  return undefined;
}

export function isRecent(current: StudioState, ref: StudioRef): boolean {
  return current.recent.some((item) => sameRef(item, ref));
}

export function isUpdated(current: StudioState, ref: StudioRef): boolean {
  return current.updated.some((item) => sameRef(item, ref));
}

export function contextParts(current: StudioState, threadId: string, visible: VisibleWorkspace) {
  const pinned = current.context[threadId] ?? [];
  const isPinned = (ref: StudioRef) => pinned.some((item) => sameRef(item, ref));
  const handoffId = current.handoff[threadId];
  const handoffTask = handoffId ? current.tasks.find((task) => task.id === handoffId) : undefined;
  const viewing = visible && current.viewing && !isPinned(current.viewing) ? current.viewing : null;
  const checked = (ids: string[], kind: "clue" | "source", items: Array<{ id: string }>) => {
    if (!ids.length) return [];
    const known = new Set(items.map((item) => item.id));
    return ids.filter((id) => known.has(id)).map((id): StudioRef => ({ kind, id })).filter((ref) => !isPinned(ref));
  };
  const selected = visible === "findings"
    ? checked(current.workspace.findings.checked, "clue", current.clues)
    : visible === "library"
      ? checked(current.workspace.library.checked, "source", current.sources)
      : [];
  return {
    pinned,
    handoffTask,
    viewing,
    viewingIncluded: Boolean(viewing && !current.viewingExcluded),
    selected,
    selectionIncluded: selected.length > 0 && !current.selectionExcluded,
  };
}

export function effectiveContext(current: StudioState, threadId: string, visible: VisibleWorkspace): StudioRef[] {
  const parts = contextParts(current, threadId, visible);
  const handoff = parts.handoffTask ? [{ kind: "task" as const, id: parts.handoffTask.id }, ...parts.handoffTask.links] : [];
  return dedupeRefs([
    ...handoff,
    ...parts.pinned,
    ...(parts.viewingIncluded && parts.viewing ? [parts.viewing] : []),
    ...(parts.selectionIncluded ? parts.selected : []),
  ]);
}

// ---------------------------------------------------------------------------
// Local simulation & Agent/MCP response generation
// ---------------------------------------------------------------------------

function relatedClues(current: StudioState, threadId: string, text: string, refs: StudioRef[]): StudioClue[] {
  const record = current.threads[threadId];
  const projectId = record?.projectId ?? "";
  const picked: StudioClue[] = [];
  const add = (clue: StudioClue | undefined) => {
    if (clue && !picked.some((item) => item.id === clue.id)) picked.push(clue);
  };
  for (const ref of expandRefs(current, refs)) {
    if (ref.kind === "clue") add(current.clues.find((clue) => clue.id === ref.id));
    if (ref.kind === "passage") current.clues.filter((clue) => clue.passageId === ref.id).forEach(add);
    if (ref.kind === "source") current.clues.filter((clue) => clue.sourceId === ref.id || clue.affectedSourceIds?.includes(ref.id)).forEach(add);
    if (ref.kind === "evidence") current.clues.filter((clue) => clue.supportingEvidenceIds?.includes(ref.id)).forEach(add);
  }
  if (refs.length && picked.length) return picked;
  const inProject = current.clues.filter((clue) => (clue.projectId === projectId || projectId === "all") && !mergeOf(current.merges, "clue", clue.id));
  if (RECATEGORIZE_PATTERN.test(text)) {
    const target = categoryInText(text);
    const movable = inProject.filter((clue) => recategorizeTarget(clue, target));
    if (movable.length) return movable;
  }
  const scored = inProject
    .map((clue) => {
      let score = 0;
      if (text.includes(clue.id)) score += 6;
      if (text.includes(categoryLabel(clue.category))) score += 2;
      clue.tags.forEach((tag) => { if (text.includes(tag)) score += 2; });
      clue.title.split(/[：“”的\s·]/).filter((part) => part.length >= 2).forEach((part) => { if (text.includes(part)) score += 3; });
      return { clue, score };
    })
    .filter((item) => item.score > 0)
    .sort((a, b) => b.score - a.score);
  if (scored.length) return scored.slice(0, 5).map((item) => item.clue);
  if (refs.length) return picked;
  return inProject.filter((clue) => clue.status !== "verified").slice(0, 4);
}

function composeReply(
  current: StudioState,
  threadId: string,
  text: string,
  clues: StudioClue[],
  refs: StudioRef[],
  built: { items: StudioSuggestion[]; empty: Partial<Record<SuggestionKind, string>> },
  task?: StudioTask,
): string {
  const record = current.threads[threadId];
  const project = current.projects.find((item) => item.id === record?.projectId);
  const projectTitle = project?.title ?? "当前项目";
  const proposals = built.items.length;
  const emptyNotes = (["recategorize", "finding", "task"] as SuggestionKind[]).flatMap((kind) => (built.empty[kind] ? [built.empty[kind] as string] : []));
  const lines: string[] = [];

  if (current.runtimeMode === "simulated_agent_mcp") {
    lines.push(
      `> **MCP / Agent 协同调用轨迹（仿真）**：已核验任务绑定 \`${project?.id ?? "src-demo-baidu"}\` · 策略版本 \`AUTH-002\` · 调用 \`src.asset.query\` / \`src.exchange.compare\` / \`src.finding.deduplications\``,
      "",
    );
  }

  // Summarize non-clue referenced objects (reports, evidences, imports, runs, changes, feedbacks, plans)
  const specialRefs = refs.filter((r) => r.kind !== "clue" && r.kind !== "source" && r.kind !== "passage");
  if (specialRefs.length) {
    lines.push("### 关联对象与来源追溯");
    specialRefs.forEach((ref) => {
      if (ref.kind === "report") {
        const rep = current.reports.find((r) => r.id === ref.id);
        if (rep) lines.push(`- **报告 ${rep.id} (v${rep.version})**：准备度 ${rep.readiness} · 平台状态「${submissionStatusLabel(rep.submission.status)}」${rep.materialGaps.length ? ` · 存在 ${rep.materialGaps.length} 项材料缺口` : " · 证据齐全"}`);
      } else if (ref.kind === "evidence") {
        const ev = current.evidences.find((e) => e.id === ref.id);
        if (ev) lines.push(`- **独立证据 ${ev.id}**：${ev.title} · 完整性 \`${ev.completeness}\` · SHA-256 \`${ev.sha256.slice(0, 16)}…\` (${ev.digestVerified === "verified" ? "已校验" : "摘要不符/已撤回"})`);
      } else if (ref.kind === "import") {
        const imp = current.imports.find((b) => b.id === ref.id);
        if (imp) lines.push(`- **导入批次 ${imp.id}**：《${imp.fileName}》共 ${imp.items.length} 条记录（${importCountsText(countImportOutcomes(imp.items))}）`);
      } else if (ref.kind === "run") {
        const run = current.workRuns.find((r) => r.id === ref.id);
        if (run) lines.push(`- **外部工作记录 ${run.id} (G${run.generation})**：状态 \`${run.state}\`${run.blockerReason ? ` · 受阻原因：${run.blockerReason}` : ""}`);
      } else if (ref.kind === "change") {
        const chg = current.changes.find((c) => c.id === ref.id);
        if (chg) lines.push(`- **变化记录 ${chg.id}**：${chg.title} · 基线 \`${chg.baselineCycleId}\` · ${chg.comparable ? "可比基线" : `不可比（${chg.incomparableReason}）`}`);
      } else if (ref.kind === "feedback") {
        const fb = current.feedbacks.find((f) => f.id === ref.id);
        if (fb) lines.push(`- **平台反馈 ${fb.id} (${fb.externalTicketId})**：「${fb.rawText.slice(0, 72)}…」`);
      } else if (ref.kind === "plan") {
        const pl = current.planVersions.find((p) => p.id === ref.id);
        if (pl) lines.push(`- **任务计划 ${pl.id} (v${pl.version})**：焦点问题「${pl.focusQuestion}」`);
      }
    });
    lines.push("");
  }

  const findingSources = Array.from(new Set(built.items.flatMap((item) => (item.kind === "finding" && item.sourceId ? [item.sourceId] : []))))
    .map((id) => findSource(current, id)?.title)
    .filter((title): title is string => Boolean(title));
  const findingCount = built.items.filter((item) => item.kind === "finding").length;
  const findingLine = findingCount
    ? `正在从所引资料${findingSources.map((title) => `《${title}》`).join("、")}中提取：列出 ${findingCount} 条还没有记录为发现的原文句子作为候选。`
    : "";
  const contextNames = refs.slice(0, 3).map((ref) => `「${refLabel(current, ref)}」`).join("、") + (refs.length > 3 ? ` 等 ${refs.length} 个对象` : "");
  if (task) lines.push(`针对研究任务「${task.title}」，按它关联的 ${task.links.length} 个对象整理：`, "");
  if (!clues.length) {
    lines.push(refs.length
      ? `本条消息带上的${contextNames}里已按对应对象范围完成分析。`
      : `「${projectTitle}」里暂时没有可以匹配的发现。可以先在资产库录入一份资产，再回来继续。`);
    if (findingLine) lines.push("", findingLine);
    if (emptyNotes.length) lines.push("", ...emptyNotes);
    if (proposals) lines.push("", `下方列出 ${proposals} 项建议操作，点击「采纳」后才会修改数据并记入处理记录。`);
    return lines.join("\n");
  }
  if (refs.length) lines.push(`根据本条消息带上的 ${refs.length} 个上下文对象，整理出 ${clues.length} 条相关发现：`);
  else if (text.trim()) lines.push(`在「${projectTitle}」中按你的描述匹配到 ${clues.length} 条发现：`);
  else lines.push(`「${projectTitle}」里这些发现还没有核对完：`);
  lines.push("");
  clues.slice(0, REPLY_SUMMARY_LIMIT).forEach((clue, index) => {
    const hit = findPassage(current, clue.passageId);
    lines.push(
      `${index + 1}. **${clue.title}** · ${categoryLabel(clue.category)} · ${statusLabel(clue.status)} · 准备度 \`${clue.readiness ?? "YELLOW"}\`${clue.stale ? " · ⚠️ 资料有更新" : ""}`,
    );
    lines.push(`   「${clue.quote}」`);
    if (hit) lines.push(`   出处：《${hit.source.title}》第 ${hit.passage.index} 段${clue.supportingEvidenceIds?.length ? ` · 证据：${clue.supportingEvidenceIds.join("、")}` : ""}`);
  });
  if (clues.length > REPLY_SUMMARY_LIMIT) {
    lines.push("", `以上只展开前 ${REPLY_SUMMARY_LIMIT} 条；其余 ${clues.length - REPLY_SUMMARY_LIMIT} 条同样计入本条回复，完整列表见下方「引用的发现」${proposals ? `，建议操作按全部 ${clues.length} 条计算` : ""}。`);
  }
  lines.push("");
  if (task) {
    const open = clues.filter((clue) => clue.status !== "verified").length;
    lines.push(open ? `还缺证据的有 ${open} 条，建议先逐条核对原文出处与对照报文，再把结论记回任务。` : "关联的发现都已核实，可以把任务标为已完成。");
  }
  if (findingLine) lines.push(findingLine, "");
  if (emptyNotes.length) lines.push(...emptyNotes, "");
  lines.push(proposals
    ? `下方列出 ${proposals} 项建议操作（含分类、发现、归并、冲突裁决、计划或报告回写），确认采纳后才会修改数据。`
    : "这次没有生成建议操作，没有修改任何数据。可以在工作区里逐条打开原文继续核对。");
  return lines.join("\n");
}

function schedule(fn: () => void, ms: number) {
  window.setTimeout(fn, ms);
}

function planWithStep(plan: ThreadPlanSnapshot, step: number): ThreadPlanSnapshot {
  return {
    ...plan,
    tasks: plan.tasks.map((task, index) => ({
      ...task,
      status: index < step ? "completed" : index === step ? "in_progress" : "pending",
    })),
    phase: step >= plan.tasks.length ? "completed" : "executing",
    updated_at: new Date().toISOString(),
  };
}

function createTask(
  current: StudioState,
  input: { projectId: string; title: string; assignee: string; links: StudioRef[]; threadId?: string; originKind?: StudioTask["originKind"]; notes?: string },
  id: string,
): StudioState {
  const task: StudioTask = {
    id,
    projectId: input.projectId,
    title: input.title,
    assignee: input.assignee,
    due: "未排期",
    status: "todo",
    links: input.links,
    threadId: input.threadId,
    originKind: input.originKind ?? "manual",
    notes: input.notes,
  };
  return { ...current, tasks: [task, ...current.tasks], recent: withRef(current.recent, { kind: "task", id }) };
}

function createClue(current: StudioState, input: { projectId: string; sourceId: string; passageId: string; quote: string; category: ClueCategory }, id: string): StudioState {
  const title = input.quote.replace(/[，。“”！？；]/g, " ").trim().slice(0, 18);
  const newClue: StudioClue = {
    id,
    projectId: input.projectId,
    title,
    category: input.category,
    status: "unverified",
    tags: ["新建摘录"],
    sourceId: input.sourceId,
    passageId: input.passageId,
    quote: input.quote,
    severity: "high",
    cvssVector: "CVSS:3.1/AV:N/AC:L/PR:L/UI:N/S:U/C:H/I:N/A:N",
    cvssScore: 6.5,
    severityBasis: "从资产原文摘录创建，待补充对照实验确认最终等级。",
    impactSummary: "待结合实验报文确认实际影响范围。",
    conditionalImpact: "受限于当前测试身份权限边界。",
    affectedSourceIds: [input.sourceId],
    claims: [{ id: `${id}-claim-1`, text: input.quote, kind: "observed", evidenceIds: ["EX-014"] }],
    supportingEvidenceIds: ["EX-014"],
    counterEvidence: [],
    missingMaterials: ["待补充与正常业务请求的对照报文"],
    targetVersion: "snapshot-v46",
    readiness: "YELLOW",
    readinessReasons: ["刚从段落创建，尚需人工复核确认"],
  };
  return {
    ...current,
    clues: [newClue, ...current.clues],
    recent: withRef(current.recent, { kind: "clue", id }),
  };
}

// ---------------------------------------------------------------------------
// Imports
// ---------------------------------------------------------------------------

interface ImportIds {
  source: () => string;
  passage: (sourceId: string) => string;
}

function commitIds(): ImportIds {
  return { source: () => nextLocalId("s"), passage: (sourceId) => nextLocalId(`${sourceId}-p`) };
}

function previewIds(): ImportIds {
  let count = 0;
  return {
    source: () => { count += 1; return `import-preview-s${count}`; },
    passage: (sourceId) => { count += 1; return `${sourceId}-preview-p${count}`; },
  };
}

function localDate(): string {
  const now = new Date();
  return `${now.getFullYear()}-${String(now.getMonth() + 1).padStart(2, "0")}-${String(now.getDate()).padStart(2, "0")}`;
}

function titleKey(title: string): string {
  return title.trim().replace(/\s+/g, " ").toLowerCase();
}

function importMatch(current: StudioState, projectId: string, values: ImportValues): { source?: StudioSource; error?: string } {
  const scoped = current.sources.filter((source) => source.projectId === projectId);
  if (values.key) {
    return { source: scoped.find((source) => current.importOrigins[source.id]?.key === values.key) ?? scoped.find((source) => source.id === values.key) };
  }
  const title = titleKey(values.title);
  const hits = scoped.filter((source) => titleKey(source.title) === title);
  if (hits.length > 1) return { error: `资产库里有 ${hits.length} 份同名资产《${values.title}》，无法确定要更新哪一份。请给记录填上唯一标识后重试。` };
  return { source: hits[0] };
}

function passageUsage(current: StudioState, source: StudioSource): Map<string, { quotes: string[]; refs: number }> {
  const ids = new Set(source.passages.map((passage) => passage.id));
  const usage = new Map<string, { quotes: string[]; refs: number }>();
  const entry = (id: string) => {
    const found = usage.get(id) ?? { quotes: [], refs: 0 };
    usage.set(id, found);
    return found;
  };
  current.clues.forEach((clue) => { if (ids.has(clue.passageId)) entry(clue.passageId).quotes.push(clue.quote); });
  const refs = [...current.tasks.flatMap((task) => task.links), ...Object.values(current.context).flat()];
  refs.forEach((ref) => { if (ref.kind === "passage" && ids.has(ref.id)) entry(ref.id).refs += 1; });
  return usage;
}

function rebuildPassages(current: StudioState, source: StudioSource, texts: string[], ids: ImportIds): { passages: StudioPassage[]; lost: string[] } {
  const usage = passageUsage(current, source);
  const slots: Array<StudioPassage | undefined> = texts.map(() => undefined);
  const used = new Set<string>();
  texts.forEach((text, at) => {
    const same = source.passages.find((passage) => !used.has(passage.id) && passage.text === text);
    if (!same) return;
    slots[at] = same;
    used.add(same.id);
  });
  const lost: string[] = [];
  source.passages.forEach((passage) => {
    const use = usage.get(passage.id);
    if (used.has(passage.id) || !use) return;
    const at = slots.findIndex((slot, index) => !slot && (use.quotes.length
      ? use.quotes.every((quote) => (texts[index] ?? "").includes(quote))
      : index === passage.index - 1));
    if (at >= 0) {
      slots[at] = passage;
      used.add(passage.id);
      return;
    }
    const parts = [use.quotes.length ? `${use.quotes.length} 条发现` : "", use.refs ? "任务或对话引用" : ""].filter(Boolean).join("、");
    lost.push(`第 ${passage.index} 段（${parts}）`);
  });
  return {
    passages: texts.map((text, index) => ({ id: slots[index]?.id ?? ids.passage(source.id), sourceId: source.id, index: index + 1, text })),
    lost,
  };
}

function applyImportRecord(
  current: StudioState,
  projectId: string,
  draft: Pick<ImportDraft, "values" | "parseError" | "mergedCount">,
  settings: ImportSettings,
  batchId: string,
  ids: ImportIds,
): { state: StudioState; result: ImportResult; previousSnap?: ImportPreviousSource } {
  const fail = (reason: string) => ({ state: current, result: { outcome: "failed" as const, reason } });
  if (draft.parseError) return fail(draft.parseError);
  const values = draft.values;
  const title = values.title.trim();
  if (!title) return fail(settings.mapping.title ? `标题为空：这条记录的「${settings.mapping.title}」没有内容。` : "标题为空：没有选择作为标题的字段。");
  if (title.length > IMPORT_TITLE_LIMIT) return fail(`标题有 ${title.length} 个字，超过 ${IMPORT_TITLE_LIMIT} 字的上限。`);
  const texts = splitPassages(values.body);
  if (!texts.length) return fail(settings.mapping.body ? `正文为空：这条记录的「${settings.mapping.body}」没有内容。` : "正文为空：没有选择作为正文的字段。");
  const kind = resolveSourceKind(values.kind);
  if (kind === undefined) {
    const names = SOURCE_KINDS.map((item) => `「${item.label}」`).join("、");
    return fail(`类型「${values.kind}」无法识别。可用的类型有${names}，留空时用默认类型。`);
  }
  if (!current.projects.some((project) => project.id === projectId)) return fail("目标项目已不存在。");
  const match = importMatch(current, projectId, values);
  if (match.error) return fail(match.error);
  const existing = match.source;

  if (!existing) {
    const id = ids.source();
    const collectedAt = values.collectedAt || localDate();
    const source: StudioSource = {
      id,
      projectId,
      title,
      kind: kind ?? settings.defaultKind,
      author: values.author || settings.defaultAuthor.trim() || "我",
      collectedAt,
      status: "inbox",
      canonicalKey: values.key || title,
      importance: "normal",
      tags: ["文件导入", ...(draft.mergedCount && draft.mergedCount > 1 ? [`合成${draft.mergedCount}条`] : [])],
      firstSeenAt: collectedAt,
      lastObservedAt: collectedAt,
      lastVerifiedAt: collectedAt,
      pinnedVersion: 1,
      passages: texts.map((text, index) => ({ id: ids.passage(id), sourceId: id, index: index + 1, text })),
    };
    return {
      state: {
        ...current,
        sources: [source, ...current.sources],
        recent: withRef(current.recent, { kind: "source", id }),
        importOrigins: { ...current.importOrigins, [id]: { batchId, key: values.key } },
      },
      result: {
        outcome: "created",
        reason: `${draft.mergedCount && draft.mergedCount > 1 ? `由 ${draft.mergedCount} 条记录合成为一份资料，` : ""}新建资产，共 ${texts.length} 段。`,
        sourceId: id,
      },
      previousSnap: { id, existed: false },
    };
  }

  const next = {
    title,
    kind: kind ?? existing.kind,
    author: values.author || existing.author,
    collectedAt: values.collectedAt || existing.collectedAt,
  };
  const bodyChanged = texts.length !== existing.passages.length || texts.some((text, index) => existing.passages[index]?.text !== text);
  const changes = [
    next.title !== existing.title ? "标题" : "",
    next.kind !== existing.kind ? "类型" : "",
    next.author !== existing.author ? "作者" : "",
    next.collectedAt !== existing.collectedAt ? "采集时间" : "",
    bodyChanged ? (texts.length === existing.passages.length ? "正文" : `正文（${existing.passages.length} 段 → ${texts.length} 段）`) : "",
  ].filter(Boolean);
  const matchedBy = values.key ? `唯一标识「${values.key}」` : "标题";
  if (!changes.length) {
    return { state: current, result: { outcome: "skipped", reason: `与资产库中的《${existing.title}》内容一致，没有需要更新的内容。`, sourceId: existing.id } };
  }
  if (settings.conflict === "skip") {
    return { state: current, result: { outcome: "skipped", reason: `按${matchedBy}匹配到已有的《${existing.title}》，按导入设置不覆盖已有资产。`, sourceId: existing.id } };
  }
  const rebuilt = rebuildPassages(current, existing, texts, ids);
  if (rebuilt.lost.length) {
    return fail(`按${matchedBy}匹配到《${existing.title}》，但新正文去掉了已被引用的原文：${rebuilt.lost.join("、")}。请在正文里保留这些文字后重试，以免发现和引用失去出处。`);
  }
  const source: StudioSource = { ...existing, ...next, passages: rebuilt.passages };
  const ref: StudioRef = { kind: "source", id: existing.id };
  return {
    state: {
      ...current,
      sources: current.sources.map((item) => (item.id === existing.id ? source : item)),
      updated: withRef(current.updated, ref),
      importOrigins: { ...current.importOrigins, [existing.id]: { batchId, key: values.key || current.importOrigins[existing.id]?.key || "" } },
    },
    result: { outcome: "updated", reason: `按${matchedBy}匹配到已有资产，更新了${changes.join("、")}。`, sourceId: existing.id },
    previousSnap: { id: existing.id, existed: true, previous: existing, previousOrigin: current.importOrigins[existing.id] },
  };
}

function runImport(current: StudioState, projectId: string, drafts: ImportDraft[], settings: ImportSettings, batchId: string, ids: ImportIds) {
  let nextState = current;
  const seen = new Map<string, { index: number; sourceId?: string }>();
  const previousSources: ImportPreviousSource[] = [];
  const results = drafts.map((draft): ImportResult => {
    const identity = draft.parseError ? "" : draft.values.key ? `key:${draft.values.key}` : `title:${titleKey(draft.values.title)}`;
    const earlier = identity ? seen.get(identity) : undefined;
    if (earlier) {
      return {
        outcome: "skipped",
        reason: `与本文件第 ${earlier.index} 条是同一份资产（${draft.values.key ? "唯一标识" : "标题"}相同），只导入第一条（或可切换为「按标识/标题合并多条记录」）。`,
        sourceId: earlier.sourceId,
      };
    }
    const applied = applyImportRecord(nextState, projectId, draft, settings, batchId, ids);
    nextState = applied.state;
    if (applied.previousSnap) previousSources.push(applied.previousSnap);
    if (identity && applied.result.outcome !== "failed") seen.set(identity, { index: draft.index, sourceId: applied.result.sourceId });
    return applied.result;
  });
  return { state: nextState, results, previousSources };
}

function importedRefs(items: ImportItem[]): StudioRef[] {
  const ids = new Set<string>();
  items.forEach((item) => { if ((item.outcome === "created" || item.outcome === "updated") && item.sourceId) ids.add(item.sourceId); });
  return Array.from(ids, (id): StudioRef => ({ kind: "source", id }));
}

export function previewImport(current: StudioState, projectId: string, drafts: ImportDraft[], settings: ImportSettings): ImportResult[] {
  return runImport(current, projectId, drafts, settings, "preview", previewIds()).results;
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

export const studio = {
  /** Resets the in-memory store to an empty workbench bound to a real SRC task. */
  initForTask(input: { taskId: string; threadId: string; title: string; goal: string }) {
    const tag = `task:${input.taskId}`;
    setState((current) => (current.hostTag === tag ? current : emptyTaskState(input)));
  },
  /** Resets the in-memory store to the demo seed (used when the prototype host remounts). */
  initDemo() {
    setState((current) => (current.hostTag === "demo" ? current : initialState()));
  },
  setActiveThread(threadId: string) {
    setState((current) => {
      const record = current.threads[threadId];
      if (!record || current.activeThreadId === threadId) return current;
      return { ...current, activeThreadId: threadId, workspace: { ...current.workspace, projectId: record.projectId } };
    });
  },

  createThread(projectId: string): string {
    const id = nextLocalId("t");
    const at = new Date().toISOString();
    setState((current) => ({
      ...current,
      threads: { ...current.threads, [id]: emptyThread(id, projectId, NEW_THREAD_TITLE, at) },
      threadOrder: [id, ...current.threadOrder],
      context: { ...current.context, [id]: [] },
    }));
    return id;
  },

  setRuntimeMode(runtimeMode: RuntimeSimulationMode) {
    setState((current) => (current.runtimeMode === runtimeMode ? current : { ...current, runtimeMode }));
  },

  /**
   * Simulates a reply. `refs` is the context snapshot taken when the message
   * was sent; `taskId` marks the message as the hand-off of that task.
   */
  send(threadId: string, text: string, refs: StudioRef[], options: { taskId?: string } = {}): string | null {
    const current = state;
    const record = current.threads[threadId];
    if (!record) return null;
    const turnId = `${threadId}-turn-${record.turns.length + 1}-${Date.now().toString(36)}`;
    const key = turnKey(threadId, turnId);
    const taskId = options.taskId && current.tasks.some((task) => task.id === options.taskId) ? options.taskId : undefined;
    const at = new Date().toISOString();
    const steps = current.runtimeMode === "simulated_agent_mcp" ? AGENT_MCP_STEPS : PLAN_STEPS;
    const modelLabel = current.runtimeMode === "simulated_agent_mcp" ? AGENT_MCP_MODEL_LABEL : LOCAL_MODEL_LABEL;
    const plan: ThreadPlanSnapshot = {
      revision: (record.plan?.revision ?? 0) + 1,
      phase: "executing",
      source: current.runtimeMode === "simulated_agent_mcp" ? "mcp-agent-simulation" : "local-simulation",
      turn_id: turnId,
      title: current.runtimeMode === "simulated_agent_mcp" ? "Agent + MCP 协同分析与证据核验" : "整理相关发现与全对象上下文（本地模拟）",
      tasks: steps.map((title, index) => ({ task_id: `${turnId}-step-${index + 1}`, title, status: index === 0 ? "in_progress" : "pending" })),
      last_change_summary:
        current.runtimeMode === "simulated_agent_mcp"
          ? "仿真执行：依次核验 Task 绑定、MCP 查询、证据比对与回写建议。"
          : "本地模拟进度：步骤按计时推进，使用离线演示数据。",
      updated_at: at,
    };
    updateThread(threadId, (item) => ({
      ...pushExchange(item, turnId, text, at, modelLabel),
      title: item.title === NEW_THREAD_TITLE && item.messages.length === 0 ? text.slice(0, 18) || NEW_THREAD_TITLE : item.title,
      plan,
    }));
    setState((now) => {
      const handoff = { ...now.handoff };
      delete handoff[threadId];
      return {
        ...now,
        threadOrder: [threadId, ...now.threadOrder.filter((id) => id !== threadId)],
        handoff,
        viewingExcluded: false,
        selectionExcluded: false,
        tasks: taskId ? now.tasks.map((task): StudioTask => (task.id === taskId && task.status === "todo" ? { ...task, status: "doing" } : task)) : now.tasks,
        taskRuns: taskId ? { ...now.taskRuns, [taskId]: { threadId, turnId, steps, step: 0, done: false } } : now.taskRuns,
        turnTasks: taskId ? { ...now.turnTasks, [key]: taskId } : now.turnTasks,
        turnRefs: { ...now.turnRefs, [key]: refs },
      };
    });
    const advanceRun = (step: number, done: boolean) => {
      if (!taskId) return;
      setState((now) => {
        const run = now.taskRuns[taskId];
        if (!run || run.turnId !== turnId) return now;
        return { ...now, taskRuns: { ...now.taskRuns, [taskId]: { ...run, step, done } } };
      });
    };
    for (let step = 1; step <= steps.length; step += 1) {
      schedule(() => {
        if (step < steps.length) {
          updateThread(threadId, (item) => (item.plan && item.plan.turn_id === turnId ? { ...item, plan: planWithStep(item.plan, step) } : item));
          advanceRun(step, false);
          return;
        }
        const latest = state;
        const clues = relatedClues(latest, threadId, text, refs);
        const built = buildSuggestions(latest, text, clues, refs, key);
        const task = taskId ? latest.tasks.find((item) => item.id === taskId) : undefined;
        const reply = composeReply(latest, threadId, text, clues, refs, built, task);
        const doneAt = new Date().toISOString();
        updateThread(threadId, (item) => {
          const seq = item.seq + 1;
          return {
            ...item,
            plan: item.plan && item.plan.turn_id === turnId ? planWithStep(item.plan, step) : item.plan,
            messages: [...item.messages, { message_id: replyMessageId(turnId), turn_id: turnId, role: "assistant", kind: "message", text: reply, stream_seq: seq, created_at: doneAt }],
            turns: item.turns.map((turn) => (turn.turn_id === turnId ? { ...turn, status: "completed", completed_at: doneAt } : turn)),
            seq,
            updatedAt: doneAt,
          };
        });
        const ids = clues.map((clue) => clue.id);
        setState((now) => ({
          ...now,
          findings: { ...now.findings, [threadId]: ids },
          turnFindings: { ...now.turnFindings, [key]: ids },
          suggestions: { ...now.suggestions, [key]: built.items },
          suggestionScopes: { ...now.suggestionScopes, [key]: built.scopes },
          suggestionEmpty: { ...now.suggestionEmpty, [key]: built.empty },
        }));
        advanceRun(step, true);
      }, STEP_MS * step);
    }
    return turnId;
  },

  addContext(threadId: string, ref: StudioRef) {
    setState((current) => {
      const list = current.context[threadId] ?? [];
      if (list.some((item) => sameRef(item, ref))) return current;
      return { ...current, context: { ...current.context, [threadId]: [...list, ref] } };
    });
  },

  removeContext(threadId: string, ref: StudioRef) {
    setState((current) => ({
      ...current,
      context: { ...current.context, [threadId]: (current.context[threadId] ?? []).filter((item) => !sameRef(item, ref)) },
    }));
  },

  clearContext(threadId: string) {
    setState((current) => ({ ...current, context: { ...current.context, [threadId]: [] } }));
  },

  addContextMany(threadId: string, refs: StudioRef[]) {
    setState((current) => {
      const list = current.context[threadId] ?? [];
      const added = refs.filter((ref) => !list.some((item) => sameRef(item, ref)));
      if (!added.length) return current;
      return { ...current, context: { ...current.context, [threadId]: [...list, ...added] } };
    });
  },

  prepareHandoff(threadId: string, taskId: string) {
    setState((current) => (current.handoff[threadId] === taskId ? current : { ...current, handoff: { ...current.handoff, [threadId]: taskId } }));
  },

  clearHandoff(threadId: string) {
    setState((current) => {
      if (!current.handoff[threadId]) return current;
      const handoff = { ...current.handoff };
      delete handoff[threadId];
      return { ...current, handoff };
    });
  },

  setViewing(ref: StudioRef | null) {
    setState((current) => {
      if (current.viewing === ref || (current.viewing && ref && sameRef(current.viewing, ref))) {
        // Re-viewing the same object re-offers it as context for the next send.
        return current.viewingExcluded ? { ...current, viewingExcluded: false } : current;
      }
      return { ...current, viewing: ref, viewingExcluded: false };
    });
  },

  setViewingExcluded(excluded: boolean) {
    setState((current) => (current.viewingExcluded === excluded ? current : { ...current, viewingExcluded: excluded }));
  },

  clearSentContext(threadId: string) {
    setState((current) => {
      if (!current.viewingExcluded && !current.selectionExcluded && !current.handoff[threadId]) return current;
      const handoff = { ...current.handoff };
      delete handoff[threadId];
      return { ...current, handoff, viewingExcluded: false, selectionExcluded: false };
    });
  },

  setSelectionExcluded(excluded: boolean) {
    setState((current) => (current.selectionExcluded === excluded ? current : { ...current, selectionExcluded: excluded }));
  },

  markSeen(ref: StudioRef) {
    setState((current) => {
      if (!isRecent(current, ref) && !isUpdated(current, ref)) return current;
      return {
        ...current,
        recent: current.recent.filter((item) => !sameRef(item, ref)),
        updated: current.updated.filter((item) => !sameRef(item, ref)),
      };
    });
  },

  setWorkspaceProject(projectId: string) {
    setState((current) => (current.workspace.projectId === projectId ? current : { ...current, workspace: { ...current.workspace, projectId } }));
  },

  setClueFilter(patch: Partial<ClueFilter>) {
    setState((current) => ({ ...current, workspace: { ...current.workspace, clueFilter: { ...current.workspace.clueFilter, ...patch } } }));
  },

  resetClueFilter() {
    setState((current) => ({ ...current, workspace: { ...current.workspace, clueFilter: EMPTY_CLUE_FILTER } }));
  },

  setLibraryUi(patch: Partial<LibraryUi>) {
    setState((current) => {
      const checkedChanged = patch.checked !== undefined && patch.checked.join() !== current.workspace.library.checked.join();
      return {
        ...current,
        selectionExcluded: checkedChanged ? false : current.selectionExcluded,
        workspace: { ...current.workspace, library: { ...current.workspace.library, ...patch } },
      };
    });
  },

  setFindingsUi(patch: Partial<FindingsUi>) {
    setState((current) => {
      const checkedChanged = patch.checked !== undefined && patch.checked.join() !== current.workspace.findings.checked.join();
      return {
        ...current,
        selectionExcluded: checkedChanged ? false : current.selectionExcluded,
        workspace: { ...current.workspace, findings: { ...current.workspace.findings, ...patch } },
      };
    });
  },

  setTasksUi(patch: Partial<TasksUi>) {
    setState((current) => ({ ...current, workspace: { ...current.workspace, tasks: { ...current.workspace.tasks, ...patch } } }));
  },

  setReportsUi(patch: Partial<ReportsUi>) {
    setState((current) => ({ ...current, workspace: { ...current.workspace, reports: { ...current.workspace.reports, ...patch } } }));
  },

  setClueCategory(clueIds: string | string[], category: ClueCategory, scope?: string): string | null {
    return setRecordField("clue", clueIds, "category", category, scope);
  },

  setClueStatus(clueIds: string | string[], status: ClueStatus, scope?: string): string | null {
    return setRecordField("clue", clueIds, "status", status, scope);
  },

  setClueSeverity(clueIds: string | string[], severity: FindingSeverity, scope?: string): string | null {
    return setRecordField("clue", clueIds, "severity", severity, scope);
  },

  setClueReadiness(clueIds: string | string[], readiness: ReadinessLevel, scope?: string): string | null {
    return setRecordField("clue", clueIds, "readiness", readiness, scope);
  },

  setSourceStatus(sourceIds: string | string[], status: StudioSource["status"], scope?: string): string | null {
    return setRecordField("source", sourceIds, "status", status, scope);
  },

  setSourceKind(sourceIds: string | string[], kind: SourceKind, scope?: string): string | null {
    return setRecordField("source", sourceIds, "kind", kind, scope);
  },

  setSourceImportance(sourceIds: string | string[], importance: AssetImportance, scope?: string): string | null {
    return setRecordField("source", sourceIds, "importance", importance, scope);
  },

  /**
   * Batch edit tags (add, remove, or replace) on sources or findings, with full undo.
   */
  batchEditTags(kind: RecordKind, ids: string[], mode: "add" | "remove" | "replace", tags: string[], scope?: string): string | null {
    const cleanTags = Array.from(new Set(tags.map((t) => t.trim()).filter(Boolean)));
    if (!ids.length || (!cleanTags.length && mode !== "replace")) return null;
    let entryId: string | null = null;
    setState((current) => {
      const targetSet = new Set(ids);
      const tagChanges: TagChange[] = [];
      const computeNext = (prev: string[]) => {
        if (mode === "replace") return cleanTags;
        if (mode === "add") return Array.from(new Set([...prev, ...cleanTags]));
        return prev.filter((t) => !cleanTags.includes(t));
      };
      let nextSources = current.sources;
      let nextClues = current.clues;
      if (kind === "source") {
        nextSources = current.sources.map((s) => {
          if (!targetSet.has(s.id)) return s;
          const from = s.tags ?? [];
          const to = computeNext(from);
          if (from.join("\u0000") === to.join("\u0000")) return s;
          tagChanges.push({ kind: "source", id: s.id, from, to });
          return { ...s, tags: to };
        });
      } else {
        nextClues = current.clues.map((c) => {
          if (!targetSet.has(c.id)) return c;
          const from = c.tags ?? [];
          const to = computeNext(from);
          if (from.join("\u0000") === to.join("\u0000")) return c;
          tagChanges.push({ kind: "clue", id: c.id, from, to });
          return { ...c, tags: to };
        });
      }
      if (!tagChanges.length) return current;
      const id = nextLocalId("h");
      entryId = id;
      const modeText = mode === "add" ? `添加标签「${cleanTags.join("、")}」` : mode === "remove" ? `移除标签「${cleanTags.join("、")}」` : `替换标签为「${cleanTags.join("、") || "空"}」`;
      return withHistory({ ...current, sources: nextSources, clues: nextClues }, {
        id,
        action: "tag",
        summary: `批量为 ${countLabel(kind, tagChanges.length)}${modeText}`,
        scope,
        refs: tagChanges.map((tc) => ({ kind, id: tc.id })),
        tagChanges,
      });
    });
    return entryId;
  },

  /**
   * Batch link checked assets or findings to a Task, WorkRun, or Report.
   */
  batchLinkRecords(kind: RecordKind, ids: string[], target: StudioRef, scope?: string): string | null {
    if (!ids.length) return null;
    let entryId: string | null = null;
    setState((current) => {
      const refsToAdd: StudioRef[] = ids.map((id) => ({ kind, id }));
      let nextTasks = current.tasks;
      let nextRuns = current.workRuns;
      let nextReports = current.reports;
      let changed = false;

      if (target.kind === "task") {
        nextTasks = current.tasks.map((t) => {
          if (t.id !== target.id) return t;
          const merged = dedupeRefs([...t.links, ...refsToAdd]);
          if (merged.length !== t.links.length) changed = true;
          return { ...t, links: merged };
        });
      } else if (target.kind === "run") {
        nextRuns = current.workRuns.map((r) => {
          if (r.id !== target.id) return r;
          const merged = dedupeRefs([...r.linkedRefs, ...refsToAdd]);
          if (merged.length !== r.linkedRefs.length) changed = true;
          return { ...r, linkedRefs: merged };
        });
      } else if (target.kind === "report") {
        nextReports = current.reports.map((rep) => {
          if (rep.id !== target.id) return rep;
          changed = true;
          return { ...rep, updatedAt: new Date().toISOString() };
        });
      }
      if (!changed) return current;
      const id = nextLocalId("h");
      entryId = id;
      return withHistory({ ...current, tasks: nextTasks, workRuns: nextRuns, reports: nextReports }, {
        id,
        action: "link",
        summary: `批量将 ${countLabel(kind, ids.length)}关联到「${refLabel(current, target)}」`,
        scope,
        refs: [target, ...refsToAdd.slice(0, 6)],
      });
    });
    return entryId;
  },

  /**
   * Batch delete sources, findings, or tasks with full snapshot undo!
   */
  batchDeleteRecords(kind: "source" | "clue" | "task", ids: string[], scope?: string): string | null {
    if (!ids.length) return null;
    let entryId: string | null = null;
    setState((current) => {
      const gone = new Set(ids);
      const deletedSources = kind === "source" ? current.sources.filter((s) => gone.has(s.id)) : [];
      const deletedClues = kind === "clue" ? current.clues.filter((c) => gone.has(c.id)) : [];
      const deletedTasks = kind === "task" ? current.tasks.filter((t) => gone.has(t.id)) : [];
      const count = deletedSources.length + deletedClues.length + deletedTasks.length;
      if (!count) return current;
      const id = nextLocalId("h");
      entryId = id;
      const noun = kind === "source" ? `${count} 份资产` : kind === "clue" ? `${count} 条发现` : `${count} 个任务`;
      return withHistory({
        ...current,
        sources: kind === "source" ? current.sources.filter((s) => !gone.has(s.id)) : current.sources,
        clues: kind === "clue" ? current.clues.filter((c) => !gone.has(c.id)) : current.clues,
        tasks: kind === "task" ? current.tasks.filter((t) => !gone.has(t.id)) : current.tasks,
        workspace: {
          ...current.workspace,
          library: kind === "source"
            ? {
                ...current.workspace.library,
                checked: current.workspace.library.checked.filter((x) => !gone.has(x)),
                sourceId: gone.has(current.workspace.library.sourceId) ? "" : current.workspace.library.sourceId,
              }
            : current.workspace.library,
          findings: kind === "clue"
            ? {
                ...current.workspace.findings,
                checked: current.workspace.findings.checked.filter((x) => !gone.has(x)),
                selectedId: gone.has(current.workspace.findings.selectedId) ? "" : current.workspace.findings.selectedId,
              }
            : current.workspace.findings,
          tasks: kind === "task"
            ? {
                ...current.workspace.tasks,
                highlightId: gone.has(current.workspace.tasks.highlightId) ? "" : current.workspace.tasks.highlightId,
              }
            : current.workspace.tasks,
        },
      }, {
        id,
        action: "delete",
        summary: `批量删除 ${noun}（可在处理记录中一键撤销恢复）`,
        scope,
        refs: ids.map((itemId) => ({ kind, id: itemId })),
        deletedSnapshot: {
          sources: deletedSources.length ? deletedSources : undefined,
          clues: deletedClues.length ? deletedClues : undefined,
          tasks: deletedTasks.length ? deletedTasks : undefined,
        },
      });
    });
    return entryId;
  },

  /**
   * Undoes a history entry: supports field changes, tag changes, merges, dismissed/undetermined duplicates,
   * newly created records (sources, findings, tasks), imported batches, and batch-deleted records!
   */
  revertHistory(entryId: string): { restored: number; kept: number } | null {
    let outcome: { restored: number; kept: number } | null = null;
    setState((current) => {
      const entry = current.history.find((item) => item.id === entryId);
      if (!entry || !canRevert(current, entry)) return current;
      if (entry.action === "merge" && entry.mergeId) {
        const undone = undoMergeIn(current, entry.mergeId);
        if (!undone) return current;
        const merge = current.merges.find((item) => item.id === entry.mergeId);
        outcome = { restored: merge?.memberIds.length ?? 0, kept: 0 };
        return undone.state;
      }
      if (entry.action === "import" && entry.batchId) {
        const undone = undoImportBatchIn(current, entry.batchId);
        if (!undone) return current;
        outcome = { restored: undone.restoredCount, kept: 0 };
        return undone.state;
      }
      const id = nextLocalId("h");
      const markReverted = (next: StudioState): StudioState => ({
        ...next,
        history: next.history.map((item) => (item.id === entryId ? { ...item, revertedBy: id } : item)),
      });
      if (entry.dismissKey) {
        const key = entry.dismissKey;
        outcome = { restored: 1, kept: 0 };
        return withHistory(markReverted({ ...current, dismissedDuplicates: current.dismissedDuplicates.filter((item) => item !== key) }), {
          id,
          action: "revert",
          summary: `撤销「${entry.summary}」：这组记录重新列为可能重复`,
          refs: entry.refs,
        });
      }
      if (entry.undeterminedKey) {
        const key = entry.undeterminedKey;
        const nextUndetermined = { ...current.undeterminedDuplicates };
        delete nextUndetermined[key];
        outcome = { restored: 1, kept: 0 };
        return withHistory(markReverted({ ...current, undeterminedDuplicates: nextUndetermined }), {
          id,
          action: "revert",
          summary: `撤销「${entry.summary}」：清除无法判断标记，恢复待审阅状态`,
          refs: entry.refs,
        });
      }
      if (entry.deletedSnapshot) {
        const snap = entry.deletedSnapshot;
        const restoredSources = snap.sources ?? [];
        const restoredClues = snap.clues ?? [];
        const restoredTasks = snap.tasks ?? [];
        const total = restoredSources.length + restoredClues.length + restoredTasks.length;
        outcome = { restored: total, kept: 0 };
        return withHistory(markReverted({
          ...current,
          sources: [...restoredSources, ...current.sources],
          clues: [...restoredClues, ...current.clues],
          tasks: [...restoredTasks, ...current.tasks],
        }), {
          id,
          action: "revert",
          summary: `撤销删除：已完整恢复 ${total} 项记录及其关联引用`,
          refs: entry.refs,
        });
      }
      if (entry.createdRefs?.length && !entry.changes?.length) {
        const rmSources = new Set(entry.createdRefs.filter((r) => r.kind === "source").map((r) => r.id));
        const rmClues = new Set(entry.createdRefs.filter((r) => r.kind === "clue").map((r) => r.id));
        const rmTasks = new Set(entry.createdRefs.filter((r) => r.kind === "task").map((r) => r.id));
        const total = rmSources.size + rmClues.size + rmTasks.size;
        outcome = { restored: total, kept: 0 };
        return withHistory(markReverted({
          ...current,
          sources: rmSources.size ? current.sources.filter((s) => !rmSources.has(s.id)) : current.sources,
          clues: rmClues.size ? current.clues.filter((c) => !rmClues.has(c.id)) : current.clues,
          tasks: rmTasks.size ? current.tasks.filter((t) => !rmTasks.has(t.id)) : current.tasks,
        }), {
          id,
          action: "revert",
          summary: `撤销新建「${entry.summary}」：已移除新建的 ${total} 项记录`,
          refs: entry.refs,
        });
      }
      if (entry.tagChanges?.length) {
        const tcMap = new Map(entry.tagChanges.map((tc) => [`${tc.kind}:${tc.id}`, tc.from]));
        outcome = { restored: entry.tagChanges.length, kept: 0 };
        return withHistory(markReverted({
          ...current,
          sources: current.sources.map((s) => (tcMap.has(`source:${s.id}`) ? { ...s, tags: tcMap.get(`source:${s.id}`) } : s)),
          clues: current.clues.map((c) => (tcMap.has(`clue:${c.id}`) ? { ...c, tags: tcMap.get(`clue:${c.id}`)! } : c)),
        }), {
          id,
          action: "revert",
          summary: `撤销标签修改「${entry.summary}」：恢复 ${entry.tagChanges.length} 项记录的原标签`,
          refs: entry.refs,
        });
      }
      const changes = entry.changes ?? [];
      const live = stillApplied(current, changes);
      const kept = changes.length - live.length;
      outcome = { restored: live.length, kept };
      const notes = [
        live.length ? `恢复了 ${live.length} 项原值` : "没有可以恢复的原值",
        kept ? `${kept} 项之后又改过，保持现在的值` : "",
        entry.action === "adopt" ? "新建的发现和任务保留" : "",
      ].filter(Boolean).join("，");
      return withHistory(markReverted(applyChanges(current, live, "from")), {
        id,
        action: "revert",
        summary: `撤销「${entry.summary}」：${notes}`,
        refs: live.map((change) => ({ kind: change.kind, id: change.id })),
        changes: live.map((change) => ({ ...change, from: change.to, to: change.from })),
      });
    });
    return outcome;
  },

  mergeRecords(input: { kind: RecordKind; primaryId: string; memberIds: string[]; reason: string; detected?: string }): { id: string } | { error: string } {
    let outcome: { id: string } | { error: string } = { error: "没有可以归并的记录。" };
    setState((current) => {
      const list: Array<{ id: string; projectId: string; title: string }> = input.kind === "clue" ? current.clues : current.sources;
      const noun = RECORD_NOUN[input.kind].noun;
      const primary = list.find((item) => item.id === input.primaryId);
      if (!primary) { outcome = { error: `作为主记录的${noun}已不存在。` }; return current; }
      if (mergeOf(current.merges, input.kind, primary.id)) { outcome = { error: `「${primary.title}」已经归并到别的${noun}里，请先撤销那次归并。` }; return current; }
      const members = Array.from(new Set(input.memberIds))
        .filter((id) => id !== primary.id)
        .map((id) => list.find((item) => item.id === id))
        .filter((item): item is { id: string; projectId: string; title: string } => Boolean(item));
      if (!members.length) { outcome = { error: `至少还要选一${RECORD_NOUN[input.kind].unit}${noun}归并到主记录。` }; return current; }
      const elsewhere = members.find((item) => item.projectId !== primary.projectId);
      if (elsewhere) { outcome = { error: `「${elsewhere.title}」属于另一个项目，只能归并同一项目里的${noun}。` }; return current; }
      const taken = members.find((item) => mergeOf(current.merges, input.kind, item.id));
      if (taken) { outcome = { error: `「${taken.title}」已经归并过了。` }; return current; }
      const reason = input.reason.trim();
      if (!reason) { outcome = { error: "请写下归并原因，之后查看或撤销时会用到。" }; return current; }
      const id = nextLocalId("merge");
      const memberIds = members.map((item) => item.id);
      const merge: MergeRecord = { id, kind: input.kind, projectId: primary.projectId, primaryId: primary.id, memberIds, reason, detected: input.detected ?? "", at: new Date().toISOString() };
      const gone = new Set(memberIds);
      const primaryRef: StudioRef = { kind: input.kind, id: primary.id };
      const library = current.workspace.library;
      const findings = current.workspace.findings;
      outcome = { id };
      return withHistory({
        ...current,
        merges: [...current.merges, merge],
        updated: withRef(current.updated, primaryRef),
        workspace: {
          ...current.workspace,
          library: input.kind === "source" ? { ...library, checked: library.checked.filter((item) => !gone.has(item)) } : library,
          findings: input.kind === "clue"
            ? { ...findings, checked: findings.checked.filter((item) => !gone.has(item)), selectedId: gone.has(findings.selectedId) ? primary.id : findings.selectedId }
            : findings,
        },
      }, {
        action: "merge",
        summary: `把 ${countLabel(input.kind, members.length)}归并到「${primary.title}」`,
        scope: `归并原因：${reason}`,
        refs: [primaryRef, ...memberIds.map((item) => ({ kind: input.kind, id: item }))],
        mergeId: id,
      });
    });
    return outcome;
  },

  undoMerge(mergeId: string): boolean {
    let done = false;
    setState((current) => {
      const undone = undoMergeIn(current, mergeId);
      if (!undone) return current;
      done = true;
      return undone.state;
    });
    return done;
  },

  dismissDuplicates(kind: RecordKind, ids: string[]): string | null {
    let entryId: string | null = null;
    setState((current) => {
      const key = duplicateKey(kind, ids);
      if (current.dismissedDuplicates.includes(key)) return current;
      const id = nextLocalId("h");
      entryId = id;
      return withHistory({ ...current, dismissedDuplicates: [...current.dismissedDuplicates, key] }, {
        id,
        action: "dismiss",
        summary: `${countLabel(kind, ids.length)}标为保持独立（不是重复）：${ids.slice(0, 2).map((item) => `「${recordTitle(current, kind, item)}」`).join("、")}${ids.length > 2 ? " 等" : ""}`,
        refs: ids.map((item) => ({ kind, id: item })),
        dismissKey: key,
      });
    });
    return entryId;
  },

  /** Marks a duplicate candidate group as "无法判断" (undetermined) with reason & missing verification note. */
  markDuplicatesUndetermined(kind: RecordKind, ids: string[], reason: string, missingNote: string): string | null {
    let entryId: string | null = null;
    setState((current) => {
      const key = duplicateKey(kind, ids);
      const id = nextLocalId("h");
      entryId = id;
      const projectId = kind === "clue"
        ? current.clues.find((c) => c.id === ids[0])?.projectId ?? STUDIO_DEFAULT_PROJECT_ID
        : current.sources.find((s) => s.id === ids[0])?.projectId ?? STUDIO_DEFAULT_PROJECT_ID;
      const record: UndeterminedDuplicate = {
        key,
        kind,
        projectId,
        ids,
        reason: reason.trim() || "现有材料不足以确定是否同一根因",
        missingNote: missingNote.trim() || "待补充后端路由或修复点对照后再裁决",
        at: new Date().toISOString(),
        by: "人工复核组 · 林工",
      };
      return withHistory({
        ...current,
        undeterminedDuplicates: { ...current.undeterminedDuplicates, [key]: record },
      }, {
        id,
        action: "undetermined",
        summary: `将 ${countLabel(kind, ids.length)}标为「无法判断」：${record.reason}`,
        scope: `待补材料：${record.missingNote}`,
        refs: ids.map((item) => ({ kind, id: item })),
        undeterminedKey: key,
      });
    });
    return entryId;
  },

  saveFilter(input: Pick<SavedFilter, "scope" | "name" | "filter">): string {
    const name = input.name.trim();
    let id = "";
    setState((current) => {
      const same = current.savedFilters.find((item) => item.scope === input.scope && item.name === name);
      id = same?.id ?? nextLocalId("filter");
      const saved = { ...input, name, id, createdAt: same?.createdAt ?? new Date().toISOString() } as SavedFilter;
      return {
        ...current,
        savedFilters: same ? current.savedFilters.map((item) => (item.id === same.id ? saved : item)) : [...current.savedFilters, saved],
      };
    });
    return id;
  },

  deleteSavedFilter(id: string) {
    setState((current) => ({ ...current, savedFilters: current.savedFilters.filter((item) => item.id !== id) }));
  },

  addSource(input: { projectId: string; title: string; kind: SourceKind; text: string; author: string; canonicalKey?: string; aliases?: string[]; importance?: AssetImportance }): string {
    const id = nextLocalId("s");
    const paragraphs = splitPassages(input.text);
    const now = localDate();
    const source: StudioSource = {
      id,
      projectId: input.projectId,
      title: input.title,
      kind: input.kind,
      author: input.author,
      collectedAt: "刚刚",
      status: "inbox",
      canonicalKey: input.canonicalKey || input.title,
      aliases: input.aliases?.length ? input.aliases : [],
      importance: input.importance ?? "normal",
      tags: ["手动录入"],
      firstSeenAt: now,
      lastObservedAt: now,
      lastVerifiedAt: now,
      pinnedVersion: 1,
      versions: [
        {
          version: 1,
          label: "v1（手动录入初始版）",
          createdAt: now,
          sourceLabel: `手动录入 · ${input.author}`,
          summary: `首次录入 ${paragraphs.length} 段正文`,
          attributes: { "规范标识 (Canonical Key)": input.canonicalKey || input.title, "资产类型": sourceKindLabel(input.kind) },
          passageCount: paragraphs.length,
          bodyPreview: paragraphs[0] ?? "",
        },
      ],
      passages: paragraphs.map((text, index) => ({ id: `${id}-p${index + 1}`, sourceId: id, index: index + 1, text })),
    };
    const ref: StudioRef = { kind: "source", id };
    setState((current) => withHistory(
      { ...current, sources: [source, ...current.sources], recent: withRef(current.recent, ref) },
      { action: "create", summary: `录入资产「${source.title}」，共 ${source.passages.length} 段`, refs: [ref], createdRefs: [ref] },
    ));
    return id;
  },

  /** Resolves a field conflict on an asset and records the basis. */
  resolveAssetConflict(sourceId: string, conflictId: string, selectedValue: string, resolutionBasis: string) {
    setState((current) => {
      const source = current.sources.find((s) => s.id === sourceId);
      if (!source || !source.conflicts) return current;
      const nextConflicts = source.conflicts.map((c) =>
        c.id === conflictId
          ? { ...c, selectedValue, resolutionBasis, resolvedBy: "manual" as const, resolvedAt: new Date().toISOString() }
          : c,
      );
      const nextAttributes = { ...(source.attributes ?? {}) };
      const targetConflict = source.conflicts.find((c) => c.id === conflictId);
      if (targetConflict) nextAttributes[targetConflict.field] = selectedValue;
      return withHistory({
        ...current,
        sources: current.sources.map((s) => (s.id === sourceId ? { ...s, conflicts: nextConflicts, attributes: nextAttributes } : s)),
        updated: withRef(current.updated, { kind: "source", id: sourceId }),
      }, {
        action: "conflict",
        summary: `裁决资产「${source.title}」的字段冲突（${targetConflict?.fieldLabel ?? conflictId}）`,
        scope: `选用值：${selectedValue} · 依据：${resolutionBasis}`,
        refs: [{ kind: "source", id: sourceId }],
      });
    });
  },

  /** Archives or restores an asset without altering frozen reports or historical observations. */
  toggleAssetArchive(sourceIds: string | string[], archived: boolean, reason = "人工归档整理"): string | null {
    const ids = Array.isArray(sourceIds) ? sourceIds : [sourceIds];
    const entryId = setRecordField("source", ids, "archived", String(archived), reason);
    if (entryId) {
      const now = localDate();
      setState((current) => ({
        ...current,
        sources: current.sources.map((s) =>
          ids.includes(s.id)
            ? { ...s, archived, archivedAt: archived ? now : undefined, archivedReason: archived ? reason : undefined }
            : s,
        ),
      }));
    }
    return entryId;
  },

  /** Unlinks or relinks an asset from a specific task/project without deleting the long-term asset entity. */
  toggleTaskUnlinkAsset(sourceId: string, taskId: string) {
    setState((current) => {
      const source = current.sources.find((s) => s.id === sourceId);
      if (!source) return current;
      const unlinked = source.unlinkedTaskIds ?? [];
      const isUnlinked = unlinked.includes(taskId);
      const nextUnlinked = isUnlinked ? unlinked.filter((id) => id !== taskId) : [...unlinked, taskId];
      return withHistory({
        ...current,
        sources: current.sources.map((s) => (s.id === sourceId ? { ...s, unlinkedTaskIds: nextUnlinked } : s)),
      }, {
        action: "archive",
        summary: isUnlinked
          ? `恢复资产「${source.title}」与当前任务 ${taskId} 的关联`
          : `解除资产「${source.title}」与任务 ${taskId} 的关联（长期资产实体与历史观测仍保留）`,
        refs: [{ kind: "source", id: sourceId }],
      });
    });
  },

  /** Publishes a fixed version of an asset to the long-term repository or pins a version across tasks. */
  publishOrPinAssetVersion(sourceId: string, version: number, targetTaskId?: string) {
    setState((current) => {
      const source = current.sources.find((s) => s.id === sourceId);
      if (!source) return current;
      return withHistory({
        ...current,
        sources: current.sources.map((s) =>
          s.id === sourceId
            ? {
                ...s,
                publishedVersion: version,
                pinnedVersion: version,
                reusedFromTaskId: targetTaskId ?? s.reusedFromTaskId,
              }
            : s,
        ),
        updated: withRef(current.updated, { kind: "source", id: sourceId }),
      }, {
        action: "link",
        summary: targetTaskId
          ? `跨任务复用资产「${source.title}」固定版本 v${version} 至任务 ${targetTaskId}（仅暴露选定版本字节，不共享实时会话）`
          : `将资产「${source.title}」固定版本 v${version} 发布至长期资产资料集`,
        refs: [{ kind: "source", id: sourceId }],
      });
    });
  },

  addClue(input: { projectId: string; sourceId: string; passageId: string; quote: string; category: ClueCategory }): string {
    const id = nextLocalId("c");
    const ref: StudioRef = { kind: "clue", id };
    setState((current) => {
      const next = createClue(current, input, id);
      const hit = findPassage(current, input.passageId);
      return withHistory(next, {
        action: "create",
        summary: `从${hit ? `《${hit.source.title}》第 ${hit.passage.index} 段` : "原文"}记下发现「${recordTitle(next, "clue", id)}」`,
        refs: [ref, { kind: "source", id: input.sourceId }],
        createdRefs: [ref],
      });
    });
    return id;
  },

  /** Updates extended finding dossier fields (severity, CVSS, impact, readiness, counter-evidence, missing materials, etc.). */
  updateFindingDossier(clueId: string, patch: Partial<StudioClue>, summaryNote = "更新发现档案详情") {
    setState((current) => {
      const clue = current.clues.find((c) => c.id === clueId);
      if (!clue) return current;
      const updatedClue: StudioClue = { ...clue, ...patch };
      return withHistory({
        ...current,
        clues: current.clues.map((c) => (c.id === clueId ? updatedClue : c)),
        updated: withRef(current.updated, { kind: "clue", id: clueId }),
      }, {
        action: "status",
        summary: `更新发现「${clue.title}」：${summaryNote}`,
        refs: [{ kind: "clue", id: clueId }],
      });
    });
  },

  addFindingClaim(clueId: string, claim: Omit<FindingClaimItem, "id">) {
    setState((current) => {
      const clue = current.clues.find((c) => c.id === clueId);
      if (!clue) return current;
      const newClaim: FindingClaimItem = { ...claim, id: nextLocalId(`${clueId}-claim`) };
      return withHistory({
        ...current,
        clues: current.clues.map((c) => (c.id === clueId ? { ...c, claims: [...(c.claims ?? []), newClaim] } : c)),
      }, {
        action: "status",
        summary: `为发现「${clue.title}」新增${claim.kind === "observed" ? "观察事实" : claim.kind === "inferred" ? "支持推论" : "未验证假设"}主张`,
        refs: [{ kind: "clue", id: clueId }],
      });
    });
  },

  addTask(input: { projectId: string; title: string; assignee: string; links: StudioRef[]; threadId?: string; originKind?: StudioTask["originKind"]; notes?: string }): string {
    const id = nextLocalId("task");
    const ref: StudioRef = { kind: "task", id };
    setState((current) => withHistory(createTask(current, input, id), {
      action: "task",
      summary: `新建任务「${input.title}」${input.links.length ? `，关联 ${input.links.length} 个对象` : ""}`,
      refs: [ref, ...input.links.slice(0, 5)],
      createdRefs: [ref],
    }));
    return id;
  },

  /**
   * Applies adopted proposals once each. Supports recategorize, finding, task,
   * merge, conflict_resolve, plan_update, report_draft, and retest_record!
   */
  dismissSuggestion(suggestionId: string) {
    setState((current) => {
      const next = { ...current.suggestions };
      for (const k of Object.keys(next)) next[k] = next[k].filter((s) => s.id !== suggestionId);
      return { ...current, suggestions: next };
    });
  },

  adoptSuggestions(key: string, suggestionIds: string[], input: { threadId: string; assignee: string }): { created: StudioRef[]; updated: StudioRef[] } {
    const created: StudioRef[] = [];
    const changed: StudioRef[] = [];
    setState((current) => {
      const list = current.suggestions[key];
      if (!list) return current;
      let next = current;
      const changes: FieldChange[] = [];
      const applied = list.map((item): StudioSuggestion => {
        if (item.result || !suggestionIds.includes(item.id)) return item;
        if (item.kind === "recategorize") {
          const clue = next.clues.find((entry) => entry.id === item.clueId);
          if (!clue || !item.to) return item;
          const to = item.to;
          if (clue.category !== to) changes.push({ kind: "clue", id: clue.id, field: "category", from: clue.category, to });
          next = { ...next, clues: next.clues.map((entry) => (entry.id === clue.id ? { ...entry, category: to } : entry)), updated: withRef(next.updated, { kind: "clue", id: clue.id }) };
          changed.push({ kind: "clue", id: clue.id });
          return { ...item, result: { kind: "clue", id: clue.id } };
        }
        if (item.kind === "finding") {
          const hit = item.passageId ? findPassage(next, item.passageId) : undefined;
          if (!hit || !item.quote) return item;
          const existing = next.clues.find((entry) => entry.passageId === hit.passage.id && entry.quote === item.quote);
          if (existing) return { ...item, result: { kind: "clue", id: existing.id } };
          const id = nextLocalId("c");
          next = createClue(next, { projectId: hit.source.projectId, sourceId: hit.source.id, passageId: hit.passage.id, quote: item.quote, category: item.category ?? "detail" }, id);
          created.push({ kind: "clue", id });
          return { ...item, result: { kind: "clue", id } };
        }
        if (item.kind === "merge" && item.primaryId && item.memberIds?.length) {
          const mKind = item.mergeKind ?? "clue";
          const mergeId = nextLocalId("merge");
          const merge: MergeRecord = {
            id: mergeId,
            kind: mKind,
            projectId: next.workspace.projectId === "all" ? STUDIO_DEFAULT_PROJECT_ID : next.workspace.projectId,
            primaryId: item.primaryId,
            memberIds: item.memberIds,
            reason: item.rationale ?? "采纳 AI 同根因归并建议",
            detected: "AI 同因归并建议",
            at: new Date().toISOString(),
          };
          const primaryRef: StudioRef = { kind: mKind, id: item.primaryId };
          next = {
            ...next,
            merges: [...next.merges, merge],
            updated: withRef(next.updated, primaryRef),
          };
          changed.push(primaryRef);
          return { ...item, result: primaryRef };
        }
        if (item.kind === "conflict_resolve" && item.sourceId && item.conflictId && item.conflictValue) {
          const src = next.sources.find((s) => s.id === item.sourceId);
          if (!src) return item;
          const nextConflicts = (src.conflicts ?? []).map((c) =>
            c.id === item.conflictId
              ? { ...c, selectedValue: item.conflictValue!, resolutionBasis: item.rationale ?? "采纳 AI 冲突裁决", resolvedBy: "ai" as const, resolvedAt: new Date().toISOString() }
              : c,
          );
          const ref: StudioRef = { kind: "source", id: src.id };
          next = {
            ...next,
            sources: next.sources.map((s) => (s.id === src.id ? { ...s, conflicts: nextConflicts } : s)),
            updated: withRef(next.updated, ref),
          };
          changed.push(ref);
          return { ...item, result: ref };
        }
        if (item.kind === "plan_update") {
          const latestPlan = next.planVersions[0];
          const nextVer = (latestPlan?.version ?? 46) + 1;
          const planId = `PLAN-0${nextVer}`;
          const newPlan: StudioPlanVersion = {
            id: planId,
            projectId: STUDIO_DEFAULT_PROJECT_ID,
            version: nextVer,
            createdAt: localDate(),
            author: "AI 建议回写 · 人工确认",
            goal: latestPlan?.goal ?? "持续梳理资产与高风险发现队列",
            focusQuestion: item.planFocusQuestion ?? "推进跨租户边界补证与复核",
            revisionBasis: item.rationale ?? "采纳对话中的 AI 计划修订建议",
            triggerRefs: (next.turnRefs[key] ?? []).slice(0, 4),
            currentJudgement: item.planJudgement ?? "已根据最新对话证据更新判断。",
            materialGaps: latestPlan?.materialGaps ?? [],
            proposedActions: ["核对新采纳的发现与归并记录", "完成报告草稿冻结与复测登记"],
            scopeVersionId: latestPlan?.scopeVersionId ?? "AUTH-002",
          };
          const ref: StudioRef = { kind: "plan", id: planId };
          next = {
            ...next,
            planVersions: [newPlan, ...next.planVersions],
            recent: withRef(next.recent, ref),
            workspace: { ...next.workspace, tasks: { ...next.workspace.tasks, tab: "plan", selectedPlanId: planId } },
          };
          created.push(ref);
          return { ...item, result: ref };
        }
        if (item.kind === "report_draft" && item.clueId) {
          const existingReport = next.reports.find((r) => r.findingId === item.clueId && r.status === "draft");
          if (existingReport) {
            const ref: StudioRef = { kind: "report", id: existingReport.id };
            next = {
              ...next,
              reports: next.reports.map((r) => (r.id === existingReport.id ? { ...r, updatedAt: localDate() } : r)),
              updated: withRef(next.updated, ref),
              workspace: { ...next.workspace, reports: { ...next.workspace.reports, tab: "reports", selectedReportId: existingReport.id } },
            };
            changed.push(ref);
            return { ...item, result: ref };
          }
          const clue = next.clues.find((c) => c.id === item.clueId);
          const reportId = `R-${item.clueId.replace(/^F-/, "")}-draft`;
          const newReport: StudioReport = {
            id: reportId,
            projectId: clue?.projectId ?? STUDIO_DEFAULT_PROJECT_ID,
            findingId: item.clueId,
            title: `${reportId} (草稿) · ${clue?.title ?? item.clueId} 交付报告`,
            version: 1,
            status: "draft",
            templateId: "bsrc",
            createdAt: localDate(),
            updatedAt: localDate(),
            targetVersion: clue?.targetVersion ?? "snapshot-v46",
            scopeRef: "AUTH-002",
            planRef: next.planVersions[0]?.id ?? "PLAN-046",
            readiness: clue?.readiness ?? "YELLOW",
            sections: {
              scopeAndTarget: `关联资产：${(clue?.affectedSourceIds ?? [clue?.sourceId ?? "A-0887"]).join("、")}`,
              preconditions: "普通测试租户会话（tenant-a-tester）",
              mechanism: clue?.quote ?? "接口访问控制策略与文档声明不一致。",
              normalVsActual: "正常应拦截跨边界请求；实际返回 200 OK。",
              steps: ["1. 准备测试身份会话", "2. 发送基线与实验对照请求", "3. 核对响应字段与截图证据"],
              pocScript: `# 自动生成的复现草稿 (${item.clueId})\ncurl -i 'https://api-887.example.invalid/api/v2/tenant/export'`,
              pocVerified: true,
              confirmedImpact: clue?.impactSummary ?? "已证实配置边界不一致。",
              conditionalImpact: clue?.conditionalImpact ?? "仅限演示沙箱范围。",
              cvssVector: clue?.cvssVector ?? "CVSS:3.1/AV:N/AC:L/PR:L/UI:N/S:U/C:H/I:L/A:N",
              cvssScore: clue?.cvssScore ?? 7.5,
              remediation: "统一网关鉴权策略并启用严格租户绑定校验。",
              limitations: "离线演示数据。",
            },
            evidenceIds: clue?.supportingEvidenceIds ?? ["EX-014"],
            materialGaps: clue?.missingMaterials ?? [],
            strangerChecklist: [
              { item: "测试账号前提与证据 SHA-256 已关联", passed: true, note: "由 AI 建议生成草稿" },
            ],
            submission: {
              status: "not_submitted",
              externalTicketId: "",
              platformName: "百度安全应急响应中心 (BSRC · 演示)",
              rewardStatus: "none",
              rewardCurrency: "CNY",
              notes: "由 AI 建议生成的报告草稿",
              auditTrail: [],
            },
          };
          const ref: StudioRef = { kind: "report", id: reportId };
          next = {
            ...next,
            reports: [newReport, ...next.reports],
            recent: withRef(next.recent, ref),
            workspace: { ...next.workspace, reports: { ...next.workspace.reports, tab: "reports", selectedReportId: reportId } },
          };
          created.push(ref);
          return { ...item, result: ref };
        }
        if (item.kind === "retest_record" && item.clueId) {
          const retestId = nextLocalId("RETEST");
          const rep = next.reports.find((r) => r.findingId === item.clueId) ?? next.reports[0];
          const rec: StudioRetestRecord = {
            id: retestId,
            projectId: STUDIO_DEFAULT_PROJECT_ID,
            findingId: item.clueId,
            baseReportId: rep?.id ?? "R-052",
            targetFixVersion: "snapshot-v47",
            result: item.retestVerdict ?? "still_vulnerable",
            prerequisiteCheck: "测试账号与目标版本前提已核对",
            normalControlCheck: "同租户正常请求返回 200 OK",
            basisAndEvidence: "由对话 AI 建议生成的复核记录，已关联 EX-014 / EX-019 对照结果。",
            evidenceIds: ["EX-014", "EX-019"],
            reviewer: "人工复核组 · 林工",
            reviewedAt: localDate(),
          };
          const ref: StudioRef = { kind: "report", id: rep?.id ?? "R-052" };
          next = {
            ...next,
            retests: [rec, ...next.retests],
            updated: withRef(next.updated, ref),
          };
          changed.push(ref);
          return { ...item, result: ref };
        }
        const clue = next.clues.find((entry) => entry.id === item.clueId);
        if (!clue) return item;
        const id = nextLocalId("task");
        next = createTask(next, {
          projectId: clue.projectId,
          title: item.title,
          assignee: input.assignee,
          links: [{ kind: "clue", id: clue.id }, { kind: "source", id: clue.sourceId }],
          threadId: input.threadId,
          originKind: "ai",
        }, id);
        created.push({ kind: "task", id });
        return { ...item, result: { kind: "task", id } };
      });
      if (!created.length && !changed.length && applied.every((item, index) => item === list[index])) return current;
      const withSuggestions = { ...next, suggestions: { ...next.suggestions, [key]: applied } };
      if (!created.length && !changed.length) return withSuggestions;
      return withHistory(withSuggestions, {
        action: "adopt",
        summary: `采纳 AI 建议：生效 ${created.length + changed.length} 项回写改动`,
        scope: `来自「${current.threads[input.threadId]?.title ?? "研究对话"}」的一条回复`,
        refs: [...changed, ...created],
        changes: changes.length ? changes : undefined,
        createdRefs: created.length ? created : undefined,
      });
    });
    return { created, updated: changed };
  },

  setTaskStatus(taskId: string, status: TaskStatus) {
    setState((current) => {
      const task = current.tasks.find((item) => item.id === taskId);
      if (!task || task.status === status) return current;
      return withHistory({ ...current, tasks: current.tasks.map((item) => (item.id === taskId ? { ...item, status } : item)) }, {
        action: "task",
        summary: `任务「${task.title}」改为「${TASK_STATUSES.find((item) => item.value === status)?.label ?? status}」`,
        refs: [{ kind: "task", id: taskId }],
      });
    });
  },

  /** Imports a parsed file into `projectId` and records the batch, including `previousSources` for full batch undo. */
  importFile(input: { projectId: string; fileName: string; size: number; text: string; settings: ImportSettings; drafts: ImportDraft[] }): { batchId: string; counts: ImportCounts } {
    const batchId = nextLocalId("IMPORT");
    let counts: ImportCounts = { created: 0, updated: 0, skipped: 0, failed: 0 };
    setState((current) => {
      const run = runImport(current, input.projectId, input.drafts, input.settings, batchId, commitIds());
      const missing: ImportResult = { outcome: "failed", reason: "没有处理这条记录。" };
      const items = input.drafts.map((draft, index): ImportItem => ({
        ...draft,
        ...(run.results[index] ?? missing),
        id: `${batchId}-r${draft.index}`,
        attempts: 1,
      }));
      counts = countImportOutcomes(items);
      const batch: ImportBatch = {
        id: batchId,
        projectId: input.projectId,
        fileName: input.fileName,
        format: input.settings.format,
        size: input.size,
        importedAt: new Date().toISOString(),
        text: input.text,
        settings: input.settings,
        items,
        previousSources: run.previousSources,
      };
      return withHistory({ ...run.state, imports: [batch, ...run.state.imports] }, {
        action: "import",
        summary: `导入《${input.fileName}》${items.length} 条记录：${importCountsText(counts)}`,
        refs: [{ kind: "import", id: batchId }, ...importedRefs(items)],
        batchId,
      });
    });
    return { batchId, counts };
  },

  /** Undoes an entire import batch, removing created sources and restoring updated ones. */
  undoImportBatch(batchId: string): number {
    let restored = 0;
    setState((current) => {
      const undone = undoImportBatchIn(current, batchId);
      if (!undone) return current;
      restored = undone.restoredCount;
      return undone.state;
    });
    return restored;
  },

  /** Simulates syncing from an external connector (FOFA, Quake, ScopeSentry, BBOT, Burp/Caido, MCP) into an ImportBatch! */
  runConnectorSync(connectorId: string, projectId: string, customQuery?: string): { batchId: string; counts: ImportCounts } | null {
    const conn = state.connectors.find((c) => c.id === connectorId);
    if (!conn) return null;
    const query = (customQuery ?? conn.defaultQuery).trim() || conn.defaultQuery;
    const tag = Math.floor(Math.random() * 90 + 10);
    const csvLines = [
      "asset_id,canonical_key,asset_type,owner,title,body",
      `A-CONN-${tag}1,${conn.id.replace("conn-", "")}-gw-${tag}1.example.invalid,api,${conn.name},A-CONN-${tag}1 · ${conn.id.replace("conn-", "")}-gw-${tag}1.example.invalid,由外部连接器「${conn.name}」同步：匹配查询 ${query}。状态 200 OK，端口 8443/TCP，技术栈 Envoy 1.29。`,
      `A-CONN-${tag}2,${conn.id.replace("conn-", "")}-edge-${tag}2.example.invalid,service,${conn.name},A-CONN-${tag}2 · ${conn.id.replace("conn-", "")}-edge-${tag}2.example.invalid,由外部连接器「${conn.name}」同步：Web 控制台节点，证书 CN=*.example.invalid，保留原始测绘响应引用。`,
      `A-0887,api-887.example.invalid,api,${conn.name},A-0887 · api-887.example.invalid,API 服务 887 · 连接器「${conn.name}」增量刷新核对通过（查询：${query}），承载 /api/v2/tenant/export 多租户导出端点。`,
    ];
    const csvText = csvLines.join("\n");
    const drafts: ImportDraft[] = [
      {
        index: 1,
        location: { label: "第 2 行 · 远端页 #1", line: 2, endLine: 2 },
        raw: csvLines[1],
        values: {
          key: `A-CONN-${tag}1`,
          title: `A-CONN-${tag}1 · ${conn.id.replace("conn-", "")}-gw-${tag}1.example.invalid`,
          body: `由外部连接器「${conn.name}」同步：匹配查询 ${query}。状态 200 OK，端口 8443/TCP，技术栈 Envoy 1.29。`,
          kind: "archive",
          author: conn.name,
          collectedAt: localDate(),
        },
        parseError: "",
      },
      {
        index: 2,
        location: { label: "第 3 行 · 远端页 #1", line: 3, endLine: 3 },
        raw: csvLines[2],
        values: {
          key: `A-CONN-${tag}2`,
          title: `A-CONN-${tag}2 · ${conn.id.replace("conn-", "")}-edge-${tag}2.example.invalid`,
          body: `由外部连接器「${conn.name}」同步：Web 控制台节点，证书 CN=*.example.invalid，保留原始测绘响应引用。`,
          kind: "field",
          author: conn.name,
          collectedAt: localDate(),
        },
        parseError: "",
      },
      {
        index: 3,
        location: { label: "第 4 行 · 远端页 #1", line: 4, endLine: 4 },
        raw: csvLines[3],
        values: {
          key: "A-0887",
          title: "A-0887 · api-887.example.invalid",
          body: `API 服务 887 · 连接器「${conn.name}」增量刷新核对通过（查询：${query}），承载 /api/v2/tenant/export 多租户导出端点。`,
          kind: "archive",
          author: conn.name,
          collectedAt: localDate(),
        },
        parseError: "",
      },
    ];
    const settings: ImportSettings = {
      format: "csv",
      fileName: `${conn.id}-sync-${localDate()}.csv`,
      delimiter: ",",
      header: true,
      txtMode: "whole",
      encoding: "utf-8",
      combineMode: "per_row",
      mapping: { key: "asset_id", title: "title", body: "body", kind: "asset_type", author: "owner", collectedAt: "" },
      defaultKind: "archive",
      defaultAuthor: conn.name,
      conflict: "update",
    };
    const done = studio.importFile({
      projectId,
      fileName: `${conn.name} 同步批次 (${localDate()})`,
      size: csvText.length,
      text: csvText,
      settings,
      drafts,
    });
    setState((current) => ({
      ...current,
      imports: current.imports.map((b) =>
        b.id === done.batchId
          ? {
              ...b,
              connectorMeta: {
                connectorId: conn.id,
                sourceSystem: conn.name,
                query,
                completeness: "complete",
                costCredits: conn.costPerSync,
                receiptId: `RCPT-${conn.id.toUpperCase()}-${tag}`,
              },
            }
          : b,
      ),
      connectors: current.connectors.map((c) =>
        c.id === connectorId
          ? { ...c, quotaRemaining: Math.max(0, c.quotaRemaining - c.costPerSync), lastSyncedAt: localDate() }
          : c,
      ),
    }));
    return done;
  },

  retryImportItems(batchId: string, itemIds?: string[]): ImportCounts {
    let counts: ImportCounts = { created: 0, updated: 0, skipped: 0, failed: 0 };
    setState((current) => {
      const batch = current.imports.find((item) => item.id === batchId);
      if (!batch) return current;
      const targets = new Set(itemIds ?? batch.items.map((item) => item.id));
      const ids = commitIds();
      let next = current;
      const retried: ImportItem[] = [];
      const items = batch.items.map((item) => {
        if (!targets.has(item.id) || item.outcome !== "failed") return item;
        const applied = applyImportRecord(next, batch.projectId, item, batch.settings, batch.id, ids);
        next = applied.state;
        const updated: ImportItem = { ...item, ...applied.result, attempts: item.attempts + 1 };
        retried.push(updated);
        return updated;
      });
      if (!retried.length) return current;
      counts = countImportOutcomes(retried);
      return withHistory({ ...next, imports: next.imports.map((item) => (item.id === batchId ? { ...batch, items } : item)) }, {
        action: "import",
        summary: `重试《${batch.fileName}》的 ${retried.length} 条失败记录：${importCountsText(counts)}`,
        refs: [{ kind: "import", id: batchId }, ...importedRefs(retried)],
        batchId,
      });
    });
    return counts;
  },

  fixImportItem(batchId: string, itemId: string, values: ImportValues): ImportResult | null {
    let result: ImportResult | null = null;
    setState((current) => {
      const batch = current.imports.find((item) => item.id === batchId);
      const item = batch?.items.find((entry) => entry.id === itemId);
      if (!batch || !item || item.outcome !== "failed") return current;
      const corrected: ImportItem = { ...item, values, parseError: "" };
      const applied = applyImportRecord(current, batch.projectId, corrected, batch.settings, batch.id, commitIds());
      result = applied.result;
      const updated: ImportItem = { ...corrected, ...applied.result, attempts: item.attempts + 1 };
      return withHistory({
        ...applied.state,
        imports: applied.state.imports.map((entry) => (entry.id === batchId ? { ...batch, items: batch.items.map((row) => (row.id === itemId ? updated : row)) } : entry)),
      }, {
        action: "import",
        summary: `修正《${batch.fileName}》第 ${item.index} 条后重新导入：${importCountsText(countImportOutcomes([updated]))}`,
        scope: applied.result.reason,
        refs: [{ kind: "import", id: batchId }, ...importedRefs([updated])],
        batchId,
      });
    });
    return result;
  },

  // -------------------------------------------------------------------------
  // Module 1: Long-term Task Dossier & Plan Versions
  // -------------------------------------------------------------------------

  createPlanVersion(input: {
    projectId: string;
    goal: string;
    focusQuestion: string;
    revisionBasis: string;
    currentJudgement: string;
    materialGaps: string[];
    proposedActions: string[];
    triggerRefs: StudioRef[];
    scopeVersionId?: string;
  }): string {
    let planId = "";
    setState((current) => {
      const latest = current.planVersions[0];
      const nextVer = (latest?.version ?? 46) + 1;
      planId = `PLAN-0${nextVer}`;
      const newPlan: StudioPlanVersion = {
        id: planId,
        projectId: input.projectId,
        version: nextVer,
        createdAt: localDate(),
        author: "资料负责人（演示）",
        goal: input.goal.trim() || latest?.goal || "",
        focusQuestion: input.focusQuestion.trim(),
        revisionBasis: input.revisionBasis.trim(),
        triggerRefs: input.triggerRefs,
        currentJudgement: input.currentJudgement.trim(),
        materialGaps: input.materialGaps,
        proposedActions: input.proposedActions,
        scopeVersionId: input.scopeVersionId ?? latest?.scopeVersionId ?? "AUTH-002",
      };
      const ref: StudioRef = { kind: "plan", id: planId };
      return withHistory({
        ...current,
        planVersions: [newPlan, ...current.planVersions],
        recent: withRef(current.recent, ref),
        workspace: { ...current.workspace, tasks: { ...current.workspace.tasks, selectedPlanId: planId } },
      }, {
        action: "plan",
        summary: `发布新计划版本「${planId} (v${nextVer})」：${newPlan.focusQuestion}`,
        scope: `修订依据：${newPlan.revisionBasis}`,
        refs: [ref, ...input.triggerRefs],
      });
    });
    return planId;
  },

  updatePlanQuestionStatus(questionId: string, status: StudioPlanQuestion["status"]) {
    setState((current) => ({
      ...current,
      planQuestions: current.planQuestions.map((q) => (q.id === questionId ? { ...q, status } : q)),
    }));
  },

  addPlanQuestion(input: { question: string; priority: StudioPlanQuestion["priority"]; rationale: string; linkedRefs: StudioRef[] }) {
    setState((current) => {
      const id = `Q-0${current.planQuestions.length + 1}`;
      const q: StudioPlanQuestion = {
        id,
        question: input.question.trim(),
        status: "open",
        priority: input.priority,
        rationale: input.rationale.trim(),
        linkedRefs: input.linkedRefs,
      };
      return withHistory({ ...current, planQuestions: [q, ...current.planQuestions] }, {
        action: "plan",
        summary: `向问题清单添加问题「${q.question}」`,
        refs: input.linkedRefs,
      });
    });
  },

  // -------------------------------------------------------------------------
  // Module 7: Standalone Evidence & Attachments
  // -------------------------------------------------------------------------

  /** Server-side digest check for real tasks; the domain reload lands the result. */
  verifyEvidenceDigest(evidenceId: string) {
    setState((current) => withHistory(current, { action: "status", summary: `校验证据摘要 ${evidenceId}`, refs: [{ kind: "evidence", id: evidenceId }] }));
  },

  markEvidenceImageReviewed(evidenceId: string, reviewer = "人工复核组 · 林工 (demo-reviewer)") {
    setState((current) => {
      const ev = current.evidences.find((e) => e.id === evidenceId);
      if (!ev) return current;
      const now = localDate();
      const ref: StudioRef = { kind: "evidence", id: evidenceId };
      return withHistory({
        ...current,
        evidences: current.evidences.map((e) =>
          e.id === evidenceId ? { ...e, imageReviewedBy: reviewer, imageReviewedAt: now, digestVerified: "verified" } : e,
        ),
        updated: withRef(current.updated, ref),
      }, {
        action: "status",
        summary: `复核人已实际查看原图并签署「${ev.title}」（${reviewer}）`,
        refs: [ref],
      });
    });
  },

  replayHttpEvidence(evidenceId: string, customBody: string, actorRef = "tenant-a-tester (重放实验)"): string | null {
    let newId: string | null = null;
    setState((current) => {
      const base = current.evidences.find((e) => e.id === evidenceId);
      if (!base) return current;
      const id = `EX-R${current.evidences.length + 15}`;
      newId = id;
      const isCrossTenant = customBody.includes("tenant-b");
      const reqRaw = (base.requestRaw ?? "").split("\r\n\r\n")[0] + `\r\n\r\n${customBody}`;
      const respBody = isCrossTenant
        ? `{\n  "code": 0,\n  "replay_of": "${base.id}",\n  "tenant_id": "tenant-b-target",\n  "owner_unit": "演示隔离租户 B（重放验证命中）",\n  "records_preview": ["secret-b-cfg-01", "replayed-item-02"]\n}`
        : `{\n  "code": 0,\n  "replay_of": "${base.id}",\n  "tenant_id": "tenant-a-own",\n  "owner_unit": "演示租户 A（正常对照）",\n  "records_preview": ["item-a-01"]\n}`;
      const respRaw = `HTTP/1.1 200 OK\r\nContent-Type: application/json; charset=utf-8\r\nX-Replay-Of: ${base.id}\r\n\r\n${respBody}`;
      const created: StudioEvidence = {
        ...base,
        id,
        title: `${id} · 重放实验（基于 ${base.id} · ${isCrossTenant ? "跨租户参数" : "同租户对照"}）`,
        actorRef,
        capturedAt: localDate(),
        version: `replay-of-${base.id}`,
        requestRaw: reqRaw,
        responseRaw: respRaw,
        baselineExchangeId: base.id,
        completeness: "complete",
        digestVerified: "verified",
      };
      const ref: StudioRef = { kind: "evidence", id };
      return withHistory({
        ...current,
        evidences: [created, ...current.evidences],
        recent: withRef(current.recent, ref),
        workspace: { ...current.workspace, findings: { ...current.workspace.findings, view: "evidences", evidenceId: id } },
      }, {
        action: "create",
        summary: `基于 ${base.id} 执行受管请求重放，生成新证据「${created.title}」（原请求保持不变）`,
        refs: [ref, { kind: "evidence", id: base.id }],
        createdRefs: [ref],
      });
    });
    return newId;
  },

  // -------------------------------------------------------------------------
  // Module 8: External Work Records (Runs) & Progress Flowback
  // -------------------------------------------------------------------------

  advanceWorkRunAndFlowback(runId: string, targetThreadId?: string): string | null {
    if (state.hostTag && state.hostTag !== "demo") return null; // real runs: no simulated flowback
    let replyTurnId: string | null = null;
    setState((current) => {
      const run = current.workRuns.find((r) => r.id === runId);
      if (!run) return current;
      const threadId = targetThreadId ?? current.activeThreadId;
      const thread = current.threads[threadId];
      const now = new Date().toISOString();
      const nextGen = run.state === "blocked" || run.state === "failed" ? run.generation + 1 : run.generation;
      const newLog = {
        at: new Date().toTimeString().slice(0, 8),
        step: `代数 G${nextGen} 续跑完成并回流进展至主对话`,
        status: "ok" as const,
        detail: `已解除受阻条件，核对关联对象 ${run.linkedRefs.map((r) => r.id).join("、")}，水位同步至主会话。`,
      };
      const updatedRun: StudioWorkRun = {
        ...run,
        state: "completed",
        generation: nextGen,
        blockerReason: undefined,
        updatedAt: localDate(),
        logs: [...run.logs, newLog],
        flowedBackThreadIds: Array.from(new Set([...run.flowedBackThreadIds, threadId])),
      };

      let nextThreads = current.threads;
      const turnId = `${threadId}-runflow-${Date.now().toString(36)}`;
      replyTurnId = turnId;
      const key = turnKey(threadId, turnId);
      const clueIds = run.linkedRefs.filter((r) => r.kind === "clue").map((r) => r.id);

      if (thread) {
        const seq = thread.seq + 2;
        const userMsg: ConversationMessage = {
          message_id: `${turnId}-user`,
          turn_id: turnId,
          role: "user",
          kind: "message",
          text: `推进外部工作记录「${run.title}」（${run.id} · G${nextGen}）并将结果回流到当前对话。`,
          stream_seq: thread.seq + 1,
          created_at: now,
        };
        const assistantMsg: ConversationMessage = {
          message_id: replyMessageId(turnId),
          turn_id: turnId,
          role: "assistant",
          kind: "message",
          text: [
            `### 外部工作记录进展回流 · ${run.id} (代次 G${nextGen})`,
            `- **目标与问题**：\`${run.targetUrl}\` — ${run.question}`,
            `- **执行身份与凭证**：${run.identityContext} · 计划版本 \`${run.planVersionId}\` · 授权快照 \`${run.authVersionId}\``,
            `- **本轮结果**：状态已更新为 **已完成 (completed)**。${newLog.detail}`,
            `- **产出证据与关联对象**：${[...run.artifactRefs, ...run.linkedRefs].map((r) => `\`${r.id}\``).join("、")}`,
          ].join("\n"),
          stream_seq: seq,
          created_at: now,
        };
        nextThreads = {
          ...current.threads,
          [threadId]: {
            ...thread,
            messages: [...thread.messages, userMsg, assistantMsg],
            turns: [
              ...thread.turns,
              {
                turn_id: turnId,
                kind: "user",
                status: "completed",
                text: userMsg.text,
                runtime_snapshot: { ...LOCAL_RUNTIME_SNAPSHOT, model: AGENT_MCP_MODEL_LABEL },
                created_at: now,
                completed_at: now,
              },
            ],
            seq,
            updatedAt: now,
          },
        };
      }

      const clues = clueIds.flatMap((id) => current.clues.filter((c) => c.id === id));
      const allRefs: StudioRef[] = [{ kind: "run", id: run.id }, ...run.artifactRefs, ...run.linkedRefs];
      const built = buildSuggestions(current, "报告 归并 复核", clues, allRefs, key);

      return withHistory({
        ...current,
        workRuns: current.workRuns.map((r) => (r.id === runId ? updatedRun : r)),
        threads: nextThreads,
        findings: clueIds.length ? { ...current.findings, [threadId]: clueIds } : current.findings,
        turnFindings: { ...current.turnFindings, [key]: clueIds },
        turnRefs: { ...current.turnRefs, [key]: allRefs },
        suggestions: { ...current.suggestions, [key]: built.items },
        suggestionScopes: { ...current.suggestionScopes, [key]: built.scopes },
        updated: withRef(current.updated, { kind: "run", id: run.id }),
      }, {
        action: "task",
        summary: `推进外部工作记录「${run.id} (G${nextGen})」并将产物与结论回流至对话`,
        refs: allRefs.slice(0, 6),
      });
    });
    return replyTurnId;
  },

  createWorkRun(input: {
    projectId: string;
    title: string;
    engine: StudioWorkRun["engine"];
    targetUrl: string;
    question: string;
    identityContext: string;
    linkedRefs: StudioRef[];
  }): string {
    const id = `demo-run-0${state.workRuns.length + 45}`;
    setState((current) => {
      const newRun: StudioWorkRun = {
        id,
        projectId: input.projectId,
        title: `${input.title.trim()} · ${id}`,
        engine: input.engine,
        targetUrl: input.targetUrl.trim() || "https://api-887.example.invalid/api/v2/tenant/export",
        question: input.question.trim() || "专项深入验证与证据采集",
        state: "running",
        generation: 1,
        identityContext: input.identityContext.trim() || "tenant-a-tester",
        planVersionId: current.planVersions[0]?.id ?? "PLAN-046",
        authVersionId: current.scopeSnapshots[0]?.id ?? "AUTH-002",
        startedAt: localDate(),
        updatedAt: localDate(),
        logs: [
          { at: new Date().toTimeString().slice(0, 8), step: "创建可信 Task→Run 绑定", status: "ok", detail: `绑定 ${id}，继承计划与授权快照，不伪造未验证 Fact。` },
        ],
        artifactRefs: [{ kind: "evidence", id: "EX-014" }],
        linkedRefs: input.linkedRefs,
        flowedBackThreadIds: [],
      };
      const ref: StudioRef = { kind: "run", id };
      return withHistory({
        ...current,
        workRuns: [newRun, ...current.workRuns],
        recent: withRef(current.recent, ref),
        workspace: { ...current.workspace, tasks: { ...current.workspace.tasks, tab: "runs", selectedRunId: id } },
      }, {
        action: "create",
        summary: `创建外部工作记录「${newRun.title}」`,
        refs: [ref, ...input.linkedRefs],
      });
    });
    return id;
  },

  // -------------------------------------------------------------------------
  // Module 9, 10, 11: Reports, Submission/Bounty Ledger, Feedback & Retests
  // -------------------------------------------------------------------------

  updateReportDraft(
    reportId: string,
    patch: Partial<Pick<StudioReport, "title" | "templateId" | "targetVersion" | "readiness" | "evidenceIds">> & {
      sections?: Partial<StudioReport["sections"]>;
    },
  ) {
    setState((current) => {
      const report = current.reports.find((r) => r.id === reportId);
      if (!report) return current;
      const nextSections = patch.sections ? { ...report.sections, ...patch.sections } : report.sections;
      const nextEvidenceIds = patch.evidenceIds ?? report.evidenceIds;
      // Re-evaluate material gaps dynamically
      const gaps: string[] = [];
      if (nextEvidenceIds.includes("ART-REV-009")) {
        gaps.push("引用了已撤回不可读证据 ART-REV-009（墓碑状态），冻结前请移除");
      }
      if (!nextSections.pocVerified) {
        gaps.push("PoC 脚本尚未标记为已执行验证（pocVerified = false）");
      }
      if (!nextSections.steps.length || !nextSections.mechanism.trim()) {
        gaps.push("漏洞机制说明或复现步骤为空");
      }
      const updated: StudioReport = {
        ...report,
        ...patch,
        sections: nextSections,
        evidenceIds: nextEvidenceIds,
        materialGaps: gaps,
        readiness: gaps.length === 0 ? "GREEN" : gaps.length === 1 ? "YELLOW" : "RED",
        updatedAt: localDate(),
      };
      const ref: StudioRef = { kind: "report", id: reportId };
      return withHistory({
        ...current,
        reports: current.reports.map((r) => (r.id === reportId ? updated : r)),
        updated: withRef(current.updated, ref),
      }, {
        action: "report",
        summary: `编辑报告「${updated.title}」草稿内容`,
        refs: [ref, { kind: "clue", id: report.findingId }],
      });
    });
  },

  /** Freezes a draft report, or creates a new version draft from a frozen report (without inheriting platform submission status!). */
  freezeOrForkReport(reportId: string, mode: "freeze" | "new_version"): { ok: boolean; newReportId?: string; error?: string } {
    let result: { ok: boolean; newReportId?: string; error?: string } = { ok: false };
    setState((current) => {
      const report = current.reports.find((r) => r.id === reportId);
      if (!report) { result = { ok: false, error: "报告不存在。" }; return current; }

      if (mode === "freeze") {
        if (report.materialGaps.length > 0) {
          result = { ok: false, error: `还有 ${report.materialGaps.length} 项材料缺口未解决（${report.materialGaps[0]}），解决后才能冻结交付版本。` };
          return current;
        }
        const now = localDate();
        const frozen: StudioReport = {
          ...report,
          status: "frozen",
          readiness: "GREEN",
          frozenAt: now,
          frozenBy: "资料负责人（演示）",
          updatedAt: now,
        };
        const ref: StudioRef = { kind: "report", id: report.id };
        result = { ok: true, newReportId: report.id };
        return withHistory({
          ...current,
          reports: current.reports.map((r) =>
            r.id === report.id
              ? frozen
              : r.id === report.supersedesReportId
                ? { ...r, status: "superseded" }
                : r,
          ),
          updated: withRef(current.updated, ref),
        }, {
          action: "report",
          summary: `冻结报告版本「${report.title} (v${report.version})」，固定所选证据 SHA-256 与水位`,
          refs: [ref, { kind: "clue", id: report.findingId }],
        });
      }

      // mode === "new_version": fork a v(N+1) draft that supersedes `report.id` and resets submission status!
      const nextVer = report.version + 1;
      const newId = `${report.id.replace(/-v\d+$/, "")}-v${nextVer}`;
      const forked: StudioReport = {
        ...report,
        id: newId,
        title: `${report.id.replace(/-v\d+$/, "")} (v${nextVer} 草稿) · 基于 v${report.version} 修订`,
        version: nextVer,
        status: "draft",
        supersedesReportId: report.id,
        createdAt: localDate(),
        updatedAt: localDate(),
        frozenAt: undefined,
        frozenBy: undefined,
        submission: {
          status: "not_submitted",
          externalTicketId: report.submission.externalTicketId,
          platformName: report.submission.platformName,
          rewardStatus: "none",
          rewardCurrency: "CNY",
          notes: `由 v${report.version} 派生的新版本草稿；不自动继承旧版的已提交或奖励状态。`,
          auditTrail: [
            {
              at: localDate(),
              by: "资料负责人（演示）",
              fromStatus: `${report.submission.status} (v${report.version})`,
              toStatus: `not_submitted (v${nextVer})`,
              note: `派生 v${nextVer} 修订草稿`,
            },
          ],
        },
      };
      const ref: StudioRef = { kind: "report", id: newId };
      result = { ok: true, newReportId: newId };
      return withHistory({
        ...current,
        reports: [forked, ...current.reports],
        recent: withRef(current.recent, ref),
        workspace: {
          ...current.workspace,
          reports: { ...current.workspace.reports, selectedReportId: newId, compareReportId: report.id },
        },
      }, {
        action: "report",
        summary: `基于 ${report.id} 创建新版本草稿「${newId} (v${nextVer})」，保留版本演进与对照关系`,
        refs: [ref, { kind: "report", id: report.id }],
      });
    });
    return result;
  },

  updateSubmissionLedger(
    reportId: string,
    patch: Partial<Pick<StudioSubmissionLedger, "status" | "externalTicketId" | "platformName" | "rewardStatus" | "rewardAmount" | "paidAt" | "notes">>,
    auditNote = "人工更新平台提交与奖励台账",
  ) {
    setState((current) => {
      const report = current.reports.find((r) => r.id === reportId);
      if (!report) return current;
      const prev = report.submission;
      const now = localDate();
      const nextSub: StudioSubmissionLedger = {
        ...prev,
        ...patch,
        submittedAt: patch.status && patch.status !== "not_submitted" ? (prev.submittedAt ?? now) : prev.submittedAt,
        submittedBy: patch.status && patch.status !== "not_submitted" ? "资料负责人（演示）" : prev.submittedBy,
        auditTrail: [
          {
            at: now,
            by: "资料负责人（演示）",
            fromStatus: `${submissionStatusLabel(prev.status)} / ${prev.rewardStatus}`,
            toStatus: `${submissionStatusLabel(patch.status ?? prev.status)} / ${patch.rewardStatus ?? prev.rewardStatus}`,
            note: auditNote,
          },
          ...prev.auditTrail,
        ],
      };
      const ref: StudioRef = { kind: "report", id: reportId };
      return withHistory({
        ...current,
        reports: current.reports.map((r) => (r.id === reportId ? { ...r, submission: nextSub, updatedAt: now } : r)),
        updated: withRef(current.updated, ref),
      }, {
        action: "report",
        summary: `登记报告「${report.id}」人工提交与奖励台账：${submissionStatusLabel(nextSub.status)}${nextSub.rewardAmount ? ` · ¥${nextSub.rewardAmount.toLocaleString()}` : ""}`,
        scope: `${nextSub.externalTicketId ? `外部编号：${nextSub.externalTicketId} · ` : ""}用户人工记录，未与外部平台自动联网`,
        refs: [ref],
      });
    });
  },

  addPlatformFeedback(input: {
    reportId: string;
    externalTicketId: string;
    platformAuthor: string;
    kind: StudioPlatformFeedback["kind"];
    rawText: string;
    attachmentName?: string;
  }): string {
    let fbId = "";
    setState((current) => {
      const report = current.reports.find((r) => r.id === input.reportId) ?? current.reports[0];
      fbId = `FB-00${current.feedbacks.length + 1}`;
      const newFb: StudioPlatformFeedback = {
        id: fbId,
        projectId: report?.projectId ?? STUDIO_DEFAULT_PROJECT_ID,
        reportId: report?.id ?? "R-052",
        findingId: report?.findingId ?? "F-052",
        externalTicketId: input.externalTicketId.trim() || report?.submission.externalTicketId || "BSRC-2026-NEW",
        platformAuthor: input.platformAuthor.trim() || "平台审核员（人工录入）",
        receivedAt: localDate(),
        kind: input.kind,
        rawText: input.rawText.trim(),
        attachments: input.attachmentName?.trim()
          ? [{ name: input.attachmentName.trim(), mediaType: "text/plain", size: 420, summary: "随反馈录入的补充说明附件" }]
          : [],
        status: "pending",
      };
      const ref: StudioRef = { kind: "feedback", id: fbId };
      return withHistory({
        ...current,
        feedbacks: [newFb, ...current.feedbacks],
        recent: withRef(current.recent, ref),
        workspace: { ...current.workspace, reports: { ...current.workspace.reports, tab: "feedbacks", selectedFeedbackId: fbId } },
      }, {
        action: "report",
        summary: `录入平台反馈「${fbId} (${newFb.externalTicketId})」，关联报告 ${newFb.reportId}`,
        refs: [ref, { kind: "report", id: newFb.reportId }, { kind: "clue", id: newFb.findingId }],
      });
    });
    return fbId;
  },

  /** Creates a follow-up task from a platform feedback item and links the feedback, report, and finding! */
  createTaskFromFeedback(feedbackId: string): string | null {
    let taskId: string | null = null;
    setState((current) => {
      const fb = current.feedbacks.find((f) => f.id === feedbackId);
      if (!fb) return current;
      const id = nextLocalId("task-fb");
      taskId = id;
      const links: StudioRef[] = [
        { kind: "feedback", id: fb.id },
        { kind: "report", id: fb.reportId },
        { kind: "clue", id: fb.findingId },
      ];
      const next = createTask(current, {
        projectId: fb.projectId,
        title: `跟进平台反馈 ${fb.id} (${fb.externalTicketId})：${fb.rawText.slice(0, 28)}…`,
        assignee: STUDIO_TEAM[1],
        links,
        threadId: current.activeThreadId,
        originKind: "feedback",
        notes: fb.rawText,
      }, id);
      return withHistory({
        ...next,
        feedbacks: next.feedbacks.map((f) => (f.id === feedbackId ? { ...f, status: "in_progress", followUpTaskId: id } : f)),
      }, {
        action: "task",
        summary: `根据平台反馈 ${fb.id} 创建跟进任务「${id}」`,
        refs: [{ kind: "task", id }, ...links],
        createdRefs: [{ kind: "task", id }],
      });
    });
    return taskId;
  },

  recordRetest(input: {
    findingId: string;
    baseReportId: string;
    newReportId?: string;
    feedbackId?: string;
    targetFixVersion: string;
    result: RetestVerdict;
    prerequisiteCheck: string;
    normalControlCheck: string;
    basisAndEvidence: string;
    evidenceIds: string[];
  }): string {
    let retestId = "";
    setState((current) => {
      retestId = `RETEST-00${current.retests.length + 1}`;
      const rec: StudioRetestRecord = {
        id: retestId,
        projectId: STUDIO_DEFAULT_PROJECT_ID,
        findingId: input.findingId,
        baseReportId: input.baseReportId,
        newReportId: input.newReportId,
        feedbackId: input.feedbackId,
        runId: "demo-run-046",
        targetFixVersion: input.targetFixVersion.trim() || "snapshot-v47",
        result: input.result,
        prerequisiteCheck: input.prerequisiteCheck.trim(),
        normalControlCheck: input.normalControlCheck.trim(),
        basisAndEvidence: input.basisAndEvidence.trim(),
        evidenceIds: input.evidenceIds.length ? input.evidenceIds : ["EX-014"],
        reviewer: "人工复核组 · 林工 (demo-reviewer)",
        reviewedAt: localDate(),
      };
      return withHistory({
        ...current,
        retests: [rec, ...current.retests],
        feedbacks: input.feedbackId
          ? current.feedbacks.map((f) => (f.id === input.feedbackId ? { ...f, status: "resolved" } : f))
          : current.feedbacks,
        workspace: { ...current.workspace, reports: { ...current.workspace.reports, tab: "retests", selectedRetestId: retestId } },
      }, {
        action: "retest",
        summary: `登记发现 ${input.findingId} 后续复核记录（${retestId}）：结论「${retestVerdictLabel(input.result)}」`,
        scope: `适用版本：${rec.targetFixVersion}`,
        refs: [
          { kind: "clue", id: input.findingId },
          { kind: "report", id: input.baseReportId },
          ...(input.feedbackId ? [{ kind: "feedback" as const, id: input.feedbackId }] : []),
        ],
      });
    });
    return retestId;
  },

  // -------------------------------------------------------------------------
  // Module 12: Changes & Continuous Monitoring
  // -------------------------------------------------------------------------

  toggleMonitorStatus(monitorId: string) {
    setState((current) => {
      const mon = current.monitors.find((m) => m.id === monitorId);
      if (!mon) return current;
      const nextStatus = mon.status === "active" ? "paused" : "active";
      return withHistory({
        ...current,
        monitors: current.monitors.map((m) => (m.id === monitorId ? { ...m, status: nextStatus } : m)),
      }, {
        action: "monitor",
        summary: `${nextStatus === "active" ? "恢复" : "暂停"}持续关注「${mon.name}」（已提交历史轮次与基线保留）`,
        refs: [],
      });
    });
  },

  runMonitorCycle(monitorId: string, simulateMode: "completed" | "partial" | "failed" = "completed"): string | null {
    let changeId: string | null = null;
    setState((current) => {
      const mon = current.monitors.find((m) => m.id === monitorId);
      if (!mon) return current;
      const cycleNum = mon.cycles.length + 45;
      const cycleId = `${mon.chainType === "rule" ? "RULE-CYCLE" : "CYCLE"}-0${cycleNum}`;
      const now = localDate();
      const cost = simulateMode === "failed" ? 0 : mon.budgetPerCycle;
      const newCycle: StudioMonitor["cycles"][number] =
        simulateMode === "completed"
          ? {
              id: cycleId,
              startedAt: now,
              finishedAt: now,
              status: "completed",
              summary: `与有效基线 ${mon.baselineCycleId} 对照完成：发现 2 处属性/条款增量变化，推进可比基线至 ${cycleId}`,
              added: 1,
              changed: 2,
              costUsed: cost,
              advancedBaseline: true,
            }
          : simulateMode === "partial"
            ? {
                id: cycleId,
                startedAt: now,
                finishedAt: now,
                status: "partial",
                summary: `局部完成（第 3 页上游响应超时）：已保存前 2 页观测，不将缺页项报告为资产删除，不推进完整基线`,
                added: 1,
                changed: 0,
                missingPages: 1,
                failureReason: "分页游标 page=3 超时 (504)",
                costUsed: Math.round(cost * 0.6),
                advancedBaseline: false,
              }
            : {
                id: cycleId,
                startedAt: now,
                finishedAt: now,
                status: "failed",
                summary: `采集失败（上游凭据/限流 429）：不刷新最近成功时间，不伪装为「无变化」`,
                added: 0,
                changed: 0,
                failureReason: "外部数据源返回 429 Too Many Requests",
                costUsed: 0,
                advancedBaseline: false,
              };

      let nextChanges = current.changes;
      if (simulateMode !== "failed") {
        const cId = `CHG-${cycleId}`;
        changeId = cId;
        const newChange: StudioChangeRecord = {
          id: cId,
          projectId: mon.projectId,
          monitorId: mon.id,
          cycleId,
          chainType: mon.chainType,
          changeKind: simulateMode === "partial" ? "incomparable" : mon.chainType === "rule" ? "rule_updated" : "modified",
          title: `${cId} · ${mon.name} 第 ${cycleNum} 轮${simulateMode === "partial" ? "局部不可比记录" : "增量变化"}`,
          observedAt: now,
          beforeVersion: `基线 ${mon.baselineCycleId}`,
          afterVersion: `${cycleId} (${now})`,
          baselineCycleId: mon.baselineCycleId,
          comparable: simulateMode === "completed",
          incomparableReason: simulateMode === "partial" ? "本轮存在 1 页超时缺失，仅记录增量观察，不触发下线判断且不推进基线。" : undefined,
          rawDiff: `--- ${mon.baselineCycleId}\n+++ ${cycleId}\n@@ -1,3 +1,4 @@\n+X-Trace-Id: trace-${cycleId}\n+observed_delta: "${mon.chainType === "rule" ? "新增测试速率说明条款" : "A-0887 网关新增 /api/v2/tenant/audit 只读端点"}"`,
          denoisedDiff: `已忽略动态时间戳与 Trace-Id：检出 ${mon.chainType === "rule" ? "范围规则条款补充" : "A-0887 端点列表新增 /api/v2/tenant/audit"}`,
          ignoredNoiseFields: ["Date", "X-Trace-Id"],
          affectedAssetIds: ["A-0887"],
          affectedFindingIds: ["F-052"],
          affectedReportIds: ["R-052"],
          reviewCandidateStatus: "pending",
        };
        nextChanges = [newChange, ...current.changes];
      }

      return withHistory({
        ...current,
        monitors: current.monitors.map((m) =>
          m.id === monitorId
            ? {
                ...m,
                budgetUsed: m.budgetUsed + newCycle.costUsed,
                baselineCycleId: newCycle.advancedBaseline ? cycleId : m.baselineCycleId,
                lastSuccessAt: simulateMode === "completed" ? now : m.lastSuccessAt,
                cycles: [newCycle, ...m.cycles],
              }
            : m,
        ),
        changes: nextChanges,
      }, {
        action: "monitor",
        summary: `执行持续关注「${mon.name}」轮次 ${cycleId}：结果「${simulateMode === "completed" ? "完整完成" : simulateMode === "partial" ? "局部缺页" : "失败"}」`,
        refs: changeId ? [{ kind: "change", id: changeId }] : [],
      });
    });
    return changeId;
  },

  createMonitor(input: {
    projectId: string;
    name: string;
    chainType: "asset" | "rule";
    queryExpression: string;
    frequencyLabel: string;
    budgetTotal: number;
  }): string {
    const id = `MON-00${state.monitors.length + 1}`;
    setState((current) => {
      const now = localDate();
      const baseCycleId = `CYCLE-INIT-${current.monitors.length + 1}`;
      const newMon: StudioMonitor = {
        id,
        projectId: input.projectId,
        name: input.name.trim(),
        chainType: input.chainType,
        status: "active",
        queryExpression: input.queryExpression.trim(),
        queryVersion: "v1 (固定于 PLAN-046)",
        frequencyLabel: input.frequencyLabel.trim() || "每 24 小时",
        intervalSeconds: 86400,
        budgetPerCycle: 50,
        budgetTotal: input.budgetTotal || 500,
        budgetUsed: 50,
        baselineCycleId: baseCycleId,
        lastSuccessAt: now,
        cycles: [
          {
            id: baseCycleId,
            startedAt: now,
            finishedAt: now,
            status: "completed",
            summary: "显式创建持续关注并完成首个完整可比基线轮次",
            added: 12,
            changed: 0,
            costUsed: 50,
            advancedBaseline: true,
          },
        ],
      };
      return withHistory({
        ...current,
        monitors: [newMon, ...current.monitors],
        workspace: { ...current.workspace, tasks: { ...current.workspace.tasks, tab: "monitors", selectedMonitorId: id } },
      }, {
        action: "monitor",
        summary: `显式创建持续关注「${newMon.name} (${id})」，建立初始可比基线 ${baseCycleId}`,
        refs: [],
      });
    });
    return id;
  },

  // -------------------------------------------------------------------------
  // Module 13: Scope, Approvals, Grants & Environment Status
  // -------------------------------------------------------------------------

  decideApproval(approvalId: string, decision: "approve_once" | "approve_grant" | "deny", reason = "") {
    setState((current) => {
      const app = current.approvals.find((a) => a.id === approvalId);
      if (!app || app.status !== "pending") return current;
      const now = localDate();
      let newGrantId: string | undefined;
      let nextGrants = current.grants;

      if (decision === "approve_grant") {
        newGrantId = `GRANT-00${current.grants.length + 1}`;
        const grant: StudioGrant = {
          id: newGrantId,
          projectId: app.projectId,
          ruleId: app.matchedRuleId,
          resourceSelector: `${app.resourceKind}: ${app.target}`,
          actions: [app.action],
          identityConstraint: app.identity,
          sourceApprovalId: app.id,
          createdBy: "资料负责人（演示）",
          createdAt: now,
          expiresAt: "2026-12-31 23:59",
          status: "active",
        };
        nextGrants = [grant, ...current.grants];
      }

      const nextStatus: StudioApproval["status"] =
        decision === "approve_once" ? "approved_once" : decision === "approve_grant" ? "approved_grant" : "denied";

      const nextApprovals = current.approvals.map((a) =>
        a.id === approvalId
          ? {
              ...a,
              status: nextStatus,
              decidedBy: "资料负责人（演示）",
              decidedAt: now,
              decisionReason: reason.trim() || (decision === "deny" ? "超出当前阶段测试边界，人工拒绝" : "核对目标、身份与输入摘要无误，人工批准"),
              grantId: newGrantId,
            }
          : a,
      );

      // If all pending approvals for the linked run are now resolved and approved, unblock the run!
      const remainingPendingForRun = app.runId
        ? nextApprovals.some((a) => a.runId === app.runId && a.status === "pending")
        : false;
      const nextRuns = current.workRuns.map((r) => {
        if (r.id !== app.runId) return r;
        if (decision === "deny") {
          return {
            ...r,
            state: "blocked" as const,
            blockerReason: `权限请求 ${app.id} 被人工拒绝（${reason.trim() || "未获授权"}），受管动作未下发。`,
          };
        }
        if (!remainingPendingForRun && r.state === "waiting_approval") {
          return {
            ...r,
            state: "running" as const,
            blockerReason: undefined,
            logs: [
              ...r.logs,
              {
                at: new Date().toTimeString().slice(0, 8),
                step: `审批 ${app.id} 已通过（${decision === "approve_grant" ? `已保存本任务 Grant ${newGrantId}` : "仅批准本次固定输入"}）`,
                status: "ok" as const,
                detail: `原位恢复受管调用 ${app.toolName} (${app.target})。`,
              },
            ],
          };
        }
        return r;
      });

      return withHistory({
        ...current,
        approvals: nextApprovals,
        grants: nextGrants,
        workRuns: nextRuns,
      }, {
        action: "approval",
        summary: `处理权限确认「${app.id} (${app.toolName})」：${decision === "approve_once" ? "批准此次固定请求" : decision === "approve_grant" ? `批准并保存本任务范围授权 (${newGrantId})` : "拒绝执行"}`,
        scope: `目标：${app.target} · 身份：${app.identity}`,
        refs: app.runId ? [{ kind: "run", id: app.runId }] : [],
      });
    });
  },

  revokeGrant(grantId: string, reason = "人工主动撤销本任务范围授权") {
    setState((current) => {
      const grant = current.grants.find((g) => g.id === grantId);
      if (!grant || grant.status !== "active") return current;
      const now = localDate();
      return withHistory({
        ...current,
        grants: current.grants.map((g) => (g.id === grantId ? { ...g, status: "revoked", revokedAt: now, revokedReason: reason } : g)),
      }, {
        action: "approval",
        summary: `撤销本任务范围授权「${grant.id}」（后续同类动作将重新触发确认，不改写已完成历史回执）`,
        scope: `原适用范围：${grant.resourceSelector}`,
        refs: [],
      });
    });
  },

  updateEnvironmentMode(patch: Partial<Pick<StudioEnvironmentStatus, "networkScopeMode" | "httpInspectionMode">>) {
    setState((current) => ({
      ...current,
      environmentStatus: { ...current.environmentStatus, ...patch },
    }));
  },

  // -------------------------------------------------------------------------
  // Server Sync & Cross-device Archive
  // -------------------------------------------------------------------------

  syncToServerNow() {
    setState((current) => ({
      ...current,
      serverSync: {
        ...current.serverSync,
        lastSyncedAt: new Date().toISOString(),
        syncRev: current.serverSync.syncRev + 1,
      },
    }));
  },
};

// ---------------------------------------------------------------------------
// Real-domain seam: server-backed field ownership on real SRC tasks
// ---------------------------------------------------------------------------

import { toast } from "@/components/chat/ui/Toast";
import type { DomainCtx, RealDomain, StudioMutatorName } from "./real/domain";
import { combineOwnedField, setOwnedFields } from "./real/owned";

interface AttachedDomain {
  domain: RealDomain;
  ctx: DomainCtx;
}

const attached: AttachedDomain[] = [];
const originals = new Map<string, (...args: unknown[]) => unknown>();
const queues = new Map<string, Promise<void>>();
let pollTimer: ReturnType<typeof setInterval> | null = null;
let focusListener: (() => void) | null = null;

/** Every field the domain replaces or combines on reload (used for optimistic rollback). */
function ownedKeys(domain: RealDomain): Array<keyof StudioState> {
  return [...domain.owns, ...Object.keys(domain.combines ?? {})] as Array<keyof StudioState>;
}

function pick(current: StudioState, keys: Array<keyof StudioState>): Partial<StudioState> {
  const out: Partial<StudioState> = {};
  for (const key of keys) (out as Record<string, unknown>)[key] = current[key];
  return out;
}

function remapRefs(refs: StudioRef[], idMap: Record<string, string>): StudioRef[] {
  return refs.map((ref) => (idMap[ref.id] ? { ...ref, id: idMap[ref.id] } : ref));
}

/** Rewrites local temporary ids to the ids the server assigned after a mutator ran. */
function applyIdMap(idMap: Record<string, string>) {
  const remap = (value: string) => idMap[value] ?? value;
  setState((current) => ({
    ...current,
    context: Object.fromEntries(Object.entries(current.context).map(([key, refs]) => [key, remapRefs(refs, idMap)])),
    viewing: current.viewing && idMap[current.viewing.id] ? { ...current.viewing, id: idMap[current.viewing.id] } : current.viewing,
    recent: remapRefs(current.recent, idMap),
    updated: remapRefs(current.updated, idMap),
    workspace: {
      ...current.workspace,
      library: {
        ...current.workspace.library,
        sourceId: remap(current.workspace.library.sourceId),
        clueId: remap(current.workspace.library.clueId),
        importBatchId: remap(current.workspace.library.importBatchId),
        checked: current.workspace.library.checked.map(remap),
      },
      findings: {
        ...current.workspace.findings,
        selectedId: remap(current.workspace.findings.selectedId),
        checked: current.workspace.findings.checked.map(remap),
        focusIds: current.workspace.findings.focusIds?.map(remap) ?? null,
      },
    },
  }));
}

async function reloadAttached(domainId?: string) {
  for (const item of attached) {
    if (domainId && item.domain.id !== domainId) continue;
    try {
      const partial = await item.domain.load(item.ctx.taskId, item.ctx);
      setState((current) => {
        const next = { ...current } as Record<string, unknown>;
        for (const [key, value] of Object.entries(partial)) {
          next[key] = combineOwnedField(key as keyof StudioState, current[key as keyof StudioState], value, item.domain.id);
        }
        return next as unknown as StudioState;
      });
    } catch (cause) {
      const message = cause instanceof Error ? cause.message : String(cause);
      setState((current) => ({ ...current, effectError: { message, code: "domain_load_failed", domain: item.domain.id, at: new Date().toISOString() } }));
    }
  }
}

function enqueueDomain(item: AttachedDomain, mutator: (ctx: DomainCtx, ...args: unknown[]) => Promise<void | { idMap?: Record<string, string> }>, args: unknown[], snapshot: Partial<StudioState>) {
  const run = (queues.get(item.domain.id) ?? Promise.resolve()).then(async () => {
    try {
      const outcome = await mutator(item.ctx, ...args);
      await reloadAttached(item.domain.id);
      if (outcome?.idMap) applyIdMap(outcome.idMap);
      setState((current) => ({ ...current, effectError: null }));
    } catch (cause) {
      const err = cause as { message?: string; code?: string };
      setState((current) => ({
        ...current,
        ...snapshot,
        effectError: {
          message: err.message || String(cause),
          code: err.code || "domain_effect_failed",
          domain: item.domain.id,
          at: new Date().toISOString(),
        },
      }));
      toast({ title: "操作未能同步到服务器", description: `${err.message || cause}${err.code ? ` [${err.code}]` : ""}`, tone: "danger" });
    }
  });
  queues.set(item.domain.id, run.catch(() => {}));
}

/**
 * Attach real domains to this store (real-task host only). Registered
 * mutators keep their local reducer for optimistic UI, then run the domain's
 * server effect on a serial per-domain queue; failures roll the owned keys
 * back and surface `effectError`. Unregistered mutators stay local.
 */
function attachDomains(input: { taskId: string; threadId: string; actor: string; domains: RealDomain[] }) {
  detachDomains();
  const ctx: DomainCtx = { taskId: input.taskId, threadId: input.threadId, actor: input.actor, state: () => state };
  const replace: Array<keyof StudioState> = [];
  const combine: Record<string, Record<string, (local: unknown, server: unknown) => unknown>> = {};
  const mutatorMap = new Map<string, Array<{ item: AttachedDomain; mutator: (ctx: DomainCtx, ...args: unknown[]) => Promise<void | { idMap?: Record<string, string> }>; keys: Array<keyof StudioState> }>>();
  for (const domain of input.domains) {
    const item: AttachedDomain = { domain, ctx };
    attached.push(item);
    replace.push(...domain.owns);
    for (const [key, merge] of Object.entries(domain.combines ?? {})) {
      (combine[key] ??= {})[domain.id] = merge as (local: unknown, server: unknown) => unknown;
    }
    for (const [name, mutator] of Object.entries(domain.mutators)) {
      const list = mutatorMap.get(name) ?? [];
      list.push({ item, mutator: mutator as (ctx: DomainCtx, ...args: unknown[]) => Promise<void | { idMap?: Record<string, string> }>, keys: ownedKeys(domain) });
      mutatorMap.set(name, list);
    }
  }
  const record = studio as unknown as Record<string, (...args: unknown[]) => unknown>;
  for (const [name, handlers] of mutatorMap) {
    const mutatorName = name as StudioMutatorName;
    if (!originals.has(name)) originals.set(name, record[mutatorName]);
    const keys = [...new Set(handlers.flatMap((h) => h.keys))];
    record[mutatorName] = (...args: unknown[]) => {
      // One optimistic snapshot for every domain backing this mutator.
      const snapshot = pick(state, keys);
      const result = originals.get(name)!.apply(studio, args);
      for (const h of handlers) void enqueueDomain(h.item, h.mutator, args, snapshot);
      return result;
    };
  }
  setOwnedFields({ replace: [...new Set(replace)], combines: combine as never });
  void reloadAttached();
  const minPoll = Math.min(...input.domains.map((d) => d.pollMs ?? 30000));
  pollTimer = setInterval(() => void reloadAttached(), minPoll);
  focusListener = () => void reloadAttached();
  window.addEventListener("focus", focusListener);
}

function detachDomains() {
  const record = studio as unknown as Record<string, (...args: unknown[]) => unknown>;
  for (const [name, original] of originals) record[name] = original;
  originals.clear();
  attached.length = 0;
  queues.clear();
  setOwnedFields(null);
  if (pollTimer) { clearInterval(pollTimer); pollTimer = null; }
  if (focusListener) { window.removeEventListener("focus", focusListener); focusListener = null; }
}

(studio as Record<string, unknown>).attachDomains = attachDomains;
(studio as Record<string, unknown>).detachDomains = detachDomains;
(studio as Record<string, unknown>).reloadDomains = () => reloadAttached();

// ---------------------------------------------------------------------------
// Conversation preview adapters
// ---------------------------------------------------------------------------

export function previewThread(record: StudioThreadRecord): ConversationThread {
  return {
    thread_id: record.id,
    title: record.title,
    title_source: "user",
    mode: "conversation",
    created_at: record.createdAt,
    updated_at: record.updatedAt,
    state: { status: "idle" },
  };
}

export function previewView(record: StudioThreadRecord): ConversationView {
  const last = record.messages.at(-1);
  return {
    thread: {
      thread_id: record.id,
      title: record.title,
      title_source: "user",
      mode: "conversation",
      created_at: record.createdAt,
      updated_at: record.updatedAt,
    },
    runtime: { adapter_id: "", instance_id: "" },
    state: { status: "idle", plan: record.plan },
    messages: record.messages.map((m) => (m.role === "assistant" && typeof m.text === "string" ? { ...m, text: cleanDemoText(m.text) } : m)),
    turns: record.turns,
    queue: [],
    messages_page: {
      limit: 50,
      has_more_before: false,
      has_more_after: false,
      newest_stream_seq: record.seq,
      newest_message_id: last?.message_id ?? null,
    },
    watermark: record.seq,
    event_watermark: record.seq,
  };
}
