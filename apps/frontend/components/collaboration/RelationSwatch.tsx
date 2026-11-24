"use client";

import type { CollaborationRelationKind } from "@/lib/agentCollaboration";

export function RelationSwatch({ kind }: { kind: CollaborationRelationKind }) {
  return (
    <svg className={`collab-relation-swatch relation-${kind}`} width="24" height="8" viewBox="0 0 24 8" aria-hidden="true">
      <line x1="1" y1="4" x2="23" y2="4" />
    </svg>
  );
}
