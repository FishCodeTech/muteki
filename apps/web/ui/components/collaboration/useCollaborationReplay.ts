"use client";

import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import type { AgentCollaborationModel } from "@/lib/agentCollaboration";
import {
  projectCollaborationAsOf,
  replayBounds,
  replayMarks,
  replayTruncated,
} from "@/lib/agentCollaborationReplay";
import type { DeckState } from "@/lib/events";

/** Play a finished run's span in 8–20s of wall time. */
export function useCollaborationReplay(deck: DeckState, model: AgentCollaborationModel) {
  const startedAt = deck.startedAt;
  const finishedAt = deck.finishedAt;
  const span = useMemo(() => replayBounds({ startedAt, finishedAt }), [finishedAt, startedAt]);
  const canReplay = deck.finished && span.end > span.start;
  const [asOf, setAsOf] = useState(span.end);
  const [playing, setPlaying] = useState(false);
  const asOfRef = useRef(span.end);
  asOfRef.current = asOf;

  useEffect(() => {
    setAsOf(span.end);
    asOfRef.current = span.end;
    setPlaying(false);
  }, [deck.runId, span.end]);

  useEffect(() => {
    if (!playing || !canReplay) return;
    let raf = 0;
    let last = performance.now();
    let current = asOfRef.current;
    const spanMs = span.end - span.start;
    const wall = Math.max(8000, Math.min(20000, spanMs / 50));
    const rate = spanMs / wall;
    const loop = (now: number) => {
      const dt = now - last;
      last = now;
      if (current >= span.end) current = span.start;
      current = Math.min(span.end, current + dt * rate);
      const snapped = current >= span.end ? span.end : Math.round(current / 250) * 250;
      asOfRef.current = snapped;
      setAsOf(snapped);
      if (current >= span.end) {
        setPlaying(false);
        return;
      }
      raf = requestAnimationFrame(loop);
    };
    raf = requestAnimationFrame(loop);
    return () => cancelAnimationFrame(raf);
  }, [canReplay, playing, span.end, span.start]);

  const replaying = canReplay && asOf < span.end;
  const view = useMemo(
    () => (replaying ? projectCollaborationAsOf(model, asOf) : model),
    [asOf, model, replaying],
  );
  const marks = useMemo(() => replayMarks(deck.blackboard.events, deck.blackboard.reasonRuns), [deck.blackboard.events, deck.blackboard.reasonRuns]);
  const truncated = canReplay && replayTruncated(deck);

  const onScrub = useCallback((next: number) => {
    setPlaying(false);
    asOfRef.current = next;
    setAsOf(next);
  }, []);

  const onTogglePlay = useCallback(() => {
    if (!canReplay) return;
    setPlaying((on) => {
      if (on) return false;
      if (asOfRef.current >= span.end) {
        asOfRef.current = span.start;
        setAsOf(span.start);
      }
      return true;
    });
  }, [canReplay, span.end, span.start]);

  return { asOf, canReplay, marks, onScrub, onTogglePlay, playing, replaying, span, truncated, view };
}
