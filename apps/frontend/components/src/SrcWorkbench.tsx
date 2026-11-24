"use client";

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { ConversationShell, type ConversationExperience, type ConversationNavigation } from "@/components/conversation/ConversationShell";
import { Badge, Button, Callout, Dialog, Input, Label, Menu, MenuItem, MenuLabel, MenuSeparator, Select, TextArea, TextField, toast } from "@muteki/ui";
import { CodeBlock } from "@muteki/ui/agent";
import { querySrc } from "@/components/src/reviewPrimitives";
import { fetchConversationProjects, fetchConversationView, sendConversationCommand, useConversation, type ConversationAgentNode, type ConversationProject } from "@/lib/useConversation";
import { apiFetch } from "@/lib/useRun";
import { downloadSrcDebugLogs, fetchSrcTask, lifecycleSrcTask, listSrcRecordPage, takeoverHistoricalSrcTask } from "@/lib/srcApi";
import { formatSrcDebugLogsSummary } from "@/lib/srcDebugLogs";
import { srcTaskThreadId, type SrcTask } from "@/lib/srcTypes";
import type { ComposerCapabilityRef } from "@/lib/composerCapabilities";
import { STUDIO_BASE, studioChatBase, visibleWorkspace } from "@/components/studio-prototype/nav";
import { studioComposer } from "@/components/studio-prototype/parts";
import {
  useStudioWorkbenchExperience,
  useStudioWorkspaceUrlSync,
} from "@/components/studio-prototype/StudioPrototype";
import { PendingSuggestionTray, StudioContextTray } from "@/components/studio-prototype/ConversationParts";
import { effectiveContext, readStudioState, refLabel, studio, useStudioSelector } from "@/components/studio-prototype/store";
import { StudioSaveIndicator, TaskPersistenceGate } from "@/components/studio-prototype/SaveStatus";
import { createAssetDomain } from "@/components/studio-prototype/real/assets";
import { createFindingDomain } from "@/components/studio-prototype/real/findings";
import { createTasksDomain } from "@/components/studio-prototype/real/tasks";
import { createTrafficDomain } from "@/components/studio-prototype/real/traffic";
import { createReportsDomain } from "@/components/studio-prototype/real/reports";
import { createSuggestionsDomain } from "@/components/studio-prototype/real/suggestions";
import { SrcError, SrcLoading, useSrcResource } from "./ui";
import { useSrcConversationExperience } from "./useSrcConversationExperience";

export function SrcWorkbench({ taskId, navigation }: { taskId: string; navigation: ConversationNavigation }) {
  const task = useSrcResource(() => fetchSrcTask(taskId), [taskId]);
  if (task.loading && !task.data) return <main id="src-main-content" tabIndex={-1} className="src-task-state"><SrcLoading label="正在读取 SRC 任务与工作台…" /></main>;
  if (!task.data) return <main id="src-main-content" tabIndex={-1} className="src-task-state"><SrcError error={task.error || "SRC 任务不可读"} onRetry={() => void task.reload()} /></main>;
  if (task.data.read_only && task.data.historical) return <HistoricalResearch key={taskId} task={task.data} navigation={navigation} onTaskChanged={() => void task.reload()} />;
  const threadId = srcTaskThreadId(task.data);
  if (!threadId) return <main id="src-main-content" tabIndex={-1} className="src-task-state"><SrcError error="这项研究尚未关联主对话，请重新加载任务。" onRetry={() => void task.reload()} /></main>;
  return (
    <TaskPersistenceGate
      taskId={taskId}
      threadId={threadId}
      projectId={task.data.project_id || `task-${taskId}`}
      title={task.data.title}
      goal={task.data.input?.goal ?? ""}
    >
      <SrcWorkbenchBody
        task={task.data}
        navigation={navigation}
        onTaskChanged={() => void task.reload()}
      />
    </TaskPersistenceGate>
  );
}

function SrcDomainStatusBadges() {
  const pendingCount = useStudioSelector((s) => Object.keys(s.pendingDomainOperations || {}).length);
  const effectErrorMessage = useStudioSelector((s) => s.effectError?.message ?? "");
  return (
    <>
      {pendingCount > 0 ? (
        <span role="status" className="inline-flex shrink-0 items-center">
          <Badge tone="accent" dot>正在确认 {pendingCount} 项工作台操作…</Badge>
        </span>
      ) : null}
      {effectErrorMessage ? (
        <span role="alert" title={effectErrorMessage} className="inline-flex max-w-[260px] shrink-0 items-center">
          <Badge tone="danger" icon="circleAlert" className="max-w-full">
            <span className="truncate">{effectErrorMessage}</span>
          </Badge>
        </span>
      ) : null}
    </>
  );
}

function parseLines(raw: string): string[] {
  return raw
    .split(/[\n,]+/)
    .map((s) => s.trim())
    .filter(Boolean);
}

function useSrcDebugLogExport(taskId: string) {
  const [busy, setBusy] = useState(false);
  const controllerRef = useRef<AbortController | null>(null);
  const activeTaskRef = useRef(taskId);
  activeTaskRef.current = taskId;

  useEffect(() => {
    setBusy(false);
    return () => {
      controllerRef.current?.abort();
      controllerRef.current = null;
    };
  }, [taskId]);

  const run = useCallback(async () => {
    if (controllerRef.current) return;
    const controller = new AbortController();
    controllerRef.current = controller;
    const requestedTaskId = taskId;
    setBusy(true);
    try {
      const result = await downloadSrcDebugLogs(requestedTaskId, { signal: controller.signal });
      if (!controller.signal.aborted && activeTaskRef.current === requestedTaskId) {
        toast({
          title: "已打包 SRC 诊断日志",
          description: formatSrcDebugLogsSummary(result),
          tone: "success",
          icon: "download",
        });
      }
    } catch (err) {
      if (err instanceof DOMException && err.name === "AbortError") return;
      if (controller.signal.aborted) return;
      if (activeTaskRef.current === requestedTaskId) {
        toast({
          title: "打包 SRC 日志失败",
          description: err instanceof Error ? err.message : String(err),
          tone: "danger",
        });
      }
    } finally {
      if (controllerRef.current === controller) {
        controllerRef.current = null;
      }
      if (activeTaskRef.current === requestedTaskId) {
        setBusy(false);
      }
    }
  }, [taskId]);

  return { busy, run };
}

function HistoricalResearch({
  task,
  navigation,
  onTaskChanged,
}: {
  task: SrcTask;
  navigation: ConversationNavigation;
  onTaskChanged: () => void;
}) {
  const [kind, setKind] = useState("frozen_report");
  const [pages, setPages] = useState([""]);
  const cursor = pages[pages.length - 1];
  const records = useSrcResource(() => listSrcRecordPage(task.task_id, kind, undefined, 50, cursor), [task.task_id, kind, cursor]);
  const summary = useSrcResource(async () => {
    const [assets, findings, reports, plans, todos] = await Promise.all([
      listSrcRecordPage(task.task_id, "asset", undefined, 1),
      listSrcRecordPage(task.task_id, "finding_claim", undefined, 1),
      listSrcRecordPage(task.task_id, "frozen_report", undefined, 1),
      listSrcRecordPage(task.task_id, "research_plan", undefined, 1),
      listSrcRecordPage(task.task_id, "workbench_todo", undefined, 1),
    ]);
    return {
      assets: assets.total,
      findings: findings.total,
      reports: reports.total,
      plans: plans.total,
      todos: todos.total,
    };
  }, [task.task_id]);

  const [takeoverOpen, setTakeoverOpen] = useState(false);
  const [projects, setProjects] = useState<ConversationProject[]>([]);
  const [projectsLoading, setProjectsLoading] = useState(false);
  const [projectsError, setProjectsError] = useState("");
  const [projectId, setProjectId] = useState(task.project_id || "");
  const [titleOverride, setTitleOverride] = useState("");
  const [goalOverride, setGoalOverride] = useState("");
  const [targetsText, setTargetsText] = useState("");
  const [excludedText, setExcludedText] = useState("");
  const [notesText, setNotesText] = useState("");
  const [overrideReason, setOverrideReason] = useState("");
  const [takeoverError, setTakeoverError] = useState("");
  const [takeoverBusy, setTakeoverBusy] = useState(false);
  const [lifecycleBusy, setLifecycleBusy] = useState(false);
  const debugLogExport = useSrcDebugLogExport(task.task_id);

  const openTakeoverDialog = useCallback(() => {
    setTakeoverError("");
    setOverrideReason("");
    setTargetsText("");
    setExcludedText("");
    setNotesText("");
    setTitleOverride("");
    setGoalOverride("");
    setProjectId(task.project_id || "");
    setTakeoverOpen(true);
    setProjectsLoading(true);
    setProjectsError("");
    void fetchConversationProjects({ scope: "src" })
      .then((list) => {
        setProjects(list);
        if (task.project_id && list.some((p) => p.project_id === task.project_id)) {
          setProjectId(task.project_id);
        }
      })
      .catch((err) => {
        setProjectsError(err instanceof Error ? err.message : String(err));
      })
      .finally(() => {
        setProjectsLoading(false);
      });
  }, [task.project_id]);

  const parsedTargets = parseLines(targetsText);
  const oldTargets = task.input?.authorization?.targets ?? [];
  const oldExcluded = task.input?.authorization?.excluded_targets ?? [];

  const submitTakeover = async () => {
    if (!parsedTargets.length) {
      setTakeoverError("请明确填写新一期授权目标（至少 1 个域名或 URL，禁止默许沿用旧授权）。");
      return;
    }
    setTakeoverBusy(true);
    setTakeoverError("");
    try {
      const excluded = parseLines(excludedText);
      const res = await takeoverHistoricalSrcTask(task.task_id, {
        ...(titleOverride.trim() ? { title: titleOverride.trim() } : {}),
        ...(goalOverride.trim() ? { goal: goalOverride.trim() } : {}),
        ...(projectId ? { project_id: projectId } : {}),
        targets: parsedTargets,
        ...(excluded.length ? { excluded_targets: excluded } : {}),
        ...(notesText.trim() ? { notes: notesText.trim() } : {}),
        ...(overrideReason.trim() ? { scope_fuse_override_reason: overrideReason.trim() } : {}),
      });
      toast({ title: "已创建新一期研究任务", description: `任务 ${res.task_id} 已绑定新的授权边界`, tone: "success" });
      setTakeoverOpen(false);
      navigation.router.push(`/src/tasks/${encodeURIComponent(res.task_id)}`);
    } catch (err) {
      setTakeoverError(err instanceof Error ? err.message : String(err));
    } finally {
      setTakeoverBusy(false);
    }
  };

  const restoreTask = async () => {
    setLifecycleBusy(true);
    try {
      await lifecycleSrcTask(task.task_id, "restore");
      toast({ title: "已恢复归档任务", tone: "success" });
      onTaskChanged();
    } catch (err) {
      toast({ title: "恢复归档失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
    } finally {
      setLifecycleBusy(false);
    }
  };

  return (
    <div className="cx-root cx-scroll h-full overflow-y-auto">
      <main className="mx-auto max-w-4xl space-y-5 p-6 text-cx-fg" id="src-main-content">
        <div className="flex flex-wrap items-center justify-between gap-3">
          <a href="/src" className="text-cx-sm text-cx-accent hover:underline">返回研究项目</a>
          <div className="flex flex-wrap items-center gap-2">
            <Button
              size="sm"
              variant="secondary"
              icon="download"
              loading={debugLogExport.busy}
              disabled={debugLogExport.busy}
              tooltip="打包当前 SRC 对话的全部脱敏诊断日志（.tar.gz）"
              data-testid="src-export-debug-logs"
              className="shrink-0"
              onClick={() => { void debugLogExport.run(); }}
            >
              {debugLogExport.busy ? "打包中…" : "打包日志"}
            </Button>
            {task.lifecycle?.state === "archived" ? (
              <Button size="sm" variant="secondary" disabled={lifecycleBusy} onClick={() => { void restoreTask(); }}>
                恢复为活跃任务
              </Button>
            ) : null}
            <Button size="sm" variant="primary" onClick={openTakeoverDialog}>
              在新一期任务中接管延续
            </Button>
          </div>
        </div>

        <section className="rounded-xl ring-1 ring-cx-ring bg-cx-bg-subtle p-4 shadow-cx-xs">
        <div className="flex flex-wrap items-center gap-2">
          <h1 className="text-cx-lg font-semibold text-cx-fg">{task.title}</h1>
          <Badge tone="warning">历史记录 · 只读</Badge>
          {task.lifecycle?.state ? <Badge tone="neutral">{task.lifecycle.state}</Badge> : null}
        </div>
        {task.input?.goal ? <p className="mt-2 text-cx-sm text-cx-fg-2">{task.input.goal}</p> : null}

        <div className="mt-4 grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
          <div className="rounded-lg border border-cx-border-subtle bg-cx-elevated p-3">
            <div className="text-cx-xs text-cx-fg-3">历史授权范围</div>
            <div className="mt-1 text-cx-sm font-medium text-cx-fg">
              {oldTargets.length ? `${oldTargets.length} 个允许目标` : "未记录目标"}
            </div>
            {oldTargets.length > 0 ? (
              <div className="mt-1 break-all font-cx-mono text-cx-2xs text-cx-fg-3">
                {oldTargets.join(", ")}
              </div>
            ) : null}
            {oldExcluded.length > 0 ? (
              <div className="mt-1 text-cx-2xs text-cx-warning">排除：{oldExcluded.join(", ")}</div>
            ) : null}
          </div>

          <div className="rounded-lg border border-cx-border-subtle bg-cx-elevated p-3">
            <div className="text-cx-xs text-cx-fg-3">资产与计划</div>
            {summary.error ? (
              <div className="mt-1 text-cx-xs text-cx-danger">{summary.error}</div>
            ) : summary.data ? (
              <div className="mt-1 space-y-0.5 text-cx-sm text-cx-fg">
                <div>资产：{summary.data.assets}</div>
                <div className="text-cx-xs text-cx-fg-3">计划：{summary.data.plans} · 待办：{summary.data.todos}</div>
              </div>
            ) : (
              <div className="mt-1 text-cx-xs text-cx-fg-3">读取中…</div>
            )}
          </div>

          <div className="rounded-lg border border-cx-border-subtle bg-cx-elevated p-3">
            <div className="text-cx-xs text-cx-fg-3">发现线索</div>
            {summary.error ? (
              <div className="mt-1 text-cx-xs text-cx-danger">读取失败</div>
            ) : summary.data ? (
              <div className="mt-1 text-cx-md font-semibold text-cx-fg">{summary.data.findings}</div>
            ) : (
              <div className="mt-1 text-cx-xs text-cx-fg-3">读取中…</div>
            )}
          </div>

          <div className="rounded-lg border border-cx-border-subtle bg-cx-elevated p-3">
            <div className="text-cx-xs text-cx-fg-3">已冻结报告</div>
            {summary.error ? (
              <div className="mt-1 text-cx-xs text-cx-danger">读取失败</div>
            ) : summary.data ? (
              <div className="mt-1 space-y-0.5">
                <div className="text-cx-md font-semibold text-cx-fg">{summary.data.reports}</div>
                <div className="text-cx-xs text-cx-fg-3">历史执行轮次：{task.run_count ?? 0}</div>
              </div>
            ) : (
              <div className="mt-1 text-cx-xs text-cx-fg-3">读取中…</div>
            )}
          </div>
        </div>
        {summary.error ? (
          <div className="mt-3">
            <Button size="xs" variant="secondary" onClick={() => void summary.reload()}>重试统计汇总</Button>
          </div>
        ) : null}
      </section>

      <section className="rounded-xl border border-cx-border-subtle bg-cx-bg-subtle p-4">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <Select<string>
            size="sm"
            ariaLabel="历史资料类型"
            value={kind}
            onChange={(next) => { setKind(next); setPages([""]); }}
            className="w-[140px]"
            options={[
              { value: "frozen_report", label: "报告" },
              { value: "asset", label: "资产" },
              { value: "finding_claim", label: "发现" },
              { value: "research_plan", label: "计划" },
              { value: "workbench_todo", label: "待办" },
            ]}
          />
          {records.data ? <span className="text-cx-xs text-cx-fg-3">共 {records.data.total} 条</span> : null}
        </div>
        {records.error ? <div className="mt-3"><SrcError error={records.error} onRetry={() => void records.reload()} /></div> : null}
        {records.loading ? <div className="mt-3"><SrcLoading label="正在读取历史资料…" /></div> : null}
        {records.data ? (
          <div className="mt-3 space-y-2">
            {records.data.records.map((record) => (
              <details key={record.record_id} className="rounded-lg border border-cx-border-subtle bg-cx-elevated px-3 py-2">
                <summary className="cursor-pointer text-cx-sm font-medium text-cx-fg">
                  {String(record.payload?.title || record.payload?.display_name || record.record_id)}
                </summary>
                <div className="mt-2">
                  <CodeBlock code={JSON.stringify(record.payload, null, 2)} language="json" />
                </div>
              </details>
            ))}
            <div className="flex items-center gap-2 pt-2">
              {pages.length > 1 ? <Button size="sm" variant="secondary" onClick={() => setPages((p) => p.slice(0, -1))}>上一页</Button> : null}
              {records.data.next_cursor ? <Button size="sm" variant="secondary" onClick={() => setPages([...pages, records.data!.next_cursor!])}>下一页</Button> : null}
            </div>
          </div>
        ) : null}
      </section>

      <Dialog
        open={takeoverOpen}
        onOpenChange={setTakeoverOpen}
        title="在新一期任务中接管延续"
        description={`基于历史任务「${task.title}」创建新的可执行研究。`}
        size="lg"
        dismissable={false}
        footer={
          <>
            <Button variant="ghost" disabled={takeoverBusy} onClick={() => setTakeoverOpen(false)}>取消</Button>
            <Button variant="primary" loading={takeoverBusy} disabled={takeoverBusy || parsedTargets.length === 0} onClick={() => { void submitTakeover(); }}>
              {takeoverBusy ? "创建中…" : "创建新一期任务"}
            </Button>
          </>
        }
      >
        <div className="space-y-3">
          <Callout tone="warning" title="需要重新确认新一期授权">
            历史任务的授权不会自动继承或默许带入。请逐行输入本期明确获准测试的目标 URL / 域名后再继续。
            {oldTargets.length > 0 ? (
              <div className="mt-1 font-cx-mono text-cx-2xs text-cx-fg-2">旧任务范围参考：{oldTargets.join(", ")}</div>
            ) : null}
          </Callout>

          <TextArea
            label="新一期授权目标"
            description="必填，每行一个或用逗号分隔"
            rows={3}
            value={targetsText}
            onChange={(e) => setTargetsText(e.target.value)}
            placeholder={"https://example.com/\nhttps://api.example.com/"}
            data-autofocus
          />

          <div className="grid gap-3 sm:grid-cols-2">
            <TextArea
              label="排除目标（可选）"
              rows={2}
              value={excludedText}
              onChange={(e) => setExcludedText(e.target.value)}
              placeholder="https://admin.example.com/"
            />
            <TextArea
              label="授权说明 / 备注（可选）"
              rows={2}
              value={notesText}
              onChange={(e) => setNotesText(e.target.value)}
              placeholder="仅限只读验证、测试账号限制等"
            />
          </div>

          <div className="grid gap-3 sm:grid-cols-2">
            <TextField
              label="任务标题覆盖（可选）"
              value={titleOverride}
              onChange={(e) => setTitleOverride(e.target.value)}
              placeholder={`延续：${task.title}`}
            />
            <div>
              <Label>所属研究项目（可选）</Label>
              <Select<string>
                ariaLabel="选择所属研究项目"
                value={projectId}
                onChange={setProjectId}
                disabled={projectsLoading}
                className="w-full"
                searchable={projects.length > 6}
                searchPlaceholder="搜索研究项目…"
                options={[
                  { value: "", label: "独立研究（不归属项目）" },
                  ...projects.map((p) => ({
                    value: p.project_id,
                    label: p.name || p.project_id,
                  })),
                ]}
              />
              {projectsError ? <p className="mt-1 text-cx-xs text-cx-danger">{projectsError}</p> : null}
            </div>
          </div>

          <TextArea
            label="新一期研究目标（可选）"
            rows={2}
            value={goalOverride}
            onChange={(e) => setGoalOverride(e.target.value)}
            placeholder={task.input?.goal || "输入本期研究重点或复测范围"}
          />

          {(takeoverError || overrideReason) ? (
            <TextField
              label="范围熔断复核理由"
              labelHint="触发范围限制时填写"
              value={overrideReason}
              onChange={(e) => setOverrideReason(e.target.value)}
              placeholder="例如：已获平台书面确认扩大或变更范围"
            />
          ) : null}

          {takeoverError ? <Callout tone="danger">{takeoverError}</Callout> : null}
        </div>
      </Dialog>
      </main>
    </div>
  );
}

const TERMINAL_SUBAGENT_STATES = new Set([
  "completed",
  "failed",
  "cancelled",
  "canceled",
  "killed",
  "error",
  "stopped",
  "settled",
  "archived",
  "done",
]);

function mapLiveSubagents(nodes: ConversationAgentNode[] | undefined) {
  if (!Array.isArray(nodes) || !nodes.length) return [];
  const tasks = readStudioState().tasks;
  const rows: Array<{
    thread_id: string;
    title?: string;
    status?: string;
    execution?: string;
    delivery?: string;
    report?: string;
    todo_id?: string;
    delegation_profile?: string;
    adapter_id?: string;
    model?: string;
    effort?: string;
    routing?: ConversationAgentNode["routing"];
    started_at?: string;
    completed_at?: string;
    updated_at?: string;
    error?: string;
  }> = [];
  for (const node of nodes) {
    const id = node.thread_id || node.agent_id;
    if (!id) continue;
    const status = (node.status || node.execution || "").toLowerCase();
    const delivery = (node.delivery || "").toLowerCase();
    if (delivery === "settled" || delivery === "not_delivered") continue;
    const isLiveOrAwaiting =
      delivery === "running"
      || delivery === "awaiting_settle"
      || (!delivery && (node.execution === "running" || node.execution === "working" || node.execution === "background" || !TERMINAL_SUBAGENT_STATES.has(status)));
    if (!isLiveOrAwaiting) continue;
    const explicitTodoId = typeof node.todo_id === "string" ? node.todo_id.trim() : "";
    const delegationProfile = typeof node.delegation_profile === "string" ? node.delegation_profile.trim() : "";
    const matchedTodo = tasks.find((t) => t.executor?.kind === "subthread" && t.executor.id === id);
    const resolvedTodoId = explicitTodoId || matchedTodo?.id || "";
    rows.push({
      thread_id: id,
      title: node.title || node.nickname || node.role || id,
      status: node.status || node.execution || "running",
      ...(node.execution ? { execution: node.execution } : {}),
      ...(node.delivery ? { delivery: node.delivery } : {}),
      ...(node.report ? { report: node.report } : {}),
      ...(resolvedTodoId ? { todo_id: resolvedTodoId } : {}),
      ...(delegationProfile ? { delegation_profile: delegationProfile } : {}),
      ...(node.adapter_id ? { adapter_id: node.adapter_id } : {}),
      ...(node.model ? { model: node.model } : {}),
      ...(node.effort ? { effort: node.effort } : {}),
      ...(node.routing ? { routing: node.routing } : {}),
      ...(node.started_at ? { started_at: node.started_at } : {}),
      ...(node.completed_at ? { completed_at: node.completed_at } : {}),
      ...(node.updated_at ? { updated_at: node.updated_at } : {}),
      ...(node.error ? { error: node.error } : {}),
    });
  }
  return rows;
}

function SrcWorkbenchBody({
  task,
  navigation,
  onTaskChanged,
}: {
  task: SrcTask;
  navigation: ConversationNavigation;
  onTaskChanged: () => void;
}) {
  const threadId = srcTaskThreadId(task);
  const srcConversation = useSrcConversationExperience(task);
  const workbench = useStudioWorkbenchExperience(threadId);
  const debugLogExport = useSrcDebugLogExport(task.task_id);
  const [lifecycleBusy, setLifecycleBusy] = useState(false);
  const [destroyOpen, setDestroyOpen] = useState(false);
  const [subagentSyncError, setSubagentSyncError] = useState("");
  const lastSubagentsSig = useRef("");
  const subagentSyncSeq = useRef(0);
  const [sideOpen, setSideOpen] = useState(false);
  const [sideThreads, setSideThreads] = useState<Array<{
    thread_id: string;
    title?: string;
    updated_at?: string;
    side_chat?: { target_label?: string; checkpoint_seq?: number; bound_run_id?: string };
  }>>([]);
  const [sideTargets, setSideTargets] = useState<Array<{
    run_id: string;
    target_id: string;
    label: string;
    status: string;
    checkpoint_seq: number;
  }>>([]);
  const [selectedTargetKey, setSelectedTargetKey] = useState("");
  const [activeSideId, setActiveSideId] = useState("");
  const sideConversation = useConversation(sideOpen ? activeSideId : "");
  const sideView = sideConversation.view?.thread.thread_id === activeSideId ? sideConversation.view : null;
  const sideMessages = sideView?.messages ?? [];
  const reviewRefsCount = useStudioSelector((s) => (sideOpen ? effectiveContext(s, threadId, visibleWorkspace(threadId)).length : 0));
  const [sideInput, setSideInput] = useState("");
  const [sideBusy, setSideBusy] = useState(false);

  const [explorerOpen, setExplorerOpen] = useState(false);
  const [explorerDbs, setExplorerDbs] = useState<Array<{ db_path: string; file_name: string; tables: Array<{ name: string; kind: string; row_count: number }> }>>([]);
  const [selectedDbPath, setSelectedDbPath] = useState("");
  const [selectedTableName, setSelectedTableName] = useState("");
  const [sampleData, setSampleData] = useState<{ columns: string[]; rows: Array<Record<string, unknown>>; row_count: number } | null>(null);
  const [sampleRawMode, setSampleRawMode] = useState(false);
  const [explorerBusy, setExplorerBusy] = useState(false);

  const loadSideChats = useCallback(async () => {
    try {
      const resp = await apiFetch(`/api/src/tasks/${encodeURIComponent(task.task_id)}/side-chats`);
      if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
      const data = await resp.json() as {
        threads?: Array<{
          thread_id: string;
          title?: string;
          updated_at?: string;
          side_chat?: { target_label?: string; checkpoint_seq?: number; bound_run_id?: string };
        }>;
        available_targets?: Array<{
          run_id: string;
          target_id: string;
          label: string;
          status: string;
          checkpoint_seq: number;
        }>;
      };
      const list = data.threads ?? [];
      setSideThreads(list);
      setSideTargets(data.available_targets ?? []);
      setActiveSideId((id) => id || list[0]?.thread_id || "");
    } catch (err) {
      toast({ title: "读取审阅对话失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
    }
  }, [task.task_id]);

  const loadSample = useCallback(async (dbPath: string, tableName: string) => {
    if (!dbPath || !tableName) {
      setSampleData(null);
      return;
    }
    setExplorerBusy(true);
    try {
      const res = await querySrc<{ columns?: string[]; rows?: Array<Record<string, unknown>>; row_count?: number }>(
        task.task_id,
        "src.workspace.sample",
        { db_path: dbPath, table_name: tableName, limit: 30 },
      );
      setSampleData({
        columns: res.columns ?? [],
        rows: res.rows ?? [],
        row_count: res.row_count ?? 0,
      });
    } catch (err) {
      toast({ title: "读取样本失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
    } finally {
      setExplorerBusy(false);
    }
  }, [task.task_id]);

  const loadExplorerTables = useCallback(async () => {
    setExplorerBusy(true);
    try {
      const res = await querySrc<{
        databases?: Array<{ db_path: string; file_name: string; tables: Array<{ name: string; kind: string; row_count: number }> }>;
      }>(task.task_id, "src.workspace.tables", {});
      const dbs = res.databases ?? [];
      setExplorerDbs(dbs);
      const activeDb = dbs.find((d) => d.db_path === selectedDbPath) ?? dbs[0];
      if (activeDb) {
        setSelectedDbPath(activeDb.db_path);
        const activeTbl = activeDb.tables.find((t) => t.name === selectedTableName) ?? activeDb.tables[0];
        if (activeTbl) {
          setSelectedTableName(activeTbl.name);
          await loadSample(activeDb.db_path, activeTbl.name);
        } else {
          setSelectedTableName("");
          setSampleData(null);
        }
      } else {
        setSelectedDbPath("");
        setSelectedTableName("");
        setSampleData(null);
      }
    } catch (err) {
      toast({ title: "加载工作区表失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
    } finally {
      setExplorerBusy(false);
    }
  }, [loadSample, selectedDbPath, selectedTableName, task.task_id]);

  useEffect(() => {
    if (explorerOpen) void loadExplorerTables();
  }, [explorerOpen, loadExplorerTables]);

  useEffect(() => {
    if (sideOpen) void loadSideChats();
  }, [loadSideChats, sideOpen]);

  const createSideChat = async () => {
    setSideBusy(true);
    try {
      const [runId, targetId] = selectedTargetKey ? selectedTargetKey.split("::") : ["", ""];
      const selectedTarget = sideTargets.find((t) => t.run_id === runId && t.target_id === targetId);
      const reviewRefs = selectedTarget ? [] : effectiveContext(readStudioState(), threadId, visibleWorkspace(threadId));
      const resp = await apiFetch(`/api/src/tasks/${encodeURIComponent(task.task_id)}/side-chats`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          command_id: crypto.randomUUID(),
          title: selectedTarget ? `Worker · ${selectedTarget.label}` : `资料审阅 · ${task.title}`,
          references: reviewRefs.map(({ kind, id }) => ({ kind, id })),
          ...(runId ? { run_id: runId } : {}),
          ...(targetId ? { target_id: targetId } : {}),
        }),
      });
      const data = await resp.json().catch(() => ({})) as { receipt?: { aggregate?: { id?: string } }; error?: { message?: string } };
      if (!resp.ok) throw new Error(data.error?.message ?? `HTTP ${resp.status}`);
      const sid = data.receipt?.aggregate?.id ?? "";
      if (sid) setActiveSideId(sid);
      await loadSideChats();
    } catch (err) {
      toast({ title: "创建侧边对话失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
    } finally {
      setSideBusy(false);
    }
  };

  const sendSideChat = async () => {
    if (!activeSideId || !sideInput.trim()) return;
    setSideBusy(true);
    try {
      const cid = crypto.randomUUID();
      await sendConversationCommand(activeSideId, "conversation.turn.send",
        { text: sideInput.trim(), client_message_id: cid },
        { commandId: cid, idempotencyKey: cid });
      setSideInput("");
    } catch (err) {
      toast({ title: "侧边发送失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
    } finally {
      setSideBusy(false);
    }
  };

  useStudioWorkspaceUrlSync(threadId, true, navigation.router, navigation.searchParams.toString());

  useEffect(() => {
    const previous = studioChatBase.current;
    studioChatBase.current = `/src/tasks/${encodeURIComponent(task.task_id)}`;
    return () => { studioChatBase.current = previous || STUDIO_BASE; };
  }, [task.task_id]);

  useEffect(() => {
    studio.setActiveThread(threadId);
  }, [threadId]);

  useEffect(() => {
    (studio as unknown as { attachDomains: (input: { taskId: string; threadId: string; projectId: string; actor: string; domains: unknown[] }) => void }).attachDomains({
      taskId: task.task_id, projectId: task.project_id || `task-${task.task_id}`, threadId, actor: "local-user",
      domains: [createAssetDomain(), createFindingDomain(), createTasksDomain(), createReportsDomain(), createSuggestionsDomain(), createTrafficDomain()],
    });
    return () => (studio as unknown as { detachDomains: () => void }).detachDomains();
  }, [task.task_id, task.project_id, threadId]);

  useEffect(() => {
    if (!threadId) return;
    let active = true;
    const syncSubagents = async () => {
      const seq = ++subagentSyncSeq.current;
      try {
        const view = await fetchConversationView(threadId, false, 1);
        if (!active || seq !== subagentSyncSeq.current) return;
        const rows = mapLiveSubagents(view.state?.subagents);
        const sig = JSON.stringify(rows);
        if (sig !== lastSubagentsSig.current) {
          lastSubagentsSig.current = sig;
          studio.setLiveSubagents(rows);
        }
        setSubagentSyncError("");
      } catch (err) {
        if (!active || seq !== subagentSyncSeq.current) return;
        setSubagentSyncError(err instanceof Error ? err.message : String(err));
      }
    };
    void syncSubagents();
    const timer = window.setInterval(() => {
      if (!document.hidden) void syncSubagents();
    }, 2500);
    const onRefresh = () => { void syncSubagents(); };
    window.addEventListener("focus", onRefresh);
    window.addEventListener("muteki:threads-changed", onRefresh);
    return () => {
      active = false;
      subagentSyncSeq.current += 1;
      window.clearInterval(timer);
      window.removeEventListener("focus", onRefresh);
      window.removeEventListener("muteki:threads-changed", onRefresh);
      lastSubagentsSig.current = "";
      studio.setLiveSubagents([]);
    };
  }, [threadId]);

  useEffect(() => {
    const onTaskEvent = (event: Event) => {
      const detail = (event as CustomEvent<{ taskId?: string; source?: string }>).detail;
      if (detail?.taskId === task.task_id && detail.source !== "workbench") {
        onTaskChanged();
      }
    };
    window.addEventListener("muteki:src-task-changed", onTaskEvent);
    return () => window.removeEventListener("muteki:src-task-changed", onTaskEvent);
  }, [onTaskChanged, task.task_id]);

  const runLifecycleAction = useCallback(async (action: "pause" | "resume" | "archive" | "restore" | "destroy") => {
    setLifecycleBusy(true);
    try {
      await lifecycleSrcTask(task.task_id, action);
      if (action === "resume" || action === "restore") {
        studio.clearEffectError();
      }
      const label = {
        pause: "已暂停研究",
        resume: "已恢复研究",
        archive: "已归档研究",
        restore: "已恢复归档任务",
        destroy: "已销毁研究任务",
      }[action];
      toast({ title: label, tone: "success" });
      if (action === "destroy") {
        setDestroyOpen(false);
        navigation.router.push("/src");
        return;
      }
      onTaskChanged();
      const nextState = action === "pause" ? "paused" : action === "archive" ? "archived" : "active";
      window.dispatchEvent(new CustomEvent("muteki:src-task-changed", {
        detail: { taskId: task.task_id, lifecycleState: nextState, source: "workbench" },
      }));
      void studio.reloadDomain("tasks").catch((reloadErr) => {
        toast({
          title: "状态已更新，但工作台列表刷新失败",
          description: reloadErr instanceof Error ? reloadErr.message : String(reloadErr),
          tone: "warning",
        });
      });
    } catch (err) {
      toast({ title: "更新研究生命周期失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
    } finally {
      setLifecycleBusy(false);
    }
  }, [navigation.router, onTaskChanged, task.task_id]);

  const experience = useMemo<ConversationExperience>(() => ({
    ...srcConversation.experience,
    ...workbench,
    headerActions: <>
      <SrcDomainStatusBadges />
      {subagentSyncError ? (
        <span role="status" className="inline-flex shrink-0 items-center" title={subagentSyncError}>
          <Badge tone="warning" icon="alert">子线程状态同步失败</Badge>
        </span>
      ) : null}
      <Button
        size="sm"
        variant="secondary"
        icon="download"
        loading={debugLogExport.busy}
        disabled={debugLogExport.busy}
        tooltip="打包当前 SRC 对话的全部脱敏诊断日志（.tar.gz）"
        data-testid="src-export-debug-logs"
        className="shrink-0"
        onClick={() => { void debugLogExport.run(); }}
      >
        {debugLogExport.busy ? "打包中…" : "打包日志"}
      </Button>
      <Button
        size="sm"
        variant={sideOpen ? "primary" : "secondary"}
        onClick={() => {
          setSideOpen((v) => !v);
          setExplorerOpen(false);
        }}
      >
        资料审阅
      </Button>
      <Menu
        ariaLabel="更多研究工具与任务操作"
        placement="bottom-end"
        trigger={
          <Button size="sm" variant={explorerOpen ? "primary" : "secondary"} disabled={lifecycleBusy}>
            更多工具
          </Button>
        }
      >
        <MenuLabel>工作台工具</MenuLabel>
        <MenuItem
          icon="database"
          checked={explorerOpen}
          onSelect={() => {
            setExplorerOpen((v) => !v);
            setSideOpen(false);
          }}
        >
          样本库（SQLite 透视）
        </MenuItem>
        <MenuSeparator />
        <MenuLabel>研究生命周期</MenuLabel>
        {task.lifecycle?.state === "paused" ? (
          <MenuItem icon="play" disabled={lifecycleBusy} onSelect={() => { void runLifecycleAction("resume"); }}>
            恢复研究
          </MenuItem>
        ) : (
          <MenuItem icon="pause" disabled={lifecycleBusy} onSelect={() => { void runLifecycleAction("pause"); }}>
            暂停研究
          </MenuItem>
        )}
        {task.lifecycle?.state === "archived" ? (
          <MenuItem icon="refresh" disabled={lifecycleBusy} onSelect={() => { void runLifecycleAction("restore"); }}>
            恢复归档研究
          </MenuItem>
        ) : (
          <MenuItem icon="archive" disabled={lifecycleBusy} onSelect={() => { void runLifecycleAction("archive"); }}>
            归档研究（转为只读）
          </MenuItem>
        )}
        <MenuItem icon="trash" danger disabled={lifecycleBusy} onSelect={() => setDestroyOpen(true)}>
          销毁研究…
        </MenuItem>
      </Menu>
      {workbench.headerActions}
    </>,
    composerAccessory: (
      <div className="flex w-full min-w-0 flex-col gap-1">
        <div className="mb-2 flex w-full min-w-0 items-center justify-between gap-2">
          <div className="min-w-0 flex-1">
            <StudioContextTray threadId={threadId} className="mb-0" />
          </div>
          <StudioSaveIndicator />
        </div>
        <PendingSuggestionTray />
      </div>
    ),
    composerControls: undefined,
    composerHandle: studioComposer,
    composerRefs: (): ComposerCapabilityRef[] => {
      const studioState = readStudioState();
      return effectiveContext(studioState, threadId, visibleWorkspace(threadId)).map((ref) => {
        const label = refLabel(studioState, ref);
        return {
          context_schema: 2 as const,
          kind: "src_object" as never,
          node_id: `ctx-src-${ref.kind}-${ref.id}`,
          id: `src_object:${ref.kind}:${ref.id}`,
          name: label,
          description: `SRC 工作台 ${ref.kind}`,
          source: "src-task",
          scope: "task",
          locator: { kind: ref.kind, id: ref.id } as never,
          snapshot: { label, text: "", captured_at: "" },
        };
      });
    },
    onSent: () => studio.clearSentContext(threadId),
    preview: undefined,
  }), [
    srcConversation.experience,
    task.task_id,
    task.lifecycle,
    threadId,
    workbench,
    subagentSyncError,
    sideOpen,
    explorerOpen,
    lifecycleBusy,
    debugLogExport.busy,
    debugLogExport.run,
    runLifecycleAction,
  ]);

  const activeDb = explorerDbs.find((d) => d.db_path === selectedDbPath) ?? explorerDbs[0];

  return (
    <div className="relative flex h-full min-h-0 w-full overflow-hidden" data-testid="src-workbench">
      <div className="relative flex min-w-0 flex-1">
        <ConversationShell threadId={threadId} navigation={navigation} experience={experience} mainContentId="src-main-content" sidebarId="src-conversation-sidebar" />
      </div>
      {explorerOpen ? (
        <aside className="cx-root absolute right-0 top-0 bottom-0 z-30 flex w-[460px] max-w-[92vw] flex-col border-l border-cx-border bg-cx-overlay text-cx-fg shadow-cx-pop" data-testid="src-workspace-explorer-drawer">
          <div className="flex items-center justify-between border-b border-cx-border-subtle px-3 py-2">
            <span className="text-cx-sm font-semibold text-cx-fg">工作区样本透视</span>
            <div className="flex items-center gap-1.5">
              <Button size="sm" variant={sampleRawMode ? "primary" : "secondary"} onClick={() => setSampleRawMode((v) => !v)}>JSON</Button>
              <Button size="sm" variant="secondary" disabled={explorerBusy} onClick={() => { void loadExplorerTables(); }}>刷新</Button>
              <Button size="sm" variant="ghost" onClick={() => setExplorerOpen(false)}>关闭</Button>
            </div>
          </div>
          <div className="grid grid-cols-2 gap-2 border-b border-cx-border-subtle px-3 py-2">
            <Select<string>
              size="sm"
              ariaLabel="选择 SQLite 文件"
              value={selectedDbPath}
              onChange={(nextPath) => {
                setSelectedDbPath(nextPath);
                const db = explorerDbs.find((d) => d.db_path === nextPath);
                const tbl = db?.tables[0]?.name ?? "";
                setSelectedTableName(tbl);
                if (nextPath && tbl) void loadSample(nextPath, tbl);
              }}
              disabled={!explorerDbs.length}
              className="w-full"
              options={explorerDbs.length
                ? explorerDbs.map((d) => ({ value: d.db_path, label: d.file_name || d.db_path }))
                : [{ value: "", label: "无 SQLite 库", disabled: true }]}
            />
            <Select<string>
              size="sm"
              ariaLabel="选择数据表"
              value={selectedTableName}
              onChange={(tbl) => {
                setSelectedTableName(tbl);
                if (selectedDbPath && tbl) void loadSample(selectedDbPath, tbl);
              }}
              disabled={!activeDb?.tables.length}
              className="w-full"
              options={activeDb?.tables.length
                ? activeDb.tables.map((t) => ({ value: t.name, label: `${t.name} (${t.row_count})` }))
                : [{ value: "", label: "无表", disabled: true }]}
            />
          </div>
          <div className="cx-scroll min-h-0 flex-1 overflow-auto p-3">
            {sampleData ? (
              sampleRawMode ? (
                <CodeBlock code={JSON.stringify(sampleData.rows, null, 2)} language="json" />
              ) : (
                <div className="overflow-x-auto rounded-lg border border-cx-border-subtle">
                  <table className="w-full border-collapse text-left font-cx-mono text-cx-xs">
                    <thead className="sticky top-0 bg-cx-bg-subtle text-cx-fg-2">
                      <tr>
                        {sampleData.columns.map((col) => (
                          <th key={col} className="border-b border-cx-border-subtle px-2 py-1.5 font-medium">{col}</th>
                        ))}
                      </tr>
                    </thead>
                    <tbody>
                      {sampleData.rows.map((row, idx) => (
                        <tr key={idx} className="border-b border-cx-border-subtle last:border-0">
                          {sampleData.columns.map((col) => (
                            <td key={col} className="max-w-[200px] truncate px-2 py-1 text-cx-fg">
                              {typeof row[col] === "object" && row[col] !== null ? JSON.stringify(row[col]) : String(row[col] ?? "—")}
                            </td>
                          ))}
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
              )
            ) : (
              <div className="py-6 text-center text-cx-xs text-cx-fg-3">暂无样本数据</div>
            )}
          </div>
        </aside>
      ) : null}
      {sideOpen ? (
        <aside className="cx-root absolute right-0 top-0 bottom-0 z-30 flex w-[380px] max-w-[90vw] flex-col border-l border-cx-border bg-cx-overlay text-cx-fg shadow-cx-pop" data-testid="src-side-chat-drawer">
          <div className="flex items-center justify-between border-b border-cx-border-subtle px-3 py-2">
            <span className="text-cx-sm font-semibold text-cx-fg">资料审阅</span>
            <div className="flex items-center gap-1.5">
              <Button size="sm" variant="secondary" disabled={sideBusy} onClick={() => { void createSideChat(); }}>新建对话</Button>
              <Button size="sm" variant="ghost" onClick={() => setSideOpen(false)}>关闭</Button>
            </div>
          </div>
          <div className="border-b border-cx-border-subtle px-3 py-2">
            <Select<string>
              size="sm"
              ariaLabel="绑定检查点上下文"
              value={selectedTargetKey}
              onChange={setSelectedTargetKey}
              className="w-full"
              options={[
                { value: "", label: `研究目标与已引用资料（${reviewRefsCount}）` },
                ...sideTargets.map((t) => ({
                  value: `${t.run_id}::${t.target_id}`,
                  label: `Worker ${t.label} · #${t.checkpoint_seq} (${t.status})`,
                })),
              ]}
            />
          </div>
          {sideThreads.length > 0 ? (
            <div className="flex flex-wrap gap-1 border-b border-cx-border-subtle px-3 py-2">
              {sideThreads.map((st) => (
                <Button
                  key={st.thread_id}
                  size="sm"
                  variant={st.thread_id === activeSideId ? "primary" : "ghost"}
                  className="max-w-full"
                  onClick={() => setActiveSideId(st.thread_id)}
                >
                  <span className="min-w-0 truncate">{st.title || st.thread_id.slice(0, 12)}</span>
                </Button>
              ))}
            </div>
          ) : null}
          <div className="cx-scroll flex min-h-0 flex-1 flex-col gap-2 overflow-y-auto p-3">
            {sideConversation.error ? <Callout tone="danger" role="alert">{sideConversation.error}</Callout> : null}
            {!sideConversation.error && !sideMessages.length ? (
              <div className="py-6 text-center text-cx-xs text-cx-fg-3">
                {activeSideId ? "开始提问以审阅当前研究资料" : "选择上下文并点击「新建对话」"}
              </div>
            ) : null}
            {sideMessages.map((msg, i) => (
              <div key={msg.message_id || i} className="rounded-lg border border-cx-border-subtle bg-cx-elevated px-2.5 py-2 text-cx-xs text-cx-fg">
                <div className="mb-1 text-cx-2xs font-medium text-cx-fg-3">{msg.role || "assistant"}</div>
                <div className="whitespace-pre-wrap">{msg.text || ""}</div>
                {msg.role === "assistant" && msg.text ? (
                  <Button
                    size="sm"
                    variant="ghost"
                    className="mt-1.5"
                    onClick={() => {
                      studioComposer.current?.insertText(`资料审阅记录 ${activeSideId} / ${msg.message_id}\n${msg.text}\n`);
                      setSideOpen(false);
                    }}
                  >
                    引用到主对话
                  </Button>
                ) : null}
              </div>
            ))}
          </div>
          <div className="flex items-center gap-2 border-t border-cx-border-subtle p-2.5">
            <Input
              size="sm"
              value={sideInput}
              onChange={(e) => setSideInput(e.target.value)}
              placeholder={activeSideId ? "提问只读分析（禁用工具）" : "选择上下文并点击新建对话"}
              disabled={!activeSideId || sideBusy}
              className="flex-1"
            />
            <Button size="md" variant="primary" disabled={!activeSideId || sideBusy || !sideInput.trim()} onClick={() => { void sendSideChat(); }}>发送</Button>
          </div>
        </aside>
      ) : null}
      <Dialog
        open={destroyOpen}
        onOpenChange={setDestroyOpen}
        title="销毁研究任务"
        tone="danger"
        size="sm"
        dismissable={false}
        description={`将永久销毁「${task.title}」，此操作不可撤销。`}
        footer={
          <>
            <Button variant="ghost" disabled={lifecycleBusy} onClick={() => setDestroyOpen(false)}>取消</Button>
            <Button variant="danger" loading={lifecycleBusy} disabled={lifecycleBusy} onClick={() => { void runLifecycleAction("destroy"); }}>
              {lifecycleBusy ? "销毁中…" : "销毁"}
            </Button>
          </>
        }
      >
        <Callout tone="danger">
          销毁将终止运行并移除工作台访问，当前任务与运行记录不可再访问；如需保留只读历史记录，请改用“归档研究”。
        </Callout>
      </Dialog>
    </div>
  );
}
