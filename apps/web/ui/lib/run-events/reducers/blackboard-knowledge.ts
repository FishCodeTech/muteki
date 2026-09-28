/** knowledge BLACKBOARD_DELTA projection; event behavior is unchanged. */
import {
  DEAD_END_CAP,
  DIRECTIVE_CAP,
  FACT_CAP,
  ROUTE_CAP,
  capPush,
  markTruncated,
  type BlackboardFact,
  type BlackboardSuppressedRoute,
  type FactEvidenceProvenance,
  type FactObservation,
  type FactLifecycleState,
  type IntentDispatchState,
} from "../types";
import { pushChat } from "../helpers";

import type { BlackboardReducerContext } from "./blackboard-context";

function evidenceProvenance(raw: any): FactEvidenceProvenance | undefined {
  if (!raw || typeof raw !== "object") return undefined;
  return {
    target: raw.target ? String(raw.target) : undefined,
    artifactRefs: Array.isArray(raw.artifact_refs)
      ? raw.artifact_refs
        .filter((ref: any) => ref && typeof ref === "object" && String(ref.artifact_id ?? ""))
        .map((ref: any) => ({
          artifactId: String(ref.artifact_id),
          sha256: ref.sha256 ? String(ref.sha256) : undefined,
          size: Number.isFinite(Number(ref.size)) ? Number(ref.size) : undefined,
          command: ref.command ? String(ref.command) : undefined,
          toolEventSeq: Number(ref.tool_event_seq) || undefined,
        }))
      : undefined,
    toolEventId: raw.tool_event_id ? String(raw.tool_event_id) : undefined,
    toolEventSeq: Number(raw.tool_event_seq) || undefined,
    toolEventTs: Number(raw.tool_event_ts) || undefined,
    toolCallId: raw.tool_call_id ? String(raw.tool_call_id) : undefined,
    workerId: raw.worker_id ? String(raw.worker_id) : undefined,
    intentId: raw.intent_id ? String(raw.intent_id) : undefined,
    targetEpoch: raw.target_epoch ? String(raw.target_epoch) : undefined,
    artifactId: raw.artifact_id ? String(raw.artifact_id) : undefined,
    artifactSha256: raw.artifact_sha256 ? String(raw.artifact_sha256) : undefined,
    observedAt: Number(raw.observed_at) || undefined,
    promotedAt: Number(raw.promoted_at) || undefined,
  };
}

/** Actors the graph bridge stamps on relayed fact events; the producer travels in source_solver. */
const RELAY_FACT_ACTORS = new Set(["coordinator", "reason"]);

/** Producer of a fact: source_solver when the recorded actor is only the relaying control actor. */
function factProducer(current: string, sourceSolver: unknown): string {
  const producer = String(sourceSolver ?? "").trim();
  return producer && RELAY_FACT_ACTORS.has(current) && producer !== current ? producer : current;
}

function appendObservation(rows: FactObservation[] | undefined, row: FactObservation): FactObservation[] {
  const current = rows || [];
  const eventId = row.provenance?.toolEventId;
  const duplicate = current.some((item) =>
    (row.observationSeq && item.observationSeq === row.observationSeq)
    || (eventId && item.provenance?.toolEventId === eventId));
  return duplicate ? current : [...current, row];
}

export function reduceBlackboardKnowledge({ ev, s, p, bb, actor, tlabel }: BlackboardReducerContext): boolean {
  switch (p.kind) {
        case "intent_proposed": {
          const fromFacts: number[] = Array.isArray(p.from_facts)
            ? p.from_facts.filter((seq: any) => typeof seq === "number" && seq > 0)
            : [];
          const intentFields = {
            dependsOn: Array.isArray(p.depends_on) ? p.depends_on.map(String).filter(Boolean) : undefined,
            expectedObservable: p.expected_observable ? String(p.expected_observable) : undefined,
            stopCondition: p.stop_condition ? String(p.stop_condition) : undefined,
            coverageKey: p.coverage_key ? String(p.coverage_key) : undefined,
            routeHash: p.route_hash ? String(p.route_hash) : undefined,
            laneKey: p.lane_key ? String(p.lane_key) : undefined,
            priority: Number.isFinite(Number(p.priority)) ? Number(p.priority) : undefined,
            requestedPriority: p.requested_priority ? String(p.requested_priority) : undefined,
            priorityReason: p.priority_reason ? String(p.priority_reason) : undefined,
            valueClaim: p.value_claim && typeof p.value_claim === "object" ? p.value_claim : undefined,
            noveltyKey: p.novelty_key ? String(p.novelty_key) : undefined,
            requiresCapabilities: Array.isArray(p.requires_capabilities)
              ? p.requires_capabilities.map(String).filter(Boolean) : undefined,
            requiredPocs: Array.isArray(p.required_pocs)
              ? p.required_pocs.map(String).filter(Boolean) : undefined,
          };
          if (!bb.intents.some((i) => i.id === p.intent_id)) {
            bb.intents = [...bb.intents, {
              id: p.intent_id, goal: p.goal ?? "", workerClass: p.worker_class ?? "code",
              fromFacts, ...intentFields, status: "open", proposedBy: actor || undefined, proposedTs: ev.ts,
            }];
          } else {
            bb.intents = bb.intents.map((i) =>
              i.id === p.intent_id ? {
                ...i, ...intentFields,
                fromFacts: fromFacts.length ? fromFacts : i.fromFacts,
                proposedBy: i.proposedBy || actor || undefined,
              } : i);
          }
          tlabel(`proposed ${p.intent_id} · ${(p.goal ?? "").slice(0, 60)}`);
          break;
        }
        case "observation_added": {
          const observationSeq = Number(p.observation_seq || 0);
          if (observationSeq <= 0) break;
          // The initial Fact may be relayed before its graph Observation.
          // Reconcile by the explicit source ID in either event order.
          const admittedFactSeq = Number(p.admitted_fact_seq)
            || bb.facts.find((fact) => fact.sourceObservationSeq === observationSeq)?.factSeq;
          const row = {
            observationSeq,
            text: String(p.text ?? ""),
            actor: String(p.source_solver ?? actor),
            intentId: p.intent_id ? String(p.intent_id) : undefined,
            targetEpoch: p.target_epoch ? String(p.target_epoch) : undefined,
            witness: p.witness ? String(p.witness) : undefined,
            artifactId: p.artifact_id ? String(p.artifact_id) : undefined,
            confidence: Number(p.confidence) || 0,
            admitted: !!p.admitted || !!admittedFactSeq,
            admittedFactSeq: admittedFactSeq || undefined,
            claimedVerified: !!p.claimed_verified,
            canonicalKey: p.canonical_key ? String(p.canonical_key) : undefined,
            provenance: evidenceProvenance(p.evidence_provenance),
            ts: ev.ts,
          };
          if (!bb.observations.some((item) => item.observationSeq === observationSeq)) {
            const pushed = capPush(bb.observations, row, FACT_CAP);
            bb.observations = pushed.list;
            if (pushed.truncated) markTruncated(bb, "facts");
          }
          tlabel(`${row.actor} observation${row.admitted ? " admitted" : " retained"}: ${row.text.slice(0, 56)}`);
          break;
        }
        case "intent_claimed": {
          bb.intents = bb.intents.map((i) =>
            i.id === p.intent_id ? { ...i, status: "claimed", worker: p.worker ?? actor, claimedTs: ev.ts } : i);
          tlabel(`${p.worker ?? actor} claimed ${p.intent_id}`);
          break;
        }
        case "intent_concluded": {
          const toFactSeq = typeof p.to_fact_seq === "number" && p.to_fact_seq > 0 ? p.to_fact_seq : undefined;
          bb.intents = bb.intents.map((i) =>
            i.id === p.intent_id ? { ...i, status: "done", dispatchState: "closed", closeReason: p.result ?? i.closeReason, worker: i.worker ?? p.worker ?? actor, concludedTs: ev.ts, toFactSeq: toFactSeq ?? i.toFactSeq } : i);
          tlabel(`${p.worker ?? actor} concluded ${p.intent_id}`);
          break;
        }
        case "fact_added": {
          const rawFactSeq = typeof p.fact_seq === "number" ? p.fact_seq : undefined;
          if (rawFactSeq !== undefined && rawFactSeq <= 0) break;
          const factSeq = rawFactSeq && rawFactSeq > 0 ? rawFactSeq : undefined;
          const sourceObservationSeq = Number(p.observation_seq) || undefined;
          if (sourceObservationSeq && factSeq) {
            bb.observations = bb.observations.map((item) =>
              item.observationSeq === sourceObservationSeq
                ? { ...item, admitted: true, admittedFactSeq: factSeq }
                : item);
          }
          const provenance = evidenceProvenance(p.evidence_provenance);
          // Bridged fact_added events arrive with actor="coordinator"; the worker
          // that produced the fact is source_solver.
          const producer = factProducer(actor, p.source_solver);
          const observation: FactObservation = {
            observationSeq: sourceObservationSeq,
            actor: producer, verified: !!p.verified, confidence: p.confidence ?? 0,
            artifactId: p.artifact_id ?? undefined, witness: p.witness ?? undefined,
            provenance, ts: ev.ts,
          };
          const row: BlackboardFact = {
            factSeq, sourceObservationSeq, fact: p.fact ?? "", verified: !!p.verified, confidence: p.confidence ?? 0,
            actor: producer, verifier: p.verifier ?? "", witness: p.witness ?? undefined,
            artifactId: p.artifact_id ?? undefined, ts: ev.ts,
            targetEpoch: p.target_epoch ? String(p.target_epoch) : provenance?.targetEpoch,
            identitySha256: p.fact_identity_sha256 ? String(p.fact_identity_sha256) : undefined,
            provenance,
            observations: [observation],
            // G0: the intent this fact was PRODUCED by (intent_products edge). The
            // backend now attaches every worker-produced fact to its intent and the
            // DB→bus bridge carries intent_id on fact_added; persist it so the canvas
            // draws the real produces-edge instead of guessing by time-window.
            intentId: p.intent_id ? String(p.intent_id) : undefined,
          };
          if (factSeq) {
            const existing = bb.facts.find((f) => f.factSeq === factSeq);
            if (existing) {
              bb.facts = bb.facts.map((f) => f.factSeq === factSeq
                  ? {
                    ...f,
                    sourceObservationSeq: row.sourceObservationSeq ?? f.sourceObservationSeq,
                    actor: factProducer(f.actor, producer),
                    fact: f.fact || row.fact,
                    verified: f.verified || row.verified,
                    confidence: Math.max(f.confidence, row.confidence),
                    verifier: row.verified ? row.verifier : f.verifier,
                    witness: row.verified ? (row.witness ?? f.witness) : f.witness,
                    artifactId: row.verified ? (row.artifactId ?? f.artifactId) : f.artifactId,
                    targetEpoch: row.targetEpoch ?? f.targetEpoch,
                    identitySha256: row.identitySha256 ?? f.identitySha256,
                    provenance: row.verified ? (row.provenance ?? f.provenance) : f.provenance,
                    observations: appendObservation(f.observations, observation),
                    summary: f.summary,
                    intentId: f.intentId ?? row.intentId,
                    promotedTs: !f.verified && row.verified ? ev.ts : f.promotedTs,
                  }
                  : f);
            } else {
              const pushed = capPush(bb.facts, row, FACT_CAP);
              bb.facts = pushed.list;
              if (pushed.truncated) markTruncated(bb, "facts");
            }
          } else {
            const pushed = capPush(bb.facts, row, FACT_CAP);
            bb.facts = pushed.list;
            if (pushed.truncated) markTruncated(bb, "facts");
          }
          tlabel(`${producer} ${p.verified ? "verified" : "candidate"}: ${(p.fact ?? "").slice(0, 56)}`);
          break;
        }
        case "fact_observed": {
          const factSeq = Number(p.fact_seq || 0);
          if (factSeq <= 0) break;
          const provenance = evidenceProvenance(p.evidence_provenance);
          const observation: FactObservation = {
            observationSeq: Number(p.observation_seq) || undefined,
            actor, verified: !!p.verified, confidence: p.confidence ?? 0,
            artifactId: p.artifact_id ?? undefined, witness: p.witness ?? undefined,
            provenance, ts: ev.ts,
          };
          // An independent re-observation names the verifying worker; the
          // producer's own re-observation leaves verifier empty.
          const observedVerifier = String(p.verifier ?? "").trim();
          bb.facts = bb.facts.map((f) => f.factSeq === factSeq
            ? {
              ...f,
              actor: factProducer(f.actor, p.source_solver),
              observations: appendObservation(f.observations, observation),
              verifier: observedVerifier || f.verifier,
            }
            : f);
          tlabel(`${actor} observed fact #${factSeq}`);
          break;
        }
        case "fact_promoted": {
          const factSeq = Number(p.fact_seq || 0);
          if (factSeq <= 0) break;
          const provenance = evidenceProvenance(p.evidence_provenance);
          bb.facts = bb.facts.map((f) => f.factSeq === factSeq ? {
            ...f,
            actor: factProducer(f.actor, p.source_solver),
            verified: true,
            confidence: p.confidence ?? 1,
            verifier: String(p.verifier ?? "").trim() || f.verifier,
            witness: p.witness ?? f.witness,
            artifactId: p.artifact_id ?? f.artifactId,
            targetEpoch: p.target_epoch ? String(p.target_epoch) : (provenance?.targetEpoch ?? f.targetEpoch),
            provenance: provenance ?? f.provenance,
            promotedTs: ev.ts,
          } : f);
          tlabel(`${actor} promoted fact #${factSeq}`);
          break;
        }
        case "dead_end": {
          const rawDeadSeq = typeof p.dead_end_seq === "number" ? p.dead_end_seq : undefined;
          if (rawDeadSeq !== undefined && rawDeadSeq <= 0) break;
          const deadEndSeq = rawDeadSeq && rawDeadSeq > 0 ? rawDeadSeq : undefined;
          const reason = p.reason ?? "";
          const norm = (x: string) => x.toLowerCase().replace(/\s+/g, " ").trim();
          const row = {
            deadEndSeq, reason, actor, ts: ev.ts,
            testedScope: p.tested_scope ? String(p.tested_scope) : undefined,
            observedResult: p.observed_result ? String(p.observed_result) : undefined,
            intentId: p.intent_id ? String(p.intent_id) : undefined,
            targetEpoch: p.target_epoch ? String(p.target_epoch) : undefined,
          };
          const sameText = (d: typeof bb.deadEnds[number]) =>
            d.actor === actor && norm(d.reason) === norm(reason);
          const existing = deadEndSeq
            ? bb.deadEnds.find((d) => d.deadEndSeq === deadEndSeq || (!d.deadEndSeq && sameText(d)))
            : bb.deadEnds.find((d) => !d.deadEndSeq && sameText(d));
          bb.deadEnds = existing
            ? bb.deadEnds.map((d) =>
                (deadEndSeq
                  ? (d.deadEndSeq === deadEndSeq || (!d.deadEndSeq && sameText(d)))
                  : (!d.deadEndSeq && sameText(d)))
                  ? { ...d, ...row }
                  : d)
            : (() => { const pushed = capPush(bb.deadEnds, row, DEAD_END_CAP); if (pushed.truncated) markTruncated(bb, "deadEnds"); return pushed.list; })();
          tlabel(`${actor} dead-end: ${(p.reason ?? "").slice(0, 56)}`);
          break;
        }
        case "intent_reopened": {
          bb.intents = bb.intents.map((i) =>
            i.id === p.intent_id ? { ...i, status: "open", dispatchState: "active", closeReason: undefined, worker: undefined, concludedTs: undefined } : i);
          tlabel(`↻ reopened ${p.intent_id} (false positive)`);
          break;
        }
        case "fact_rejected":
        case "fact_superseded": {
          // A: a fact failed review — mark its lifecycle state so the board dims it
          // and verified/candidate selectors exclude it.
          const fs = typeof p.fact_seq === "number" ? p.fact_seq : undefined;
          const newState: FactLifecycleState = p.kind === "fact_rejected" ? "rejected" : "superseded";
          bb.facts = bb.facts.map((f) =>
            f.factSeq === fs ? { ...f, state: newState, verified: false } : f);
          tlabel(`${actor} ${newState} fact #${fs ?? "?"}`);
          break;
        }
        case "fact_merged": {
          const fromSeq = typeof p.from_fact_seq === "number" ? p.from_fact_seq : undefined;
          const intoSeq = typeof p.to_fact_seq === "number" ? p.to_fact_seq : undefined;
          bb.facts = bb.facts.map((f) =>
            f.factSeq === fromSeq ? { ...f, state: "merged", mergedInto: intoSeq, verified: false } : f);
          tlabel(`${actor} merged fact #${fromSeq ?? "?"} → #${intoSeq ?? "?"}`);
          break;
        }
        case "intent_state_changed": {
          // A/J: dispatch_state transition (finalize → resume/closed, revive → active).
          const ds = (p.dispatch_state ?? p.dispatchState) as IntentDispatchState | undefined;
          // a single delta can carry a comma-joined list of intent ids.
          const ids = String(p.intent_id ?? "").split(",").map((x) => x.trim()).filter(Boolean);
          const idSet = new Set(ids);
          bb.intents = bb.intents.map((i) =>
            idSet.has(i.id)
              ? { ...i, dispatchState: ds ?? i.dispatchState,
                closeReason: ds
                  ? (p.close_reason ?? p.stop_reason ?? p.reason ?? i.closeReason)
                  : i.closeReason,
                priority: p.priority !== undefined ? Number(p.priority) : i.priority,
                priorityReason: p.priority_reason ? String(p.priority_reason) : i.priorityReason }
              : i);
          if (ds) tlabel(`${ids.length} intent(s) → ${ds}`);
          break;
        }
        case "fact_challenged": {
          const factSeq = Number(p.fact_seq || 0);
          const reason = String(p.reason ?? "");
          bb.facts = bb.facts.map((f) =>
            f.factSeq === factSeq
              ? { ...f, state: "challenged", challenged: true, revalidated: false, challengeReason: reason }
              : f);
          tlabel(`fact #${factSeq} challenged: ${reason.slice(0, 64)}`);
          break;
        }
        case "fact_revalidated": {
          const factSeq = Number(p.fact_seq || 0);
          bb.facts = bb.facts.map((f) =>
            f.factSeq === factSeq
              ? { ...f, state: "revalidated", challenged: false, revalidated: true, challengeReason: undefined }
              : f);
          tlabel(`fact #${factSeq} revalidated`);
          break;
        }
        case "route_suppressed": {
          const routeHash = String(p.route_hash ?? "");
          const row: BlackboardSuppressedRoute = {
            routeHash,
            label: p.label ? String(p.label) : undefined,
            reason: String(p.reason ?? ""),
            actor,
            ts: ev.ts,
            reopened: false,
          };
          {
            const nextRoutes = [...bb.suppressedRoutes.filter((r) => r.routeHash !== routeHash), row];
            if (nextRoutes.length > ROUTE_CAP) markTruncated(bb, "routes");
            bb.suppressedRoutes = nextRoutes.slice(-ROUTE_CAP);
          }
          tlabel(`route suppressed: ${routeHash}${row.reason ? ` · ${row.reason.slice(0, 48)}` : ""}`);
          break;
        }
        case "route_reopened": {
          const routeHash = String(p.route_hash ?? "");
          bb.suppressedRoutes = bb.suppressedRoutes.map((r) =>
            r.routeHash === routeHash ? { ...r, reopened: true } : r);
          tlabel(`route reopened: ${routeHash}`);
          break;
        }
        case "branch_split": {
          const branchId = String(p.branch_id ?? p.parent ?? "");
          {
            const pushed = capPush(bb.branches, {
              branchId,
              title: String(p.title ?? branchId),
              actor,
              ts: ev.ts,
              status: "open",
            }, ROUTE_CAP);
            bb.branches = pushed.list;
            if (pushed.truncated) markTruncated(bb, "routes");
          }
          tlabel(`branch split: ${String(p.title ?? branchId).slice(0, 72)}`);
          break;
        }
        case "branch_resolved": {
          const ids = [p.branch_id, ...(Array.isArray(p.resolved) ? p.resolved : [])]
            .map((x) => String(x ?? "").trim())
            .filter(Boolean);
          const idSet = new Set(ids);
          bb.branches = bb.branches.map((b) =>
            idSet.has(b.branchId) ? { ...b, status: "resolved" } : b);
          tlabel(`branch resolved: ${ids.join(", ") || "branch"}`);
          break;
        }
        case "coordinator_directive": {
          const directive = String(p.directive ?? "");
          const action = String(p.action ?? "note");
          const routeHash = String(p.route_hash ?? "").trim() || undefined;
          {
            const pushed = capPush(bb.directives, { action, directive, actor, routeHash, ts: ev.ts }, DIRECTIVE_CAP);
            bb.directives = pushed.list;
            if (pushed.truncated) markTruncated(bb, "directives");
          }
          tlabel(`directive ${action}: ${directive.slice(0, 72)}`);
          if (directive) {
            pushChat(s, { role: "system", kind: "status",
              content: `Review directive (${action}): ${directive}`, ts: ev.ts });
          }
          break;
        }
        case "capability_published": {
          const key = String(p.capability_key ?? "");
          if (!key) break;
          const row = {
            key, targetEpoch: String(p.target_epoch ?? ""),
            kind: String(p.kind ?? "generic"),
            quality: p.quality ? String(p.quality) : undefined,
            sharing: p.sharing ? String(p.sharing) : undefined,
            state: "active" as const,
            accessPathId: p.access_path_id ? String(p.access_path_id) : undefined,
            ts: ev.ts,
          };
          bb.capabilities = [...bb.capabilities.filter((item) =>
            !(item.key === key && item.targetEpoch === row.targetEpoch)), row];
          bb.capabilityGaps = bb.capabilityGaps.map((gap) =>
            gap.targetEpoch === row.targetEpoch
              && gap.requiredCapabilities.every((required) =>
                bb.capabilities.some((capability) => capability.targetEpoch === gap.targetEpoch
                  && capability.key === required && capability.state === "active"))
              ? { ...gap, state: "resolved" as const } : gap);
          tlabel(`capability active: ${key}`);
          break;
        }
        case "capability_retired": {
          const key = String(p.capability_key ?? "");
          const epoch = String(p.target_epoch ?? "");
          bb.capabilities = bb.capabilities.map((item) =>
            item.key === key && item.targetEpoch === epoch
              ? { ...item, state: "retired" as const, ts: ev.ts } : item);
          tlabel(`capability retired: ${key}`);
          break;
        }
        case "capability_gap_reported": {
          const id = String(p.gap_id ?? "");
          if (!id) break;
          const row = {
            id, targetEpoch: String(p.target_epoch ?? ""),
            description: String(p.description ?? ""),
            requiredCapabilities: Array.isArray(p.required_capabilities)
              ? p.required_capabilities.map(String) : [],
            consumers: Array.isArray(p.consumers) ? p.consumers.map(String) : [],
            state: "open" as const, ts: ev.ts,
          };
          bb.capabilityGaps = [...bb.capabilityGaps.filter((item) => item.id !== id), row];
          tlabel(`capability gap: ${row.requiredCapabilities.join(", ") || row.description}`);
          break;
        }
        case "access_path_published": {
          const id = String(p.access_path_id ?? "");
          if (!id) break;
          const row = {
            id, targetEpoch: String(p.target_epoch ?? ""),
            endpoint: p.endpoint ? String(p.endpoint) : undefined,
            reach: Array.isArray(p.reach) ? p.reach.map(String) : [],
            operations: Array.isArray(p.operations) ? p.operations.map(String) : [],
            quality: p.quality ? String(p.quality) : undefined,
            capabilityKeys: Array.isArray(p.capability_keys) ? p.capability_keys.map(String) : [],
            state: "ready" as const, ts: ev.ts,
          };
          bb.accessPaths = [...bb.accessPaths.filter((item) => item.id !== id), row];
          tlabel(`shared access ready: ${id}`);
          break;
        }
        case "access_path_state_changed": {
          const id = String(p.access_path_id ?? "");
          const state = String(p.state ?? "") as "ready" | "degraded" | "closed";
          bb.accessPaths = bb.accessPaths.map((item) =>
            item.id === id ? { ...item, state, ts: ev.ts } : item);
          tlabel(`shared access ${id} → ${state}`);
          break;
        }
        case "value_receipt": {
          const id = String(p.receipt_id ?? "");
          if (!id) break;
          const row = {
            id, intentId: String(p.intent_id ?? ""),
            effect: p.effect ? String(p.effect) : undefined,
            requestedPriority: p.requested_priority ? String(p.requested_priority) : undefined,
            effectivePriority: Number(p.effective_priority) || 0,
            achieved: !!p.achieved,
            capabilitiesAdded: Array.isArray(p.capabilities_added) ? p.capabilities_added.map(String) : [],
            unblockedCount: Number(p.unblocked_count) || 0,
            ts: ev.ts,
          };
          bb.valueReceipts = [...bb.valueReceipts.filter((item) => item.id !== id), row];
          tlabel(`value receipt ${row.intentId}: ${row.achieved ? "achieved" : "not achieved"}`);
          break;
        }
        case "runtime_resource_registered":
        case "runtime_resource_state_changed": {
          tlabel(`runtime resource ${String(p.resource_id ?? "")} ${String(p.state ?? "registered")}`);
          break;
        }
    default:
      return false;
  }
  return true;
}
