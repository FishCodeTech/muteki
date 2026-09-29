import type {
  CollaborationAgent,
  CollaborationKnowledgeItem,
  CollaborationKnowledgeKind,
  CollaborationRelationKind,
} from "@/lib/agentCollaboration";
import type { IconName } from "@/lib/iconNames";
import { compactLaneStatus } from "@/lib/workerLanePresentation";
import {
  CTF_CANVAS_KIND_NAMES,
  PENTEST_CANVAS_KIND_NAMES,
  type RunCanvasMode,
} from "@/lib/swarmProjection";

export type Translate = (key: string, vars?: Record<string, string | number>) => string;

export type AgentDisplay = {
  title: string;
  initial: string;
  /** EngineLogo / --eng-* key; empty for coordinator or unknown workers. */
  engineKey: string;
  subtitle: string;
  engine: string;
  color: string;
};

export const RELATION_KINDS: CollaborationRelationKind[] = ["model", "dispatch", "handoff", "review", "verify", "report", "directive", "lock"];
export const KNOWLEDGE_KINDS: CollaborationKnowledgeKind[] = [
  "intent", "step", "fact", "candidate", "dead_end", "poc", "report",
  "review", "finding", "route", "branch", "directive", "flag", "lock", "goal",
];

export function knowledgeKindsForMode(mode: RunCanvasMode): CollaborationKnowledgeKind[] {
  return mode === "ctf"
    ? [...CTF_CANVAS_KIND_NAMES]
    : [...PENTEST_CANVAS_KIND_NAMES];
}

export function roleLabel(agent: CollaborationAgent, t: Translate): string {
  if (agent.role === "coordinator") return t("collab.role.coordinator");
  if (agent.role === "decision") return t("collab.role.decision");
  if (agent.role === "source") return t("collab.role.source");
  if (agent.role === "review") return t("collab.role.review");
  if (agent.role === "verifier") return t("collab.role.verifier");
  if (agent.phase.includes("race")) return t("collab.role.race");
  return t("collab.role.explore");
}

export function coordinatorStatus(agent: CollaborationAgent, t: Translate, mode?: RunCanvasMode): string {
  if (agent.statusKind === "solved") return t(mode === "pentest" ? "collab.legend.goalProven" : "worker.solved");
  if (agent.statusKind === "paused") return t("worker.paused");
  if (agent.statusKind === "error") return t("worker.error");
  if (!agent.online) return t("collab.complete");
  if (agent.statusKind === "waiting") return t("wlane.waiting");
  return t("collab.status.coordinating");
}

export function statusLabel(agent: CollaborationAgent, t: Translate, mode?: RunCanvasMode): string {
  if (agent.role === "source") return t("collab.source.recorded");
  if (agent.role === "decision") return t(agent.online ? "collab.decision.running" : "collab.decision.recorded");
  if (mode === "pentest" && agent.statusKind === "solved") return t("collab.legend.goalProven");
  return agent.role === "coordinator"
    ? coordinatorStatus(agent, t, mode)
    : compactLaneStatus(agent.presentation, agent.online, t);
}

export function knowledgeIcon(kind: CollaborationKnowledgeItem["kind"]): IconName {
  if (kind === "fact") return "checkCircle";
  if (kind === "observation") return "help";
  if (kind === "candidate") return "help";
  if (kind === "dead_end" || kind === "route") return "xCircle";
  if (kind === "intent" || kind === "step") return "target";
  if (kind === "goal") return "flag";
  if (kind === "poc") return "terminal";
  if (kind === "report") return "list";
  if (kind === "review") return "shieldAlert";
  if (kind === "finding") return "alert";
  if (kind === "branch") return "gitBranch";
  if (kind === "directive") return "send";
  return "flag";
}

export function knowledgeLabel(
  kind: CollaborationKnowledgeItem["kind"],
  t: Translate,
  mode?: RunCanvasMode,
): string {
  if (mode === "ctf") {
    if (kind === "intent" || kind === "step") return t("collab.knowledge.step");
    if (kind === "fact") return t("collab.knowledge.factRecord");
    if (kind === "observation") return t("collab.knowledge.observation");
    if (kind === "dead_end") return t("collab.knowledge.dead_end");
    if (kind === "poc") return t("collab.knowledge.resource");
    if (kind === "goal") return t("collab.knowledge.goal");
    if (kind === "finding") return t("collab.knowledge.findingResult");
  }
  return t(`collab.knowledge.${kind}`);
}

/** Verify edges cite a fact check (`fact:`) or a report reproduction (`report:`). */
export function verifyBasisLabel(refId: string, t: Translate): string {
  return refId.startsWith("report:") ? t("collab.basis.reportRepro") : t("collab.basis.factVerify");
}

export function relationLabel(kind: CollaborationRelationKind, t: Translate): string {
  return t(`collab.relation.${kind}`);
}
