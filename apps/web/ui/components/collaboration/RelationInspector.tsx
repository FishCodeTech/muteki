"use client";

import { useEffect, useState } from "react";
import { Button } from "@heroui/react";

import { Icon } from "@/components/Icon";
import {
  OPERATOR_ID,
  type CollaborationAgent,
  type CollaborationKnowledgeItem,
  type CollaborationRelation,
} from "@/lib/agentCollaboration";
import type { BlackboardIntent } from "@/lib/events";
import type { RunCanvasMode } from "@/lib/swarmProjection";
import { useT } from "@/lib/i18n";

import { relationLabel, roleLabel, verifyBasisLabel } from "./collaborationPresentation";
import { KnowledgeDetail, KnowledgeRow, knowledgeProposerLabel, knowledgeVerifierLabel } from "./KnowledgeRow";

export function RelationInspector({
  relation,
  source,
  target,
  references,
  knowledge,
  intents,
  actorName,
  onBack,
  onSelectAgent,
  onSelectKnowledge,
  onOpenFact,
  onOpenPoc,
  onClosePanel,
  canvasMode,
}: {
  relation: CollaborationRelation;
  source: CollaborationAgent;
  target: CollaborationAgent;
  references: CollaborationKnowledgeItem[];
  knowledge: CollaborationKnowledgeItem[];
  intents: BlackboardIntent[];
  actorName: (id?: string) => string;
  onBack: () => void;
  onSelectAgent: (id: string) => void;
  onSelectKnowledge: (item: CollaborationKnowledgeItem) => void;
  onOpenFact?: (seq: number) => void;
  onOpenPoc?: (id: string) => void;
  onClosePanel?: () => void;
  canvasMode?: RunCanvasMode;
}) {
  const t = useT();
  const [activeRefId, setActiveRefId] = useState(references[0]?.id);
  useEffect(() => {
    setActiveRefId((current) => (references.some((item) => item.id === current) ? current : references[0]?.id));
  }, [references, relation.id]);
  const active = references.find((item) => item.id === activeRefId);
  const byOperator = relation.initiator === OPERATOR_ID;
  const spawnPhase = relation.kind === "dispatch" ? (target.spawnPhase || target.phase) : "";
  const sourceName = actorName(source.id);
  const targetName = actorName(target.id);
  // An operator-added dispatch keeps the coordinator on the route card, but the
  // sentence names the operator, matching "added by operator" below it.
  const directionSource = byOperator ? actorName(OPERATOR_ID) : sourceName;
  return (
    <>
      <div className="collab-relation-head" tabIndex={-1}>
        <Button variant="ghost" isIconOnly aria-label={t("collab.action.back")} onClick={onBack}>
          <Icon name="arrowRight" size={14} className="collab-back-icon" />
        </Button>
        <span><small>{t("collab.relationship")}</small><strong>{relationLabel(relation.kind, t)}</strong></span>
        <b>{relation.count}</b>
        <button type="button" className="collab-panel-close" aria-label={t("collab.hideDetails")} onClick={onClosePanel}>
          <Icon name="x" size={14} />
        </button>
      </div>
      <div className="collab-inspector-body">
        <div className={`collab-relation-route relation-${relation.kind}`}>
          <button type="button" aria-label={t("collab.action.selectAgent", { name: sourceName })} onClick={() => onSelectAgent(source.id)}>
            <b>{sourceName}</b><small>{roleLabel(source, t)}</small>
          </button>
          <i><Icon name="arrowRight" size={15} /></i>
          <button type="button" aria-label={t("collab.action.selectAgent", { name: targetName })} onClick={() => onSelectAgent(target.id)}>
            <b>{targetName}</b><small>{roleLabel(target, t)}</small>
          </button>
        </div>
        <p className="collab-relation-direction">
          {t(`collab.direction.${relation.kind}`, { source: directionSource, target: targetName })}
        </p>
        {(byOperator || (spawnPhase && !references.length)) && (
          <dl className="collab-runtime-detail">
            {byOperator && <><dt>{t("collab.field.agent")}</dt><dd>{t("collab.byOperator")}</dd></>}
            {spawnPhase && !references.length && <><dt>{t("collab.field.spawnPhase")}</dt><dd>{spawnPhase}</dd></>}
          </dl>
        )}
        <section className="collab-detail-section">
          <h3>{t("collab.relationBasis")}</h3>
          {references.length ? (
            <div className="collab-inspector-list">
              {references.map((item, index) => (
                <KnowledgeRow
                  key={`${item.id}:${index}`}
                  item={item}
                  actor={actorName(item.agentId)}
                  selected={item.id === activeRefId}
                  onSelect={setActiveRefId}
                  kindLabel={relation.kind === "verify" ? verifyBasisLabel(item.id, t) : undefined}
                  mode={canvasMode}
                />
              ))}
            </div>
          ) : (
            <div className="collab-section-empty">
              {relation.kind === "dispatch" ? t("collab.dispatchBasis") : t("collab.noBasis")}
            </div>
          )}
        </section>
        {active && (
          <KnowledgeDetail
            item={active}
            actor={actorName(active.agentId)}
            proposer={knowledgeProposerLabel(active, actorName)}
            verifier={knowledgeVerifierLabel(active, actorName, t)}
            knowledge={knowledge}
            intents={intents}
            onSelectKnowledge={onSelectKnowledge}
            onOpenFact={onOpenFact}
            onOpenPoc={onOpenPoc}
            onOpenAgentKnowledge={() => onSelectKnowledge(active)}
            mode={canvasMode}
          />
        )}
      </div>
    </>
  );
}
