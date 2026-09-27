/** control BLACKBOARD_DELTA projection; event behavior is unchanged. */
import {
  REVIEW_CAP,
  capPush,
  markTruncated,
  type BlackboardReviewFinding,
  type DirectivePreemption,
  type DirectiveStatus,
  type HitlNeedKind,
  type OperatorDirective,
  type ResourceLock,
} from "../types";
import {
  lane,
  pushChat,
} from "../helpers";

import type { BlackboardReducerContext } from "./blackboard-context";

export function reduceBlackboardControl({ ev, s, p, bb, actor, tlabel }: BlackboardReducerContext): boolean {
  switch (p.kind) {
        case "operator_directive_changed": {
          // B: upsert the directive state chain + surface it as a chat bubble.
          const did = String(p.directive_id ?? p.id ?? "");
          if (did) {
            const row: OperatorDirective = {
              id: did,
              text: p.text ?? "",
              action: p.action ?? "directive",
              status: (p.status ?? "received") as DirectiveStatus,
              preemption: (p.preemption ?? p.preempt_policy) as DirectivePreemption | undefined,
              boundWorker: p.bound_worker ?? p.boundWorker ?? undefined,
              ts: ev.ts,
            };
            const existing = s.operatorDirectives.find((d) => d.id === did);
            s.operatorDirectives = existing
              ? s.operatorDirectives.map((d) => d.id === did ? { ...d, ...row, text: row.text || d.text } : d)
              : [...s.operatorDirectives, row];
            // first time we see it bound/received, announce it in the thread.
            if (!existing) {
              pushChat(s, { role: "system", kind: "guidance",
                content: `operator ${row.action}: ${row.text}`, ts: ev.ts });
            }
          }
          tlabel(`operator ${p.action ?? "directive"} → ${p.status ?? "?"}`);
          break;
        }
        case "hitl_classified": {
          // F: annotate the matching hand-raise with its triage; auto-resolving
          // kinds (non external_blocker) are silently dismissed from the card stack.
          const nk = (p.need_kind ?? p.needKind) as HitlNeedKind | undefined;
          const w = String(p.worker ?? actor ?? "");
          const need = String(p.need ?? "");
          const pauses = nk ? nk === "external_blocker" : true;
          s.hitlRequests = s.hitlRequests
            // Runtime adapters can initially label an interaction as an
            // external_blocker and the coordinator can then refine it to a
            // non-blocking category.  Match the correlated worker/need pair even
            // when the first event already carried that provisional needKind.
            .map((r) => (r.worker === w && (!need || r.prompt === need))
              ? { ...r, needKind: nk, pausesBehavior: pauses } : r)
            // auto-resolving kinds don't need an operator decision — drop their cards
            .filter((r) => !(r.worker === w && (!need || r.prompt === need)
              && nk && nk !== "external_blocker" && !r.pausesBehavior));
          tlabel(`${w} hand-raise classified: ${nk ?? "external_blocker"}`);
          break;
        }
        case "resource_lock_changed": {
          // E: a unified resource lock was acquired/released/expired/denied.
          const lid = String(p.lock_id ?? p.lockId ?? "");
          const status = (p.status ?? "active") as ResourceLock["status"];
          if (lid) {
            const row: ResourceLock = {
              lockId: lid,
              resourceKey: p.resource_key ?? p.resourceKey ?? lid,
              scope: p.scope ?? "activity",
              riskClass: p.risk_class ?? p.riskClass ?? undefined,
              status,
              ownerWorker: p.owner_worker ?? p.ownerWorker ?? actor ?? undefined,
              heldBy: String(p.held_by ?? p.heldBy ?? "").trim() || undefined,
              ts: ev.ts,
            };
            const idx = s.resourceLocks.findIndex((l) => l.lockId === lid);
            if (status === "denied") {
              // A denial is a request record (owner_worker = requester, held_by =
              // holder); the active lock it collided with stays as it is.
              if (row.ownerWorker && row.heldBy) {
                s.lockRequests = [...s.lockRequests, {
                  lockId: lid,
                  resourceKey: row.resourceKey,
                  scope: row.scope,
                  riskClass: row.riskClass,
                  requester: row.ownerWorker,
                  holder: row.heldBy,
                  ts: ev.ts,
                }].slice(-200);
              }
            } else if (status === "released" || status === "expired") {
              s.resourceLocks = s.resourceLocks.filter((l) => l.lockId !== lid);
            } else {
              s.resourceLocks = idx >= 0
                ? s.resourceLocks.map((l) => l.lockId === lid ? { ...l, ...row } : l)
                : [...s.resourceLocks, row];
            }
          }
          tlabel(`resource ${p.status ?? "lock"}: ${(p.resource_key ?? "").slice(0, 48)}`);
          break;
        }
        case "review_started": {
          const w = p.worker ?? actor;
          if (w && !bb.workers.includes(w)) bb.workers = [...bb.workers, w];
          if (w) {
            const l = lane(s, w);
            s.lanes[l.solverId] = {
              ...l,
              role: "review",
              phase: "review",
              status: "reviewing",
              online: true,
              statusReason: p.trigger ?? "review",
            };
          }
          tlabel(`review started${p.trigger ? ` (${p.trigger})` : ""}`);
          break;
        }
        case "review_finished": {
          const w = p.worker ?? actor;
          if (w && s.lanes[w]) {
            s.lanes[w] = {
              ...s.lanes[w],
              role: "review",
              phase: s.lanes[w].phase ?? "review",
              status: "finished",
              online: false,
              statusReason: "review_finished",
            };
          }
          tlabel(`review finished${w ? `: ${w}` : ""}`);
          break;
        }
        case "review_finding": {
          const id = String(p.finding_id ?? (p.seq != null ? `rvw-${p.seq}` : ""));
          if (!id) break;
          const recommendedActions = Array.isArray(p.recommended_actions)
            ? p.recommended_actions.map((item: unknown) => String(item ?? "").trim()).filter(Boolean)
            : undefined;
          const evidenceSeqs = Array.isArray(p.evidence_seqs)
            ? p.evidence_seqs.map((item: unknown) => Number(item)).filter((n: number) => Number.isFinite(n) && n > 0)
            : undefined;
          const intentIds = Array.isArray(p.intent_ids)
            ? p.intent_ids.map((item: unknown) => String(item ?? "").trim()).filter(Boolean)
            : undefined;
          const row: BlackboardReviewFinding = {
            id,
            kind: String(p.finding_kind ?? "finding"),
            severity: String(p.severity ?? "info"),
            summary: String(p.summary ?? ""),
            routeHash: p.route_hash ? String(p.route_hash) : undefined,
            branchId: p.branch_id ? String(p.branch_id) : undefined,
            recommendedActions,
            evidenceSeqs,
            intentIds,
            worker: String(p.worker ?? "").trim() || undefined,
            actor,
            ts: ev.ts,
          };
          const existing = bb.reviewFindings.find((item) => item.id === id);
          if (existing) {
            // The same finding_id arrives twice (direct emit + graph bridge); keep
            // whichever copy carried the review worker / references.
            bb.reviewFindings = bb.reviewFindings.map((item) => item.id === id ? {
              ...item,
              ...row,
              worker: row.worker || item.worker,
              evidenceSeqs: row.evidenceSeqs?.length ? row.evidenceSeqs : item.evidenceSeqs,
              intentIds: row.intentIds?.length ? row.intentIds : item.intentIds,
            } : item);
          } else {
            const pushed = capPush(bb.reviewFindings, row, REVIEW_CAP);
            bb.reviewFindings = pushed.list;
            if (pushed.truncated) markTruncated(bb, "reviews");
          }
          tlabel(`review ${row.severity}: ${row.summary.slice(0, 72)}`);
          break;
        }
    default:
      return false;
  }
  return true;
}
