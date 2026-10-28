"use client";

import { Button, Callout } from "@/components/chat/ui";
import { evidenceRef, findingRef, type ResearchEvidence, type ResearchFinding, type ResearchRef } from "@/lib/srcResearch";
import { ArtifactInspector } from "./ArtifactInspector";
import { RequestWorkspace } from "./RequestWorkspace";
import { PaneHeader, RefChip } from "./SrcResearchParts";
import styles from "./srcResearch.module.css";

/** Raw evidence in the main work area; the caller keeps filters and selection for the return trip. */
export function SrcResearchEvidence({ taskId, evidence, finding, backLabel, inContext, onBack, onAddContext, onOpenRef }: {
  taskId: string;
  evidence: ResearchEvidence;
  finding?: ResearchFinding;
  backLabel: string;
  inContext: boolean;
  onBack: () => void;
  onAddContext: (...refs: ResearchRef[]) => void;
  onOpenRef: (ref: ResearchRef) => void;
}) {
  const ref = evidenceRef(evidence);
  const artifactId = evidence.artifact_id || evidence.sha256 || evidence.id || "";
  return <div className={styles.evidenceView} onKeyDown={event => {
    if (event.key === "Escape" && !event.defaultPrevented && !(event.target as HTMLElement).closest("input,textarea,select,[contenteditable='true']")) { event.preventDefault(); onBack(); }
  }}>
    <PaneHeader onBack={onBack} backLabel={backLabel}
      eyebrow={evidence.exchange_id ? "原始证据 · 请求与响应" : "原始证据 · Artifact 原文"}
      title={evidence.exchange_id || artifactId}
      actions={<Button size="sm" variant={inContext ? "soft" : "secondary"} icon={inContext ? "check" : "messageCircle"} disabled={inContext} onClick={() => onAddContext(ref)}>{inContext ? "已在对话上下文" : "加入对话上下文"}</Button>} />
    <div className={styles.evidenceMeta}>
      {finding ? <><span>引用自</span><RefChip refValue={findingRef(finding)} onOpen={onOpenRef} /></> : null}
      {evidence.source_run_id ? <><span>来源 Run</span><RefChip compact refValue={{ kind: "run", id: evidence.source_run_id, label: evidence.source_run_id }} onOpen={onOpenRef} /></> : null}
      {evidence.sha256 ? <code title={evidence.sha256}>sha256:{evidence.sha256.slice(0, 16)}…</code> : null}
    </div>
    <div className={styles.evidenceBody}>
      {evidence.exchange_id ? <RequestWorkspace taskId={taskId} initialExchangeId={evidence.exchange_id} />
        : artifactId ? <ArtifactInspector taskId={taskId} artifactId={artifactId} />
          : <Callout tone="warning">此证据引用缺少 exchange 或 artifact 编号，无法定位原文。</Callout>}
    </div>
  </div>;
}
