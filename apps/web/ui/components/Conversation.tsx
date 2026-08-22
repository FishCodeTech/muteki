"use client";

import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import type { CSSProperties, PointerEvent as ReactPointerEvent, ReactNode } from "react";
import {
  ChatMessage, DeckState, HitlRequest, SolverCost, SwarmDigest,
  coordinatorThread, hitlDeliveryState, swarmDigest,
} from "@/lib/events";
import { getWorkerSettings, checkAuth, SavedFile } from "@/lib/useRun";
import {
  commandIdForDecision, type DecisionControlAction,
} from "@/lib/controlClient";
import { useT, useLang } from "@/lib/i18n";
import { EngineBar } from "@/components/EngineBar";
import { Icon, type IconName } from "@/components/Icon";
import { SelectionGlider } from "@/components/SelectionGlider";
import { RunInspector } from "@/components/RunInspector";
import { CopyText } from "@/components/CopyText";
import { NumberField } from "@/components/NumberField";
import { reportToMarkdown } from "@/lib/reportMarkdown";
import { useCopied } from "@/lib/useCopied";
import { InspectorSkeleton, SkelLine } from "@/components/Skeleton";
import type { ArtifactView } from "@/lib/events";

/**
 * The conversation spine (the redesign's centre column): a ChatGPT/Claude-style
 * thread between the operator and the COORDINATOR (DeepSeek `reason`). It owns
 * the welcome/dispatch state and the dual-mode composer. The worker firehose,
 * fact-graph and blackboard live in the persistent right-column RunInspector
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
  // worker isolation: when true, the run uses a controlled Docker runtime that
  // can't read the host challenge-source tree. Default false = host subprocess.
  containerMode?: boolean;
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
  { key: "hint", labelKey: "quick.hint", tipKey: "quick.hint.tip", icon: "help" },
  { key: "directive", labelKey: "quick.directive", tipKey: "quick.directive.tip", icon: "pencil" },
  { key: "redirect", labelKey: "quick.redirect", tipKey: "quick.redirect.tip", icon: "network" },
  { key: "focus", labelKey: "quick.focus", tipKey: "quick.focus.tip", icon: "target" },
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

function inspectorWidthMax(viewportWidth?: number): number {
  if (!viewportWidth || viewportWidth <= 0) return INSPECTOR_WIDTH_MAX;
  return Math.max(INSPECTOR_WIDTH_MIN, Math.min(INSPECTOR_WIDTH_MAX, Math.round(viewportWidth * 0.46)));
}

function clampInspectorWidth(width: number, viewportWidth?: number): number {
  const next = Number.isFinite(width) ? width : INSPECTOR_WIDTH_DEFAULT;
  return Math.round(Math.min(inspectorWidthMax(viewportWidth), Math.max(INSPECTOR_WIDTH_MIN, next)));
}

function clock(ts: number): string {
  if (!ts) return "";
  const ms = ts < 1e12 ? ts * 1000 : ts;
  const d = new Date(ms);
  if (isNaN(d.getTime())) return "";
  const p = (n: number) => String(n).padStart(2, "0");
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

function fmtSize(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(0)} KB`;
  return `${(n / 1024 / 1024).toFixed(1)} MB`;
}

/** event ts (seconds or ms) → ms. */
function tsMs(ts: number): number {
  return ts < 1e12 ? ts * 1000 : ts;
}

/** Compact elapsed: "M:SS" under an hour, "H:MM:SS" beyond. */
function fmtDuration(ms: number): string {
  if (!isFinite(ms) || ms < 0) ms = 0;
  const total = Math.floor(ms / 1000);
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  const p = (n: number) => String(n).padStart(2, "0");
  return h > 0 ? `${h}:${p(m)}:${p(s)}` : `${m}:${p(s)}`;
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
  const end = finishedAt != null ? tsMs(finishedAt) : (freezeRef.current ?? now);
  return fmtDuration(end - tsMs(startedAt));
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
}: {
  m: ChatMessage;
  t: (k: string, vars?: Record<string, string | number>) => string;
}) {
  const [copied, copy] = useCopied();
  const text = m.i18nKey ? t(m.i18nKey, m.i18nVars) : m.content;
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
          <button
            type="button"
            className={`coord-copy ${copied ? "copied" : ""}`}
            onClick={() => copy(text)}
            title={t("coord.copyMsg")}
            aria-label={t("coord.copyMsgAria")}
          >
            <Icon name={copied ? "check" : "copy"} size={12} />
            <span className="cc-lbl">{copied ? t("common.copied") : t("common.copyShort")}</span>
          </button>
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
        {t("coord.digest", {
          phase: phaseLabel(digest.phase, t),
          verified: digest.verified, candidates: digest.candidates,
          intents: digest.openIntents, dead: digest.deadEnds,
          online: digest.onlineWorkers, total: digest.totalWorkers,
        })}
      </div>
      {digest.latestVerified && (
        <div className="digest-sub">{t("coord.latestVerified", { fact: digest.latestVerified })}</div>
      )}
    </div>
  );
}

function AnswerBubble({ digest, t }: { digest: SwarmDigest; t: (k: string, v?: Record<string, string | number>) => string }) {
  const pentest = digest.mode === "pentest";
  const none = pentest
    ? digest.reports.length === 0 && digest.phase !== "goal_met"
    : digest.flags.length === 0 && digest.phase !== "goal_met";
  const multi = pentest ? digest.expectedReports > 1 : digest.expectedFlags > 1;
  return (
    <div className={`coord-bubble answer ${none ? "none" : ""}`}>
      <span className="coord-node" aria-hidden="true"><Icon name={none ? "clock" : "check"} size={12} /></span>
      <div className="coord-digest-title">
        {t("coord.answerTitle")}
        {multi && (pentest ? digest.reports.length > 0 : digest.flags.length > 0) && (
          <span className="ans-flag-count">
            {pentest ? `${digest.reports.length}/${digest.expectedReports}` : `${digest.flags.length}/${digest.expectedFlags}`}
          </span>
        )}
      </div>
      {pentest && digest.reports.length > 0 ? (
        <div className="body">
          {digest.reports.map((row) => (
            <div key={row.id}>
              <CopyText
                value={reportToMarkdown(row)}
                className="ans-flag"
                titleKey="runtime.reports.copyMarkdown"
                ariaLabelKey="runtime.reports.copyMarkdownAria"
              >
                {row.title}
              </CopyText>
            </div>
          ))}
        </div>
      ) : !pentest && digest.flags.length > 0 ? (
        <div className="body">
          {digest.flags.map((f) => (
            <div key={f}>
              <CopyText value={f} className="ans-flag">{t("coord.answerFlag", { flag: f })}</CopyText>
            </div>
          ))}
        </div>
      ) : digest.phase === "goal_met" ? (
        <div className="body">
          <CopyText
            value={digest.goalWhy || ""}
            className="ans-flag goal"
            titleKey="common.copyAnswer"
            ariaLabelKey="common.copyAnswerAria"
          >
            {t("coord.answerGoal", { why: digest.goalWhy || "" })}
          </CopyText>
        </div>
      ) : (
        <div className="body">{t("coord.answerNone", { verified: digest.verified, dead: digest.deadEnds })}</div>
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
    <div className="flow-popover" role="dialog" aria-label={t("flow.title")} onClick={(e) => e.stopPropagation()}>
      <div className="flow-head">
        <div>
          <div className="flow-title">{t("flow.title")}</div>
          <div className="flow-sub">{t("flow.subtitle")}</div>
        </div>
        <button className="flow-close" onClick={onClose} title={t("settings.close")} aria-label={t("settings.close")}><Icon name="x" size={14} /></button>
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
        <span>{t("flow.metrics", {
          workers: `${digest.onlineWorkers}/${digest.totalWorkers}`,
          facts: digest.verified,
          intents: digest.openIntents,
          dead: digest.deadEnds,
          hitl: hitlCount,
        })}</span>
      </div>
    </div>
  );
}

function StatusHero({ digest, hitlCount, t }: { digest: SwarmDigest; hitlCount: number; t: (k: string, v?: Record<string, string | number>) => string }) {
  const [flowOpen, setFlowOpen] = useState(false);
  const [resultsOpen, setResultsOpen] = useState(false);
  const elapsed = useElapsed(
    digest.startedAt,
    digest.finishedAt,
    digest.phase === "solved" || digest.phase === "goal_met" || digest.phase === "finished",
  );
  const live = digest.phase === "running" || digest.phase === "collecting" || digest.phase === "racing";
  const singleFlag = digest.flags.length === 1 ? digest.flags[0] : "";
  const hasResultLedger = digest.mode === "pentest"
    ? digest.reports.length > 1 || digest.expectedReports > 1 || (digest.phase === "collecting" && digest.reports.length > 0)
    : digest.flags.length > 1 || digest.expectedFlags > 1 || (digest.phase === "collecting" && digest.flags.length > 0);
  const resultsVisible = resultsOpen && hasResultLedger;
  const resultCount = digest.mode === "pentest"
    ? (digest.expectedReports > 1
      ? t("hero.results.reportProgress", { n: digest.reports.length, total: digest.expectedReports })
      : t("hero.results.reportCount", { n: digest.reports.length }))
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
    detail = digest.mode === "pentest"
      ? t("hero.detail.reportPipeline", {
          submitted: digest.reportSubmitted,
          reproducing: digest.reportReproducing,
          accepted: digest.reportAccepted,
          total: digest.expectedReports,
        })
      : t("hero.detail.collecting", { n: digest.flags.length, total: digest.expectedFlags });
  } else if (digest.phase === "paused") {
    detail = hitlCount > 0 ? t("hero.detail.pausedN", { n: hitlCount }) : t("hero.detail.paused");
  } else if (digest.phase === "goal_met") {
    detail = digest.goalWhy || t("hero.detail.goalMet");
  } else if (digest.phase === "finished") {
    detail = t("hero.detail.finished", { verified: digest.verified, dead: digest.deadEnds });
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
    detail = digest.mode === "pentest"
      && (digest.reportSubmitted > 0 || digest.reportReproducing > 0 || digest.verifyingActive > 0)
      ? t("hero.detail.reportPipeline", {
          submitted: digest.reportSubmitted,
          reproducing: digest.reportReproducing,
          accepted: digest.reportAccepted,
          total: digest.expectedReports,
        })
      : digest.latestVerified
        ? t("hero.detail.runningFact", { fact: digest.latestVerified })
        : digest.onlineWorkers > 0
          ? t("hero.detail.running", { online: digest.onlineWorkers, total: digest.totalWorkers })
          : t("hero.detail.runningIdle", { total: digest.totalWorkers });
  } else {
    detail = t("hero.detail.draft");
  }
  useEffect(() => {
    if (!flowOpen && !resultsVisible) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      setFlowOpen(false);
      setResultsOpen(false);
    };
    const onDoc = () => setFlowOpen(false);
    window.addEventListener("keydown", onKey);
    if (flowOpen) window.addEventListener("click", onDoc);
    return () => {
      window.removeEventListener("keydown", onKey);
      window.removeEventListener("click", onDoc);
    };
  }, [flowOpen, resultsVisible]);

  const toggleFlow = (e: { stopPropagation: () => void }) => {
    e.stopPropagation();
    setResultsOpen(false);
    setFlowOpen((v) => !v);
  };
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
          <span className="sh-phase">{t(`coord.phase.${digest.phase}`)}</span>
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
            <button
              type="button"
              className="sh-results-toggle"
              aria-expanded={resultsVisible}
              aria-controls="status-flag-results"
              onClick={toggleResults}
              title={t(resultsVisible ? "hero.results.collapse" : "hero.results.expand")}
            >
              <span className="sh-results-count">{resultCount}</span>
              {digest.flags[0] && <code className="sh-results-preview">{digest.flags[0]}</code>}
              {digest.flags.length > 1 && <span className="sh-results-more">+{digest.flags.length - 1}</span>}
              <Icon name="chevronDown" size={13} />
            </button>
          ) : singleFlag ? (
            <CopyText value={singleFlag} className="sh-detail sh-detail-flag">{singleFlag}</CopyText>
          ) : (
            <span className="sh-detail" title={detail}>{detail}</span>
          )}
        </span>
        <span className="sh-meta">
          {live && digest.onlineWorkers > 0 && (
            <span className="sh-workers" title={t("meta.workers")}>
              <Icon name="cpu" size={12} /> {digest.onlineWorkers}/{digest.totalWorkers}
            </span>
          )}
          {elapsed && <span className="sh-elapsed" title={t("meta.elapsed")}><Icon name="clock" size={12} /> {elapsed}</span>}
          <button
            type="button"
            className="sh-flow"
            aria-haspopup="dialog"
            aria-expanded={flowOpen}
            aria-label={t("flow.open")}
            onClick={toggleFlow}
          >
            <Icon name="list" size={12} /> {t("flow.short")}
          </button>
        </span>
      </div>
      {resultsVisible && (
        <section id="status-flag-results" className="sh-results-panel" aria-label={digest.mode === "pentest" ? t("hero.results.reportTitle") : t("hero.results.title")}>
          <header className="sh-results-head">
            <span><Icon name={digest.mode === "pentest" ? "list" : "flag"} size={13} /> {digest.mode === "pentest" ? t("hero.results.reportTitle") : t("hero.results.title")}</span>
            <span>{resultCount}</span>
          </header>
          <div className="sh-results-list">
            {digest.mode === "pentest"
              ? digest.reports.map((row, index) => (
                <div className="sh-result-row" key={row.id}>
                  <span className="sh-result-index">{String(index + 1).padStart(2, "0")}</span>
                  <CopyText
                    value={reportToMarkdown(row)}
                    className="sh-result-value"
                    titleKey="runtime.reports.copyMarkdown"
                    ariaLabelKey="runtime.reports.copyMarkdownAria"
                  >
                    {row.title}
                  </CopyText>
                </div>
              ))
              : digest.flags.map((flag, index) => (
                <div className="sh-result-row" key={`${index}-${flag}`}>
                  <span className="sh-result-index">{String(index + 1).padStart(2, "0")}</span>
                  <CopyText value={flag} className="sh-result-value">{flag}</CopyText>
                </div>
              ))}
          </div>
        </section>
      )}
      {flowOpen && <FlowPopover digest={digest} hitlCount={hitlCount} onClose={() => setFlowOpen(false)} t={t} />}
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
          title={durableDelivery.detail}
        >
          <Icon name={deliveryPhase === "pending" || deliveryPhase === "observed" ? "clock" : "alert"} size={14} />
          <span className="hitl-delivery-copy">{t(deliveryKey)}</span>
          {retryable && (
            <button
              type="button"
              className="hitl-delivery-retry"
              disabled={sending}
              onClick={retryDelivery}
            >
              {sending ? t("hitl.delivery.retrying") : t("hitl.delivery.retry")}
            </button>
          )}
        </div>
      )}
      {pauses && !answerRecorded && <div className="hitl-opts">
        {req.options.map((o) => (
          <button key={o} type="button" disabled={sending} onClick={() => submit(o)}>{o}</button>
        ))}
        <input
          ref={inputRef}
          className="hitl-free"
          value={free}
          disabled={sending}
          aria-label={t("hitl.inputAria")}
          placeholder={hasOptions ? t("hitl.orType") : t("hitl.inputAria")}
          onChange={(e) => setFree(e.target.value)}
          onKeyDown={(e) => { if (e.key === "Enter") { e.preventDefault(); submit(free); } }}
        />
        <button
          type="button"
          className="hitl-send"
          disabled={sending || !free.trim()}
          onClick={() => submit(free)}
        >
          {sending ? t("hitl.sending") : t("hitl.submit")}
        </button>
        {onDismiss && (
          <button
            type="button"
            className="hitl-dismiss"
            disabled={sending}
            title={t("hitl.dismiss.tip")}
            onClick={dismiss}
          >
            {t("hitl.dismiss")}
          </button>
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
        {messages.map((m) => <CoordBubble key={m.id} m={m} t={t} />)}
        {deck.hitlRequests.map((r, i) => (
          <HitlCard key={r.id} req={r} first={i === 0} onAnswer={onAnswer} onDismiss={onDismiss} />
        ))}
        {running && <DigestBubble digest={digest} t={t} />}
        {deck.finished && <AnswerBubble digest={digest} t={t} />}
      </div>
    </div>
  );
}

function Composer({
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
  onResolve,
  attachments,
  onAddFiles,
  onRemoveFile,
  prefill,
  onPrefillConsumed,
}: {
  started: boolean;
  solved: boolean;
  running: boolean;
  finished: boolean;
  followupPending: boolean;
  paused: boolean;
  solvers: string[];
  flags: string[];
  // Returns false when the dispatch was intercepted before launch (e.g. the
  // open-ended collect confirm) — the composer keeps the prompt text then.
  onDispatch: (prompt: string, opts: DispatchOpts) => void | boolean | Promise<void | boolean>;
  onCommand: (target: string, action: string, text: string,
              opts?: ControlCommandOpts) => Promise<boolean>;
  onResolve: (text?: string) => void;
  attachments: SavedFile[];
  onAddFiles: (files: FileList | File[]) => void;
  onRemoveFile: (path: string) => void;
  prefill: ComposerPrefill | null;
  onPrefillConsumed: () => void;
}) {
  const t = useT();
  const [text, setText] = useState("");
  const [markFalseOpen, setMarkFalseOpen] = useState(false);
  const [cmdTarget, setCmdTarget] = useState("global");
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
  const [mode, setMode] = useState<"ctf" | "pentest">("ctf");
  const [goal, setGoal] = useState("");
  const [scope, setScope] = useState("");
  const [collect, setCollect] = useState(false);
  const [collectCount, setCollectCount] = useState("");  // "" = unknown count
  const [flagFormat, setFlagFormat] = useState<"brace" | "token" | "custom">("brace");
  const [flagWrapper, setFlagWrapper] = useState("");
  const [containerMode, setContainerMode] = useState(false);
  // P2-v3: when the coordinator runs inside a container, local worker mode is
  // rejected server-side — force container mode and lock the toggle.
  const [containerLocked, setContainerLocked] = useState(false);
  const [raceTimeout, setRaceTimeout] = useState("720");
  const [wallClockBudget, setWallClockBudget] = useState("0");
  const [maxTotalWorkers, setMaxTotalWorkers] = useState("0");
  const [costBudgetUsd, setCostBudgetUsd] = useState("0");
  const [advancedOpen, setAdvancedOpen] = useState(false);
  /** Per-run overrides are sent only after the operator edits advanced fields. */
  const [advancedTouched, setAdvancedTouched] = useState(false);
  useEffect(() => {
    let cancelled = false;
    try {
      const saved = window.localStorage.getItem("muteki.webSearch");
      if (saved === "0") setWebSearch(false);
      const m = window.localStorage.getItem("muteki.mode");
      if (m === "pentest") setMode("pentest");
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
  const pickMode = (m: "ctf" | "pentest") => {
    setMode(m);
    try { window.localStorage.setItem("muteki.mode", m); } catch { /* ignore */ }
  };

  useEffect(() => {
    if (!prefill || started) return;
    setText(prefill.text);
    setMode(prefill.mode);
    setGoal(prefill.goal ?? "");
    setScope(prefill.scope ?? "");
    try { window.localStorage.setItem("muteki.mode", prefill.mode); } catch { /* ignore */ }
    window.requestAnimationFrame(() => {
      dispatchRef.current?.focus();
      dispatchRef.current?.setSelectionRange(prefill.text.length, prefill.text.length);
    });
    onPrefillConsumed();
  }, [onPrefillConsumed, prefill, started]);

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
    const runCaps = advancedTouched
      ? {
          raceTimeout: parseInt(raceTimeout, 10) || undefined,
          wallClockBudget: optionalInt(wallClockBudget),
          maxTotalWorkers: optionalInt(maxTotalWorkers),
          costBudgetUsd: optionalFloat(costBudgetUsd),
        }
      : {};
    const dispatched = await onDispatch(v, mode === "pentest"
      ? { webSearch, mode, goal: goal.trim(), scope: scope.trim(),
          collectCount: parseInt(collectCount, 10) || 0, containerMode, ...runCaps }
      : { webSearch, mode: "ctf", collect, containerMode,
          flagFormat,
          flagWrapper: flagFormat === "custom" ? flagWrapper.trim() : undefined,
          collectCount: collect ? (parseInt(collectCount, 10) || 0) : undefined,
          ...runCaps });
    // Intercepted dispatches (open-ended collect confirm) keep the text so
    // "返回填写数量" does not throw the prompt away.
    if (dispatched !== false) setText("");
  };
  const command = (action: string) => {
    const raw = text.trim();
    if (["pause", "resume", "freeze", "thaw"].includes(action)) {
      onCommand(cmdTarget, action, ""); return;
    }
    let a = action, payload = raw;
    if (raw.startsWith("/")) { const [v, ...rest] = raw.slice(1).split(" "); a = v; payload = rest.join(" "); }
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
  const runningActions = paused
    ? QUICK_RUNNING.map((action) => action.key === "pause"
      ? { key: "resume", labelKey: "quick.resume", tipKey: "quick.resume.tip", icon: "play" as IconName }
      : action)
    : QUICK_RUNNING;

  if (!started) {
    return (
      <div className="composer2 motion-run-enter">
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
          <div className="mode-row">
            <div className="mode-seg" role="tablist" aria-label={t("composer.mode")}>
              <button
                type="button" role="tab" aria-selected={mode === "ctf"}
                className={mode === "ctf" ? "on" : ""}
                title={t("composer.modeCtfTitle")}
                onClick={() => pickMode("ctf")}
              >{t("composer.modeCtf")}</button>
              <button
                type="button" role="tab" aria-selected={mode === "pentest"}
                className={mode === "pentest" ? "on" : ""}
                title={t("composer.modePentestTitle")}
                onClick={() => pickMode("pentest")}
              >{t("composer.modePentest")}</button>
            </div>
          </div>
          <textarea
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
          {mode === "pentest" && (
            <div className="pentest-fields">
              <input className="pf-input" value={goal} onChange={(e) => setGoal(e.target.value)} placeholder={t("composer.goalPlaceholder")} />
              <input className="pf-input" value={scope} onChange={(e) => setScope(e.target.value)} placeholder={t("composer.scopePlaceholder")} />
              <NumberField
                className="collect-count"
                min={0}
                allowEmpty
                value={collectCount}
                onChange={setCollectCount}
                scrubLabel="#"
                placeholder={t("composer.findingsCountPlaceholder")}
                title={t("composer.findingsCountTitle")}
                ariaLabel={t("composer.findingsCountPlaceholder")}
              />
            </div>
          )}
          {attachments.length > 0 && (
            <div className="attach-row">
              {attachments.map((f) => (
                <span className="attach-chip" key={f.path}>
                  <span className="afn"><Icon name="paperclip" size={13} /> {f.name}</span>
                  <span className="asz">{fmtSize(f.size)}</span>
                  <button className="arm" onClick={() => onRemoveFile(f.path)} title={t("composer.removeFile")} aria-label={t("composer.removeFile")}><Icon name="x" size={13} /></button>
                </span>
              ))}
            </div>
          )}
          <div className="crow">
            <button type="button" className="attach-btn" onClick={() => fileRef.current?.click()} title={t("composer.attach")} aria-label={t("composer.attach")}><Icon name="paperclip" /></button>
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
            <span className="auto-note"><b>▸</b> {t("composer.autoNote")}</span>
            <span className="spacer" />
            {mode === "ctf" && (
              <button
                type="button"
                className={`websearch-toggle ${collect ? "on" : "off"}`}
                onClick={toggleCollect}
                aria-pressed={collect}
                title={t("composer.collectTitle")}
              >
                <Icon name={collect ? "target" : "flag"} size={14} />
                {collect ? t("composer.collectOn") : t("composer.collectOff")}
              </button>
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
                title={t("composer.collectCountTitle")}
                ariaLabel={t("composer.collectCountPlaceholder")}
              />
            )}
            <button
              type="button"
              className={`websearch-toggle ${webSearch ? "on" : "off"}`}
              onClick={toggleWeb}
              aria-pressed={webSearch}
              title={t(webSearch ? "composer.webOnTitle" : "composer.webOffTitle")}
            >
              <Icon name={webSearch ? "globe" : "lock"} size={14} />
              {webSearch ? t("composer.webOn") : t("composer.webOff")}
            </button>
            <button
              type="button"
              className={`websearch-toggle ${containerMode ? "on" : "off"}${containerLocked ? " locked" : ""}`}
              onClick={toggleContainer}
              aria-pressed={containerMode}
              disabled={containerLocked}
              title={containerLocked
                ? t("composer.containerLockedTitle")
                : t(containerMode ? "composer.containerOnTitle" : "composer.containerOffTitle")}
            >
              <Icon name={containerMode ? "lock" : "globe"} size={14} />
              {containerMode ? t("composer.containerOn") : t("composer.containerOff")}
            </button>
            <button
              type="button"
              className={`advanced-toggle ${advancedOpen ? "on" : ""}`}
              onClick={() => setAdvancedOpen((v) => !v)}
              aria-expanded={advancedOpen}
              aria-controls="dispatch-advanced-controls"
              title={t("composer.advancedTitle")}
            >
              <Icon name="gear" size={14} />
              {t("composer.advanced")}
              <Icon name="chevronDown" size={13} className="advanced-chevron" />
            </button>
            <button className="send" onClick={dispatch} disabled={!text.trim()} title={t("composer.dispatchTitle")} aria-label={t("composer.dispatchTitle")}><Icon name="send" size={15} /></button>
          </div>
          {advancedOpen && (
            <div id="dispatch-advanced-controls" className="composer-advanced-panel">
              {mode === "ctf" && (
                <label className="advanced-field flag-format-field">
                  <span>{t("composer.flagFormat")}</span>
                  <div className="flag-format-controls">
                    <select
                      className="advanced-select"
                      value={flagFormat}
                      onChange={(e) => {
                        const v = e.target.value;
                        pickFlagFormat(v === "token" ? "token" : v === "custom" ? "custom" : "brace");
                      }}
                      title={t("composer.flagFormatTitle")}
                    >
                      <option value="brace">{t("composer.flagFormatBrace")}</option>
                      <option value="custom">{t("composer.flagFormatCustom")}</option>
                      <option value="token">{t("composer.flagFormatToken")}</option>
                    </select>
                    {flagFormat === "custom" && (
                      <input
                        className="flag-wrapper-input"
                        value={flagWrapper}
                        onChange={(e) => updateFlagWrapper(e.target.value)}
                        placeholder={t("composer.flagWrapperPlaceholder")}
                        title={t("composer.flagWrapperTitle")}
                      />
                    )}
                  </div>
                </label>
              )}
              <div className="advanced-metrics-grid">
                <label className="advanced-field advanced-metric-field">
                  <span>{t("composer.raceTimeout")}</span>
                  <NumberField className="collect-count" min={1} value={raceTimeout}
                    onChange={(v) => { setAdvancedTouched(true); setRaceTimeout(v); }}
                    suffix="s"
                    title={t("composer.raceTimeoutTitle")} />
                </label>
                <label className="advanced-field advanced-metric-field">
                  <span>{t("composer.wallBudget")}</span>
                  <NumberField className="collect-count" min={0} value={wallClockBudget}
                    onChange={(v) => { setAdvancedTouched(true); setWallClockBudget(v); }}
                    suffix="s"
                    title={t("composer.wallBudgetTitle")} />
                </label>
                <label className="advanced-field advanced-metric-field">
                  <span>{t("composer.maxTotalWorkers")}</span>
                  <NumberField className="collect-count" min={0} value={maxTotalWorkers}
                    onChange={(v) => { setAdvancedTouched(true); setMaxTotalWorkers(v); }}
                    title={t("composer.maxTotalWorkersTitle")} />
                </label>
                <label className="advanced-field advanced-metric-field">
                  <span>{t("composer.costBudget")}</span>
                  <NumberField className="collect-count" min={0} step={0.01} value={costBudgetUsd}
                    onChange={(v) => { setAdvancedTouched(true); setCostBudgetUsd(v); }}
                    suffix="USD"
                    title={t("composer.costBudgetTitle")} />
                </label>
              </div>
            </div>
          )}
        </div>
        <div className="hintline">
          {t("composer.hintline")}
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
      <div className="composer2 command-composer motion-run-enter">
        <div className="wrap command-wrap">
          <div className="command-row">
            <input
              ref={commandRef}
              data-composer-input
              className="command-input"
              value={text}
              disabled={followupPending}
              onChange={(e) => setText(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); ask(); }
              }}
              placeholder="输入要向解题 Worker 追问的问题"
            />
            <button className="send" disabled={!text.trim() || followupPending} onClick={ask} title="Ask" aria-label="Ask"><Icon name="send" size={15} /></button>
          </div>
          <div className="command-actionbar">
            <div className="quick">
              <button className="primary" disabled={followupPending} title={t("quick.resolveTitle")} onClick={() => onResolve()}><Icon name="refresh" size={13} />{t("quick.resolve")}</button>
              <button disabled={!text.trim() || followupPending} title={t("quick.ask.tip")} onClick={ask}><Icon name="help" size={13} />{t("quick.ask")}</button>
              <button disabled={followupPending} title={t("quick.writeup.tip")} onClick={() => void onCommand("global", "writeup", "")}><Icon name="pencil" size={13} />{t("quick.writeup")}</button>
              {solved ? (
                <>
                  <span className="quick-sep" />
                  <button className="danger" disabled={followupPending} title={t("quick.markFalseTitle")} onClick={() => {
                    if (flags.length === 1) void onCommand("global", "mark_false", flags[0]);
                    else setMarkFalseOpen((value) => !value);
                  }}><Icon name="alert" size={13} />{t("quick.markFalse")}</button>
                </>
              ) : null}
            </div>
            <div className="command-hint">{followupPending ? "正在处理后续操作…" : t("composer.finishedHint")}</div>
          </div>
          {solved && markFalseOpen && flags.length > 1 ? (
            <div className="markfalse-picker" role="group" aria-label={t("quick.markFalseTitle")}>{flags.map((flag) => (
              <button key={flag} type="button" title={flag} onClick={() => { void onCommand("global", "mark_false", flag); setMarkFalseOpen(false); }}>{flag}</button>
            ))}</div>
          ) : null}
        </div>
      </div>
    );
  }

  return (
    <div className="composer2 command-composer motion-run-enter">
      <div className="wrap command-wrap">
        <div className="command-row">
          <label className="command-target">{t("composer.to")}
            <select value={cmdTarget} onChange={(e) => setCmdTarget(e.target.value)}>
              <option value="global">{t("composer.allSolvers")}</option>
              {solvers.map((s) => <option key={s} value={`solver:${s}`}>{s}</option>)}
            </select>
            <Icon name="chevronDown" size={12} />
          </label>
          <input
            ref={commandRef}
            data-composer-input
            className="command-input"
            value={text}
            onChange={(e) => setText(e.target.value)}
            onKeyDown={(e) => { if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); command("hint"); } }}
            placeholder={t("composer.commandPlaceholder")}
          />
          <button className="send" onClick={() => command("hint")} title={t("composer.send")} aria-label={t("composer.send")}><Icon name="send" size={15} /></button>
        </div>
        <div className="command-actionbar">
          <div className="quick">
            {running ? (
              <>
                {runningActions.map((a) => (
                  <button key={a.key} title={t(a.tipKey)} aria-label={t(a.tipKey)} onClick={() => command(a.key)}><Icon name={a.icon} size={13} />{t(a.labelKey)}</button>
                ))}
                <span className="quick-sep" />
                <button className="danger" onClick={() => command("stop")} title={t("quick.stopTitle")}><Icon name="xCircle" size={13} />{t("quick.stop")}</button>
              </>
            ) : (
              <span className="command-hint">任务正在结束，请等待完成事件</span>
            )}
          </div>
          <div className="command-hint">{running ? t("composer.steerHint") : t("composer.finishedHint")}</div>
        </div>
        {solved && markFalseOpen && flags.length > 1 && (
          <div className="markfalse-picker" role="group" aria-label={t("quick.markFalseTitle")}>
            {flags.map((f) => (
              <button
                key={f}
                type="button"
                title={f}
                onClick={() => {
                  onCommand(cmdTarget, "mark_false", f);
                  setMarkFalseOpen(false);
                  setText("");
                }}
              >
                {f}
              </button>
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
  goal?: string;
  scope?: string;
};

/** First-run actions that prefill the real dispatch composer. */
const WELCOME_EXAMPLES: Array<{ key: string; icon: IconName; mode: "ctf" | "pentest" }> = [
  { key: "ex1", icon: "globe", mode: "ctf" },
  { key: "ex2", icon: "lock", mode: "ctf" },
  { key: "ex3", icon: "target", mode: "pentest" },
];

function Welcome({
  t,
  onChoose,
}: {
  t: (k: string) => string;
  onChoose: (prefill: ComposerPrefill) => void;
}) {
  return (
    <div className="welcome">
      <div className="welcome-hero">
        <div className="wm">無敵 <em>Muteki</em></div>
        <div className="sub">{t("welcome.sub")}</div>
      </div>
      <div className="suggest-label">{t("welcome.examplesLabel")}</div>
      <div className="suggest">
        {WELCOME_EXAMPLES.map((ex) => (
          <button
            type="button"
            className="suggest-card"
            key={ex.key}
            onClick={() => onChoose({
              mode: ex.mode,
              text: t(`welcome.${ex.key}.prompt`),
              goal: ex.mode === "pentest" ? t("welcome.ex3.goal") : undefined,
            })}
          >
            <span className="s-ico" aria-hidden="true"><Icon name={ex.icon} size={16} /></span>
            <span className="s-cat">{t(`welcome.${ex.key}.cat`)}</span>
            <span className="s-nm">{t(`welcome.${ex.key}.nm`)}</span>
            <span className="s-tg">{t(`welcome.${ex.key}.tg`)}</span>
          </button>
        ))}
      </div>
    </div>
  );
}

export function Conversation({
  deck,
  running,
  loading,
  onCommand,
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
  onOpenWorkspace,
  onOpenReport,
  onHitlAnswered,
  connected,
  onOpenBtw,
}: {
  deck: DeckState;
  running: boolean;
  loading: boolean;
  onCommand: (target: string, action: string, text: string,
              opts?: ControlCommandOpts) => Promise<boolean>;
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
  onOpenWorkspace: () => void;
  onOpenReport: (reportId: string) => void;
  // fired after an operator answers a blocking HITL decision — owner toasts.
  onHitlAnswered?: () => void;
  connected: boolean;
  onOpenBtw?: () => void;
}) {
  const t = useT();
  const { lang, setLang } = useLang();
  const scrollRef = useRef<HTMLDivElement>(null);
  const stick = useRef(true);
  const [inspectorWidth, setInspectorWidth] = useState(INSPECTOR_WIDTH_DEFAULT);
  const [composerPrefill, setComposerPrefill] = useState<ComposerPrefill | null>(null);
  const consumeComposerPrefill = useCallback(() => setComposerPrefill(null), []);
  const [inspectorResizing, setInspectorResizing] = useState(false);
  const inspectorResizeCleanup = useRef<(() => void) | null>(null);
  useEffect(() => {
    const el = scrollRef.current;
    if (el && stick.current) el.scrollTop = el.scrollHeight;
  }, [deck.chat, deck.hitlRequests, deck.started, deck.finished]);
  useEffect(() => {
    try {
      const raw = window.localStorage.getItem(INSPECTOR_WIDTH_STORAGE_KEY);
      if (raw) setInspectorWidth(clampInspectorWidth(Number(raw), window.innerWidth));
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
    ? (lastSystem.i18nKey ? t(lastSystem.i18nKey, lastSystem.i18nVars) : lastSystem.content)
    : "";

  return (
    <div
      className={`convo ${deck.started && !artifactOpen ? "has-inspector" : ""} ${artifactOpen ? "runtime-peer-open" : ""} ${inspectorResizing ? "inspector-resizing" : ""}`}
      style={{ "--inspector-width": `${inspectorWidth}px` } as CSSProperties}
    >
      <div className="sr-only" role="status" aria-live="polite" aria-atomic="true" aria-label={t("a11y.status")}>{liveStatus}</div>
      <div className="convo-top motion-shell-piece">
        <button className="icon-btn" onClick={onToggleRail} title={t("convo.toggleRuns")} aria-label={t("convo.toggleRuns")}><Icon name="menu" /></button>
        <span className="title">{deck.started ? deck.challengeName || t("convo.run") : t("convo.newSolve")}</span>
        {deck.started && deck.category && <span className="cat">{deck.category}</span>}
        {deck.runId && (
          <span className="rid" title={`sessions/${deck.runId}`}>
            sessions/{deck.runId}
            {deck.started && (
              <button className="rid-open" onClick={onOpenWorkspace} title={t("convo.openWorkspace")} aria-label={t("convo.openWorkspace")}><Icon name="panel" size={13} /></button>
            )}
          </span>
        )}
        {deck.started && (
          <div className="convo-view-switch selection-glide-host" role="tablist" aria-label={t("convo.viewSwitcher")}>
            <SelectionGlider selectedKey={artifactOpen ? "runtime" : "conversation"} selector='button[aria-selected="true"]' className="compact" duration={240} />
            <button type="button" role="tab" aria-selected={!artifactOpen} className={!artifactOpen ? "on" : ""} onClick={onShowConversation}>
              <Icon name="rows" size={13} />
              <span>{t("convo.viewConversation")}</span>
            </button>
            <button type="button" role="tab" aria-selected={artifactOpen} className={artifactOpen ? "on" : ""} onClick={() => onOpenArtifact(artifactView)}>
              <Icon name="panel" size={13} />
              <span>{t("convo.viewRuntime")}</span>
            </button>
          </div>
        )}
        <span className="spacer" />
        {onOpenBtw && (
          <button
            className="btw-btn"
            onClick={onOpenBtw}
            title={t("btw.btnTitle")}
            aria-label={t("btw.btnTitle")}
          >
            {t("btw.btn")}
          </button>
        )}
        {deck.started && (
          <span className={`runstate ${runStateClass}`}>{runStateLabel}</span>
        )}
        {latestControl && (
          <span
            className={`control-receipt status-${latestControl.status}`}
            title={latestControl.detail || `${latestControl.id} · ${latestControl.target}`}
          >
            /{latestControl.action} · {t(`control.status.${latestControl.status}`)}
          </span>
        )}
        <EngineBar degradedEngines={deck.degradedEngines} />
        <span className={`dot ${connState}`} role="img" aria-label={connectionLabel} title={connectionLabel} />
        <button
          className="icon-btn"
          onClick={onToggleTheme}
          title={t(theme === "dark" ? "theme.toLight" : "theme.toDark")}
          aria-label={t(theme === "dark" ? "theme.toLight" : "theme.toDark")}
        >
          <Icon name={theme === "dark" ? "sun" : "moon"} />
        </button>
        <button className="lang-btn" onClick={() => setLang(lang === "zh" ? "en" : "zh")} title={t("lang.toggleTitle")} aria-label={t("lang.toggleTitle")}>{t("lang.toggle")}</button>
      </div>

      <div className="convo-body">
        <div className="convo-mainpane">
          <div
            className={`convo-scroll ${deck.started ? "has-workspace" : ""}`}
            ref={scrollRef}
            onScroll={(e) => {
              const el = e.currentTarget;
              stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 70;
            }}
          >
            {!deck.started && !loading ? (
              <Welcome t={t} onChoose={setComposerPrefill} />
            ) : (
              <div className={`workspace ${artifactOpen ? "solo" : ""}`}>
                <div className="coord-col motion-run-enter">
                  {deck.started && running && !connected && (
                    <div className="conn-banner" role="status" aria-live="polite">
                      <span className="cb-ico" aria-hidden="true"><Icon name="plug" size={14} /></span>
                      <span className="cb-msg">{t("convo.streamLost")}</span>
                    </div>
                  )}
                  <StatusHero digest={digest} hitlCount={blockingHitlCount} t={t} />
                  {loading ? (
                    <div className="coord-thread">
                      <div className="coord-wrap">
                        <div className="coord-loading">
                          <span className="skel-spin" aria-hidden="true" />
                          <span className="cl-title">{t("loading.run")}</span>
                          <span className="cl-hint">{t("loading.runHint")}</span>
                        </div>
                        <div className="coord-skel" aria-hidden="true">
                          {[68, 82, 54].map((w, i) => (
                            <div className="coord-skel-bubble" key={i}>
                              <SkelLine w={`${w}%`} h={10} />
                              <SkelLine w={`${w - 16}%`} h={10} />
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

          <Composer
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
            onResolve={onResolve}
            attachments={attachments}
            onAddFiles={onAddFiles}
            onRemoveFile={onRemoveFile}
            prefill={composerPrefill}
            onPrefillConsumed={consumeComposerPrefill}
          />
        </div>

        {deck.started && !artifactOpen && (
          <div className="inspector-shell conversation-inspector-shell">
            <div
              className="inspector-resizer"
              role="separator"
              tabIndex={0}
              aria-label={t("insp.run.resize")}
              title={t("insp.run.resize")}
              aria-orientation="vertical"
              aria-valuemin={INSPECTOR_WIDTH_MIN}
              aria-valuemax={INSPECTOR_WIDTH_MAX}
              aria-valuenow={inspectorWidth}
              onPointerDown={startInspectorResize}
              onKeyDown={onInspectorResizeKey}
              onDoubleClick={() => setInspectorWidth(clampInspectorWidth(INSPECTOR_WIDTH_DEFAULT, window.innerWidth))}
            />
            {loading ? (
              <aside className="run-inspector motion-inspector" aria-label={t("insp.run.title")} aria-busy="true">
                  <InspectorSkeleton />
              </aside>
            ) : (
              <RunInspector
                deck={deck}
                running={running}
                artifactOpen={artifactOpen}
                artifactView={artifactView}
                onOpenArtifact={onOpenArtifact}
                onSpawnWorker={onSpawnWorker}
                onKillWorker={onKillWorker}
                onOpenWorker={onOpenWorker}
                onWriteup={onWriteup}
                onMarkFalseFlag={onMarkFalseFlag}
                onOpenReport={onOpenReport}
              />
            )}
          </div>
        )}

        <div className="convo-runtime-peer">
          {runtimePanel}
        </div>
      </div>
    </div>
  );
}
