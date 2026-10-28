"use client";
import { Icon, type IconName } from "@/components/Icon";


import { useEffect, useMemo, useState } from "react";

import {
  Button,
  Callout,
  Dialog,
  EmptyState,
  Checkbox,
  IconButton,
  Collapse,
  Input,
  Menu,
  MenuItem,
  SegmentedControl,
  Select,
  TextArea,
  TextField,
  toast,
} from "@/components/chat/ui";
import { STUDIO_TEAM } from "./demoData";
import {
  type StudioPlanQuestion,
  type StudioPlanVersion,
  type StudioRef,
  type StudioTask,
} from "./model";
import { isSheetViewport, openRef, openWorkspace, revealConversation, revealTurn } from "./nav";
import {
  FIELD,
  MENU_HIT,
  OPTION_HIT,
  SheetAskBar,
  StudioBadge,
  displayTitle,
  dragRef,
  useRememberedScroll,
  useViewing,
  REF_KIND_LABEL,
} from "./parts";
import { findClueDuplicates } from "./records";
import { refLabel } from "./store";
import { approvalTitle, RUN_STATE_LABEL, RUN_STATE_TONE } from "./records";
import type { StudioApproval } from "./model";
import { SectionHeader, SurfaceToolbar } from "@/components/chat/surfaces/shared";
import { SurfaceCard, SurfaceRow } from "@/components/chat/surfaces/OverviewSurface";
import { PathBreadcrumb } from "@/components/chat/surfaces/PathBreadcrumb";
import { Dl, sendAbout } from "./Kit";
import { TodoList, type TodoItem } from "@/components/agentui/agents/todo-list";
import { ApprovalCard } from "@/components/agentui/agents/approval-card";
import { StatusDot, CodeBlock } from "@/components/chat/ui";
import { readStudioState, studio, useStudioState } from "./store";




function handoffInstruction(task: StudioTask): string {
  return `请推进任务「${task.title}」：先列出关联材料里还缺的证据，再给出下一步。`;
}

export function handTaskToAi(task: StudioTask) {
  const threadId = readStudioState().activeThreadId;
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
}




// ---------------------------------------------------------------------------
// Module 1: Long-Term Task Dossier & Plan Versions
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Minimal drill-in views for the 概览 workspace.
// ---------------------------------------------------------------------------

function PlanDrillIn({ projectId }: { projectId: string }) {
  const state = useStudioState();
  const ui = state.workspace.tasks;
  const plans = state.planVersions;
  const plan = plans.find((p) => p.id === ui.selectedPlanId) ?? plans[0];
  const questions = state.planQuestions;
  const materials = state.scopeMaterials;
  const [reviseId, setReviseId] = useState("");
  const [publishOpen, setPublishOpen] = useState(false);
  const [focusQ, setFocusQ] = useState("");
  const [judgement, setJudgement] = useState("");
  const [gapsText, setGapsText] = useState("");
  const [actionsText, setActionsText] = useState("");

  if (!plan) return <EmptyState compact icon="compass" title="还没有研究计划" />;
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
    toast({ title: `已发布计划版本 ${id}`, tone: "success" });
  };

  return (
    <div className="mx-auto flex w-full max-w-[760px] flex-col gap-4 p-4 pb-10" data-testid="studio-plan-drill">
      <SurfaceCard className="p-3">
        <div className="flex items-start gap-3">
          <div className="min-w-0 flex-1"><p className="text-[15px] font-semibold text-cx-fg">{plan.focusQuestion || plan.goal}</p></div>
          <StudioBadge tone="accent">第 {plan.version} 版</StudioBadge>
        </div>
        <Dl className="mt-3" items={[
          { label: "版本", value: <Select<string> value={plan.id} onChange={(selectedPlanId) => studio.setTasksUi({ selectedPlanId })} size="sm" options={plans.map((p) => ({ value: p.id, label: `第 ${p.version} 版 · ${p.createdAt}` }))} ariaLabel="切换计划版本" className="h-7 min-w-[160px] text-[13px]" popoverClassName={OPTION_HIT} /> },
          { label: "目标", value: plan.goal },
        ]} />
        <div className="mt-3 flex items-center gap-2">
          <Button size="sm" variant="primary" onClick={() => sendAbout(planRef, `请结合最新监控变化与复核结果，为计划 ${plan.id} 起草第 ${plan.version + 1} 版修订。`)}>修订计划</Button>
        </div>
      </SurfaceCard>

      <div>
        <SectionHeader title="当前判断" />
        <p className="px-1 text-[13px] leading-6 text-cx-fg">{plan.currentJudgement}</p>
      </div>

      <div>
        <SectionHeader title="材料缺口" count={gapItems.length} action={gapItems.length ? <button type="button" className="text-[12px] font-medium text-cx-accent hover:underline" onClick={() => sendAbout(planRef, `计划 ${plan.id} 有 ${gapItems.length} 项材料缺口，请帮我补齐。`)}>让 Agent 补齐</button> : undefined} />
        {gapItems.length ? <TodoList items={gapItems} title="材料缺口" /> : <SurfaceCard><SurfaceRow icon="check" title="没有缺口" /></SurfaceCard>}
      </div>

      <div>
        <SectionHeader title="研究问题" count={questions.length} />
        <SurfaceCard>
          {questions.map((q) => (
            <SurfaceRow
              key={q.id}
              icon="help"
              title={q.question}
              subtitle={q.rationale}
              trailing={(
                <Menu ariaLabel={`问题状态`} className={MENU_HIT} trigger={<button type="button" className="shrink-0 text-[12px] text-cx-fg-3 hover:text-cx-fg">{statusText[q.status]}</button>}>
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
          <TextField label="聚焦问题" value={focusQ || plan.focusQuestion} onChange={(e) => setFocusQ(e.target.value)} className={FIELD} />
          <TextArea label="当前判断" value={judgement || plan.currentJudgement} onChange={(e) => setJudgement(e.target.value)} className="min-h-[80px] text-[13px]" />
          <TextArea label="材料缺口（每行一项）" value={gapsText || (plan.materialGaps ?? []).join("\n")} onChange={(e) => setGapsText(e.target.value)} className="min-h-[70px] text-[13px]" />
          <TextArea label="建议动作（每行一项）" value={actionsText || (plan.proposedActions ?? []).join("\n")} onChange={(e) => setActionsText(e.target.value)} className="min-h-[70px] text-[13px]" />
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
        <table className="w-full border-collapse text-left text-[13px]">
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
          <div className="min-w-0 flex-1"><p className="text-[15px] font-semibold text-cx-fg">{displayTitle(run.title)}</p></div>
          <StudioBadge tone={RUN_STATE_TONE[run.state]}>{RUN_STATE_LABEL[run.state]}</StudioBadge>
        </div>
        <Dl className="mt-3" items={[
          { label: "编号", value: run.id, mono: true },
          { label: "引擎", value: ENGINE_LABEL[run.engine] ?? run.engine },
          { label: "目标", value: run.targetUrl, mono: true },
          { label: "更新", value: `${run.updatedAt} · 第 ${run.generation} 代` },
        ]} />
        <div className="mt-3 flex items-center gap-2">
          {isDemoHost(state) ? (
            <Button size="sm" variant="primary" onClick={flowback}>回流到对话</Button>
          ) : (
            <span className="text-[12px] text-cx-fg-4">真实运行——进展以运行线程与日志为准，工作台不做模拟回流</span>
          )}
        </div>
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

function ChangesDrillIn({ active }: { active: boolean }) {
  const state = useStudioState();
  const [diffMode, setDiffMode] = useState<"denoised" | "raw">("denoised");
  const [openId, setOpenId] = useState("");
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
          <div className="mx-auto flex w-full max-w-[760px] flex-col gap-4 p-4 pb-10">
            <SurfaceCard className="p-3">
              <div className="flex items-start gap-3">
                <div className="min-w-0 flex-1"><p className="text-[15px] font-semibold text-cx-fg">{displayTitle(open.title)}</p></div>
                <StudioBadge tone={open.comparable ? "success" : "warning"}>{open.comparable ? "可比" : "不可比"}</StudioBadge>
              </div>
              <Dl className="mt-3" items={[
                { label: "编号", value: open.id, mono: true },
                { label: "版本", value: `${open.beforeVersion} → ${open.afterVersion}` },
                { label: "观察", value: open.observedAt },
              ]} />
            </SurfaceCard>
            {!open.comparable && open.incomparableReason ? <p className="px-1 text-[12px] text-cx-fg-4">{open.incomparableReason}</p> : null}
            <div>
              <SectionHeader title="差异" action={<SegmentedControl<"denoised" | "raw"> value={diffMode} onChange={setDiffMode} size="sm" ariaLabel="差异模式" options={[{ value: "denoised", label: "降噪" }, { value: "raw", label: "原始" }]} />} />
              <CodeBlock code={diffMode === "denoised" ? (open.denoisedDiff || open.rawDiff) : open.rawDiff} className="max-h-[320px]" />
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
    <div className="mx-auto flex w-full max-w-[760px] flex-col gap-4 p-4 pb-10" data-testid="studio-changes-list">
      <div>
        <SectionHeader title="变化记录" count={state.changes.length} />
        <SurfaceCard>
          {state.changes.map((chg) => (
            <SurfaceRow key={chg.id} icon="activity" title={displayTitle(chg.title)} subtitle={`${chg.observedAt} · ${chg.comparable ? "可比" : "不可比"}`} trailing={chg.comparable ? undefined : <span className="shrink-0 text-[12px] text-cx-warning">需复核</span>} onClick={() => setOpenId(chg.id)} />
          ))}
          {!state.changes.length ? <SurfaceRow icon="checkCheck" title="暂无变化" /> : null}
        </SurfaceCard>
      </div>
    </div>
  );
}

function MonitorsDrillIn({ active }: { active: boolean }) {
  const state = useStudioState();
  const monitors = state.monitors ?? [];
  void active;
  return (
    <div className="mx-auto flex w-full max-w-[760px] flex-col gap-4 p-4 pb-10" data-testid="studio-monitors-drill">
      {monitors.map((m) => (
        <div key={m.id}>
          <SectionHeader title={m.name} action={<Menu ariaLabel={`监控 ${m.name}`} className={MENU_HIT} trigger={<IconButton size="sm" icon="more" label="更多" />}><MenuItem icon="play" onSelect={() => studio.runMonitorCycle(m.id)}>运行一轮</MenuItem></Menu>} />
          <SurfaceCard className="p-3">
            <Dl items={[
              { label: "查询", value: `${m.queryExpression} · ${m.queryVersion}`, mono: true },
              { label: "频率", value: m.frequencyLabel },
              { label: "预算", value: `${m.budgetUsed}/${m.budgetTotal}（${Math.round((m.budgetUsed / Math.max(1, m.budgetTotal)) * 100)}%）` },
            ]} />
            <div className="mt-3">
              <Button size="sm" variant="primary" onClick={() => studio.toggleMonitorStatus(m.id)}>{m.status === "active" ? "暂停" : "恢复"}</Button>
            </div>
            {(m.cycles ?? []).length ? (
              <div className="mt-3 flex flex-col divide-y divide-cx-border-subtle">
                {m.cycles.map((c) => (
                  <div key={c.id} className="flex items-center gap-2 py-1.5 text-[13px]">
                    <StatusDot tone={c.status === "completed" ? "success" : c.status === "partial" ? "warning" : "danger"} />
                    <span className="min-w-0 flex-1 truncate text-cx-fg">{c.summary}</span>
                    <span className="shrink-0 text-[12px] text-cx-fg-4">{c.finishedAt}</span>
                  </div>
                ))}
              </div>
            ) : null}
          </SurfaceCard>
        </div>
      ))}
      {!monitors.length ? <EmptyState compact icon="bell" title="暂无监控" /> : null}
      {state.changes.length ? <ChangesDrillIn active={active} /> : null}
    </div>
  );
}

function PendingApprovalCard({ approval: a }: { approval: StudioApproval }) {
  const [rejecting, setRejecting] = useState(false);
  const [reason, setReason] = useState("");
  const [paramsOpen, setParamsOpen] = useState(false);
  const t = approvalTitle(a);
  const confirmReject = () => {
    if (!reason.trim()) return;
    studio.decideApproval(a.id, "deny", reason.trim());
    setRejecting(false);
    setReason("");
  };
  return (
    <ApprovalCard
      title={t.title}
      description={`${a.identity} · ${a.target}`}
      status="pending"
      approveLabel="允许一次"
      rejectLabel="拒绝"
      onApprove={() => studio.decideApproval(a.id, "approve_once")}
      onReject={() => setRejecting((v) => !v)}
    >
      <div className="flex flex-col gap-2">
        {a.matchedRuleReason ? <p className="text-[12px] text-cx-fg-3">{a.matchedRuleReason}</p> : null}
        {rejecting ? (
          <div className="flex items-center gap-2">
            <Input
              size="sm"
              value={reason}
              onChange={(e) => setReason(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && !e.nativeEvent.isComposing) confirmReject();
                if (e.key === "Escape") { setRejecting(false); setReason(""); }
              }}
              placeholder="拒绝原因"
              aria-label="拒绝原因"
              autoFocus
            />
            <Button size="sm" variant="ghost" disabled={!reason.trim()} onClick={confirmReject}>确认拒绝</Button>
          </div>
        ) : (
          <div className="flex items-center gap-2">
            <Button size="sm" variant="ghost" onClick={() => studio.decideApproval(a.id, "approve_grant")}>保存为授权</Button>
            {a.argumentsPreview ? (
              <button type="button" className="text-[12px] font-medium text-cx-fg-3 hover:text-cx-fg" onClick={() => setParamsOpen((v) => !v)}>{paramsOpen ? "收起参数" : "查看参数"}</button>
            ) : null}
          </div>
        )}
        {a.argumentsPreview ? (
          <Collapse open={paramsOpen}><CodeBlock code={a.argumentsPreview} className="max-h-[200px]" /></Collapse>
        ) : null}
      </div>
    </ApprovalCard>
  );
}

function ScopeDrillIn() {
  const state = useStudioState();
  const pending = state.approvals.filter((a) => a.status === "pending");
  const snapshot = state.scopeSnapshots[0];
  const grants = state.grants ?? [];

  return (
    <div className="mx-auto flex w-full max-w-[760px] flex-col gap-4 p-4 pb-10" data-testid="studio-scope-drill">
      <div>
        <SectionHeader title="待确认" count={pending.length} />
        <div className="flex flex-col gap-3">
          {pending.map((a) => <PendingApprovalCard key={a.id} approval={a} />)}
          {!pending.length ? <SurfaceCard><SurfaceRow icon="check" title="没有待确认的授权" /></SurfaceCard> : null}
        </div>
      </div>

      <div>
        <SectionHeader title="已授权" count={grants.length} />
        <SurfaceCard>
          {grants.map((g) => (
            <SurfaceRow key={g.id} icon="shield" title={g.resourceSelector} subtitle={`${(g.actions ?? []).join("、")} · ${g.identityConstraint ?? ""}`} trailing={<Menu ariaLabel="授权操作" className={MENU_HIT} trigger={<IconButton size="sm" icon="more" label="更多" />}><MenuItem icon="x" danger onSelect={() => studio.revokeGrant(g.id)}>撤销</MenuItem></Menu>} />
          ))}
          {!grants.length ? <SurfaceRow icon="shield" title="暂无授权" /> : null}
        </SurfaceCard>
      </div>

      {snapshot ? (
        <div>
          <SectionHeader title="范围快照" />
          <SurfaceCard className="p-3">
            <Dl items={[
              { label: "版本", value: `v${snapshot.version} · ${snapshot.createdAt}` },
              { label: "允许", value: (snapshot.allowedTargets ?? []).join("、") || "—" },
              { label: "排除", value: (snapshot.excludedTargets ?? []).join("、") || "—" },
              { label: "条件", value: (snapshot.conditions ?? []).join("、") || "—" },
            ]} />
          </SurfaceCard>
        </div>
      ) : null}
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
          {env.channels.map((ch) => <SurfaceRow key={ch.id} icon="network" title={ch.name} subtitle={`${ch.status === "enforced" ? "强制受控" : ch.status === "declared_only" ? "仅声明" : "记录"} · ${ch.limitation}`} />)}
        </SurfaceCard>
      </div>
    </div>
  );
}

/** Drill-in chrome: back + breadcrumb above a reused sub-view. */
function InspectorLinks() {
  const state = readStudioState();
  const taskId = state.hostTag?.startsWith("task:") ? state.hostTag.slice(5) : "";
  if (!taskId) return <EmptyState compact icon="link" title="真实任务才有检查器入口" />;
  const base = `/src/tasks/${encodeURIComponent(taskId)}`;
  const items = [
    { href: `${base}/policy`, icon: "shield" as const, title: "权限与审批", subtitle: "执行策略、授权与待审批动作" },
    { href: `${base}/artifacts`, icon: "archive" as const, title: "Artifact", subtitle: "证据与原始文件" },
    { href: `${base}/exchanges`, icon: "activity" as const, title: "报文", subtitle: "HTTP 交换记录" },
    { href: `${base}/research`, icon: "compass" as const, title: "研究资料", subtitle: "研究数据与历史" },
  ];
  return (
    <div className="mx-auto flex w-full max-w-[760px] flex-col gap-2 p-4 pb-10">
      {items.map((item) => (
        <a key={item.href} href={item.href} className="flex items-center gap-3 rounded-lg border border-cx-border px-4 py-3 transition-colors hover:bg-cx-bg-2">
          <Icon name={item.icon} size={15} className="text-cx-fg-4" />
          <span className="min-w-0 flex-1">
            <span className="block text-[13px] font-medium text-cx-fg">{item.title}</span>
            <span className="block text-[12px] text-cx-fg-4">{item.subtitle}</span>
          </span>
        </a>
      ))}
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

export function TasksWorkspace({ active, sheet = false }: { active: boolean; sheet?: boolean }) {
  const state = useStudioState();
  const ui = state.workspace.tasks;
  const subTab = ui.tab ?? "overview";
  const scroll = useRememberedScroll("tasks:overview", active);
  const projectId = state.workspace.projectId;
  const activeThread = state.threads[state.activeThreadId];
  const highlighted = ui.highlightId ? state.tasks.find((task) => task.id === ui.highlightId) : undefined;
  const pendingApprovals = state.approvals.filter((a) => a.status === "pending");
  const activeRuns = state.workRuns.filter((r) => r.state === "blocked" || r.state === "running");

  const tasks = useMemo(
    () => state.tasks.filter((task) => projectId === "all" || task.projectId === projectId || task.id === ui.highlightId),
    [projectId, state.tasks, ui.highlightId],
  );
  const newTaskProject = projectId === "all" ? activeThread?.projectId ?? state.projects[0]?.id ?? "" : projectId;

  const pendingConflicts = useMemo(() => state.sources.flatMap((s) => (s.conflicts ?? []).filter((c) => !c.resolvedBy || c.resolvedBy === "rule").map((c) => ({ source: s, conf: c }))), [state.sources]);
  const pendingFeedback = useMemo(() => state.feedbacks.filter((f) => f.status !== "resolved"), [state.feedbacks]);
  const dupGroups = useMemo(() => findClueDuplicates(state.clues, state.merges, state.dismissedDuplicates, state.undeterminedDuplicates), [state.clues, state.merges, state.dismissedDuplicates, state.undeterminedDuplicates]);
  const gapReports = useMemo(() => state.reports.filter((r) => r.status === "draft" && (r.materialGaps?.length ?? 0) > 0), [state.reports]);

  useViewing(active && subTab === "overview", highlighted ? { kind: "task", id: highlighted.id } : null);
  useEffect(() => { if (highlighted) studio.markSeen({ kind: "task", id: highlighted.id }); }, [highlighted]);

  if (subTab === "plan") return <div className="flex h-full min-h-0 flex-col" data-testid="studio-tasks-workspace"><DrillIn crumb="研究计划"><PlanDrillIn projectId={projectId} /></DrillIn><SheetAskBar sheet={sheet} target={null} /></div>;
  if (subTab === "runs") return <div className="flex h-full min-h-0 flex-col" data-testid="studio-tasks-workspace"><DrillIn crumb={ui.selectedRunId ? `工作记录/${ui.selectedRunId}` : "工作记录"}><RunDrillIn projectId={projectId} /></DrillIn><SheetAskBar sheet={sheet} target={null} /></div>;
  if (subTab === "monitors") return <div className="flex h-full min-h-0 flex-col" data-testid="studio-tasks-workspace"><DrillIn crumb="变化与监控"><MonitorsDrillIn active={active} /></DrillIn><SheetAskBar sheet={sheet} target={null} /></div>;
  if (subTab === "scope" || subTab === "env") return <div className="flex h-full min-h-0 flex-col" data-testid="studio-tasks-workspace"><DrillIn crumb={subTab === "scope" ? "范围与授权" : "环境与连接器"}>{subTab === "scope" ? <ScopeDrillIn /> : <EnvDrillIn />}</DrillIn><SheetAskBar sheet={sheet} target={null} /></div>;
  if (subTab === "links") return <div className="flex h-full min-h-0 flex-col" data-testid="studio-tasks-workspace"><DrillIn crumb="资料与检查器"><InspectorLinks /></DrillIn><SheetAskBar sheet={sheet} target={null} /></div>;

  const plan = state.planVersions[0];
  const kinds: Array<{ icon: IconName; items: Array<{ title: string; reason: string; go: () => void }>; many: { title: string; go: () => void } }> = [
    {
      icon: "shield",
      items: pendingApprovals.map((a) => { const t = approvalTitle(a); return { title: t.title, reason: t.subtitle, go: () => studio.setTasksUi({ tab: "scope" }) }; }),
      many: { title: `${pendingApprovals.length} 项授权待确认`, go: () => studio.setTasksUi({ tab: "scope" }) },
    },
    {
      icon: "gitCompare",
      items: pendingConflicts.map(({ source, conf }) => ({ title: `${conf.fieldLabel} · ${displayTitle(source.title).slice(0, 24)}`, reason: "字段冲突待裁决", go: () => openRef({ kind: "source", id: source.id }) })),
      many: { title: `${pendingConflicts.length} 个资产有字段冲突`, go: () => studio.setLibraryUi({ conflict: "unresolved" }) },
    },
    {
      icon: "alert",
      items: state.workRuns.filter((r) => r.state === "blocked").map((r) => ({ title: displayTitle(r.title), reason: `受阻：${r.blockerReason ?? "等待处理"}`, go: () => studio.setTasksUi({ tab: "runs", selectedRunId: r.id }) })),
      many: { title: `${state.workRuns.filter((r) => r.state === "blocked").length} 条工作记录受阻`, go: () => studio.setTasksUi({ tab: "runs" }) },
    },
    {
      icon: "messageCircle",
      items: pendingFeedback.map((f) => ({ title: `${f.externalTicketId}`, reason: `平台反馈待处理 · ${f.rawText.slice(0, 30)}`, go: () => openRef({ kind: "feedback", id: f.id }) })),
      many: { title: `${pendingFeedback.length} 条平台反馈待处理`, go: () => studio.setReportsUi({ tab: "feedbacks", view: "report" }) },
    },
    {
      icon: "file",
      items: gapReports.map((r) => ({ title: displayTitle(r.title ?? r.id), reason: `报告草稿缺 ${r.materialGaps?.length ?? 0} 项材料`, go: () => openRef({ kind: "report", id: r.id }) })),
      many: { title: `${gapReports.length} 份报告草稿缺材料`, go: () => openWorkspace("reports") },
    },
    {
      icon: "copy",
      items: dupGroups.map((g) => ({ title: `重复候选（${g.ids.length} 条）`, reason: "待审阅", go: () => studio.setFindingsUi({ view: "duplicates" }) })),
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
  const openTasks = tasks.filter((t) => t.status !== "done");
  const monitors = state.monitors ?? [];

  return (
    <div className="flex h-full min-h-0 flex-col" data-testid="studio-tasks-workspace">
      <div ref={scroll.ref} className="cx-scroll min-h-0 flex-1 overflow-y-auto">
        <div className="mx-auto flex w-full max-w-[760px] flex-col gap-4 p-4 pb-10">
          <SurfaceCard className="p-3">
            <div className="flex items-start gap-3">
              <div className="min-w-0 flex-1">
                <p className="truncate text-[15px] font-semibold text-cx-fg">{displayTitle(state.projects.find((p) => p.id === (projectId === "all" ? state.projects[0]?.id : projectId))?.title ?? "SRC 工作台")}</p>
              </div>
              <StudioBadge tone="accent">进行中</StudioBadge>
            </div>
            <Dl className="mt-3" items={[
              { label: "当前判断", value: plan?.currentJudgement ?? "—" },
              { label: "计划", value: plan ? `第 ${plan.version ?? ""} 版 · ${(plan.materialGaps ?? []).length} 个缺口` : "—" },
              { label: "范围", value: `${pendingApprovals.length} 项待确认` },
            ]} />
          </SurfaceCard>

          <div>
            <SectionHeader title="需要你处理" count={inbox.length} />
            <SurfaceCard>
              {inbox.length ? inbox.map((item, i) => (
                <SurfaceRow key={i} icon={item.icon} title={item.title} subtitle={item.reason} onClick={item.go} />
              )) : <SurfaceRow icon="check" title="暂无需要处理的事项" />}
            </SurfaceCard>
          </div>

          <div>
            <SectionHeader title="执行中" count={activeRuns.length + openTasks.length} action={<button type="button" className="text-[12px] font-medium text-cx-accent hover:underline" onClick={() => studio.setTasksUi({ tab: "runs" })}>全部工作记录</button>} />
            <SurfaceCard>
              {activeRuns.map((run) => (
                <SurfaceRow key={run.id} icon="terminal" tone={run.state === "running" ? "running" : "default"} title={displayTitle(run.title)} subtitle={run.state === "blocked" ? `受阻：${run.blockerReason ?? ""}` : RUN_STATE_LABEL[run.state]} onClick={() => studio.setTasksUi({ tab: "runs", selectedRunId: run.id })} />
              ))}
              {openTasks.map((task) => (
                <SurfaceRow
                  key={task.id}
                  icon="listTodo"
                  leading={(
                    <span className="grid size-8 shrink-0 place-items-center">
                      <Checkbox checked={task.status === "done"} aria-label={`完成「${task.title}」`} onCheckedChange={() => studio.setTaskStatus(task.id, "done")} className="[&>button]:size-5" />
                    </span>
                  )}
                  title={task.title}
                  subtitle={[task.assignee, task.due].filter(Boolean).join(" · ")}
                  trailing={(
                    <Menu ariaLabel={`任务 ${task.title} 操作`} className={MENU_HIT} trigger={<IconButton size="sm" icon="more" label="任务操作" />}>
                      <MenuItem icon="sparkles" onSelect={() => handTaskToAi(task)}>交给 Agent</MenuItem>
                      <MenuItem icon="check" onSelect={() => studio.setTaskStatus(task.id, "done")}>标为完成</MenuItem>
                    </Menu>
                  )}
                  onClick={() => studio.setTasksUi({ highlightId: task.id })}
                />
              ))}
              {!activeRuns.length && !openTasks.length ? <SurfaceRow icon="checkCheck" title="当前没有执行中的工作" /> : null}
              <NewTaskInline projectId={newTaskProject} />
            </SurfaceCard>
          </div>

          <div>
            <SectionHeader title="监控" count={monitors.length + state.changes.length} action={<button type="button" className="text-[12px] font-medium text-cx-accent hover:underline" onClick={() => studio.setTasksUi({ tab: "monitors" })}>全部变化</button>} />
            <SurfaceCard>
              {monitors.map((m) => (
                <SurfaceRow key={m.id} icon="bell" title={m.name} subtitle={[m.frequencyLabel ?? "每天", m.budgetUsed != null && m.budgetTotal ? `预算 ${Math.round((m.budgetUsed / m.budgetTotal) * 100)}%` : null].filter(Boolean).join(" · ")} onClick={() => studio.setTasksUi({ tab: "monitors" })} />
              ))}
              {state.changes.length ? <SurfaceRow icon="activity" title={`${state.changes.length} 条变化`} subtitle="资产与规则变化记录" onClick={() => studio.setTasksUi({ tab: "monitors" })} /> : null}
              {!monitors.length && !state.changes.length ? <SurfaceRow icon="bell" title="暂无监控" /> : null}
            </SurfaceCard>
          </div>

          <div>
            <SectionHeader title="计划与范围" />
            <SurfaceCard>
              <SurfaceRow icon="compass" title="研究计划" subtitle={plan ? `第 ${plan.version ?? ""} 版 · ${plan.focusQuestion?.slice(0, 30) ?? ""}` : ""} onClick={() => studio.setTasksUi({ tab: "plan" })} />
              <SurfaceRow icon="shield" title="范围与授权" subtitle={pendingApprovals.length ? `${pendingApprovals.length} 项待确认` : "无待确认事项"} onClick={() => studio.setTasksUi({ tab: "scope" })} />
              <SurfaceRow icon="plug" title="环境与连接器" subtitle="网络范围、取证模式与通道覆盖" onClick={() => studio.setTasksUi({ tab: "env" })} />
              {state.hostTag?.startsWith("task:") ? <SurfaceRow icon="layers" title="资料与检查器" subtitle="权限审批 · Artifact · 报文 · 研究资料" onClick={() => studio.setTasksUi({ tab: "links" })} /> : null}
            </SurfaceCard>
          </div>
        </div>
      </div>
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
    const id = studio.addTask({ projectId, title: title.trim(), assignee: STUDIO_TEAM[0] ?? "", links, threadId });
    setTitle("");
    studio.setTasksUi({ highlightId: id });
    toast({ title: "已新建任务", tone: "success" });
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
