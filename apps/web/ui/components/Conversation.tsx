"use client";

import { MotionIcon } from "@/components/MotionIcon";

import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import type { CSSProperties, PointerEvent as ReactPointerEvent, ReactNode } from "react";
import {
  ChatMessage, DeckState, HitlRequest, ProgressBrief, ProgressBriefItem, SwarmDigest,
  coordinatorThread, hitlDeliveryState, swarmDigest,
} from "@/lib/events";
import { getWorkerSettings, checkAuth, SavedFile } from "@/lib/useRun";
import {
  commandIdForDecision, type DecisionControlAction,
} from "@/lib/controlClient";
import { useT, useLang } from "@/lib/i18n";
import { formatClock, formatElapsed, toEpochMs } from "@/lib/format";
import { EngineBar } from "@/components/EngineBar";
import { Icon, type IconName } from "@/components/Icon";
import { RunInspector } from "@/components/RunInspector";
import { RunSignalsStrip } from "@/components/RunSignalsStrip";
import { NumberField } from "@/components/NumberField";
import { useCopied } from "@/lib/useCopied";
import { Accordion, Button, Chip, Input, Label, ListBox, ListBoxItem, Popover, Select, Skeleton, Spinner, Tabs, TextArea } from "@heroui/react";
import type { ArtifactView } from "@/lib/events";

/**
 * The conversation spine (the redesign's centre column): a ChatGPT/Claude-style
 * thread between the operator and the COORDINATOR (DeepSeek `reason`). It owns
 * the welcome/dispatch state and the dual-mode composer. The worker firehose,
 * fact-graph and blackboard live in the collapsible right-column RunInspector
 * and the peer runtime workspace. The deck stays a dumb subscriber.
 *
 * i18n: static UI is translated; agent-produced text renders verbatim. Only
 * system lifecycle lines + the synthesized progress/answer turns carry keys.
 */

export interface DispatchOpts {
  webSearch: boolean;
  mode: "ctf" | "pentest";
  goal?: string;
  scope?: string;
  // collect mode only controls multi-flag collection. Flag format is independent:
  // default brace regex, or explicit token mode for bare-password ladders.
  collect?: boolean;
  flagFormat?: "brace" | "token" | "custom";
  flagWrapper?: string;
  // optional flag count for collect mode: >0 → stop after collecting that many
  // distinct flags; blank/0 → unknown count, collect until the operator stops.
  collectCount?: number;
  reportGoalMode?: "automatic" | "count";
  expectedFindings?: number;
  // worker isolation: when true, the run uses a controlled Docker runtime that
  // can't read the host challenge-source tree. Default false = host subprocess.
  containerMode?: boolean;
  allowOperatorInput: boolean;
  raceTimeout?: number;
  wallClockBudget?: number;
  maxTotalWorkers?: number;
  costBudgetUsd?: number;
  raceEngines?: string[];
}

export interface ControlCommandOpts {
  requestId?: string;
  commandId?: string;
}

// RUNNING — steer the live swarm:
const QUICK_RUNNING: Array<{ key: string; labelKey: string; tipKey: string; icon: IconName }> = [
  { key: "directive", labelKey: "quick.directive", tipKey: "quick.directive.tip", icon: "target" },
  { key: "pause", labelKey: "quick.pause", tipKey: "quick.pause.tip", icon: "pause" },
  { key: "freeze", labelKey: "quick.freeze", tipKey: "quick.freeze.tip", icon: "lock" },
  { key: "thaw", labelKey: "quick.thaw", tipKey: "quick.thaw.tip", icon: "play" },
];
// max height the dispatch textarea auto-grows to (~6–7 rows) before it scrolls
// internally; mirrored by `.composer2 textarea { max-height }` in globals.css.
const DISPATCH_MAX_H = 180;
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

function fmtSize(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(0)} KB`;
  return `${(n / 1024 / 1024).toFixed(1)} MB`;
}

function getCategoryColor(category?: string): "default" | "accent" | "warning" | "danger" | "success" {
  if (!category) return "default";
  const cat = category.toLowerCase().trim();
  if (["web", "websec", "api", "cloud"].includes(cat)) return "accent";
  if (["pwn", "binary", "exp", "exploit", "rev", "reverse"].includes(cat)) return "danger";
  if (["crypto", "cryptography", "math", "algo"].includes(cat)) return "warning";
  if (["forensics", "stego", "defense", "incident"].includes(cat)) return "success";
  if (["misc", "ai", "hardware", "iot", "blockchain", "pentest"].includes(cat)) return "accent";
  return "default";
}

/** Live run duration: ticks every second while the run is open, freezes at
 *  finishedAt − startedAt once it ends. "" when the run hasn't started. */
function useElapsed(startedAt?: number, finishedAt?: number, freeze = false): string {
  const [now, setNow] = useState(() => Date.now());
  const freezeRef = useRef<number | undefined>(undefined);
  if (freeze && freezeRef.current == null) freezeRef.current = now;
  if (!freeze) freezeRef.current = undefined;
  const live = startedAt != null && finishedAt == null && !freeze;
  useEffect(() => {
    if (!live) return;
    const id = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(id);
  }, [live]);
  if (startedAt == null) return "";
  const end = finishedAt != null ? toEpochMs(finishedAt) : (freezeRef.current ?? now);
  return formatElapsed(end - toEpochMs(startedAt));
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
  { key: "race", icon: "target", phases: ["racing"] },
  { key: "reason", icon: "cpu", phases: ["running", "paused"] },
  { key: "explore", icon: "terminal", phases: ["running", "collecting"] },
  { key: "collect", icon: "flag", phases: ["collecting", "solved"] },
  { key: "finish", icon: "check", phases: ["solved", "goal_met", "finished"] },
  { key: "respond", icon: "pencil", phases: ["solved", "goal_met", "finished"] },
];

function activeFlowIndex(digest: SwarmDigest): number {
  if (digest.phase === "draft") return 0;
  if (digest.phase === "racing") return 1;
  if (digest.phase === "collecting") return 4;
  if (digest.phase === "solved" || digest.phase === "goal_met" || digest.phase === "finished") return 5;
  return 2;
}

// ── coordinator bubble (human · system · coordinator/reason) ─────────────────
function CoordBubble({
  m,
  t,
  mode,
}: {
  m: ChatMessage;
  t: (k: string, vars?: Record<string, string | number>) => string;
  mode: "ctf" | "pentest";
}) {
  const [copied, copy] = useCopied();
  const text = mode === "pentest" && m.i18nKey === "sys.goalMet"
    && m.i18nVars?.why === "model_goal_with_evidence"
    ? t("sys.pentestGoalMet")
    : m.i18nKey ? t(m.i18nKey, m.i18nVars) : m.content;
  const standbyReply = m.role === "agent" && m.solverId?.endsWith("-standby");
  const who = m.role === "human" ? t("coord.you") : m.role === "system"
    ? (m.kind === "insight" ? "insight" : "system")
    : standbyReply ? t("coord.solveWorker") : t("coord.title");
  const cls = m.role === "human" ? "you" : m.role === "system" ? `system ${m.kind}` : `coordinator ${m.kind}`;
  // long coordinator reasoning folds to keep the thread scannable
  const isLong = m.role === "agent" && m.kind === "reasoning" && text.length > 520;
  // Coordinator-produced prose (text/reasoning) is what a generated writeup lands
  // as — there is no distinct "writeup" message kind, so any substantive agent
  // bubble gets a copy button. Copying the whole report into a ticket/writeup is
  // the natural terminal action (mirrors flag-copy). Skip terse/empty bubbles and
  // the operator's own + system lifecycle lines.
  const copyable = m.role === "agent" && (m.kind === "text" || m.kind === "reasoning") && text.trim().length > 40;
  const nodeIcon: IconName = m.role === "human" ? "send"
    : m.role === "agent" ? "cpu"
      : m.kind === "insight" ? "pencil" : "clock";
  return (
    <div className={`coord-bubble ${cls}`} data-mid={m.id}>
      <span className="coord-node" aria-hidden="true"><Icon name={nodeIcon} size={12} /></span>
      <div className="who">
        {who} <span className="k">{t(`msg.kind.${m.kind}`)}</span>
        {copyable && (
          <Button
            size="sm"
            variant="ghost"
            className={`coord-copy ${copied ? "copied" : ""}`}
            onPress={() => copy(text)}
            aria-label={t("coord.copyMsgAria")}
          >
            <MotionIcon active={copied} from="copy" to="check" size={12} />
            <span className="cc-lbl">{copied ? t("common.copied") : t("common.copyShort")}</span>
          </Button>
        )}
        {clock(m.ts) && <span className="ts">{clock(m.ts)}</span>}
      </div>
      {isLong ? (
        <details className="coord-fold">
          <summary>{text.slice(0, 240).trimEnd()}…</summary>
          <div className="body">{text}</div>
        </details>
      ) : (
        <div className="body">{text}</div>
      )}
    </div>
  );
}

function DigestBubble({ digest, t }: { digest: SwarmDigest; t: (k: string, v?: Record<string, string | number>) => string }) {
  return (
    <div className="coord-bubble digest">
      <span className="coord-node" aria-hidden="true"><Icon name="radio" size={12} /></span>
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
}: {
  brief: ProgressBrief;
  ts: number;
  t: (k: string, v?: Record<string, string | number>) => string;
}) {
  const sections: Array<{ key: keyof ProgressBrief["sections"]; icon: IconName }> = [
    { key: "confirmed", icon: "check" },
    { key: "active", icon: "radio" },
    { key: "blocked", icon: "xCircle" },
    { key: "next", icon: "target" },
  ];
  const visible = sections.filter(({ key }) => brief.sections[key].length > 0);
  const detailCount = visible.reduce((count, { key }) => count + brief.sections[key].length, 0);
  return (
    <section
      className={`coord-bubble progress-brief kind-${brief.kind}`}
      aria-label={t("progress.title")}
      data-brief-id={brief.id}
    >
      <span className="coord-node" aria-hidden="true">
        <Icon name={brief.kind === "blocker" || brief.kind === "stalled" ? "alert" : "radio"} size={12} />
      </span>
      <header className="progress-brief-head">
        <span>{t("coord.title")} <span className="k">{t("msg.kind.progress")}</span></span>
        <span className="progress-brief-kind">{t(`progress.kind.${brief.kind}`)}</span>
        {clock(ts) && <time>{clock(ts)}</time>}
      </header>
      <div className="progress-brief-summary">
        {brief.summaryKey
          ? t(brief.summaryKey, brief.summaryVars)
          : brief.summary || t(`progress.headline.${brief.kind}`)}
      </div>
      {visible.length > 0 && (
        <Accordion hideSeparator className="progress-brief-details">
          <Accordion.Item id={`progress-details-${brief.id}`}>
            <Accordion.Heading>
              <Accordion.Trigger>
                <span>{t("progress.details", { count: detailCount })}</span>
                <Accordion.Indicator />
              </Accordion.Trigger>
            </Accordion.Heading>
            <Accordion.Panel>
              <Accordion.Body className="progress-brief-details-body">
                <div className="progress-brief-sections">
                  {visible.map(({ key, icon }) => (
                    <section className={`progress-brief-section section-${key}`} key={key}>
                      <h3><Icon name={icon} size={12} /> {t(`progress.section.${key}`)}</h3>
                      <ul>
                        {brief.sections[key].map((item, index) => (
                          <li key={`${key}-${item.ref?.kind ?? "item"}-${item.ref?.id ?? index}`}>
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
              </Accordion.Body>
            </Accordion.Panel>
          </Accordion.Item>
        </Accordion>
      )}
    </section>
  );
}

function AnswerBubble({ digest, t }: { digest: SwarmDigest; t: (k: string, v?: Record<string, string | number>) => string }) {
  const none = digest.flags.length === 0 && digest.phase !== "goal_met";
  const multi = digest.expectedFlags > 1;
  return (
    <div className={`coord-bubble answer ${none ? "none" : ""}`}>
      <span className="coord-node" aria-hidden="true"><Icon name={none ? "clock" : "check"} size={12} /></span>
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
              <Button size="sm" variant="outline" className="copytext ans-flag" aria-label={t("common.copyFlag")} onPress={() => void navigator.clipboard.writeText(f)}>{t("coord.answerFlag", { flag: f })}</Button>
            </div>
          ))}
        </div>
      ) : digest.phase === "goal_met" && digest.mode === "pentest" ? (
        <div className="body">{t("coord.pentestAnswer")}</div>
      ) : digest.phase === "goal_met" ? (
        <div className="body">
          <Button size="sm" variant="outline" className="copytext ans-flag goal" aria-label={t("common.copyAnswer")} onPress={() => void navigator.clipboard.writeText(digest.goalWhy || "")}>
            {t("coord.answerGoal", { why: digest.goalWhy || "" })}
          </Button>
        </div>
      ) : (
        <div className="body">{t("coord.answerNoneCtf", { facts: digest.verified })}</div>
      )}
    </div>
  );
}

// Maps each digest phase to the status-hero glyph. Keeps the visual vocabulary
// consistent with the rest of the deck (same Icon set).
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

/** Always-visible run-status summary at the top of the coordinator column.
 *  It keeps the current phase and timing compact while exposing every accepted
 *  flag through an expandable result ledger. Reads only existing `swarmDigest`
 *  fields; the live pulse is gated on prefers-reduced-motion in CSS. */
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
        <Button size="sm" variant="ghost" isIconOnly className="flow-close" onPress={onClose} aria-label={t("settings.close")}><Icon name="x" size={14} /></Button>
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
              steps: digest.openSteps,
              goals: digest.goals,
              hitl: hitlCount,
            })}</span>
      </div>
    </div>
  );
}

function StatusHero({ digest, hitlCount, progress, t }: { digest: SwarmDigest; hitlCount: number; progress?: ProgressBrief; t: (k: string, v?: Record<string, string | number>) => string }) {
  const [flowOpen, setFlowOpen] = useState(false);
  const [resultsOpen, setResultsOpen] = useState(false);
  const elapsed = useElapsed(
    digest.startedAt,
    digest.finishedAt,
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
  // the one detail line that matters most for THIS phase.
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
    detail = t("hero.detail.finishedCtf", { facts: digest.verified, steps: digest.openSteps });
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
              variant="ghost"
              className="sh-results-toggle"
              aria-expanded={resultsVisible}
              aria-controls="status-flag-results"
              onPress={toggleResults}
              aria-label={t(resultsVisible ? "hero.results.collapse" : "hero.results.expand")}
            >
              <span className="sh-results-count">{resultCount}</span>
              {digest.flags[0] && <code className="sh-results-preview">{digest.flags[0]}</code>}
              {digest.flags.length > 1 && <span className="sh-results-more">+{digest.flags.length - 1}</span>}
              <Icon name="chevronDown" size={13} />
            </Button>
          ) : singleFlag ? (
            <Button size="sm" variant="outline" className="copytext sh-detail sh-detail-flag" aria-label={t("common.copyFlag")} onPress={() => void navigator.clipboard.writeText(singleFlag)}>{singleFlag}</Button>
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
          {digest.mode !== "pentest" && <Popover isOpen={flowOpen} onOpenChange={(isOpen) => { setFlowOpen(isOpen); if (isOpen) setResultsOpen(false); }}>
          <Popover.Trigger
            className="sh-flow"
            aria-label={t("flow.open")}
          >
            <Icon name="list" size={12} /> {t("flow.short")}
          </Popover.Trigger>
          <Popover.Content placement="bottom end" className="p-0"><Popover.Dialog aria-label={t("flow.title")}><FlowPopover digest={digest} hitlCount={hitlCount} onClose={() => setFlowOpen(false)} t={t} /></Popover.Dialog></Popover.Content>
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
                  <Button size="sm" variant="outline" className="copytext sh-result-value" aria-label={t("common.copyFlag")} onPress={() => void navigator.clipboard.writeText(flag)}>{flag}</Button>
                </div>
              ))}
          </div>
        </section>
      )}
    </div>
  );
}

/** A pending human-in-the-loop decision. Because a pending request PAUSES the
 *  whole swarm, the card is rendered high-priority (amber bar + alert icon +
 *  "needs your decision" heading). When the request carries `options`, each is a
 *  one-click answer button; the free-text input is always available for a custom
 *  answer (Enter submits). The FIRST pending card autofocuses its input so the
 *  operator can just type + Enter. Admission locks the answer exactly once; the
 *  correlated durable control receipt then renders pending/recovery state until
 *  EFFECT_OBSERVED closes the card. */
function HitlCard({
  req, first, onAnswer, onDismiss,
}: {
  req: HitlRequest;
  first: boolean;
  onAnswer: (requestId: string, opt: string, commandId: string) => Promise<boolean>;
  onDismiss?: (requestId: string, commandId: string) => Promise<boolean>;
}) {
  const t = useT();
  const [free, setFree] = useState("");
  const [sending, setSending] = useState(false);
  const [locallyRecorded, setLocallyRecorded] = useState(false);
  const commandIdsRef = useRef<Partial<Record<DecisionControlAction, string>>>({});
  const attemptRef = useRef<{
    action: DecisionControlAction;
    value: string;
    commandId: string;
  } | null>(null);
  const inputRef = useRef<HTMLInputElement>(null);
  const hasOptions = req.options.length > 0;
  // F: only an external_blocker actually freezes the swarm + needs an operator
  // answer; the other kinds are auto-handled (lane lock / route suppress / low-conf
  // candidate). An unclassified card (no needKind) defaults to blocking (back-compat).
  const pauses = req.pausesBehavior ?? true;
  const kindLabel = req.needKind ? t(`hitl.kind.${req.needKind}`) : t("hitl.title");
  const durableDelivery = hitlDeliveryState(req);
  // Lock immediately when POST /control succeeds; the SSE projection replaces
  // this local bridge as soon as PERSISTED/terminal lifecycle events arrive.
  const answerRecorded = locallyRecorded || durableDelivery.locked;
  const deliveryPhase = durableDelivery.locked ? durableDelivery.phase
    : locallyRecorded ? "pending" : "open";
  // autofocus the topmost pending request's input — when the current first card
  // is answered and clears, the next one becomes `first` and grabs focus.
  useEffect(() => {
    if (first && pauses) inputRef.current?.focus();
  }, [first, pauses]);
  useEffect(() => {
    if (durableDelivery.locked) {
      setLocallyRecorded(true);
      setSending(false);
    }
  }, [durableDelivery.locked]);
  const submit = async (value: string) => {
    const v = value.trim();
    if (!v || sending || answerRecorded) return;
    const commandId = commandIdForDecision(commandIdsRef.current, "answer_decision");
    attemptRef.current = { action: "answer_decision", value: v, commandId };
    setSending(true);
    const ok = await onAnswer(req.id, v, commandId);
    setSending(false);
    if (ok) setLocallyRecorded(true);
  };
  const dismiss = async () => {
    if (!onDismiss || sending || answerRecorded) return;
    const commandId = commandIdForDecision(commandIdsRef.current, "dismiss");
    attemptRef.current = { action: "dismiss", value: "", commandId };
    setSending(true);
    const ok = await onDismiss(req.id, commandId);
    setSending(false);
    if (ok) setLocallyRecorded(true);
  };
  const retryDelivery = async () => {
    const attempt = attemptRef.current;
    if (!attempt || sending) return;
    setSending(true);
    const ok = attempt.action === "answer_decision"
      ? await onAnswer(req.id, attempt.value, attempt.commandId)
      : await onDismiss?.(req.id, attempt.commandId) ?? false;
    setSending(false);
    if (ok) setLocallyRecorded(true);
  };
  const deliveryKey = deliveryPhase === "open"
    ? undefined : `hitl.delivery.${deliveryPhase}`;
  const retryable = answerRecorded && deliveryPhase !== "observed"
    && attemptRef.current !== null;
  const activeCommandId = durableDelivery.commandId
    ?? attemptRef.current?.commandId;
  return (
    <div
      className={`hitl-card ${first ? "first" : ""} ${sending ? "sending" : ""} ${answerRecorded ? "answered-readonly" : ""} delivery-${deliveryPhase} ${pauses ? "blocking" : "auto"}`}
      role="group"
      aria-label={t("hitl.region")}
    >
      <div className="hitl-head">
        <span className="hitl-ico" aria-hidden="true"><Icon name={pauses ? "alert" : "help"} size={16} /></span>
        <span className="hitl-title">{kindLabel}</span>
        {pauses
          ? <span className="hitl-blocking">{t("hitl.titleBlocking")}</span>
          : <span className="hitl-auto">{t("hitl.autoResolving")}</span>}
      </div>
      <div className="body">{req.promptZh || req.prompt}</div>
      {req.promptZh && req.promptZh !== req.prompt && (
        <details className="hitl-raw">
          <summary>{t("hitl.showOriginal")}</summary>
          <div>{req.prompt}</div>
        </details>
      )}
      {/* F: auto-resolving cards are informational — no input, the swarm handles it */}
      {pauses && answerRecorded && deliveryKey && (
        <div
          className={`hitl-delivery-state ${deliveryPhase}`}
          role="status"
          aria-live="polite"
          data-command-id={activeCommandId}
          data-tooltip={durableDelivery.detail}
        >
          <Icon name={deliveryPhase === "pending" || deliveryPhase === "observed" ? "clock" : "alert"} size={14} />
          <span className="hitl-delivery-copy">{t(deliveryKey)}</span>
          {retryable && (
            <Button
              size="sm"
              variant="outline"
              className="hitl-delivery-retry"
              isDisabled={sending}
              onPress={retryDelivery}
            >
              {sending ? t("hitl.delivery.retrying") : t("hitl.delivery.retry")}
            </Button>
          )}
        </div>
      )}
      {pauses && !answerRecorded && <div className="hitl-opts">
        {req.options.map((o) => (
          <Button key={o} size="sm" variant="outline" isDisabled={sending} onPress={() => submit(o)}>{o}</Button>
        ))}
        <Input
          ref={inputRef}
          className="hitl-free"
          value={free}
          disabled={sending}
          aria-label={t("hitl.inputAria")}
          placeholder={hasOptions ? t("hitl.orType") : t("hitl.inputAria")}
          onChange={(e) => setFree(e.target.value)}
          onKeyDown={(e) => { if (e.key === "Enter") { e.preventDefault(); submit(free); } }}
        />
        <Button
          size="sm"
          variant="primary"
          className="hitl-send"
          isDisabled={sending || !free.trim()}
          onPress={() => submit(free)}
        >
          {sending ? t("hitl.sending") : t("hitl.submit")}
        </Button>
        {onDismiss && (
          <Button
            size="sm"
            variant="ghost"
            className="hitl-dismiss"
            isDisabled={sending}
            aria-label={t("hitl.dismiss.tip")}
            onPress={dismiss}
          >
            {t("hitl.dismiss")}
          </Button>
        )}
      </div>}
    </div>
  );
}

function CoordinatorThread({
  deck,
  running,
  onAnswer,
  onDismiss,
}: {
  deck: DeckState;
  running: boolean;
  onAnswer: (requestId: string, opt: string, commandId: string) => Promise<boolean>;
  onDismiss?: (requestId: string, commandId: string) => Promise<boolean>;
}) {
  const t = useT();
  const messages = coordinatorThread(deck);
  const digest = swarmDigest(deck);
  const hasProgress = messages.some((message) => message.kind === "progress" && message.progressBrief);
  const legacyBrief = !hasProgress && deck.finished ? legacyProgressBrief(deck, digest) : undefined;
  const hasContent = messages.length > 0 || deck.hitlRequests.length > 0;
  const feedRef = useRef<HTMLDivElement>(null);
  const stick = useRef(true);
  useEffect(() => {
    const el = feedRef.current;
    if (el && stick.current) el.scrollTop = el.scrollHeight;
  }, [deck.chat, deck.hitlRequests, running, deck.finished]);
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
        {!hasContent && !deck.finished && (
          <div className="coord-empty">{t("coord.empty")}</div>
        )}
        {messages.map((m) => m.kind === "progress" && m.progressBrief
          ? <ProgressBriefBubble key={m.id} brief={m.progressBrief} ts={m.ts} t={t} />
          : <CoordBubble key={m.id} m={m} t={t} mode={deck.mode || "ctf"} />)}
        {deck.hitlRequests.map((r, i) => (
          <HitlCard key={r.id} req={r} first={i === 0} onAnswer={onAnswer} onDismiss={onDismiss} />
        ))}
        {running && !hasProgress && <DigestBubble digest={digest} t={t} />}
        {legacyBrief && <ProgressBriefBubble brief={legacyBrief} ts={deck.finishedAt ?? Date.now()} t={t} />}
        {deck.finished && <AnswerBubble digest={digest} t={t} />}
      </div>
    </div>
  );
}

function Composer({
  workspaceMode,
  started,
  solved,
  running,
  finished,
  followupPending,
  paused,
  solvers,
  flags,
  onDispatch,
  onCommand,
  onRequestProgress,
  onResolve,
  attachments,
  onAddFiles,
  onRemoveFile,
  prefill,
  onPrefillConsumed,
  onOpenBtw,
  onOpenReport,
  controlledText,
  onControlledTextChange,
}: {
  workspaceMode: "ctf" | "pentest";
  started: boolean;
  solved: boolean;
  running: boolean;
  finished: boolean;
  followupPending: boolean;
  paused: boolean;
  solvers: string[];
  flags: string[];
  onDispatch: (prompt: string, opts: DispatchOpts) => void | boolean | Promise<void | boolean>;
  onCommand: (target: string, action: string, text: string,
              opts?: ControlCommandOpts) => Promise<boolean>;
  onRequestProgress: () => Promise<boolean>;
  onResolve: (text?: string) => void;
  attachments: SavedFile[];
  onAddFiles: (files: FileList | File[]) => void;
  onRemoveFile: (path: string) => void;
  prefill: ComposerPrefill | null;
  onPrefillConsumed: () => void;
  onOpenBtw?: () => void;
  onOpenReport?: () => void;
  controlledText?: string;
  onControlledTextChange?: (value: string) => void;
}) {
  const t = useT();
  const [internalText, setInternalText] = useState("");
  const text = controlledText ?? internalText;
  const setText = useCallback((value: string) => {
    setInternalText(value);
    onControlledTextChange?.(value);
  }, [onControlledTextChange]);
  const [markFalseOpen, setMarkFalseOpen] = useState(false);
  const [cmdTarget, setCmdTarget] = useState("global");
  const [progressPending, setProgressPending] = useState(false);
  const fileRef = useRef<HTMLInputElement>(null);
  // the live composer field (dispatch textarea before start, command input after)
  // — Cmd/Ctrl+K focuses whichever is mounted.
  const dispatchRef = useRef<HTMLTextAreaElement>(null);
  const commandRef = useRef<HTMLInputElement>(null);

  // Auto-grow: the dispatch textarea expands with its content from its 1-row
  // default up to DISPATCH_MAX_H, then scrolls internally. Measured by resetting
  // height to "auto" (so scrollHeight reflects content, not the current box) and
  // capping the result. Driven from a layout effect on `text` so it also resets
  // after a dispatch clears the field. CSS keeps max-height in sync for the cap.
  const autoGrow = useCallback(() => {
    const el = dispatchRef.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, DISPATCH_MAX_H)}px`;
  }, []);
  // re-measure on every text change (typing, paste, and the reset-to-empty after
  // dispatch) and on first mount of the dispatch composer.
  useLayoutEffect(() => { autoGrow(); }, [text, started, autoGrow]);

  // Global composer focus shortcut: bare "/" (when not already typing in a field)
  // jumps focus to the composer. Guarded so it never hijacks typing inside an
  // input/textarea/select/contenteditable, leaving Enter=submit and Shift+Enter=
  // newline untouched. NOTE: Cmd/Ctrl+K is OWNED by the command palette (page.tsx)
  // now — it no longer focuses the composer here. The palette still offers a
  // "focus composer" action (and "/" stays) so the affordance isn't lost.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const el = e.target as HTMLElement | null;
      const typing = !!el && (
        el.tagName === "INPUT" || el.tagName === "TEXTAREA" ||
        el.tagName === "SELECT" || el.isContentEditable
      );
      // Collab canvas owns bare "/" for its search field; do not steal focus.
      if (el?.closest?.(".react-flow, .collab-shell")) return;
      const slash = e.key === "/" && !e.metaKey && !e.ctrlKey && !e.altKey && !typing;
      if (!slash) return;
      const field = dispatchRef.current ?? commandRef.current;
      if (!field) return;
      e.preventDefault();
      field.focus();
      field.select?.();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);
  const [dragOver, setDragOver] = useState(false);
  const [webSearch, setWebSearch] = useState(true);
  const mode = workspaceMode;
  const [collect, setCollect] = useState(false);
  const [collectCount, setCollectCount] = useState("");  // "" = unknown count
  const [reportGoalMode, setReportGoalMode] = useState<"automatic" | "count">("automatic");
  const [expectedFindings, setExpectedFindings] = useState("10");
  const [flagFormat, setFlagFormat] = useState<"brace" | "token" | "custom">("brace");
  const [flagWrapper, setFlagWrapper] = useState("");
  const [containerMode, setContainerMode] = useState(false);
  const [allowOperatorInput, setAllowOperatorInput] = useState(false);
  // P2-v3: when the coordinator runs inside a container, local worker mode is
  // rejected server-side — force container mode and lock the toggle.
  const [containerLocked, setContainerLocked] = useState(false);
  const [raceTimeout, setRaceTimeout] = useState("300");
  const [wallClockBudget, setWallClockBudget] = useState("0");
  const [maxTotalWorkers, setMaxTotalWorkers] = useState("0");
  const [costBudgetUsd, setCostBudgetUsd] = useState("0");
  const [advancedOpen, setAdvancedOpen] = useState(false);
  /** Per-run overrides are sent only after the operator edits advanced fields. */
  const [advancedTouched, setAdvancedTouched] = useState<Partial<Record<"raceTimeout" | "wallClockBudget" | "maxTotalWorkers" | "costBudgetUsd", true>>>({});
  useEffect(() => {
    let cancelled = false;
    try {
      const saved = window.localStorage.getItem("muteki.webSearch");
      if (saved === "0") setWebSearch(false);
      if (window.localStorage.getItem("muteki.collect") === "1") setCollect(true);
      const savedFlagFormat = window.localStorage.getItem("muteki.flagFormat");
      if (savedFlagFormat === "token" || savedFlagFormat === "custom") setFlagFormat(savedFlagFormat);
      const savedFlagWrapper = window.localStorage.getItem("muteki.flagWrapper");
      if (savedFlagWrapper) setFlagWrapper(savedFlagWrapper);
      const savedContainer = window.localStorage.getItem("muteki.containerMode");
      if (savedContainer === "1") setContainerMode(true);
      else if (savedContainer !== "0") {
        getWorkerSettings().then((c) => {
          if (!cancelled && c?.worker_backend === "container") setContainerMode(true);
        });
      }
      if (window.localStorage.getItem("muteki.allowOperatorInput") === "1") {
        setAllowOperatorInput(true);
      }
    } catch { /* ignore */ }
    // P2-v3: ask the backend whether IT runs in a container. If so, container
    // mode is mandatory — force it on and lock the toggle (local is server-side
    // rejected, so a local toggle would only produce confusing 400s).
    checkAuth().then((a) => {
      if (!cancelled && a?.inContainer) { setContainerMode(true); setContainerLocked(true); }
    }).catch(() => { /* ignore */ });
    return () => { cancelled = true; };
  }, []);
  const toggleWeb = () => setWebSearch((v) => {
    const nv = !v;
    try { window.localStorage.setItem("muteki.webSearch", nv ? "1" : "0"); } catch { /* ignore */ }
    return nv;
  });
  const toggleCollect = () => setCollect((v) => {
    const nv = !v;
    try { window.localStorage.setItem("muteki.collect", nv ? "1" : "0"); } catch { /* ignore */ }
    return nv;
  });
  const pickFlagFormat = (fmt: "brace" | "token" | "custom") => {
    setFlagFormat(fmt);
    try { window.localStorage.setItem("muteki.flagFormat", fmt); } catch { /* ignore */ }
  };
  const updateFlagWrapper = (value: string) => {
    setFlagWrapper(value);
    try { window.localStorage.setItem("muteki.flagWrapper", value); } catch { /* ignore */ }
  };
  const toggleContainer = () => {
    if (containerLocked) return;  // P2-v3: forced on inside a container
    setContainerMode((v) => {
      const nv = !v;
      try { window.localStorage.setItem("muteki.containerMode", nv ? "1" : "0"); } catch { /* ignore */ }
      return nv;
    });
  };
  const toggleOperatorInput = () => setAllowOperatorInput((value) => {
    const next = !value;
    try {
      window.localStorage.setItem("muteki.allowOperatorInput", next ? "1" : "0");
    } catch { /* ignore */ }
    return next;
  });
  useEffect(() => {
    if (!prefill || started) return;
    setText(prefill.text);
    window.requestAnimationFrame(() => {
      dispatchRef.current?.focus();
      dispatchRef.current?.setSelectionRange(prefill.text.length, prefill.text.length);
    });
    onPrefillConsumed();
  }, [onPrefillConsumed, prefill, setText, started]);

  // A dispatch that required backend confirmation deliberately kept the prose
  // in the composer. Once the confirmed Run starts, clear it before the same
  // field switches into operator-command mode.
  const wasStarted = useRef(started);
  useEffect(() => {
    if (started && !wasStarted.current) setText("");
    wasStarted.current = started;
  }, [started, setText]);

  const dispatch = async () => {
    const v = text.trim();
    if (!v) return;
    const optionalInt = (raw: string) => {
      const parsed = parseInt(raw, 10);
      return Number.isNaN(parsed) ? undefined : parsed;
    };
    const optionalFloat = (raw: string) => {
      const parsed = parseFloat(raw);
      return Number.isNaN(parsed) ? undefined : parsed;
    };
    const runCaps = {
      ...(advancedTouched.raceTimeout ? { raceTimeout: parseInt(raceTimeout, 10) || undefined } : {}),
      ...(advancedTouched.wallClockBudget ? { wallClockBudget: optionalInt(wallClockBudget) } : {}),
      ...(advancedTouched.maxTotalWorkers ? { maxTotalWorkers: optionalInt(maxTotalWorkers) } : {}),
      ...(advancedTouched.costBudgetUsd ? { costBudgetUsd: optionalFloat(costBudgetUsd) } : {}),
    };
    if (mode === "pentest" && reportGoalMode === "count" &&
        (!Number.isSafeInteger(Number(expectedFindings)) || Number(expectedFindings) < 1)) {
      window.alert("请填写大于 0 的报告目标数量");
      return;
    }
    const dispatched = await onDispatch(v, {
      webSearch: mode === "pentest" ? true : webSearch,
      mode, collect, containerMode,
      allowOperatorInput: mode === "pentest" ? true : allowOperatorInput,
      flagFormat,
      flagWrapper: flagFormat === "custom" ? flagWrapper.trim() : undefined,
      collectCount: collect ? (parseInt(collectCount, 10) || 0) : undefined,
      reportGoalMode: mode === "pentest" ? reportGoalMode : undefined,
      expectedFindings: mode === "pentest" && reportGoalMode === "count" ? Number(expectedFindings) : undefined,
      ...runCaps,
    });
    if (dispatched !== false) setText("");
  };
  const command = (action: string) => {
    const raw = text.trim();
    if (["pause", "resume", "freeze", "thaw"].includes(action)) {
      onCommand(cmdTarget, action, ""); return;
    }
    let a = action, payload = raw;
    if (raw.startsWith("/")) { const [v, ...rest] = raw.slice(1).split(" "); a = v; payload = rest.join(" "); }
    if (a === "focus" || a === "redirect") a = "directive";
    if (a === "resolve") { onResolve(payload || undefined); setText(""); return; }
    if (a === "mark_false" && !payload && flags.length > 1) {
      setMarkFalseOpen((v) => !v);
      return;
    }
    const NO_ARG = new Set([
      "writeup", "mark_false", "ask", "stop", "pause", "resume", "freeze", "thaw",
    ]);
    if (!payload && !NO_ARG.has(a)) return;
    onCommand(cmdTarget, a, payload);
    setMarkFalseOpen(false);
    setText("");
  };
  const requestProgress = async () => {
    if (progressPending) return;
    setProgressPending(true);
    try {
      await onRequestProgress();
    } finally {
      setProgressPending(false);
    }
  };
  const runningActions = paused
    ? QUICK_RUNNING.map((action) => action.key === "pause"
      ? { key: "resume", labelKey: "quick.resume", tipKey: "quick.resume.tip", icon: "play" as IconName }
      : action)
    : QUICK_RUNNING;
  const btwButton = onOpenBtw ? (
    <Button
      size="sm"
      variant="ghost"
      className="btw-btn"
      onPress={onOpenBtw}
      data-tooltip={t("btw.btnTitle")}
      aria-label={t("btw.btnTitle")}
    >
      <Icon name="eye" size={13} />
      {t("btw.btn")}
    </Button>
  ) : null;

  if (!started) {
    return (
      <div className="composer2 t-page-slide">
        <div
          className={`wrap ${dragOver ? "dragover" : ""}`}
          onDragOver={(e) => {
            // only react to file drags, not text/element drags within the page
            if (!Array.from(e.dataTransfer.types || []).includes("Files")) return;
            e.preventDefault();
            if (!dragOver) setDragOver(true);
          }}
          onDragLeave={(e) => {
            // ignore leaves into descendants (mode-row, textarea, overlay) — only
            // clear when the pointer actually exits the .wrap, else the overlay flickers
            if (e.currentTarget.contains(e.relatedTarget as Node | null)) return;
            setDragOver(false);
          }}
          onDrop={(e) => {
            e.preventDefault();
            setDragOver(false);
            if (e.dataTransfer.files?.length) onAddFiles(e.dataTransfer.files);
          }}
        >
          {dragOver && (
            <div className="drop-overlay" aria-hidden="true">
              <span className="drop-ico"><Icon name="upload" size={26} /></span>
              <span className="drop-label">{t("composer.dropHint")}</span>
            </div>
          )}
          <TextArea
            ref={dispatchRef}
            data-composer-input
            rows={1}
            value={text}
            onChange={(e) => setText(e.target.value)}
            onInput={autoGrow}
            onKeyDown={(e) => { if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) { e.preventDefault(); void dispatch(); } }}
            onPaste={(e) => {
              // Paste-to-attach: if the clipboard carries files (e.g. a screenshot,
              // a pcap, a binary), attach them instead of dumping bytes/text. Check
              // `files` first, then fall back to `items` (some browsers surface a
              // pasted image only as a kind:"file" item). Plain text falls through.
              const cd = e.clipboardData;
              const fromItems = Array.from(cd.items || [])
                .filter((it) => it.kind === "file")
                .map((it) => it.getAsFile())
                .filter((f): f is File => f != null);
              if (cd.files?.length) { e.preventDefault(); onAddFiles(cd.files); }
              else if (fromItems.length) { e.preventDefault(); onAddFiles(fromItems); }
            }}
            placeholder={t(mode === "pentest" ? "composer.pentestPlaceholder" : "composer.dispatchPlaceholder")}
          />
          {attachments.length > 0 && (
            <div className="attach-row">
              {attachments.map((f) => (
                <span className="attach-chip" key={f.path}>
                  <span className="afn"><Icon name="paperclip" size={13} /> {f.name}</span>
                  <span className="asz">{fmtSize(f.size)}</span>
                  <Button size="sm" variant="ghost" isIconOnly className="arm" onPress={() => onRemoveFile(f.path)} aria-label={t("composer.removeFile")}><Icon name="x" size={13} /></Button>
                </span>
              ))}
            </div>
          )}
          <div className="crow">
            <Button size="sm" variant="ghost" isIconOnly className="attach-btn" onPress={() => fileRef.current?.click()} aria-label={t("composer.attach")}><Icon name="paperclip" /></Button>
            <input ref={fileRef} type="file" multiple style={{ display: "none" }}
              onChange={(e) => {
                // Snapshot to a static array BEFORE resetting value: onAddFiles is
                // async (it may await newRun() on a draft), and `value = ""` clears
                // the live FileList mid-flight — leaving the upload with 0 files and
                // a 422 from the endpoint. Array.from() copies the File refs first.
                const picked = e.target.files ? Array.from(e.target.files) : [];
                e.target.value = "";
                if (picked.length) onAddFiles(picked);
              }} />
            <span className="auto-note"><b>▸</b> {mode === "pentest" && reportGoalMode === "count"
              ? `逐条提交报告，累计 ${expectedFindings || "?"} 份后结束`
              : t(mode === "pentest" ? "composer.pentestAutoNote" : "composer.autoNote")}</span>
            <span className="spacer" />
            {mode === "pentest" && <Button
              size="sm" variant="ghost"
              className="websearch-toggle"
              onPress={() => setReportGoalMode((value) => value === "automatic" ? "count" : "automatic")}
              aria-label="切换报告完成条件"
              aria-pressed={reportGoalMode === "count"}
            ><Icon name="target" size={14} />{reportGoalMode === "count" ? "按报告数结束" : "自动判断结束"}</Button>}
            {mode === "pentest" && reportGoalMode === "count" && <NumberField
              className="collect-count" min={1} value={expectedFindings}
              onChange={setExpectedFindings} scrubLabel="#"
              ariaLabel="目标报告数量" data-tooltip="提交多少份独立漏洞报告后结束"
            />}
            {mode === "ctf" && (
              <Button
                size="sm"
                variant="ghost"
                className={`websearch-toggle ${collect ? "on" : "off"}`}
                onPress={toggleCollect}
                aria-pressed={collect}
                aria-label={t("composer.collectTitle")}
              >
                <Icon name={collect ? "target" : "flag"} size={14} />
                {collect ? t("composer.collectOn") : t("composer.collectOff")}
              </Button>
            )}
            {mode === "ctf" && collect && (
              <NumberField
                className="collect-count"
                min={0}
                allowEmpty
                value={collectCount}
                onChange={setCollectCount}
                scrubLabel="#"
                placeholder={t("composer.collectCountPlaceholder")}
                data-tooltip={t("composer.collectCountTitle")}
                ariaLabel={t("composer.collectCountPlaceholder")}
              />
            )}
            {mode === "ctf" && <Button
              size="sm"
              variant="ghost"
              className={`websearch-toggle ${webSearch ? "on" : "off"}`}
              onPress={toggleWeb}
              aria-pressed={webSearch}
              aria-label={t(webSearch ? "composer.webOnTitle" : "composer.webOffTitle")}
            >
              <Icon name={webSearch ? "globe" : "lock"} size={14} />
              {webSearch ? t("composer.webOn") : t("composer.webOff")}
            </Button>}
            <Button
              size="sm"
              variant="ghost"
              className={`websearch-toggle ${containerMode ? "on" : "off"}${containerLocked ? " locked" : ""}`}
              onPress={toggleContainer}
              aria-pressed={containerMode}
              isDisabled={containerLocked}
              aria-label={containerLocked
                ? t("composer.containerLockedTitle")
                : t(containerMode ? "composer.containerOnTitle" : "composer.containerOffTitle")}
            >
              <Icon name={containerMode ? "lock" : "globe"} size={14} />
              {containerMode ? t("composer.containerOn") : t("composer.containerOff")}
            </Button>
            <Button
              size="sm"
              variant="ghost"
              className={`advanced-toggle ${advancedOpen ? "on" : ""}`}
              onPress={() => setAdvancedOpen((v) => !v)}
              aria-expanded={advancedOpen}
              aria-controls="dispatch-advanced-controls"
              aria-label={t("composer.advancedTitle")}
            >
              <Icon name="gear" size={14} />
              {t("composer.advanced")}
              <Icon name="chevronDown" size={13} className="advanced-chevron" />
            </Button>
            <Button size="sm" variant="primary" isIconOnly className="send" onPress={dispatch} isDisabled={!text.trim()} aria-label={t("composer.dispatchTitle")}><Icon name="send" size={15} /></Button>
          </div>
          {advancedOpen && (
            <div id="dispatch-advanced-controls" className="composer-advanced-panel" role="region" aria-label={t("composer.advancedTitle")}>
              <div className="advanced-panel-heading">
                <strong>{t("composer.advancedRunSettings")}</strong>
                <span>{t("composer.advancedRunHint")}</span>
              </div>
              <div className="advanced-options-grid">
                <div className="advanced-option-card">
                  <div className="advanced-option-copy">
                    <strong>{t("composer.operatorInput")}</strong>
                    <span>{t("composer.operatorInputHint")}</span>
                  </div>
                  <Button
                    size="sm"
                    variant="ghost"
                    className={`websearch-toggle ${allowOperatorInput ? "on" : "off"}`}
                    onPress={toggleOperatorInput}
                    aria-pressed={allowOperatorInput}
                    aria-label={t(allowOperatorInput
                      ? "composer.operatorInputOnTitle"
                      : "composer.operatorInputOffTitle")}
                  >
                    <Icon name={allowOperatorInput ? "help" : "lock"} size={14} />
                    {t(allowOperatorInput
                      ? "composer.operatorInputOn"
                      : "composer.operatorInputOff")}
                  </Button>
                </div>
                {mode === "ctf" && (
                  <div className="advanced-option-card advanced-flag-card">
                    <div className="advanced-option-copy">
                      <strong>{t("composer.flagFormat")}</strong>
                      <span>{t("composer.flagFormatHint")}</span>
                    </div>
                    <div className="flag-format-controls">
                      <Select
                        className="advanced-select"
                        selectedKey={flagFormat}
                        onSelectionChange={(key) => {
                          const v = String(key ?? "brace");
                          pickFlagFormat(v === "token" ? "token" : v === "custom" ? "custom" : "brace");
                        }}
                        aria-label={t("composer.flagFormatTitle")}
                      >
                        <Select.Trigger><Select.Value /><Select.Indicator /></Select.Trigger>
                        <Select.Popover><ListBox>
                          <ListBoxItem id="brace">{t("composer.flagFormatBrace")}</ListBoxItem>
                          <ListBoxItem id="custom">{t("composer.flagFormatCustom")}</ListBoxItem>
                          <ListBoxItem id="token">{t("composer.flagFormatToken")}</ListBoxItem>
                        </ListBox></Select.Popover>
                      </Select>
                      {flagFormat === "custom" && (
                        <Input
                          className="flag-wrapper-input"
                          value={flagWrapper}
                          onChange={(e) => updateFlagWrapper(e.target.value)}
                          placeholder={t("composer.flagWrapperPlaceholder")}
                          aria-label={t("composer.flagWrapperTitle")}
                        />
                      )}
                    </div>
                  </div>
                )}
              </div>
              <div className="advanced-section-heading">
                <strong>{t("composer.advancedLimits")}</strong>
                <span>{t("composer.advancedLimitsHint")}</span>
              </div>
              <div className="advanced-metrics-grid">
                <div className="advanced-metric-card">
                  <span>{t("composer.raceTimeout")}</span>
                  <NumberField className="collect-count" min={0} value={raceTimeout}
                    onChange={(v) => { setAdvancedTouched((old) => ({ ...old, raceTimeout: true })); setRaceTimeout(v); }}
                    ariaLabel={t("composer.raceTimeout")} title={t("composer.raceTimeoutTitle")} suffix="s" />
                  <small>{t("composer.raceTimeoutHint")}</small>
                </div>
                <div className="advanced-metric-card">
                  <span>{t("composer.wallBudget")}</span>
                  <NumberField className="collect-count" min={0} value={wallClockBudget}
                    onChange={(v) => { setAdvancedTouched((old) => ({ ...old, wallClockBudget: true })); setWallClockBudget(v); }}
                    ariaLabel={t("composer.wallBudget")} title={t("composer.wallBudgetTitle")} suffix="s" />
                  <small>{t("composer.zeroUnlimited")}</small>
                </div>
                <div className="advanced-metric-card">
                  <span>{t("composer.maxTotalWorkers")}</span>
                  <NumberField className="collect-count" min={0} value={maxTotalWorkers}
                    onChange={(v) => { setAdvancedTouched((old) => ({ ...old, maxTotalWorkers: true })); setMaxTotalWorkers(v); }}
                    ariaLabel={t("composer.maxTotalWorkers")} title={t("composer.maxTotalWorkersTitle")} />
                  <small>{t("composer.zeroUnlimited")}</small>
                </div>
                <div className="advanced-metric-card">
                  <span>{t("composer.costBudget")}</span>
                  <NumberField className="collect-count" min={0} step={0.01} value={costBudgetUsd}
                    onChange={(v) => { setAdvancedTouched((old) => ({ ...old, costBudgetUsd: true })); setCostBudgetUsd(v); }}
                    ariaLabel={t("composer.costBudget")} title={t("composer.costBudgetTitle")} suffix="USD" />
                  <small>{t("composer.zeroUnlimited")}</small>
                </div>
              </div>
            </div>
          )}
        </div>
        <div className="hintline">
          {mode === "pentest" && reportGoalMode === "count"
            ? `已设定 ${expectedFindings || "?"} 份独立报告目标 · ⌘↵ 开始测试`
            : t(mode === "pentest" ? "composer.pentestHintline" : "composer.hintline")}
          <span className="kbd-hint" aria-label={t("composer.focusHint")}>
            <kbd>{t("composer.focusKey")}</kbd> {t("composer.focusHint")}
          </span>
          <span className="kbd-hint" aria-label={t("palette.hint")}>
            <kbd>{t("palette.key")}</kbd> {t("palette.hint")}
          </span>
        </div>
      </div>
    );
  }

  if (finished) {
    const ask = () => {
      const question = text.trim();
      if (!question || followupPending) return;
      void onCommand("global", "ask", question).then((ok) => {
        if (ok) setText("");
      });
    };
    return (
      <div className="composer2 command-composer t-page-slide">
        <div className="wrap command-wrap">
          <div className="command-row">
            <Input
              ref={commandRef}
              data-composer-input
              className="command-input"
              value={text}
              disabled={followupPending}
              onChange={(e) => setText(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); ask(); }
              }}
              placeholder={mode === "pentest" ? "向测试 Worker 追问，或补充目标背景" : "输入要向解题 Worker 追问的问题"}
            />
            <Button size="sm" variant="primary" isIconOnly className="send" isDisabled={!text.trim() || followupPending} onPress={ask} aria-label="Ask"><Icon name="send" size={15} /></Button>
          </div>
          <div className="command-actionbar">
            <div className="quick">
              <Button size="sm" variant="primary" className="primary" isDisabled={followupPending} aria-label={mode === "pentest" ? "继续测试" : t("quick.resolveTitle")} onPress={() => onResolve()}><Icon name="refresh" size={13} />{mode === "pentest" ? "继续测试" : t("quick.resolve")}</Button>
              <Button size="sm" variant="ghost" isDisabled={!text.trim() || followupPending} aria-label={t("quick.ask.tip")} onPress={ask}><Icon name="help" size={13} />{t("quick.ask")}</Button>
              {btwButton}
              {mode === "ctf" && <Button size="sm" variant="ghost" isDisabled={followupPending} aria-label={t("quick.writeup.tip")} onPress={() => void onCommand("global", "writeup", "")}><Icon name="pencil" size={13} />{t("quick.writeup")}</Button>}
              {mode === "pentest" && onOpenReport && <Button size="sm" variant="ghost" onPress={onOpenReport}><Icon name="rows" size={13} />查看报告</Button>}
              {mode === "ctf" && solved ? (
                <>
                  <span className="quick-sep" />
                  <Button size="sm" variant="danger-soft" className="danger" isDisabled={followupPending} aria-label={t("quick.markFalseTitle")} onPress={() => {
                    if (flags.length === 1) void onCommand("global", "mark_false", flags[0]);
                    else setMarkFalseOpen((value) => !value);
                  }}><Icon name="alert" size={13} />{t("quick.markFalse")}</Button>
                </>
              ) : null}
            </div>
            <div className="command-hint">{followupPending ? "正在处理后续操作…" : t("composer.finishedHint")}</div>
          </div>
          {mode === "ctf" && solved && markFalseOpen && flags.length > 1 ? (
            <div className="markfalse-picker" role="group" aria-label={t("quick.markFalseTitle")}>{flags.map((flag) => (
              <Button key={flag} size="sm" variant="danger-soft" aria-label={flag} onPress={() => { void onCommand("global", "mark_false", flag); setMarkFalseOpen(false); }}>{flag}</Button>
            ))}</div>
          ) : null}
        </div>
      </div>
    );
  }

  return (
    <div className="composer2 command-composer t-page-slide">
      <div className="wrap command-wrap">
        <div className="command-row">
          <Select className="command-target" selectedKey={cmdTarget} onSelectionChange={(key) => setCmdTarget(String(key ?? "global"))} aria-label={t("composer.to")}>
            <Label>{t("composer.to")}</Label>
            <Select.Trigger><Select.Value /><Select.Indicator /></Select.Trigger>
            <Select.Popover><ListBox>
              <ListBoxItem id="global">{mode === "pentest" ? "全部 Worker" : t("composer.allSolvers")}</ListBoxItem>
              {solvers.map((s) => <ListBoxItem key={s} id={`solver:${s}`}>{s}</ListBoxItem>)}
            </ListBox></Select.Popover>
          </Select>
          <Input
            ref={commandRef}
            data-composer-input
            className="command-input"
            value={text}
            onChange={(e) => setText(e.target.value)}
            onKeyDown={(e) => { if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); command("hint"); } }}
            placeholder={t("composer.commandPlaceholder")}
          />
          <Button size="sm" variant="primary" isIconOnly className="send" onPress={() => command("hint")} aria-label={t("composer.send")}><Icon name="send" size={15} /></Button>
        </div>
        <div className="command-actionbar">
          <div className="quick">
            {running && (
              <Button
                size="sm"
                variant="ghost"
                isDisabled={progressPending}
                aria-busy={progressPending}
                aria-label={t("quick.progress.tip")}
                onPress={() => void requestProgress()}
              >
                <Icon name="radio" size={13} />{t("quick.progress")}
              </Button>
            )}
            {btwButton}
            {running ? (
              <>
                {runningActions.map((a) => (
                  <Button key={a.key} size="sm" variant="ghost" aria-label={mode === "pentest" && a.key === "directive" ? "将原文作为下一步测试方向，不扩大授权范围" : t(a.tipKey)} onPress={() => command(a.key)}><Icon name={a.icon} size={13} />{t(a.labelKey)}</Button>
                ))}
                <span className="quick-sep" />
                <Button size="sm" variant="danger-soft" className="danger" onPress={() => command("stop")} aria-label={t("quick.stopTitle")}><Icon name="xCircle" size={13} />{t("quick.stop")}</Button>
              </>
            ) : (
              <span className="command-hint">任务正在结束，请等待完成事件</span>
            )}
          </div>
          <div className="command-hint">{running ? t("composer.steerHint") : t("composer.finishedHint")}</div>
        </div>
        {mode === "ctf" && solved && markFalseOpen && flags.length > 1 && (
          <div className="markfalse-picker" role="group" aria-label={t("quick.markFalseTitle")}>
            {flags.map((f) => (
              <Button
                size="sm"
                variant="danger-soft"
                key={f}
                aria-label={f}
                onPress={() => {
                  onCommand(cmdTarget, "mark_false", f);
                  setMarkFalseOpen(false);
                  setText("");
                }}
              >
                {f}
              </Button>
            ))}
          </div>
        )}
      </div>
    </div>
  );
}

type ComposerPrefill = {
  mode: "ctf" | "pentest";
  text: string;
};

/** First-run actions that prefill the real dispatch composer. */
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
    <div className="welcome">
      <div className="welcome-hero">
        <h1 className="wm">{t(`${prefix}.title`)}</h1>
        <div className="sub">{t(`${prefix}.sub`)}</div>
      </div>
      <div className="suggest-label">{t("welcome.examplesLabel")}</div>
      <div className="suggest">
        {examples.map((ex) => (
          <Button
            variant="ghost"
            className="suggest-card"
            key={ex.key}
            onPress={() => onChoose({
              mode: ex.mode,
              text: t(`${prefix}.${ex.key}.prompt`),
            })}
          >
            <span className="s-ico" aria-hidden="true"><Icon name={ex.icon} size={16} /></span>
            <span className="s-cat">{t(`${prefix}.${ex.key}.cat`)}</span>
            <span className="s-nm">{t(`${prefix}.${ex.key}.nm`)}</span>
            <span className="s-tg">{t(`${prefix}.${ex.key}.tg`)}</span>
          </Button>
        ))}
      </div>
    </div>
  );
}

export function Conversation({
  workspaceMode = "ctf",
  deck,
  running,
  loading,
  onCommand,
  onRequestProgress,
  onResolve,
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
  onOpenBtw,
  reportOpen = false,
  reportPanel,
  onOpenReport,
}: {
  workspaceMode?: "ctf" | "pentest";
  deck: DeckState;
  running: boolean;
  loading: boolean;
  onCommand: (target: string, action: string, text: string,
              opts?: ControlCommandOpts) => Promise<boolean>;
  onRequestProgress: () => Promise<boolean>;
  onResolve: (text?: string) => void;
  // Returns false when the dispatch was intercepted before launch (e.g. the
  // open-ended collect confirm) — the composer keeps the prompt text then.
  onDispatch: (prompt: string, opts: DispatchOpts) => void | boolean | Promise<void | boolean>;
  attachments: SavedFile[];
  onAddFiles: (files: FileList | File[]) => void;
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
  // open the "Worker 详情" panel focused on a single worker (roster row click).
  onOpenWorker: (solverId: string) => void;
  onOpenAgent?: (solverId: string) => void;
  onOpenKnowledge?: (id: string) => void;
  onOpenWorkspace: () => void;
  // fired after an operator answers a blocking HITL decision — owner toasts.
  onHitlAnswered?: () => void;
  connected: boolean;
  onOpenBtw?: () => void;
  reportOpen?: boolean;
  reportPanel?: ReactNode;
  onOpenReport?: () => void;
}) {
  const t = useT();
  const { lang, setLang } = useLang();
  const scrollRef = useRef<HTMLDivElement>(null);
  const stick = useRef(true);
  const [inspectorWidth, setInspectorWidth] = useState(INSPECTOR_WIDTH_DEFAULT);
  const [inspectorOpen, setInspectorOpen] = useState(true);
  const [composerPrefill, setComposerPrefill] = useState<ComposerPrefill | null>(null);
  const [pentestDraftText, setPentestDraftText] = useState("");
  const consumeComposerPrefill = useCallback(() => setComposerPrefill(null), []);
  const [inspectorResizing, setInspectorResizing] = useState(false);
  const inspectorResizeCleanup = useRef<(() => void) | null>(null);
  useEffect(() => {
    const el = scrollRef.current;
    if (!el) return;
    // A fresh draft renders the welcome panel, not a chronological feed. On
    // narrow screens that panel is taller than the available viewport; pinning
    // it to the bottom hid the title and template label on every page load.
    if (!deck.started) {
      stick.current = true;
      el.scrollTop = 0;
      return;
    }
    if (stick.current) el.scrollTop = el.scrollHeight;
  }, [deck.chat, deck.hitlRequests, deck.started, deck.finished]);
  useEffect(() => {
    try {
      const raw = window.localStorage.getItem(INSPECTOR_WIDTH_STORAGE_KEY);
      if (raw) setInspectorWidth(clampInspectorWidth(Number(raw), window.innerWidth));
      const openRaw = window.localStorage.getItem(INSPECTOR_OPEN_STORAGE_KEY);
      if (openRaw === "0" || openRaw === "false") setInspectorOpen(false);
    } catch {
      // keep default when storage is unavailable
    }
  }, []);
  useEffect(() => () => inspectorResizeCleanup.current?.(), []);
  useEffect(() => {
    try {
      window.localStorage.setItem(INSPECTOR_WIDTH_STORAGE_KEY, String(inspectorWidth));
    } catch {
      // ignore storage failures
    }
  }, [inspectorWidth]);
  useEffect(() => {
    try {
      window.localStorage.setItem(INSPECTOR_OPEN_STORAGE_KEY, inspectorOpen ? "1" : "0");
    } catch {
      // ignore storage failures
    }
  }, [inspectorOpen]);

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

  const solvers = Object.keys(deck.lanes);
  const digest = swarmDigest(deck);
  const latestProgress = [...deck.chat].reverse().find(
    (message) => message.kind === "progress" && message.progressBrief,
  )?.progressBrief ?? (deck.finished ? legacyProgressBrief(deck, digest) : undefined);
  // F: only blocking (external_blocker) hand-raises pause the swarm — the hero/flow
  // counts must reflect those, not auto-resolving informational cards.
  const blockingHitlCount = deck.hitlRequests.filter((r) => (r.pausesBehavior ?? true)).length;
  const onWriteup = () => onCommand("global", "writeup", "");
  const onMarkFalseFlag = (flag: string) => onCommand("global", "mark_false", flag);
  // Before a solve is dispatched the deck is a local draft with no backend run,
  // so useRun opens no SSE stream by design — that's "idle", not "disconnected".
  // Only a started-but-unfinished run that has lost its stream is truly off.
  const connState = connected ? "on" : !deck.started || deck.finished ? "idle" : "off";
  const connectionLabel = connected
    ? t("convo.connected")
    : deck.finished
      ? t("convo.finished")
      : !deck.started
        ? t("convo.idle")
        : t("convo.disconnected");
  const runStateLabel = deck.preparing
    ? t("convo.preparing")
    : digest.phase === "paused"
    ? t("convo.paused")
    : running
      ? t("convo.live")
      : t("convo.finished");
  const runStateClass = deck.preparing ? "live" : digest.phase === "paused" ? "paused" : running ? "live" : "done";
  const latestControl = deck.controlCommands[deck.controlCommands.length - 1];

  // Screen-reader live region: mirror ONLY the latest system lifecycle line
  // (run started, solved, finished, reopened, goal met, …) into a visually-hidden
  // polite region. The full worker firehose is deliberately NOT announced — that
  // would spam assistive tech with hundreds of messages; lifecycle lines are the
  // few transitions an operator actually needs read aloud.
  const lastSystem = [...deck.chat].reverse().find((m) => m.role === "system");
  const liveStatus = lastSystem
    ? (workspaceMode === "pentest" && lastSystem.i18nKey === "sys.goalMet"
        && lastSystem.i18nVars?.why === "model_goal_with_evidence"
        ? t("sys.pentestGoalMet")
        : lastSystem.i18nKey ? t(lastSystem.i18nKey, lastSystem.i18nVars) : lastSystem.content)
    : "";
  const showInspector = deck.started && !artifactOpen && !reportOpen && inspectorOpen;
  const pentestRuntimeOpen = workspaceMode === "pentest" && artifactOpen && !reportOpen;
  const inspectorToggleLabel = t(inspectorOpen ? "insp.run.hide" : "insp.run.show");
  const composerElement = <Composer
    workspaceMode={workspaceMode}
    started={deck.started}
    solved={deck.solved}
    running={running}
    finished={deck.finished}
    followupPending={deck.followupPending}
    paused={digest.phase === "paused"}
    solvers={solvers}
    flags={deck.flags}
    onDispatch={onDispatch}
    onCommand={onCommand}
    onRequestProgress={onRequestProgress}
    onResolve={onResolve}
    attachments={attachments}
    onAddFiles={onAddFiles}
    onRemoveFile={onRemoveFile}
    prefill={composerPrefill}
    onPrefillConsumed={consumeComposerPrefill}
    onOpenBtw={onOpenBtw}
    onOpenReport={onOpenReport}
    controlledText={workspaceMode === "pentest" ? pentestDraftText : undefined}
    onControlledTextChange={workspaceMode === "pentest" ? setPentestDraftText : undefined}
  />;

  return (
    <div
      className={`convo ${showInspector ? "has-inspector" : ""} ${artifactOpen && !reportOpen ? "runtime-peer-open" : ""} ${inspectorResizing ? "inspector-resizing" : ""}`}
      style={{ "--inspector-width": `${inspectorWidth}px` } as CSSProperties}
    >
      <div className="sr-only" role="status" aria-live="polite" aria-atomic="true" aria-label={t("a11y.status")}>{liveStatus}</div>
      <div className="convo-top t-texts-reveal">
        <Button
          size="sm"
          variant="ghost"
          isIconOnly
          className="icon-btn"
          onPress={onToggleRail}
          aria-label={workspaceMode === "pentest" ? "切换测试列表" : t("convo.toggleRuns")}
        >
          <Icon name="menu" size={15} />
        </Button>
        <div className="convo-top-context">
          <span
            className="title"
            data-tooltip={deck.started ? deck.challengeName || t("convo.run") : workspaceMode === "pentest" ? "新测试" : t("convo.newSolve")}
            title={deck.started ? deck.challengeName || t("convo.run") : workspaceMode === "pentest" ? "新测试" : t("convo.newSolve")}
          >
            {deck.started ? deck.challengeName || t("convo.run") : workspaceMode === "pentest" ? "新测试" : t("convo.newSolve")}
          </span>
          {(deck.started && deck.category && workspaceMode === "ctf") || deck.runId ? (
            <div className="convo-top-meta">
              {deck.started && deck.category && workspaceMode === "ctf" && (
                <Chip
                  size="sm"
                  variant="soft"
                  color={getCategoryColor(deck.category)}
                  className="cat uppercase"
                >
                  {deck.category}
                </Chip>
              )}
              {deck.runId && (
                <Chip
                  size="sm"
                  variant="secondary"
                  className="rid"
                  data-tooltip={`sessions/${deck.runId}`}
                  title={`sessions/${deck.runId}`}
                >
                  <span>sessions/{deck.runId}</span>
                  {deck.started && (
                    <Button
                      size="sm"
                      variant="ghost"
                      isIconOnly
                      className="rid-open"
                      onPress={onOpenWorkspace}
                      aria-label={t("convo.openWorkspace")}
                    >
                      <Icon name="panel" size={11} />
                    </Button>
                  )}
                </Chip>
              )}
            </div>
          ) : null}
        </div>
        {deck.started && (
          <div className="convo-top-view">
            <Tabs
              selectedKey={reportOpen ? "report" : artifactOpen ? "runtime" : "conversation"}
              onSelectionChange={(key) => {
                if (key === "runtime") onOpenArtifact(artifactView);
                else if (key === "collaboration") onOpenArtifact("collaboration");
                else if (key === "report") onOpenReport?.();
                else onShowConversation();
              }}
            >
              <Tabs.List className="convo-view-switch" aria-label={t("convo.viewSwitcher")}>
                <Tabs.Tab id="conversation">
                  <Icon name="rows" size={13} />
                  <span>{t("convo.viewConversation")}</span>
                  <Tabs.Indicator />
                </Tabs.Tab>
                <Tabs.Tab id="runtime">
                  <Icon name="panel" size={13} />
                  <span>{t("convo.viewRuntime")}</span>
                  <Tabs.Indicator />
                </Tabs.Tab>
                <Tabs.Tab id="collaboration">
                  <Icon name="network" size={13} />
                  <span>{t("convo.viewCollaboration")}</span>
                  <Tabs.Indicator />
                </Tabs.Tab>
                {workspaceMode === "pentest" && <Tabs.Tab id="report">
                  <Icon name="rows" size={13} />
                  <span>报告</span>
                  <Tabs.Indicator />
                </Tabs.Tab>}
              </Tabs.List>
            </Tabs>
          </div>
        )}
        <div className="convo-top-controls">
          <div className="convo-top-status">
            {deck.started && (
              <Chip
                size="sm"
                variant={runStateClass === "live" || runStateClass === "paused" ? "soft" : "secondary"}
                color={runStateClass === "live" ? "success" : runStateClass === "paused" ? "warning" : "default"}
                className={`runstate ${runStateClass}`}
              >
                <span className={`runstate-dot ${runStateClass}`} aria-hidden="true" />
                <span>{runStateLabel}</span>
              </Chip>
            )}
            {latestControl && (
              <Chip
                size="sm"
                variant="soft"
                color={
                  latestControl.status === "effect_observed"
                    ? "success"
                    : latestControl.status === "failed" || latestControl.status === "rejected"
                    ? "danger"
                    : latestControl.status === "partial" || latestControl.status === "unknown"
                    ? "warning"
                    : "default"
                }
                className={`control-receipt status-${latestControl.status}`}
                data-tooltip={latestControl.detail || `${latestControl.id} · ${latestControl.target}`}
                title={latestControl.detail || `${latestControl.id} · ${latestControl.target}`}
              >
                /{latestControl.action} · {t(`control.status.${latestControl.status}`)}
              </Chip>
            )}
            <EngineBar degradedEngines={deck.degradedEngines} />
            <span className={`dot ${connState}`} role="img" aria-label={connectionLabel} data-tooltip={connectionLabel} />
          </div>
          <div className="convo-top-actions">
            {deck.started && !artifactOpen && !reportOpen && (
              <Button
                size="sm"
                variant="ghost"
                isIconOnly
                className="icon-btn convo-inspector-toggle"
                onPress={() => setInspectorOpen((open) => !open)}
                aria-label={inspectorToggleLabel}
                aria-pressed={inspectorOpen}
                data-active={inspectorOpen ? "true" : "false"}
                data-tooltip={inspectorToggleLabel}
              >
                <Icon name="panel" size={14} />
              </Button>
            )}
            <Button
              size="sm"
              variant="ghost"
              isIconOnly
              className="icon-btn"
              onPress={onToggleTheme}
              aria-label={t(theme === "dark" ? "theme.toLight" : "theme.toDark")}
            >
              <Icon name={theme === "dark" ? "sun" : "moon"} size={14} />
            </Button>
            <Button
              size="sm"
              variant="ghost"
              className="lang-btn"
              onPress={() => setLang(lang === "zh" ? "en" : "zh")}
              aria-label={t("lang.toggleTitle")}
            >
              {t("lang.toggle")}
            </Button>
          </div>
        </div>
      </div>

      <div className="convo-body">
        <div className="convo-mainpane">
          {reportOpen && reportPanel ? reportPanel : <>
          <div
            className={`convo-scroll ${deck.started ? "has-workspace" : ""}`}
            ref={scrollRef}
            onScroll={(e) => {
              const el = e.currentTarget;
              stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 70;
            }}
          >
            {!deck.started && !loading ? (
              <Welcome t={t} onChoose={setComposerPrefill} workspaceMode={workspaceMode} />
            ) : (
              <div className={`workspace ${artifactOpen ? "solo" : ""}`}>
                <div className="coord-col t-page-slide">
                  {deck.started && running && !connected && (
                    <div className="conn-banner" role="status" aria-live="polite">
                      <span className="cb-ico" aria-hidden="true"><Icon name="plug" size={14} /></span>
                      <span className="cb-msg">{t("convo.streamLost")}</span>
                    </div>
                  )}
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
                  {loading ? (
                    <div className="coord-thread">
                      <div className="coord-wrap">
                      <div className="coord-loading">
                          <Spinner size="sm" color="accent" aria-label={workspaceMode === "pentest" ? "正在加载测试" : t("loading.run")} />
                          <span className="cl-title">{workspaceMode === "pentest" ? "正在加载测试" : t("loading.run")}</span>
                          <span className="cl-hint">{workspaceMode === "pentest" ? "正在恢复本次测试的实时记录。" : t("loading.runHint")}</span>
                        </div>
                        <div className="coord-skel" aria-hidden="true">
                          {[68, 82, 54].map((w, i) => (
                            <div className="coord-skel-bubble" key={i}>
                              <Skeleton className="skel-line t-skeleton" style={{ width: `${w}%`, height: 10 }} />
                              <Skeleton className="skel-line t-skeleton" style={{ width: `${w - 16}%`, height: 10 }} />
                            </div>
                          ))}
                        </div>
                      </div>
                    </div>
                  ) : (
                    <CoordinatorThread
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
                    />
                  )}
                </div>
              </div>
            )}
          </div>

          {!pentestRuntimeOpen && composerElement}
          </>}
        </div>

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
                running={running}
                artifactOpen={artifactOpen}
                artifactView={artifactView}
                onOpenArtifact={onOpenArtifact}
                onSpawnWorker={onSpawnWorker}
                onKillWorker={onKillWorker}
                onOpenWorker={onOpenWorker}
                onOpenAgent={onOpenAgent}
                onWriteup={onWriteup}
                onMarkFalseFlag={onMarkFalseFlag}
                onClose={() => setInspectorOpen(false)}
              />
            )}
          </div>
        )}

        <div className="convo-runtime-peer">{runtimePanel}</div>
      </div>
    </div>
  );
}
