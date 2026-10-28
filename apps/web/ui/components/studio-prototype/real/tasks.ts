/**
 * The real tasks domain: binds the 概览/任务 workbench (plan versions,
 * research questions, runs, monitors/changes, approvals/grants/policy, and
 * durable workbench TODOs) to the SRC backend. Server records are
 * authoritative; see `domain.ts` for the seam contract.
 */
import type {
  StudioApproval, StudioChangeRecord, StudioEnvironmentStatus, StudioGrant,
  StudioMonitor, StudioPlanQuestion, StudioPlanVersion, StudioScopeMaterial,
  StudioScopeSnapshot, StudioTask, StudioWorkRun, StudioRef,
} from "../model";
import type { TaskRun } from "../store";
import type { HistoryEntry } from "../records";
import { postCommand, querySrc } from "../../src/reviewPrimitives";
import type { DomainCtx, RealDomain } from "./domain";

type Api = Record<string, unknown>;
const text = (v: unknown) => (typeof v === "string" ? v : "");
const num = (v: unknown, d = 0) => (Number.isFinite(Number(v)) ? Number(v) : d);
const iso = (v: unknown) => text(v) || "";

async function recordList(ctx: DomainCtx, kind: string, limit = 500) {
  const page = await querySrc<{ records?: Array<{ payload: Api; revision: number }> }>(ctx.taskId, "src.record.list", { kind, limit });
  return (page.records ?? []).map((r) => ({ ...r.payload, _revision: r.revision })) as Array<Api & { _revision: number }>;
}

async function recordHistory(ctx: DomainCtx, undo: { op: string; params?: Record<string, unknown> }, fallback: { action: string; summary: string; refs?: StudioRef[] }) {
  const entry = ctx.state().history[0];
  const payload = entry && entry.summary === fallback.summary
    ? { id: entry.id, action: entry.action, summary: entry.summary, refs: entry.refs, at: entry.at }
    : { id: `wb-${Date.now().toString(36)}`, action: fallback.action, summary: fallback.summary, refs: fallback.refs ?? [], at: new Date().toISOString() };
  await postCommand(ctx.taskId, "src.workbench.history.record", { entry: { ...payload, undo, domain: "tasks" } });
}

const RUN_STATES = ["queued", "planning", "running", "paused", "blocked", "completed", "failed", "archived"] as const;

export function createTasksDomain(): RealDomain {
  return {
    id: "tasks",
    pollMs: 30000,
    owns: ["tasks", "planVersions", "planQuestions", "scopeMaterials", "scopeSnapshots", "workRuns", "taskRuns", "changes", "monitors", "approvals", "grants", "environmentStatus"],
    combines: {
      history: (local, server) => {
        const serverIds = new Set((server as HistoryEntry[]).map((e) => e.id));
        return [...(server as HistoryEntry[]), ...(local as HistoryEntry[]).filter((e) => e.serverDomain !== "tasks" && !serverIds.has(e.id))];
      },
    },
    async load(taskId, ctx) {
      const [todoRows, questionRows, planRows, runRows, monitorRows, changeRows, approvalRows, grantRows, authRows, histPage, policy, caps] = await Promise.all([
        querySrc<{ items?: Api[] }>(taskId, "src.workbench.todo.list", {}),
        recordList(ctx, "research_question"),
        recordList(ctx, "research_plan"),
        querySrc<Api[] | { items?: Api[] }>(taskId, "src.run.list", {}),
        querySrc<{ items?: Api[]; monitors?: Api[] } | Api[]>(taskId, "src.monitor.list", {}),
        recordList(ctx, "monitor_change"),
        recordList(ctx, "execution_approval"),
        recordList(ctx, "task_grant"),
        recordList(ctx, "authorization_snapshot"),
        querySrc<{ items?: Api[] }>(taskId, "src.workbench.history", { limit: 1000 }),
        querySrc<Api>(taskId, "src.policy.current", {}).catch(() => ({})),
        querySrc<Api>(taskId, "src.execution.capabilities", {}).catch(() => ({})),
      ]);
      const projectId = `task-${taskId}`;
      const tasks: StudioTask[] = (todoRows.items ?? []).filter((t) => t.status !== "deleted").map((t) => ({
        id: text(t.todo_id), projectId, title: text(t.title), assignee: text(t.assignee) || text(t.owner),
        due: iso(t.due), status: (["open", "in_progress", "done", "blocked", "waiting", "backlog"].includes(text(t.status)) ? text(t.status) : "open") as StudioTask["status"],
        links: (t.links as StudioRef[]) ?? [], threadId: text(t.thread_id) || undefined,
        originKind: (text(t.origin_kind) || "manual") as StudioTask["originKind"], notes: text(t.notes) || undefined,
      } as StudioTask));
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
            createdAt: iso(p.created_at), author: text(p.created_by) || "operator",
            goal: text(p.goal), focusQuestion: text(p.question), revisionBasis: text(p.rationale),
            triggerRefs: (p.trigger_refs as StudioRef[]) ?? [],
            currentJudgement: text(p.current_judgement) || text(p.judgement),
            materialGaps: (p.gaps as string[]) ?? [], proposedActions: (p.actions as string[]) ?? [],
            scopeVersionId: text(p.scope_version_id),
          } as StudioPlanVersion);
        }
      }
      const runs = (Array.isArray(runRows) ? runRows : runRows.items ?? []) as Api[];
      const workRuns: StudioWorkRun[] = runs.map((r) => ({
        id: text(r.run_id) || text(r.record_id), projectId,
        title: text(r.title) || text(r.task_kind) || text(r.run_id), engine: "pentest_swarm",
        targetUrl: text(r.target) || text(r.target_url), question: text(r.question) || text(r.goal),
        state: (RUN_STATES.includes(text(r.state) as never) ? text(r.state) : "queued") as StudioWorkRun["state"],
        generation: num(r.generation) || 1, identityContext: text(r.identity_ref),
        planVersionId: "", authVersionId: "",
        startedAt: iso(r.started_at) || iso(r.created_at), updatedAt: iso(r.updated_at) || iso(r.started_at),
        blockerReason: text(r.blocked_reason) || text(r.failure_reason) || undefined,
        logs: ((r.logs as Api[]) ?? []).map((l, i) => ({ at: iso(l.at), step: text(l.step) || text(l.title) || `step-${i}`, status: "ok" as const, detail: text(l.detail) })),
        linkedRefs: (r.linked_refs as StudioRef[]) ?? [],
      } as StudioWorkRun));
      const monitors = ((Array.isArray(monitorRows) ? monitorRows : (monitorRows.items ?? monitorRows.monitors ?? [])) as Api[]);
      const studioMonitors: StudioMonitor[] = monitors.map((m) => ({
        id: text(m.monitor_id), projectId, name: text(m.name) || text(m.monitor_id),
        chainType: (text(m.chain_type) === "rule" ? "rule" : "asset"),
        status: (text(m.status) === "paused" ? "paused" : "active"),
        queryExpression: text(m.query) || text(m.query_expression),
        queryVersion: text(m.query_version) || String(num(m.current_version) || 1),
        frequencyLabel: text(m.frequency_label) || text(m.schedule) || "手动",
        intervalSeconds: num(m.interval_seconds), budgetPerCycle: num(m.budget_per_cycle),
        budgetTotal: num(m.budget_total), budgetUsed: num(m.budget_used) || num(m.cost_used),
        baselineCycleId: text(m.last_cycle_id) || text(m.baseline_cycle_id),
        lastSuccessAt: iso(m.last_success_at),
        cycles: ((m.cycles as Api[]) ?? []).map((c) => ({
          id: text(c.cycle_id), startedAt: iso(c.started_at), finishedAt: iso(c.finished_at),
          status: (text(c.status) || "completed") as never, summary: text(c.summary),
          added: num(c.added), changed: num(c.changed), missingPages: num(c.missing_pages) || undefined,
          failureReason: text(c.failure_reason) || undefined, costUsed: num(c.cost_used), advancedBaseline: c.advanced_baseline === true,
        })),
      } as StudioMonitor));
      const changeItems = changeRows as Api[];
      const changes: StudioChangeRecord[] = changeItems.map((c) => ({
        id: text(c.change_id) || text(c.record_id), projectId,
        monitorId: text(c.monitor_id), cycleId: text(c.cycle_id),
        chainType: (text(c.chain_type) === "rule" ? "rule" : "asset"),
        changeKind: (text(c.change_kind) || "modified") as StudioChangeRecord["changeKind"],
        title: text(c.title) || text(c.summary) || text(c.change_id), observedAt: iso(c.observed_at) || iso(c.created_at),
        beforeVersion: text(c.before_version), afterVersion: text(c.after_version), baselineCycleId: text(c.baseline_cycle_id),
        comparable: c.comparable !== false, incomparableReason: text(c.incomparable_reason) || undefined,
        rawDiff: text(c.raw_diff), denoisedDiff: text(c.denoised_diff),
        ignoredNoiseFields: (c.ignored_noise_fields as string[]) ?? [],
        affectedAssetIds: (c.affected_asset_ids as string[]) ?? [],
        affectedFindingIds: (c.affected_finding_ids as string[]) ?? [],
        affectedReportIds: (c.affected_report_ids as string[]) ?? [],
      } as StudioChangeRecord));
      const approvals: StudioApproval[] = approvalRows.filter((a) => a.status !== "decided" || text(a.decision) === "pending").map((a) => ({
        id: text(a.approval_id) || text(a.record_id), projectId,
        runId: text(a.run_id) || undefined, toolName: text(a.tool_name) || text(a.tool),
        action: text(a.action), resourceKind: (["file", "http", "browser", "network", "mcp"].includes(text(a.resource_kind)) ? text(a.resource_kind) : "http") as StudioApproval["resourceKind"],
        target: text(a.target), identity: text(a.identity_ref) || text(a.identity), cwd: text(a.cwd),
        inputDigest: text(a.input_digest), argumentsPreview: text(a.arguments_preview) || JSON.stringify(a.arguments ?? ""),
        matchedRuleId: text(a.matched_rule_id), matchedRuleReason: text(a.matched_rule_reason) || text(a.reason),
        status: (text(a.status) === "pending" ? "pending" : text(a.status)) as StudioApproval["status"],
        requestedAt: iso(a.requested_at) || iso(a.created_at), expiresAt: iso(a.expires_at),
        decidedBy: text(a.decided_by) || undefined, decidedAt: iso(a.decided_at) || undefined,
        _revision: num(a._revision),
      } as StudioApproval));
      const grants: StudioGrant[] = grantRows.map((g) => ({
        id: text(g.grant_id) || text(g.record_id), projectId,
        ruleId: text(g.rule_id), resourceSelector: text(g.resource_selector) || text(g.selector),
        actions: (g.actions as string[]) ?? [], identityConstraint: text(g.identity_constraint) || text(g.identity_ref),
        sourceApprovalId: text(g.source_approval_id), createdBy: text(g.created_by), createdAt: iso(g.created_at),
        expiresAt: iso(g.expires_at), status: (text(g.status) || "active") as StudioGrant["status"], _revision: num(g._revision),
      } as StudioGrant));
      const p = (policy ?? {}) as Api;
      const capList = (((caps as Api).capabilities ?? (caps as Api).tools) as Api[]) ?? [];
      const env: StudioEnvironmentStatus = {
        sandboxBoundary: text(p.sandbox_boundary) || "server",
        networkScopeMode: (text(p.network_scope_mode) === "enforced" ? "enforced" : "declared"),
        httpInspectionMode: (["off", "record", "enforce"].includes(text(p.http_inspection_mode)) ? text(p.http_inspection_mode) : "off") as StudioEnvironmentStatus["httpInspectionMode"],
        channels: capList.map((c) => ({
          id: text(c.id) || text(c.name), name: text(c.name) || text(c.id),
          status: (["enforced", "declared_only", "record_only", "unavailable"].includes(text(c.status)) ? text(c.status) : "declared_only") as "enforced" | "declared_only" | "record_only" | "unavailable",
          trustedFields: text(c.trusted_fields), boundary: text(c.boundary), limitation: text(c.limitation),
        })),
      };
      const scopeSnapshots: StudioScopeSnapshot[] = authRows.map((a) => ({
        id: text(a.record_id), projectId,
        version: num(a.version) || num(a._revision) || 1,
        createdAt: iso(a.created_at), author: text(a.created_by) || text(a.actor) || "operator",
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
      }));
      const scopeMaterials: StudioScopeMaterial[] = authRows.map((a) => ({
        id: `${a.record_id}-mat`, projectId, name: text(a.title) || "授权材料",
        kind: "scope_rules" as const,
        version: String(num(a.version) || 1), digest: text(a.digest) || text(a.sha256),
        readable: true, collectedAt: iso(a.created_at),
        summary: text(a.summary) || "", excerpt: text(a.excerpt) || JSON.stringify(a).slice(0, 4000),
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
        const [input] = args as [{ title: string; assignee?: string; notes?: string; threadId?: string; originKind?: string }];
        const saved = await postCommand<Api>(ctx.taskId, "src.workbench.todo.save", {
          payload: { title: input.title, assignee: input.assignee ?? ctx.actor, notes: input.notes, thread_id: input.threadId, origin_kind: input.originKind ?? "manual", status: "open" },
        });
        const serverId = text(saved.todo_id);
        const local = ctx.state().tasks.find((t) => t.title === input.title && t.id !== serverId);
        return { idMap: local ? { [local.id]: serverId } : undefined };
      },
      async setTaskStatus(ctx, ...args) {
        const [taskId, status] = args as [string, string];
        await postCommand(ctx.taskId, "src.workbench.todo.save", { todo_id: taskId, payload: { status } });
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
          run_kind: "pentest.target", title: input.title ?? "", target: input.targetUrl ?? "", question: input.question ?? "",
        });
        const runId = text(ensured.run_id);
        if (runId) await postCommand(ctx.taskId, "src.run.start", { run_id: runId });
        const local = ctx.state().workRuns[0];
        return { idMap: local && runId ? { [local.id]: runId } : undefined };
      },
      async toggleMonitorStatus(ctx, ...args) {
        const [monitorId] = args as [string];
        const current = ctx.state().monitors.find((m) => m.id === monitorId);
        await postCommand(ctx.taskId, "src.monitor.state", { monitor_id: monitorId, state: current?.status === "active" ? "paused" : "active" });
      },
      async runMonitorCycle(ctx, ...args) {
        const [monitorId] = args as [string, string?];
        await postCommand(ctx.taskId, "src.monitor.run_once", { monitor_id: monitorId });
      },
      async createMonitor(ctx, ...args) {
        const [input] = args as [{ name?: string; chainType?: string; queryExpression?: string; schedule?: string }];
        await postCommand(ctx.taskId, "src.monitor.create", {
          payload: { continuous: true, name: input.name ?? "", chain_type: input.chainType ?? "asset", query: input.queryExpression ?? "", schedule: input.schedule ?? "" },
        });
      },
      async decideApproval(ctx, ...args) {
        const [approvalId, decision, reason] = args as [string, "approve_once" | "approve_grant" | "deny", string?];
        const approval = ctx.state().approvals.find((a) => a.id === approvalId) as (StudioApproval & { _revision?: number }) | undefined;
        await postCommand(ctx.taskId, "src.approval.decide", {
          approval_id: approvalId, decision, reason: reason ?? "",
          base_revision: approval?._revision ?? 0,
        });
      },
      async revokeGrant(ctx, ...args) {
        const [grantId, reason] = args as [string, string?];
        const grant = ctx.state().grants.find((g) => g.id === grantId) as (StudioGrant & { _revision?: number }) | undefined;
        await postCommand(ctx.taskId, "src.grant.revoke", { grant_id: grantId, reason: reason ?? "", base_revision: grant?._revision ?? 0 });
      },
      async updateEnvironmentMode(ctx, ...args) {
        const [patch] = args as [Record<string, unknown>];
        await postCommand(ctx.taskId, "src.policy.update", patch);
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
