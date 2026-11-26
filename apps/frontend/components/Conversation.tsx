"use client";

import { MotionIcon } from "@/components/MotionIcon";

import Link from "next/link";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { CSSProperties, PointerEvent as ReactPointerEvent, ReactNode } from "react";
import {
  ChatMessage, ControlCommand, DeckState, GuardApprovalDecisionAction, ProgressBrief, ProgressBriefItem, SwarmDigest,
  classifyHitlRequest, coordinatorThread, formatFollowupProgressDetail, swarmDigest,
} from "@/lib/events";
import { SavedFile, type OperatorTriageLabel } from "@/lib/useRun";
import { useT, useLang } from "@/lib/i18n";
import { formatClock, toEpochMs } from "@/lib/format";
import { EngineBar } from "@/components/EngineBar";
import { Icon, type IconName } from "@muteki/ui";
import { RunInspector } from "@/components/RunInspector";
import { RunSignalsStrip } from "@/components/RunSignalsStrip";
import { RunHeaderMenu } from "@/components/run-view/RunHeaderMenu";
import { RunStatePill, useElapsed } from "@/components/run-view/RunStatePill";
import { RunCategoryTag } from "@/components/run-view/RunCategoryTag";
import { HitlDock } from "@/components/run-view/HitlDock";
import { EmergencyStopConfirmDialog, PentestGuardApprovalQueue } from "@/components/run-view/PentestGuardApprovalQueue";
import { CtfPiChat } from "@/components/CtfPiChat";
import { CommandComposer, type FollowupActions } from "@/components/solve/CommandComposer";
import { DispatchComposer } from "@/components/solve/DispatchComposer";
import { ConversationMessage } from "@/components/conversation/ConversationMessage";
import { ConversationThinking } from "@/components/conversation/ConversationThinking";
import { ToolEntry } from "@/components/chat/timeline/ToolEntry";
import type { PresentableTool } from "@/components/chat/timeline/toolPresentation";
import { useCopied } from "@/lib/useCopied";
import { useCopyText } from "@/lib/useCopyText";
import { Accordion, AccordionContent, AccordionItem, AccordionTrigger, Button, Popover, SegmentedControl, Skeleton, Spinner, Tabs, TabsList, TabsTrigger } from "@muteki/ui";
import type { ArtifactView } from "@/lib/events";
import { RunSelectionAsk, RunSideChatFrameContext, type RunSideChatFrame } from "@/components/run-side/RunSideChat";
import { RUN_SIDE_WIDTH_MIN } from "@/components/run-side/runSideChatStore";
import { useMediaQuery } from "@/lib/useMediaQuery";

export type RunSideChatSlot = {
  open: boolean;
  side: "left" | "right";
  maximized: boolean;
  width: number;
  node: ReactNode;
  onAskSelection: (text: string) => void;
};
const RUN_SIDE_MAIN_MIN = 420;


export interface DispatchOpts {
  webSearch: boolean;
  mode: "ctf" | "pentest";
  executionKind?: "swarm" | "local_pi";
  localPiModel?: string;
  localPiAccountId?: string;
  localPiEffort?: string;
  localPiSkills?: string[];
  dispatchTemplateId?: string;
  goal?: string;
  scope?: string;
  collect?: boolean;
  flagFormat?: "brace" | "token" | "custom";
  flagWrapper?: string;
  collectCount?: number;
  reportGoalMode?: "automatic" | "count";
  expectedFindings?: number;
  containerMode?: boolean;
  allowOperatorInput: boolean;
  wallClockBudget?: number;
  maxTotalWorkers?: number;
  costBudgetUsd?: number;
  productionGuard?: { enabled: boolean; mode?: "off" | "protected" };
}

export interface ControlCommandOpts {
  expectedGeneration?: number;
  contextId?: string;
  requestId?: string;
  commandId?: string;
}

const COMPOSER_COMPACT_DISTANCE_PX = 200;
const COMPOSER_EXPAND_DISTANCE_PX = 8;
const COMPOSER_FADE_DISTANCE_PX = 2;
const INSPECTOR_WIDTH_DEFAULT = 360;
const INSPECTOR_WIDTH_MIN = 300;
const INSPECTOR_WIDTH_MAX = 560;
const INSPECTOR_WIDTH_STORAGE_KEY = "muteki.runInspector.width";
const INSPECTOR_OPEN_STORAGE_KEY = "muteki.runInspector.open";

function inspectorWidthMax(viewportWidth?: number): number {
  if (!viewportWidth || viewportWidth <= 0) return INSPECTOR_WIDTH_MAX;
  return Math.max(INSPECTOR_WIDTH_MIN, Math.min(INSPECTOR_WIDTH_MAX, Math.round(viewportWidth * 0.46)));
}

function clampInspectorWidth(width: number, viewportWidth?: number): number {
  const next = Number.isFinite(width) ? width : INSPECTOR_WIDTH_DEFAULT;
  return Math.round(Math.min(inspectorWidthMax(viewportWidth), Math.max(INSPECTOR_WIDTH_MIN, next)));
}

function clock(ts: number): string {
  return formatClock(ts, "");
}

function phaseLabel(phase: SwarmDigest["phase"], t: (k: string) => string): string {
  return t(`coord.phase.${phase}`);
}

const FLOW_STEPS: Array<{
  key: string;
  icon: IconName;
  phases: SwarmDigest["phase"][];
}> = [
  { key: "dispatch", icon: "send", phases: ["draft"] },
  { key: "reason", icon: "cpu", phases: ["racing", "running", "paused"] },
  { key: "explore", icon: "terminal", phases: ["running", "collecting"] },
  { key: "collect", icon: "flag", phases: ["collecting", "solved"] },
  { key: "finish", icon: "check", phases: ["solved", "goal_met", "finished"] },
  { key: "respond", icon: "pencil", phases: ["solved", "goal_met", "finished"] },
];

function activeFlowIndex(digest: SwarmDigest): number {
  if (digest.phase === "draft") return 0;
  if (digest.phase === "collecting") return 3;
  if (digest.phase === "solved" || digest.phase === "goal_met" || digest.phase === "finished") return 4;
  return 1;
}

function messageText(
  m: ChatMessage,
  t: (k: string, vars?: Record<string, string | number>) => string,
  mode: "ctf" | "pentest",
): string {
  return mode === "pentest" && m.i18nKey === "sys.goalMet"
    && m.i18nVars?.why === "model_goal_with_evidence"
    ? t("sys.pentestGoalMet")
    : m.i18nKey ? t(m.i18nKey, m.i18nVars) : m.content;
}

function isRoutineSystemLine(m: ChatMessage): boolean {
  return !m.followupId && m.role === "system" && m.kind === "status" && timelineNode(m).tone === "muted";
}

type ThreadItem =
  | { kind: "message"; message: ChatMessage }
  | { kind: "events"; messages: ChatMessage[] };

function groupThread(messages: ChatMessage[]): ThreadItem[] {
  const items: ThreadItem[] = [];
  let run: ChatMessage[] = [];
  const flush = () => {
    if (run.length > 1) items.push({ kind: "events", messages: run });
    else if (run.length === 1) items.push({ kind: "message", message: run[0] });
    run = [];
  };
  for (const message of messages) {
    if (isRoutineSystemLine(message)) {
      run.push(message);
      continue;
    }
    flush();
    items.push({ kind: "message", message });
  }
  flush();
  return items;
}

function SystemEventsGroup({
  messages,
  t,
  mode,
}: {
  messages: ChatMessage[];
  t: (k: string, vars?: Record<string, string | number>) => string;
  mode: "ctf" | "pentest";
}) {
  const [open, setOpen] = useState(false);
  const last = messages[messages.length - 1];
  return (
    <div className={`coord-bubble system status rv-events ${open ? "open" : ""}`} data-tone="muted">
      <TimelineNode node={{ tone: "muted" }} />
      <button type="button" className="rv-events-toggle" aria-expanded={open} onClick={() => setOpen((value) => !value)}>
        {!open && <span className="body">{messageText(last, t, mode)}</span>}
        <span className="rv-events-count">{t("rv.thread.events", { n: messages.length })}</span>
        <Icon name="chevronDown" size={12} className="rv-events-chev" />
        {!open && clock(last.ts) && <span className="ts">{clock(last.ts)}</span>}
      </button>
      {open && (
        <div className="rv-events-list">
          {messages.map((m) => (
            <div className="rv-events-row" key={m.id} data-mid={m.id}>
              <span className="body">{messageText(m, t, mode)}</span>
              {clock(m.ts) && <span className="ts">{clock(m.ts)}</span>}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

function CoordBubble({
  deck, onCancelFollowup, onExpireDirective, onOpenWriteup, onOpenReport,
  m,
  t,
  mode,
}: {
  deck?: DeckState;
  onCancelFollowup?: FollowupActions["onCancelFollowup"];
  onExpireDirective?: (id: string) => Promise<void>;
  onOpenWriteup?: (reportId?: string) => void;
  onOpenReport?: () => void;
  m: ChatMessage;
  t: (k: string, vars?: Record<string, string | number>) => string;
  mode: "ctf" | "pentest";
}) {
  const [copied, copy] = useCopied();
  const text = messageText(m, t, mode);
  const standbyReply = m.role === "agent" && (m.solverId === "followup" || m.solverId === "standby" || m.solverId?.endsWith("-standby"));
  const who = m.role === "human" ? t("coord.you") : m.role === "system"
    ? (m.kind === "insight" ? "insight" : "system")
    : standbyReply ? (mode === "pentest" ? "任务分析助手" : "Writeup 助手") : t("coord.title");
  const cls = m.role === "human" ? "you" : m.role === "system" ? `system ${m.kind}` : `coordinator ${m.kind}`;
  const isLong = m.role === "agent" && m.kind === "reasoning" && text.length > 520;
  const copyable = m.role === "agent" && (m.kind === "text" || m.kind === "reasoning") && text.trim().length > 40;
  const directive = deck?.operatorDirectives.find(d => d.id === m.directiveId);
  const time = clock(m.ts);
  const isWriteupReady = m.role === "system" && m.followupKind === "writeup" && m.followupSucceeded;
  const isReportReady = m.role === "system" && m.followupKind === "pentest_report" && m.followupSucceeded;
  const isReadyLink = isWriteupReady || isReportReady;
  const isGenerating = m.role === "system" && !m.sealed && Boolean(m.followupId) && (
    m.followupKind === "pentest_report"
      ? Boolean(deck?.reportGenerating && (!deck.activeReportJobId || deck.activeReportJobId === m.followupId))
      : Boolean(deck?.followupPending && deck.activeFollowupJobId === m.followupId)
  );
  const isFollowupFailed = m.role === "system" && m.sealed && (m.followupKind === "writeup" || m.followupKind === "pentest_report") && m.followupSucceeded === false;
  const node = timelineNode(m, isGenerating, isFollowupFailed);
  const generatingDetail = isGenerating
    ? (m.followupDetail || (m.followupKind !== "pentest_report" ? formatFollowupProgressDetail(deck?.followupProgress) : ""))
    : "";

  if (m.role === "system" && m.kind !== "insight") {
    return (
      <div className={`coord-bubble ${cls}`} data-mid={m.id} data-tone={node.tone}>
        <TimelineNode node={node} />
        <div
          className={`sys-line${isReadyLink ? " is-writeup-link" : ""}${isGenerating ? " is-generating" : ""}`}
          data-generating-kind={isGenerating ? (m.followupKind || "followup") : undefined}
        >
          <span className="body">{text}</span>
          {(isWriteupReady || isGenerating) && m.actor?.model && <span className="coord-writeup-model">{m.actor.model}</span>}
          {isGenerating && generatingDetail && <span className="coord-gen-detail">{generatingDetail}</span>}
          {isWriteupReady && onOpenWriteup && (
            <button
              type="button"
              data-writeup-link="true"
              className="coord-writeup-link"
              onClick={() => onOpenWriteup(m.reportId)}
            >
              <span>查看 Writeup</span>
              <Icon name="arrowUpRight" size={12} />
            </button>
          )}
          {isReportReady && onOpenReport && (
            <button
              type="button"
              data-report-link="true"
              className="coord-writeup-link"
              onClick={onOpenReport}
            >
              <span>查看测试报告</span>
              <Icon name="arrowUpRight" size={12} />
            </button>
          )}
          {isGenerating && m.followupKind !== "pentest_report" && m.followupId && (
            <button
              type="button"
              className="coord-gen-cancel"
              onClick={() => void onCancelFollowup?.(m.followupId!)}
            >
              取消
            </button>
          )}
          {time && <span className="ts">{time}</span>}
        </div>
      </div>
    );
  }

  return (
    <div className={`coord-bubble ${cls}`} data-mid={m.id} data-tone={node.tone}>
      <TimelineNode node={node} />
      <div className="who">
        {who} {standbyReply && m.actor?.model && <span className="rounded-full bg-cx-hover px-2 text-cx-xs">{m.actor.model}</span>} {m.role !== "human" && <span className="k">{t(`msg.kind.${m.kind}`)}</span>}
        {copyable && (
          <Button
            size="sm"
            variant="unstyled"
            className={`coord-copy ${copied ? "copied" : ""}`}
            onClick={() => copy(text)}
            aria-label={t("coord.copyMsgAria")}
          >
            <MotionIcon active={copied} from="copy" to="check" size={12} />
            <span className="cc-lbl">{copied ? t("common.copied") : t("common.copyShort")}</span>
          </Button>
        )}
        {time && <span className="ts">{time}</span>}
      </div>
      {directive && <div className="flex items-center gap-2 text-cx-xs text-cx-fg-3">
        <span>{directive.status === "consumed" ? "已落实为探索步骤" : directive.status === "expired" ? "已撤回" : "已应用"}</span>
        {!deck?.finished && !["consumed", "expired"].includes(directive.status) && <button type="button" onClick={() => void onExpireDirective?.(directive.id)}>撤回</button>}
      </div>}
      {isLong ? (
        <details className="coord-fold">
          <summary>{text.slice(0, 240).trimEnd()}…</summary>
          <div className="body">{text}</div>
        </details>
      ) : text.trimStart().startsWith("{") && /error|request_id|too many|quota|rate.?limit/i.test(text) && text.trim().length > 60 ? (
        <details className="coord-fold coord-error">
          <summary>{(() => {
            const code = text.match(/\b([45]\d\d)\b/);
            return code ? t("msg.errorHttp", { code: code[1] }) : (text.split("\n")[0] || text).slice(0, 120);
          })()}</summary>
          <div className="body">{text}</div>
        </details>
      ) : (
        <div className="body">{text}</div>
      )}
    </div>
  );
}

type TimelineTone = "you" | "agent" | "muted" | "info" | "success" | "danger" | "warn" | "insight";
interface TimelineNodeSpec { icon?: IconName; tone: TimelineTone; spin?: boolean }

const SYSTEM_DANGER_KEY = /fail|unavailable|noProgress|exhausted|rejected|error/i;
const SYSTEM_SUCCESS_KEY = /solved|goalMet|accepted/i;

function timelineNode(m: ChatMessage, generating = false, followupFailed = false): TimelineNodeSpec {
  if (m.role === "human") return { icon: "user", tone: "you" };
  if (m.role === "agent") return { icon: "sparkles", tone: "agent" };
  if (m.kind === "insight") return { icon: "lightbulb", tone: "insight" };
  if (m.kind === "guidance") return { icon: "compass", tone: "warn" };
  if (m.kind === "flag") return { icon: "flag", tone: "success" };
  if (generating) return { icon: "loader", tone: "info", spin: true };
  if (m.role === "system" && (m.followupKind === "writeup" || m.followupKind === "pentest_report") && m.followupSucceeded) {
    return { icon: "check", tone: "success" };
  }
  if (followupFailed) return { icon: "alert", tone: "danger" };
  const key = m.i18nKey ?? "";
  if (SYSTEM_DANGER_KEY.test(key)) return { icon: "alert", tone: "danger" };
  if (SYSTEM_SUCCESS_KEY.test(key)) return { icon: "check", tone: "success" };
  return { tone: "muted" };
}

function TimelineNode({ node }: { node: TimelineNodeSpec }) {
  return (
    <span className="coord-node" data-tone={node.tone} data-dot={node.icon ? undefined : "true"} aria-hidden="true">
      {node.icon ? <Icon name={node.icon} size={12} className={node.spin ? "cx-spin" : undefined} /> : null}
    </span>
  );
}

function DigestBubble({ digest, t, compact = false }: { digest: SwarmDigest; t: (k: string, v?: Record<string, string | number>) => string; compact?: boolean }) {
  if (compact) {
    if (!digest.latestVerified) return null;
    return (
      <div className="coord-bubble system status" data-tone="info">
        <TimelineNode node={{ icon: "activity", tone: "info" }} />
        <div className="sys-line">
          <span className="body">{t("coord.latestFact", { fact: digest.latestVerified })}</span>
        </div>
      </div>
    );
  }
  return (
    <div className="coord-bubble digest">
      <TimelineNode node={{ icon: "activity", tone: "info" }} />
      <div className="coord-digest-title">{t("coord.digestTitle")}</div>
      <div className="body">
        {digest.mode === "pentest"
          ? t("coord.pentestDigest", {
              facts: digest.verified, steps: digest.openSteps, goals: digest.goals,
              online: digest.onlineWorkers, total: digest.totalWorkers,
            })
          : t("coord.digestCtf", {
              phase: phaseLabel(digest.phase, t),
              facts: digest.verified,
              steps: digest.openSteps,
              goals: digest.goals,
              online: digest.onlineWorkers,
              total: digest.totalWorkers,
            })}
      </div>
      {digest.latestVerified && (
        <div className="digest-sub">{t(digest.mode === "pentest" ? "coord.latestVerified" : "coord.latestFact", { fact: digest.latestVerified })}</div>
      )}
    </div>
  );
}

function progressItemText(
  item: ProgressBriefItem | undefined,
  t: (k: string, v?: Record<string, string | number>) => string,
): string {
  if (!item) return "";
  return item.textKey ? t(item.textKey) : (item.text || "");
}

function legacyProgressBrief(deck: DeckState, digest: SwarmDigest): ProgressBrief {
  return {
    id: `legacy-${deck.runId}`,
    kind: deck.finished ? "final" : "periodic",
    phase: digest.phase,
    mode: digest.mode === "pentest" ? "pentest" : "ctf",
    trigger: "replay",
    summaryKey: "progress.summaryUnavailable",
    sourceFromSeq: 0,
    sourceToSeq: 0,
    sections: { confirmed: [], active: [], blocked: [], next: [] },
  };
}

function ProgressBriefBubble({
  brief,
  ts,
  t,
  folded = false,
}: {
  brief: ProgressBrief;
  ts: number;
  t: (k: string, v?: Record<string, string | number>) => string;
  folded?: boolean;
}) {
  const [open, setOpen] = useState(!folded);
  useEffect(() => { if (folded) setOpen(false); }, [folded]);
  const sections: Array<{ key: keyof ProgressBrief["sections"]; icon: IconName }> = [
    { key: "confirmed", icon: "check" },
    { key: "active", icon: "radio" },
    { key: "blocked", icon: "xCircle" },
    { key: "next", icon: "target" },
  ];
  const visible = sections.filter(({ key }) => brief.sections[key].length > 0);
  const detailCount = visible.reduce((count, { key }) => count + brief.sections[key].length, 0);
  const summaryText = brief.summaryKey
    ? t(brief.summaryKey, brief.summaryVars)
    : brief.summary || t(`progress.headline.${brief.kind}`);
  const node: TimelineNodeSpec = brief.kind === "blocker" || brief.kind === "stalled" ? { icon: "alert", tone: "warn" } : { icon: "listChecks", tone: "info" };
  if (!open) {
    return (
      <section className={`coord-bubble progress-brief kind-${brief.kind} rv-brief-folded`} aria-label={t("progress.title")} data-brief-id={brief.id}>
        <TimelineNode node={node} />
        <button type="button" className="rv-fold-row" aria-expanded={false} onClick={() => setOpen(true)}>
          <span className="progress-brief-kind">{t(`progress.kind.${brief.kind}`)}</span>
          <span className="rv-fold-text">{summaryText}</span>
          {clock(ts) && <time>{clock(ts)}</time>}
          <Icon name="chevronDown" size={12} className="rv-fold-chev" />
        </button>
      </section>
    );
  }
  return (
    <section
      className={`coord-bubble progress-brief kind-${brief.kind}`}
      aria-label={t("progress.title")}
      data-brief-id={brief.id}
    >
      <TimelineNode node={node} />
      <header className="progress-brief-head">
        <span>{t("msg.kind.progress")}</span>
        <span className="progress-brief-kind">{t(`progress.kind.${brief.kind}`)}</span>
        {clock(ts) && <time>{clock(ts)}</time>}
        {folded && (
          <button type="button" className="rv-fold-close" aria-expanded onClick={() => setOpen(false)} aria-label={t("insp.run.collapseSection")}>
            <Icon name="chevronUp" size={12} />
          </button>
        )}
      </header>
      <div className="progress-brief-summary">
        {(() => {
          const summary = summaryText;
          const rawAt = summary.indexOf('{"');
          if (rawAt < 0 || !/error|request.?id|too many|quota|rate.?limit/i.test(summary.slice(rawAt))) return summary;
          const code = summary.match(/\b([45]\d\d)\b/);
          return (
            <details className="coord-fold coord-error">
              <summary>{code ? t("msg.errorHttp", { code: code[1] }) : summary.slice(0, rawAt).trim()}</summary>
              <div className="body">{summary}</div>
            </details>
          );
        })()}
      </div>
      {visible.length > 0 && (
        <Accordion className="progress-brief-details border-0 bg-transparent rounded-none divide-y-0">
          <AccordionItem value={`progress-details-${brief.id}`}>
            <AccordionTrigger className="px-0 py-1 text-cx-xs font-normal text-cx-fg-3 hover:bg-transparent hover:text-cx-fg">
              <span>{t("progress.details", { count: detailCount })}</span>
            </AccordionTrigger>
            <AccordionContent className="progress-brief-details-body px-0 pb-0">
              <div className="progress-brief-sections">
                {visible.map(({ key, icon }) => (
                  <section className={`progress-brief-section section-${key}`} key={key}>
                    <h3><Icon name={icon} size={12} /> {t(`progress.section.${key}`)}</h3>
                    <ul>
                      {brief.sections[key].map((item, index) => (
                        <li key={`${key}-${item.ref?.kind ?? "item"}-${item.ref?.id ?? index}-${index}`}>
                          <span>{progressItemText(item, t)}</span>
                          {item.ref?.id && <code>{item.ref.kind}:{item.ref.id}</code>}
                        </li>
                      ))}
                    </ul>
                  </section>
                ))}
              </div>
              {brief.sourceToSeq > 0 && (
                <footer>{t("progress.source", { from: brief.sourceFromSeq, to: brief.sourceToSeq })}</footer>
              )}
            </AccordionContent>
          </AccordionItem>
        </Accordion>
      )}
    </section>
  );
}

function AnswerBubble({ digest, t, onOpenReport }: { digest: SwarmDigest; t: (k: string, v?: Record<string, string | number>) => string; onOpenReport?: () => void }) {
  const copyText = useCopyText();
  const none = digest.flags.length === 0 && digest.phase !== "goal_met";
  const multi = digest.expectedFlags > 1;
  return (
    <div className={`coord-bubble answer ${none ? "none" : ""}`}>
      <TimelineNode node={none ? { icon: "clock", tone: "muted" } : { icon: "check", tone: "success" }} />
      <div className="coord-digest-title">
        {t("coord.answerTitle")}
        {multi && digest.flags.length > 0 && (
          <span className="ans-flag-count">
            {`${digest.flags.length}/${digest.expectedFlags}`}
          </span>
        )}
      </div>
      {digest.flags.length > 0 ? (
        <div className="body">
          {digest.flags.map((f) => (
            <div key={f}>
              <Button size="sm" variant="unstyled" className="copytext ans-flag" aria-label={t("common.copyFlag")} onClick={() => copyText(f)}>{t("coord.answerFlag", { flag: f })}</Button>
            </div>
          ))}
        </div>
      ) : digest.phase === "goal_met" && digest.mode === "pentest" ? (
        <div className="body">{t("coord.pentestAnswer")}</div>
      ) : digest.phase === "goal_met" ? (
        <div className="body">
          <Button size="sm" variant="unstyled" className="copytext ans-flag goal" aria-label={t("common.copyAnswer")} onClick={() => copyText(digest.goalWhy || "")}>
            {t("coord.answerGoal", { why: digest.goalWhy || "" })}
          </Button>
        </div>
      ) : (
        <div className="body">{t("coord.answerNoneCtf", { facts: digest.verified })}</div>
      )}
      {digest.mode === "pentest" && onOpenReport && (
        <div className="answer-actions">
          <button
            type="button"
            data-report-link="true"
            className="coord-writeup-link"
            onClick={onOpenReport}
          >
            <span>查看测试报告</span>
            <Icon name="arrowUpRight" size={12} />
          </button>
        </div>
      )}
    </div>
  );
}

const PHASE_ICON: Record<SwarmDigest["phase"], IconName> = {
  draft: "dot",
  racing: "target",
  running: "target",
  collecting: "flag",
  paused: "pause",
  solved: "flag",
  goal_met: "check",
  finished: "check",
};

function FlowPopover({
  digest,
  hitlCount,
  onClose,
  t,
}: {
  digest: SwarmDigest;
  hitlCount: number;
  onClose: () => void;
  t: (k: string, v?: Record<string, string | number>) => string;
}) {
  const active = activeFlowIndex(digest);
  return (
    <div className="flow-popover">
      <div className="flow-head">
        <div>
          <div className="flow-title">{t("flow.title")}</div>
          <div className="flow-sub">{t("flow.subtitle")}</div>
        </div>
        <Button size="sm" variant="ghost" iconOnly className="flow-close" onClick={onClose} aria-label={t("settings.close")}><Icon name="x" size={14} /></Button>
      </div>
      <div className="flow-steps">
        {FLOW_STEPS.map((step, i) => {
          const activeStep = i === active || step.phases.includes(digest.phase);
          const done = i < active;
          return (
            <div className={`flow-step ${activeStep ? "active" : ""} ${done ? "done" : ""}`} key={step.key}>
              <span className="flow-node" aria-hidden="true">
                <Icon name={done ? "check" : step.icon} size={14} />
              </span>
              <span className="flow-copy">
                <span className="flow-name">{t(`flow.${step.key}.name`)}</span>
                <span className="flow-desc">{t(`flow.${step.key}.desc`)}</span>
              </span>
            </div>
          );
        })}
      </div>
      <div className="flow-live">
        <span>{t("flow.now", { phase: phaseLabel(digest.phase, t) })}</span>
        <span>{digest.mode === "pentest"
          ? t("flow.metrics", {
              workers: `${digest.onlineWorkers}/${digest.totalWorkers}`,
              facts: digest.verified,
              intents: digest.openIntents,
              dead: digest.deadEnds,
              hitl: hitlCount,
            })
          : t("flow.metricsCtf", {
              workers: `${digest.onlineWorkers}/${digest.totalWorkers}`,
              facts: digest.verified,
              steps: digest.steps,
              goals: digest.goals,
              hitl: hitlCount,
            })}</span>
      </div>
    </div>
  );
}

function StatusHero({ digest, hitlCount, progress, t }: { digest: SwarmDigest; hitlCount: number; progress?: ProgressBrief; t: (k: string, v?: Record<string, string | number>) => string }) {
  const copyText = useCopyText();
  const [flowOpen, setFlowOpen] = useState(false);
  const [resultsOpen, setResultsOpen] = useState(false);
  const elapsed = useElapsed(
    digest.startedAt,
    digest.finishedAt,
    digest.activeDurationMs,
    digest.activeSince,
    digest.phase === "solved" || digest.phase === "goal_met" || digest.phase === "finished",
  );
  const live = digest.phase === "running" || digest.phase === "collecting" || digest.phase === "racing";
  const singleFlag = digest.flags.length === 1 ? digest.flags[0] : "";
  const hasResultLedger = digest.platformConfirmationRequired || digest.flags.length > 1 || digest.expectedFlags > 1 || (digest.phase === "collecting" && digest.flags.length > 0);
  const resultsVisible = resultsOpen && hasResultLedger;
  const progressDetail = progress?.summary || progressItemText(
    progress?.sections.active[0]
      ?? progress?.sections.next[0]
      ?? progress?.sections.confirmed.at(-1)
      ?? progress?.sections.blocked[0],
    t,
  );
  const resultCount = digest.platformConfirmationRequired
      ? t("hero.detail.collectingPlatform", {
          n: digest.flags.length, accepted: digest.platformAccepted, need: digest.expectedFlags,
        })
      : (digest.expectedFlags > 1
        ? t("hero.results.progress", { n: digest.flags.length, total: digest.expectedFlags })
        : t("hero.results.count", { n: digest.flags.length }));
  let detail: string;
  if (digest.phase === "solved") {
    detail = digest.expectedFlags > 1
      ? t("hero.detail.solvedMulti", { n: digest.flags.length, total: digest.expectedFlags })
      : (digest.flags[0] ? t("hero.detail.solved", { flag: digest.flags[0] }) : t("hero.detail.solvedNoFlag"));
  } else if (digest.phase === "collecting") {
    detail = digest.platformConfirmationRequired
        ? t("hero.detail.collectingPlatform", {
            n: digest.flags.length,
            accepted: digest.platformAccepted,
            need: digest.expectedFlags,
          })
        : t("hero.detail.collecting", { n: digest.flags.length, total: digest.expectedFlags });
  } else if (digest.phase === "paused") {
    detail = hitlCount > 0 ? t("hero.detail.pausedN", { n: hitlCount }) : t("hero.detail.paused");
  } else if (digest.phase === "goal_met") {
    detail = digest.mode === "pentest"
      ? t("hero.pentestGoalMet")
      : digest.goalWhy || t("hero.detail.goalMet");
  } else if (digest.phase === "finished") {
    detail = t("hero.detail.finishedCtf", { facts: digest.verified, steps: digest.steps });
  } else if (digest.phase === "racing") {
    detail = digest.verifyingActive > 0 || digest.raceTotal > 0
      ? t("hero.detail.racingVerify", {
          raceActive: digest.raceActive || digest.onlineWorkers,
          raceTotal: digest.raceTotal || digest.totalWorkers,
          verifyingActive: digest.verifyingActive,
          verifyingMax: digest.verifyingMax,
        })
      : t("hero.detail.racing", { online: digest.onlineWorkers, total: digest.totalWorkers });
  } else if (digest.phase === "running") {
    detail = digest.mode === "pentest" && !progressDetail
      ? digest.onlineWorkers > 0
        ? t("hero.pentestRunning", { online: digest.onlineWorkers })
        : t("hero.pentestWaiting")
      : progressDetail
        ? progressDetail
        : digest.latestVerified
        ? t("hero.detail.runningFactCtf", { fact: digest.latestVerified })
        : digest.onlineWorkers > 0
          ? t("hero.detail.runningCtf", { online: digest.onlineWorkers, total: digest.totalWorkers })
          : t("hero.detail.runningIdle", { total: digest.totalWorkers });
  } else {
    detail = t("hero.detail.draft");
  }
  useEffect(() => {
    if (!resultsVisible) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      setResultsOpen(false);
    };
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("keydown", onKey);
    };
  }, [resultsVisible]);

  const toggleResults = () => {
    setFlowOpen(false);
    setResultsOpen((value) => !value);
  };
  return (
    <div className={`status-hero-shell ${resultsVisible ? "results-open" : ""}`}>
      <div
        className={`status-hero phase-${digest.phase} ${live ? "live" : ""} ${flowOpen || resultsVisible ? "open" : ""}`}
      >
        <span className="sh-status">
          <span className="sh-ico" aria-hidden="true"><Icon name={PHASE_ICON[digest.phase]} size={15} /></span>
          <span className="sh-phase">{t(digest.mode === "pentest" ? `coord.pentestPhase.${digest.phase}` : `coord.phase.${digest.phase}`)}</span>
          {(digest.phase === "racing" || digest.verifyingActive > 0) && (
            <span className="sh-phase-pills" aria-label={t("hero.label.progress")}>
              {(digest.phase === "racing" || digest.raceTotal > 0) && (
                <span className="sh-phase-pill race">
                  {t("coord.phasePill.race", {
                    n: digest.raceFinished || 0,
                    m: digest.raceTotal || digest.totalWorkers,
                  })}
                </span>
              )}
              {digest.verifyingActive > 0 && (
                <span className="sh-phase-pill verifying">
                  {t("coord.phasePill.verifying", {
                    k: digest.verifyingActive,
                    l: digest.verifyingMax,
                  })}
                </span>
              )}
            </span>
          )}
        </span>
        <span className="sh-main">
          {hasResultLedger ? (
            <Button
              size="sm"
              variant="unstyled"
              className="sh-results-toggle"
              aria-expanded={resultsVisible}
              aria-controls="status-flag-results"
              onClick={toggleResults}
              aria-label={t(resultsVisible ? "hero.results.collapse" : "hero.results.expand")}
            >
              <span className="sh-results-count">{resultCount}</span>
              {digest.flags[0] && <code className="sh-results-preview">{digest.flags[0]}</code>}
              {digest.flags.length > 1 && <span className="sh-results-more">+{digest.flags.length - 1}</span>}
              <Icon name="chevronDown" size={13} />
            </Button>
          ) : singleFlag ? (
            <Button size="sm" variant="unstyled" className="copytext sh-detail sh-detail-flag" aria-label={t("common.copyFlag")} onClick={() => copyText(singleFlag)}>{singleFlag}</Button>
          ) : (
            <span className="sh-detail" data-tooltip={detail}>{detail}</span>
          )}
        </span>
        <span className="sh-meta">
          {live && digest.onlineWorkers > 0 && (
            <span className="sh-workers" data-tooltip={t("meta.workers")}>
              <Icon name="cpu" size={12} /> {digest.onlineWorkers}/{digest.totalWorkers}
            </span>
          )}
          {elapsed && <span className="sh-elapsed" data-tooltip={t("meta.elapsed")}><Icon name="clock" size={12} /> {elapsed}</span>}
          {digest.mode !== "pentest" && <Popover
            open={flowOpen}
            onOpenChange={(isOpen) => { setFlowOpen(isOpen); if (isOpen) setResultsOpen(false); }}
            placement="bottom-end"
            ariaLabel={t("flow.title")}
            className="p-0"
            trigger={
              <button
                type="button"
                className="sh-flow"
                aria-label={t("flow.open")}
              >
                <Icon name="list" size={12} /> {t("flow.short")}
              </button>
            }
          >
            <FlowPopover digest={digest} hitlCount={hitlCount} onClose={() => setFlowOpen(false)} t={t} />
          </Popover>}
        </span>
      </div>
      {resultsVisible && (
        <section id="status-flag-results" className="sh-results-panel" aria-label={t("hero.results.title")}>
          <header className="sh-results-head">
            <span><Icon name="flag" size={13} /> {t("hero.results.title")}</span>
            <span>{resultCount}</span>
          </header>
          <div className="sh-results-list">
            {digest.flags.map((flag, index) => (
                <div className="sh-result-row" key={`${index}-${flag}`}>
                  <span className="sh-result-index">{String(index + 1).padStart(2, "0")}</span>
                  <Button size="sm" variant="unstyled" className="copytext sh-result-value" aria-label={t("common.copyFlag")} onClick={() => copyText(flag)}>{flag}</Button>
                </div>
              ))}
          </div>
        </section>
      )}
    </div>
  );
}

function CoordinatorThread({
  deck,
  onCancelFollowup, onExpireDirective, onOpenWriteup, onOpenReport,
  running,
  onAnswer,
  onDismiss,
  onOpenControlHistory,
}: {
  deck: DeckState;
  onCancelFollowup?: FollowupActions["onCancelFollowup"];
  onExpireDirective?: (id: string) => Promise<void>;
  onOpenWriteup?: (reportId?: string) => void;
  onOpenReport?: () => void;
  running: boolean;
  onAnswer: (requestId: string, opt: string, commandId: string) => Promise<boolean>;
  onDismiss?: (requestId: string, commandId: string) => Promise<boolean>;
  onOpenControlHistory?: () => void;
}) {
  const t = useT();
  const { chat, driverKind } = deck;
  const { rows, latestBrief } = useMemo(() => {
    const messages = coordinatorThread({ chat, driverKind });
    return {
      rows: groupThread(messages),
      latestBrief: [...messages].reverse().find((message) => message.kind === "progress" && message.progressBrief),
    };
  }, [chat, driverKind]);
  const digest = swarmDigest(deck);
  const hasProgress = Boolean(latestBrief);
  const latestBriefId = latestBrief?.id;
  const legacyBrief = !hasProgress && deck.finished ? legacyProgressBrief(deck, digest) : undefined;
  const hasContent = rows.length > 0 || deck.hitlRequests.length > 0 || deck.hitlHistory.length > 0;
  const feedRef = useRef<HTMLDivElement>(null);
  const stick = useRef(true);
  useEffect(() => {
    const el = feedRef.current;
    if (el && stick.current) el.scrollTop = el.scrollHeight;
  }, [deck.chat, deck.hitlRequests, deck.hitlHistory, running, deck.finished]);
  return (
    <div
      className="coord-thread"
      ref={feedRef}
      onScroll={(e) => {
        const el = e.currentTarget;
        stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 80;
      }}
    >
      <div className="coord-wrap">
        {!hasContent && !deck.finished && !deck.chatThreadId && (
          <div className="coord-empty">{t("coord.empty")}</div>
        )}
        {rows.map((item) => item.kind === "events"
          ? <SystemEventsGroup key={item.messages[0].id} messages={item.messages} t={t} mode={deck.mode || "ctf"} />
          : item.message.kind === "progress" && item.message.progressBrief
            ? <ProgressBriefBubble key={item.message.id} brief={item.message.progressBrief} ts={item.message.ts} t={t} folded={item.message.id !== latestBriefId} />
            : <CoordBubble key={item.message.id} deck={deck} onCancelFollowup={onCancelFollowup} onExpireDirective={onExpireDirective} onOpenWriteup={onOpenWriteup} onOpenReport={onOpenReport} m={item.message} t={t} mode={deck.mode || "ctf"} />)}
        <HitlDock
          hitlRequests={deck.hitlRequests}
          hitlHistory={deck.hitlHistory}
          onAnswer={onAnswer}
          onDismiss={onDismiss}
          onOpenControlHistory={onOpenControlHistory}
        />
        {running && !hasProgress && <DigestBubble digest={digest} t={t} compact={deck.mode !== "pentest"} />}
        {legacyBrief && <ProgressBriefBubble brief={legacyBrief} ts={deck.finishedAt ?? Date.now()} t={t} />}
        {deck.finished && <AnswerBubble digest={digest} t={t} onOpenReport={onOpenReport} />}
      </div>
    </div>
  );
}

function LocalPiChatThread({
  deck,
  onCancelFollowup, onExpireDirective, onOpenWriteup, onOpenReport,
  running,
  onAnswer,
  onDismiss,
  onOpenControlHistory,
}: {
  deck: DeckState;
  onCancelFollowup?: FollowupActions["onCancelFollowup"];
  onExpireDirective?: (id: string) => Promise<void>;
  onOpenWriteup?: (reportId?: string) => void;
  onOpenReport?: () => void;
  running: boolean;
  onAnswer: (requestId: string, opt: string, commandId: string) => Promise<boolean>;
  onDismiss?: (requestId: string, commandId: string) => Promise<boolean>;
  onOpenControlHistory?: () => void;
}) {
  const t = useT();
  const { chat, driverKind, chatThreadId } = deck;
  const messages = useMemo(() => coordinatorThread({ chat, driverKind }).filter(
    (m) => chatThreadId ? Boolean(m.followupId || m.directiveId) : m.role !== "system" || (m.i18nKey !== "coord.sys.runStarted" && m.i18nKey !== "coord.sys.flagCandidate"),
  ), [chat, driverKind, chatThreadId]);
  const hasContent = messages.length > 0 || deck.hitlRequests.length > 0 || deck.hitlHistory.length > 0;
  const feedRef = useRef<HTMLDivElement>(null);
  const stick = useRef(true);
  useEffect(() => {
    const el = feedRef.current;
    if (el && stick.current) el.scrollTop = el.scrollHeight;
  }, [deck.chat, deck.hitlRequests, deck.hitlHistory, running, deck.finished]);

  const lastMessage = messages[messages.length - 1];
  const waitingForFirstAgentItem = !deck.chatThreadId && running && (!lastMessage || lastMessage.role === "human");
  const showEmptyState = !hasContent && !deck.finished && !deck.chatThreadId;
  if (!hasContent && !waitingForFirstAgentItem && !showEmptyState) return null;

  return (
    <div
      className="coord-thread"
      ref={feedRef}
      onScroll={(e) => {
        const el = e.currentTarget;
        stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 80;
      }}
    >
      <div className="coord-wrap flex flex-col gap-3 py-4">
        {showEmptyState && (
          <div className="coord-empty">{t("coord.empty")}</div>
        )}
        {messages.map((m, idx) => {
          const isLast = idx === messages.length - 1;
          if (m.followupId || m.directiveId || m.solverId === "followup" || m.solverId === "standby") return <CoordBubble key={m.id} m={m} t={t} mode={deck.mode || "ctf"} deck={deck} onCancelFollowup={onCancelFollowup} onExpireDirective={onExpireDirective} onOpenWriteup={onOpenWriteup} onOpenReport={onOpenReport} />;
          if (m.role === "human") {
            return (
              <ConversationMessage
                key={m.id}
                role="user"
                messageId={m.id}
                text={m.content}
                createdAt={m.ts ? new Date(toEpochMs(m.ts)).toISOString() : undefined}
                showActions={false}
              />
            );
          }
          if (m.role === "system") {
            const body = m.i18nKey ? t(m.i18nKey, m.i18nVars) : m.content;
            if (!body) return null;
            return (
              <div key={m.id} className="cb-sys">
                <span className="cb-sys-dot" />
                <span>{body}</span>
              </div>
            );
          }
          if (m.kind === "reasoning") {
            return (
              <ConversationThinking
                key={m.id}
                working={running && !m.sealed && isLast}
                summary={m.content.trim().split("\n")[0]?.slice(0, 100) || "思考过程"}
                rows={[{ primary: m.content }]}
              />
            );
          }
          if (m.kind === "tool") {
            const sepIdx = m.content.indexOf(" — ");
            const name = sepIdx >= 0 ? m.content.slice(0, sepIdx).trim() : m.content.trim() || "tool";
            const argsSummary = sepIdx >= 0 ? m.content.slice(sepIdx + 3).trim() : undefined;
            const durationMs =
              m.toolStartedAt != null && m.toolEndedAt != null
                ? Math.max(0, Math.round(toEpochMs(m.toolEndedAt) - toEpochMs(m.toolStartedAt)))
                : undefined;
            const tool: PresentableTool = {
              id: m.toolCallId || m.id,
              name,
              status: m.toolPending ? "running" : m.toolFailed ? "failed" : "completed",
              argsSummary,
              outputSummary: m.toolOutput,
              durationMs,
            };
            return <ToolEntry key={m.id} tool={tool} threadId={deck.runId} />;
          }
          return (
            <ConversationMessage
              key={m.id}
              role="assistant"
              sourceProvider="pi"
              messageId={m.id}
              text={m.content}
              isStreaming={running && !m.sealed && isLast}
              createdAt={m.ts ? new Date(toEpochMs(m.ts)).toISOString() : undefined}
              showIdentity={false}
              showActions={!running || !!m.sealed || !isLast}
            />
          );
        })}
        {waitingForFirstAgentItem && (
          <ConversationThinking working={true} activeLabel="正在思考…" />
        )}
        <HitlDock
          hitlRequests={deck.hitlRequests}
          hitlHistory={deck.hitlHistory}
          onAnswer={onAnswer}
          onDismiss={onDismiss}
          onOpenControlHistory={onOpenControlHistory}
        />
      </div>
    </div>
  );
}

function Composer({
  runId,
  workspaceMode,
  driverKind,
  localPiSession,
  started,
  solved,
  running,
  finished,
  followupPending,
  writeupGenerating,
  reportGenerating,
  hasWriteup, activeFollowupJobId,
  paused,
  solvers,
  flags,
  onDispatch,
  onCommand,
  onRequestProgress,
  onResolve,
  onFollowup, onCancelFollowup, onOpenWriteup,
  attachments,
  onAddFiles,
  onRemoveFile,
  prefill,
  onPrefillConsumed,
  onOpenSideChat,
  onOpenReport,
  receipt,
  controlledText,
  onControlledTextChange,
  dispatchHeader,
  dispatchPending,
  compact,
  onRequestExpand,
  productionGuardProtected,
  guardPendingCount,
  guardEmergencyStopping,
  onEmergencyStop,
  onProductionGuardChange,
}: {
  runId: string;
  workspaceMode: "ctf" | "pentest";
  driverKind?: "idle" | "swarm" | "local_pi";
  localPiSession?: DeckState["localPiSession"];
  started: boolean;
  solved: boolean;
  running: boolean;
  finished: boolean;
  followupPending: boolean;
  writeupGenerating?: boolean;
  reportGenerating?: boolean;
  hasWriteup: boolean; activeFollowupJobId?: string;
  paused: boolean;
  solvers: string[];
  flags: string[];
  onDispatch: (prompt: string, opts: DispatchOpts) => void | boolean | Promise<void | boolean>;
  onCommand: (target: string, action: string, text: string,
              opts?: ControlCommandOpts) => Promise<boolean>;
  onFollowup: FollowupActions["onFollowup"];
  onCancelFollowup: FollowupActions["onCancelFollowup"];
  onOpenWriteup?: (reportId?: string) => void;
  onRequestProgress: () => Promise<boolean>;
  onResolve: (text?: string) => Promise<boolean>;
  attachments: SavedFile[];
  onAddFiles: (files: FileList | File[]) => void | Promise<number | void>;
  onRemoveFile: (path: string) => void;
  prefill: ComposerPrefill | null;
  onPrefillConsumed: () => void;
  onOpenSideChat?: (question?: string) => void;
  onOpenReport?: () => void;
  receipt?: ControlCommand;
  controlledText?: string;
  onControlledTextChange?: (value: string) => void;
  dispatchHeader?: ReactNode;
  dispatchPending: boolean;
  compact?: boolean;
  onRequestExpand?: () => void;
  productionGuardProtected?: boolean;
  guardPendingCount?: number;
  guardEmergencyStopping?: boolean;
  onEmergencyStop?: () => Promise<boolean>;
  onProductionGuardChange?: (enabled: boolean) => void;
}) {
  const [internalText, setInternalText] = useState("");
  const text = controlledText ?? internalText;
  const setText = useCallback((value: string) => {
    setInternalText(value);
    onControlledTextChange?.(value);
  }, [onControlledTextChange]);
  const mode = workspaceMode;

  const wasStarted = useRef(started);
  useEffect(() => {
    if (started && !wasStarted.current) setText("");
    wasStarted.current = started;
  }, [started, setText]);

  if (!started) {
    return (
      <DispatchComposer
        mode={mode}
        text={text}
        onTextChange={setText}
        attachments={attachments}
        onAddFiles={onAddFiles}
        onRemoveFile={onRemoveFile}
        onDispatch={onDispatch}
        prefill={prefill}
        onPrefillConsumed={onPrefillConsumed}
        below={dispatchHeader}
        dispatchPending={dispatchPending}
        onProductionGuardChange={onProductionGuardChange}
      />
    );
  }

  return (
    <CommandComposer
      runId={runId}
      mode={mode}
      driverKind={driverKind}
      localPiSession={localPiSession}
      phase={finished ? "finished" : "running"}
      text={text}
      onTextChange={setText}
      running={running}
      paused={paused}
      followupPending={followupPending}
      writeupGenerating={writeupGenerating}
      reportGenerating={reportGenerating}
      solved={solved}
      solvers={solvers}
      flags={flags}
      onCommand={onCommand}
      onRequestProgress={onRequestProgress}
      onResolve={onResolve}
      onFollowup={onFollowup} onCancelFollowup={onCancelFollowup} onOpenWriteup={onOpenWriteup ? () => onOpenWriteup() : undefined}
      hasWriteup={hasWriteup} activeFollowupJobId={activeFollowupJobId}
      onOpenSideChat={onOpenSideChat}
      onOpenReport={onOpenReport}
      receipt={receipt}
      compact={compact}
      onRequestExpand={onRequestExpand}
      productionGuardProtected={productionGuardProtected}
      guardPendingCount={guardPendingCount}
      guardEmergencyStopping={guardEmergencyStopping}
      onEmergencyStop={onEmergencyStop}
    />
  );
}

type ComposerPrefill = {
  mode: "ctf" | "pentest";
  text: string;
};

const WELCOME_EXAMPLES: Array<{ key: string; icon: IconName; mode: "ctf" | "pentest" }> = [
  { key: "ex1", icon: "globe", mode: "ctf" },
  { key: "ex2", icon: "lock", mode: "ctf" },
  { key: "ex3", icon: "cpu", mode: "ctf" },
];
const PENTEST_WELCOME_EXAMPLES: Array<{ key: string; icon: IconName; mode: "ctf" | "pentest" }> = [
  { key: "ex1", icon: "target", mode: "pentest" },
  { key: "ex2", icon: "shield", mode: "pentest" },
  { key: "ex3", icon: "refresh", mode: "pentest" },
];

function Welcome({
  t,
  workspaceMode = "ctf",
}: {
  t: (k: string) => string;
  workspaceMode?: "ctf" | "pentest";
}) {
  const prefix = workspaceMode === "pentest" ? "welcome.pentest" : "welcome";
  return (
    <div className="welcome workbench-welcome-content">
      <div className="welcome-hero workbench-welcome-intro">
        <h1 className="workbench-welcome-title">{t(`${prefix}.title`)}</h1>
        <p className="workbench-welcome-subtitle">{t(`${prefix}.sub`)}</p>
      </div>
    </div>
  );
}

function WelcomeExamples({
  t,
  onChoose,
  workspaceMode = "ctf",
}: {
  t: (k: string) => string;
  onChoose: (prefill: ComposerPrefill) => void;
  workspaceMode?: "ctf" | "pentest";
}) {
  const prefix = workspaceMode === "pentest" ? "welcome.pentest" : "welcome";
  const examples = workspaceMode === "pentest" ? PENTEST_WELCOME_EXAMPLES : WELCOME_EXAMPLES;
  return (
    <section className="welcome-examples" aria-label={t("welcome.examplesLabel")}>
      <div className="suggest">
        {examples.map((ex) => (
          <Button
            variant="unstyled"
            className="suggest-card"
            key={ex.key}
            data-tooltip={t(`${prefix}.${ex.key}.tg`)}
            onClick={() => onChoose({
              mode: ex.mode,
              text: t(`${prefix}.${ex.key}.prompt`),
            })}
          >
            <span className="s-ico" aria-hidden="true"><Icon name={ex.icon} size={16} /></span>
            <span className="s-nm">{t(`${prefix}.${ex.key}.nm`)}</span>
          </Button>
        ))}
      </div>
    </section>
  );
}

export function Conversation({
  workspaceMode = "ctf",
  deck,
  running,
  loading,
  pendingDispatchPrompt,
  dispatchAccepted,
  composerDraftId,
  replayEmpty = false,
  onCommand,
  onRequestProgress,
  onResolve,
  onFollowup, onCancelFollowup, onOpenWriteup,
  onDispatch,
  attachments,
  onAddFiles,
  onRemoveFile,
  artifactOpen,
  artifactView,
  onOpenArtifact,
  onShowConversation,
  runtimePanel,
  onToggleRail,
  theme,
  onToggleTheme,
  onSpawnWorker,
  onKillWorker,
  onOpenWorker,
  onOpenAgent,
  onOpenKnowledge,
  onOpenWorkspace,
  onHitlAnswered,
  connected,
  onOpenSideChat,
  sideChat,
  onExpireDirective, onToggleFreezeWorker,
  reportOpen = false,
  reportPanel,
  onOpenReport,
  operatorLabel,
  onUpdateOperatorLabel,
  chatThreadId,
  runTitle,
  onDecideGuardApproval,
  onEmergencyStop,
}: {
  workspaceMode?: "ctf" | "pentest";
  deck: DeckState;
  running: boolean;
  loading: boolean;
  pendingDispatchPrompt?: string;
  dispatchAccepted: boolean;
  composerDraftId: string;
  replayEmpty?: boolean;
  onCommand: (target: string, action: string, text: string,
              opts?: ControlCommandOpts) => Promise<boolean>;
  onFollowup: FollowupActions["onFollowup"];
  onCancelFollowup: FollowupActions["onCancelFollowup"];
  onOpenWriteup?: (reportId?: string) => void;
  onRequestProgress: () => Promise<boolean>;
  onResolve: (text?: string) => Promise<boolean>;
  onDispatch: (prompt: string, opts: DispatchOpts) => void | boolean | Promise<void | boolean>;
  attachments: SavedFile[];
  onAddFiles: (files: FileList | File[]) => void | Promise<number | void>;
  onRemoveFile: (path: string) => void;
  artifactOpen: boolean;
  artifactView: ArtifactView;
  onOpenArtifact: (view: ArtifactView) => void;
  onShowConversation: () => void;
  runtimePanel: ReactNode;
  onToggleRail: () => void;
  theme: "light" | "dark";
  onToggleTheme: () => void;
  onSpawnWorker: (engine?: string) => void;
  onKillWorker: (solverId: string) => void;
  onOpenWorker: (solverId: string) => void;
  onOpenAgent?: (solverId: string) => void;
  onOpenKnowledge?: (id: string) => void;
  onOpenWorkspace: () => void;
  onHitlAnswered?: () => void;
  connected: boolean;
  onOpenSideChat?: (question?: string) => void;
  sideChat?: RunSideChatSlot;
  onExpireDirective: (id: string) => Promise<void>;
  onToggleFreezeWorker: (id: string, paused: boolean) => void;
  reportOpen?: boolean;
  reportPanel?: ReactNode;
  onOpenReport?: () => void;
  operatorLabel?: OperatorTriageLabel | null;
  onUpdateOperatorLabel?: (label: OperatorTriageLabel | null) => void;
  chatThreadId?: string;
  runTitle?: string;
  onDecideGuardApproval?: (approvalId: string, action: GuardApprovalDecisionAction, note?: string) => Promise<boolean>;
  onEmergencyStop?: () => Promise<boolean>;
}) {
  const t = useT();
  const { lang, setLang } = useLang();
  const scrollRef = useRef<HTMLDivElement>(null);
  const stick = useRef(true);
  const [composerCompact, setComposerCompact] = useState(false);
  const lastScrollTop = useRef(0);
  const composerCompactRef = useRef(false);
  composerCompactRef.current = composerCompact;
  const expandComposer = useCallback(() => setComposerCompact(false), []);
  const [composerFade, setComposerFade] = useState(false);
  const syncComposerFade = useCallback((el: HTMLElement) => {
    setComposerFade(el.scrollHeight - el.scrollTop - el.clientHeight > COMPOSER_FADE_DISTANCE_PX);
  }, []);
  const [inspectorWidth, setInspectorWidth] = useState(INSPECTOR_WIDTH_DEFAULT);
  const [inspectorOpen, setInspectorOpen] = useState(true);
  const [composerPrefill, setComposerPrefill] = useState<ComposerPrefill | null>(null);
  const [draftProtectedMode, setDraftProtectedMode] = useState(false);
  const [topEmergencyStopOpen, setTopEmergencyStopOpen] = useState(false);
  const [topEmergencyStopBusy, setTopEmergencyStopBusy] = useState(false);
  const [pentestDrafts, setPentestDrafts] = useState<Record<string, string>>({});
  const pentestDraftKey = deck.started ? deck.runId : composerDraftId;
  const pentestDraftText = pentestDrafts[pentestDraftKey] ?? "";
  const setPentestDraftText = useCallback((value: string) => {
    setPentestDrafts((previous) => ({ ...previous, [pentestDraftKey]: value }));
  }, [pentestDraftKey]);
  const consumeComposerPrefill = useCallback(() => setComposerPrefill(null), []);
  const [inspectorResizing, setInspectorResizing] = useState(false);
  const inspectorResizeCleanup = useRef<(() => void) | null>(null);
  const runNotFound = Boolean(loading && replayEmpty);
  useEffect(() => { setComposerCompact(false); }, [deck.runId, deck.started]);
  useEffect(() => {
    const el = scrollRef.current;
    if (!el) return;
    const onWheel = (event: WheelEvent) => {
      if (!composerCompactRef.current || event.deltaY <= 0) return;
      if (el.scrollHeight - el.scrollTop - el.clientHeight > COMPOSER_EXPAND_DISTANCE_PX) return;
      setComposerCompact(false);
    };
    el.addEventListener("wheel", onWheel, { passive: true });
    return () => el.removeEventListener("wheel", onWheel);
  }, [deck.started]);
  useEffect(() => {
    const el = scrollRef.current;
    if (!el || !deck.started || typeof ResizeObserver === "undefined") return;
    syncComposerFade(el);
    const observer = new ResizeObserver(() => syncComposerFade(el));
    const observeChildren = () => {
      observer.disconnect();
      observer.observe(el);
      for (const child of Array.from(el.children)) observer.observe(child);
    };
    observeChildren();
    const children = new MutationObserver(observeChildren);
    children.observe(el, { childList: true });
    return () => { children.disconnect(); observer.disconnect(); };
  }, [deck.started, reportOpen, syncComposerFade]);
  useEffect(() => {
    const el = scrollRef.current;
    if (!el) return;
    if (!deck.started) {
      stick.current = true;
      el.scrollTop = 0;
      return;
    }
    if (stick.current) el.scrollTop = el.scrollHeight;
  }, [deck.chat, deck.hitlRequests, deck.hitlHistory, deck.started, deck.finished]);
  useEffect(() => {
    try {
      const raw = window.localStorage.getItem(INSPECTOR_WIDTH_STORAGE_KEY);
      if (raw) setInspectorWidth(clampInspectorWidth(Number(raw), window.innerWidth));
      const openRaw = window.localStorage.getItem(INSPECTOR_OPEN_STORAGE_KEY);
      if (openRaw === "0" || openRaw === "false") setInspectorOpen(false);
    } catch {
    }
  }, []);
  useEffect(() => () => inspectorResizeCleanup.current?.(), []);
  useEffect(() => {
    try {
      window.localStorage.setItem(INSPECTOR_WIDTH_STORAGE_KEY, String(inspectorWidth));
    } catch {
    }
  }, [inspectorWidth]);
  useEffect(() => {
    try {
      window.localStorage.setItem(INSPECTOR_OPEN_STORAGE_KEY, inspectorOpen ? "1" : "0");
    } catch {
    }
  }, [inspectorOpen]);

  const bodyRef = useRef<HTMLDivElement>(null);
  const [bodyWidth, setBodyWidth] = useState(0);
  useEffect(() => {
    const el = bodyRef.current;
    if (!el || typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(([entry]) => setBodyWidth(Math.round(entry.contentRect.width)));
    observer.observe(el);
    return () => observer.disconnect();
  }, []);
  const sideOpen = Boolean(sideChat?.open && deck.started);
  const sideMaximized = sideOpen && Boolean(sideChat?.maximized);
  const sideWidth = sideChat?.width ?? 0;
  const [sideView, setSideView] = useState<"main" | "side">("side");
  useEffect(() => { if (sideOpen) setSideView("side"); }, [sideOpen]);
  const inspectorFloats = useMediaQuery("(max-width: 1080px)");
  const inspectorDocked = deck.started && !artifactOpen && !reportOpen && inspectorOpen && !inspectorFloats;
  const inspectorAutoClosed = useRef(false);
  const previousFit = useRef({ open: false, width: 0 });
  useEffect(() => {
    const previous = previousFit.current;
    previousFit.current = { open: sideOpen, width: bodyWidth };
    if (!sideOpen) {
      if (inspectorAutoClosed.current) { inspectorAutoClosed.current = false; setInspectorOpen(true); }
      return;
    }
    if (sideMaximized || !bodyWidth) return;
    const fits = bodyWidth >= RUN_SIDE_MAIN_MIN + sideWidth + inspectorWidth;
    if (inspectorAutoClosed.current && !inspectorOpen && fits && !inspectorFloats) {
      inspectorAutoClosed.current = false;
      setInspectorOpen(true);
      return;
    }
    if (!inspectorDocked) return;
    if ((!previous.open || bodyWidth < previous.width) && !fits) {
      inspectorAutoClosed.current = true;
      setInspectorOpen(false);
    }
  }, [sideOpen, sideMaximized, bodyWidth, inspectorDocked, inspectorOpen, inspectorFloats, sideWidth, inspectorWidth]);
  const inspectorSpace = inspectorDocked ? inspectorWidth : 0;
  const sideNarrow = sideOpen && !sideMaximized && bodyWidth > 0 && bodyWidth - inspectorSpace < RUN_SIDE_MAIN_MIN + RUN_SIDE_WIDTH_MIN;
  const sideMaxWidth = Math.max(RUN_SIDE_WIDTH_MIN, bodyWidth - inspectorSpace - RUN_SIDE_MAIN_MIN);
  const sideFrame = useMemo<RunSideChatFrame>(() => ({ narrow: sideNarrow, view: sideView, setView: setSideView, maxWidth: sideMaxWidth }), [sideNarrow, sideView, sideMaxWidth]);
  const sideClasses = !sideOpen || !sideChat ? "" : [
    "run-side-open",
    `run-side-${sideChat.side}`,
    sideMaximized ? "run-side-max" : "",
    sideNarrow ? (sideView === "side" ? "run-side-only" : "run-side-main-only") : "",
  ].filter(Boolean).join(" ");

  const resizeInspectorTo = useCallback((clientX: number) => {
    const viewport = typeof window !== "undefined" ? window.innerWidth : undefined;
    const next = viewport ? viewport - clientX : INSPECTOR_WIDTH_DEFAULT;
    setInspectorWidth(clampInspectorWidth(next, viewport));
  }, []);

  const startInspectorResize = (e: ReactPointerEvent<HTMLDivElement>) => {
    e.preventDefault();
    e.stopPropagation();
    inspectorResizeCleanup.current?.();
    setInspectorResizing(true);
    document.body.classList.add("inspector-resizing");

    const onMove = (ev: PointerEvent) => {
      ev.preventDefault();
      resizeInspectorTo(ev.clientX);
    };
    const stop = () => {
      setInspectorResizing(false);
      document.body.classList.remove("inspector-resizing");
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", stop);
      window.removeEventListener("pointercancel", stop);
      inspectorResizeCleanup.current = null;
    };

    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", stop);
    window.addEventListener("pointercancel", stop);
    inspectorResizeCleanup.current = stop;
    resizeInspectorTo(e.clientX);
  };

  const onInspectorResizeKey = (e: React.KeyboardEvent<HTMLDivElement>) => {
    const viewport = typeof window !== "undefined" ? window.innerWidth : undefined;
    if (e.key === "ArrowLeft") {
      e.preventDefault();
      setInspectorWidth((w) => clampInspectorWidth(w + (e.shiftKey ? 32 : 12), viewport));
    } else if (e.key === "ArrowRight") {
      e.preventDefault();
      setInspectorWidth((w) => clampInspectorWidth(w - (e.shiftKey ? 32 : 12), viewport));
    } else if (e.key === "Home") {
      e.preventDefault();
      setInspectorWidth(INSPECTOR_WIDTH_MIN);
    } else if (e.key === "End") {
      e.preventDefault();
      setInspectorWidth(inspectorWidthMax(viewport));
    } else if (e.key === "Enter") {
      e.preventDefault();
      setInspectorWidth(clampInspectorWidth(INSPECTOR_WIDTH_DEFAULT, viewport));
    }
  };

  const solvers = Object.entries(deck.lanes).filter(([, lane]) => lane.online).map(([id]) => id);
  const digest = swarmDigest(deck);
  const latestProgress = [...deck.chat].reverse().find(
    (message) => message.kind === "progress" && message.progressBrief,
  )?.progressBrief ?? (deck.finished ? legacyProgressBrief(deck, digest) : undefined);
  const blockingHitlCount = deck.hitlRequests.filter((r) => classifyHitlRequest(r) === "pending").length;
  const onMarkFalseFlag = (flag: string) => onCommand("global", "mark_false", flag);
  const latestControl = deck.controlCommands[deck.controlCommands.length - 1];
  const copyText = useCopyText();
  const triageLabel = operatorLabel ?? deck.operatorLabel ?? null;

  const lastSystem = [...deck.chat].reverse().find((m) => m.role === "system");
  const liveStatus = lastSystem
    ? (workspaceMode === "pentest" && lastSystem.i18nKey === "sys.goalMet"
        && lastSystem.i18nVars?.why === "model_goal_with_evidence"
        ? t("sys.pentestGoalMet")
        : lastSystem.i18nKey ? t(lastSystem.i18nKey, lastSystem.i18nVars) : lastSystem.content)
    : "";
  const showInspector = deck.started && !artifactOpen && !reportOpen && inspectorOpen;
  const pendingPrompt = deck.started ? undefined : pendingDispatchPrompt;
  const openingRun = loading && !pendingPrompt;
  const runHeading = deck.started
    ? deck.challengeName || t("convo.run")
    : openingRun
      ? runTitle || t("convo.run")
      : workspaceMode === "pentest" ? "新测试" : t("convo.newSolve");
  const pentestRuntimeOpen = workspaceMode === "pentest" && artifactOpen && !reportOpen;
  const inspectorToggleLabel = t(inspectorOpen ? "insp.run.hide" : "insp.run.show");
  const writeupGenerating = deck.followupPending && (
    deck.activeFollowupKind === "writeup"
    || deck.chat.some((m) => m.followupId === deck.activeFollowupJobId && m.followupKind === "writeup")
  );
  const reportGenerating = Boolean(deck.reportGenerating);
  const productionGuard = deck.productionGuard;
  const productionGuardProtected = workspaceMode === "pentest" && Boolean(productionGuard?.enabled);
  const guardPendingCount = productionGuardProtected
    ? (productionGuard?.pending_approvals.filter((item) => item.status === "pending" || item.status === "resolving").length ?? 0)
    : 0;
  const guardEmergencyStopping = Boolean(
    productionGuardProtected
    && (productionGuard?.stop_status === "emergency_stopping" || productionGuard?.emergency_stop_receipt?.status === "emergency_stopping"),
  );
  const protectedActive = productionGuardProtected && !deck.finished && (running || guardPendingCount > 0);
  const handleEmergencyStop = useCallback(async (): Promise<boolean> => {
    if (onEmergencyStop) return onEmergencyStop();
    return onCommand("global", "emergency_stop", "", { expectedGeneration: deck.controlGeneration });
  }, [onEmergencyStop, onCommand, deck.controlGeneration]);
  const handleDecideGuardApproval = useCallback(async (
    approvalId: string,
    action: GuardApprovalDecisionAction,
    note?: string,
  ): Promise<boolean> => {
    if (onDecideGuardApproval) return onDecideGuardApproval(approvalId, action, note);
    return false;
  }, [onDecideGuardApproval]);
  const composerElement = <Composer
    runId={deck.runId}
    key={deck.started ? `run:${deck.runId}` : `draft:${composerDraftId}`}
    workspaceMode={workspaceMode}
    driverKind={deck.driverKind}
    localPiSession={deck.localPiSession}
    started={deck.started}
    dispatchPending={Boolean(pendingPrompt)}
    solved={deck.solved}
    running={running}
    finished={deck.finished}
    followupPending={deck.followupPending}
    writeupGenerating={writeupGenerating}
    reportGenerating={reportGenerating}
    paused={digest.phase === "paused"}
    solvers={solvers}
    flags={deck.flags}
    onDispatch={onDispatch}
    onCommand={onCommand}
    onRequestProgress={onRequestProgress}
    onResolve={onResolve}
    onFollowup={onFollowup} onCancelFollowup={onCancelFollowup} onOpenWriteup={onOpenWriteup}
    hasWriteup={deck.hasWriteup} activeFollowupJobId={deck.activeFollowupJobId}
    attachments={attachments}
    onAddFiles={onAddFiles}
    onRemoveFile={onRemoveFile}
    prefill={composerPrefill}
    onPrefillConsumed={consumeComposerPrefill}
    onOpenSideChat={onOpenSideChat}
    onOpenReport={onOpenReport}
    receipt={latestControl}
    compact={composerCompact}
    onRequestExpand={expandComposer}
    productionGuardProtected={productionGuardProtected}
    guardPendingCount={guardPendingCount}
    guardEmergencyStopping={guardEmergencyStopping}
    onEmergencyStop={handleEmergencyStop}
    onProductionGuardChange={setDraftProtectedMode}
    controlledText={workspaceMode === "pentest" ? pentestDraftText : undefined}
    onControlledTextChange={workspaceMode === "pentest" ? setPentestDraftText : undefined}
    dispatchHeader={!deck.started && !loading
      ? <WelcomeExamples t={t} onChoose={setComposerPrefill} workspaceMode={workspaceMode} />
      : null}
  />;

  return (
    <div
      className={`convo ${showInspector ? "has-inspector" : ""} ${artifactOpen && !reportOpen ? "runtime-peer-open" : ""} ${inspectorResizing ? "inspector-resizing" : ""} ${sideClasses}`}
      style={{ "--inspector-width": `${inspectorWidth}px`, "--run-side-width": `${Math.min(sideWidth, sideMaxWidth)}px` } as CSSProperties}
    >
      <div className="sr-only" role="status" aria-live="polite" aria-atomic="true" aria-label={t("a11y.status")}>{liveStatus}</div>
      <div className="convo-top">
        <Button
          size="sm"
          variant="ghost"
          iconOnly
          className="icon-btn"
          onClick={onToggleRail}
          aria-label={workspaceMode === "pentest" ? "切换测试列表" : t("convo.toggleRuns")}
        >
          <Icon name="menu" size={15} />
        </Button>
        <div className="convo-top-context">
          <span
            className="title"
            data-tooltip={runHeading}
            title={runHeading}
          >
            {runHeading}
          </span>
          {deck.started || workspaceMode === "pentest" ? (
            <div className="convo-top-meta">
              {deck.started && deck.category && workspaceMode === "ctf" && (
                <RunCategoryTag category={deck.category} />
              )}
              {deck.started && triageLabel && (
                <span className="rv-triage-tag">{t(`rv.triage.${triageLabel}`)}</span>
              )}
            </div>
          ) : null}
        </div>
        {deck.started && (
          <div className="convo-top-view">
            <Tabs
              value={reportOpen ? "report" : artifactOpen ? "runtime" : "conversation"}
              variant="segmented"
              size="sm"
              onChange={(key: string) => {
                if (key === "runtime") onOpenArtifact(artifactView);
                else if (key === "collaboration") onOpenArtifact("collaboration");
                else if (key === "report") onOpenReport?.();
                else onShowConversation();
              }}
            >
              <TabsList className="convo-view-switch" ariaLabel={t("convo.viewSwitcher")}>
                <TabsTrigger value="conversation" icon="rows">
                  <span>{t("convo.viewConversation")}</span>
                </TabsTrigger>
                <TabsTrigger value="runtime" icon="panel">
                  <span>{t("convo.viewRuntime")}</span>
                </TabsTrigger>
                <TabsTrigger value="collaboration" disabled={deck.driverKind === "local_pi"} icon="network">
                  <span>{t("convo.viewCollaboration")}</span>
                </TabsTrigger>
                {workspaceMode === "pentest" && <TabsTrigger value="report" icon="rows">
                  <span>报告</span>
                </TabsTrigger>}
              </TabsList>
            </Tabs>
          </div>
        )}
        <div className="convo-top-controls">
          {!deck.started && !loading && <EngineBar deck={deck} protectedMode={workspaceMode === "pentest" && draftProtectedMode} />}
          {protectedActive ? (
            <Button
              size="xs"
              variant="danger"
              icon="stopCircle"
              loading={topEmergencyStopBusy || guardEmergencyStopping}
              disabled={topEmergencyStopBusy || guardEmergencyStopping}
              data-testid="topbar-emergency-stop"
              onClick={() => setTopEmergencyStopOpen(true)}
            >
              紧急停止 Run
            </Button>
          ) : null}
          <RunStatePill deck={deck} running={running} connected={connected} />
          <div className="convo-top-actions">
            <RunHeaderMenu
              t={t}
              runId={deck.runId}
              started={deck.started}
              theme={theme}
              operatorLabel={triageLabel}
              onUpdateOperatorLabel={onUpdateOperatorLabel}
              onOpenWorkspace={onOpenWorkspace}
              onToggleTheme={onToggleTheme}
              onToggleLang={() => setLang(lang === "zh" ? "en" : "zh")}
              copyText={copyText}
            />
            {deck.started && !artifactOpen && !reportOpen && (
              <Button
                size="sm"
                variant="ghost"
                iconOnly
                className="icon-btn convo-inspector-toggle"
                onClick={() => setInspectorOpen((open) => !open)}
                aria-label={inspectorToggleLabel}
                aria-pressed={inspectorOpen}
                data-active={inspectorOpen ? "true" : "false"}
                data-tooltip={inspectorToggleLabel}
              >
                <Icon name={inspectorOpen ? "panelRightClose" : "panelRightOpen"} size={15} />
              </Button>
            )}
          </div>
        </div>
      </div>

      {sideNarrow ? (
        <div className="run-side-switch">
          <SegmentedControl value={sideView} onChange={setSideView} ariaLabel="切换主对话与侧边聊天"
            options={[{ value: "main", label: "主对话" }, { value: "side", label: "侧边聊天" }]} />
        </div>
      ) : null}
      <div className="convo-body" ref={bodyRef}>
        <div className="convo-mainpane">
          {reportOpen && reportPanel ? reportPanel : <>
          <div
            className={`convo-scroll ${deck.started ? "has-workspace" : ""}`}
            ref={scrollRef}
            onScroll={(e) => {
              const el = e.currentTarget;
              stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 70;
              const previousTop = lastScrollTop.current;
              lastScrollTop.current = el.scrollTop;
              if (!deck.started) return;
              syncComposerFade(el);
              const distance = el.scrollHeight - el.scrollTop - el.clientHeight;
              if (!composerCompactRef.current && el.scrollTop < previousTop - 1 && distance > COMPOSER_COMPACT_DISTANCE_PX) setComposerCompact(true);
              else if (composerCompactRef.current && el.scrollTop > previousTop && distance <= COMPOSER_EXPAND_DISTANCE_PX) setComposerCompact(false);
            }}
          >
            {pendingPrompt ? (
              <div className="workspace">
                <div className="coord-col">
                  <div className="coord-thread">
                    <div className="coord-wrap">
                      <CoordBubble
                        m={{ id: "pending-dispatch", role: "human", kind: "text", content: pendingPrompt, ts: 0 }}
                        t={t}
                        mode={workspaceMode}
                      />
                      <div className="mt-3 flex items-center gap-2 text-cx-sm text-cx-fg-3" role="status">
                        <Spinner size={14} className="text-cx-accent" />
                        {t(dispatchAccepted ? "convo.dispatchAccepted" : "convo.dispatching")}
                      </div>
                    </div>
                  </div>
                </div>
              </div>
            ) : !deck.started && !loading ? (
              <Welcome t={t} workspaceMode={workspaceMode} />
            ) : (
              <div className={`workspace ${artifactOpen ? "solo" : ""}`}>
                <div className="coord-col t-page-slide">
                  {deck.started && running && !connected && (
                    <div className="conn-banner" role="status" aria-live="polite">
                      <span className="cb-ico" aria-hidden="true"><Icon name="plug" size={14} /></span>
                      <span className="cb-msg">{t("convo.streamLost")}</span>
                    </div>
                  )}
                  {deck.driverKind === "local_pi" ? (
                    deck.flags.length > 0 && (
                      <div className="coord-sticky-head">
                        <StatusHero digest={digest} hitlCount={blockingHitlCount} progress={latestProgress} t={t} />
                      </div>
                    )
                  ) : openingRun ? null : (
                    <div className="coord-sticky-head">
                      <StatusHero digest={digest} hitlCount={blockingHitlCount} progress={latestProgress} t={t} />
                      {deck.started && workspaceMode !== "pentest" && (
                        <RunSignalsStrip
                          deck={deck}
                          onOpenArtifact={onOpenArtifact}
                          onOpenKnowledge={onOpenKnowledge}
                        />
                      )}
                    </div>
                  )}
                  {!loading && productionGuardProtected ? (
                    <PentestGuardApprovalQueue
                      guard={productionGuard}
                      onDecideApproval={handleDecideGuardApproval}
                      onEmergencyStop={handleEmergencyStop}
                    />
                  ) : null}
                  {loading ? (
                    <div className="coord-thread">
                      <div className={runNotFound ? "coord-wrap coord-wrap--nf" : "coord-wrap"}>
                      <div className="coord-loading">
                        {runNotFound ? (
                          <>
                            <span className="cl-title">{t("loading.notFound")}</span>
                            <span className="cl-hint"><code>{deck.runId}</code> — {t("loading.notFoundHint")}</span>
                            <Link className="coord-nf-back" href={workspaceMode === "pentest" ? "/pentest" : "/ctf"}>{t("loading.backToList")}</Link>
                          </>
                        ) : (<>
                          <Spinner size={14} className="text-cx-accent" />
                          <span className="cl-title">{workspaceMode === "pentest" ? "正在加载测试" : t("loading.run")}</span>
                          <span className="cl-hint">{workspaceMode === "pentest" ? "正在恢复本次测试的实时记录。" : t("loading.runHint")}</span>
                          </>)}
                        </div>
                        {!runNotFound && <div className="coord-skel" aria-hidden="true">
                          {[68, 82, 54].map((w, i) => (
                            <div className="coord-skel-bubble" key={i}>
                              <Skeleton className="skel-line t-skeleton" style={{ width: `${w}%`, height: 10 }} />
                              <Skeleton className="skel-line t-skeleton" style={{ width: `${w - 16}%`, height: 10 }} />
                            </div>
                          ))}
                        </div>}
                      </div>
                    </div>
                  ) : deck.driverKind === "local_pi" ? (
                    <>
                    {(chatThreadId || deck.chatThreadId) && <CtfPiChat key={chatThreadId || deck.chatThreadId} threadId={(chatThreadId || deck.chatThreadId)!} />}
                    <LocalPiChatThread onCancelFollowup={onCancelFollowup} onExpireDirective={onExpireDirective} onOpenWriteup={onOpenWriteup} onOpenReport={onOpenReport}
                      deck={deck}
                      running={running}
                      onAnswer={async (requestId, opt, commandId) => {
                        const ok = await onCommand(
                          "global", "answer_decision", opt, { requestId, commandId });
                        if (ok) onHitlAnswered?.();
                        return ok;
                      }}
                      onDismiss={async (requestId, commandId) => {
                        const ok = await onCommand(
                          "global", "dismiss", "", { requestId, commandId });
                        if (ok) onHitlAnswered?.();
                        return ok;
                      }}
                      onOpenControlHistory={() => onOpenArtifact("timeline")}
                    />
                    </>
                  ) : (
                    <CoordinatorThread onCancelFollowup={onCancelFollowup} onExpireDirective={onExpireDirective} onOpenWriteup={onOpenWriteup} onOpenReport={onOpenReport}
                      deck={deck}
                      running={running}
                      onAnswer={async (requestId, opt, commandId) => {
                        const ok = await onCommand(
                          "global", "answer_decision", opt, { requestId, commandId });
                        if (ok) onHitlAnswered?.();
                        return ok;
                      }}
                      onDismiss={async (requestId, commandId) => {
                        const ok = await onCommand(
                          "global", "dismiss", "", { requestId, commandId });
                        if (ok) onHitlAnswered?.();
                        return ok;
                      }}
                      onOpenControlHistory={() => onOpenArtifact("timeline")}
                    />
                  )}
                </div>
              </div>
            )}
          </div>

          {!pentestRuntimeOpen && !openingRun && (deck.started
            ? <div className="run-composer-dock" data-fade={composerFade ? "" : undefined}>{composerElement}</div>
            : composerElement)}
          </>}
        </div>

        {sideOpen && sideChat ? (
          <section className="run-side-slot" aria-label="侧边聊天">
            <RunSideChatFrameContext.Provider value={sideFrame}>{sideChat.node}</RunSideChatFrameContext.Provider>
          </section>
        ) : null}
        {deck.started && sideChat ? <RunSelectionAsk containerRef={scrollRef} onAsk={sideChat.onAskSelection} /> : null}

        {showInspector && (
          <div className="inspector-shell conversation-inspector-shell">
            <div
              className="inspector-resizer"
              role="separator"
              tabIndex={0}
              aria-label={t("insp.run.resize")}
              data-tooltip={t("insp.run.resize")}
              aria-orientation="vertical"
              aria-valuemin={INSPECTOR_WIDTH_MIN}
              aria-valuemax={INSPECTOR_WIDTH_MAX}
              aria-valuenow={inspectorWidth}
              onPointerDown={startInspectorResize}
              onKeyDown={onInspectorResizeKey}
              onDoubleClick={() => setInspectorWidth(clampInspectorWidth(INSPECTOR_WIDTH_DEFAULT, window.innerWidth))}
            />
            {loading ? (
              <aside className="run-inspector t-page-slide" aria-label={t("insp.run.title")} aria-busy="true">
                  <div className="insp-skel" aria-hidden="true">
                    <div className="insp-skel-sec">
                      <Skeleton className="skel-line t-skeleton" style={{ width: 56, height: 9, marginBottom: 4 }} />
                      <Skeleton className="skel-box t-skeleton" style={{ width: "100%", height: 42, borderRadius: 9 }} />
                      <div className="insp-skel-chips">
                        {Array.from({ length: 5 }).map((_, index) => <Skeleton key={index} className="skel-box t-skeleton" style={{ width: 62 + (index % 3) * 16, height: 22, borderRadius: 999 }} />)}
                      </div>
                    </div>
                    <div className="insp-skel-sec">
                      <Skeleton className="skel-line t-skeleton" style={{ width: 70, height: 9, marginBottom: 4 }} />
                      {Array.from({ length: 3 }).map((_, index) => (
                        <div className="insp-skel-row" key={index}>
                          <Skeleton className="skel-box t-skeleton" style={{ width: 28, height: 28, borderRadius: 7 }} />
                          <div className="insp-skel-row-meta">
                            <Skeleton className="skel-line t-skeleton" style={{ width: `${64 - index * 8}%`, height: 10 }} />
                            <Skeleton className="skel-line t-skeleton" style={{ width: `${40 + index * 6}%`, height: 8 }} />
                          </div>
                        </div>
                      ))}
                    </div>
                  </div>
              </aside>
            ) : (
              <RunInspector
                deck={deck}
                onOpenReport={onOpenReport}
                onOpenWriteup={onOpenWriteup ? () => onOpenWriteup() : undefined} onToggleFreezeWorker={onToggleFreezeWorker}
                running={running}
                onOpenArtifact={onOpenArtifact}
                onSpawnWorker={onSpawnWorker}
                onKillWorker={onKillWorker}
                onOpenWorker={onOpenWorker}
                onOpenAgent={onOpenAgent}
                onMarkFalseFlag={onMarkFalseFlag}
                onClose={() => setInspectorOpen(false)}
              />
            )}
          </div>
        )}

        <div className="convo-runtime-peer">{runtimePanel}</div>
      </div>
      <EmergencyStopConfirmDialog
        open={topEmergencyStopOpen}
        busy={topEmergencyStopBusy}
        onOpenChange={setTopEmergencyStopOpen}
        onConfirm={async () => {
          if (topEmergencyStopBusy) return;
          setTopEmergencyStopBusy(true);
          try {
            await handleEmergencyStop();
          } finally {
            setTopEmergencyStopBusy(false);
            setTopEmergencyStopOpen(false);
          }
        }}
      />
    </div>
  );
}
