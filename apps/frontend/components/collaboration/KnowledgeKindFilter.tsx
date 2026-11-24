"use client";

import { Chip, Icon } from "@muteki/ui";
import type { CollaborationKnowledgeItem, CollaborationKnowledgeKind } from "@/lib/agentCollaboration";
import { useT } from "@/lib/i18n";

import type { RunCanvasMode } from "@/lib/swarmProjection";

import { KNOWLEDGE_KINDS, knowledgeIcon, knowledgeLabel, knowledgeKindsForMode } from "./collaborationPresentation";

export function knowledgeKindCounts(items: CollaborationKnowledgeItem[]): Map<CollaborationKnowledgeKind, number> {
  const counts = new Map<CollaborationKnowledgeKind, number>();
  for (const item of items) counts.set(item.kind, (counts.get(item.kind) || 0) + 1);
  return counts;
}

export function KnowledgeKindChips({
  counts,
  selected,
  onToggle,
  mode,
}: {
  counts: Map<CollaborationKnowledgeKind, number>;
  selected: Set<CollaborationKnowledgeKind>;
  onToggle: (kind: CollaborationKnowledgeKind) => void;
  mode?: RunCanvasMode;
}) {
  const t = useT();
  const catalog = mode ? knowledgeKindsForMode(mode) : KNOWLEDGE_KINDS;
  const kinds = catalog.filter((kind) => (counts.get(kind) || 0) > 0 || selected.has(kind));
  if (kinds.length === 0) return null;
  return (
    <div className="collab-kind-chips" role="group" aria-label={t("collab.kindFilter")}>
      {kinds.map((kind) => {
        const count = counts.get(kind) || 0;
        const on = selected.has(kind);
        return (
          <Chip
            key={kind}
            size="sm"
            variant="soft"
            tone={on ? "accent" : "neutral"}
            selected={on}
            className={`collab-kind-chip${on ? " on" : ""}`}
            aria-pressed={on}
            onClick={() => onToggle(kind)}
          >
            <Icon name={knowledgeIcon(kind)} size={11} />
            <span>{knowledgeLabel(kind, t, mode)}</span>
            <span className="cx-tabular opacity-75">{count}</span>
          </Chip>
        );
      })}
    </div>
  );
}
