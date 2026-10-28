"use client";

import { useMemo, useRef, type CSSProperties, type KeyboardEvent as ReactKeyboardEvent, type PointerEvent as ReactPointerEvent } from "react";
import { TraceGlyph } from "./traceIcons";
import { useCopy } from "./traceCopy";
import { formatClock, formatDuration, niceRange, specEdge, toRealTime, type TraceScenario } from "./traceModel";
import { useClockValue } from "./traceShared";

const BUCKETS = 72;

export interface PlaybackControls {
  playing: boolean;
  speed: number;
  follow: boolean;
}

export function TracePlayback({
  scenario,
  mode,
  controls,
  band,
  onPlay,
  onPause,
  onSeek,
  onSpeed,
  onLatest,
  onPrevError,
  onNextError,
  onHoverTime,
}: {
  scenario: TraceScenario;
  mode: "live" | "replay";
  controls: PlaybackControls;
  band: { from: number; to: number } | null;
  onPlay: () => void;
  onPause: () => void;
  onSeek: (time: number, phase: "start" | "move" | "end") => void;
  onSpeed: (speed: number) => void;
  onLatest: () => void;
  onPrevError: () => void;
  onNextError: () => void;
  onHoverTime: (time: number | null) => void;
}) {
  const copy = useCopy();
  const cursorTenths = useClockValue((clock) => Math.floor(clock.cursor() / 100), 0);
  // Live heads move every frame; whole seconds are enough for the scrubber's layout.
  const headValue = useClockValue((clock) => (mode === "replay" ? clock.head() : Math.ceil(clock.head() / 1000) * 1000), 0);
  const head = Math.max(1, headValue);
  const scale = mode === "replay" ? head : niceRange(head + 1000, 30_000);
  const speeds = scenario.duration > 1_200_000 ? [1, 10, 30, 60] : scenario.duration > 240_000 ? [1, 4, 10, 20] : [1, 2, 4, 8];
  const trackRef = useRef<HTMLDivElement>(null);
  const dragging = useRef(false);

  const histogram = useMemo(() => {
    const counts = new Array<number>(BUCKETS).fill(0);
    const errors = new Array<boolean>(BUCKETS).fill(false);
    for (const event of scenario.events) {
      if (event.ts > head) break;
      counts[Math.min(BUCKETS - 1, Math.floor((event.ts / scale) * BUCKETS))]++;
    }
    const failures: number[] = [];
    for (const span of scenario.spans) {
      const at = specEdge(span);
      if (!span.failed || at > head) continue;
      failures.push(at);
      errors[Math.min(BUCKETS - 1, Math.floor((at / scale) * BUCKETS))] = true;
    }
    const max = Math.max(1, ...counts);
    return { counts, errors, max, failures };
  }, [scenario, head, scale]);

  const timeAt = (clientX: number) => {
    const rect = trackRef.current?.getBoundingClientRect();
    if (!rect) return 0;
    return Math.max(0, Math.min(head, ((clientX - rect.left) / rect.width) * scale));
  };
  const down = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (event.button !== 0) return;
    dragging.current = true;
    event.currentTarget.setPointerCapture(event.pointerId);
    onSeek(timeAt(event.clientX), "start");
  };
  const move = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (dragging.current) onSeek(timeAt(event.clientX), "move");
    else onHoverTime(timeAt(event.clientX));
  };
  const up = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (!dragging.current) return;
    dragging.current = false;
    onSeek(timeAt(event.clientX), "end");
  };
  const jump = (time: number) => {
    onSeek(time, "start");
    onSeek(time, "end");
  };
  const key = (event: ReactKeyboardEvent) => {
    const step = event.shiftKey ? 5000 : 1000;
    if (event.key === "ArrowLeft") jump(Math.max(0, cursorTenths * 100 - step));
    else if (event.key === "ArrowRight") jump(Math.min(head, cursorTenths * 100 + step));
    else if (event.key === "Home") jump(0);
    else if (event.key === "End") onLatest();
    else return;
    event.preventDefault();
    event.stopPropagation();
  };

  const atHead = controls.follow || cursorTenths * 100 >= head - 100;
  const playing = controls.playing || controls.follow;
  const cursor = cursorTenths * 100;
  const gaps = scenario.gaps;
  const cursorLabel = formatClock(toRealTime(gaps, cursor));
  return (
    <div className="rp" data-clock="all" style={{ "--sc": scale, "--head": head } as CSSProperties}>
      <button type="button" className="rp-btn rp-play" onClick={playing ? onPause : onPlay} aria-label={playing ? copy.pause : copy.play} title={playing ? copy.pause : copy.play}>
        <TraceGlyph name={playing ? "pause" : "play"} size={14} />
      </button>
      <span className="rp-time">
        <b>{cursorLabel}</b>
        <span>/ {formatClock(toRealTime(gaps, head))}</span>
      </span>
      <div
        ref={trackRef}
        className="rp-track"
        role="slider"
        tabIndex={0}
        aria-label={copy.progress}
        aria-valuemin={0}
        aria-valuemax={Math.round(head)}
        aria-valuenow={Math.round(cursor)}
        aria-valuetext={cursorLabel}
        onPointerDown={down}
        onPointerMove={move}
        onPointerUp={up}
        onPointerCancel={up}
        onPointerLeave={() => onHoverTime(null)}
        onKeyDown={key}
      >
        <div className="rp-hist" aria-hidden="true">
          {histogram.counts.map((count, index) => (
            <i
              key={index}
              data-error={histogram.errors[index] || undefined}
              data-future={(index + 0.5) * (scale / BUCKETS) > cursor || undefined}
              style={{ height: count ? `${18 + (count / histogram.max) * 82}%` : 0 }}
            />
          ))}
        </div>
        {gaps.map((gap) => (
          <span key={gap.start} className="rp-gap" style={{ "--a": gap.at, "--b": gap.at + gap.width } as CSSProperties} title={copy.idleHint(formatDuration(gap.end - gap.start))} />
        ))}
        {mode === "live" && <span className="rp-unknown" aria-hidden="true" />}
        {band && <span className="rp-band" style={{ "--a": band.from, "--b": band.to } as CSSProperties} aria-hidden="true" />}
        {histogram.failures.map((at, index) => (
          <span key={`${at}:${index}`} className="rp-fail" style={{ "--t": at } as CSSProperties} aria-hidden="true" />
        ))}
        <span className="rp-played" aria-hidden="true" />
        <span className="rp-handle" aria-hidden="true" />
      </div>
      <div className="rp-errors">
        <button type="button" className="rp-btn" onClick={onPrevError} aria-label={copy.prevError} title={copy.prevError} disabled={!histogram.failures.some((at) => at < cursor - 50)}>
          <TraceGlyph name="prevError" size={13} />
        </button>
        <button type="button" className="rp-btn" onClick={onNextError} aria-label={copy.nextError} title={copy.nextError} disabled={!histogram.failures.some((at) => at > cursor + 50)}>
          <TraceGlyph name="nextError" size={13} />
        </button>
      </div>
      <div className="rp-speed" role="radiogroup" aria-label={copy.speed}>
        {speeds.map((speed) => (
          <button
            type="button"
            key={speed}
            role="radio"
            aria-checked={controls.speed === speed}
            data-on={controls.speed === speed || undefined}
            onClick={() => onSpeed(speed)}
          >
            {speed}×
          </button>
        ))}
      </div>
      <button type="button" className="rp-latest" data-on={(mode === "live" && controls.follow) || undefined} disabled={atHead && (mode === "replay" || controls.follow)} onClick={onLatest} title={copy.latestHint}>
        <TraceGlyph name="live" size={13} />
        <span>{mode === "live" ? copy.live : copy.end}</span>
      </button>
    </div>
  );
}
