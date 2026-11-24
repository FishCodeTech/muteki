import { apiFetch } from "@/lib/useRun";

export type ControlTier = "observe" | "organize" | "operate";
export type ControlCategory =
  | "answer_input"
  | "credentials"
  | "competition_submit"
  | "settings_write"
  | "terminal"
  | "delete"
  | "git_remote"
  | "pr_write"
  | "history_edit";

export type ControlScope = "project" | "host" | { projects: string[] };
export type ControlSource = "default" | "project" | "thread" | "subagent";

export interface ControlGrant {
  scope: ControlScope;
  tiers: ControlTier[];
  categories: ControlCategory[];
}

export interface GrantDelta {
  scope_changed: boolean;
  added_tiers: ControlTier[];
  removed_tiers: ControlTier[];
  added_categories: ControlCategory[];
  removed_categories: ControlCategory[];
}

export interface SubagentPolicy {
  approval_mode: "orchestrator" | "auto" | "always_allow" | "manual" | string;
  child_worktrees: boolean;
  default_role?: string;
}

export interface ControlGrantRequest {
  request_id: string;
  grant: ControlGrant;
  reason: string;
  requested_at: string;
  status: "pending" | "approved" | "declined";
}

export interface ThreadLifecycle {
  settled: boolean;
  settled_at?: string | null;
  settled_reason?: string;
  pinned: boolean;
  follow_up: boolean;
  follow_up_note?: string;
  snoozed_until?: string | null;
}

export interface ThreadReference {
  target_thread_id: string;
  target_title: string;
  target_project_id: string;
  target_engine: string;
  reason: string;
  anchors: string[];
  boundary_notice?: string | null;
  created_at: string;
  actor: string;
}

export interface ReviewRecord {
  summary: string;
  verdict: string;
  evidence_anchors: string[];
  recorded_at: string;
  actor_thread_id: string;
}

export interface ArmedSelfSettle {
  when: "turn_end" | "pr_merged" | "pr_closed" | string;
  consent_quote: string;
  consent_message_id: string;
  armed_at: string;
  reason: string;
}

export interface PeerCouncilReport {
  headline?: string;
  verdict?: string;
  summary?: string;
  key_points?: string[];
  rebuttals?: string[];
  evidence_anchors?: string[];
  confidence?: string;
  reported_at?: string;
}

export interface PeerCouncilMemberState {
  member_id: string;
  thread_id: string;
  role_title: string;
  perspective: string;
  adapter_id: string;
  instance_id: string;
  credential_id: string;
  model: string;
  effort: string;
  status: string;
  round_completed: number;
  round1_headline?: string | null;
  round1_anchor?: string | null;
  round1_report?: PeerCouncilReport | null;
  round2_headline?: string | null;
  round2_anchor?: string | null;
  round2_report?: PeerCouncilReport | null;
  error?: string | null;
}

export interface PeerCouncilSession {
  group_id: string;
  parent_thread_id: string;
  parent_turn_id?: string | null;
  preset_id?: string | null;
  preset_name?: string | null;
  topic: string;
  context?: string;
  rounds: number;
  total_rounds?: number;
  current_round: number;
  phase: "round1" | "round2" | "round3" | "completed" | "cancelled" | string;
  sibling_observe_unlocked: boolean;
  members: PeerCouncilMemberState[];
  created_at: string;
  updated_at: string;
  completed_at?: string | null;
}

export interface ControlThreadGrant {
  thread_id: string;
  grant: ControlGrant;
  inherited_grant: ControlGrant;
  inherited_source: ControlSource;
  delta: GrantDelta;
  source: ControlSource;
  is_subagent: boolean;
  discussion_group_id?: string | null;
  requests: ControlGrantRequest[];
  pending: ControlGrantRequest[];
  references: ThreadReference[];
  review?: ReviewRecord | null;
  self_settle?: ArmedSelfSettle | null;
  settle_suggestion?: {due_at: number; activity_at: number} | null;
  native_subagent?: string | null;
  lifecycle: ThreadLifecycle;
  discussions: PeerCouncilSession[];
}

export type OrchestrationRoleMode = "inherit" | "pinned";
export type OrchestrationEffort = "" | "low" | "medium" | "high" | "xhigh";

export interface OrchestrationRole {
  id: string;
  label: string;
  description: string;
  enabled: boolean;
  mode: OrchestrationRoleMode | string;
  provider: string;
  profile: string;
  model: string;
  effort: OrchestrationEffort | string;
  guidelines: string;
}

export type OrchestrationWorkbenchMode = "conversation" | "src";
export type OrchestrationScopeSource = "default" | "mode_override";

export interface OrchestrationProfile {
  preset: string;
  auto_inject_instructions: boolean;
  default_role?: string;
  roles: OrchestrationRole[];
}

export interface OrchestrationModelPricing {
  status: string;
  input_per_mtok?: number | null;
  output_per_mtok?: number | null;
}

export interface OrchestrationRuntimeModel {
  id: string;
  verified?: boolean;
  reasoning?: boolean;
  pricing?: OrchestrationModelPricing | null;
}

export interface OrchestrationRuntimeItem {
  engine: string;
  adapter_id: string;
  instance_id: string;
  credential_id: string;
  available: boolean;
  reason: string;
  models: OrchestrationRuntimeModel[];
  default_model?: string;
  access_modes?: string[];
  catalog_status?: string;
  catalog_error_code?: string;
  name?: string;
  profile?: string;
}

export interface OrchestrationProvidersResponse {
  providers: OrchestrationRuntimeItem[];
  runtimes: OrchestrationRuntimeItem[];
  limits?: {
    max_concurrent_subagents?: number;
    max_depth?: number;
  };
  note?: string;
}

export interface ControlSettings {
  enabled: boolean;
  standard_grant: ControlGrant;
  default_grant: ControlGrant | null;
  project_overrides: Record<string, ControlGrant>;
  subagent_policy: SubagentPolicy;
  orchestration?: OrchestrationProfile;
  orchestration_overrides?: Partial<Record<OrchestrationWorkbenchMode, OrchestrationProfile>>;
}

export interface PendingGrantRequestRow {
  thread_id: string;
  thread_title: string;
  project_id: string;
  is_subagent: boolean;
  request: ControlGrantRequest;
}

export interface CustomThreadGrantRow {
  thread_id: string;
  thread_title: string;
  project_id: string;
  is_subagent: boolean;
  grant: ControlGrant;
  inherited_grant: ControlGrant;
  inherited_source: ControlSource;
  delta: GrantDelta;
}

export interface RecordedReviewRow {
  thread_id: string;
  thread_title: string;
  project_id: string;
  settled: boolean;
  review: ReviewRecord;
}

export interface ControlActivityRow {
  at: string;
  actor_thread_id: string;
  target_thread_id: string;
  action: string;
  detail: Record<string, unknown>;
}

export const TIER_LABELS: Record<ControlTier, { title: string; description: string }> = {
  observe: {
    title: "读取 (Observe)",
    description: "查看会话骨架与锚点（自动脱敏 + 防注入围栏）、Git / PR / 终端输出、监听状态与正式跨会话引用",
  },
  organize: {
    title: "整理 (Organize)",
    description: "置顶、待跟进、归置结案（settle）、重命名、归档会话，写入带证据锚点的审阅记录",
  },
  operate: {
    title: "运行 (Operate)",
    description: "启动和委派线程、排队/插话/定时发送消息、审批委派线程及本地 Git 提交",
  },
};

export const CATEGORY_LABELS: Record<ControlCategory, { title: string; description: string }> = {
  terminal: {
    title: "操控交互终端",
    description: "向会话内嵌终端创建 Shell、写入命令输入、发送中断或关闭终端",
  },
  git_remote: {
    title: "远端推送与分支合并",
    description: "执行 git push、git pull、切换分支或将子代理 Worktree 合并回主分支",
  },
  pr_write: {
    title: "PR 创建与评论写入",
    description: "通过 gh 在远端仓库创建 Pull Request 或发表 PR 评论",
  },
  history_edit: {
    title: "回滚轮次与移除排队消息",
    description: "回退会话历史轮次（rewind）或移除用户已排队的待发消息",
  },
  delete: {
    title: "永久删除会话",
    description: "彻底删除已归档会话及其关联终端、Worktree 与控制记录（不可恢复）",
  },
  answer_input: {
    title: "代答人类提问",
    description: "替用户回答非子代理会话等待的结构化提问或交互审批",
  },
  credentials: {
    title: "导入本机凭据",
    description: "从宿主机导入模型账号、凭据文件或 VPN 配置文件引用",
  },
  competition_submit: {
    title: "提交比赛 Flag",
    description: "向外部 CTF / 竞赛平台提交最终答案",
  },
  settings_write: {
    title: "修改全局设置",
    description: "修改工作台设置",
  },
};

export const CATEGORY_GROUPS: Array<{
  id: string;
  title: string;
  description: string;
  items: ControlCategory[];
}> = [
  {
    id: "exec",
    title: "执行与终端",
    description: "在目标会话交互式终端内执行命令或发送信号。",
    items: ["terminal"],
  },
  {
    id: "git_pr",
    title: "代码合并与远端协作",
    description: "影响共享分支、远端 Git 仓库或 GitHub Pull Request 的写操作。",
    items: ["git_remote", "pr_write"],
  },
  {
    id: "history_delete",
    title: "会话历史与永久删除",
    description: "改写已有对话轮次、丢弃人类排队消息或永久移除会话数据。",
    items: ["history_edit", "delete"],
  },
  {
    id: "identity_settings",
    title: "代答与设置",
    description: "回答其他线程中的用户提问，或修改显示偏好。",
    items: ["answer_input", "settings_write"],
  },
];

export const SOURCE_LABELS: Record<ControlSource, string> = {
  default: "全局默认",
  project: "项目覆盖",
  thread: "单会话覆盖",
  subagent: "子代理受限沙箱",
};

export function scopeLabel(scope: ControlScope): string {
  if (scope === "host") return "整台主机";
  if (scope === "project") return "仅当前项目";
  return `当前项目 + 指定 ${scope.projects.length} 个项目`;
}

export function describeGrant(grant: ControlGrant): string {
  const tiers = grant.tiers.map((tier) => TIER_LABELS[tier]?.title.replace(/\s*\(.*\)$/, "") ?? tier);
  const categories = grant.categories.map((category) => CATEGORY_LABELS[category]?.title ?? category);
  const parts = [...tiers, ...categories];
  return `${parts.length ? parts.join("、") : "无控制权限"}（${scopeLabel(grant.scope)}）`;
}

export function grantPresetId(grant: ControlGrant): "none" | "observe" | "organize" | "standard" | "custom" {
  if (grant.categories.length > 0 || grant.scope !== "project") return "custom";
  const t = [...grant.tiers].sort().join(",");
  if (t === "") return "none";
  if (t === "observe") return "observe";
  if (t === "observe,organize") return "organize";
  if (t === "observe,operate,organize") return "standard";
  return "custom";
}

export function grantFromPreset(preset: "observe" | "organize" | "standard"): ControlGrant {
  if (preset === "observe") return { scope: "project", tiers: ["observe"], categories: [] };
  if (preset === "organize") return { scope: "project", tiers: ["observe", "organize"], categories: [] };
  return { scope: "project", tiers: ["observe", "organize", "operate"], categories: [] };
}

export class ControlRequestError extends Error {
  readonly code: string;
  constructor(message: string, code = "") {
    super(message);
    this.name = "ControlRequestError";
    this.code = code;
  }
}

async function parse<T>(response: Response, action: string): Promise<T> {
  if (response.ok) return (await response.json()) as T;
  let message = `${action}失败（${response.status}）`;
  let code = "";
  try {
    const body = (await response.json()) as { error?: { code?: string; message?: string } };
    if (body.error?.message) message = body.error.message;
    code = body.error?.code ?? "";
  } catch {
  }
  throw new ControlRequestError(message, code);
}

const CONTROL_ERROR_TEXT: Record<string, string> = {
  "control.thread_busy": "线程仍在运行、等待审批或输入，或已归档，暂时不能收尾。",
  pull_request_host_disabled: "这个代码平台已在设置中停用。",
  pull_request_no_credential: "缺少可用的访问令牌，或令牌已失效。请到 设置 → 代码平台 连接。",
  pull_request_rate_limited: "代码平台暂时限制了请求频率，请稍后再试。",
  pull_request_not_found: "代码平台上找不到这个拉取请求，或当前令牌无权访问。",
  pull_request_too_large: "这个拉取请求内容过大，无法读取。",
  pull_request_invalid_read: "代码平台返回的数据无法识别。",
  pull_request_unknown_host: "这个地址不是已配置代码平台上的拉取请求。请检查地址，或先到 设置 → 代码平台 添加这个平台。",
  pull_request_no_repository: "只填编号时，当前项目的远程仓库需要位于已配置的代码平台。请改为粘贴完整地址。",
};

export function controlErrorCodeText(code: string | undefined): string | undefined {
  return code ? CONTROL_ERROR_TEXT[code] : undefined;
}

export function controlErrorText(cause: unknown, fallback: string): string {
  if (cause instanceof ControlRequestError && CONTROL_ERROR_TEXT[cause.code]) return CONTROL_ERROR_TEXT[cause.code];
  if (cause instanceof Error && cause.message) return cause.message;
  return fallback;
}

export async function fetchControlThread(threadId: string): Promise<ControlThreadGrant> {
  return parse(await apiFetch(`/api/control/threads/${encodeURIComponent(threadId)}`), "读取控制状态");
}

export async function putThreadGrant(threadId: string, grant: ControlGrant | null): Promise<ControlThreadGrant> {
  return parse(
    await apiFetch(`/api/control/threads/${encodeURIComponent(threadId)}/grant`, {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ grant }),
    }),
    grant ? "保存单会话授权" : "回收单会话授权",
  );
}

export async function answerGrantRequest(threadId: string, requestId: string, approve: boolean): Promise<void> {
  await parse(
    await apiFetch(`/api/control/threads/${encodeURIComponent(threadId)}/requests/${encodeURIComponent(requestId)}`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ approve }),
    }),
    approve ? "批准授权" : "拒绝授权",
  );
}

export async function addThreadReference(
  threadId: string,
  targetThreadId: string,
  reason = "",
  anchors: string[] = [],
): Promise<{ reference: ThreadReference }> {
  return parse(
    await apiFetch(`/api/control/threads/${encodeURIComponent(threadId)}/references`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ target_thread_id: targetThreadId, reason, anchors }),
    }),
    "添加跨会话引用",
  );
}

export async function removeThreadReference(threadId: string, targetThreadId: string): Promise<void> {
  await parse(
    await apiFetch(
      `/api/control/threads/${encodeURIComponent(threadId)}/references/${encodeURIComponent(targetThreadId)}`,
      { method: "DELETE" },
    ),
    "移除跨会话引用",
  );
}

export async function cancelSelfSettle(threadId: string): Promise<void> {
  await parse(
    await apiFetch(`/api/control/threads/${encodeURIComponent(threadId)}/self-settle`, {
      method: "DELETE",
    }),
    "取消预约结案",
  );
}

export async function fetchControlSettings(): Promise<ControlSettings> {
  return parse(await apiFetch("/api/control/settings"), "读取控制与编排设置");
}

export interface ControlSettingsPatch {
  enabled?: boolean;
  default_grant?: ControlGrant | null;
  project_overrides?: Record<string, ControlGrant>;
  subagent_policy?: SubagentPolicy;
  orchestration_mode?: OrchestrationWorkbenchMode;
  orchestration?: OrchestrationProfile | null;
  orchestration_overrides?: Partial<Record<OrchestrationWorkbenchMode, OrchestrationProfile | null>>;
}

export async function fetchOrchestrationProviders(): Promise<OrchestrationProvidersResponse> {
  return parse(await apiFetch("/api/control/orchestration/providers"), "读取可用 Agent 与模型列表");
}

export async function updateControlSettings(patch: ControlSettingsPatch): Promise<ControlSettings> {
  return parse(
    await apiFetch("/api/control/settings", {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(patch),
    }),
    "保存控制与编排设置",
  );
}

export async function fetchPendingGrantRequests(): Promise<PendingGrantRequestRow[]> {
  const body = await parse<{
    pending?: PendingGrantRequestRow[];
    requests?: Array<{
      thread_id: string;
      thread_title: string;
      project_id: string;
      is_subagent?: boolean;
      request_id?: string;
      grant?: ControlGrant;
      reason?: string;
      requested_at?: string;
      request?: ControlGrantRequest;
    }>;
  }>(
    await apiFetch("/api/control/pending-requests"),
    "读取待审批授权申请",
  );
  if (Array.isArray(body.pending)) return body.pending;
  return (body.requests ?? []).map((row) => ({
    thread_id: row.thread_id,
    thread_title: row.thread_title,
    project_id: row.project_id,
    is_subagent: Boolean(row.is_subagent),
    request: row.request ?? {
      request_id: String(row.request_id || ""),
      grant: row.grant ?? { scope: "project", tiers: [], categories: [] },
      reason: String(row.reason || ""),
      requested_at: String(row.requested_at || ""),
      status: "pending",
    },
  }));
}

export async function fetchCustomThreadGrants(): Promise<CustomThreadGrantRow[]> {
  const body = await parse<{
    thread_grants?: CustomThreadGrantRow[];
    threads?: CustomThreadGrantRow[];
  }>(
    await apiFetch("/api/control/thread-grants"),
    "读取单会话授权列表",
  );
  return body.threads ?? body.thread_grants ?? [];
}

export async function fetchControlReviews(limit = 50): Promise<{reviews: RecordedReviewRow[]; canonicalCount: number}> {
  const body = await parse<{
    canonical_count: number;
    reviews: Array<
      RecordedReviewRow & {
        lifecycle?: ThreadLifecycle;
      }
    >;
  }>(
    await apiFetch(`/api/control/reviews?limit=${limit}`),
    "读取审阅记录",
  );
  return {canonicalCount: body.canonical_count ?? 0, reviews: (body.reviews ?? []).map((row) => ({
    thread_id: row.thread_id,
    thread_title: row.thread_title,
    project_id: row.project_id,
    settled: Boolean(row.settled ?? row.lifecycle?.settled),
    review: row.review,
  }))};
}

export async function fetchControlDiscussions(parentThreadId?: string): Promise<PeerCouncilSession[]> {
  const qs = parentThreadId ? `?parent_thread_id=${encodeURIComponent(parentThreadId)}` : "";
  const body = await parse<{ discussions: PeerCouncilSession[] }>(
    await apiFetch(`/api/control/discussions${qs}`),
    "读取圆桌讨论记录",
  );
  return body.discussions ?? [];
}

export async function fetchControlDiscussion(groupId: string): Promise<PeerCouncilSession> {
  const body = await parse<PeerCouncilSession & { discussion?: PeerCouncilSession }>(
    await apiFetch(`/api/control/discussions/${encodeURIComponent(groupId)}`),
    "读取圆桌讨论详情",
  );
  return body.discussion ?? body;
}

export async function fetchControlActivity(limit = 60): Promise<ControlActivityRow[]> {
  const body = await parse<{ activity: ControlActivityRow[] }>(
    await apiFetch(`/api/control/activity?limit=${limit}`),
    "读取控制审计记录",
  );
  return body.activity ?? [];
}

export async function settleControlThread(threadId: string): Promise<void> {
  await parse(
    await apiFetch(`/api/control/threads/${encodeURIComponent(threadId)}/settle`, { method: "POST" }),
    "收尾线程",
  );
}

export async function dismissSettleSuggestion(threadId: string): Promise<void> {
  await parse(
    await apiFetch(`/api/control/threads/${encodeURIComponent(threadId)}/settle-suggestion`, { method: "DELETE" }),
    "暂不收尾",
  );
}

export type PullRequestChecks = "passing" | "failing" | "pending";
export type PullRequestReview = "approved" | "changes_requested" | "required";
export type PullRequestMergeability = "clean" | "conflicting" | "pending" | "unknown";
export type PullRequestSyncError =
  | { reason: "no_credential" | "host_disabled" | "not_found" | "failed" }
  | { reason: "rate_limited"; retry_at: number };

export interface PullRequestSnapshot {
  state: "open" | "closed" | "merged";
  title: string;
  head_branch: string;
  base_branch: string;
  is_draft: boolean;
  updated_at?: string | null;
  synced_at: number;
  author?: { login: string; avatar_url?: string | null } | null;
  additions?: number | null;
  deletions?: number | null;
  changed_files?: number | null;
  review_decision?: PullRequestReview | null;
  checks_state?: PullRequestChecks | null;
  mergeability: PullRequestMergeability;
}

export interface ThreadPullRequestLink {
  key: { host: string; repository: string; number: number };
  source: string;
  url: string;
  linked_at?: number;
  snapshot?: PullRequestSnapshot;
  sync_error?: PullRequestSyncError;
  watch?: { started_at: string } | null;
}

export type PullRequestLinkOp = "link" | "unlink" | "watch" | "unwatch" | "refresh";

function pullRequestsPath(threadId: string): string {
  return `/api/control/threads/${encodeURIComponent(threadId)}/pull-requests`;
}

export function pullRequestLabel(key: ThreadPullRequestLink["key"]): string {
  return `${key.host}/${key.repository}#${key.number}`;
}

export async function fetchThreadPullRequestsWithState(threadId: string): Promise<{
  pull_requests: ThreadPullRequestLink[];
  pull_request_watches_paused: boolean;
}> {
  const body = await parse<{
    pull_requests?: ThreadPullRequestLink[];
    pull_request_watches_paused?: boolean;
  }>(
    await apiFetch(pullRequestsPath(threadId)),
    "读取拉取请求",
  );
  return {
    pull_requests: body.pull_requests ?? [],
    pull_request_watches_paused: Boolean(body.pull_request_watches_paused),
  };
}

export async function fetchThreadPullRequests(threadId: string): Promise<ThreadPullRequestLink[]> {
  const state = await fetchThreadPullRequestsWithState(threadId);
  return state.pull_requests;
}

export async function setPullRequestWatchesPaused(threadId: string, paused: boolean): Promise<boolean> {
  const body = await parse<{ pull_request_watches_paused?: boolean }>(
    await apiFetch(`${pullRequestsPath(threadId)}/watches-paused`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ paused }),
    }),
    paused ? "暂停拉取请求监听" : "恢复拉取请求监听",
  );
  return Boolean(body.pull_request_watches_paused);
}

export async function readPullRequest(
  threadId: string,
  key: string,
  read: { type: string; content?: unknown },
): Promise<{
  key: ThreadPullRequestLink["key"];
  capabilities: Record<string, unknown>;
  terms: Record<string, string>;
  response: { type: string; content?: unknown };
}> {
  return parse(
    await apiFetch(`${pullRequestsPath(threadId)}/read`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ key, read }),
    }),
    "读取拉取请求详情",
  );
}

export async function fetchPullRequestStack(threadId: string, key: string): Promise<{
  key: ThreadPullRequestLink["key"];
  stack: Record<string, unknown>;
}> {
  return parse(
    await apiFetch(`${pullRequestsPath(threadId)}/stack`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ key }),
    }),
    "读取拉取请求栈",
  );
}

export async function setPullRequestFilesViewed(
  threadId: string,
  key: string,
  paths: string[],
  viewed: boolean,
): Promise<void> {
  await parse(
    await apiFetch(`${pullRequestsPath(threadId)}/viewed`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ key, paths, viewed }),
    }),
    "更新文件已读状态",
  );
}

export async function submitPullRequestReview(
  threadId: string,
  key: string,
  verdict: "Comment" | "Approve" | "RequestChanges",
  head: string,
  body: string,
  comments: Array<Record<string, unknown>> = [],
): Promise<{ outcome: { type: string; content?: unknown } }> {
  return parse(
    await apiFetch(`${pullRequestsPath(threadId)}/review`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ key, verdict, head, body, comments }),
    }),
    "提交审阅",
  );
}

export async function runPullRequestHostAction(
  threadId: string,
  key: string,
  action: { type: string; content?: unknown },
): Promise<{ outcome: { type: string; content?: unknown } }> {
  return parse(
    await apiFetch(`${pullRequestsPath(threadId)}/host-action`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ key, action }),
    }),
    "执行拉取请求操作",
  );
}

export async function threadPullRequestAction(
  threadId: string,
  op: PullRequestLinkOp,
  target: { reference: string } | { key: string },
): Promise<void> {
  await parse(
    await apiFetch(pullRequestsPath(threadId), {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ op, ...target }),
    }),
    "更新拉取请求",
  );
}
