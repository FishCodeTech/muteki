import type {
  StudioApproval, StudioChangeRecord, StudioEnvironmentStatus, StudioGrant,
  StudioMonitor, StudioPlanQuestion, StudioPlanVersion, StudioScopeMaterial,
  StudioScopeSnapshot, StudioTask, StudioWorkRun, StudioRef,
} from "../model";
import type { TaskRun } from "../store";
import type { HistoryEntry } from "../records";
import { postCommand, querySrc } from "../../src/reviewPrimitives";
import { recordListAll, type DomainCtx, type RealDomain } from "./domain";

type Api = Record<string, unknown>;
const text = (v: unknown) => (typeof v === "string" ? v : "");
const num = (v: unknown, d = 0) => (Number.isFinite(Number(v)) ? Number(v) : d);
const iso = (v: unknown) => text(v) || "";

const TODO_STATUS_FROM_SERVER: Record<string, StudioTask["status"]> = {
  open: "todo",
  in_progress: "doing",
  done: "done",
  todo: "todo",
  doing: "doing",
};

const TODO_STATUS_TO_SERVER: Record<string, string> = {
  todo: "open",
  doing: "in_progress",
  done: "done",
  open: "open",
  in_progress: "in_progress",
};

function formatSelector(selector: unknown): string {
  if (typeof selector === "string") return selector;
  if (!selector || typeof selector !== "object") return "";
  const obj = selector as Api;
  const urlVal = typeof obj.url === "string" ? obj.url : text((obj.url as Api | undefined)?.target);
  return [text(obj.tool_name), urlVal, text(obj.cwd)].filter(Boolean).join(" · ");
}

function formatIdentityConstraint(value: unknown): string {
  if (typeof value === "string") return value;
  if (!value || typeof value !== "object") return "";
  const obj = value as Api;
  return text(obj.identity_ref) || Object.entries(obj).map(([k, v]) => `${k}=${String(v)}`).join(" · ");
}

async function recordHistory(ctx: DomainCtx, undo: { op: string; params?: Record<string, unknown> }, fallback: { action: string; summary: string; refs?: StudioRef[] }) {
  const entry = (ctx.proposal?.() ?? ctx.state()).history[0];
  const payload = entry && entry.summary === fallback.summary
    ? { id: entry.id, action: entry.action, summary: entry.summary, refs: entry.refs, at: entry.at }
    : { id: `wb-${crypto.randomUUID().slice(0, 8)}`, action: fallback.action, summary: fallback.summary, refs: fallback.refs ?? [], at: new Date().toISOString() };
  await postCommand(ctx.taskId, "src.workbench.history.record", { entry: { ...payload, undo, domain: "tasks" } });
}

const RUN_STATES = ["queued", "planning", "running", "paused", "frozen", "stopped", "blocked", "completed", "failed", "archived", "waiting_approval"] as const;

export interface MonitorSchedulerHealth {
  state?: string;
  scheduler_running?: boolean;
  active_monitors?: number;
  unresolved_cycles?: number;
  consecutive_failures?: number;
  restart_count?: number;
  last_error?: string | null;
  last_result?: unknown;
  external_notifications?: boolean;
}

export interface ScopeSnapshotMeta {
  policyVersion: number;
  addedTargets: string[];
  removedTargets: string[];
  notes: string;
}

const schedulerHealthByTask = new Map<string, MonitorSchedulerHealth>();
const monitorRevisionById = new Map<string, number>();
const scopeMetaById = new Map<string, ScopeSnapshotMeta>();
const policyMetaByTask = new Map<string, { policyId: string; version: number }>();

export function getMonitorSchedulerHealth(taskId: string): MonitorSchedulerHealth | null {
  return taskId ? (schedulerHealthByTask.get(taskId) ?? null) : null;
}

export function getScopeSnapshotMeta(snapshotId: string): ScopeSnapshotMeta | null {
  return snapshotId ? (scopeMetaById.get(snapshotId) ?? null) : null;
}

export function getPolicyMeta(taskId: string): { policyId: string; version: number } | null {
  return taskId ? (policyMetaByTask.get(taskId) ?? null) : null;
}

function formatStructuredDiff(value: unknown): string {
  if (typeof value === "string") return value;
  if (value == null) return "";
  try {
    return JSON.stringify(value, null, 2);
  } catch {
    return String(value);
  }
}

function parseTodoExecutor(raw: unknown): StudioTask["executor"] | undefined {
  if (!raw || typeof raw !== "object") return undefined;
  const obj = raw as Api;
  const kind = text(obj.kind);
  if (kind !== "human" && kind !== "subthread" && kind !== "run") return undefined;
  const id = text(obj.id) || text(obj.thread_id) || text(obj.run_id) || undefined;
  return { kind, ...(id ? { id } : {}) };
}

export function createTasksDomain(): RealDomain {
  return {
    id: "tasks",
    pollMs: 5000,
    owns: ["tasks", "planVersions", "planQuestions", "scopeMaterials", "scopeSnapshots", "workRuns", "taskRuns", "changes", "monitors", "approvals", "grants", "environmentStatus"],
    combines: {
      history: (local, server) => {
        const serverIds = new Set((server as HistoryEntry[]).map((e) => e.id));
        return [...(server as HistoryEntry[]), ...(local as HistoryEntry[]).filter((e) => e.serverDomain !== "tasks" && !serverIds.has(e.id))];
      },
    },
    async load(taskId, ctx) {
      const [todoRows, questionRows, planRows, runRows, monitorRows, changeRows, approvalRows, grantRows, authRows, histPage, policy, caps, schedulerHealth] = await Promise.all([
        querySrc<{ items?: Api[] }>(taskId, "src.workbench.todo.list", {}),
        recordListAll(taskId, "research_question"),
        recordListAll(taskId, "research_plan"),
        querySrc<Api[] | { items?: Api[] }>(taskId, "src.run.list", {}),
        querySrc<{ items?: Api[]; monitors?: Api[] } | Api[]>(taskId, "src.monitor.list", {}),
        recordListAll(taskId, "monitor_change"),
        recordListAll(taskId, "execution_approval"),
        recordListAll(taskId, "task_grant"),
        recordListAll(taskId, "authorization_snapshot"),
        querySrc<{ items?: Api[] }>(taskId, "src.workbench.history", { limit: 1000 }),
        querySrc<Api>(taskId, "src.policy.current", {}),
        querySrc<Api>(taskId, "src.execution.capabilities", {}),
        querySrc<MonitorSchedulerHealth>(taskId, "src.monitor.scheduler_health", {}),
      ]);
      schedulerHealthByTask.set(taskId, schedulerHealth ?? {});
      const projectId = ctx.projectId;
      const runs = (Array.isArray(runRows) ? runRows : runRows.items ?? []) as Api[];
      const workRuns: StudioWorkRun[] = runs.map((r) => {
        const b = (r.binding && typeof r.binding === "object" ? (r.binding as Api) : r);
        const snap = (r.run && typeof r.run === "object" ? (r.run as Api) : r);
        const contract = (b.contract && typeof b.contract === "object" ? (b.contract as Api) : {});
        const rawState = text(snap.state) || text(snap.status) || text(r.state);
        const controlMode = text(snap.control_mode) || text(r.control_mode);
        const terminalStates = new Set(["completed", "failed", "archived", "stopped"]);
        const mappedState = !terminalStates.has(rawState) && (controlMode === "frozen" || controlMode === "paused" || controlMode === "stopped")
          ? controlMode
          : rawState === "starting"
          ? "running"
          : RUN_STATES.includes(rawState as never)
          ? rawState
          : "queued";
        const runId = text(b.run_id) || text(snap.run_id) || text(r.run_id) || text(r.record_id);
        const targetUrl = text(b.target_url) || text(contract.target) || text(r.target) || text(r.target_url);
        const question = text(contract.question) || text(snap.question) || text(r.question) || text(r.goal);
        return {
          id: runId, projectId,
          title: question || targetUrl || runId, engine: "pentest_swarm",
          targetUrl, question,
          state: mappedState as StudioWorkRun["state"],
          generation: num(snap.generation) || num(b.binding_revision) || 1,
          identityContext: text((b.identity as Api)?.context_id) || text(r.identity_ref),
          planVersionId: text(b.plan_ref), authVersionId: text(b.authorization_ref),
          startedAt: iso(snap.started_at) || iso(snap.created_at) || iso(b.created_at),
          updatedAt: iso(snap.updated_at) || iso(snap.started_at) || iso(b.created_at),
          blockerReason: text(snap.blocked_reason) || text(snap.failure_reason) || text((snap.detail as Api)?.error) || undefined,
          logs: ((r.logs as Api[]) ?? []).map((l, i) => ({ at: iso(l.at), step: text(l.step) || text(l.title) || `step-${i}`, status: "ok" as const, detail: text(l.detail) })),
          linkedRefs: (r.linked_refs as StudioRef[]) ?? [],
          artifactRefs: [],
          flowedBackThreadIds: [],
        } as StudioWorkRun;
      });
      const liveRunIds = new Set(workRuns.filter((r) => ["running", "queued", "planning"].includes(r.state)).map((r) => r.id));
      const terminalSubStates = new Set(["completed", "failed", "cancelled", "canceled", "killed", "error", "stopped", "settled", "archived", "done"]);
      const liveSubagentList = (ctx.state().liveSubagents ?? []).filter((s) => {
        const delivery = (s.delivery || "").toLowerCase();
        if (delivery === "running") return true;
        if (delivery === "awaiting_settle" || delivery === "settled" || delivery === "not_delivered") return false;
        const exec = (s.execution || "").toLowerCase();
        if (exec === "running" || exec === "working" || exec === "background") return true;
        return !terminalSubStates.has((s.status || "").toLowerCase());
      });
      const tasks: StudioTask[] = (todoRows.items ?? []).filter((t) => t.status !== "deleted").map((t) => {
        const todoId = text(t.todo_id);
        const status = TODO_STATUS_FROM_SERVER[text(t.status)] ?? "todo";
        const executor = parseTodoExecutor(t.executor);
        const waitingFor = text(t.waiting_for) || undefined;
        const blockerReason = text(t.blocker_reason) || undefined;
        const reportBack = text(t.report_back) || undefined;
        const serverExecState = text(t.execution_state);
        const hasLiveRun = executor?.kind === "run" && Boolean(executor.id && liveRunIds.has(executor.id));
        const hasLiveSubthread = liveSubagentList.some(
          (s) => (executor?.kind === "subthread" && executor.id === s.thread_id) || s.todo_id === todoId,
        );
        const executionState: StudioTask["executionState"] =
          status === "done"
            ? "done"
            : blockerReason || serverExecState === "blocked"
            ? "blocked"
            : waitingFor || serverExecState === "waiting_reply"
            ? "waiting_reply"
            : hasLiveRun || hasLiveSubthread
            ? "running"
            : status === "doing" || serverExecState === "manual_doing" || serverExecState === "running"
            ? "manual_doing"
            : "todo";
        return {
          id: todoId,
          projectId,
          title: text(t.title),
          assignee: text(t.assignee) || text(t.owner),
          due: iso(t.due),
          status,
          links: Array.isArray(t.links) ? (t.links as StudioRef[]) : [],
          threadId: text(t.thread_id) || undefined,
          originKind: (text(t.origin_kind) || "manual") as StudioTask["originKind"],
          notes: text(t.notes) || undefined,
          executor,
          executionState,
          waitingFor,
          blockerReason,
          reportBack,
        } as StudioTask;
      });
      const questionLatest = new Map<string, Api>();
      for (const q of questionRows) {
        const key = text(q.question_id) || text(q.parent_id) || text(q.record_id);
        if (!questionLatest.has(key) || num(q.version) > num(questionLatest.get(key)!.version)) questionLatest.set(key, q);
      }
      const planQuestions: StudioPlanQuestion[] = [...questionLatest.entries()].map(([key, q]) => ({
        id: key, question: text(q.question),
        status: (["open", "investigating", "answered", "blocked"].includes(text(q.status)) ? text(q.status) : "open") as StudioPlanQuestion["status"],
        priority: (["high", "medium", "low"].includes(text(q.priority)) ? text(q.priority) : "medium") as StudioPlanQuestion["priority"],
        rationale: text(q.rationale) || text(q.reason), linkedRefs: [],
      }));
      const planById = new Map<string, Api[]>();
      for (const p of planRows) {
        const key = text(p.parent_id) || text(p.plan_id) || text(p.record_id);
        planById.set(key, [...(planById.get(key) ?? []), p]);
      }
      const planVersions: StudioPlanVersion[] = [];
      for (const [planId, rows] of planById) {
        for (const p of rows.sort((a, b) => num(a.version) - num(b.version))) {
          planVersions.push({
            id: num(p.version) > 1 ? `${planId}:v${num(p.version)}` : planId,
            projectId, version: num(p.version) || num(p.revision) || 1,
            createdAt: iso(p.created_at) || iso(p._created_at), author: text(p.created_by) || "operator",
            goal: text(p.goal), focusQuestion: text(p.question), revisionBasis: text(p.rationale),
            triggerRefs: Array.isArray(p.trigger_refs) ? (p.trigger_refs as StudioRef[]) : [],
            currentJudgement: text(p.current_judgement) || text(p.judgement),
            materialGaps: Array.isArray(p.gaps) ? (p.gaps as string[]) : [],
            proposedActions: Array.isArray(p.actions) ? (p.actions as string[]) : [],
            scopeVersionId: text(p.scope_version_id),
          } as StudioPlanVersion);
        }
      }
      planVersions.sort((a, b) => b.version - a.version);
      const monitors = ((Array.isArray(monitorRows) ? monitorRows : (monitorRows.items ?? monitorRows.monitors ?? [])) as Api[]);
      const studioMonitors: StudioMonitor[] = monitors.map((m) => {
        const monitorId = text(m.monitor_id);
        const def = (m.definition && typeof m.definition === "object" ? (m.definition as Api) : m) as Api;
        const source = (def.source && typeof def.source === "object"
          ? (def.source as Api)
          : m.source && typeof m.source === "object"
          ? (m.source as Api)
          : {}) as Api;
        const sched = (def.schedule && typeof def.schedule === "object" ? (def.schedule as Api) : {}) as Api;
        const budget = (def.budget && typeof def.budget === "object" ? (def.budget as Api) : {}) as Api;
        const intervalSeconds = num(sched.interval_seconds) || num(m.interval_seconds);
        const rev = num(m.revision ?? m._revision, 1);
        if (monitorId) monitorRevisionById.set(monitorId, rev);
        const rawFreq = text(m.frequency_label)
          || text(sched.cron)
          || (intervalSeconds > 0 ? `每 ${intervalSeconds} 秒` : "")
          || (typeof def.schedule === "string" ? def.schedule : text(m.schedule))
          || "手动";
        return {
          id: monitorId,
          projectId,
          name: text(def.name) || text(m.name) || monitorId,
          chainType: ((text(def.chain_type) || text(m.chain_type)) === "rule" ? "rule" : "asset"),
          status: (text(m.status) === "paused" ? "paused" : "active"),
          queryExpression: text(source.query) || text(def.query) || text(m.query) || text(m.query_expression) || text(source.connector_id),
          queryVersion: text(m.query_version) || String(num(m.current_version) || num(m.version) || 1),
          frequencyLabel: rawFreq,
          intervalSeconds,
          budgetPerCycle: num(budget.per_cycle) || num(m.budget_per_cycle),
          budgetTotal: num(budget.total) || num(m.budget_total),
          budgetUsed: num(m.cost_used) || num(m.budget_used),
          baselineCycleId: text(m.last_cycle_id) || text(m.baseline_cycle_id),
          lastSuccessAt: iso(m.last_success_at),
          cycles: ((m.cycles as Api[]) ?? []).map((c) => {
            const comp = (c.comparison && typeof c.comparison === "object" ? (c.comparison as Api) : {}) as Api;
            const cost = (c.cost && typeof c.cost === "object" ? (c.cost as Api) : {}) as Api;
            return {
              id: text(c.cycle_id),
              startedAt: iso(c.started_at) || iso(c.scheduled_at),
              finishedAt: iso(c.finished_at) || iso(c.started_at),
              status: (text(c.status) || "completed") as never,
              summary: text(c.summary) || text(comp.summary) || text(c.end_reason) || text(c.error) || text(c.status),
              added: num(c.added) || num(comp.added),
              changed: num(c.changed) || num(comp.changed),
              missingPages: num(c.missing_pages) || undefined,
              failureReason: text(c.failure_reason) || text(c.error) || undefined,
              costUsed: num(cost.actual) || num(c.cost_used),
              advancedBaseline: c.advanced_baseline === true || Boolean(c.baseline_id),
            };
          }),
        } as StudioMonitor;
      });
      const changeItems = changeRows as Api[];
      const changes: StudioChangeRecord[] = changeItems.map((c) => ({
        id: text(c.change_id) || text(c.record_id), projectId,
        monitorId: text(c.monitor_id), cycleId: text(c.cycle_id),
        chainType: (text(c.chain_type) === "rule" ? "rule" : "asset"),
        changeKind: (text(c.change_kind) || "modified") as StudioChangeRecord["changeKind"],
        title: text(c.title) || text(c.summary) || text(c.change_id), observedAt: iso(c.observed_at) || iso(c.created_at) || iso(c._created_at),
        beforeVersion: text(c.before_version), afterVersion: text(c.after_version), baselineCycleId: text(c.baseline_cycle_id),
        comparable: c.comparable !== false, incomparableReason: text(c.incomparable_reason) || undefined,
        rawDiff: formatStructuredDiff(c.raw_diff), denoisedDiff: formatStructuredDiff(c.denoised_diff),
        ignoredNoiseFields: (c.ignored_noise_fields as string[]) ?? [],
        affectedAssetIds: (c.affected_asset_ids as string[]) ?? [],
        affectedFindingIds: (c.affected_finding_ids as string[]) ?? [],
        affectedReportIds: (c.affected_report_ids as string[]) ?? [],
      } as StudioChangeRecord));
      const p = (policy ?? {}) as Api;
      policyMetaByTask.set(taskId, {
        policyId: text(p.policy_id) || "src-policy",
        version: num(p.version, 0),
      });
      const policyRules = (p.rules as Api[]) ?? [];
      const ruleByRef = new Map<string, Api>();
      for (const rule of policyRules) {
        ruleByRef.set(`${text(rule.rule_id)}:v${num(rule.version)}`, rule);
      }
      const approvals: StudioApproval[] = approvalRows.filter((a) => a.status !== "decided" || text(a.decision) === "pending").map((a) => {
        const snap = (a.request_snapshot && typeof a.request_snapshot === "object" ? (a.request_snapshot as Api) : {});
        const matchedRefs = ((a.matched_rule_refs as unknown[]) ?? []).map(String).filter(Boolean);
        const reusableApproval = matchedRefs.length > 0 && matchedRefs.every((ref) => ruleByRef.get(ref)?.reusable_approval === true);
        const snapArgs = snap.arguments;
        const snapTarget = text(snap.target)
          || text(((snap.resource_facts as Api | undefined)?.observed_url as Api | undefined)?.value)
          || text((snapArgs as Api | undefined)?.url)
          || text(a.target);
        const matchedRule = matchedRefs.length ? ruleByRef.get(matchedRefs[0]) : undefined;
        return {
          id: text(a.approval_id) || text(a.record_id),
          projectId,
          runId: text(snap.run_id) || text(a.run_id) || undefined,
          toolName: text(snap.tool_name) || text(a.tool_name) || text(a.tool),
          action: text(snap.action) || text(a.action),
          resourceKind: (["file", "http", "browser", "network", "mcp"].includes(text(snap.resource_kind) || text(a.resource_kind)) ? (text(snap.resource_kind) || text(a.resource_kind)) : "http") as StudioApproval["resourceKind"],
          target: snapTarget,
          identity: text(snap.identity_ref) || text(a.identity_ref) || text(a.identity),
          cwd: text(snap.cwd) || text(a.cwd),
          inputDigest: text(a.input_digest),
          argumentsPreview: text(a.arguments_preview) || (snapArgs !== undefined ? JSON.stringify(snapArgs, null, 2) : ""),
          matchedRuleId: matchedRefs[0] || text(a.matched_rule_id),
          matchedRuleReason: text(matchedRule?.reason) || text(a.matched_rule_reason) || text(a.reason),
          reusableApproval,
          status: (text(a.status) === "pending" ? "pending" : text(a.status)) as StudioApproval["status"],
          requestedAt: iso(a.requested_at) || iso(a.created_at),
          expiresAt: iso(a.expires_at),
          decidedBy: text(a.decided_by) || undefined,
          decidedAt: iso(a.decided_at) || undefined,
          _revision: num(a._revision),
        } as StudioApproval;
      });
      const grants: StudioGrant[] = grantRows.map((g) => ({
        id: text(g.grant_id) || text(g.record_id), projectId,
        ruleId: text(g.rule_id) || ((g.rule_refs as string[] | undefined) ?? []).join(", "),
        resourceSelector: formatSelector(g.resource_selector) || text(g.selector),
        actions: (g.actions as string[]) ?? [],
        identityConstraint: formatIdentityConstraint(g.identity_constraints ?? g.identity_constraint) || text(g.identity_ref),
        sourceApprovalId: text(g.source_approval_id), createdBy: text(g.created_by), createdAt: iso(g.created_at),
        expiresAt: iso(g.expires_at), status: (text(g.status) || "active") as StudioGrant["status"], _revision: num(g._revision),
      } as StudioGrant));
      const inspectReq = (p.inspection_requirements && typeof p.inspection_requirements === "object" ? (p.inspection_requirements as Api) : {});
      const rawNet = text(p.network_mode) || text(p.network_scope_mode);
      const rawInspect = text(inspectReq.mode) || text(p.http_inspection_mode);
      const catalog = ((caps as Api).capabilities ?? (caps as Api).tools) as Api[] | Api | undefined;
      const capList: Api[] = Array.isArray(catalog) ? catalog
        : Object.entries(catalog ?? {}).map(([id, value]) => ({ id, ...((value ?? {}) as Api) }));
      const env: StudioEnvironmentStatus = {
        sandboxBoundary: text(p.sandbox_boundary) || "server",
        networkScopeMode: rawNet === "enforced" ? "enforced" : "declared",
        httpInspectionMode: (["off", "record", "enforce"].includes(rawInspect) ? rawInspect : "off") as StudioEnvironmentStatus["httpInspectionMode"],
        channels: capList.map((c) => ({
          id: text(c.id) || text(c.name), name: text(c.name) || text(c.id),
          status: (["enforced", "declared_only", "record_only", "unavailable"].includes(text(c.status)) ? text(c.status)
            : c.pre_dispatch_gate === true ? "enforced" : (caps as Api).available === false ? "unavailable" : "declared_only") as "enforced" | "declared_only" | "record_only" | "unavailable",
          trustedFields: text(c.trusted_fields) || ((c.trusted_input_fields as string[]) ?? []).join("、"),
          boundary: text(c.boundary) || text(c.environment_boundary), limitation: text(c.limitation) || text(c.cancellation),
        })),
      };
      const sortedAuthRows = [...authRows].sort((a, b) => {
        const verDiff = (num(b.policy_version) || num(b.version) || num(b._revision)) - (num(a.policy_version) || num(a.version) || num(a._revision));
        if (verDiff !== 0) return verDiff;
        return iso(b.created_at || b._created_at).localeCompare(iso(a.created_at || a._created_at));
      });
      const scopeSnapshots: StudioScopeSnapshot[] = sortedAuthRows.map((a) => {
        const id = text(a.record_id) || text(a._record_id);
        const policyVersion = num(a.policy_version) || num(a.version) || num(a._revision) || 1;
        const addedTargets = Array.isArray(a.added_targets) ? a.added_targets.map(String) : [];
        const removedTargets = Array.isArray(a.removed_targets) ? a.removed_targets.map(String) : [];
        if (id) {
          scopeMetaById.set(id, {
            policyVersion,
            addedTargets,
            removedTargets,
            notes: text(a.raw_policy_text),
          });
        }
        return {
          id,
          projectId,
          version: num(a.version) || policyVersion,
          createdAt: iso(a.created_at) || iso(a._created_at),
          author: text(a.created_by) || text(a.actor) || "operator",
          allowedTargets: (a.allowed_targets as string[]) ?? (a.targets as string[]) ?? [],
          excludedTargets: (a.excluded_targets as string[]) ?? [],
          conditions: (a.conditions as string[]) ?? [],
          identities: ((a.identities as Api[]) ?? []).map((i) => ({
            id: text(i.id) || text(i.identity_ref), name: text(i.name) || text(i.identity_ref),
            role: text(i.role), tenant: text(i.tenant),
            status: (["verified", "expired", "unverified"].includes(text(i.status)) ? text(i.status) : "unverified") as "verified" | "expired" | "unverified",
            lastCheckedAt: iso(i.last_checked_at),
          })),
          rawPolicyText: text(a.raw_policy_text) || JSON.stringify(a, null, 2),
          diffSummary: text(a.diff_summary),
        };
      });
      const scopeMaterials: StudioScopeMaterial[] = sortedAuthRows.map((a) => ({
        id: `${text(a.record_id) || text(a._record_id)}-mat`, projectId, name: text(a.title) || "授权材料",
        kind: "scope_rules" as const,
        version: String(num(a.version) || 1), digest: text(a.digest),
        readable: true, collectedAt: iso(a.created_at) || iso(a._created_at),
        summary: text(a.summary) || "", excerpt: text(a.excerpt) || JSON.stringify(a, null, 2),
      }));
      const history = (histPage.items ?? []).filter((row) => text(row.domain) === "tasks").map((row): HistoryEntry => ({
        id: text(row.entry_id), at: iso(row.at),
        action: (text(row.action) || "status") as HistoryEntry["action"],
        summary: text(row.summary), refs: (row.refs as StudioRef[]) ?? [],
        serverId: text(row.entry_id), serverDomain: "tasks",
        serverUndo: (row.undo as HistoryEntry["serverUndo"]) ?? undefined,
        revertedBy: row.reverted_at ? "server" : undefined,
      }));
      return {
        tasks, planQuestions, planVersions, workRuns, taskRuns: {} as Record<string, TaskRun>,
        monitors: studioMonitors, changes, approvals, grants,
        scopeMaterials, scopeSnapshots, environmentStatus: env, history,
      };
    },
    mutators: {
      async addTask(ctx, ...args) {
        const [input] = args as [{ title: string; assignee?: string; notes?: string; threadId?: string; originKind?: string; links?: StudioRef[] }];
        const saved = await postCommand<Api>(ctx.taskId, "src.workbench.todo.save", {
          payload: {
            title: input.title,
            assignee: input.assignee ?? ctx.actor,
            notes: input.notes,
            thread_id: input.threadId,
            origin_kind: input.originKind ?? "manual",
            links: input.links ?? [],
            status: "open",
          },
        });
        const serverId = text(saved.todo_id);
        const local = ctx.proposal?.().tasks.find((t) => t.title === input.title && t.id !== serverId);
        return { idMap: local ? { [local.id]: serverId } : undefined };
      },
      async setTaskStatus(ctx, ...args) {
        const [taskId, status] = args as [string, string];
        const serverStatus = TODO_STATUS_TO_SERVER[status] ?? "open";
        await postCommand(ctx.taskId, "src.workbench.todo.save", { todo_id: taskId, payload: { status: serverStatus } });
        await recordHistory(ctx, { op: "todo_status", params: { todo_id: taskId } }, { action: "status", summary: `更新任务状态 ${taskId} → ${status}`, refs: [{ kind: "task", id: taskId }] });
      },
      async batchDeleteRecords(ctx, ...args) {
        const [kind, ids] = args as ["source" | "clue" | "task", string[]];
        if (kind !== "task") return;
        for (const id of ids) await postCommand(ctx.taskId, "src.workbench.todo.save", { todo_id: id, payload: { status: "deleted" } });
        await recordHistory(ctx, { op: "todo_status", params: { todo_ids: ids, status: "open" } }, { action: "delete", summary: `删除 ${ids.length} 条任务（可撤销）`, refs: ids.map((i) => ({ kind: "task", id: i })) });
      },
      async createPlanVersion(ctx, ...args) {
        const [input] = args as [{ goal?: string; focusQuestion?: string; revisionBasis?: string; currentJudgement?: string; materialGaps?: string[]; proposedActions?: string[]; planId?: string }];
        await postCommand(ctx.taskId, "src.research.plan.save", {
          payload: {
            question: input.focusQuestion || input.goal || "", rationale: input.revisionBasis || "",
            gaps: input.materialGaps ?? [], actions: input.proposedActions ?? [],
            goal: input.goal ?? "", current_judgement: input.currentJudgement ?? "",
          },
          plan_id: input.planId ?? "", actor_id: ctx.actor,
        });
        await recordHistory(ctx, { op: "noop" }, { action: "status", summary: "保存计划版本" });
      },
      async addPlanQuestion(ctx, ...args) {
        const [input] = args as [{ question: string; priority?: string; rationale?: string }];
        await postCommand(ctx.taskId, "src.research.permission_question.add", {
          question: input.question, actor: { kind: "operator", id: ctx.actor },
          priority: input.priority ?? "medium", rationale: input.rationale ?? "",
        });
      },
      async updatePlanQuestionStatus(ctx, ...args) {
        const [questionId, status] = args as [string, string];
        await postCommand(ctx.taskId, "src.workbench.question.status", { question_id: questionId, status });
      },
      async createWorkRun(ctx, ...args) {
        const [input] = args as [{ title?: string; engine?: string; targetUrl?: string; question?: string }];
        const ensured = await postCommand<Api>(ctx.taskId, "src.run.ensure", {
          target_url: input.targetUrl ?? "", question: input.question || input.title || "核验目标安全性",
        });
        const binding = (ensured.binding && typeof ensured.binding === "object" ? (ensured.binding as Api) : {});
        const runObj = (ensured.run && typeof ensured.run === "object" ? (ensured.run as Api) : {});
        const runId = text(binding.run_id) || text(runObj.run_id) || text(ensured.run_id);
        if (runId) await postCommand(ctx.taskId, "src.run.start", { run_id: runId });
        const local = ctx.proposal?.().workRuns[0];
        return { idMap: local && runId ? { [local.id]: runId } : undefined };
      },
      async toggleMonitorStatus(ctx, ...args) {
        const [monitorId] = args as [string];
        const current = ctx.state().monitors.find((m) => m.id === monitorId);
        const latest = await querySrc<Api>(ctx.taskId, "src.monitor.get", { monitor_id: monitorId });
        const expectedRevision = num(latest.revision ?? monitorRevisionById.get(monitorId), 1);
        const currentStatus = text(latest.status) || current?.status || "active";
        await postCommand(ctx.taskId, "src.monitor.state", {
          monitor_id: monitorId,
          state: currentStatus === "active" ? "paused" : "active",
          expected_revision: expectedRevision,
        });
      },
      async runMonitorCycle(ctx, ...args) {
        const [monitorId] = args as [string, string?];
        await postCommand(ctx.taskId, "src.monitor.run_once", { monitor_id: monitorId });
      },
      async createMonitor(ctx, ...args) {
        const [input] = args as [{
          name?: string;
          chainType?: string;
          queryExpression?: string;
          schedule?: string;
          connectorId?: string;
          intervalSeconds?: number;
          budgetTotal?: number;
          budgetPerCycle?: number;
        }];
        const policy = await querySrc<Api>(ctx.taskId, "src.policy.current", {});
        const authorizationRef = `${text(policy.policy_id) || "src-policy"}:v${num(policy.version, 0)}`;
        let connectorId = text(input.connectorId);
        if (!connectorId) {
          const statusRes = await querySrc<{ connectors?: Api[] }>(ctx.taskId, "src.connector.status", {});
          const preferred = (statusRes.connectors ?? []).find((c) => c.readiness === "ready" || c.readiness === "configured_unverified")
            ?? (statusRes.connectors ?? [])[0];
          connectorId = text(preferred?.connector_id) || "github";
        }
        const cfg = await querySrc<Api>(ctx.taskId, "src.connector.config", { connector_id: connectorId });
        const configVersion = num(cfg.version, 1);
        const manifestVersion = text((cfg.manifest as Api | undefined)?.version) || "v1";
        const intervalSeconds = Math.max(60, num(input.intervalSeconds, 3600));
        const budgetTotal = Math.max(1, num(input.budgetTotal, 100));
        const budgetPerCycle = Math.max(1, num(input.budgetPerCycle, Math.min(budgetTotal, 10)));
        const payload = {
          continuous: true,
          name: input.name ?? "",
          chain_type: input.chainType === "rule" ? "rule" : "asset",
          authorization_ref: authorizationRef,
          source: {
            connector_id: connectorId,
            config_version: configVersion,
            capability_manifest_ref: manifestVersion,
            ...(input.queryExpression?.trim() ? { query: input.queryExpression.trim() } : {}),
          },
          schedule: { interval_seconds: intervalSeconds },
          budget: { total: budgetTotal, per_cycle: budgetPerCycle, call_limit: budgetPerCycle },
        };
        await postCommand(ctx.taskId, "src.monitor.create", { payload });
      },
      async decideApproval(ctx, ...args) {
        const [approvalId, rawDecision, reason] = args as [string, "approve_once" | "approve_grant" | "approve_task_scope" | "deny", string?];
        const decision = rawDecision === "approve_grant" ? "approve_task_scope" : rawDecision;
        const approval = ctx.state().approvals.find((a) => a.id === approvalId);
        await postCommand(ctx.taskId, "src.approval.decide", {
          approval_id: approvalId,
          decision,
          reason: reason ?? "",
          base_revision: approval?._revision ?? 1,
        });
      },
      async revokeGrant(ctx, ...args) {
        const [grantId, reason] = args as [string, string?];
        const grant = ctx.state().grants.find((g) => g.id === grantId);
        await postCommand(ctx.taskId, "src.grant.revoke", { grant_id: grantId, reason: reason ?? "", base_revision: grant?._revision ?? 1 });
      },
      async updateEnvironmentMode(ctx, ...args) {
        const [patch] = args as [{ networkScopeMode?: "declared" | "enforced"; httpInspectionMode?: "off" | "record" | "enforce" }];
        const current = await querySrc<Api>(ctx.taskId, "src.policy.current", {});
        const baseVersion = num(current.version, 1);
        const body: Api = { base_version: baseVersion };
        if (patch.networkScopeMode) body.network_mode = patch.networkScopeMode;
        if (patch.httpInspectionMode) {
          const existingReq = (current.inspection_requirements && typeof current.inspection_requirements === "object"
            ? (current.inspection_requirements as Api)
            : {});
          body.inspection_requirements = { ...existingReq, mode: patch.httpInspectionMode };
        }
        await postCommand(ctx.taskId, "src.policy.update", body);
      },
      async revertHistory(ctx, ...args) {
        const [entryId] = args as [string];
        const entry = ctx.state().history.find((e) => e.id === entryId || e.serverId === entryId);
        if (!entry || (entry.serverDomain && entry.serverDomain !== "tasks")) return;
        const undo = entry.serverUndo;
        if (!undo) return;
        const params = (undo.params ?? {}) as Record<string, unknown>;
        if (undo.op === "todo_status") {
          const ids = (params.todo_ids as string[]) ?? [params.todo_id as string];
          for (const id of ids.filter(Boolean)) {
            const prev = (params.previous as Record<string, string> | undefined)?.[id] ?? (params.status as string) ?? "open";
            await postCommand(ctx.taskId, "src.workbench.todo.save", { todo_id: id, payload: { status: prev } });
          }
        }
        await postCommand(ctx.taskId, "src.workbench.history.revert", { entry_id: entry.serverId ?? entryId });
      },
    },
  };
}
