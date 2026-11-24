"use client";
import { type IconName } from "@muteki/ui";


import { useCallback, useEffect, useMemo, useState } from "react";

import {
  Button,
  Callout,
  Dialog,
  EmptyState,
  Checkbox,
  IconButton,
  Input,
  Label,
  Menu,
  MenuItem,
  SegmentedControl,
  Select,
  Switch,
  TextArea,
  TextField,
  toast,
  useContextMenu,
} from "@muteki/ui";
import { lifecycleSrcTask, renewSrcAuthorization } from "@/lib/srcApi";
import { controlErrorText, settleControlThread } from "@/lib/controlGrants";
import { postCommand, querySrc } from "../src/reviewPrimitives";
import { STUDIO_TEAM } from "./constants";
import {
  EVIDENCE_KIND_LABELS,
  type StudioPlanQuestion,
  type StudioPlanVersion,
  type StudioRef,
  type StudioTask,
} from "./model";
import { isSheetViewport, openRef, openWorkspace, revealConversation, revealTurn, studioRouter } from "./nav";
import {
  SheetAskBar,
  StudioBadge,
  askAi,
  displayTitle,
  dragRef,
  useRememberedScroll,
  useViewing,
  REF_KIND_LABEL,
} from "./parts";
import { findClueDuplicates, mergeOf, RUN_STATE_LABEL, RUN_STATE_TONE } from "./records";
import { SectionHeader, SurfaceToolbar } from "@/components/chat/surfaces/shared";
import { SurfaceCard, SurfaceRow } from "@/components/chat/surfaces/OverviewSurface";
import { PathBreadcrumb } from "@/components/chat/surfaces/PathBreadcrumb";
import { Dl, sendAbout } from "./Kit";
import { TodoList, type TodoItem } from "@muteki/ui/agent";
import { StatusDot } from "@muteki/ui";
import { CodeBlock } from "@muteki/ui/agent";
import {
  afterDomainMutation,
  attachedTaskId,
  hasAttachedDomains,
  readStudioState,
  refLabel,
  studio,
  useStudioState,
  type StudioState,
  type StudioSuggestion,
} from "./store";
import {
  getMonitorSchedulerHealth,
  getScopeSnapshotMeta,
  type MonitorSchedulerHealth,
} from "./real/tasks";




function handoffInstruction(task: StudioTask): string {
  return `请推进任务「${task.title}」：先列出关联材料里还缺的证据，再给出下一步。`;
}

export function handTaskToAi(task: StudioTask): boolean {
  const threadId = readStudioState().activeThreadId;
  if (hasAttachedDomains()) {
    studio.prepareHandoff(threadId, task.id);
    const accepted = askAi({ kind: "task", id: task.id }, handoffInstruction(task));
    if (!accepted) {
      studio.clearHandoff(threadId);
      return false;
    }
    toast({
      title: "已带入对话",
      description: "任务和指令已填入真实对话，发送后交给 Agent。",
      tone: "success",
    });
    return true;
  }
  const refs: StudioRef[] = [{ kind: "task", id: task.id }, ...task.links];
  studio.send(threadId, handoffInstruction(task), refs, { taskId: task.id });
  if (isSheetViewport()) {
    toast({
      title: "已交给 AI",
      description: "进度显示在任务卡片上，回复在对话里。",
      tone: "success",
      action: { label: "看对话", onClick: revealConversation },
    });
  }
  return true;
}






function PlanDrillIn({ projectId }: { projectId: string }) {
  const state = useStudioState();
  const ui = state.workspace.tasks;
  const plans = state.planVersions;
  const plan = plans.find((p) => p.id === ui.selectedPlanId) ?? plans[0];
  const questions = state.planQuestions;
  const materials = state.scopeMaterials;
  const [reviseId, setReviseId] = useState("");
  const [publishOpen, setPublishOpen] = useState(false);
  const [questionOpen, setQuestionOpen] = useState(false);
  const [newQuestion, setNewQuestion] = useState("");
  const [newQuestionPriority, setNewQuestionPriority] = useState<StudioPlanQuestion["priority"]>("medium");
  const [focusQ, setFocusQ] = useState("");
  const [judgement, setJudgement] = useState("");
  const [gapsText, setGapsText] = useState("");
  const [actionsText, setActionsText] = useState("");

  if (!plan) return (
    <>
      <EmptyState compact icon="compass" title="还没有研究计划" action={<Button size="sm" variant="primary" onClick={() => setPublishOpen(true)}>创建计划</Button>} />
      <Dialog
        open={publishOpen}
        onOpenChange={setPublishOpen}
        title="创建研究计划"
        footer={(
          <Button
            size="sm"
            variant="primary"
            disabled={!focusQ.trim()}
            onClick={() => {
              studio.createPlanVersion({ projectId: projectId === "all" ? state.projects[0]?.id ?? "" : projectId, goal: focusQ.trim(), focusQuestion: focusQ.trim(), revisionBasis: "人工创建", currentJudgement: judgement, materialGaps: [], proposedActions: [], triggerRefs: [] });
              void afterDomainMutation().then(result => { if (result.ok) setPublishOpen(false); });
            }}
          >
            创建
          </Button>
        )}
      >
        <div className="flex flex-col gap-3">
          <TextField label="研究目标" value={focusQ} onChange={event => setFocusQ(event.target.value)} />
          <TextArea label="当前判断" value={judgement} onChange={event => setJudgement(event.target.value)} />
        </div>
      </Dialog>
    </>
  );
  const planRef: StudioRef = { kind: "plan", id: plan.id };
  const gapItems: TodoItem[] = (plan.materialGaps ?? []).map((g, i) => ({ id: `g-${i}`, title: g, status: "pending" as const }));
  const statusText: Record<StudioPlanQuestion["status"], string> = { open: "待研究", investigating: "调查中", answered: "已回答", blocked: "受阻" };

  const submitRevision = () => {
    const id = studio.createPlanVersion({
      projectId: projectId === "all" ? state.projects[0]?.id ?? "" : projectId,
      goal: plan.goal,
      focusQuestion: focusQ || plan.focusQuestion,
      revisionBasis: "人工修订",
      currentJudgement: judgement || plan.currentJudgement,
      materialGaps: gapsText.split("\n").map((x) => x.trim()).filter(Boolean),
      proposedActions: actionsText.split("\n").map((x) => x.trim()).filter(Boolean),
      triggerRefs: plan.triggerRefs ?? [],
    });
    setPublishOpen(false);
    studio.setTasksUi({ selectedPlanId: id });
    if (hasAttachedDomains()) {
      void afterDomainMutation().then((res) => { if (res.ok) toast({ title: "已发布新版计划", tone: "success" }); });
    } else {
      toast({ title: `已发布计划版本 ${id}`, tone: "success" });
    }
  };

  const submitQuestion = () => {
    if (!newQuestion.trim()) return;
    studio.addPlanQuestion({ question: newQuestion.trim(), priority: newQuestionPriority, rationale: "", linkedRefs: [] });
    setNewQuestion("");
    setQuestionOpen(false);
    if (hasAttachedDomains()) {
      void afterDomainMutation().then((res) => { if (res.ok) toast({ title: "已登记研究问题", tone: "success" }); });
    }
  };

  return (
    <div className="mx-auto flex w-full max-w-[760px] flex-col gap-4 p-4 pb-10" data-testid="studio-plan-drill">
      <SurfaceCard className="p-3">
        <div className="flex items-start gap-3">
          <div className="min-w-0 flex-1"><p className="text-cx-md font-semibold text-cx-fg">{plan.focusQuestion || plan.goal}</p></div>
          <StudioBadge tone="accent">第 {plan.version} 版</StudioBadge>
        </div>
        <Dl className="mt-3" items={[
          { label: "版本", value: <Select<string> value={plan.id} onChange={(selectedPlanId) => studio.setTasksUi({ selectedPlanId })} size="sm" options={plans.map((p) => ({ value: p.id, label: `第 ${p.version} 版 · ${p.createdAt}` }))} ariaLabel="切换计划版本" className="min-w-[160px]" /> },
          { label: "目标", value: plan.goal },
        ]} />
        <div className="mt-3 flex flex-wrap items-center gap-2">
          <Button size="sm" variant="primary" onClick={() => setPublishOpen(true)}>发布新版</Button>
          <Button size="sm" variant="secondary" onClick={() => sendAbout(planRef, `请结合最新监控变化与复核结果，为计划 ${plan.id} 起草第 ${plan.version + 1} 版修订。`)}>让 Agent 修订</Button>
        </div>
      </SurfaceCard>

      <div>
        <SectionHeader title="当前判断" />
        <p className="px-1 text-cx-sm leading-6 text-cx-fg">{plan.currentJudgement}</p>
      </div>

      <div>
        <SectionHeader title="材料缺口" count={gapItems.length} action={gapItems.length ? <button type="button" className="text-cx-xs font-medium text-cx-accent hover:underline" onClick={() => sendAbout(planRef, `计划 ${plan.id} 有 ${gapItems.length} 项材料缺口，请帮我补齐。`)}>让 Agent 补齐</button> : undefined} />
        {gapItems.length ? <TodoList items={gapItems} title="材料缺口" /> : <SurfaceCard><SurfaceRow icon="check" title="没有缺口" /></SurfaceCard>}
      </div>

      <div>
        <SectionHeader title="研究问题" count={questions.length} action={<button type="button" className="text-cx-xs font-medium text-cx-accent hover:underline" onClick={() => setQuestionOpen((v) => !v)}>{questionOpen ? "取消" : "添加问题"}</button>} />
        {questionOpen ? (
          <div className="mb-2 flex flex-wrap items-center gap-2">
            <Select<StudioPlanQuestion["priority"]> value={newQuestionPriority} onChange={setNewQuestionPriority} size="sm" ariaLabel="优先级" className="min-w-[84px]" options={[{ value: "high", label: "高" }, { value: "medium", label: "中" }, { value: "low", label: "低" }]} />
            <Input size="sm" value={newQuestion} onChange={(e) => setNewQuestion(e.target.value)} placeholder="研究问题" aria-label="研究问题" className="min-w-[180px] flex-1" />
            <Button size="sm" variant="primary" disabled={!newQuestion.trim()} onClick={submitQuestion}>保存</Button>
          </div>
        ) : null}
        <SurfaceCard>
          {questions.map((q) => (
            <SurfaceRow
              key={q.id}
              icon="help"
              title={q.question}
              subtitle={q.rationale}
              trailing={(
                <Menu ariaLabel={`问题状态`} trigger={<button type="button" className="shrink-0 text-cx-xs text-cx-fg-3 hover:text-cx-fg">{statusText[q.status]}</button>}>
                  {(["open", "investigating", "answered", "blocked"] as const).map((st) => (
                    <MenuItem key={st} checked={q.status === st} onSelect={() => studio.updatePlanQuestionStatus(q.id, st)}>{statusText[st]}</MenuItem>
                  ))}
                </Menu>
              )}
            />
          ))}
          {!questions.length ? <SurfaceRow icon="help" title="暂无研究问题" /> : null}
        </SurfaceCard>
      </div>

      <div>
        <SectionHeader title="范围材料" count={materials.length} />
        <SurfaceCard>
          {materials.map((m) => <SurfaceRow key={m.id} icon="file" title={m.name} subtitle={`${m.version} · ${m.digest.slice(0, 12)}`} />)}
          {!materials.length ? <SurfaceRow icon="file" title="暂无范围材料" /> : null}
        </SurfaceCard>
      </div>

      <div>
        <SectionHeader title="版本历史" count={plans.length} />
        <SurfaceCard>
          {plans.map((p) => (
            <SurfaceRow key={p.id} icon="history" title={`第 ${p.version} 版${p.id === plan.id ? " · 当前" : ""}`} subtitle={p.createdAt} selected={p.id === plan.id} onClick={p.id === plan.id ? undefined : () => setReviseId(p.id)} />
          ))}
        </SurfaceCard>
      </div>

      <Dialog open={publishOpen} onOpenChange={setPublishOpen} title="人工发布新版计划" size="md" footer={<><Button variant="ghost" onClick={() => setPublishOpen(false)}>取消</Button><Button variant="primary" onClick={submitRevision}>发布</Button></>}>
        <div className="flex flex-col gap-3">
          <TextField label="聚焦问题" value={focusQ || plan.focusQuestion} onChange={(e) => setFocusQ(e.target.value)} />
          <TextArea label="当前判断" value={judgement || plan.currentJudgement} onChange={(e) => setJudgement(e.target.value)} className="min-h-[80px]" />
          <TextArea label="材料缺口（每行一项）" value={gapsText || (plan.materialGaps ?? []).join("\n")} onChange={(e) => setGapsText(e.target.value)} className="min-h-[70px]" />
          <TextArea label="建议动作（每行一项）" value={actionsText || (plan.proposedActions ?? []).join("\n")} onChange={(e) => setActionsText(e.target.value)} className="min-h-[70px]" />
        </div>
      </Dialog>
      {reviseId ? <PlanDiffDialog current={plan} other={plans.find((p) => p.id === reviseId)!} onClose={() => setReviseId("")} /> : null}
    </div>
  );
}

function PlanDiffDialog({ current, other, onClose }: { current: StudioPlanVersion; other: StudioPlanVersion; onClose: () => void }) {
  const rows: Array<[string, string, string]> = [
    ["聚焦问题", current.focusQuestion, other.focusQuestion],
    ["当前判断", current.currentJudgement, other.currentJudgement],
    ["材料缺口", (current.materialGaps ?? []).join("、") || "无", (other.materialGaps ?? []).join("、") || "无"],
    ["建议动作", (current.proposedActions ?? []).join("、") || "无", (other.proposedActions ?? []).join("、") || "无"],
  ];
  return (
    <Dialog open onOpenChange={(o) => { if (!o) onClose(); }} title={`第 ${other.version} 版 → 第 ${current.version} 版`} size="lg">
      <div className="overflow-x-auto rounded-xl border border-cx-border-subtle">
        <table className="w-full border-collapse text-left text-cx-sm">
          <thead><tr className="border-b border-cx-border-subtle bg-cx-bg-subtle"><th className="w-[110px] px-3 py-2 text-cx-fg-3">项</th><th className="px-3 py-2">当前</th><th className="px-3 py-2 text-cx-fg-2">对照</th></tr></thead>
          <tbody className="divide-y divide-cx-border-subtle">
            {rows.map(([k, a, b]) => <tr key={k} className={a !== b ? "bg-cx-accent-soft/30" : ""}><td className="px-3 py-2 text-cx-fg-3">{k}</td><td className="px-3 py-2">{a}</td><td className="px-3 py-2 text-cx-fg-2">{b}</td></tr>)}
          </tbody>
        </table>
      </div>
    </Dialog>
  );
}


const ENGINE_LABEL: Record<string, string> = { pentest_swarm: "渗透集群", local_mock: "本地模拟", agent_mcp: "Agent + MCP" };

function RunDrillIn({ projectId }: { projectId: string }) {
  const state = useStudioState();
  const ui = state.workspace.tasks;
  const runs = state.workRuns;
  const run = runs.find((r) => r.id === ui.selectedRunId);
  const srcTaskId = attachedTaskId();
  const [steerText, setSteerText] = useState("");
  const [interveneBusy, setInterveneBusy] = useState(false);

  if (!run) {
    return (
      <div className="mx-auto flex w-full max-w-[760px] flex-col gap-4 p-4 pb-10" data-testid="studio-runs-list">
        <div>
          <SectionHeader title="工作记录" count={runs.length} />
          <SurfaceCard>
            {runs.map((r) => (
              <SurfaceRow
                key={r.id}
                icon={r.state === "running" ? "terminal" : r.state === "blocked" || r.state === "failed" ? "alert" : "check"}
                tone={r.state === "running" ? "running" : "default"}
                title={displayTitle(r.title)}
                subtitle={RUN_STATE_LABEL[r.state] + (r.blockerReason ? ` · ${r.blockerReason}` : "")}
                onClick={() => studio.setTasksUi({ selectedRunId: r.id })}
              />
            ))}
            {!runs.length ? <SurfaceRow icon="terminal" title="暂无工作记录" /> : null}
          </SurfaceCard>
        </div>
      </div>
    );
  }

  const isDemoHost = (s2: { hostTag?: string }) => (s2.hostTag ?? "demo") === "demo";
  const runRef: StudioRef = { kind: "run", id: run.id };
  const artifacts = run.linkedRefs.filter((ref) => ref.kind === "source" || ref.kind === "evidence" || ref.kind === "clue");
  const stepItems: TodoItem[] = run.logs.map((log, i) => ({
    id: `log-${i}`,
    title: log.step,
    status: log.status === "running" ? "in-progress" as const : log.status === "error" ? "cancelled" as const : "completed" as const,
    detail: `${log.at} · ${log.detail}`,
  }));

  const flowback = () => {
    const turnId = studio.advanceWorkRunAndFlowback(run.id);
    toast({
      title: `已推进 ${run.id} 并回流进展至对话`,
      tone: "success",
      action: turnId ? { label: "去对话查看", onClick: () => revealTurn(state.activeThreadId, turnId) } : undefined,
    });
  };

  return (
    <div className="mx-auto flex w-full max-w-[760px] flex-col gap-4 p-4 pb-10" data-testid="studio-run-drill">
      <SurfaceCard className="p-3" {...dragRef(runRef)}>
        <div className="flex items-start gap-3">
          <div className="min-w-0 flex-1"><p className="text-cx-md font-semibold text-cx-fg">{displayTitle(run.title)}</p></div>
          <StudioBadge tone={RUN_STATE_TONE[run.state]}>{RUN_STATE_LABEL[run.state]}</StudioBadge>
        </div>
        <Dl className="mt-3" items={[
          { label: "编号", value: run.id, mono: true },
          { label: "引擎", value: ENGINE_LABEL[run.engine] ?? run.engine },
          { label: "目标", value: run.targetUrl, mono: true },
          { label: "更新", value: `${run.updatedAt} · 第 ${run.generation} 代` },
        ]} />
        {isDemoHost(state) ? (
          <div className="mt-3 flex items-center gap-2">
            <Button size="sm" variant="primary" onClick={flowback}>回流到对话</Button>
          </div>
        ) : srcTaskId ? (
          <div className="mt-3 flex flex-wrap items-center gap-2 border-t border-cx-border-subtle pt-3">
            {(() => {
              const terminal = ["completed", "failed", "stopped", "archived"].includes(run.state);
              const sendControl = async (action: "pause" | "resume" | "freeze" | "thaw" | "stop", okTitle: string) => {
                setInterveneBusy(true);
                try {
                  await postCommand(srcTaskId, "src.run.intervene", { run_id: run.id, action });
                  await studio.reloadDomain("tasks");
                  toast({ title: okTitle, tone: "success" });
                } catch (err) {
                  toast({ title: "Run 控制失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
                } finally {
                  setInterveneBusy(false);
                }
              };
              return (
                <>
                  {run.state === "paused" ? (
                    <Button size="sm" variant="secondary" disabled={interveneBusy} onClick={() => void sendControl("resume", "已恢复该 Run")}>
                      继续 Run
                    </Button>
                  ) : !terminal && run.state !== "frozen" ? (
                    <Button size="sm" variant="secondary" disabled={interveneBusy} onClick={() => void sendControl("pause", "已暂停该 Run")}>
                      暂停 Run
                    </Button>
                  ) : null}
                  {run.state === "frozen" ? (
                    <Button size="sm" variant="secondary" disabled={interveneBusy} onClick={() => void sendControl("thaw", "已解冻该 Run")}>
                      解冻 Run
                    </Button>
                  ) : !terminal ? (
                    <Button size="sm" variant="secondary" disabled={interveneBusy} onClick={() => void sendControl("freeze", "已冻结该 Run")}>
                      冻结 Run
                    </Button>
                  ) : null}
                  {!terminal ? (
                    <Button size="sm" variant="danger-soft" disabled={interveneBusy} onClick={() => void sendControl("stop", "已停止该 Run")}>
                      停止 Run
                    </Button>
                  ) : null}
                  <Input
                    value={steerText}
                    onChange={(e) => setSteerText(e.target.value)}
                    placeholder="转向提示或聚焦范围"
                    aria-label="Run 转向指令"
                    disabled={terminal}
                    className="h-8 min-w-[180px] flex-1"
                  />
                  <Button
                    size="sm"
                    variant="primary"
                    disabled={interveneBusy || terminal || !steerText.trim()}
                    onClick={async () => {
                      setInterveneBusy(true);
                      try {
                        await postCommand(srcTaskId, "src.run.intervene", {
                          run_id: run.id,
                          action: "steer",
                          note: steerText.trim(),
                        });
                        setSteerText("");
                        await studio.reloadDomain("tasks");
                        toast({ title: "已下发转向干预", tone: "success" });
                      } catch (err) {
                        toast({ title: "转向失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
                      } finally {
                        setInterveneBusy(false);
                      }
                    }}
                  >
                    下发转向
                  </Button>
                </>
              );
            })()}
          </div>
        ) : null}
      </SurfaceCard>

      {run.state === "blocked" ? <Callout tone="warning">受阻：{run.blockerReason ?? "等待处理"}</Callout> : null}

      <TodoList title={`步骤 ${stepItems.filter((i) => i.status === "completed").length}/${stepItems.length}`} items={stepItems} />

      {artifacts.length ? (
        <div>
          <SectionHeader title="产物" count={artifacts.length} />
          <SurfaceCard>
            {artifacts.map((ref) => <SurfaceRow key={`${ref.kind}:${ref.id}`} icon={ref.kind === "source" ? "book" : ref.kind === "clue" ? "bookmark" : "file"} title={refLabel(state, ref)} subtitle={REF_KIND_LABEL[ref.kind]} onClick={() => openRef(ref)} />)}
          </SurfaceCard>
        </div>
      ) : null}
      <span className="hidden">{projectId}</span>
    </div>
  );
}

function ChangesDrillIn({ active, embedded = false }: { active: boolean; embedded?: boolean }) {
  const state = useStudioState();
  const [diffMode, setDiffMode] = useState<"denoised" | "raw">("denoised");
  const openId = state.workspace.tasks.selectedChangeId ?? "";
  const setOpenId = (selectedChangeId: string) => studio.setTasksUi({ selectedChangeId });
  const open = state.changes.find((c) => c.id === openId);
  void active;

  if (open) {
    const affected = [...[...(open.affectedFindingIds ?? []), ...(open.affectedReportIds ?? []).map((id) => `report:${id}`)].length ? [...(open.affectedFindingIds ?? [])] : []].map((id): StudioRef => ({ kind: "clue", id }));
    return (
      <div className="flex min-h-0 flex-1 flex-col" data-testid="studio-change-detail">
        <SurfaceToolbar>
          <IconButton size="sm" icon="arrowLeft" label="返回" onClick={() => setOpenId("")} />
          <PathBreadcrumb path={`变化/${open.id}`} rootLabel="概览" lastIsFile onSelect={() => setOpenId("")} />
        </SurfaceToolbar>
        <div className="cx-scroll min-h-0 flex-1 overflow-y-auto">
          <div className={embedded ? "flex flex-col gap-4 pt-4" : "mx-auto flex w-full max-w-[760px] flex-col gap-4 p-4 pb-10"}>
            <SurfaceCard className="p-3">
              <div className="flex items-start gap-3">
                <div className="min-w-0 flex-1"><p className="text-cx-md font-semibold text-cx-fg">{displayTitle(open.title)}</p></div>
                <StudioBadge tone={open.comparable ? "success" : "warning"}>{open.comparable ? "可比" : "不可比"}</StudioBadge>
              </div>
              <Dl className="mt-3" items={[
                { label: "编号", value: open.id, mono: true },
                { label: "版本", value: `${open.beforeVersion} → ${open.afterVersion}` },
                { label: "观察", value: open.observedAt },
              ]} />
            </SurfaceCard>
            {!open.comparable && open.incomparableReason ? <p className="px-1 text-cx-xs text-cx-fg-4">{open.incomparableReason}</p> : null}
            <div>
              <SectionHeader title="差异" action={<SegmentedControl<"denoised" | "raw"> value={diffMode} onChange={setDiffMode} size="sm" ariaLabel="差异模式" options={[{ value: "denoised", label: "降噪" }, { value: "raw", label: "原始" }]} />} />
              <CodeBlock
                code={(diffMode === "denoised" ? (open.denoisedDiff || open.rawDiff) : open.rawDiff) || "暂无结构化差异详情"}
                language="json"
                className="max-h-[320px]"
              />
            </div>
            {affected.length ? (
              <div>
                <SectionHeader title="受影响" count={affected.length} />
                <SurfaceCard>
                  {affected.map((ref) => <SurfaceRow key={ref.id} icon="bookmark" title={refLabel(state, ref)} subtitle="发现" onClick={() => openRef(ref)} />)}
                </SurfaceCard>
              </div>
            ) : null}
          </div>
        </div>
      </div>
    );
  }

  return (
    <div className={embedded ? "flex flex-col gap-4" : "mx-auto flex w-full max-w-[760px] flex-col gap-4 p-4 pb-10"} data-testid="studio-changes-list">
      <div>
        <SectionHeader title="变化记录" count={state.changes.length} />
        <SurfaceCard>
          {state.changes.map((chg) => (
            <SurfaceRow key={chg.id} icon="activity" title={displayTitle(chg.title)} subtitle={`${chg.observedAt} · ${chg.comparable ? "可比" : "不可比"}`} trailing={chg.comparable ? undefined : <span className="shrink-0 text-cx-xs text-cx-warning">需复核</span>} onClick={() => setOpenId(chg.id)} />
          ))}
          {!state.changes.length ? <SurfaceRow icon="checkCheck" title="暂无变化" /> : null}
        </SurfaceCard>
      </div>
    </div>
  );
}

const ONLINE_MONITOR_CONNECTORS = ["github", "gitlab", "fofa", "quake", "shodan", "zoomeye", "aws_ec2", "scopesentry"] as const;

type MonitorPreflightResult = {
  ok?: boolean;
  blocking_reasons?: Array<{ code?: string; message?: string }>;
  warnings?: Array<{ code?: string; message?: string }>;
  connector_readiness?: string;
  policy_version?: number;
};

function MonitorsDrillIn({ active }: { active: boolean }) {
  const state = useStudioState();
  const srcTaskId = attachedTaskId(state);
  const monitors = state.monitors ?? [];
  const [liveHealth, setLiveHealth] = useState<MonitorSchedulerHealth | null>(null);
  const [healthError, setHealthError] = useState("");
  const [healthBusy, setHealthBusy] = useState(false);

  const [createOpen, setCreateOpen] = useState(false);
  const [monName, setMonName] = useState("");
  const [monQuery, setMonQuery] = useState("");
  const [monChain, setMonChain] = useState<"asset" | "rule">("asset");
  const [monConnector, setMonConnector] = useState<string>("github");
  const [monIntervalSec, setMonIntervalSec] = useState("3600");
  const [monBudgetTotal, setMonBudgetTotal] = useState("100");
  const [monBudgetPerCycle, setMonBudgetPerCycle] = useState("10");
  const [connectors, setConnectors] = useState<Array<{ connector_id: string; readiness?: string }>>([]);
  const [preflightBusy, setPreflightBusy] = useState(false);
  const [preflightResult, setPreflightResult] = useState<MonitorPreflightResult | null>(null);
  const [preflightError, setPreflightError] = useState("");
  const [createBusy, setCreateBusy] = useState(false);
  const [createError, setCreateError] = useState("");

  const cachedHealth = srcTaskId ? getMonitorSchedulerHealth(srcTaskId) : null;
  const schedulerHealth = liveHealth ?? cachedHealth;

  const refreshHealth = useCallback(async () => {
    if (!srcTaskId) return;
    setHealthBusy(true);
    setHealthError("");
    try {
      const next = await querySrc<MonitorSchedulerHealth>(srcTaskId, "src.monitor.scheduler_health", {});
      setLiveHealth(next);
    } catch (err) {
      setHealthError(err instanceof Error ? err.message : String(err));
    } finally {
      setHealthBusy(false);
    }
  }, [srcTaskId]);

  const openCreateDialog = useCallback(() => {
    setCreateOpen(true);
    setPreflightResult(null);
    setPreflightError("");
    setCreateError("");
    if (!srcTaskId) return;
    void querySrc<{ connectors?: Array<{ connector_id: string; readiness?: string }> }>(srcTaskId, "src.connector.status", {})
      .then((res) => {
        const list = (res.connectors ?? []).filter((c) => ONLINE_MONITOR_CONNECTORS.includes(c.connector_id as never));
        setConnectors(list);
        const ready = list.find((c) => c.readiness === "ready" || c.readiness === "configured_unverified") ?? list[0];
        if (ready?.connector_id) setMonConnector(ready.connector_id);
      })
      .catch((err) => {
        setPreflightError(err instanceof Error ? err.message : String(err));
      });
  }, [srcTaskId]);

  const runPreflight = useCallback(async (): Promise<MonitorPreflightResult | null> => {
    if (!srcTaskId) return null;
    setPreflightBusy(true);
    setPreflightError("");
    try {
      const policy = await querySrc<Record<string, unknown>>(srcTaskId, "src.policy.current", {});
      const authorizationRef = `${String(policy.policy_id || "src-policy")}:v${Number(policy.version || 0)}`;
      let sourcePayload: Record<string, unknown> = {
        connector_id: monConnector,
        ...(monQuery.trim() ? { query: monQuery.trim() } : {}),
      };
      try {
        const cfg = await querySrc<{ version?: number; manifest?: { version?: string } }>(srcTaskId, "src.connector.config", { connector_id: monConnector });
        sourcePayload = {
          ...sourcePayload,
          config_version: Number(cfg.version || 1),
          capability_manifest_ref: String(cfg.manifest?.version || "v1"),
        };
      } catch {
      }
      const interval = Math.max(60, Number(monIntervalSec) || 3600);
      const total = Math.max(1, Number(monBudgetTotal) || 100);
      const perCycle = Math.max(1, Number(monBudgetPerCycle) || Math.min(total, 10));
      const res = await querySrc<MonitorPreflightResult>(srcTaskId, "src.monitor.preflight", {
        definition: {
          continuous: true,
          name: monName.trim() || "monitor-preflight",
          chain_type: monChain,
          authorization_ref: authorizationRef,
          source: sourcePayload,
          schedule: { interval_seconds: interval },
          budget: { total, per_cycle: perCycle, call_limit: perCycle },
        },
      });
      setPreflightResult(res);
      return res;
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      setPreflightError(msg);
      return null;
    } finally {
      setPreflightBusy(false);
    }
  }, [monBudgetPerCycle, monBudgetTotal, monChain, monConnector, monIntervalSec, monName, monQuery, srcTaskId]);

  const submitMonitor = async () => {
    if (!monName.trim() || !monQuery.trim()) return;
    if (!srcTaskId) {
      studio.createMonitor({
        projectId: state.workspace.projectId,
        name: monName.trim(),
        chainType: monChain,
        queryExpression: monQuery.trim(),
        frequencyLabel: "manual",
        budgetTotal: Number(monBudgetTotal) || 100,
      });
      setMonName("");
      setMonQuery("");
      setCreateOpen(false);
      if (hasAttachedDomains()) {
        void afterDomainMutation().then((res) => { if (res.ok) toast({ title: "已创建监控", tone: "success" }); });
      }
      return;
    }
    setCreateBusy(true);
    setCreateError("");
    try {
      const check = await runPreflight();
      if (!check) {
        setCreateError("监控预检未完成，请先修复预检报错。");
        return;
      }
      const blocking = check.blocking_reasons ?? [];
      if (check.ok === false || blocking.length > 0) {
        setCreateError(blocking.map((b) => b.message || b.code).filter(Boolean).join("；") || "监控预检未通过。");
        return;
      }
      const [policy, cfg] = await Promise.all([
        querySrc<Record<string, unknown>>(srcTaskId, "src.policy.current", {}),
        querySrc<{ version?: number; manifest?: { version?: string } }>(srcTaskId, "src.connector.config", { connector_id: monConnector }),
      ]);
      const interval = Math.max(60, Number(monIntervalSec) || 3600);
      const total = Math.max(1, Number(monBudgetTotal) || 100);
      const perCycle = Math.max(1, Number(monBudgetPerCycle) || Math.min(total, 10));
      await postCommand(srcTaskId, "src.monitor.create", {
        payload: {
          continuous: true,
          name: monName.trim(),
          chain_type: monChain,
          authorization_ref: `${String(policy.policy_id || "src-policy")}:v${Number(policy.version || 0)}`,
          source: {
            connector_id: monConnector,
            config_version: Number(cfg.version || 1),
            capability_manifest_ref: String(cfg.manifest?.version || "v1"),
            query: monQuery.trim(),
          },
          schedule: { interval_seconds: interval },
          budget: { total, per_cycle: perCycle, call_limit: perCycle },
        },
      });
      await studio.reloadDomain("tasks");
      setMonName("");
      setMonQuery("");
      setPreflightResult(null);
      setCreateOpen(false);
      toast({ title: "已创建监控", tone: "success" });
    } catch (err) {
      setCreateError(err instanceof Error ? err.message : String(err));
    } finally {
      setCreateBusy(false);
    }
  };
  void active;
  return (
    <div className="mx-auto flex w-full max-w-[760px] flex-col gap-4 p-4 pb-10" data-testid="studio-monitors-drill">
      {srcTaskId ? (
        <div>
          <SectionHeader
            title="调度器健康"
            action={<Button size="xs" variant="secondary" disabled={healthBusy} onClick={() => { void refreshHealth(); }}>刷新状态</Button>}
          />
          {healthError ? (
            <Callout tone="danger" action={<Button size="xs" variant="secondary" onClick={() => { void refreshHealth(); }}>重试</Button>}>
              读取调度器健康失败：{healthError}
            </Callout>
          ) : schedulerHealth ? (
            <SurfaceCard className="p-3">
              <div className="mb-2 flex items-center gap-2">
                <StudioBadge tone={schedulerHealth.scheduler_running ? "success" : "warning"}>
                  {schedulerHealth.scheduler_running ? "调度运行中" : `调度未运行 (${schedulerHealth.state || "stopped"})`}
                </StudioBadge>
                {typeof schedulerHealth.consecutive_failures === "number" && schedulerHealth.consecutive_failures > 0 ? (
                  <StudioBadge tone="danger">连续失败 {schedulerHealth.consecutive_failures}</StudioBadge>
                ) : null}
              </div>
              <Dl items={[
                { label: "活跃监控", value: String(schedulerHealth.active_monitors ?? 0) },
                { label: "未决周期", value: String(schedulerHealth.unresolved_cycles ?? 0) },
                { label: "重启次数", value: String(schedulerHealth.restart_count ?? 0) },
                { label: "外部通知", value: schedulerHealth.external_notifications ? "已配置" : "未配置" },
                ...(schedulerHealth.last_error ? [{ label: "最近错误", value: String(schedulerHealth.last_error) }] : []),
              ]} />
            </SurfaceCard>
          ) : (
            <SurfaceCard className="p-3 text-cx-xs text-cx-fg-3">尚未读取到调度器心跳</SurfaceCard>
          )}
        </div>
      ) : null}

      <div className="flex items-center justify-between">
        <SectionHeader title="监控任务" count={monitors.length} />
        <Button size="sm" variant="secondary" icon="plus" onClick={openCreateDialog}>新建监控</Button>
      </div>
      {monitors.map((m) => (
        <div key={m.id}>
          <SectionHeader title={m.name} action={<Menu ariaLabel={`监控 ${m.name}`} trigger={<IconButton size="sm" icon="more" label="更多" />}><MenuItem icon="play" onSelect={() => studio.runMonitorCycle(m.id)}>运行一轮</MenuItem></Menu>} />
          <SurfaceCard className="p-3">
            <Dl items={[
              { label: "查询", value: `${m.queryExpression} · v${m.queryVersion}`, mono: true },
              { label: "频率", value: m.frequencyLabel },
              { label: "预算", value: `${m.budgetUsed}/${m.budgetTotal}（${Math.round((m.budgetUsed / Math.max(1, m.budgetTotal)) * 100)}%）` },
            ]} />
            <div className="mt-3">
              <Button size="sm" variant="primary" onClick={() => studio.toggleMonitorStatus(m.id)}>{m.status === "active" ? "暂停" : "恢复"}</Button>
            </div>
            {(m.cycles ?? []).length ? (
              <div className="mt-3 flex flex-col divide-y divide-cx-border-subtle">
                {m.cycles.map((c) => (
                  <div key={c.id} className="flex items-center gap-2 py-1.5 text-cx-sm">
                    <StatusDot tone={c.status === "completed" ? "success" : c.status === "partial" ? "warning" : "danger"} />
                    <span className="min-w-0 flex-1 truncate text-cx-fg">{c.summary}</span>
                    <span className="shrink-0 text-cx-xs text-cx-fg-4">{c.finishedAt}</span>
                  </div>
                ))}
              </div>
            ) : null}
          </SurfaceCard>
        </div>
      ))}
      {!monitors.length ? <EmptyState compact icon="bell" title="暂无监控" /> : null}
      {state.changes.length ? <ChangesDrillIn active={active} embedded /> : null}
      <Dialog
        open={createOpen}
        onOpenChange={setCreateOpen}
        title="新建监控"
        size="md"
        footer={
          <div className="flex items-center justify-end gap-2">
            <Button variant="ghost" disabled={createBusy} onClick={() => setCreateOpen(false)}>取消</Button>
            {srcTaskId ? (
              <Button variant="secondary" disabled={preflightBusy || createBusy} onClick={() => { void runPreflight(); }}>
                {preflightBusy ? "预检中…" : "运行预检"}
              </Button>
            ) : null}
            <Button variant="primary" disabled={createBusy || preflightBusy || !monName.trim() || !monQuery.trim()} onClick={() => { void submitMonitor(); }}>
              {createBusy ? "创建中…" : "创建"}
            </Button>
          </div>
        }
      >
        <div className="flex flex-col gap-3">
          <TextField label="名称" value={monName} onChange={(e) => setMonName(e.target.value)} />
          <div>
            <Label>监控类型</Label>
            <Select<"asset" | "rule">
              value={monChain}
              onChange={setMonChain}
              ariaLabel="监控类型"

              options={[{ value: "asset", label: "资产变化" }, { value: "rule", label: "规则/代码仓" }]}
            />
          </div>
          {srcTaskId ? (
            <div>
              <Label>数据源连接器</Label>
              <Select<string>
                ariaLabel="数据源连接器"
                value={monConnector}
                onChange={(next) => { setMonConnector(next); setPreflightResult(null); }}

                options={ONLINE_MONITOR_CONNECTORS.map((id) => {
                  const status = connectors.find((c) => c.connector_id === id)?.readiness;
                  return {
                    value: id,
                    label: status ? `${id} (${status})` : id,
                  };
                })}
              />
            </div>
          ) : null}
          <TextField label="查询表达式或仓库" value={monQuery} onChange={(e) => { setMonQuery(e.target.value); setPreflightResult(null); }} />
          {srcTaskId ? (
            <div className="grid grid-cols-3 gap-2">
              <TextField label="周期（秒）" value={monIntervalSec} onChange={(e) => setMonIntervalSec(e.target.value)} />
              <TextField label="总预算" value={monBudgetTotal} onChange={(e) => setMonBudgetTotal(e.target.value)} />
              <TextField label="单轮上限" value={monBudgetPerCycle} onChange={(e) => setMonBudgetPerCycle(e.target.value)} />
            </div>
          ) : null}
          {preflightError ? <Callout tone="danger">{preflightError}</Callout> : null}
          {preflightResult ? (
            <div className="space-y-2">
              {(preflightResult.blocking_reasons ?? []).length > 0 ? (
                <Callout tone="danger" title="预检阻塞项">
                  <ul className="list-disc pl-4 text-cx-xs">
                    {(preflightResult.blocking_reasons ?? []).map((r, i) => <li key={i}>{r.message || r.code}</li>)}
                  </ul>
                </Callout>
              ) : (
                <Callout tone="success">
                  预检通过 · 连接器就绪度：{preflightResult.connector_readiness ?? "unknown"} · 授权 v{preflightResult.policy_version ?? "?"}
                </Callout>
              )}
              {(preflightResult.warnings ?? []).length > 0 ? (
                <Callout tone="warning" title="预检提示">
                  <ul className="list-disc pl-4 text-cx-xs">
                    {(preflightResult.warnings ?? []).map((w, i) => <li key={i}>{w.message || w.code}</li>)}
                  </ul>
                </Callout>
              ) : null}
            </div>
          ) : null}
          {createError ? <Callout tone="danger">{createError}</Callout> : null}
        </div>
      </Dialog>
    </div>
  );
}

function parseScopeTargets(raw: string): string[] {
  return raw
    .split(/[\n,]+/)
    .map((s) => s.trim())
    .filter(Boolean);
}

function ScopeDrillIn() {
  const state = useStudioState();
  const srcTaskId = attachedTaskId(state);
  const snapshots = state.scopeSnapshots ?? [];
  const snapshot = snapshots[0];

  const [renewOpen, setRenewOpen] = useState(false);
  const [baseVersion, setBaseVersion] = useState(1);
  const [targetsText, setTargetsText] = useState("");
  const [excludedText, setExcludedText] = useState("");
  const [notesText, setNotesText] = useState("");
  const [diffSummary, setDiffSummary] = useState("");
  const [overrideReason, setOverrideReason] = useState("");
  const [renewBusy, setRenewBusy] = useState(false);
  const [renewLoading, setRenewLoading] = useState(false);
  const [renewError, setRenewError] = useState("");

  const openRenewDialog = () => {
    setRenewError("");
    setDiffSummary("");
    setOverrideReason("");
    setTargetsText((snapshot?.allowedTargets ?? []).join("\n"));
    setExcludedText((snapshot?.excludedTargets ?? []).join("\n"));
    setNotesText(getScopeSnapshotMeta(snapshot?.id ?? "")?.notes ?? "");
    setBaseVersion(getScopeSnapshotMeta(snapshot?.id ?? "")?.policyVersion ?? snapshot?.version ?? 1);
    setRenewOpen(true);
    if (!srcTaskId) return;
    setRenewLoading(true);
    void querySrc<Record<string, unknown>>(srcTaskId, "src.policy.current", {})
      .then((p) => {
        if (Number.isFinite(Number(p.version))) setBaseVersion(Number(p.version));
        if (Array.isArray(p.authorized_targets)) setTargetsText(p.authorized_targets.map(String).join("\n"));
        if (Array.isArray(p.excluded_targets)) setExcludedText(p.excluded_targets.map(String).join("\n"));
        if (typeof p.notes === "string" && p.notes) setNotesText(p.notes);
        else if (typeof p.raw_policy_text === "string" && p.raw_policy_text) setNotesText(p.raw_policy_text);
      })
      .catch((err) => {
        setRenewError(err instanceof Error ? err.message : String(err));
      })
      .finally(() => {
        setRenewLoading(false);
      });
  };

  const submitRenewal = async () => {
    if (!srcTaskId) return;
    const targets = parseScopeTargets(targetsText);
    if (!targets.length) {
      setRenewError("请至少填写 1 个授权目标。");
      return;
    }
    setRenewBusy(true);
    setRenewError("");
    try {
      await renewSrcAuthorization(srcTaskId, {
        base_policy_version: baseVersion,
        targets,
        excluded_targets: parseScopeTargets(excludedText),
        ...(notesText.trim() ? { notes: notesText.trim() } : {}),
        ...(diffSummary.trim() ? { diff_summary: diffSummary.trim() } : {}),
        ...(overrideReason.trim() ? { scope_fuse_override_reason: overrideReason.trim() } : {}),
      });
      await Promise.all([studio.reloadDomain("tasks"), studio.reloadDomain("assets")]);
      toast({ title: "已更新本期授权范围", tone: "success" });
      setRenewOpen(false);
    } catch (err) {
      setRenewError(err instanceof Error ? err.message : String(err));
    } finally {
      setRenewBusy(false);
    }
  };

  return (
    <div className="mx-auto flex w-full max-w-[760px] flex-col gap-4 p-4 pb-10" data-testid="studio-scope-drill">
      <div>
        <SectionHeader
          title="范围快照与版本历史"
          count={snapshots.length}
          action={srcTaskId ? <Button size="sm" variant="secondary" onClick={openRenewDialog}>续期 / 更新本期授权范围</Button> : undefined}
        />
        {snapshot ? (
          <SurfaceCard className="p-3">
            <Dl items={[
              { label: "当前版本", value: `v${snapshot.version} · ${snapshot.createdAt}` },
              { label: "允许", value: (snapshot.allowedTargets ?? []).join("、") || "—" },
              { label: "排除", value: (snapshot.excludedTargets ?? []).join("、") || "—" },
              { label: "条件", value: (snapshot.conditions ?? []).join("、") || "—" },
              ...(snapshot.diffSummary ? [{ label: "变更说明", value: snapshot.diffSummary }] : []),
            ]} />
          </SurfaceCard>
        ) : (
          <SurfaceCard><SurfaceRow icon="shield" title="暂无授权范围快照" /></SurfaceCard>
        )}

        {snapshots.length > 0 ? (
          <div className="mt-3 space-y-2">
            {snapshots.map((snap, idx) => {
              const prev = snapshots[idx + 1];
              const meta = getScopeSnapshotMeta(snap.id);
              const added = meta?.addedTargets.length
                ? meta.addedTargets
                : prev
                ? (snap.allowedTargets ?? []).filter((t) => !(prev.allowedTargets ?? []).includes(t))
                : [];
              const removed = meta?.removedTargets.length
                ? meta.removedTargets
                : prev
                ? (prev.allowedTargets ?? []).filter((t) => !(snap.allowedTargets ?? []).includes(t))
                : [];
              return (
                <SurfaceCard key={snap.id || idx} className="p-3">
                  <div className="flex flex-wrap items-center justify-between gap-2 text-cx-xs">
                    <span className="font-medium text-cx-fg">v{snap.version} · {snap.author || "operator"}</span>
                    <span className="text-cx-fg-4">{snap.createdAt}</span>
                  </div>
                  {snap.diffSummary ? <p className="mt-1 text-cx-xs text-cx-fg-2">{snap.diffSummary}</p> : null}
                  {(added.length > 0 || removed.length > 0) ? (
                    <div className="mt-2 flex flex-wrap gap-1.5 font-cx-mono text-cx-2xs">
                      {added.map((t) => <StudioBadge key={`add-${t}`} tone="success">+ {t}</StudioBadge>)}
                      {removed.map((t) => <StudioBadge key={`rem-${t}`} tone="danger">- {t}</StudioBadge>)}
                    </div>
                  ) : null}
                </SurfaceCard>
              );
            })}
          </div>
        ) : null}
      </div>

      <Dialog
        open={renewOpen}
        onOpenChange={setRenewOpen}
        title="续期 / 更新本期授权范围"
        description={`基于策略版本 v${baseVersion} 更新允许与排除目标，并生成新的授权快照。`}
        size="lg"
        dismissable={false}
        footer={
          <div className="flex items-center justify-end gap-2">
            <Button size="sm" variant="ghost" disabled={renewBusy} onClick={() => setRenewOpen(false)}>取消</Button>
            <Button size="sm" variant="primary" disabled={renewBusy || renewLoading || parseScopeTargets(targetsText).length === 0} onClick={() => { void submitRenewal(); }}>
              {renewBusy ? "保存中…" : "确认更新授权"}
            </Button>
          </div>
        }
      >
        <div className="space-y-3">
          <div>
            <label className="mb-1 block text-cx-xs font-medium text-cx-fg">允许目标（每行或逗号分隔）</label>
            <TextArea rows={4} value={targetsText} onChange={(e) => setTargetsText(e.target.value)} placeholder="https://example.com/" />
          </div>
          <div>
            <label className="mb-1 block text-cx-xs font-medium text-cx-fg">排除目标（可选）</label>
            <TextArea rows={2} value={excludedText} onChange={(e) => setExcludedText(e.target.value)} placeholder="https://admin.example.com/" />
          </div>
          <div>
            <label className="mb-1 block text-cx-xs font-medium text-cx-fg">授权备注 / 限制说明（可选）</label>
            <TextArea rows={2} value={notesText} onChange={(e) => setNotesText(e.target.value)} />
          </div>
          <TextField label="变更摘要（可选）" value={diffSummary} onChange={(e) => setDiffSummary(e.target.value)} placeholder="例如：续期主站并新增 API 子域" />
          {(renewError || overrideReason) ? (
            <TextField label="范围熔断复核理由（若触发范围保护时填写）" value={overrideReason} onChange={(e) => setOverrideReason(e.target.value)} />
          ) : null}
          {renewError ? <Callout tone="danger">{renewError}</Callout> : null}
        </div>
      </Dialog>
    </div>
  );
}

function EnvDrillIn() {
  const state = useStudioState();
  const env = state.environmentStatus;
  return (
    <div className="mx-auto flex w-full max-w-[760px] flex-col gap-4 p-4 pb-10" data-testid="studio-env-drill">
      <SurfaceCard className="p-3">
        <Dl items={[
          { label: "边界", value: env.sandboxBoundary },
          { label: "网络范围", value: <SegmentedControl<"declared" | "enforced"> value={env.networkScopeMode} onChange={(v) => studio.updateEnvironmentMode({ networkScopeMode: v })} size="sm" ariaLabel="网络范围" options={[{ value: "declared", label: "声明" }, { value: "enforced", label: "强制" }]} /> },
          { label: "HTTP 取证", value: <SegmentedControl<"off" | "record" | "enforce"> value={env.httpInspectionMode} onChange={(v) => studio.updateEnvironmentMode({ httpInspectionMode: v })} size="sm" ariaLabel="HTTP 取证" options={[{ value: "off", label: "关闭" }, { value: "record", label: "记录" }, { value: "enforce", label: "强制" }]} /> },
        ]} />
      </SurfaceCard>
      <div>
        <SectionHeader title="通道" count={env.channels.length} />
        <SurfaceCard>
          {env.channels.map((ch) => <SurfaceRow key={ch.id} icon="network" title={ch.name} subtitle={ch.status === "enforced" ? "强制受控" : ch.status === "declared_only" ? "仅声明" : "记录"} />)}
        </SurfaceCard>
      </div>
    </div>
  );
}

type CoordinationStatus = {
  task_lifecycle?: { state?: string };
  policy?: {
    enabled?: boolean;
    auto_recon?: boolean;
    auto_reproduce?: boolean;
    merge_window_seconds?: number;
    max_concurrent_runs?: number;
    max_coordination_tokens?: number;
  };
  pending_wakeups?: Array<{
    wakeup_id?: string;
    title?: string;
    source_kind?: string;
    run_id?: string;
    target_run_id?: string;
    refs?: { run_id?: string; monitor_id?: string };
  }>;
  recent_decisions?: Array<{
    decision_id?: string;
    action?: string;
    rationale?: string;
    status?: string;
    effect_state?: string;
    effect_status?: string;
    target_run_id?: string;
    finding_id?: string;
    report_id?: string;
    created_at?: string;
  }>;
  unread_notices?: Array<{
    notice_id?: string;
    decision_id?: string;
    action?: string;
    rationale?: string;
    summary?: string;
    target_run_id?: string;
    finding_id?: string;
    report_id?: string;
  }>;
  metrics?: {
    pending_count?: number;
    decided_count?: number;
    effect_confirmed_count?: number;
    active_monitors?: number;
    active_runs?: number;
  };
};

function effectStatusMeta(status?: string): { label: string; tone: "success" | "warning" | "neutral" | "danger" } {
  switch (status) {
    case "effect_confirmed":
      return { label: "已确认", tone: "success" };
    case "effect_failed":
      return { label: "未生效", tone: "danger" };
    case "effect_unknown":
    case "pending_confirmation":
    case "pending":
      return { label: "待确认", tone: "warning" };
    default:
      return { label: "已记录", tone: "neutral" };
  }
}

function SuperAgentCoordinationCard({
  taskId,
  status,
  error,
  onRetry,
  onStatusChange,
}: {
  taskId: string;
  status: CoordinationStatus | null;
  error?: string;
  onRetry?: () => void;
  onStatusChange: (next: CoordinationStatus) => void;
}) {
  const [savingKey, setSavingKey] = useState<"" | "enabled" | "auto_recon" | "auto_reproduce">("");

  if (!taskId) return null;
  const enabled = status?.policy?.enabled ?? true;
  const autoRecon = status?.policy?.auto_recon ?? true;
  const autoReproduce = status?.policy?.auto_reproduce ?? true;
  const pendingWakeups = (status?.pending_wakeups ?? []).slice(0, 4);
  const recentDecisions = (status?.recent_decisions ?? []).slice(0, 3);
  const pendingCount = status?.metrics?.pending_count ?? status?.pending_wakeups?.length ?? 0;
  const activeMonitors = status?.metrics?.active_monitors ?? 0;
  const activeRuns = status?.metrics?.active_runs ?? 0;

  const togglePolicy = async (key: "enabled" | "auto_recon" | "auto_reproduce", next: boolean) => {
    setSavingKey(key);
    try {
      await postCommand(taskId, "src.coordination.policy.save", { [key]: next, idempotency_key: crypto.randomUUID() });
      onStatusChange({ ...status, policy: { ...status?.policy, [key]: next } });
      const names = { enabled: "自动唤醒", auto_recon: "自动侦察", auto_reproduce: "自动复现" };
      toast({ title: `${next ? "已开启" : "已暂停"}${names[key]}`, tone: "success" });
    } catch (err) {
      toast({ title: err instanceof Error ? err.message : "更新失败", tone: "danger" });
    } finally {
      setSavingKey("");
    }
  };

  return (
    <div className="space-y-2">
      <SectionHeader title="自动跟进与协调策略" count={pendingCount || undefined} />
      {error ? (
        <Callout tone="warning" action={onRetry ? <Button size="xs" variant="secondary" onClick={onRetry}>重试</Button> : undefined}>
          读取协调状态失败：{error}
        </Callout>
      ) : null}
      <SurfaceCard>
        <div className="flex items-center justify-between gap-3 border-b border-cx-border-subtle px-3.5 py-2.5">
          <div className="min-w-0 flex-1">
            <p className="text-cx-sm font-medium text-cx-fg">自动唤醒</p>
            <p className="truncate text-cx-xs text-cx-fg-4">
              {`监控 ${activeMonitors} · 运行 ${activeRuns} · 待处理 ${pendingCount}`}
            </p>
          </div>
          <Switch checked={enabled} disabled={Boolean(savingKey)} onCheckedChange={(checked) => void togglePolicy("enabled", checked)} aria-label="自动唤醒开关" />
        </div>
        <div className="flex flex-wrap items-center justify-between gap-4 border-b border-cx-border-subtle px-3.5 py-2 text-cx-xs">
          <label className="flex items-center gap-2 text-cx-fg-2">
            <span>自动侦察（auto_recon）</span>
            <Switch checked={autoRecon} disabled={Boolean(savingKey)} onCheckedChange={(checked) => void togglePolicy("auto_recon", checked)} aria-label="自动侦察开关" />
          </label>
          <label className="flex items-center gap-2 text-cx-fg-2">
            <span>自动复现（auto_reproduce）</span>
            <Switch checked={autoReproduce} disabled={Boolean(savingKey)} onCheckedChange={(checked) => void togglePolicy("auto_reproduce", checked)} aria-label="自动复现开关" />
          </label>
        </div>
        {pendingWakeups.map((w, i) => (
          <SurfaceRow
            key={w.wakeup_id ?? `w-${i}`}
            icon="bell"
            title={w.title || w.source_kind || "待处理事件"}
            subtitle={w.source_kind ?? ""}
            onClick={() => {
              const runId = w.refs?.run_id || w.run_id || w.target_run_id;
              if (runId) studio.setTasksUi({ tab: "runs", selectedRunId: runId });
              else if (w.source_kind === "monitor_change" || w.source_kind === "monitor") studio.setTasksUi({ tab: "monitors" });
            }}
          />
        ))}
        {recentDecisions.map((d, i) => {
          const eff = effectStatusMeta(d.effect_state ?? d.effect_status ?? d.status);
          return (
            <SurfaceRow
              key={d.decision_id ?? `d-${i}`}
              icon="sparkles"
              title={d.action ?? "决策"}
              subtitle={d.rationale ?? ""}
              trailing={<StudioBadge tone={eff.tone}>{eff.label}</StudioBadge>}
              onClick={() => {
                if (d.target_run_id) studio.setTasksUi({ tab: "runs", selectedRunId: d.target_run_id });
                else if (d.report_id) openRef({ kind: "report", id: d.report_id });
                else if (d.finding_id) openWorkspace("findings");
              }}
            />
          );
        })}
      </SurfaceCard>
    </div>
  );
}

function CoverageDrillIn({ taskId }: { taskId: string }) {
  const [audit, setAudit] = useState<Record<string, unknown> | null>(null);
  const [busy, setBusy] = useState(false);
  const [addOpen, setAddOpen] = useState(false);
  const [itemMethod, setItemMethod] = useState("GET");
  const [itemTarget, setItemTarget] = useState("");
  const [itemHypothesis, setItemHypothesis] = useState("");
  const [itemPriority, setItemPriority] = useState("high");
  const [blockerTarget, setBlockerTarget] = useState("");
  const [blockerOpen, setBlockerOpen] = useState(false);
  const [blockerCategory, setBlockerCategory] = useState("auth_required");
  const [blockerDetail, setBlockerDetail] = useState("");

  const load = useCallback(async () => {
    if (!taskId) return;
    setBusy(true);
    try {
      const res = await querySrc<Record<string, unknown>>(taskId, "src.coverage.audit", {});
      setAudit(res);
    } catch (err) {
      toast({ title: "加载覆盖率失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
    } finally {
      setBusy(false);
    }
  }, [taskId]);

  useEffect(() => {
    void load();
  }, [load]);

  const seed = async () => {
    if (!taskId) return;
    setBusy(true);
    try {
      await postCommand(taskId, "src.coverage.seed", { derive: true, idempotency_key: crypto.randomUUID() });
      await load();
      toast({ title: "已生成覆盖率基线", tone: "success" });
    } catch (err) {
      toast({ title: "生成失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
    } finally {
      setBusy(false);
    }
  };

  const saveItem = async () => {
    if (!taskId || !itemTarget.trim() || !itemHypothesis.trim()) return;
    setBusy(true);
    try {
      await postCommand(taskId, "src.coverage.item.save", {
        item_id: `cov-${Date.now().toString(36)}`,
        method: itemMethod,
        target: itemTarget.trim(),
        hypothesis: itemHypothesis.trim(),
        priority: itemPriority,
      });
      setAddOpen(false);
      setItemTarget("");
      setItemHypothesis("");
      await load();
      toast({ title: "已保存覆盖率条目", tone: "success" });
    } catch (err) {
      toast({ title: "保存失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
    } finally {
      setBusy(false);
    }
  };

  const claimItem = async (itemId: string) => {
    if (!taskId) return;
    setBusy(true);
    try {
      await postCommand(taskId, "src.coverage.item.claim", { item_id: itemId, note: "workbench_claim" });
      await load();
    } catch (err) {
      toast({ title: "认领失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
    } finally {
      setBusy(false);
    }
  };

  const saveBlocker = async () => {
    if (!taskId || !blockerDetail.trim()) return;
    setBusy(true);
    try {
      await postCommand(taskId, "src.coverage.blocker.record", {
        blocker_id: `blk-${Date.now().toString(36)}`,
        category: blockerCategory,
        target: blockerTarget.trim() || "https://target",
        detail: blockerDetail.trim(),
      });
      setBlockerOpen(false);
      setBlockerDetail("");
      await load();
      toast({ title: "已记录受阻项", tone: "success" });
    } catch (err) {
      toast({ title: "记录失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
    } finally {
      setBusy(false);
    }
  };

  const items = Array.isArray(audit?.items) ? (audit.items as Array<Record<string, unknown>>) : [];
  const blockers = Array.isArray(audit?.open_blockers) ? (audit.open_blockers as Array<Record<string, unknown>>) : [];
  const counts = (audit?.counts as Record<string, unknown> | undefined) ?? {};

  return (
    <div className="mx-auto flex w-full max-w-[760px] flex-col gap-4 p-4 pb-10" data-testid="studio-coverage-drill">
      <div className="flex flex-wrap items-center gap-2">
        <Button size="sm" variant="primary" disabled={!taskId || busy} onClick={() => { void seed(); }}>生成基线</Button>
        <Button size="sm" variant="secondary" disabled={!taskId || busy} onClick={() => setAddOpen(true)}>新增条目</Button>
        <Button size="sm" variant="ghost" icon="refresh" disabled={!taskId || busy} onClick={() => { void load(); }}>刷新</Button>
      </div>
      <SurfaceCard className="p-3">
        <Dl items={[
          { label: "门禁状态", value: String(audit?.status ?? "unmodeled") },
          { label: "已验证 / 适用", value: `${String(audit?.verified_items ?? 0)} / ${String(audit?.applicable_items ?? 0)}` },
          { label: "待测", value: String(counts.pending ?? 0) },
          { label: "受阻", value: String(blockers.length) },
        ]} />
      </SurfaceCard>
      <div>
        <SectionHeader title="覆盖条目" count={items.length} />
        <SurfaceCard>
          {items.length ? items.map((it) => {
            const id = String(it.item_id ?? "");
            const st = String(it.evidence_state ?? "pending");
            return (
              <SurfaceRow
                key={id}
                icon="listChecks"
                title={`${String(it.method ?? "GET")} ${String(it.target ?? "")}`}
                subtitle={String(it.hypothesis ?? "")}
                trailing={(
                  <span className="flex items-center gap-1.5">
                    <StudioBadge tone={it.priority === "high" || it.priority === "critical" ? "warning" : "neutral"}>{String(it.priority ?? "medium")}</StudioBadge>
                    <StudioBadge tone={st === "verified" ? "success" : st === "blocked" || st === "failed" ? "danger" : "neutral"}>{st}</StudioBadge>
                    {st === "pending" ? <Button size="sm" variant="secondary" disabled={busy} onClick={() => { void claimItem(id); }}>认领</Button> : null}
                    <Button size="sm" variant="ghost" onClick={() => { setBlockerTarget(String(it.target ?? "")); setBlockerOpen(true); }}>记受阻</Button>
                  </span>
                )}
              />
            );
          }) : <SurfaceRow icon="listChecks" title="暂无覆盖率条目" />}
        </SurfaceCard>
      </div>
      {blockers.length ? (
        <div>
          <SectionHeader title="未解除阻塞项" count={blockers.length} />
          <SurfaceCard>
            {blockers.map((b, i) => (
              <SurfaceRow key={String(b.blocker_id ?? i)} icon="alert" title={String(b.category ?? "blocker")} subtitle={`${String(b.target ?? "")} · ${String(b.detail ?? "")}`} />
            ))}
          </SurfaceCard>
        </div>
      ) : null}
      <Dialog
        open={addOpen}
        onOpenChange={setAddOpen}
        title="新增覆盖率验证条目"
        size="md"
        footer={<><Button variant="ghost" onClick={() => setAddOpen(false)}>取消</Button><Button variant="primary" disabled={busy || !itemTarget.trim() || !itemHypothesis.trim()} onClick={() => { void saveItem(); }}>保存</Button></>}
      >
        <div className="flex flex-col gap-3">
          <div className="flex flex-col"><Label>方法</Label><Select<string> value={itemMethod} onChange={setItemMethod} options={["GET", "POST", "PUT", "DELETE", "PATCH"].map((m) => ({ value: m, label: m }))} ariaLabel="HTTP 方法" /></div>
          <TextField label="目标 URL" value={itemTarget} onChange={(e) => setItemTarget(e.target.value)} placeholder="http://127.0.0.1:51061/api/v1/orders/1" />
          <TextField label="验证假设" value={itemHypothesis} onChange={(e) => setItemHypothesis(e.target.value)} placeholder="IDOR 跨租户越权读取" />
          <div className="flex flex-col"><Label>优先级</Label><Select<string> value={itemPriority} onChange={setItemPriority} options={[{ value: "high", label: "high" }, { value: "medium", label: "medium" }, { value: "low", label: "low" }]} ariaLabel="优先级" /></div>
        </div>
      </Dialog>
      <Dialog
        open={blockerOpen}
        onOpenChange={setBlockerOpen}
        title="记录覆盖率阻塞项"
        size="md"
        footer={<><Button variant="ghost" onClick={() => setBlockerOpen(false)}>取消</Button><Button variant="primary" disabled={busy || !blockerDetail.trim()} onClick={() => { void saveBlocker(); }}>提交</Button></>}
      >
        <div className="flex flex-col gap-3">
          <TextField label="目标 URL" value={blockerTarget} onChange={(e) => setBlockerTarget(e.target.value)} />
          <TextField label="阻塞分类" value={blockerCategory} onChange={(e) => setBlockerCategory(e.target.value)} placeholder="auth_required" />
          <TextField label="阻塞说明" value={blockerDetail} onChange={(e) => setBlockerDetail(e.target.value)} placeholder="需双身份验证码" />
        </div>
      </Dialog>
    </div>
  );
}

function DrillIn({ crumb, children }: { crumb: string; children: React.ReactNode }) {
  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <SurfaceToolbar>
        <IconButton size="sm" icon="arrowLeft" label="返回概览" onClick={() => studio.setTasksUi({ tab: "overview", highlightId: "" })} />
        <PathBreadcrumb path={crumb} rootLabel="概览" lastIsFile onSelect={(dir) => { if (dir === "") { studio.setTasksUi({ tab: "overview", highlightId: "" }); } else { studio.setTasksUi({ selectedRunId: "" }); } }} />
      </SurfaceToolbar>
      <div className="cx-scroll min-h-0 flex-1 overflow-y-auto">{children}</div>
    </div>
  );
}

const SUBAGENT_TERMINAL_STATES = new Set([
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

function isLiveRunningSubagent(sub: NonNullable<StudioState["liveSubagents"]>[number]): boolean {
  const delivery = (sub.delivery || "").toLowerCase();
  if (delivery === "running") return true;
  if (delivery === "awaiting_settle" || delivery === "settled" || delivery === "not_delivered") return false;
  const exec = (sub.execution || "").toLowerCase();
  if (exec === "running" || exec === "working" || exec === "background") return true;
  const status = (sub.status || "").toLowerCase();
  return status ? !SUBAGENT_TERMINAL_STATES.has(status) : true;
}

const SUBAGENT_ROUTING_SOURCE_LABELS: Record<string, string> = {
  orchestration_role: "角色配置",
  workspace_policy: "工作区默认",
  explicit_request: "显式指定",
  inherited_parent: "继承主会话",
};

function formatSubagentRoutingLabel(sub: NonNullable<StudioState["liveSubagents"]>[number]): string {
  const source = sub.routing?.source || "";
  if (!source) return "";
  const label = SUBAGENT_ROUTING_SOURCE_LABELS[source] || source;
  const scopeTag =
    sub.routing?.workbench_mode === "src"
      ? sub.routing?.orchestration_scope === "mode_override"
        ? "SRC 独立"
        : "继承聊天"
      : "";
  if (source === "orchestration_role" || source === "workspace_policy") {
    const parts = [sub.routing?.role, scopeTag].filter(Boolean);
    return parts.length ? `${label}(${parts.join(" · ")})` : label;
  }
  return label;
}

function deriveTodoExecutionState(
  task: StudioTask,
  workRuns: StudioState["workRuns"],
  liveSubagents: StudioState["liveSubagents"],
): NonNullable<StudioTask["executionState"]> {
  if (task.status === "done") return "done";
  if (task.blockerReason || task.executionState === "blocked") return "blocked";
  if (task.waitingFor || task.executionState === "waiting_reply") return "waiting_reply";
  const hasLiveRun = task.executor?.kind === "run"
    && Boolean(task.executor.id && workRuns.some((r) => r.id === task.executor?.id && ["running", "queued", "planning"].includes(r.state)));
  const hasLiveSubthread = (liveSubagents ?? []).some(
    (s) => isLiveRunningSubagent(s) && ((task.executor?.kind === "subthread" && task.executor.id === s.thread_id) || s.todo_id === task.id),
  );
  if (hasLiveRun || hasLiveSubthread) return "running";
  if (task.status === "doing" || task.executionState === "manual_doing") return "manual_doing";
  return "todo";
}

function delegateTodoReadonlyAnalysis(task: StudioTask): boolean {
  const threadId = readStudioState().activeThreadId;
  const refs: StudioRef[] = [{ kind: "task", id: task.id }, ...(task.links ?? [])];
  for (const ref of refs) studio.addContext(threadId, ref);
  const refSummary = refs.map((r) => `${r.kind}:${r.id}`).join(", ");
  const prompt = [
    `请为待办任务发起只读子线程分析（delegate readonly_analysis）：`,
    `- todo_id: ${task.id}`,
    `- delegation_profile: readonly_analysis`,
    `- title: ${task.title}`,
    ...(task.notes ? [`- notes: ${task.notes}`] : []),
    `- allowed_refs: [${refSummary}]`,
    `请使用 muteki_start_thread({ subagent: true, todo_id: "${task.id}", delegation_profile: "readonly_analysis" }) 启动只读子线程完成核查（若 SRC 多 Agent 编排花名册中启用了匹配角色，可同时传入 role），并在子线程完成后回传结论与建议。`,
  ].join("\n");
  if (hasAttachedDomains()) {
    const accepted = askAi({ kind: "task", id: task.id }, prompt);
    if (accepted) {
      toast({ title: "已填充只读子线程委派指令", description: "发送后将由主对话启动只读分析子线程。", tone: "success" });
    }
    return accepted;
  }
  studio.send(threadId, prompt, refs, { taskId: task.id });
  toast({ title: "已发起只读分析委派", tone: "success" });
  return true;
}

const EXEC_STATE_META: Record<NonNullable<StudioTask["executionState"]>, { label: string; tone: "neutral" | "accent" | "running" | "warning" | "danger" | "success" }> = {
  todo: { label: "待办", tone: "neutral" },
  manual_doing: { label: "人工进行中", tone: "accent" },
  running: { label: "子线/Run 执行中", tone: "running" },
  waiting_reply: { label: "等待回复", tone: "warning" },
  blocked: { label: "受阻", tone: "danger" },
  done: { label: "已完成", tone: "success" },
};

function TaskRow({ task, srcTaskId }: { task: StudioTask; srcTaskId: string }) {
  const state = useStudioState();
  const ctx = useContextMenu();
  const done = task.status === "done";
  const execState = deriveTodoExecutionState(task, state.workRuns, state.liveSubagents);
  const execMeta = EXEC_STATE_META[execState];
  const [editingNotes, setEditingNotes] = useState(false);
  const [notesDraft, setNotesDraft] = useState(task.notes ?? "");
  const [notesBusy, setNotesBusy] = useState(false);

  useEffect(() => {
    if (!editingNotes) setNotesDraft(task.notes ?? "");
  }, [editingNotes, task.notes]);

  const saveNotes = async () => {
    if (!srcTaskId) {
      toast({ title: "仅真实研究任务支持持久化备注", tone: "warning" });
      return;
    }
    setNotesBusy(true);
    try {
      await postCommand(srcTaskId, "src.workbench.todo.save", {
        todo_id: task.id,
        payload: { notes: notesDraft.trim() },
      });
      await studio.reloadDomain("tasks");
      setEditingNotes(false);
      toast({ title: "已保存待办备注", tone: "success" });
    } catch (err) {
      toast({ title: "保存备注失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
    } finally {
      setNotesBusy(false);
    }
  };

  const actions = <>
    <MenuItem icon="sparkles" onSelect={() => handTaskToAi(task)}>交给 Agent</MenuItem>
    <MenuItem icon="bot" onSelect={() => delegateTodoReadonlyAnalysis(task)}>委派只读子线程分析</MenuItem>
    <MenuItem icon="pencil" onSelect={() => setEditingNotes((v) => !v)}>{editingNotes ? "收起备注" : "编辑备注"}</MenuItem>
    {!done && task.status !== "doing" ? (
      <MenuItem icon="play" onSelect={() => studio.setTaskStatus(task.id, "doing")}>标为人工进行中</MenuItem>
    ) : null}
    <MenuItem icon="check" onSelect={() => studio.setTaskStatus(task.id, done ? "todo" : "done")}>{done ? "重新打开" : "标为完成"}</MenuItem>
  </>;
  const detailParts = [
    task.assignee,
    task.due,
    task.waitingFor ? `等待：${task.waitingFor}` : "",
    task.blockerReason ? `阻塞：${task.blockerReason}` : "",
    task.reportBack ? `回传：${task.reportBack}` : "",
  ].filter(Boolean);

  return (
    <div onContextMenu={ctx.onContextMenu} onKeyDown={ctx.onKeyDown}>
      <SurfaceRow
        icon="listTodo"
        leading={(
          <span className="grid size-8 shrink-0 place-items-center">
            <Checkbox checked={done} aria-label={`完成「${task.title}」`} onCheckedChange={(checked) => studio.setTaskStatus(task.id, checked ? "done" : "todo")} className="[&>button]:size-5" />
          </span>
        )}
        title={
          <span className="flex flex-wrap items-center gap-1.5">
            <span className={done ? "line-through text-cx-fg-3" : ""}>{task.title}</span>
            <StudioBadge tone={execMeta.tone}>{execMeta.label}</StudioBadge>
          </span>
        }
        subtitle={
          <span className="flex flex-col gap-0.5">
            {detailParts.length ? <span>{detailParts.join(" · ")}</span> : null}
            {task.notes ? <span className="text-cx-xs text-cx-fg-2">备注：{task.notes}</span> : null}
          </span>
        }
        trailing={(
          <span className="flex items-center gap-1">
            <Button
              size="xs"
              variant="ghost"
              title="委派只读子线程分析"
              onClick={(e) => {
                e.stopPropagation();
                delegateTodoReadonlyAnalysis(task);
              }}
            >
              只读委派
            </Button>
            <Button
              size="xs"
              variant="ghost"
              title="编辑研究备注"
              onClick={(e) => {
                e.stopPropagation();
                setEditingNotes((v) => !v);
              }}
            >
              备注
            </Button>
            <Menu ariaLabel={`任务 ${task.title} 操作`} trigger={<IconButton size="sm" icon="more" label={`任务 ${task.title} 操作`} />}>
              {actions}
            </Menu>
          </span>
        )}
        onClick={() => studio.setTasksUi({ highlightId: task.id })}
      />
      {editingNotes ? (
        <div className="border-t border-cx-border-subtle bg-cx-bg-subtle px-4 py-2.5">
          <TextArea
            rows={2}
            value={notesDraft}
            onChange={(e) => setNotesDraft(e.target.value)}
            placeholder="记录该待办的验证笔记、排除依据或观察结论"
          />
          <div className="mt-2 flex items-center justify-end gap-2">
            <Button size="xs" variant="ghost" disabled={notesBusy} onClick={() => setEditingNotes(false)}>取消</Button>
            <Button size="xs" variant="primary" disabled={notesBusy} onClick={() => { void saveNotes(); }}>
              {notesBusy ? "保存中…" : "保存备注"}
            </Button>
          </div>
        </div>
      ) : null}
      {ctx.render(actions, `任务 ${task.title} 操作`)}
    </div>
  );
}

const SUGGESTION_KIND_LABELS: Record<string, string> = {
  task: "待办建议",
  clue: "发现线索",
  new_finding: "新发现",
  finding: "发现补充",
  classify: "资产分类",
  recategorize: "重新归类",
  conflict_resolve: "冲突裁决",
  report_draft: "报告草稿",
  plan_update: "计划修订",
  retest_record: "复测记录",
};

interface ProjectMemoryRecord {
  scope_key?: string;
  project_id?: string | null;
  preferences: string;
  notes: string;
  revision: number;
  updated_by?: string;
  updated_at?: string;
}

function ProjectMemoryCard({ taskId }: { taskId: string }) {
  const [memory, setMemory] = useState<ProjectMemoryRecord | null>(null);
  const [editing, setEditing] = useState(false);
  const [prefDraft, setPrefDraft] = useState("");
  const [notesDraft, setNotesDraft] = useState("");
  const [busy, setBusy] = useState(false);

  const loadMemory = useCallback(async () => {
    try {
      const res = await querySrc<ProjectMemoryRecord>(taskId, "src.project.memory.get", {});
      setMemory(res);
    } catch {
    }
  }, [taskId]);

  useEffect(() => {
    void loadMemory();
    const timer = window.setInterval(() => {
      if (!document.hidden) void loadMemory();
    }, 5000);
    return () => { window.clearInterval(timer); };
  }, [loadMemory]);

  const startEdit = () => {
    setPrefDraft(memory?.preferences ?? "");
    setNotesDraft(memory?.notes ?? "");
    setEditing(true);
  };

  const save = async () => {
    setBusy(true);
    try {
      const saved = await postCommand<ProjectMemoryRecord>(taskId, "src.project.memory.save", {
        preferences: prefDraft,
        notes: notesDraft,
        mode: "replace",
        idempotency_key: crypto.randomUUID(),
      });
      if (saved && typeof saved === "object" && "preferences" in saved) {
        setMemory(saved);
      } else {
        await loadMemory();
      }
      setEditing(false);
      toast({ title: "已更新项目偏好与笔记", tone: "success" });
    } catch (err) {
      toast({ title: "保存项目记忆失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
    } finally {
      setBusy(false);
    }
  };

  const hasContent = Boolean((memory?.preferences ?? "").trim() || (memory?.notes ?? "").trim());

  return (
    <section className="space-y-2" data-testid="overview-project-memory">
      <SectionHeader
        title="项目偏好与长期笔记"
        action={
          editing ? undefined : (
            <button
              type="button"
              className="text-cx-xs font-medium text-cx-accent hover:underline"
              onClick={startEdit}
            >
              编辑
            </button>
          )
        }
      />
      <SurfaceCard className="p-3">
        {editing ? (
          <div className="space-y-2.5">
            <div>
              <Label className="mb-1 block text-cx-xs text-cx-fg-2">偏好（preferences · 输出与测试习惯）</Label>
              <TextArea
                rows={3}
                value={prefDraft}
                onChange={(e) => setPrefDraft(e.target.value)}
                placeholder="例如：汇报精炼列要点、优先验证越权、不要客套话"
              />
            </div>
            <div>
              <Label className="mb-1 block text-cx-xs text-cx-fg-2">笔记（notes · 项目上下文与增量备忘）</Label>
              <TextArea
                rows={3}
                value={notesDraft}
                onChange={(e) => setNotesDraft(e.target.value)}
                placeholder="例如：环境特征、已确认排除项、关键路由记录"
              />
            </div>
            <div className="flex items-center justify-end gap-2">
              <Button size="xs" variant="ghost" disabled={busy} onClick={() => setEditing(false)}>取消</Button>
              <Button size="xs" variant="primary" disabled={busy} onClick={() => { void save(); }}>
                {busy ? "保存中…" : "保存"}
              </Button>
            </div>
          </div>
        ) : hasContent ? (
          <div className="space-y-2 text-cx-xs">
            {(memory?.preferences ?? "").trim() ? (
              <div>
                <div className="text-cx-2xs font-medium text-cx-fg-3">偏好（preferences）</div>
                <p className="mt-0.5 whitespace-pre-wrap text-cx-fg">{memory?.preferences}</p>
              </div>
            ) : null}
            {(memory?.notes ?? "").trim() ? (
              <div>
                <div className="text-cx-2xs font-medium text-cx-fg-3">笔记（notes）</div>
                <p className="mt-0.5 whitespace-pre-wrap text-cx-fg-2">{memory?.notes}</p>
              </div>
            ) : null}
          </div>
        ) : (
          <p className="text-cx-xs text-cx-fg-3">
            暂无项目偏好或笔记；在对话中说明习惯后会自动沉淀于此，同项目任务共享。
          </p>
        )}
      </SurfaceCard>
    </section>
  );
}

export function TasksWorkspace({ active, sheet = false }: { active: boolean; sheet?: boolean }) {
  const state = useStudioState();
  const ui = state.workspace.tasks;
  const subTab = ui.tab ?? "overview";
  const scroll = useRememberedScroll("tasks:overview", active);
  const projectId = state.workspace.projectId;
  const activeThread = state.threads[state.activeThreadId];
  const highlighted = ui.highlightId ? state.tasks.find((task) => task.id === ui.highlightId) : undefined;
  const activeRuns = state.workRuns.filter((r) => ["running", "paused", "frozen", "blocked", "waiting_approval", "queued", "planning"].includes(r.state));
  const liveSubagents = state.liveSubagents ?? [];
  const runningSubagents = liveSubagents.filter(isLiveRunningSubagent);
  const awaitingSettleSubagents = liveSubagents.filter((sub) => (sub.delivery || "").toLowerCase() === "awaiting_settle");
  const [settlingSubagents, setSettlingSubagents] = useState<Record<string, boolean>>({});
  const [settlingAllSubagents, setSettlingAllSubagents] = useState(false);

  const settleOneSubagent = useCallback(async (sub: NonNullable<StudioState["liveSubagents"]>[number], silent = false): Promise<boolean> => {
    const label = sub.title || sub.thread_id;
    setSettlingSubagents((prev) => ({ ...prev, [sub.thread_id]: true }));
    try {
      await settleControlThread(sub.thread_id);
      if (!silent) toast({ title: `已收尾「${label}」`, tone: "success", duration: 2200 });
      window.dispatchEvent(new Event("muteki:threads-changed"));
      return true;
    } catch (err) {
      if (!silent) {
        toast({ title: `收尾「${label}」失败`, description: controlErrorText(err, "收尾失败"), tone: "danger" });
      }
      return false;
    } finally {
      setSettlingSubagents((prev) => {
        const next = { ...prev };
        delete next[sub.thread_id];
        return next;
      });
    }
  }, []);

  const settleAllAwaitingSubagents = useCallback(async () => {
    if (!awaitingSettleSubagents.length) return;
    setSettlingAllSubagents(true);
    try {
      const results = await Promise.all(awaitingSettleSubagents.map((sub) => settleOneSubagent(sub, true)));
      const settledCount = results.filter(Boolean).length;
      if (settledCount > 0) {
        toast({ title: `已收尾 ${settledCount} 个委派子线程`, tone: "success", duration: 2400 });
        window.dispatchEvent(new Event("muteki:threads-changed"));
      }
      if (settledCount < awaitingSettleSubagents.length) {
        toast({ title: `${awaitingSettleSubagents.length - settledCount} 个子线程收尾失败`, tone: "warning" });
      }
    } finally {
      setSettlingAllSubagents(false);
    }
  }, [awaitingSettleSubagents, settleOneSubagent]);
  const [showDone, setShowDone] = useState(false);

  const tasks = useMemo(
    () => state.tasks.filter((task) => projectId === "all" || task.projectId === projectId || task.id === ui.highlightId),
    [projectId, state.tasks, ui.highlightId],
  );
  const newTaskProject = projectId === "all" ? activeThread?.projectId ?? state.projects[0]?.id ?? "" : projectId;

  const pendingConflicts = useMemo(() => state.sources.flatMap((s) => (s.conflicts ?? []).filter((c) => !c.resolvedBy || c.resolvedBy === "rule").map((c) => ({ source: s, conf: c }))), [state.sources]);
  const pendingFeedback = useMemo(() => state.feedbacks.filter((f) => f.status !== "resolved"), [state.feedbacks]);
  const dupGroups = useMemo(() => findClueDuplicates(state.clues, state.merges, state.dismissedDuplicates, state.undeterminedDuplicates), [state.clues, state.merges, state.dismissedDuplicates, state.undeterminedDuplicates]);
  const gapReports = useMemo(() => state.reports.filter((r) => r.status === "draft" && (r.materialGaps?.length ?? 0) > 0), [state.reports]);

  const srcTaskId = attachedTaskId(state) || (state.hostTag?.startsWith("task:") ? state.hostTag.slice(5) : "");
  const [coordination, setCoordination] = useState<CoordinationStatus | null>(null);
  const [coordinationError, setCoordinationError] = useState("");
  const [lifecycleBusy, setLifecycleBusy] = useState(false);
  const [destroyOpen, setDestroyOpen] = useState(false);

  const loadCoordination = useCallback(async () => {
    if (!srcTaskId) {
      setCoordination(null);
      setCoordinationError("");
      return;
    }
    try {
      const res = await querySrc<CoordinationStatus>(srcTaskId, "src.coordination.status", {});
      setCoordination(res);
      setCoordinationError("");
    } catch (err) {
      setCoordinationError(err instanceof Error ? err.message : String(err));
    }
  }, [srcTaskId]);

  useEffect(() => {
    if (!srcTaskId) {
      setCoordination(null);
      setCoordinationError("");
      return;
    }
    let alive = true;
    let inFlight = false;
    const load = () => {
      if (inFlight) return;
      inFlight = true;
      querySrc<CoordinationStatus>(srcTaskId, "src.coordination.status", {})
        .then((res) => {
          if (alive) {
            setCoordination(res);
            setCoordinationError("");
          }
        })
        .catch((err) => {
          if (alive) setCoordinationError(err instanceof Error ? err.message : String(err));
        })
        .finally(() => {
          inFlight = false;
        });
    };
    load();
    const timer = window.setInterval(() => {
      if (typeof document === "undefined" || !document.hidden) load();
    }, 5000);
    const onVisibility = () => {
      if (!document.hidden) load();
    };
    const onTaskEvent = (event: Event) => {
      const detail = (event as CustomEvent<{ taskId?: string; lifecycleState?: string; source?: string }>).detail;
      if (detail?.taskId !== srcTaskId) return;
      if (detail.source !== "taskboard" && detail.lifecycleState) {
        setCoordination((prev) => ({
          ...prev,
          task_lifecycle: { state: detail.lifecycleState },
        }));
      }
      if (detail.source !== "taskboard") load();
    };
    document.addEventListener("visibilitychange", onVisibility);
    window.addEventListener("muteki:src-task-changed", onTaskEvent);
    return () => {
      alive = false;
      window.clearInterval(timer);
      document.removeEventListener("visibilitychange", onVisibility);
      window.removeEventListener("muteki:src-task-changed", onTaskEvent);
    };
  }, [srcTaskId]);

  const lifecycleState = coordination?.task_lifecycle?.state ?? "active";
  const runLifecycle = async (action: "pause" | "resume" | "archive" | "restore" | "destroy") => {
    if (!srcTaskId) return;
    setLifecycleBusy(true);
    try {
      await lifecycleSrcTask(srcTaskId, action);
      if (action === "resume" || action === "restore") {
        studio.clearEffectError();
      }
      const nextState = action === "pause" ? "paused" : action === "archive" ? "archived" : "active";
      if (action !== "destroy") {
        setCoordination((prev) => ({
          ...prev,
          task_lifecycle: { state: nextState },
        }));
      }
      const msgMap = {
        pause: "已暂停研究",
        resume: "已恢复研究",
        archive: "已归档研究",
        restore: "已恢复归档任务",
        destroy: "已销毁研究任务",
      };
      toast({ title: msgMap[action], tone: "success" });
      if (action === "destroy") {
        setDestroyOpen(false);
        if (studioRouter.current) studioRouter.current.push("/src");
        else window.location.assign("/src");
        return;
      }
      window.dispatchEvent(new CustomEvent("muteki:src-task-changed", {
        detail: { taskId: srcTaskId, lifecycleState: nextState, source: "taskboard" },
      }));
      void studio.reloadDomain("tasks").catch((reloadErr) => {
        toast({
          title: "状态已更新，但工作台列表刷新失败",
          description: reloadErr instanceof Error ? reloadErr.message : String(reloadErr),
          tone: "warning",
        });
      });
    } catch (err) {
      toast({ title: err instanceof Error ? err.message : "生命周期操作失败", tone: "danger" });
    } finally {
      setLifecycleBusy(false);
    }
  };

  useViewing(active && subTab === "overview", highlighted ? { kind: "task", id: highlighted.id } : null);
  useEffect(() => { if (highlighted) studio.markSeen({ kind: "task", id: highlighted.id }); }, [highlighted]);

  if (subTab === "plan") return <div className="flex h-full min-h-0 flex-col" data-testid="studio-tasks-workspace"><DrillIn crumb="研究计划"><PlanDrillIn projectId={projectId} /></DrillIn><SheetAskBar sheet={sheet} target={null} /></div>;
  if (subTab === "runs") return <div className="flex h-full min-h-0 flex-col" data-testid="studio-tasks-workspace"><DrillIn crumb={ui.selectedRunId ? `工作记录/${ui.selectedRunId}` : "工作记录"}><RunDrillIn projectId={projectId} /></DrillIn><SheetAskBar sheet={sheet} target={null} /></div>;
  if (subTab === "monitors") return <div className="flex h-full min-h-0 flex-col" data-testid="studio-tasks-workspace"><DrillIn crumb="变化与监控"><MonitorsDrillIn active={active} /></DrillIn><SheetAskBar sheet={sheet} target={null} /></div>;
  if (subTab === "coverage") return <div className="flex h-full min-h-0 flex-col" data-testid="studio-tasks-workspace"><DrillIn crumb="覆盖率门禁"><CoverageDrillIn taskId={srcTaskId} /></DrillIn><SheetAskBar sheet={sheet} target={null} /></div>;
  if (subTab === "scope" || subTab === "env") return <div className="flex h-full min-h-0 flex-col" data-testid="studio-tasks-workspace"><DrillIn crumb={subTab === "scope" ? "范围与授权" : "环境与连接器"}>{subTab === "scope" ? <ScopeDrillIn /> : <EnvDrillIn />}</DrillIn><SheetAskBar sheet={sheet} target={null} /></div>;

  const plan = state.planVersions[0];
  const unreadNotices = coordination?.unread_notices ?? [];
  const pendingSuggestions: Array<{ key: string; item: StudioSuggestion }> = [];
  for (const [sugKey, list] of Object.entries(state.suggestions ?? {})) {
    for (const item of list ?? []) {
      if (!item.result) {
        pendingSuggestions.push({ key: sugKey, item });
      }
    }
  }
  const blockedOrWaitingTodos = tasks.filter((t) => {
    const exec = deriveTodoExecutionState(t, state.workRuns, liveSubagents);
    return exec === "blocked" || exec === "waiting_reply";
  });
  const kinds: Array<{ icon: IconName; items: Array<{ title: string; reason: string; go: () => void }>; many: { title: string; go: () => void } }> = [
    {
      icon: "bell",
      items: unreadNotices.map((n) => ({
        title: n.action || "协调通知",
        reason: n.rationale ?? n.summary ?? "",
        go: () => {
          if (srcTaskId && n.notice_id) {
            void postCommand(srcTaskId, "src.coordination.notice.ack", { notice_id: n.notice_id, idempotency_key: crypto.randomUUID() })
              .then(() => setCoordination((prev) => ({
                ...prev,
                unread_notices: (prev?.unread_notices ?? []).filter((x) => x.notice_id !== n.notice_id),
              })))
              .catch(() => {});
          }
          if (n.target_run_id) studio.setTasksUi({ tab: "runs", selectedRunId: n.target_run_id });
          else if (n.report_id) openRef({ kind: "report", id: n.report_id });
          else if (n.finding_id) openWorkspace("findings");
        },
      })),
      many: { title: `${unreadNotices.length} 条协调通知待查看`, go: () => studio.setTasksUi({ tab: "runs" }) },
    },
    {
      icon: "gitCompare",
      items: pendingConflicts.map(({ source, conf }) => ({ title: `${conf.fieldLabel} · ${displayTitle(source.title).slice(0, 24)}`, reason: "字段冲突待裁决", go: () => openRef({ kind: "source", id: source.id }) })),
      many: { title: `${pendingConflicts.length} 个资产有字段冲突`, go: () => { studio.setLibraryUi({ conflict: "unresolved" }); openWorkspace("library"); } },
    },
    {
      icon: "alert",
      items: state.workRuns.filter((r) => r.state === "blocked").map((r) => ({ title: displayTitle(r.title), reason: `受阻：${r.blockerReason ?? "等待处理"}`, go: () => studio.setTasksUi({ tab: "runs", selectedRunId: r.id }) })),
      many: { title: `${state.workRuns.filter((r) => r.state === "blocked").length} 条工作记录受阻`, go: () => studio.setTasksUi({ tab: "runs" }) },
    },
    {
      icon: "messageCircle",
      items: pendingFeedback.map((f) => ({ title: `${f.externalTicketId || f.id}`, reason: `平台反馈待处理 · ${f.rawText.slice(0, 30)}`, go: () => openRef({ kind: "feedback", id: f.id }) })),
      many: { title: `${pendingFeedback.length} 条平台反馈待处理`, go: () => { studio.setReportsUi({ tab: "feedbacks", view: "report" }); openWorkspace("reports"); } },
    },
    {
      icon: "file",
      items: gapReports.map((r) => ({ title: displayTitle(r.title ?? r.id), reason: `报告草稿缺 ${r.materialGaps?.length ?? 0} 项材料`, go: () => openRef({ kind: "report", id: r.id }) })),
      many: { title: `${gapReports.length} 份报告草稿缺材料`, go: () => openWorkspace("reports") },
    },
    {
      icon: "copy",
      items: dupGroups.map((g) => ({ title: `重复候选（${g.ids.length} 条）`, reason: "待审阅", go: () => { studio.setFindingsUi({ view: "duplicates" }); openWorkspace("findings"); } })),
      many: { title: `${dupGroups.length} 组重复候选待审阅`, go: () => { studio.setFindingsUi({ view: "duplicates" }); openWorkspace("findings"); } },
    },
  ];
  const inbox: Array<{ icon: IconName; title: string; reason: string; go: () => void }> = kinds
    .filter((kind) => kind.items.length)
    .map((kind) => kind.items.length > 2 ? { icon: kind.icon, title: kind.many.title, reason: kind.items[0]?.reason ?? "", go: kind.many.go } : null)
    .flatMap((agg, i) => {
      const kind = kinds.filter((k) => k.items.length)[i];
      return agg ? [agg] : kind.items.map((item) => ({ icon: kind.icon, ...item }));
    });
  const visibleTasks = showDone ? tasks : tasks.filter((t) => t.status !== "done");
  const doneCount = tasks.filter((t) => t.status === "done").length;
  const monitors = state.monitors ?? [];
  const latestEvidences = state.evidences.slice(0, 4);
  const latestClues = state.clues.filter((c) => !mergeOf(state.merges, "clue", c.id)).slice(0, 4);
  const tasksDomainError = state.domainErrors?.tasks;
  const pendingDecisionCount = pendingSuggestions.length + blockedOrWaitingTodos.length + inbox.length;

  return (
    <div className="flex h-full min-h-0 flex-col" data-testid="studio-tasks-workspace">
      <div ref={scroll.ref} className="cx-scroll min-h-0 flex-1 overflow-y-auto">
        <div className="mx-auto flex w-full max-w-[760px] flex-col gap-4 p-4 pb-10">
          {tasksDomainError ? (
            <Callout tone="danger" action={<Button size="xs" variant="secondary" onClick={() => { void studio.reloadDomain("tasks"); }}>重试加载</Button>}>
              任务与执行域加载异常：{tasksDomainError}
            </Callout>
          ) : null}

          <SurfaceCard className="p-3">
            <div className="flex flex-wrap items-center gap-2">
              <div className="min-w-0 flex-1">
                <p className="truncate text-cx-md font-semibold text-cx-fg">{displayTitle(state.projects.find((p) => p.id === (projectId === "all" ? state.projects[0]?.id : projectId))?.title ?? "SRC 工作台")}</p>
              </div>
              <StudioBadge tone={lifecycleState === "paused" ? "warning" : lifecycleState === "archived" ? "neutral" : "accent"}>
                {lifecycleState === "paused" ? "已暂停" : lifecycleState === "archived" ? "已归档" : "进行中"}
              </StudioBadge>
              {srcTaskId ? (
                <div className="flex flex-wrap items-center gap-1.5">
                  {lifecycleState !== "archived" ? (
                    <Button size="sm" variant="secondary" disabled={lifecycleBusy} onClick={() => { void runLifecycle(lifecycleState === "paused" ? "resume" : "pause"); }}>
                      {lifecycleState === "paused" ? "恢复研究" : "暂停研究"}
                    </Button>
                  ) : null}
                  <Menu
                    ariaLabel="任务生命周期操作"
                    trigger={<Button size="sm" variant="ghost" disabled={lifecycleBusy}>生命周期</Button>}
                  >
                    {lifecycleState === "archived" ? (
                      <MenuItem icon="refresh" onSelect={() => { void runLifecycle("restore"); }}>恢复归档研究</MenuItem>
                    ) : (
                      <MenuItem icon="archive" onSelect={() => { void runLifecycle("archive"); }}>归档研究</MenuItem>
                    )}
                    <MenuItem icon="trash" danger onSelect={() => setDestroyOpen(true)}>销毁研究…</MenuItem>
                  </Menu>
                </div>
              ) : null}
            </div>
            <Dl className="mt-3" items={[
              { label: "当前判断", value: plan?.currentJudgement ?? "—" },
              { label: "计划", value: plan ? `第 ${plan.version ?? ""} 版 · ${(plan.materialGaps ?? []).length} 个缺口` : "—" },
              { label: "监控", value: `${monitors.length} 个监控 · ${state.changes.length} 条变化` },
            ]} />
            <div className="mt-3 flex flex-wrap gap-1.5 border-t border-cx-border-subtle pt-2.5">
              <Button size="xs" variant="secondary" onClick={() => studio.setTasksUi({ tab: "plan" })}>研究计划</Button>
              <Button size="xs" variant="secondary" onClick={() => studio.setTasksUi({ tab: "runs" })}>工作记录 ({state.workRuns.length})</Button>
              <Button size="xs" variant="secondary" onClick={() => studio.setTasksUi({ tab: "monitors" })}>变化与监控 ({monitors.length}/{state.changes.length})</Button>
              <Button size="xs" variant="secondary" onClick={() => studio.setTasksUi({ tab: "scope" })}>范围与授权</Button>
              <Button size="xs" variant="secondary" onClick={() => studio.setTasksUi({ tab: "coverage" })}>覆盖率门禁</Button>
              <Button size="xs" variant="secondary" onClick={() => studio.setTasksUi({ tab: "env" })}>环境与连接器</Button>
            </div>
          </SurfaceCard>

          {srcTaskId ? <ProjectMemoryCard taskId={srcTaskId} /> : null}

          <section className="space-y-3" data-testid="overview-active-work">
            <SectionHeader
              title="活跃工作"
              count={activeRuns.length + runningSubagents.length}
              action={<button type="button" className="text-cx-xs font-medium text-cx-accent hover:underline" onClick={() => studio.setTasksUi({ tab: "runs" })}>全部工作记录</button>}
            />
            <SurfaceCard>
              {activeRuns.map((run) => (
                <SurfaceRow
                  key={run.id}
                  icon="terminal"
                  tone={run.state === "running" ? "running" : "default"}
                  title={displayTitle(run.title)}
                  subtitle={run.state === "blocked" ? `受阻：${run.blockerReason ?? ""}` : `后台 Run · ${RUN_STATE_LABEL[run.state]} · ${run.targetUrl || run.id}`}
                  trailing={(
                    <span className="flex shrink-0 items-center gap-1.5">
                      <StudioBadge tone="neutral">后台 Worker Run</StudioBadge>
                      <StudioBadge tone={RUN_STATE_TONE[run.state]}>{RUN_STATE_LABEL[run.state]}</StudioBadge>
                    </span>
                  )}
                  onClick={() => studio.setTasksUi({ tab: "runs", selectedRunId: run.id })}
                />
              ))}
              {runningSubagents.map((sub) => (
                <SurfaceRow
                  key={`sub-${sub.thread_id}`}
                  icon="bot"
                  tone="running"
                  title={sub.title || sub.thread_id}
                  subtitle={[
                    `子线程 · ${sub.status || "running"}`,
                    [sub.adapter_id, sub.model].filter(Boolean).join(" / "),
                    formatSubagentRoutingLabel(sub),
                    sub.delegation_profile === "scoped_execution" ? "有界执行" : sub.delegation_profile === "readonly_analysis" ? "只读分析" : "",
                    sub.todo_id ? `关联待办 ${sub.todo_id}` : "",
                    sub.updated_at || "",
                  ].filter(Boolean).join(" · ")}
                  trailing={(
                    <span className="flex shrink-0 items-center gap-2">
                      <StudioBadge tone="running">委派子线程</StudioBadge>
                      <a
                        href={`/chat/${encodeURIComponent(sub.thread_id)}`}
                        target="_blank"
                        rel="noreferrer"
                        className="text-cx-xs font-medium text-cx-accent hover:underline"
                        onClick={(e) => e.stopPropagation()}
                      >
                        查看对话
                      </a>
                    </span>
                  )}
                  onClick={() => {
                    if (sub.todo_id) studio.setTasksUi({ highlightId: sub.todo_id });
                  }}
                />
              ))}
              {!activeRuns.length && !runningSubagents.length ? (
                <SurfaceRow icon="checkCheck" title="当前没有运行或冻结中的 Run / 子线程" />
              ) : null}
            </SurfaceCard>
            {awaitingSettleSubagents.length > 0 ? (
              <div className="space-y-2" data-testid="overview-awaiting-settle-subagents">
                <SectionHeader
                  title="待收尾委派子线程"
                  count={awaitingSettleSubagents.length}
                  action={(
                    <Button
                      size="xs"
                      variant="secondary"
                      disabled={settlingAllSubagents}
                      onClick={() => { void settleAllAwaitingSubagents(); }}
                    >
                      {settlingAllSubagents ? "收尾中…" : "全部收尾"}
                    </Button>
                  )}
                />
                <SurfaceCard>
                  {awaitingSettleSubagents.map((sub) => {
                    const st = (sub.status || "").toLowerCase();
                    const badgeTone = st === "failed" || st === "error"
                      ? "danger"
                      : st === "completed"
                      ? "success"
                      : "warning";
                    const busy = Boolean(settlingSubagents[sub.thread_id]) || settlingAllSubagents;
                    return (
                      <SurfaceRow
                        key={`awaiting-sub-${sub.thread_id}`}
                        icon="bot"
                        tone="default"
                        title={sub.title || sub.thread_id}
                        subtitle={[
                          `子线程 · ${sub.status || "finished"}`,
                          [sub.adapter_id, sub.model].filter(Boolean).join(" / "),
                          formatSubagentRoutingLabel(sub),
                          sub.delegation_profile === "scoped_execution" ? "有界执行" : sub.delegation_profile === "readonly_analysis" ? "只读分析" : "",
                          sub.todo_id ? `关联待办 ${sub.todo_id}` : "",
                          sub.completed_at ? `结束于 ${sub.completed_at}` : (sub.updated_at || ""),
                        ].filter(Boolean).join(" · ")}
                        trailing={(
                          <span className="flex shrink-0 items-center gap-2">
                            <StudioBadge tone={badgeTone}>
                              {st === "failed" ? "失败待收尾" : "待收尾"}
                            </StudioBadge>
                            <Button
                              size="xs"
                              variant="secondary"
                              disabled={busy}
                              onClick={(e) => {
                                e.stopPropagation();
                                void settleOneSubagent(sub);
                              }}
                            >
                              {busy ? "收尾中" : "收尾"}
                            </Button>
                            <a
                              href={`/chat/${encodeURIComponent(sub.thread_id)}`}
                              target="_blank"
                              rel="noreferrer"
                              className="text-cx-xs font-medium text-cx-accent hover:underline"
                              onClick={(e) => e.stopPropagation()}
                            >
                              查看对话
                            </a>
                          </span>
                        )}
                        onClick={() => {
                          if (sub.todo_id) studio.setTasksUi({ highlightId: sub.todo_id });
                        }}
                      />
                    );
                  })}
                </SurfaceCard>
              </div>
            ) : null}
            {srcTaskId ? (
              <SuperAgentCoordinationCard
                taskId={srcTaskId}
                status={coordination}
                error={coordinationError}
                onRetry={() => { void loadCoordination(); }}
                onStatusChange={setCoordination}
              />
            ) : null}
          </section>

          <section className="space-y-3" data-testid="overview-pending-decisions">
            <SectionHeader title="待决定事项" count={pendingDecisionCount} />

            {pendingSuggestions.length > 0 ? (
              <SurfaceCard>
                {pendingSuggestions.map(({ key: sugKey, item: sug }) => (
                  <SurfaceRow
                    key={sug.id}
                    icon="sparkles"
                    title={sug.title || sug.quote || sug.id}
                    subtitle={[SUGGESTION_KIND_LABELS[sug.kind] ?? sug.kind, sug.rationale].filter(Boolean).join(" · ")}
                    trailing={(
                      <span className="flex shrink-0 items-center gap-1.5">
                        <Button
                          size="xs"
                          variant="ghost"
                          onClick={(e) => {
                            e.stopPropagation();
                            studio.dismissSuggestion(sug.id);
                            if (hasAttachedDomains()) void afterDomainMutation();
                          }}
                        >
                          忽略
                        </Button>
                        <Button
                          size="xs"
                          variant="primary"
                          onClick={(e) => {
                            e.stopPropagation();
                            studio.adoptSuggestions(sugKey, [sug.id], {
                              threadId: state.activeThreadId,
                              assignee: srcTaskId ? "local-user" : (STUDIO_TEAM[0] ?? ""),
                            });
                            if (hasAttachedDomains()) {
                              void afterDomainMutation().then((res) => { if (res.ok) toast({ title: "已采纳建议", tone: "success" }); });
                            } else {
                              toast({ title: "已采纳建议", tone: "success" });
                            }
                          }}
                        >
                          采纳
                        </Button>
                      </span>
                    )}
                  />
                ))}
              </SurfaceCard>
            ) : null}

            {blockedOrWaitingTodos.length > 0 ? (
              <SurfaceCard>
                {blockedOrWaitingTodos.map((t) => (
                  <SurfaceRow
                    key={`bw-${t.id}`}
                    icon="alert"
                    title={t.title}
                    subtitle={t.blockerReason ? `阻塞：${t.blockerReason}` : `等待回复：${t.waitingFor ?? "外部确认"}`}
                    trailing={<StudioBadge tone={t.blockerReason ? "danger" : "warning"}>{t.blockerReason ? "受阻" : "等待回复"}</StudioBadge>}
                    onClick={() => studio.setTasksUi({ highlightId: t.id })}
                  />
                ))}
              </SurfaceCard>
            ) : null}

            {(inbox.length > 0 || pendingDecisionCount === 0) ? (
              <SurfaceCard>
                {inbox.length ? inbox.map((item, i) => (
                  <SurfaceRow key={i} icon={item.icon} title={item.title} subtitle={item.reason} onClick={item.go} />
                )) : <SurfaceRow icon="check" title="暂无待决定事项" />}
              </SurfaceCard>
            ) : null}
          </section>

          <section className="space-y-3" data-testid="overview-evidence-and-todos">
            <SectionHeader
              title="最新证据与发现"
              count={latestEvidences.length + latestClues.length}
              action={<button type="button" className="text-cx-xs font-medium text-cx-accent hover:underline" onClick={() => openWorkspace("findings")}>进入发现与证据</button>}
            />
            <SurfaceCard>
              {latestClues.map((clue) => (
                <SurfaceRow
                  key={`clue-${clue.id}`}
                  icon="bookmark"
                  title={displayTitle(clue.title)}
                  subtitle={`发现 · ${clue.severity ?? ""} · ${clue.status}`}
                  onClick={() => openRef({ kind: "clue", id: clue.id })}
                />
              ))}
              {latestEvidences.map((ev) => (
                <SurfaceRow
                  key={`ev-${ev.id}`}
                  icon="file"
                  title={ev.title}
                  subtitle={`${EVIDENCE_KIND_LABELS[ev.kind] ?? ev.kind} · ${ev.capturedAt || ""}`}
                  onClick={() => openRef({ kind: "evidence", id: ev.id })}
                />
              ))}
              {!latestClues.length && !latestEvidences.length ? (
                <SurfaceRow icon="search" title="暂无最新证据或发现" onClick={() => openWorkspace("findings")} />
              ) : null}
            </SurfaceCard>

            <SectionHeader
              title="待办与研究笔记"
              count={visibleTasks.length}
              action={doneCount ? <button type="button" className="text-cx-xs text-cx-fg-3 hover:text-cx-fg" onClick={() => setShowDone((v) => !v)}>{showDone ? "隐藏已完成" : `显示已完成 (${doneCount})`}</button> : undefined}
            />
            <SurfaceCard>
              {visibleTasks.map((task) => <TaskRow key={task.id} task={task} srcTaskId={srcTaskId} />)}
              {!visibleTasks.length ? <SurfaceRow icon="checkCheck" title="暂无待办任务" /> : null}
              <NewTaskInline projectId={newTaskProject} />
            </SurfaceCard>
          </section>
        </div>
      </div>

      <Dialog
        open={destroyOpen}
        onOpenChange={setDestroyOpen}
        title="销毁研究任务"
        tone="danger"
        size="sm"
        dismissable={false}
        description="确认永久销毁当前研究任务？该操作不可撤销。"
        footer={
          <div className="flex items-center justify-end gap-2">
            <Button size="sm" variant="ghost" disabled={lifecycleBusy} onClick={() => setDestroyOpen(false)}>取消</Button>
            <Button size="sm" variant="danger" disabled={lifecycleBusy} onClick={() => { void runLifecycle("destroy"); }}>
              {lifecycleBusy ? "销毁中…" : "确认销毁"}
            </Button>
          </div>
        }
      >
        <Callout tone="danger">销毁后将不可再恢复，如仅需只读保留请选择“归档研究”。</Callout>
      </Dialog>

      <SheetAskBar sheet={sheet} target={highlighted ? { kind: "task", id: highlighted.id } : null} prefill={highlighted ? `关于任务「${highlighted.title}」：` : undefined} />
    </div>
  );
}

function NewTaskInline({ projectId }: { projectId: string }) {
  const [title, setTitle] = useState("");
  const submit = () => {
    if (!title.trim() || !projectId) return;
    const threadId = readStudioState().activeThreadId;
    const links = (readStudioState().context[threadId] ?? []).filter((ref) => ref.kind !== "task");
    const assignee = hasAttachedDomains() ? "" : (STUDIO_TEAM[0] ?? "");
    const id = studio.addTask({ projectId, title: title.trim(), assignee, links, threadId });
    setTitle("");
    studio.setTasksUi({ highlightId: id });
    if (hasAttachedDomains()) {
      void afterDomainMutation().then((res) => { if (res.ok) toast({ title: "已新建任务", tone: "success" }); });
    } else {
      toast({ title: "已新建任务", tone: "success" });
    }
  };
  return (
    <div className="px-2 py-1">
      <Input
        size="sm"
        icon="plus"
        value={title}
        onChange={(e) => setTitle(e.target.value)}
        onKeyDown={(e) => { if (e.key === "Enter" && !e.nativeEvent.isComposing) submit(); }}
        placeholder="添加任务，Enter 确认"
        aria-label="新任务标题"
      />
    </div>
  );
}
