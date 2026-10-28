"use client";

import { useEffect, useMemo, useRef, useState } from "react";
import type { DeckState } from "@/lib/events";
import { useLang, useT } from "@/lib/i18n";
import { runUsageAmount, runUsageTokens, useRunUsageLedger } from "@/lib/useRunUsageLedger";
import { formatTokens } from "./traceModel";
import { ActivityConsole, type ConsoleClock } from "./ActivityConsole";
import { createDeckTraceCache, scenarioFromDeck, type DeckTraceCache } from "./fromDeck";

/** Streaming text updates the deck many times a second; the timeline only needs a few frames of it. */
function useThrottled<T>(value: T, ms: number): T {
  const [state, setState] = useState(value);
  const last = useRef(0);
  useEffect(() => {
    if (!ms) {
      setState(value);
      return;
    }
    const wait = last.current + ms - Date.now();
    if (wait <= 0) {
      last.current = Date.now();
      setState(value);
      return;
    }
    const timer = window.setTimeout(() => {
      last.current = Date.now();
      setState(value);
    }, wait);
    return () => window.clearTimeout(timer);
  }, [value, ms]);
  return ms ? state : value;
}

export function ActivityTimeline({
  deck,
  running,
  focusSpeaker,
}: {
  deck: DeckState;
  running: boolean;
  focusSpeaker?: { id: string; nonce: number } | null;
}) {
  const { lang } = useLang();
  const t = useT();
  const view = useThrottled(deck, running ? 250 : 0);
  const cache = useRef<DeckTraceCache | null>(null);
  const { scenario, origin } = useMemo(() => {
    cache.current ??= createDeckTraceCache();
    return scenarioFromDeck(view, running, { lang, t, cache: cache.current });
  }, [view, running, lang, t]);
  const { data: usage, error: usageError } = useRunUsageLedger(deck.runId, deck.executionGeneration > 0 ? deck.executionGeneration : undefined);
  const summary = useMemo(() => {
    const en = lang === "en";
    const amount = usage ? runUsageAmount(usage.totals, en) : null;
    const totalTokens = usage?.totals.total_tokens;
    return {
      cost: amount
        ? <>{amount.amount}{amount.source ? <small className="rt-stat-sub"> {amount.source}</small> : null}</>
        : usageError ? (en ? "Read failed" : "读取失败") : (en ? "Loading" : "读取中"),
      tokens: totalTokens ?? null,
      tokenLabel: totalTokens == null
        ? runUsageTokens(usage?.totals, formatTokens, en)
        : <>{formatTokens(totalTokens)}{usage!.totals.token_coverage === "partial" ? <small className="rt-stat-sub"> {en ? "partial" : "部分"}</small> : null}</>,
    };
  }, [usage, usageError, lang]);
  const focus = useMemo(
    () => (focusSpeaker ? { spanId: `trace:agent:${focusSpeaker.id}`, nonce: focusSpeaker.nonce } : null),
    [focusSpeaker],
  );
  const live = running && origin != null;
  const clock = useMemo<ConsoleClock>(
    () => (live ? { mode: "live", origin: origin! } : { mode: "replay", startAt: "end" }),
    [live, origin],
  );
  return (
    <ActivityConsole
      className="rt-embedded"
      scenario={scenario}
      clock={clock}
      resetKey={view.runId}
      autoFold
      lang={lang}
      timeOrigin={origin}
      summary={summary}
      focus={focus}
    />
  );
}
