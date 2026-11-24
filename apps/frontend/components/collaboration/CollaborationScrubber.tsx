"use client";

import { useMemo } from "react";
import { Icon } from "@muteki/ui";
import { formatClock } from "@/lib/format";
import { useT } from "@/lib/i18n";
import type { ReplayMark } from "@/lib/agentCollaborationReplay";

export function CollaborationScrubber({
  start,
  end,
  value,
  playing,
  truncated,
  marks,
  onChange,
  onTogglePlay,
}: {
  start: number;
  end: number;
  value: number;
  playing: boolean;
  truncated: boolean;
  marks: ReplayMark[];
  onChange: (asOf: number) => void;
  onTogglePlay: () => void;
}) {
  const t = useT();
  const span = Math.max(1, end - start);
  const playLabel = t(playing ? "collab.replay.pause" : "collab.replay.play");
  const timeLabel = formatClock(value);
  const markButtons = useMemo(() => marks.map((mark) => {
    const pct = ((mark.ts - start) / span) * 100;
    if (pct < 0 || pct > 100) return null;
    const label = t(`collab.event.${mark.kind}`);
    return (
      <button
        key={mark.id}
        type="button"
        className={`kind-${mark.kind}`}
        style={{ left: `${pct}%` }}
        title={label}
        aria-label={`${label} · ${formatClock(mark.ts)}`}
        onClick={() => onChange(mark.ts)}
      />
    );
  }), [marks, onChange, span, start, t]);
  return (
    <div className="collab-scrubber" role="group" aria-label={t("collab.replay")}>
      <button
        type="button"
        className="collab-scrubber-play"
        aria-label={playLabel}
        title={playLabel}
        aria-pressed={playing}
        onClick={onTogglePlay}
      >
        <Icon name={playing ? "pause" : "play"} size={12} />
      </button>
      <div className="collab-scrubber-track">
        <div className="collab-scrubber-marks">
          {markButtons}
        </div>
        <input
          type="range"
          min={0}
          max={Math.ceil(span / 1000)}
          step={1}
          value={Math.min(Math.ceil(span / 1000), Math.round((value - start) / 1000))}
          aria-label={t("collab.replay")}
          aria-valuetext={timeLabel}
          onClick={(event) => event.currentTarget.focus({ preventScroll: true })}
          onChange={(event) => onChange(Math.min(end, start + Number(event.target.value) * 1000))}
        />
      </div>
      <time className="collab-scrubber-time" dateTime={new Date(value).toISOString()}>
        {timeLabel}
      </time>
      <p className="collab-scrubber-note">{t("collab.replay.appearance")}</p>
      {truncated && <p className="collab-scrubber-warn">{t("collab.replay.truncated")}</p>}
    </div>
  );
}
