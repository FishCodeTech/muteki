"use client";

import { useMemo, useState } from "react";
import { cn } from "@/lib/cn";
import { Icon } from "../Icon";
import { Badge, Button, EmptyState, Input, type Tone } from "@/components/chat/ui";
import { TodoList, TodoStatusIcon, type TodoItem, type TodoItemStatus } from "@/components/agentui/agents/todo-list";
import {
  sendConversationCommand,
  type ConversationView,
  type PlanTask,
  type ThreadPlanSnapshot,
} from "@/lib/useConversation";
import type { ConversationToolRecord } from "./conversationEventViews";
import type { DrawerDetailPayload } from "./ConversationDetailsDrawer";
import {
  disableCopy,
  isDefiniteSupport,
  levelOf,
  matrixFromRuntimeConnection,
} from "@/lib/interactionCapabilityMatrix";

const PHASE_LABEL: Record<string, string> = {
  proposed: "计划建议",
  executing: "执行中",
  awaiting_decision: "等待决策",
  completed: "已完成",
  cleared: "已清除",
  unsupported: "不支持计划事件",
};

const TASK_LABEL: Record<string, string> = {
  pending: "待开始",
  in_progress: "进行中",
  completed: "已完成",
  blocked: "阻塞",
  cancelled: "已取消",
};

export function planPhaseLabel(phase?: string | null): string {
  return PHASE_LABEL[String(phase || "")] || "无计划";
}

export function planSupportsProvider(view: ConversationView): boolean {
  const matrix = matrixFromRuntimeConnection(view.runtime_connection);
  if (matrix) {
    const level = levelOf(matrix, "plan");
    if (level === "unsupported" || level === "expired") return false;
    if (isDefiniteSupport(matrix, "plan")) return true;
  }
  return view.runtime_connection?.capabilities?.plan === true;
}

export function planUnsupportedCopy(view: ConversationView): string {
  const matrix = matrixFromRuntimeConnection(view.runtime_connection);
  if (matrix) {
    const copy = disableCopy(matrix, "plan");
    if (levelOf(matrix, "plan") === "unsupported" && copy.reason) {
      return `${copy.reason}。${copy.alternative}`;
    }
  }
  return (
    view.state.plan?.unsupported_reason
    || "Pi 等未上报 plan/task 事件的接入不会从正文待办臆测进度。请改看工具过程或最终回答。"
  );
}

export function resolveThreadPlan(view: ConversationView): ThreadPlanSnapshot | null {
  return view.state.plan ?? null;
}

function toolPayload(record: ConversationToolRecord): DrawerDetailPayload {
  const isTerminal = /^(shell|exec_command|terminal|bash)$/i.test(record.name);
  return {
    type: record.status === "failed" ? "error" : "tool",
    title: record.name,
    subtitle: record.turnId || record.occurredAt,
    status: record.status,
    toolName: record.name,
    input: record.argsSummary,
    output: isTerminal ? undefined : record.outputSummary,
    terminalOutput: isTerminal ? record.outputSummary : undefined,
    error: record.error,
  };
}

function findEvidenceTool(
  task: PlanTask,
  tools: ConversationToolRecord[],
): ConversationToolRecord | null {
  for (const evidence of task.evidence || []) {
    const match = tools.find((tool) => (
      String(tool.id || "") === evidence.id
      || String(tool.id || "") === String(evidence.id || "")
    ));
    if (match) return match;
  }
  if (task.status !== "in_progress" && task.status !== "completed") return null;
  const sameTurn = tools.filter((tool) => tool.turnId && tool.turnId === (task.evidence?.[0]?.turn_id || ""));
  return sameTurn.at(-1) || null;
}

function todoStatus(status: string): TodoItemStatus {
  if (status === "in_progress") return "in-progress";
  if (status === "completed") return "completed";
  if (status === "cancelled") return "cancelled";
  return "pending";
}

function TaskGlyph({ status }: { status: string }) {
  if (status === "blocked") return <Icon name="circleAlert" size={17} className="text-cx-warning" />;
  return <TodoStatusIcon status={todoStatus(status)} />;
}

function todoItems(tasks: PlanTask[]): TodoItem[] {
  return tasks.map((task) => ({
    id: task.task_id,
    title: task.title,
    status: todoStatus(String(task.status)),
    detail: task.status === "blocked" ? <span className="text-[12px] text-cx-warning">阻塞</span> : undefined,
  }));
}

const PHASE_TONE: Record<string, Tone> = {
  proposed: "neutral",
  executing: "running",
  awaiting_decision: "warning",
  completed: "success",
};

function PlanProgress({ done, total }: { done: number; total: number }) {
  const pct = total ? Math.round((done / total) * 100) : 0;
  return (
    <div className="flex items-center gap-2.5">
      <div className="h-1 flex-1 overflow-hidden rounded-full bg-cx-hover">
        <div className="h-full rounded-full bg-cx-accent transition-[width] duration-500 ease-cx-out" style={{ width: `${pct}%` }} />
      </div>
      <span className="cx-tabular shrink-0 text-[11.5px] text-cx-fg-3">{done}/{total}</span>
    </div>
  );
}

export function ConversationProposedPlanCard({
  view,
  onOpenPlan,
}: {
  view: ConversationView;
  onOpenPlan?: () => void;
}) {
  const plan = resolveThreadPlan(view);
  // Keep a timeline entry-point for proposed / executing / awaiting so refresh
  // and reconnect can reopen the Plan panel without hunting the launcher.
  if (!plan || !["proposed", "executing", "awaiting_decision"].includes(String(plan.phase))) {
    return null;
  }
  if (!plan.tasks.length && plan.phase !== "awaiting_decision") {
    return null;
  }
  const phase = String(plan.phase);
  let phaseCopy = "";
  if (phase === "proposed") {
    phaseCopy = "Runtime 上报的计划建议，尚未开始执行。";
  } else if (phase === "executing") {
    phaseCopy = plan.last_change_summary || "";
  } else if (phase === "awaiting_decision") {
    phaseCopy = String(plan.awaiting?.summary || "等待用户决策或 Runtime 确认修改。");
  }
  return (
    <section className="cx-animate-in flex flex-col gap-1.5" data-phase={plan.phase} aria-label="执行计划">
      <TodoList
        items={todoItems(plan.tasks)}
        title={
          <span className="flex min-w-0 items-baseline gap-2">
            <span className="truncate">{plan.title || "执行计划"}</span>
            <span className="shrink-0 text-[12px] font-normal text-cx-fg-4">{planPhaseLabel(plan.phase)} · 第 {plan.revision} 版</span>
          </span>
        }
        defaultOpen
        maxHeight={248}
        className="bg-cx-elevated shadow-cx-sm"
      />
      {phaseCopy || onOpenPlan ? (
        <div className="flex items-center gap-2 px-1">
          {phaseCopy ? <p className="min-w-0 flex-1 text-[12.5px] leading-5 text-cx-fg-3">{phaseCopy}</p> : <span className="flex-1" />}
          {onOpenPlan ? (
            <Button size="xs" variant="ghost" iconRight="panelRightOpen" onClick={onOpenPlan} className="shrink-0 text-cx-fg-3">
              在面板中查看
            </Button>
          ) : null}
        </div>
      ) : null}
    </section>
  );
}

export function ConversationPlanPanel({
  view,
  tools,
  onOpenDetails,
}: {
  view: ConversationView;
  tools: ConversationToolRecord[];
  onOpenDetails: (payload: DrawerDetailPayload) => void;
}) {
  const plan = resolveThreadPlan(view);
  const supported = planSupportsProvider(view);
  const [amendment, setAmendment] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const pendingAmendment = plan?.pending_amendment;
  const tasks = useMemo(() => plan?.tasks || [], [plan?.tasks]);
  const counts = useMemo(() => {
    const total = tasks.length;
    const done = tasks.filter((task) => task.status === "completed").length;
    const blocked = tasks.filter((task) => task.status === "blocked").length;
    return { total, done, blocked };
  }, [tasks]);

  const submitAmend = async () => {
    const text = amendment.trim();
    if (!text || busy) return;
    setBusy(true);
    setError("");
    try {
      await sendConversationCommand(view.thread.thread_id, "conversation.plan.amend", {
        text,
        plan_revision: plan?.revision || 0,
        client_message_id: `plan_amend_${Date.now().toString(36)}`,
      });
      setAmendment("");
    } catch (exc) {
      setError(exc instanceof Error ? exc.message : String(exc));
    } finally {
      setBusy(false);
    }
  };

  if ((!plan || plan.phase === "cleared" || plan.phase === "unsupported") && !supported) {
    return (
      <div className="flex h-full flex-col" data-phase="unsupported">
        <EmptyState
          icon="info"
          title="当前 Runtime 不支持结构化计划"
          description={plan?.unsupported_reason || planUnsupportedCopy(view)}
        />
      </div>
    );
  }

  if (!plan || plan.phase === "cleared" || tasks.length === 0) {
    return (
      <div className="flex h-full flex-col" data-phase="empty">
        <EmptyState
          icon="listTodo"
          title={supported ? "等待 Agent 上报计划" : "还没有计划"}
          description="只展示 Adapter 上报的结构化计划，不会把回复正文里的待办当成真实进度。"
        />
      </div>
    );
  }

  return (
    <div className="flex min-h-0 flex-col gap-4 p-4" data-phase={plan.phase}>
      <header className="flex flex-col gap-2.5">
        <div className="flex items-start justify-between gap-3">
          <div className="min-w-0">
            <strong className="block text-[15px] font-semibold leading-6 text-cx-fg">{plan.title || "计划步骤"}</strong>
            <span className="text-[12px] text-cx-fg-4">第 {plan.revision} 版{counts.blocked ? ` · ${counts.blocked} 个阻塞` : ""}</span>
          </div>
          <Badge tone={PHASE_TONE[String(plan.phase)] ?? "neutral"} dot>{planPhaseLabel(plan.phase)}</Badge>
        </div>
        {counts.total ? <PlanProgress done={counts.done} total={counts.total} /> : null}
      </header>
      {plan.last_change_summary ? (
        <p className="rounded-xl bg-cx-bg-subtle px-3 py-2 text-[12.5px] leading-5 text-cx-fg-2">{plan.last_change_summary}</p>
      ) : null}
      {plan.phase === "awaiting_decision" ? (
        <p className="rounded-xl bg-cx-warning-soft px-3 py-2 text-[12.5px] leading-5 text-cx-fg" role="status">
          {String(plan.awaiting?.summary || "等待用户输入或审批")}
        </p>
      ) : null}
      {pendingAmendment && typeof pendingAmendment === "object" ? (
        <p className="rounded-xl border border-dashed border-cx-border-strong px-3 py-2 text-[12.5px] text-cx-fg-3" role="status">
          修改尚未获 Runtime 确认：{String((pendingAmendment as { text?: string }).text || "")}
        </p>
      ) : null}
      <ol className="m-0 flex list-none flex-col p-0">
        {tasks.map((task, index) => {
          const evidenceTool = findEvidenceTool(task, tools);
          const status = String(task.status);
          return (
            <li key={task.task_id} data-status={task.status} className="group/task relative flex gap-3 pb-3 last:pb-0">
              {index < tasks.length - 1 ? (
                <span className="absolute bottom-0 left-[11.5px] top-7 w-px bg-cx-border" aria-hidden />
              ) : null}
              <span className="relative z-[1] grid size-6 shrink-0 place-items-center rounded-full bg-cx-bg">
                <TaskGlyph status={status} />
              </span>
              <div className="min-w-0 flex-1">
                <div className="flex items-start gap-2">
                  <strong className={cn("min-w-0 flex-1 text-[13px] font-medium leading-5", status === "completed" ? "text-cx-fg-3" : "text-cx-fg")}>
                    {task.title}
                  </strong>
                  <span className="shrink-0 text-[11.5px] leading-5 text-cx-fg-4">{TASK_LABEL[status] || status}</span>
                </div>
                <div className="mt-0.5 flex items-center gap-2">
                  <code className="font-cx-mono text-[11px] text-cx-fg-4">{task.task_id}</code>
                  {evidenceTool ? (
                    <button
                      type="button"
                      onClick={() => onOpenDetails(toolPayload(evidenceTool))}
                      className="inline-flex items-center gap-1 text-[11.5px] font-medium text-cx-accent opacity-80 hover:opacity-100"
                    >
                      <Icon name="terminal" size={11} />
                      查看工具日志
                    </button>
                  ) : null}
                </div>
                {task.blocked_reason ? (
                  <p className="mt-1 rounded-lg bg-cx-warning-soft px-2 py-1 text-[12px] leading-5 text-cx-fg-2">{task.blocked_reason}</p>
                ) : null}
              </div>
            </li>
          );
        })}
      </ol>
      <form
        className="mt-auto flex items-center gap-2 border-t border-cx-border-subtle pt-3"
        onSubmit={(event) => {
          event.preventDefault();
          void submitAmend();
        }}
      >
        <Input
          value={amendment}
          onChange={(event) => setAmendment(event.target.value)}
          aria-label="提出计划修改"
          placeholder="提出对计划步骤的修改…"
          disabled={busy}
          size="sm"
          className="flex-1"
        />
        <Button type="submit" variant="primary" size="sm" loading={busy} disabled={!amendment.trim() || busy}>
          提出修改
        </Button>
      </form>
      {error ? <p className="text-[12px] text-cx-danger">{error}</p> : null}
    </div>
  );
}
