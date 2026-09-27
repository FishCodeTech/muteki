"use client";

import { Panel, useReactFlow } from "@xyflow/react";

import { Icon } from "@/components/Icon";
import { RELATION_RECENT_WINDOW_MS, type CollaborationRelationKind } from "@/lib/agentCollaboration";
import { MOTION } from "@/lib/agentCollaborationLayout";
import { formatClock } from "@/lib/format";
import { useT } from "@/lib/i18n";
import type { LaneStatusKind } from "@/lib/workerLanePresentation";

import { RELATION_KINDS, relationLabel } from "./collaborationPresentation";
import { RelationSwatch } from "./RelationSwatch";

/** Status dot samples: one representative LaneStatusKind per colour the cards use. */
const LEGEND_STATES: Array<{ kind: LaneStatusKind; label: string }> = [
  { kind: "thinking", label: "collab.legend.active" },
  { kind: "waiting", label: "collab.legend.waiting" },
  { kind: "error", label: "collab.legend.issue" },
  { kind: "solved", label: "collab.legend.solved" },
  { kind: "offline", label: "collab.legend.offline" },
];

function CanvasLegend({ relationKinds, onToggleRelation }: {
  relationKinds: Set<CollaborationRelationKind>;
  onToggleRelation: (kind: CollaborationRelationKind) => void;
}) {
  const t = useT();
  return (
    <div className="collab-canvas-legend" role="group" aria-label={t("collab.legend")}>
      <h4>{t("collab.legend.relations")}</h4>
      <div className="collab-legend-relations">
        {RELATION_KINDS.map((kind) => {
          const on = relationKinds.has(kind);
          return (
            <button key={kind} type="button" className={on ? "on" : ""} aria-pressed={on} onClick={() => onToggleRelation(kind)}>
              <RelationSwatch kind={kind} />
              <span>{relationLabel(kind, t)}</span>
            </button>
          );
        })}
      </div>
      <p>{t("collab.legend.arrow")}</p>
      <p>{t("collab.legend.recent", { s: RELATION_RECENT_WINDOW_MS / 1000 })}</p>
      <h4>{t("collab.legend.states")}</h4>
      <div className="collab-legend-states">
        {LEGEND_STATES.map((state) => (
          <span key={state.kind} className={`collab-agent-state state-${state.kind}`}><i /><span>{t(state.label)}</span></span>
        ))}
      </div>
      <p>{t("collab.legend.engineBar")}</p>
    </div>
  );
}

/**
 * The single bottom bar: zoom / fit, live state, counts, the relayout prompt
 * and the legend toggle. Fit goes through the canvas' requestFit so this and
 * the toolbar button frame the graph identically.
 */
export function CanvasBar({
  running,
  finished,
  finishedAt,
  agentCount,
  relationCount,
  layoutStale,
  reduceMotion,
  legendOpen,
  relationKinds,
  onFit,
  onRelayout,
  onToggleLegend,
  onToggleRelation,
  replayNote,
  presenceToasts,
  onPresenceClick,
}: {
  running: boolean;
  finished: boolean;
  finishedAt?: number;
  agentCount: number;
  relationCount: number;
  layoutStale: boolean;
  reduceMotion: boolean;
  legendOpen: boolean;
  relationKinds: Set<CollaborationRelationKind>;
  onFit: () => void;
  onRelayout: () => void;
  onToggleLegend: () => void;
  onToggleRelation: (kind: CollaborationRelationKind) => void;
  replayNote?: string;
  presenceToasts?: Array<{ id: string; agentId: string; text: string }>;
  onPresenceClick?: (agentId: string) => void;
}) {
  const t = useT();
  const { zoomIn, zoomOut } = useReactFlow();
  const duration = reduceMotion ? 0 : MOTION.center;
  const statusKey = running ? "collab.live" : finished ? "collab.complete" : "collab.standby";
  return (
    <Panel position="bottom-left" className="collab-canvas-bar" role="toolbar" aria-label={t("collab.a11y.controls")}>
      <div className="collab-canvas-zoom">
        <button type="button" aria-label={t("collab.a11y.zoomOut")} title={t("collab.a11y.zoomOut")} onClick={() => void zoomOut({ duration })}>
          <Icon name="minus" size={12} />
        </button>
        <button type="button" aria-label={t("collab.a11y.zoomIn")} title={t("collab.a11y.zoomIn")} onClick={() => void zoomIn({ duration })}>
          <Icon name="plus" size={12} />
        </button>
        <button type="button" aria-label={t("collab.a11y.fitView")} title={t("collab.a11y.fitView")} onClick={onFit}>
          <Icon name="crosshair" size={12} />
        </button>
      </div>
      <span className="collab-canvas-live">
        <i className={running ? "live" : ""} />
        {t(statusKey)}
        {finished && <time>{formatClock(finishedAt, "—")}</time>}
        {replayNote && <small>{replayNote}</small>}
      </span>
      <span className="collab-canvas-stat"><b>{agentCount}</b><small>{t("collab.agents")}</small></span>
      <span className="collab-canvas-stat"><b>{relationCount}</b><small>{t("collab.relationships")}</small></span>
      {presenceToasts?.map((toast) => (
        <button
          type="button"
          className="collab-presence-toast"
          key={toast.id}
          onClick={() => onPresenceClick?.(toast.agentId)}
        >
          {toast.text}
        </button>
      ))}
      {layoutStale && (
        <button type="button" className="collab-relayout" onClick={onRelayout}>
          <Icon name="refresh" size={11} />{t("collab.relayout")}
        </button>
      )}
      <button
        type="button"
        className={`collab-legend-toggle ${legendOpen ? "on" : ""}`}
        aria-pressed={legendOpen}
        aria-expanded={legendOpen}
        onClick={onToggleLegend}
      >
        <Icon name="info" size={12} />{t("collab.legend")}
      </button>
      {legendOpen && <CanvasLegend relationKinds={relationKinds} onToggleRelation={onToggleRelation} />}
    </Panel>
  );
}

/** Bottom-right toggle for the mini map; the map itself renders just above it. */
export function MiniMapToggle({ open, onToggle }: { open: boolean; onToggle: () => void }) {
  const t = useT();
  const label = t(open ? "collab.hideMiniMap" : "collab.showMiniMap");
  return (
    <Panel position="bottom-right" className="collab-minimap-toggle">
      <button type="button" className={open ? "on" : ""} aria-pressed={open} aria-label={label} title={label} onClick={onToggle}>
        <Icon name="grid" size={13} />
      </button>
    </Panel>
  );
}
