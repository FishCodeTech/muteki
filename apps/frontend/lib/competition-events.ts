

export interface ConnectionView {
  connection_id: string;
  platform_kind: string;
  canonical_base_url: string;
  account_key: string;
  capabilities: Record<string, unknown>;
  status: string;
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
  scheduler_state: string;
  tombstoned: boolean;
  created_at?: string;
  updated_at?: string;
}

export interface PolicyView {
  competition_id: string;
  automation_mode: string;
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
  remote_state: string;
  state: string;
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
  instance_required?: boolean;
  platform_type?: string | number;
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
  state: string;
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
  state: string;
  last_error: string;
  created_at?: string;
  updated_at?: string;
}

export interface QueueEntryView {
  competition_id: string;
  competition_challenge_id: string;
  state: string;
  priority: number;
  score: number;
  not_before?: string | null;
  admission_decision: Record<string, unknown>;
  enqueued_at?: string;
  updated_at?: string;
}

export interface BudgetView {
  competition_id: string;
  kind: string;
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
  digest: string;
  source_run_id: string;
  source_ref: string;
  gate_verdict: string;
  source_execution_generation?: number;
  source_worker_id?: string;
  source_session_id?: string;
  shared_graph_fact_id?: string;
  witness_digest?: string;
  witness_artifact_path?: string;
  state: string;
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
  state: string;
  retry_after_at?: string | null;
  remote_receipt: string;
  last_error: string;
  created_at?: string;
  updated_at?: string;
}

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
  run_id?: string;
}


export interface ChatMessage {
  key: string;
  seq: number;
  author: string;
  text: string;
  at: string;
  commandId?: string;
  receiptState?: string;
  local?: boolean;
}

export type CardTone = "info" | "ok" | "warn" | "error";

export interface ActivityCard {
  key: string;
  seq: number;
  kind: string;
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
  watermark: number;
  lastSeq: number;
  protocolError: string;
  messages: ChatMessage[];
  cards: ActivityCard[];
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
    lastSeq: Math.max(deck.lastSeq, watermark),
    protocolError: "",
    dirty: 0,
  };
}

export function applySnapshot(
  deck: CompetitionDeck,
  snapshot: CompetitionSnapshotView,
): CompetitionDeck {
  if (!snapshot || !snapshot.competition || !Array.isArray(snapshot.challenges)) {
    return { ...deck, protocolError: "snapshot 响应缺少必需字段" };
  }
  return applySnapshotFrame(deck, { snapshot, watermark_seq: snapshot.event_watermark });
}


function str(v: unknown): string {
  return typeof v === "string" ? v : v == null ? "" : String(v);
}

function num(v: unknown): number {
  const n = Number(v);
  return Number.isFinite(n) ? n : 0;
}

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
      return null;
  }
}

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
