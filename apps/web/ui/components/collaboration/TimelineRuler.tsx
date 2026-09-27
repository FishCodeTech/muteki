"use client";

import { type Node, type NodeProps } from "@xyflow/react";

import { formatClock } from "@/lib/format";
import { useT } from "@/lib/i18n";

export const TIMELINE_RULER_ID = "__timeline-ruler";

export type RulerNodeData = {
  startedAt: number;
  span: number;
};

export type RulerFlowNode = Node<RulerNodeData>;

const TICKS = [0, 10, 20, 30, 40, 50, 60, 70, 80, 90, 100] as const;

/** Time axis for timeline layout: one labelled tick every 10% of the run span. */
export function TimelineRuler({ data }: NodeProps<RulerFlowNode>) {
  const t = useT();
  return (
    <div className="collab-timeline-ruler" aria-label={t("collab.timeline.scale")}>
      {TICKS.map((pct) => (
        <span key={pct} style={{ left: `${pct}%` }}>
          <i />
          <time>{formatClock(data.startedAt + data.span * (pct / 100))}</time>
        </span>
      ))}
    </div>
  );
}
