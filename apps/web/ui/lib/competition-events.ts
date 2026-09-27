/**
 * Competition 前端独立 reducer（任务书 COMP-10，设计 14.2/14.3/14.4）。
 *
 * 与 UI-INTEG-01 的 lib/events.ts 完全隔离：比赛页面不经过 Run reducer，
 * 只消费 COMP-09 SSE（``GET /api/competitions/{id}/events``）下发的
 * 当前 snapshot 帧。Public Event 仅用于独立历史时间线。
 *
 * 协议约定（与 apps/web/competition_api.py 对齐）：
 *
 * - 首帧 ``event: snapshot``，data 为 ``{snapshot, watermark_seq}``；
 *   snapshot 必须携带 competition / challenges / event_watermark，
 *   缺这些必需字段即视为协议不兼容（``protocolError`` 非空，页面显示
 *   明确提示而不是渲染半吊子状态）。
 * - 后续仍为 snapshot，SSE id 为快照水位；低水位快照被忽略。
 *   重连直接恢复当前状态，历史通过独立分页接口读取。
 * - 未知或非核心事件类型不报错、不渲染，只推进 seq（协议向前兼容）。
 *
 * reducer 对事件做两层处理：
 *
 * 1. 状态覆盖：对已知事件把可确定的字段增量打到 snapshot 副本上
 *    （题目状态、队列条目、调度器状态、策略、租约、提交、候选），
 *    保证两次 snapshot 拉取之间 UI 也是活的；
 * 2. 活动卡片：把事件翻译为设计 14.4 的结构化操作卡片（连接测试、
 *    同步差异、调度 / 准入、实例、子 Run、提交回执、失败分类、
 *    Advisor 建议），聊天消息进 ``messages``。
 */

// ---------------------------------------------------------------------------
// 后端 JSON 视图类型（snake_case，与 COMP-09 查询 Handler 的 view 对齐）
// ---------------------------------------------------------------------------

export interface ConnectionView {
  connection_id: string;
  platform_kind: string;
  canonical_base_url: string;
  account_key: string;
  capabilities: Record<string, unknown>;
  status: string; // active / disabled / auth_required
  last_error: string;
  has_credential: boolean;
  created_at?: string;
  updated_at?: string;
}

export interface CompetitionInfo {
  competition_id: string;
  connection_id: string;
  external_competition_id: string;
  title: string;
  description: string;
  starts_at?: string | null;
  ends_at?: string | null;
  platform_status?: Record<string, unknown>;
  scheduler_state: string; // stopped / running / paused
  tombstoned: boolean;
  created_at?: string;
  updated_at?: string;
}

export interface PolicyView {
  competition_id: string;
  automation_mode: string; // observe / assisted / autonomous
  max_concurrent_runs: number;
  max_instances: number;
  submission_cooldown_seconds: number;
  category_allow: string[];
  category_deny: string[];
  budget_limits: Record<string, number>;
  updated_at?: string;
}

export interface ChallengeView {
  challenge_id: string;
  competition_id: string;
  external_challenge_id: string;
  name: string;
  category: string;
  current_revision_id?: string | null;
  remote_state: string; // open / closed / solved_remote / hidden
  state: string; // ChallengeState
  paused_from?: string | null;
  tombstoned: boolean;
  created_at?: string;
  updated_at?: string;
}

export interface RevisionView {
  revision_id: string;
  competition_challenge_id: string;
  content_hash: string;
  revision_seq: number;
  name: string;
  category: string;
  points: number;
  description: string;
  target: string;
  flag_format: string;
  hints: string[];
  prerequisites: string[];
  multi_flag: boolean;
  expected_flags: number;
  created_at?: string;
}

export interface BindingView {
  binding_id: string;
  competition_id: string;
  competition_challenge_id: string;
  revision_id: string;
  run_id: string;
  execution_generation: number;
  lease_id?: string | null;
  state: string; // BindingState
  created_at?: string;
  updated_at?: string;
}

export interface LeaseView {
  lease_id: string;
  connection_id: string;
  competition_id: string;
  competition_challenge_id: string;
  platform_instance_id: string;
  generation: number;
  fencing_token: number;
  owner: string;
  address: string;
  has_credential: boolean;
  ttl_seconds: number;
  renew_deadline_at?: string | null;
  expires_at?: string | null;
  state: string; // LeaseState
  last_error: string;
  created_at?: string;
  updated_at?: string;
}

export interface QueueEntryView {
  competition_id: string;
  competition_challenge_id: string;
  state: string; // queued / dispatching / held / done / dropped
  priority: number;
  score: number;
  not_before?: string | null;
  admission_decision: Record<string, unknown>;
  enqueued_at?: string;
  updated_at?: string;
}

export interface BudgetView {
  competition_id: string;
  kind: string; // tokens / cost / wallclock / submissions / instances
  limit: number;
  used: number;
  window: string;
  resets_at?: string | null;
  updated_at?: string;
}

export interface CandidateView {
  candidate_id: string;
  competition_id: string;
  competition_challenge_id: string;
  answer_slot: number;
  digest: string; // 只有摘要，绝无候选原文
  source_run_id: string;
  source_ref: string;
  gate_verdict: string;
  source_execution_generation?: number;
  source_worker_id?: string;
  source_session_id?: string;
  shared_graph_fact_id?: string;
  witness_digest?: string;
  witness_artifact_path?: string;
  state: string; // candidate / awaiting_approval / approved / ...
  created_at?: string;
  updated_at?: string;
}

export interface SubmissionView {
  submission_id: string;
  competition_id: string;
  competition_challenge_id: string;
  candidate_id: string;
  answer_slot: number;
  digest: string;
  attempt: number;
  state: string; // queued / submitting / correct / wrong / ...
  retry_after_at?: string | null;
  remote_receipt: string;
  last_error: string;
  created_at?: string;
  updated_at?: string;
}

/** snapshot.challenges[] 行（ChallengesSnapshot 的 JSON 形态）。 */
export interface ChallengeSnapshotRow {
  challenge: ChallengeView;
  current_revision: RevisionView | null;
  active_binding: BindingView | null;
  active_lease: LeaseView | null;
  queue_entry: QueueEntryView | null;
  candidates: CandidateView[];
  submissions: SubmissionView[];
}

export interface CompetitionSnapshotView {
  generated_at: string;
  schema_version_db?: number | null;
  connection: ConnectionView | null;
  competition: CompetitionInfo | null;
  policy: PolicyView | null;
  challenges: ChallengeSnapshotRow[];
  queue: QueueEntryView[];
  budgets: BudgetView[];
  event_watermark: number;
  projection_watermarks: Record<string, number>;
  pending_receipts: number;
  pending_outbox: number;
}

/** SSE ``event: competition`` 的完整 Public Event。 */
export interface CompetitionPublicEvent {
  seq: number;
  event_type: string;
  occurred_at: string;
  competition_id: string;
  aggregate_type: string;
  aggregate_id: string;
  payload: Record<string, unknown>;
  command_id?: string;
  correlation_id?: string;
  /** 子 Run 链接（提升到顶层，直接构造 /run/{id}）。 */
  run_id?: string;
}

// ---------------------------------------------------------------------------
// Deck 状态
// ---------------------------------------------------------------------------

export interface ChatMessage {
  key: string;
  seq: number;
  author: string;
  text: string;
  at: string;
  commandId?: string;
  receiptState?: string;
  /** 本地发出的消息在 SSE 回环前先占位显示。 */
  local?: boolean;
}

export type CardTone = "info" | "ok" | "warn" | "error";

/** 设计 14.4 的结构化操作卡片。 */
export interface ActivityCard {
  key: string;
  seq: number;
  kind: string; // sync / challenge / queue / instance / run / submission / policy / scheduler / advisor / connection
  title: string;
  lines: string[];
  tone: CardTone;
  at: string;
  runId?: string;
  commandId?: string;
}

export interface CompetitionDeck {
  competitionId: string;
  snapshot: CompetitionSnapshotView | null;
  /** snapshot 帧声明的水位（恢复游标基于此值）。 */
  watermark: number;
  /** 已消费的最大 stream seq（Last-Event-ID 续传游标）。 */
  lastSeq: number;
  /** 非空 = 协议不兼容（核心状态缺必需字段），页面显示而不是瞎渲染。 */
  protocolError: string;
  messages: ChatMessage[];
  cards: ActivityCard[];
  /** 自上次完整 snapshot 后收到实体事件的计数（hook 据此防抖刷新）。 */
  dirty: number;
}

export function emptyCompetitionDeck(competitionId: string): CompetitionDeck {
  return {
    competitionId,
    snapshot: null,
    watermark: 0,
    lastSeq: 0,
    protocolError: "",
    messages: [],
    cards: [],
    dirty: 0,
  };
}

// ---------------------------------------------------------------------------
// snapshot 帧
// ---------------------------------------------------------------------------

function isRecord(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

function normalizeSnapshot(snapshot: CompetitionSnapshotView): CompetitionSnapshotView {
  const challenges = snapshot.challenges
    .filter((row): row is ChallengeSnapshotRow => isRecord(row) && isRecord(row.challenge))
    .map((row) => ({
      ...row,
      current_revision: isRecord(row.current_revision) ? row.current_revision : null,
      active_binding: isRecord(row.active_binding) ? row.active_binding : null,
      active_lease: isRecord(row.active_lease) ? row.active_lease : null,
      queue_entry: isRecord(row.queue_entry) ? row.queue_entry : null,
      candidates: Array.isArray(row.candidates) ? row.candidates.filter(isRecord) as unknown as CandidateView[] : [],
      submissions: Array.isArray(row.submissions) ? row.submissions.filter(isRecord) as unknown as SubmissionView[] : [],
    }));
  return {
    ...snapshot,
    challenges,
    queue: Array.isArray(snapshot.queue) ? snapshot.queue.filter(isRecord) as unknown as QueueEntryView[] : [],
    budgets: Array.isArray(snapshot.budgets) ? snapshot.budgets.filter(isRecord) as unknown as BudgetView[] : [],
    projection_watermarks: isRecord(snapshot.projection_watermarks) ? snapshot.projection_watermarks as Record<string, number> : {},
  };
}

/**
 * 应用 SSE 首帧 snapshot。核心状态缺必需字段（snapshot 本体、
 * competition、challenges 数组、数字 watermark）时置 protocolError。
 */
export function applySnapshotFrame(
  deck: CompetitionDeck,
  frame: unknown,
): CompetitionDeck {
  if (!isRecord(frame) || !isRecord(frame.snapshot)) {
    return { ...deck, protocolError: "snapshot 帧缺少 snapshot 本体" };
  }
  const rawSnapshot = frame.snapshot as unknown as CompetitionSnapshotView;
  if (!isRecord(rawSnapshot.competition)) {
    return { ...deck, protocolError: "snapshot 缺少 competition 核心状态" };
  }
  if (!Array.isArray(rawSnapshot.challenges)) {
    return { ...deck, protocolError: "snapshot 缺少 challenges 列表" };
  }
  const snapshot = normalizeSnapshot(rawSnapshot);
  const watermark = Number(frame.watermark_seq ?? snapshot.event_watermark);
  if (!Number.isFinite(watermark)) {
    return { ...deck, protocolError: "snapshot 帧缺少数字 watermark_seq" };
  }
  if (snapshot.competition?.competition_id !== deck.competitionId) return deck;
  if (watermark < deck.watermark) return deck;
  if (watermark === deck.watermark && deck.snapshot &&
      Date.parse(snapshot.generated_at) < Date.parse(deck.snapshot.generated_at)) return deck;
  return {
    ...deck,
    snapshot,
    watermark,
    // 实时流传输当前快照，水位之前的实体状态已包含在这一帧中。
    lastSeq: Math.max(deck.lastSeq, watermark),
    protocolError: "",
    dirty: 0,
  };
}

/** 防抖后的完整 snapshot 刷新（GET /api/competitions/{id}）走这里。 */
export function applySnapshot(
  deck: CompetitionDeck,
  snapshot: CompetitionSnapshotView,
): CompetitionDeck {
  if (!snapshot || !snapshot.competition || !Array.isArray(snapshot.challenges)) {
    return { ...deck, protocolError: "snapshot 响应缺少必需字段" };
  }
  return applySnapshotFrame(deck, { snapshot, watermark_seq: snapshot.event_watermark });
}

// ---------------------------------------------------------------------------
// 事件归约
// ---------------------------------------------------------------------------

function str(v: unknown): string {
  return typeof v === "string" ? v : v == null ? "" : String(v);
}

function num(v: unknown): number {
  const n = Number(v);
  return Number.isFinite(n) ? n : 0;
}

/** 对 snapshot 副本做字段级增量更新（只改事件里确实携带的字段）。 */
function patchSnapshot(
  snapshot: CompetitionSnapshotView,
  ev: CompetitionPublicEvent,
): CompetitionSnapshotView {
  const p = ev.payload;
  const next: CompetitionSnapshotView = {
    ...snapshot,
    challenges: snapshot.challenges.map((row) => ({
      ...row,
      challenge: { ...row.challenge },
      queue_entry: row.queue_entry ? { ...row.queue_entry } : null,
      active_binding: row.active_binding ? { ...row.active_binding } : null,
      active_lease: row.active_lease ? { ...row.active_lease } : null,
      candidates: row.candidates.map((c) => ({ ...c })),
      submissions: row.submissions.map((s) => ({ ...s })),
    })),
    queue: snapshot.queue.map((q) => ({ ...q })),
    competition: snapshot.competition ? { ...snapshot.competition } : null,
    policy: snapshot.policy ? { ...snapshot.policy } : null,
  };
  const challengeId = str(p.challenge_id);
  const row = next.challenges.find(
    (r) => r.challenge.challenge_id === challengeId,
  );

  switch (ev.event_type) {
    case "competition.challenge.discovered": {
      if (!row && challengeId) {
        next.challenges = [
          ...next.challenges,
          {
            challenge: {
              challenge_id: challengeId,
              competition_id: next.competition?.competition_id ?? "",
              external_challenge_id: str(p.external_challenge_id),
              name: str(p.name),
              category: str(p.category),
              current_revision_id: null,
              remote_state: str(p.remote_state) || "open",
              state: "discovered",
              paused_from: null,
              tombstoned: false,
            },
            current_revision: null,
            active_binding: null,
            active_lease: null,
            queue_entry: null,
            candidates: [],
            submissions: [],
          },
        ];
      }
      return next;
    }
    case "competition.challenge.state_changed": {
      if (row) {
        row.challenge.state = str(p.to) || row.challenge.state;
        if (str(p.to) === "paused") {
          row.challenge.paused_from = str(p.paused_from) || str(p.from) || null;
        } else {
          row.challenge.paused_from = null;
        }
        if (str(p.remote_state)) row.challenge.remote_state = str(p.remote_state);
      }
      return next;
    }
    case "competition.queue.enqueued": {
      const entry: QueueEntryView = {
        competition_id: next.competition?.competition_id ?? "",
        competition_challenge_id: challengeId,
        state: "queued",
        priority: num(p.priority),
        score: num(p.score),
        not_before: null,
        admission_decision: {},
      };
      const idx = next.queue.findIndex(
        (q) => q.competition_challenge_id === challengeId,
      );
      if (idx >= 0) next.queue[idx] = { ...next.queue[idx], ...entry };
      else next.queue = [...next.queue, entry];
      if (row) row.queue_entry = { ...(row.queue_entry ?? entry), ...entry };
      return next;
    }
    case "competition.scheduler.started":
    case "competition.scheduler.paused":
    case "competition.scheduler.resumed": {
      if (next.competition) {
        next.competition.scheduler_state =
          str(p.to) ||
          (ev.event_type === "competition.scheduler.started" ||
          ev.event_type === "competition.scheduler.resumed"
            ? "running"
            : "paused");
      }
      return next;
    }
    case "competition.policy.updated": {
      if (next.policy && isRecord(p.changes)) {
        Object.assign(next.policy, p.changes);
      }
      return next;
    }
    case "competition.instance.lease_requested":
    case "competition.instance.state_changed":
    case "competition.instance.release_requested": {
      const leaseId = str(p.lease_id);
      for (const r of next.challenges) {
        if (r.active_lease && r.active_lease.lease_id === leaseId) {
          if (str(p.to)) r.active_lease.state = str(p.to);
          if (str(p.state)) r.active_lease.state = str(p.state);
          if (str(p.address)) r.active_lease.address = str(p.address);
          if (str(p.expires_at)) r.active_lease.expires_at = str(p.expires_at);
          if (p.generation != null) r.active_lease.generation = num(p.generation);
        }
      }
      return next;
    }
    case "competition.run_binding.state_changed": {
      const bindingId = str(p.binding_id);
      for (const r of next.challenges) {
        if (r.active_binding && r.active_binding.binding_id === bindingId) {
          if (str(p.to)) r.active_binding.state = str(p.to);
          if (p.execution_generation != null) {
            r.active_binding.execution_generation = num(p.execution_generation);
          }
        }
      }
      return next;
    }
    case "competition.platform_submission.state_changed": {
      const subId = str(p.submission_id);
      for (const r of next.challenges) {
        for (const s of r.submissions) {
          if (s.submission_id === subId) {
            if (str(p.to)) s.state = str(p.to);
            if (str(p.state)) s.state = str(p.state);
            if (str(p.retry_after_at)) s.retry_after_at = str(p.retry_after_at);
            if (str(p.receipt)) s.remote_receipt = str(p.receipt);
          }
        }
      }
      return next;
    }
    case "competition.submission.candidate_state_changed": {
      const candId = str(p.candidate_id);
      for (const r of next.challenges) {
        for (const c of r.candidates) {
          if (c.candidate_id === candId && str(p.to)) c.state = str(p.to);
        }
      }
      return next;
    }
    default:
      return next;
  }
}

// ---------------------------------------------------------------------------
// 活动卡片（设计 14.4）
// ---------------------------------------------------------------------------

const RETRYABLE_STATES = new Set([
  "rate_limited",
  "transient_failure",
  "auth_required",
]);

export function cardForEvent(ev: CompetitionPublicEvent): ActivityCard | null {
  const p = ev.payload;
  const base = {
    key: `ev-${ev.seq}`,
    seq: ev.seq,
    at: ev.occurred_at,
    commandId: ev.command_id,
    runId: ev.run_id,
  };
  switch (ev.event_type) {
    case "competition.connection.test_requested":
      return {
        ...base,
        kind: "connection",
        title: "平台连接测试",
        lines: [`连接 ${str(p.connection_id)} 的探测已下发（outbox）`],
        tone: "info",
      };
    case "competition.registered":
      return {
        ...base,
        kind: "sync",
        title: "比赛已登记",
        lines: [`「${str(p.title) || str(p.external_competition_id)}」已登记`],
        tone: "ok",
      };
    case "competition.sync.requested": {
      const lines = ["同步请求已下发"];
      const diffKeys = ["added", "changed", "unchanged", "removed"] as const;
      const diff = diffKeys
        .filter((k) => p[k] != null)
        .map((k) => `${k}=${num(p[k])}`);
      if (diff.length) lines.push(`差异：${diff.join("，")}`);
      return { ...base, kind: "sync", title: "同步差异", lines, tone: "info" };
    }
    case "competition.challenge.discovered":
      return {
        ...base,
        kind: "challenge",
        title: "发现题目",
        lines: [
          `${str(p.category) || "未分类"} / ${str(p.name) || str(p.external_challenge_id)}`,
        ],
        tone: "info",
      };
    case "competition.challenge.state_changed":
      return {
        ...base,
        kind: "challenge",
        title: "题目状态变更",
        lines: [`${str(p.from)} → ${str(p.to)}`],
        tone: str(p.to) === "solved" ? "ok" : "info",
      };
    case "competition.queue.enqueued":
      return {
        ...base,
        kind: "queue",
        title: "题目已排队",
        lines: [`优先级 ${num(p.priority)}`],
        tone: "info",
      };
    case "competition.scheduler.started":
    case "competition.scheduler.paused":
    case "competition.scheduler.resumed":
      return {
        ...base,
        kind: "scheduler",
        title: "调度器",
        lines: [`${str(p.from)} → ${str(p.to)}`],
        tone: "info",
      };
    case "competition.scheduler.admission_decision":
      return {
        ...base,
        kind: "scheduler",
        title: "准入决策",
        lines: [
          `动作 ${str(p.action) || "—"}${str(p.reason) ? `，原因 ${str(p.reason)}` : ""}`,
        ],
        tone: str(p.action) === "admit" ? "ok" : "info",
      };
    case "competition.scheduler.budget_exhausted":
      return {
        ...base,
        kind: "scheduler",
        title: "预算耗尽",
        lines: [str(p.reason) || "相关操作已暂停并记录原因"],
        tone: "warn",
      };
    case "competition.scheduler.dispatch_paused":
      return {
        ...base,
        kind: "scheduler",
        title: "派发暂停",
        lines: [str(p.reason) || str(p.hold_reason) || "认证或平台策略变化"],
        tone: "warn",
      };
    case "competition.policy.updated": {
      const changes = isRecord(p.changes)
        ? Object.entries(p.changes).map(([k, v]) => `${k}=${JSON.stringify(v)}`)
        : [];
      return {
        ...base,
        kind: "policy",
        title: "自动化策略变更",
        lines: changes.length ? changes : ["策略已更新"],
        tone: "info",
      };
    }
    case "competition.instance.lease_requested":
      return {
        ...base,
        kind: "instance",
        title: "实例申请",
        lines: [`租约 ${str(p.lease_id)} 已创建`],
        tone: "info",
      };
    case "competition.instance.state_changed":
      return {
        ...base,
        kind: "instance",
        title: "实例状态",
        lines: [
          `${str(p.from)} → ${str(p.to)}`,
          ...(str(p.address) ? [`地址 ${str(p.address)}`] : []),
        ],
        tone: "info",
      };
    case "competition.instance.release_requested":
      return {
        ...base,
        kind: "instance",
        title: "实例释放",
        lines: [`${str(p.from)} → ${str(p.to)}`],
        tone: "info",
      };
    case "competition.instance.quota_rejected":
      return {
        ...base,
        kind: "instance",
        title: "实例配额拒绝",
        lines: [str(p.reason) || `配额 ${str(p.scope)} 上限 ${num(p.limit)}`],
        tone: "warn",
      };
    case "competition.instance.reconcile_degraded":
      return {
        ...base,
        kind: "instance",
        title: "实例恢复降级",
        lines: [str(p.reason) || "重启后实例状态未知，已降级暂停"],
        tone: "warn",
      };
    case "competition.run_binding.created":
      return {
        ...base,
        kind: "run",
        title: "子 Run 创建",
        lines: [
          `Run ${str(p.run_id) || "—"}，执行代 ${num(p.execution_generation) || 1}`,
        ],
        tone: "ok",
      };
    case "competition.run_binding.state_changed":
      return {
        ...base,
        kind: "run",
        title: "子 Run 状态",
        lines: [
          `${str(p.from)} → ${str(p.to)}（执行代 ${num(p.execution_generation)}）`,
        ],
        tone: str(p.to) === "failed" ? "error" : "info",
      };
    case "competition.platform_submission.approved":
      return {
        ...base,
        kind: "submission",
        title: "提交已批准",
        lines: [`第 ${num(p.attempt) || 1} 次尝试，候选 ${str(p.candidate_id)}`],
        tone: "info",
      };
    case "competition.platform_submission.queued":
      return {
        ...base,
        kind: "submission",
        title: "提交入队",
        lines: [`提交 ${str(p.submission_id)} 等待远端回执`],
        tone: "info",
      };
    case "competition.platform_submission.retry_requested":
      return {
        ...base,
        kind: "submission",
        title: "提交重试",
        lines: [`${str(p.from)} → ${str(p.to)}`],
        tone: "info",
      };
    case "competition.platform_submission.state_changed": {
      const to = str(p.to) || str(p.state);
      const lines = [`状态 ${to}`];
      if (str(p.receipt)) lines.push(`远端回执：${str(p.receipt)}`);
      if (str(p.retry_after_at)) lines.push(`冷却至 ${str(p.retry_after_at)}`);
      if (str(p.error)) lines.push(`失败分类：${str(p.error)}`);
      return {
        ...base,
        kind: "submission",
        title: "远端提交回执",
        lines,
        tone:
          to === "correct"
            ? "ok"
            : to === "wrong" || RETRYABLE_STATES.has(to)
              ? to === "wrong"
                ? "error"
                : "warn"
              : "info",
      };
    }
    case "competition.submission.candidate_registered":
      return {
        ...base,
        kind: "submission",
        title: "候选登记",
        lines: [
          `digest ${str(p.digest).slice(0, 12)}…，Gate ${str(p.gate_verdict) || "—"}`,
        ],
        tone: "info",
      };
    case "competition.submission.candidate_state_changed":
      return {
        ...base,
        kind: "submission",
        title: "候选状态",
        lines: [
          `${str(p.from)} → ${str(p.to)}${str(p.hold_reason) ? `（${str(p.hold_reason)}）` : ""}`,
        ],
        tone: str(p.to) === "awaiting_approval" ? "warn" : "info",
      };
    case "competition.advisor.advice":
      return {
        ...base,
        kind: "advisor",
        title: "Advisor 建议",
        lines: [str(p.text) || str(p.reason) || JSON.stringify(p).slice(0, 200)],
        tone: "info",
      };
    default:
      // 未知 / 非核心事件：不渲染卡片，只推进 seq（协议向前兼容）。
      return null;
  }
}

// ---------------------------------------------------------------------------
// 主归约入口
// ---------------------------------------------------------------------------

const MAX_CHAT = 400;
const MAX_CARDS = 300;

export function reduceCompetitionEvent(
  deck: CompetitionDeck,
  ev: CompetitionPublicEvent,
): CompetitionDeck {
  const seq = Number(ev?.seq);
  if (!Number.isFinite(seq)) return deck; // 无 seq 的帧不参与流
  // 断线重连由 Last-Event-ID 保证不重复；这里兜底忽略乱序 / 重复帧。
  if (seq <= Math.max(deck.lastSeq, deck.watermark)) return deck;

  let next: CompetitionDeck = { ...deck, lastSeq: seq };

  // 聊天消息进 messages（附带结构化命令的回执引用也在这里显示）。
  if (ev.event_type === "competition.message.posted") {
    const msg: ChatMessage = {
      key: `ev-${seq}`,
      seq,
      author: str(ev.payload.author) || "operator",
      text: str(ev.payload.text),
      at: ev.occurred_at,
      commandId: str(ev.payload.command_id) || undefined,
      receiptState: str(ev.payload.receipt_state) || undefined,
    };
    // 本地占位消息（同文本同作者）回环后替换为权威事件。
    const messages = next.messages.some(
      (m) => m.local && m.text === msg.text && m.author === msg.author,
    )
      ? next.messages.map((m) =>
          m.local && m.text === msg.text && m.author === msg.author ? msg : m,
        )
      : [...next.messages, msg];
    next = { ...next, messages: messages.slice(-MAX_CHAT) };
    return next; // 消息不触碰实体状态
  }

  // 实体状态增量覆盖 + dirty 计数（hook 防抖刷新完整 snapshot）。
  if (next.snapshot) {
    const patched = patchSnapshot(next.snapshot, ev);
    if (patched !== next.snapshot) {
      next = { ...next, snapshot: patched, dirty: next.dirty + 1 };
    }
  } else {
    next = { ...next, dirty: next.dirty + 1 };
  }

  const card = cardForEvent(ev);
  if (card) {
    next = { ...next, cards: [...next.cards, card].slice(-MAX_CARDS) };
  }
  return next;
}

/** 本地命令回执 → 操作卡片（结构化 command receipt 是实际操作记录）。 */
export function localReceiptCard(input: {
  commandId: string;
  commandType: string;
  state: string;
  detail?: string;
  errorCode?: string;
  runId?: string;
  deduplicated?: boolean;
}): ActivityCard {
  const failed = input.state === "failed" || input.state === "conflict";
  return {
    key: `local-${input.commandId || Math.random().toString(36).slice(2)}`,
    seq: -1,
    kind: "command",
    title: `命令回执 ${input.commandType}`,
    lines: [
      `状态 ${input.state}${input.deduplicated ? "（幂等去重）" : ""}`,
      ...(input.detail ? [input.detail] : []),
      ...(input.errorCode ? [`错误 ${input.errorCode}`] : []),
    ],
    tone: failed ? "error" : "ok",
    at: new Date().toISOString(),
    commandId: input.commandId || undefined,
    runId: input.runId,
  };
}

// ---------------------------------------------------------------------------
// 派生选择器（顶部指标 / 面板共用）
// ---------------------------------------------------------------------------

export interface ToplineStats {
  solved: number;
  points: number;
  active: number;
  terminalPhaseWaiting: number;
  revisitWaiting: number;
  queued: number;
  runningBindings: number;
  activeLeases: number;
  pendingSubmissions: number;
  awaitingApproval: number;
  nextQueuedName: string;
}

const SOLVED = "solved";
const ACTIVE_CHALLENGE_STATES = new Set([
  "provisioning",
  "dispatching",
  "running",
  "candidate_found",
  "submitting",
]);
const ACTIVE_BINDING_STATES = new Set([
  "planned",
  "creating",
  "starting",
  "active",
  "resolving",
]);
const ACTIVE_LEASE_STATES = new Set([
  "requested",
  "provisioning",
  "active",
  "renewing",
  "releasing",
]);

export function toplineStats(snapshot: CompetitionSnapshotView | null): ToplineStats {
  const stats: ToplineStats = {
    solved: 0,
    points: 0,
    active: 0,
    terminalPhaseWaiting: 0,
    revisitWaiting: 0,
    queued: 0,
    runningBindings: 0,
    activeLeases: 0,
    pendingSubmissions: 0,
    awaitingApproval: 0,
    nextQueuedName: "",
  };
  if (!snapshot) return stats;
  let nextQueued: { priority: number; score: number; name: string } | null = null;
  for (const row of snapshot.challenges) {
    const state = row.challenge.state;
    if (state === SOLVED) {
      stats.solved += 1;
      stats.points += row.current_revision?.points ?? 0;
    }
    if (ACTIVE_CHALLENGE_STATES.has(state)) stats.active += 1;
    if (row.queue_entry?.admission_decision?.reason === "terminal_phase_wait") {
      stats.terminalPhaseWaiting += 1;
    } else if (
      state === "running" &&
      row.active_binding?.state === "paused" &&
      ["queued", "held"].includes(row.queue_entry?.state || "")
    ) {
      stats.revisitWaiting += 1;
    }
    if (state === "queued") {
      stats.queued += 1;
      const entry = row.queue_entry;
      if (entry?.admission_decision?.reason === "terminal_phase_wait") continue;
      const candidate = {
        priority: entry?.priority ?? 0,
        score: entry?.score ?? 0,
        name: row.current_revision?.name || row.challenge.name,
      };
      if (
        !nextQueued ||
        candidate.priority > nextQueued.priority ||
        (candidate.priority === nextQueued.priority &&
          candidate.score > nextQueued.score)
      ) {
        nextQueued = candidate;
      }
    }
    if (row.active_binding && ACTIVE_BINDING_STATES.has(row.active_binding.state)) {
      stats.runningBindings += 1;
    }
    if (row.active_lease && ACTIVE_LEASE_STATES.has(row.active_lease.state)) {
      stats.activeLeases += 1;
    }
    for (const s of row.submissions) {
      if (s.state === "queued" || s.state === "submitting") {
        stats.pendingSubmissions += 1;
      }
    }
    for (const c of row.candidates) {
      if (c.state === "awaiting_approval") stats.awaitingApproval += 1;
    }
  }
  stats.nextQueuedName = nextQueued?.name ?? "";
  return stats;
}
