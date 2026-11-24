"use client";

import { useEffect, useRef, useState } from "react";
import { swarmPhase, type DeckState } from "@/lib/events";
import { formatElapsed, toEpochMs } from "@/lib/format";
import { useT } from "@/lib/i18n";

export function useElapsed(startedAt?: number, finishedAt?: number, activeDurationMs = 0, activeSince?: number, freeze = false): string {
  const [now, setNow] = useState(() => Date.now());
  const freezeRef = useRef<number | undefined>(undefined);
  if (freeze && freezeRef.current == null) freezeRef.current = now;
  if (!freeze) freezeRef.current = undefined;
  const live = activeSince != null && finishedAt == null && !freeze;
  useEffect(() => {
    if (!live) return;
    const id = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(id);
  }, [live]);
  if (startedAt == null) return "";
  const end = finishedAt != null ? toEpochMs(finishedAt) : (freezeRef.current ?? now);
  return formatElapsed(activeDurationMs + (activeSince == null ? 0 : Math.max(0, end - toEpochMs(activeSince))));
}

type Tone = "live" | "paused" | "solved" | "done" | "warn" | "danger";

export function RunStatePill({ deck, running, connected }: { deck: DeckState; running: boolean; connected: boolean }) {
  const t = useT();
  const phase = swarmPhase(deck);
  const need = Math.max(1, deck.expectedFlags || 1);
  const collectUnknown = !!deck.multiFlag && need <= 1;
  const piStandby = deck.driverKind === "local_pi" && (deck.finished || deck.localPiSession?.turnActive === false);
  const piSolved = phase === "solved"
    || deck.solved
    || (!deck.platformConfirmationRequired && !collectUnknown && deck.flags.length >= need)
    || (deck.finished && !deck.platformConfirmationRequired && collectUnknown && deck.flags.length > 0);
  const guard = deck.productionGuard;
  const guardStopToken = (guard?.stop_status || guard?.emergency_stop_receipt?.status || "").toLowerCase();
  const hasPendingGuardApproval = Boolean(
    guard?.enabled
    && !deck.finished
    && guard.pending_approvals.some((item) => item.status === "pending" || item.status === "resolving"),
  );
  const guardState: { tone: Tone; label: string } | null = guard?.enabled
    ? guardStopToken === "emergency_stopping"
      ? { tone: "danger", label: t("rv.state.emergencyStopping") }
      : guardStopToken === "stop_partial" || guardStopToken === "partial"
        ? { tone: "danger", label: t("rv.state.stopPartial") }
        : guardStopToken === "stop_unknown" || guardStopToken === "unknown"
          ? { tone: "danger", label: t("rv.state.stopUnknown") }
          : hasPendingGuardApproval
            ? { tone: "warn", label: t("rv.state.waitingApproval") }
            : null
    : null;
  const state: { tone: Tone; label: string } = guardState
    ? guardState
    : deck.preparing
      ? { tone: "live", label: t("rv.state.preparing") }
      : phase === "paused"
        ? { tone: "paused", label: t("rv.state.paused") }
        : deck.driverKind === "local_pi"
          ? (piSolved
              ? (piStandby ? { tone: "solved", label: t("rv.state.solved") } : { tone: "live", label: t("rv.state.live") })
              : deck.finished
                ? { tone: "done", label: t("rv.state.finished") }
                : piStandby ? { tone: "done", label: t("rv.state.idle") } : { tone: "live", label: t("rv.state.live") })
          : running
            ? (connected ? { tone: "live", label: t("rv.state.live") } : { tone: "warn", label: t("rv.state.reconnecting") })
            : phase === "solved"
              ? { tone: "solved", label: t("rv.state.solved") }
              : phase === "goal_met"
                ? { tone: "solved", label: t("rv.state.goalMet") }
                : { tone: "done", label: t("rv.state.finished") };
  const elapsed = useElapsed(
    deck.startedAt,
    deck.finishedAt,
    deck.activeDurationMs,
    deck.activeSince,
    phase === "solved" || phase === "goal_met" || phase === "finished",
  );
  if (!deck.started) return null;
  return (
    <span className={`rv-state tone-${state.tone}`}>
      <span className="rv-state-dot" aria-hidden="true" />
      <span>{state.label}</span>
      {elapsed && <time className="rv-state-time" aria-label={t("meta.elapsed")}>{elapsed}</time>}
    </span>
  );
}
