"use client";

import type { CSSProperties } from "react";

import { Icon } from "@/components/Icon";
import type { CollaborationRelation, CollaborationRelationKind } from "@/lib/agentCollaboration";
import { useT } from "@/lib/i18n";

import type { AgentDisplay } from "./collaborationPresentation";
import { relationLabel } from "./collaborationPresentation";

/** Knowledge-flow kinds only; dispatch / report / lock would drown the matrix. */
export const FLOW_KINDS = ["handoff", "review", "verify"] as const satisfies readonly CollaborationRelationKind[];

const FLOW_KIND_SET = new Set<CollaborationRelationKind>(FLOW_KINDS);

export type FlowPart = {
  kind: CollaborationRelationKind;
  count: number;
  relationId: string;
};

export type FlowPair = {
  sourceId: string;
  targetId: string;
  count: number;
  kind: CollaborationRelationKind;
  relationId: string;
  parts: FlowPart[];
};

function pairKey(sourceId: string, targetId: string): string {
  return `${sourceId}\0${targetId}`;
}

function dominantPart(parts: FlowPart[]): FlowPart {
  let best = parts[0];
  for (let i = 1; i < parts.length; i += 1) {
    if (parts[i].count > best.count) best = parts[i];
  }
  return best;
}

/** Aggregate visible knowledge-flow relations by (source, target). */
export function buildFlowPairs(relations: CollaborationRelation[]): { pairs: Map<string, FlowPair>; max: number } {
  const pairs = new Map<string, FlowPair>();
  let max = 0;
  for (const relation of relations) {
    if (!FLOW_KIND_SET.has(relation.kind)) continue;
    const key = pairKey(relation.source, relation.target);
    const prior = pairs.get(key);
    if (!prior) {
      const pair: FlowPair = {
        sourceId: relation.source,
        targetId: relation.target,
        count: relation.count,
        kind: relation.kind,
        relationId: relation.id,
        parts: [{ kind: relation.kind, count: relation.count, relationId: relation.id }],
      };
      pairs.set(key, pair);
      max = Math.max(max, pair.count);
      continue;
    }
    prior.parts.push({ kind: relation.kind, count: relation.count, relationId: relation.id });
    prior.count += relation.count;
    const lead = dominantPart(prior.parts);
    prior.kind = lead.kind;
    prior.relationId = lead.relationId;
    max = Math.max(max, prior.count);
  }
  return { pairs, max };
}

function depthOf(count: number, max: number): string {
  if (!max) return "0%";
  return `${Math.round((count / max) * 72)}%`;
}

export function FlowMatrix({
  agentIds,
  displays,
  relations,
  selectedRelationId,
  onSelectRelation,
}: {
  agentIds: string[];
  displays: Map<string, AgentDisplay>;
  relations: CollaborationRelation[];
  selectedRelationId: string | null;
  onSelectRelation: (relationId: string) => void;
}) {
  const t = useT();
  const { pairs, max } = buildFlowPairs(relations);
  const n = agentIds.length;
  const outgoing = new Map<string, number>();
  const incoming = new Map<string, number>();
  let grand = 0;
  for (const pair of pairs.values()) {
    outgoing.set(pair.sourceId, (outgoing.get(pair.sourceId) ?? 0) + pair.count);
    incoming.set(pair.targetId, (incoming.get(pair.targetId) ?? 0) + pair.count);
    grand += pair.count;
  }

  if (!n || pairs.size === 0) {
    return (
      <div className="collab-flow" role="status">
        <div className="collab-flow-empty">
          <Icon name="network" size={24} />
          <strong>{t("collab.flow.empty")}</strong>
          <span>{t("collab.flow.emptyHint")}</span>
        </div>
      </div>
    );
  }

  const nameOf = (id: string) => displays.get(id)?.title || id;

  return (
    <div className="collab-flow" role="region" aria-label={t("collab.flow")}>
      <div
        className="collab-flow-grid"
        role="grid"
        aria-rowcount={n + 2}
        aria-colcount={n + 2}
        style={{
          "--flow-n": n,
        } as CSSProperties}
      >
        <span className="collab-flow-corner" role="columnheader">{t("collab.flow")}</span>
        {agentIds.map((id) => (
          <span key={`col-${id}`} className="collab-flow-col" role="columnheader" title={nameOf(id)}>
            {nameOf(id)}
          </span>
        ))}
        <span className="collab-flow-col total" role="columnheader">{t("collab.flow.out")}</span>

        {agentIds.flatMap((sourceId) => {
          const rowHead = (
            <span key={`row-${sourceId}`} className="collab-flow-row-head" role="rowheader" title={nameOf(sourceId)}>
              {nameOf(sourceId)}
            </span>
          );
          const cells = agentIds.map((targetId) => {
            const pair = pairs.get(pairKey(sourceId, targetId));
            if (!pair) {
              return <span key={`${sourceId}:${targetId}`} className="collab-flow-cell empty" role="gridcell" />;
            }
            const selected = pair.relationId === selectedRelationId;
            const kindText = pair.parts
              .map((part) => `${relationLabel(part.kind, t)} ×${part.count}`)
              .join(" · ");
            const label = t("collab.flow.cell", {
              source: nameOf(sourceId),
              target: nameOf(targetId),
              kind: relationLabel(pair.kind, t),
              n: pair.count,
            });
            return (
              <button
                key={`${sourceId}:${targetId}`}
                type="button"
                role="gridcell"
                className={`collab-flow-cell kind-${pair.kind} ${selected ? "selected" : ""}`}
                style={{ "--flow-depth": depthOf(pair.count, max) } as CSSProperties}
                aria-label={label}
                title={`${label}${pair.parts.length > 1 ? ` · ${kindText}` : ""}`}
                aria-selected={selected}
                onClick={() => onSelectRelation(pair.relationId)}
              >
                {pair.parts.length > 1 && (
                  <i className="collab-flow-stack" aria-hidden="true">
                    {pair.parts.map((part) => (
                      <b key={part.kind} className={`kind-${part.kind}`} style={{ flex: part.count }} />
                    ))}
                  </i>
                )}
                <span>{pair.count}</span>
              </button>
            );
          });
          const rowTotal = (
            <span key={`out-${sourceId}`} className="collab-flow-cell total" role="gridcell">
              {outgoing.get(sourceId) || ""}
            </span>
          );
          return [rowHead, ...cells, rowTotal];
        })}

        <span className="collab-flow-row-head total" role="rowheader">{t("collab.flow.in")}</span>
        {agentIds.map((id) => (
          <span key={`in-${id}`} className="collab-flow-cell total" role="gridcell">{incoming.get(id) || ""}</span>
        ))}
        <span className="collab-flow-cell total grand" role="gridcell">{grand || ""}</span>
      </div>
    </div>
  );
}
