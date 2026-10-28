/**
 * Shared object model for the workbench prototype. Every view renders these
 * shapes; the demo data in demoData.ts is only one source of them.
 */

export type ClueCategory = "person" | "place" | "timeline" | "turn" | "detail";
export type ClueStatus = "unverified" | "verified" | "doubtful";
export type SourceKind = "interview" | "field" | "archive" | "minutes";
export type TaskStatus = "todo" | "doing" | "done";
export type FindingSeverity = "critical" | "high" | "medium" | "low";
export type ReadinessLevel = "GREEN" | "YELLOW" | "RED";
export type AssetImportance = "core" | "high" | "normal" | "low";

export interface StudioProject {
  id: string;
  title: string;
  summary: string;
  company?: string;
  longTermGoal?: string;
}

export interface StudioPassage {
  id: string;
  sourceId: string;
  /** 1-based paragraph number shown in citations. */
  index: number;
  text: string;
}

export interface AssetObservation {
  id: string;
  sourceKind: "import" | "connector" | "run" | "manual";
  sourceLabel: string;
  batchOrRunId: string;
  observedAt: string;
  ingestedAt: string;
  artifactRef: string;
  sha256: string;
  summary: string;
  fields: Record<string, string>;
}

export interface AssetVersionSnapshot {
  version: number;
  label: string;
  createdAt: string;
  sourceLabel: string;
  summary: string;
  attributes: Record<string, string>;
  passageCount: number;
  bodyPreview: string;
}

export interface AssetFieldConflict {
  id: string;
  field: string;
  fieldLabel: string;
  selectedValue: string;
  resolutionBasis: string;
  resolvedBy?: "rule" | "manual" | "ai";
  resolvedAt?: string;
  candidates: Array<{
    source: string;
    value: string;
    observedAt: string;
    artifactRef: string;
  }>;
}

export interface AssetRelationItem {
  id: string;
  direction: "outbound" | "inbound";
  relationType: string;
  targetId: string;
  targetLabel: string;
  targetKind: "source" | "endpoint" | "ip" | "app";
  origin: "observed" | "derived" | "inferred";
  evidenceRef: string;
  observedAt: string;
}

export interface StudioSource {
  id: string;
  projectId: string;
  title: string;
  kind: SourceKind;
  author: string;
  collectedAt: string;
  status: "filed" | "inbox";
  passages: StudioPassage[];
  /** Extended SRC asset repository fields (all optional with safe defaults). */
  canonicalKey?: string;
  aliases?: string[];
  importance?: AssetImportance;
  tags?: string[];
  firstSeenAt?: string;
  lastObservedAt?: string;
  lastVerifiedAt?: string;
  lastFailedAt?: string;
  lastFailedReason?: string;
  archived?: boolean;
  archivedAt?: string;
  archivedReason?: string;
  unlinkedTaskIds?: string[];
  pinnedVersion?: number;
  publishedVersion?: number;
  reusedFromTaskId?: string;
  attributes?: Record<string, string>;
  sourceHistory?: AssetObservation[];
  versions?: AssetVersionSnapshot[];
  conflicts?: AssetFieldConflict[];
  relations?: AssetRelationItem[];
}

export interface FindingClaimItem {
  id: string;
  text: string;
  kind: "observed" | "inferred" | "unverified";
  evidenceIds: string[];
  note?: string;
}

export interface StudioClue {
  id: string;
  projectId: string;
  title: string;
  category: ClueCategory;
  status: ClueStatus;
  tags: string[];
  sourceId: string;
  passageId: string;
  /** Exact substring of the passage text; the reader highlights it. */
  quote: string;
  /** Extended SRC finding dossier fields. */
  severity?: FindingSeverity;
  cvssVector?: string;
  cvssScore?: number;
  severityBasis?: string;
  impactSummary?: string;
  conditionalImpact?: string;
  affectedSourceIds?: string[];
  claims?: FindingClaimItem[];
  supportingEvidenceIds?: string[];
  counterEvidence?: string[];
  missingMaterials?: string[];
  targetVersion?: string;
  reviewer?: string;
  reviewedAt?: string;
  validationType?: "live_reproduction" | "artifact_review" | "unverified";
  readiness?: ReadinessLevel;
  readinessReasons?: string[];
  stale?: boolean;
  staleReason?: string;
  sourceRunId?: string;
}

export type StudioRefKind =
  | "source"
  | "passage"
  | "clue"
  | "task"
  | "report"
  | "evidence"
  | "import"
  | "run"
  | "change"
  | "feedback"
  | "plan";

export interface StudioRef {
  kind: StudioRefKind;
  id: string;
}

export interface StudioTask {
  id: string;
  projectId: string;
  title: string;
  assignee: string;
  due: string;
  status: TaskStatus;
  links: StudioRef[];
  threadId?: string;
  originKind?: "manual" | "finding" | "feedback" | "change" | "ai";
  notes?: string;
}

// ---------------------------------------------------------------------------
// Module 1: Long-term Task Dossier & Plan Versions
// ---------------------------------------------------------------------------

export interface StudioPlanQuestion {
  id: string;
  question: string;
  status: "open" | "investigating" | "answered" | "blocked";
  priority: "high" | "medium" | "low";
  rationale: string;
  linkedRefs: StudioRef[];
}

export interface StudioScopeMaterial {
  id: string;
  projectId: string;
  name: string;
  kind: "policy_doc" | "openapi" | "js_bundle" | "asset_snapshot" | "scope_rules";
  version: string;
  digest: string;
  readable: boolean;
  unreadableReason?: string;
  collectedAt: string;
  summary: string;
  excerpt: string;
}

export interface StudioPlanVersion {
  id: string;
  projectId: string;
  version: number;
  createdAt: string;
  author: string;
  goal: string;
  focusQuestion: string;
  revisionBasis: string;
  triggerRefs: StudioRef[];
  currentJudgement: string;
  materialGaps: string[];
  proposedActions: string[];
  scopeVersionId: string;
}

// ---------------------------------------------------------------------------
// Module 7: Standalone Evidence & Attachments
// ---------------------------------------------------------------------------

export type EvidenceKind =
  | "http_exchange"
  | "screenshot"
  | "file_artifact"
  | "poc_script"
  | "oast_callback"
  | "ws_stream";

export type EvidenceCompleteness = "complete" | "partial" | "unavailable" | "revoked";

export interface StudioEvidence {
  id: string;
  projectId: string;
  title: string;
  kind: EvidenceKind;
  sourceKind: "browser" | "http_tool" | "proxy" | "import" | "connector";
  sourceRunId?: string;
  actorRef: string;
  capturedAt: string;
  version: string;
  sha256: string;
  digestVerified: "verified" | "mismatch" | "unchecked";
  size: number;
  mediaType: string;
  completeness: EvidenceCompleteness;
  unreadableReason?: string;
  linkedSourceIds: string[];
  linkedClueIds: string[];
  method?: string;
  declaredUrl?: string;
  observedUrl?: string;
  statusCode?: number;
  protocol?: string;
  requestHeaders?: Array<[string, string]>;
  responseHeaders?: Array<[string, string]>;
  requestRaw?: string;
  responseRaw?: string;
  baselineExchangeId?: string;
  captureUrl?: string;
  dimensions?: string;
  browserGeneration?: number;
  imageReviewedBy?: string;
  imageReviewedAt?: string;
  visualPreviewData?: {
    caption: string;
    domHighlight: string;
    consoleLog: string;
    networkSummary: string;
  };
  content?: string;
  diffBefore?: string;
  denoisedNote?: string;
}

// ---------------------------------------------------------------------------
// Module 8: External Work Records (Runs) & Progress Flowback
// ---------------------------------------------------------------------------

export type WorkRunState = "running" | "completed" | "blocked" | "failed" | "waiting_approval";

export interface StudioWorkRun {
  id: string;
  projectId: string;
  title: string;
  engine: "pentest_swarm" | "asset_recon" | "browser_verify" | "retest_runner" | "mcp_agent";
  targetUrl: string;
  question: string;
  state: WorkRunState;
  generation: number;
  identityContext: string;
  planVersionId: string;
  authVersionId: string;
  startedAt: string;
  updatedAt: string;
  blockerReason?: string;
  logs: Array<{
    at: string;
    step: string;
    status: "ok" | "warn" | "error" | "running";
    detail: string;
  }>;
  artifactRefs: StudioRef[];
  linkedRefs: StudioRef[];
  flowedBackThreadIds: string[];
}

// ---------------------------------------------------------------------------
// Module 9, 10, 11: Report Delivery, Submission/Bounty Ledger & Retest
// ---------------------------------------------------------------------------

export type ReportTemplateId = "bsrc" | "general_src" | "butian" | "edusrc";

export interface StudioReportTemplate {
  id: ReportTemplateId;
  name: string;
  platform: string;
  version: string;
  requiredFields: string[];
  description: string;
}

export type SubmissionStatus =
  | "not_submitted"
  | "submitted"
  | "needs_info"
  | "accepted"
  | "duplicate"
  | "rejected";

export type RewardStatus = "none" | "pending_confirm" | "confirmed" | "paid";

export interface StudioSubmissionLedger {
  status: SubmissionStatus;
  externalTicketId: string;
  platformName: string;
  submittedAt?: string;
  submittedBy?: string;
  rewardStatus: RewardStatus;
  rewardAmount?: number;
  rewardCurrency: string;
  paidAt?: string;
  notes: string;
  auditTrail: Array<{
    at: string;
    by: string;
    fromStatus: string;
    toStatus: string;
    note: string;
  }>;
}

export interface StudioReport {
  id: string;
  projectId: string;
  findingId: string;
  title: string;
  version: number;
  status: "draft" | "frozen" | "superseded" | "invalidated";
  supersedesReportId?: string;
  templateId: ReportTemplateId;
  createdAt: string;
  updatedAt: string;
  frozenAt?: string;
  frozenBy?: string;
  targetVersion: string;
  scopeRef: string;
  planRef: string;
  readiness: ReadinessLevel;
  sections: {
    scopeAndTarget: string;
    preconditions: string;
    mechanism: string;
    normalVsActual: string;
    steps: string[];
    pocScript: string;
    pocVerified: boolean;
    confirmedImpact: string;
    conditionalImpact: string;
    cvssVector: string;
    cvssScore: number;
    remediation: string;
    limitations: string;
  };
  evidenceIds: string[];
  materialGaps: string[];
  strangerChecklist: Array<{
    item: string;
    passed: boolean;
    note: string;
  }>;
  submission: StudioSubmissionLedger;
}

export interface StudioPlatformFeedback {
  id: string;
  projectId: string;
  reportId: string;
  findingId: string;
  externalTicketId: string;
  platformAuthor: string;
  receivedAt: string;
  kind: "needs_more_evidence" | "fix_claim" | "scope_question" | "accepted_notice" | "duplicate_notice";
  rawText: string;
  attachments: Array<{
    name: string;
    mediaType: string;
    size: number;
    summary: string;
    evidenceId?: string;
  }>;
  status: "pending" | "in_progress" | "resolved";
  followUpTaskId?: string;
  updatedReportId?: string;
}

export type RetestVerdict = "fixed" | "still_vulnerable" | "inconclusive";

export interface StudioRetestRecord {
  id: string;
  projectId: string;
  findingId: string;
  baseReportId: string;
  newReportId?: string;
  feedbackId?: string;
  runId?: string;
  targetFixVersion: string;
  result: RetestVerdict;
  prerequisiteCheck: string;
  normalControlCheck: string;
  basisAndEvidence: string;
  evidenceIds: string[];
  reviewer: string;
  reviewedAt: string;
}

// ---------------------------------------------------------------------------
// Module 12: Changes & Continuous Monitoring
// ---------------------------------------------------------------------------

export interface StudioChangeRecord {
  id: string;
  projectId: string;
  monitorId: string;
  cycleId: string;
  chainType: "asset" | "rule";
  changeKind: "added" | "modified" | "removed_candidate" | "rule_updated" | "incomparable";
  title: string;
  observedAt: string;
  beforeVersion: string;
  afterVersion: string;
  baselineCycleId: string;
  comparable: boolean;
  incomparableReason?: string;
  rawDiff: string;
  denoisedDiff: string;
  ignoredNoiseFields: string[];
  affectedAssetIds: string[];
  affectedFindingIds: string[];
  affectedReportIds: string[];
  reviewCandidateStatus: "pending" | "queued" | "dismissed";
}

export interface StudioMonitorCycle {
  id: string;
  startedAt: string;
  finishedAt: string;
  status: "completed" | "partial" | "failed" | "incomparable";
  summary: string;
  added: number;
  changed: number;
  missingPages?: number;
  failureReason?: string;
  costUsed: number;
  advancedBaseline: boolean;
}

export interface StudioMonitor {
  id: string;
  projectId: string;
  name: string;
  chainType: "asset" | "rule";
  status: "active" | "paused";
  queryExpression: string;
  queryVersion: string;
  frequencyLabel: string;
  intervalSeconds: number;
  budgetPerCycle: number;
  budgetTotal: number;
  budgetUsed: number;
  baselineCycleId: string;
  lastSuccessAt: string;
  cycles: StudioMonitorCycle[];
}

// ---------------------------------------------------------------------------
// Module 13: Scope, Approvals & Environment Status
// ---------------------------------------------------------------------------

export interface StudioScopeSnapshot {
  id: string;
  projectId: string;
  version: number;
  createdAt: string;
  author: string;
  allowedTargets: string[];
  excludedTargets: string[];
  conditions: string[];
  identities: Array<{
    id: string;
    name: string;
    role: string;
    tenant: string;
    status: "verified" | "expired" | "unverified";
    lastCheckedAt: string;
  }>;
  rawPolicyText: string;
  diffSummary: string;
}

export interface StudioApproval {
  id: string;
  projectId: string;
  runId?: string;
  toolName: string;
  action: string;
  resourceKind: "file" | "http" | "browser" | "network" | "mcp";
  target: string;
  identity: string;
  cwd: string;
  inputDigest: string;
  argumentsPreview: string;
  matchedRuleId: string;
  matchedRuleReason: string;
  status: "pending" | "approved_once" | "approved_grant" | "denied" | "expired" | "superseded";
  requestedAt: string;
  expiresAt: string;
  decidedBy?: string;
  decidedAt?: string;
  decisionReason?: string;
  grantId?: string;
}

export interface StudioGrant {
  id: string;
  projectId: string;
  ruleId: string;
  resourceSelector: string;
  actions: string[];
  identityConstraint: string;
  sourceApprovalId: string;
  createdBy: string;
  createdAt: string;
  expiresAt: string;
  status: "active" | "expired" | "revoked";
  revokedAt?: string;
  revokedReason?: string;
}

export interface StudioEnvironmentStatus {
  sandboxBoundary: string;
  networkScopeMode: "declared" | "enforced";
  httpInspectionMode: "off" | "record" | "enforce";
  channels: Array<{
    id: string;
    name: string;
    status: "enforced" | "declared_only" | "record_only" | "unavailable";
    trustedFields: string;
    boundary: string;
    limitation: string;
  }>;
}

// ---------------------------------------------------------------------------
// Module 3 & 15: External Connectors & Simulated Services
// ---------------------------------------------------------------------------

export interface StudioConnector {
  id: string;
  name: string;
  category: "search_engine" | "recon_pipeline" | "proxy_import" | "asset_repo" | "mcp_plugin";
  version: string;
  status: "ready" | "degraded" | "quota_limited";
  capabilities: string[];
  supportedAssetTypes: string[];
  defaultQuery: string;
  quotaRemaining: number;
  quotaTotal: number;
  costPerSync: number;
  completenessGuarantee: string;
  lastSyncedAt?: string;
  description: string;
}

// ---------------------------------------------------------------------------
// Labels and Helpers
// ---------------------------------------------------------------------------

export const CLUE_CATEGORIES: Array<{ value: ClueCategory; label: string }> = [
  { value: "person", label: "访问边界" },
  { value: "place", label: "服务策略" },
  { value: "timeline", label: "会话周期" },
  { value: "turn", label: "角色模型" },
  { value: "detail", label: "配置数据" },
];

export const CLUE_STATUSES: Array<{ value: ClueStatus; label: string }> = [
  { value: "unverified", label: "复核中" },
  { value: "verified", label: "已复核" },
  { value: "doubtful", label: "待补证" },
];

export const SOURCE_KINDS: Array<{ value: SourceKind; label: string }> = [
  { value: "interview", label: "子域名" },
  { value: "field", label: "Web 服务" },
  { value: "archive", label: "API 服务" },
  { value: "minutes", label: "CDN 节点" },
];

export const TASK_STATUSES: Array<{ value: TaskStatus; label: string }> = [
  { value: "doing", label: "进行中" },
  { value: "todo", label: "待开始" },
  { value: "done", label: "已完成" },
];

export const FINDING_SEVERITIES: Array<{ value: FindingSeverity; label: string; tone: "danger" | "warning" | "accent" | "neutral" }> = [
  { value: "critical", label: "严重", tone: "danger" },
  { value: "high", label: "高危", tone: "warning" },
  { value: "medium", label: "中危", tone: "accent" },
  { value: "low", label: "低危", tone: "neutral" },
];

export const READINESS_LEVELS: Array<{ value: ReadinessLevel; label: string; tone: "success" | "warning" | "danger" }> = [
  { value: "GREEN", label: "可交付", tone: "success" },
  { value: "YELLOW", label: "待补充", tone: "warning" },
  { value: "RED", label: "不可交付", tone: "danger" },
];

export const ASSET_IMPORTANCE_LEVELS: Array<{ value: AssetImportance; label: string }> = [
  { value: "core", label: "核心资产" },
  { value: "high", label: "重要资产" },
  { value: "normal", label: "常规资产" },
  { value: "low", label: "边缘资产" },
];

export const SUBMISSION_STATUSES: Array<{ value: SubmissionStatus; label: string; tone: "neutral" | "accent" | "warning" | "success" | "danger" }> = [
  { value: "not_submitted", label: "未提交", tone: "neutral" },
  { value: "submitted", label: "已人工提交", tone: "accent" },
  { value: "needs_info", label: "平台待补证", tone: "warning" },
  { value: "accepted", label: "平台已确认", tone: "success" },
  { value: "duplicate", label: "平台判重复", tone: "warning" },
  { value: "rejected", label: "平台已拒绝", tone: "danger" },
];

export const REWARD_STATUSES: Array<{ value: RewardStatus; label: string; tone: "neutral" | "warning" | "accent" | "success" }> = [
  { value: "none", label: "未评定奖励", tone: "neutral" },
  { value: "pending_confirm", label: "待确认金额", tone: "warning" },
  { value: "confirmed", label: "奖励已确认", tone: "accent" },
  { value: "paid", label: "奖金已到账", tone: "success" },
];

export const RETEST_VERDICTS: Array<{ value: RetestVerdict; label: string; tone: "success" | "danger" | "warning" }> = [
  { value: "fixed", label: "已解决", tone: "success" },
  { value: "still_vulnerable", label: "仍存在", tone: "danger" },
  { value: "inconclusive", label: "无法判断", tone: "warning" },
];

export const EVIDENCE_KIND_LABELS: Record<EvidenceKind, string> = {
  http_exchange: "HTTP 原始报文",
  screenshot: "浏览器截图证据",
  file_artifact: "原始文件/配置",
  poc_script: "可运行 PoC 脚本",
  oast_callback: "OAST 异步回连",
  ws_stream: "WS/SSE 协议流",
};

export function categoryLabel(category: ClueCategory): string {
  return CLUE_CATEGORIES.find((item) => item.value === category)?.label ?? category;
}

export function statusLabel(status: ClueStatus): string {
  return CLUE_STATUSES.find((item) => item.value === status)?.label ?? status;
}

export function sourceKindLabel(kind: SourceKind): string {
  return SOURCE_KINDS.find((item) => item.value === kind)?.label ?? kind;
}

export function severityLabel(severity?: FindingSeverity): string {
  return FINDING_SEVERITIES.find((item) => item.value === severity)?.label ?? "高危";
}

export function severityTone(severity?: FindingSeverity): "danger" | "warning" | "accent" | "neutral" {
  return FINDING_SEVERITIES.find((item) => item.value === severity)?.tone ?? "warning";
}

export function readinessLabel(readiness?: ReadinessLevel): string {
  return READINESS_LEVELS.find((item) => item.value === readiness)?.label ?? "待补充";
}

export function readinessTone(readiness?: ReadinessLevel): "success" | "warning" | "danger" {
  return READINESS_LEVELS.find((item) => item.value === readiness)?.tone ?? "warning";
}

export function importanceLabel(importance?: AssetImportance): string {
  return ASSET_IMPORTANCE_LEVELS.find((item) => item.value === importance)?.label ?? "常规资产";
}

export function submissionStatusLabel(status: SubmissionStatus): string {
  return SUBMISSION_STATUSES.find((item) => item.value === status)?.label ?? status;
}

export function rewardStatusLabel(status: RewardStatus): string {
  return REWARD_STATUSES.find((item) => item.value === status)?.label ?? status;
}

export function retestVerdictLabel(verdict: RetestVerdict): string {
  return RETEST_VERDICTS.find((item) => item.value === verdict)?.label ?? verdict;
}

export function statusTone(status: ClueStatus): "neutral" | "success" | "warning" {
  if (status === "verified") return "success";
  if (status === "doubtful") return "warning";
  return "neutral";
}

export function sameRef(a: StudioRef, b: StudioRef): boolean {
  return a.kind === b.kind && a.id === b.id;
}
