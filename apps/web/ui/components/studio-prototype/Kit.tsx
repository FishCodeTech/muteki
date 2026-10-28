"use client";

import type { ReactNode } from "react";
import { cn } from "@/lib/cn";
import { CopyButton } from "@/components/chat/ui";
import {
  EVIDENCE_KIND_LABELS,
  readinessLabel,
  submissionStatusLabel,
  type StudioClue,
  type StudioEvidence,
  type StudioPlanVersion,
  type StudioPlatformFeedback,
  type StudioRef,
  type StudioReport,
  type StudioSource,
  type StudioWorkRun,
} from "./model";
import { revealConversation } from "./nav";
import { citeRef } from "./parts";
import { sendNow } from "./ConversationParts";
import { findSource, readStudioState, type StudioState } from "./store";

/** Attach `target` to the next message and send `prompt` right away. */
export function sendAbout(target: StudioRef | null, prompt: string) {
  const state = readStudioState();
  if (target) citeRef(target, { silent: true });
  revealConversation();
  sendNow(state.activeThreadId, prompt);
}

/** Field-grid markup used by every header card (same <dl> pattern as the chat 概览 surface). */
export function Dl({ items, className }: { items: Array<{ label: ReactNode; value: ReactNode; mono?: boolean; copy?: string }>; className?: string }) {
  return (
    <dl className={cn("grid grid-cols-[auto_minmax(0,1fr)] gap-x-4 gap-y-2 text-[13px]", className)}>
      {items.map((item, index) => (
        <div key={index} className="contents">
          <dt className="text-cx-fg-4">{item.label}</dt>
          <dd className="flex min-w-0 items-center gap-1.5 text-cx-fg-2">
            <span className={cn("min-w-0 truncate", item.mono && "font-cx-mono text-[12px]")}>{item.value}</span>
            {item.copy ? <CopyButton text={item.copy} label="复制" /> : null}
          </dd>
        </div>
      ))}
    </dl>
  );
}

export interface BriefContent {
  tone?: "neutral" | "warning" | "danger" | "success";
  headline: string;
  points?: string[];
  actions?: Array<{ label: string; prompt: string }>;
}
export function briefFor(state: StudioState, target: StudioRef): BriefContent | null {
  switch (target.kind) {
    case "source": {
      const source = findSource(state, target.id);
      return source ? briefForSource(state, source) : null;
    }
    case "clue": {
      const clue = state.clues.find((item) => item.id === target.id);
      return clue ? briefForClue(state, clue) : null;
    }
    case "evidence": {
      const ev = state.evidences.find((item) => item.id === target.id);
      return ev ? briefForEvidence(state, ev) : null;
    }
    case "report": {
      const report = state.reports.find((item) => item.id === target.id);
      return report ? briefForReport(state, report) : null;
    }
    case "run": {
      const run = state.workRuns.find((item) => item.id === target.id);
      return run ? briefForRun(state, run) : null;
    }
    case "plan": {
      const plan = state.planVersions.find((item) => item.id === target.id);
      return plan ? briefForPlan(state, plan) : null;
    }
    case "feedback": {
      const fb = state.feedbacks.find((item) => item.id === target.id);
      return fb ? briefForFeedback(state, fb) : null;
    }
    default:
      return null;
  }
}

export function briefForSource(state: StudioState, source: StudioSource): BriefContent {
  const conflicts = (source.conflicts ?? []).filter((conf) => !conf.resolvedBy || conf.resolvedBy === "rule");
  const openClues = state.clues.filter((clue) => (
    (clue.sourceId === source.id || clue.affectedSourceIds?.includes(source.id)) && clue.status !== "verified"
  ));
  const points: string[] = [];
  if (conflicts.length) points.push(`${conflicts.length} 个字段存在多来源冲突，需要裁决`);
  if (openClues.length) points.push(`${openClues.length} 条关联发现尚未核实`);
  if (source.lastFailedAt) points.push(`最近探测失败：${source.lastFailedReason ?? source.lastFailedAt}`);
  const tone: BriefContent["tone"] = conflicts.length ? "warning" : source.lastFailedAt ? "warning" : openClues.length ? "neutral" : "success";
  const headline = conflicts.length
    ? `存在 ${conflicts.length} 个待裁决字段冲突`
    : source.lastFailedAt
      ? "最近一次观测失败，资产未确认下线"
      : openClues.length
        ? `资产正常，有 ${openClues.length} 条发现待核实`
        : "字段一致，观测新鲜，暂无风险";
  const actions: BriefContent["actions"] = [];
  if (conflicts.length) actions.push({ label: "裁决冲突", prompt: `请分析资产 ${source.id} 的字段冲突并给出裁决建议。` });
  actions.push({ label: "提取发现", prompt: `请阅读资产 ${source.id} 的原文，提取还没有记录的发现。` });
  if (source.lastFailedAt) actions.push({ label: "刷新观测", prompt: `资产 ${source.id} 最近探测失败，请规划一次新的观测并说明要核对的内容。` });
  return { tone, headline, points, actions: actions.slice(0, 3) };
}

export function briefForClue(state: StudioState, clue: StudioClue): BriefContent {
  void state;
  const readiness = clue.readiness ?? "YELLOW";
  const points: string[] = [];
  (clue.missingMaterials ?? []).slice(0, 2).forEach((item) => points.push(`缺少：${item}`));
  if (clue.stale) points.push(`资料已更新，结论需复核${clue.staleReason ? `：${clue.staleReason}` : ""}`);
  const tone: BriefContent["tone"] = clue.stale || readiness === "RED" ? "danger" : readiness === "YELLOW" ? "warning" : "success";
  const headline = clue.stale
    ? "发现已陈旧，结论可能不再成立"
    : readiness === "GREEN"
      ? "证据齐备，达到可交付状态"
      : readiness === "RED"
        ? "缺关键证据，暂不可交付"
        : "证据不完整，交付前需要补充";
  const actions: BriefContent["actions"] = [
    { label: "让 AI 补证", prompt: `发现 ${clue.id} 缺少材料：${(clue.missingMaterials ?? []).join("；") || "无"}。请结合已有证据规划补充验证步骤。` },
    { label: "核验反证", prompt: `请核验发现 ${clue.id} 的反证与替代解释是否已排除，结论是否仍成立。` },
    { label: "起草报告", prompt: `请为发现 ${clue.id} 起草或更新交付报告草稿。` },
  ];
  return { tone, headline, points, actions };
}

export function briefForEvidence(state: StudioState, ev: StudioEvidence): BriefContent {
  const linked = ev.linkedClueIds.map((id) => state.clues.find((c) => c.id === id)?.title ?? id);
  const points: string[] = [];
  if (ev.unreadableReason) points.push(ev.unreadableReason);
  if (linked.length) points.push(`可用于发现：${linked.slice(0, 2).join("、")}${linked.length > 2 ? ` 等 ${linked.length} 条` : ""}`);
  if (ev.digestVerified !== "verified") points.push("SHA-256 摘要未校验或不符，引用前建议人工核对原件");
  const tone: BriefContent["tone"] = ev.completeness === "revoked" ? "danger" : ev.completeness === "complete" && ev.digestVerified === "verified" ? "success" : "warning";
  const headline = ev.completeness === "revoked"
    ? "原件已撤回，仅保留墓碑信息"
    : ev.completeness === "partial"
      ? "内容部分截断，使用时注意边界"
      : ev.completeness === "unavailable"
        ? "内容当前不可用"
        : "证据完整，可直接引用";
  return {
    tone,
    headline,
    points,
    actions: [
      { label: "核对完整性", prompt: `请核对证据 ${ev.id}（${EVIDENCE_KIND_LABELS[ev.kind]}）的完整性与摘要校验，说明它能支撑哪些结论。` },
      { label: "提取发现", prompt: `请阅读证据 ${ev.id} 的内容，提取还没有记录的发现或主张。` },
    ],
  };
}

export function briefForReport(state: StudioState, report: StudioReport): BriefContent {
  void state;
  const gaps = report.materialGaps ?? [];
  const revoked = report.evidenceIds.length > 0;
  void revoked;
  const tone: BriefContent["tone"] = gaps.length ? "danger" : report.status === "frozen" ? "success" : report.readiness === "GREEN" ? "success" : "warning";
  const headline = report.status === "frozen"
    ? `v${report.version} 已冻结，处于「${submissionStatusLabel(report.submission.status)}」`
    : gaps.length
      ? `冻结前存在 ${gaps.length} 项阻止冻结的材料缺口`
      : "材料齐备，可以冻结当前版本";
  const points = gaps.slice(0, 3).map((gap) => `缺口：${gap}`);
  const actions: BriefContent["actions"] = gaps.length
    ? [{ label: "让 AI 补齐", prompt: `报告 ${report.id} v${report.version} 冻结前有材料缺口：${gaps.join("；")}。请补齐对应内容。` }]
    : report.status === "draft"
      ? [{ label: "冻结前检查", prompt: `请检查报告 ${report.id} v${report.version} 是否满足冻结与提交条件。` }]
      : undefined;
  return { tone, headline, points, actions };
}

export function briefForRun(state: StudioState, run: StudioWorkRun): BriefContent {
  void state;
  const tone: BriefContent["tone"] = run.state === "failed" ? "danger" : run.state === "blocked" || run.state === "waiting_approval" ? "warning" : run.state === "completed" ? "success" : "neutral";
  const headline = run.state === "blocked"
    ? "工作记录受阻，需要人工处理"
    : run.state === "failed"
      ? "最近一次执行失败"
      : run.state === "waiting_approval"
        ? "等待权限确认后才能继续"
        : run.state === "completed"
          ? "执行已完成，产物可回流"
          : "执行进行中";
  const points = run.blockerReason ? [run.blockerReason] : undefined;
  const actions: BriefContent["actions"] = [
    { label: "分析原因", prompt: `请分析工作记录 ${run.id} 的当前状态${run.blockerReason ? `（${run.blockerReason}）` : ""}，给出下一步建议。` },
    { label: "规划下一步", prompt: `结合工作记录 ${run.id} 的日志与产物，规划下一轮执行动作。` },
  ];
  return { tone, headline, points, actions };
}

export function briefForPlan(state: StudioState, plan: StudioPlanVersion): BriefContent {
  void state;
  const gaps = plan.materialGaps ?? [];
  return {
    tone: gaps.length ? "warning" : "neutral",
    headline: plan.currentJudgement || `当前为 v${plan.version}，聚焦「${plan.focusQuestion}」`,
    points: gaps.slice(0, 3).map((gap) => `材料缺口：${gap}`),
    actions: [
      { label: "起草下一版", prompt: `请根据最新证据与进展，为任务计划 ${plan.id} 起草下一版本。` },
      { label: "检查缺口", prompt: `请检查任务计划 ${plan.id} 的材料缺口，给出补齐方案。` },
    ],
  };
}

export function briefForFeedback(state: StudioState, fb: StudioPlatformFeedback): BriefContent {
  const report = state.reports.find((item) => item.id === fb.reportId);
  const points: string[] = [];
  if (report) points.push(`关联报告 ${report.id} v${report.version}（${readinessLabel(report.readiness)}）`);
  return {
    tone: fb.status === "resolved" ? "success" : "warning",
    headline: fb.status === "resolved" ? "反馈已处理完毕" : "平台要求补充信息，建议尽快回应",
    points,
    actions: [
      { label: "起草回应", prompt: `请根据平台反馈 ${fb.id} 的要求，规划补证步骤并起草回应。` },
    ],
  };
}

// ---------------------------------------------------------------------------
// Section & SectionNav — borderless sections with a sticky anchor nav.
// ---------------------------------------------------------------------------

