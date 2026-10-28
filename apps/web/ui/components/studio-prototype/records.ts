/**
 * Shapes and pure helpers for managing many records: filters and saved
 * filters, duplicate detection, merges and the processing history. Nothing
 * here touches the store, so the store and every view can share it.
 */

import type {
  AssetImportance,
  ClueCategory,
  ClueStatus,
  FindingSeverity,
  ReadinessLevel,
  SourceKind,
  StudioApproval,
  WorkRunState,
  StudioClue,
  StudioRef,
  StudioSource,
  StudioTask,
} from "./model";

export type RecordKind = "source" | "clue";
/** What a workspace shows in place of its list. */
export type RecordView = "list" | "duplicates" | "history" | "evidences";

export const RECORD_NOUN: Record<RecordKind, { unit: string; noun: string }> = {
  source: { unit: "份", noun: "资产" },
  clue: { unit: "条", noun: "发现" },
};

export function countLabel(kind: RecordKind, count: number): string {
  return `${count} ${RECORD_NOUN[kind].unit}${RECORD_NOUN[kind].noun}`;
}

export const SOURCE_STATUSES: Array<{ value: StudioSource["status"]; label: string }> = [
  { value: "inbox", label: "待整理" },
  { value: "filed", label: "已入库" },
];

export function sourceStatusLabel(status: StudioSource["status"]): string {
  return SOURCE_STATUSES.find((item) => item.value === status)?.label ?? status;
}

// ---------------------------------------------------------------------------
// Filters
// ---------------------------------------------------------------------------

export type FindingsPresence = "all" | "with" | "open" | "without";
export type FilterTimeRange = "all" | "24h" | "7d" | "30d" | "stale";

export interface SourceFilter {
  query: string;
  kind: "all" | SourceKind;
  status: "all" | StudioSource["status"];
  /** Whether the source has findings, or findings still to verify. */
  findings: FindingsPresence;
  /** "all", "manual", "imported", or the id of one import batch. */
  origin: string;
  /** Lists sources merged into another one, which are hidden by default. */
  showMerged: boolean;
  /** Observation time / freshness filter. */
  timeRange: FilterTimeRange;
  /** Asset importance level. */
  importance: "all" | AssetImportance;
  /** Asset tag filter. */
  tag: string;
  /** Whether linked to any report. */
  report: "all" | "with" | "without";
  /** Linked work record (Run) id; "" for any. */
  runId: string;
  /** Field conflict presence. */
  conflict: "all" | "with" | "unresolved";
  /** Lifecycle visibility: active only, archived, unlinked from task, or all. */
  lifecycle: "active" | "archived" | "unlinked" | "all";
}

export const EMPTY_SOURCE_FILTER: SourceFilter = {
  query: "",
  kind: "all",
  status: "all",
  findings: "all",
  origin: "all",
  showMerged: false,
  timeRange: "all",
  importance: "all",
  tag: "",
  report: "all",
  runId: "",
  conflict: "all",
  lifecycle: "active",
};

export interface ClueFilter {
  query: string;
  category: "all" | ClueCategory;
  status: "all" | ClueStatus;
  tag: string;
  /** Source the findings were taken from; "" for any. */
  sourceId: string;
  task: "all" | "with" | "without";
  showMerged: boolean;
  /** Finding importance / severity filter. */
  severity: "all" | FindingSeverity;
  /** Delivery readiness filter. */
  readiness: "all" | ReadinessLevel;
  /** Time range filter. */
  timeRange: FilterTimeRange;
  /** Linked report presence / state. */
  report: "all" | "with" | "frozen" | "without";
  /** Linked work record (Run) id; "" for any. */
  runId: string;
  /** Stale status filter. */
  stale: "all" | "stale" | "fresh";
}

export const EMPTY_CLUE_FILTER: ClueFilter = {
  query: "",
  category: "all",
  status: "all",
  tag: "",
  sourceId: "",
  task: "all",
  showMerged: false,
  severity: "all",
  readiness: "all",
  timeRange: "all",
  report: "all",
  runId: "",
  stale: "all",
};

/** The filter fields of a wider object, such as the library view state. */
export function pickSourceFilter(value: SourceFilter): SourceFilter {
  return {
    query: value.query,
    kind: value.kind,
    status: value.status,
    findings: value.findings,
    origin: value.origin,
    showMerged: value.showMerged,
    timeRange: value.timeRange ?? "all",
    importance: value.importance ?? "all",
    tag: value.tag ?? "",
    report: value.report ?? "all",
    runId: value.runId ?? "",
    conflict: value.conflict ?? "all",
    lifecycle: value.lifecycle ?? "active",
  };
}

export function pickClueFilter(value: ClueFilter): ClueFilter {
  return {
    query: value.query,
    category: value.category,
    status: value.status,
    tag: value.tag,
    sourceId: value.sourceId,
    task: value.task,
    showMerged: value.showMerged,
    severity: value.severity ?? "all",
    readiness: value.readiness ?? "all",
    timeRange: value.timeRange ?? "all",
    report: value.report ?? "all",
    runId: value.runId ?? "",
    stale: value.stale ?? "all",
  };
}

export function sameFilter<T extends object>(a: T, b: T): boolean {
  const left = a as Record<string, unknown>;
  const right = b as Record<string, unknown>;
  return Object.keys({ ...left, ...right }).every((key) => left[key] === right[key]);
}

export type SavedFilter =
  | { id: string; scope: "library"; name: string; createdAt: string; filter: SourceFilter }
  | { id: string; scope: "findings"; name: string; createdAt: string; filter: ClueFilter };

// ---------------------------------------------------------------------------
// Merges, Undetermined Duplicates, and History
// ---------------------------------------------------------------------------

/**
 * Duplicates folded into one primary record. Nothing about the members
 * changes: they keep their source, passage, quote, tasks and chat
 * references, and undoing the merge only lists them again.
 */
export interface MergeRecord {
  id: string;
  kind: RecordKind;
  projectId: string;
  primaryId: string;
  memberIds: string[];
  /** Why the records were merged, as the user wrote it. */
  reason: string;
  /** What the duplicate check found, when the merge started from it. */
  detected: string;
  at: string;
  undoneAt?: string;
}

/**
 * A duplicate candidate group explicitly marked as "无法判断" (undetermined)
 * with a reason / missing evidence note so it can be revisited after verification.
 */
export interface UndeterminedDuplicate {
  key: string;
  kind: RecordKind;
  projectId: string;
  ids: string[];
  reason: string;
  missingNote: string;
  at: string;
  by: string;
}

/** One field of one record changed by a history entry; undo restores `from`. */
export interface FieldChange {
  kind: RecordKind;
  id: string;
  field: "status" | "category" | "kind" | "severity" | "readiness" | "importance" | "archived";
  from: string;
  to: string;
}

export interface TagChange {
  kind: RecordKind;
  id: string;
  from: string[];
  to: string[];
}

export type HistoryAction =
  | "create"
  | "import"
  | "status"
  | "category"
  | "merge"
  | "unmerge"
  | "dismiss"
  | "undetermined"
  | "revert"
  | "adopt"
  | "task"
  | "delete"
  | "tag"
  | "link"
  | "archive"
  | "conflict"
  | "report"
  | "approval"
  | "monitor"
  | "retest"
  | "plan";

export interface HistoryEntry {
  id: string;
  at: string;
  action: HistoryAction;
  /** One line, in words shown to the user. */
  summary: string;
  /** What the action covered or why it was taken, as stated at the time. */
  scope?: string;
  refs: StudioRef[];
  changes?: FieldChange[];
  tagChanges?: TagChange[];
  mergeId?: string;
  batchId?: string;
  /** `duplicateKey` of a group marked as not duplicates. */
  dismissKey?: string;
  /** `duplicateKey` of a group marked as undetermined. */
  undeterminedKey?: string;
  /** Created record ids that can be removed on undo. */
  createdRefs?: StudioRef[];
  /** Deleted records snapshot that can be restored on undo. */
  deletedSnapshot?: {
    sources?: StudioSource[];
    clues?: StudioClue[];
    tasks?: StudioTask[];
  };
  /** Id of the entry that undid this one. */
  revertedBy?: string;
  /** Server-side record id when this entry is backed by the real backend. */
  serverId?: string;
  /** Which real domain owns this entry's server record. */
  serverDomain?: string;
  /** Server-side undo instructions recorded for real-domain undo. */
  serverUndo?: { op: string; params?: Record<string, unknown> };
}

const memberCache = new WeakMap<MergeRecord[], Map<string, MergeRecord>>();

function memberIndex(merges: MergeRecord[]): Map<string, MergeRecord> {
  let index = memberCache.get(merges);
  if (!index) {
    index = new Map();
    for (const merge of merges) {
      if (merge.undoneAt) continue;
      for (const id of merge.memberIds) index.set(`${merge.kind}:${id}`, merge);
    }
    memberCache.set(merges, index);
  }
  return index;
}

/** The active merge that folded this record into another one. */
export function mergeOf(merges: MergeRecord[], kind: RecordKind, id: string): MergeRecord | undefined {
  return memberIndex(merges).get(`${kind}:${id}`);
}

/** Active merges whose primary is this record. */
export function mergesInto(merges: MergeRecord[], kind: RecordKind, id: string): MergeRecord[] {
  return merges.filter((merge) => !merge.undoneAt && merge.kind === kind && merge.primaryId === id);
}

/** The record that finally represents `id`, following merges of merges. */
export function finalPrimary(merges: MergeRecord[], kind: RecordKind, id: string): string {
  let current = id;
  for (let step = 0; step < 20; step += 1) {
    const merge = mergeOf(merges, kind, current);
    if (!merge) return current;
    current = merge.primaryId;
  }
  return current;
}

const historyCache = new WeakMap<HistoryEntry[], Map<string, HistoryEntry[]>>();

/** Entries that touched `ref`, newest first. */
export function historyFor(history: HistoryEntry[], ref: StudioRef): HistoryEntry[] {
  let index = historyCache.get(history);
  if (!index) {
    index = new Map();
    for (const entry of history) {
      const seen = new Set<string>();
      for (const item of entry.refs) {
        const key = `${item.kind}:${item.id}`;
        if (seen.has(key)) continue;
        seen.add(key);
        const list = index.get(key);
        if (list) list.push(entry);
        else index.set(key, [entry]);
      }
    }
    historyCache.set(history, index);
  }
  return index.get(`${ref.kind}:${ref.id}`) ?? [];
}

// ---------------------------------------------------------------------------
// Duplicates & AI Merge Suggestions
// ---------------------------------------------------------------------------

export interface DuplicateGroup {
  /** `duplicateKey` of the members. */
  key: string;
  kind: RecordKind;
  projectId: string;
  ids: string[];
  reasons: string[];
  /** Present when the user marked this group as "无法判断". */
  undetermined?: UndeterminedDuplicate;
}

export interface AiMergeSuggestion {
  id: string;
  groupKey: string;
  kind: RecordKind;
  projectId: string;
  primaryId: string;
  memberIds: string[];
  confidence: "high" | "medium";
  rationale: string;
  sharedRootCause: string;
  fixBoundary: string;
  adoptedMergeId?: string;
  disposition?: "adopted" | "independent" | "undetermined";
}

export function duplicateKey(kind: RecordKind, ids: string[]): string {
  return `${kind}:${[...ids].sort().join(",")}`;
}

const PUNCTUATION = /[\s.,;:!?'"()[\]{}<>\-_/\\|~`·，。、；：？！“”‘’（）《》〈〉【】「」『』…—]+/g;

/** Text compared for duplicates: case, spaces and punctuation ignored. */
export function normalizeForCompare(text: string): string {
  return text.toLowerCase().replace(PUNCTUATION, "");
}

/** Short fingerprint of long text, so whole bodies can be compared through a map. */
function fingerprint(text: string): string {
  let a = 0x811c9dc5;
  let b = 0x2545f491;
  for (let index = 0; index < text.length; index += 1) {
    const code = text.charCodeAt(index);
    a = Math.imul(a ^ code, 16777619);
    b = Math.imul(b ^ code, 0x5bd1e995);
    b ^= b >>> 13;
  }
  return `${text.length}:${(a >>> 0).toString(36)}:${(b >>> 0).toString(36)}`;
}

function clip(text: string, length: number): string {
  return text.length > length ? `${text.slice(0, length)}…` : text;
}

/** Union-find over record ids that remembers why records were joined. */
class DuplicateSets {
  private parent = new Map<string, string>();
  private links: Array<{ ids: string[]; reason: string }> = [];

  private find(id: string): string {
    let root = id;
    while (this.parent.has(root) && this.parent.get(root) !== root) root = this.parent.get(root) as string;
    let node = id;
    while (node !== root) {
      const next = this.parent.get(node) as string;
      this.parent.set(node, root);
      node = next;
    }
    return root;
  }

  link(ids: string[], reason: string) {
    if (ids.length < 2) return;
    ids.forEach((id) => { if (!this.parent.has(id)) this.parent.set(id, id); });
    const root = this.find(ids[0]);
    ids.slice(1).forEach((id) => {
      const other = this.find(id);
      if (other !== root) this.parent.set(other, root);
    });
    this.links.push({ ids, reason });
  }

  groups(
    kind: RecordKind,
    order: Array<{ id: string; projectId: string }>,
    dismissed: Set<string>,
    undeterminedMap?: Record<string, UndeterminedDuplicate>,
  ): DuplicateGroup[] {
    const members = new Map<string, string[]>();
    order.forEach((item) => {
      if (!this.parent.has(item.id)) return;
      const root = this.find(item.id);
      const list = members.get(root);
      if (list) list.push(item.id);
      else members.set(root, [item.id]);
    });
    const reasons = new Map<string, string[]>();
    this.links.forEach((link) => {
      const root = this.find(link.ids[0]);
      const list = reasons.get(root) ?? [];
      if (!list.includes(link.reason)) list.push(link.reason);
      reasons.set(root, list);
    });
    const projects = new Map(order.map((item) => [item.id, item.projectId]));
    const out: DuplicateGroup[] = [];
    members.forEach((ids, root) => {
      if (ids.length < 2) return;
      const key = duplicateKey(kind, ids);
      if (dismissed.has(key)) return;
      out.push({
        key,
        kind,
        projectId: projects.get(ids[0]) ?? "",
        ids,
        reasons: reasons.get(root) ?? [],
        undetermined: undeterminedMap?.[key],
      });
    });
    return out.sort((a, b) => b.ids.length - a.ids.length);
  }
}

function bucket<T extends { id: string; projectId: string }>(items: T[], key: (item: T) => string): Map<string, T[]> {
  const out = new Map<string, T[]>();
  items.forEach((item) => {
    const value = key(item);
    if (!value) return;
    const id = `${item.projectId}\u0000${value}`;
    const list = out.get(id);
    if (list) list.push(item);
    else out.set(id, [item]);
  });
  return out;
}

const sourceMemo: {
  sources?: StudioSource[];
  merges?: MergeRecord[];
  dismissed?: string[];
  undetermined?: Record<string, UndeterminedDuplicate>;
  result: DuplicateGroup[];
} = { result: [] };

/**
 * Sources of the same project that look like the same material: the same
 * title, the same body text, or overlapping alias / canonicalKey.
 */
export function findSourceDuplicates(
  sources: StudioSource[],
  merges: MergeRecord[],
  dismissed: string[],
  undetermined?: Record<string, UndeterminedDuplicate>,
): DuplicateGroup[] {
  if (
    sourceMemo.sources === sources &&
    sourceMemo.merges === merges &&
    sourceMemo.dismissed === dismissed &&
    sourceMemo.undetermined === undetermined
  ) {
    return sourceMemo.result;
  }
  const live = sources.filter((source) => !mergeOf(merges, "source", source.id) && !source.archived);
  const sets = new DuplicateSets();
  bucket(live, (source) => {
    const title = normalizeForCompare(source.title);
    return title.length >= 2 ? title : "";
  }).forEach((items) => sets.link(items.map((item) => item.id), `标题相同「${clip(items[0].title, 24)}」`));
  bucket(live, (source) => {
    const body = normalizeForCompare(source.passages.map((passage) => passage.text).join(""));
    return body.length >= 12 ? fingerprint(body) : "";
  }).forEach((items) => sets.link(items.map((item) => item.id), "正文相同"));
  // Also link demo alias / service cluster overlaps (e.g. A-0887 & A-0890 sharing backend gateway)
  bucket(live, (source) => source.attributes?.["同源网关组"] ?? "").forEach((items) => {
    if (items.length >= 2) {
      sets.link(items.map((item) => item.id), `共享后端服务网关与证书指纹（${items[0].attributes?.["同源网关组"]}）`);
    }
  });
  const result = sets.groups("source", live, new Set(dismissed), undetermined);
  Object.assign(sourceMemo, { sources, merges, dismissed, undetermined, result });
  return result;
}

const clueMemo: {
  clues?: StudioClue[];
  merges?: MergeRecord[];
  dismissed?: string[];
  undetermined?: Record<string, UndeterminedDuplicate>;
  result: DuplicateGroup[];
} = { result: [] };

/**
 * Findings of the same project that record the same thing: the same quote,
 * the same title, overlapping quotes from one passage, or same category on a shared primary asset.
 */
export function findClueDuplicates(
  clues: StudioClue[],
  merges: MergeRecord[],
  dismissed: string[],
  undetermined?: Record<string, UndeterminedDuplicate>,
): DuplicateGroup[] {
  if (
    clueMemo.clues === clues &&
    clueMemo.merges === merges &&
    clueMemo.dismissed === dismissed &&
    clueMemo.undetermined === undetermined
  ) {
    return clueMemo.result;
  }
  const live = clues.filter((clue) => !mergeOf(merges, "clue", clue.id));
  const sets = new DuplicateSets();
  bucket(live, (clue) => {
    const quote = normalizeForCompare(clue.quote);
    return quote.length >= 4 ? quote : "";
  }).forEach((items) => sets.link(items.map((item) => item.id), "引文相同"));
  bucket(live, (clue) => {
    const title = normalizeForCompare(clue.title);
    return title.length >= 2 ? title : "";
  }).forEach((items) => sets.link(items.map((item) => item.id), `标题相同「${clip(items[0].title, 24)}」`));
  bucket(live, (clue) => clue.passageId).forEach((items) => {
    for (let i = 0; i < items.length; i += 1) {
      for (let j = i + 1; j < items.length; j += 1) {
        const a = normalizeForCompare(items[i].quote);
        const b = normalizeForCompare(items[j].quote);
        if (a && b && a !== b && (a.includes(b) || b.includes(a))) sets.link([items[i].id, items[j].id], "摘自同一段原文，引文互相包含");
      }
    }
  });
  // Same category + shared primary asset (homologous finding clusters like F-052/F-058/F-064)
  bucket(live, (clue) => `${clue.sourceId}:${clue.category}`).forEach((items) => {
    if (items.length >= 2) {
      sets.link(items.map((item) => item.id), `共同关联主资产 ${items[0].sourceId} 且属同类问题`);
    }
  });
  const result = sets.groups("clue", live, new Set(dismissed), undetermined);
  Object.assign(clueMemo, { clues, merges, dismissed, undetermined, result });
  return result;
}

/**
 * Generates structured AI merge suggestions for duplicate candidate groups,
 * picking the highest-readiness/verified record as primary and explaining the root-cause overlap.
 */
export function buildAiMergeSuggestions(
  groups: DuplicateGroup[],
  sources: StudioSource[],
  clues: StudioClue[],
): AiMergeSuggestion[] {
  const sourceById = new Map(sources.map((s) => [s.id, s]));
  const clueById = new Map(clues.map((c) => [c.id, c]));
  return groups.map((group): AiMergeSuggestion => {
    if (group.kind === "clue") {
      const items = group.ids.map((id) => clueById.get(id)).filter((c): c is StudioClue => Boolean(c));
      const sorted = [...items].sort((a, b) => {
        const score = (c: StudioClue) => (c.status === "verified" ? 100 : c.status === "unverified" ? 50 : 10) + (c.supportingEvidenceIds?.length ?? 0) * 10;
        return score(b) - score(a);
      });
      const primary = sorted[0]?.id ?? group.ids[0];
      const members = group.ids.filter((id) => id !== primary);
      const primaryClue = clueById.get(primary);
      return {
        id: `ai-merge-${group.key}`,
        groupKey: group.key,
        kind: "clue",
        projectId: group.projectId,
        primaryId: primary,
        memberIds: members,
        confidence: group.reasons.some((r) => r.includes("引文相同") || r.includes("共同关联主资产")) ? "high" : "medium",
        rationale: `AI 同因分析：${group.reasons.join("；")}。建议保留状态与证据最完整的「${primaryClue?.title ?? primary}」为主记录，其余 ${members.length} 条作为同根因路径并入，保留各自出处与证据快照。`,
        sharedRootCause: `主资产 ${primaryClue?.sourceId ?? ""} 上的同一访问控制/配置策略边界不一致`,
        fixBoundary: `统一在 ${primaryClue?.sourceId ?? "目标服务"} 网关层与权限策略配置处修复`,
      };
    }
    const items = group.ids.map((id) => sourceById.get(id)).filter((s): s is StudioSource => Boolean(s));
    const sorted = [...items].sort((a, b) => (b.status === "filed" ? 100 : 0) + b.passages.length - ((a.status === "filed" ? 100 : 0) + a.passages.length));
    const primary = sorted[0]?.id ?? group.ids[0];
    const members = group.ids.filter((id) => id !== primary);
    const primarySource = sourceById.get(primary);
    return {
      id: `ai-merge-${group.key}`,
      groupKey: group.key,
      kind: "source",
      projectId: group.projectId,
      primaryId: primary,
      memberIds: members,
      confidence: "high",
      rationale: `AI 资产归一建议：${group.reasons.join("；")}。建议以「${primarySource?.title ?? primary}」为主实体，保留全部历史观测与别名。`,
      sharedRootCause: "同一底层服务实体的多入口域名/重复导入记录",
      fixBoundary: "合并实体身份，保留各来源观测历史",
    };
  });
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

export function formatTime(iso: string): string {
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return iso;
  const pad = (value: number) => String(value).padStart(2, "0");
  return `${date.getMonth() + 1}月${date.getDate()}日 ${pad(date.getHours())}:${pad(date.getMinutes())}`;
}

/** Display-only title cleanup: strips "ID · " (with optional parenthetical), leading severity, and trailing demo/object-id suffixes. */
export function displayTitle(title: string): string {
  const out = title
    .replace(/^[A-Z]+-[A-Za-z0-9-]+(?:\s*\([^)]*\))?\s*·\s*/, "")
    .replace(/^(?:严重|高危|中危|低危|信息)\s*·\s*/, "")
    .replace(/\s*·\s*(?:\d+（虚构）|\d+ 报告草稿|[A-Za-z]+-[A-Za-z0-9-]+)$/, "")
    .replace(/（虚构）$|（离线演示）$|（演示记录）$/, "")
    .trim();
  return out || title;
}


export const RUN_STATE_LABEL: Record<WorkRunState, string> = { running: "执行中", completed: "已完成", blocked: "受阻", failed: "失败", waiting_approval: "待确认" };
export const RUN_STATE_TONE: Record<WorkRunState, "accent" | "success" | "warning" | "danger"> = { running: "accent", completed: "success", blocked: "warning", failed: "danger", waiting_approval: "warning" };

const APPROVAL_STATUS_LABEL: Record<StudioApproval["status"], string> = { pending: "等待授权确认", approved_once: "已允许一次", approved_grant: "已存为授权", denied: "已拒绝", expired: "已过期", superseded: "已被替代" };

/** Pure presentation mapping for approvals: a short title + status subtitle. Raw target/arguments stay in the card body. */
export function approvalTitle(a: StudioApproval): { title: string; subtitle: string } {
  let title = `${a.action} · ${a.target}`;
  const stripScheme = (target: string) => target.replace(/^https?:\/\//, "");
  const base = (target: string) => stripScheme(target).split("/").filter(Boolean).pop() ?? stripScheme(target);
  if (a.resourceKind === "file" && /read/i.test(a.action)) title = `读取文件 · ${base(a.target)}`;
  else if (a.resourceKind === "http") {
    const m = a.action.match(/\(([A-Z]+)\)/);
    title = `发送${m ? ` ${m[1]} ` : " "}请求 · ${stripScheme(a.target)}`;
  } else if (a.resourceKind === "browser") title = `浏览器访问 · ${stripScheme(a.target)}`;
  else if (a.resourceKind === "network") title = `网络连接 · ${a.target}`;
  else if (a.resourceKind === "mcp") title = `调用 MCP 工具 · ${a.toolName}`;
  const subtitle = APPROVAL_STATUS_LABEL[a.status] + (a.matchedRuleReason && a.matchedRuleReason.length <= 24 ? ` · ${a.matchedRuleReason}` : "");
  return { title, subtitle };
}

/** Display-only cleanup of seeded demo reply text (assistant messages only). */
export function cleanDemoText(text: string): string {
  let out = text
    .replace(/Finding (F-\d+) · ([^\n·]+?) · \d+（虚构）/g, "$1 $2")
    .replace(/ · \d+（虚构）/g, "")
    .replace(/（虚构）/g, "")
    .replace(/虚构高风险记录/g, "高风险记录")
    .replace(/虚构资产/g, "资产");
  const lines = out.split("\n");
  const disclaimers = lines.filter((l) => l.trim().startsWith("演示数据"));
  if (disclaimers.length > 1) {
    out = lines.filter((l) => !l.trim().startsWith("演示数据")).join("\n").replace(/\n{3,}/g, "\n\n").trimEnd() + "\n\n" + disclaimers[0].trim();
  }
  return out;
}
