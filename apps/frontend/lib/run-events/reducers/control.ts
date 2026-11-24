import {
  EventType,
  type ControlCommand,
  type ControlCommandStatus,
  type DeckState,
  type HitlNeedKind,
  type HitlRequest,
  type MutekiEvent,
} from "../types";
import { archiveHitlRequest, classifyHitlRequest } from "../hitl";
import {
  applyObservedControlState,
  gid,
  pushChat,
} from "../helpers";
import { defaultProductionGuardState, normalizeStopStatus } from "./guard";

function syncAwaitingOperator(s: DeckState, resolvedPrompts: Set<string>): void {
  const nextPending = s.hitlRequests.find((r) => classifyHitlRequest(r) === "pending");
  if (nextPending) {
    s.awaitingOperator = nextPending.prompt;
    return;
  }
  if (s.awaitingOperator && resolvedPrompts.has(s.awaitingOperator)) {
    s.awaitingOperator = undefined;
  }
}

export function reduceControl(ev: MutekiEvent, s: DeckState): DeckState | undefined {
  const sid = ev.solver_id || "";
  const p = ev.payload || {};
  switch (ev.event_type) {
    case EventType.NODE_SUMMARIZED: {
      const summary = (p.summary ?? "").trim();
      if (!summary) break;
      const bb = s.blackboard;
      if (p.node_kind === "intent") {
        bb.intents = bb.intents.map((it) => it.id === p.intent_id ? { ...it, summary } : it);
      } else if (typeof p.fact_seq === "number" && p.fact_seq > 0) {
        bb.facts = bb.facts.map((f) => f.factSeq === p.fact_seq ? { ...f, summary } : f);
      }
      break;
    }
    case EventType.CONTEXT_STATE:
      s.gauge = { zones: p.zones ?? [], total: p.total ?? 0, limit: p.limit ?? 0 };
      break;
    case EventType.COST_UPDATE: {
      const cu = {
        usd: typeof p.usd === "number" ? p.usd : 0,
        tokensIn: typeof p.input_tokens === "number" ? p.input_tokens : 0,
        tokensOut: typeof p.output_tokens === "number" ? p.output_tokens : 0,
        unpricedCalls: typeof p.unpriced_calls === "number" ? p.unpriced_calls : undefined,
      };
      if (p.auxiliary_total) {
        s.auxiliaryCost = { usd: p.auxiliary_total.usd, tokensIn: p.auxiliary_total.input_tokens,
          tokensOut: p.auxiliary_total.output_tokens, unpricedCalls: p.auxiliary_total.unpriced_calls };
      }
      if (p.scope === "auxiliary") {
        if (!p.auxiliary_total) s.auxiliaryCost = cu;
        break;
      }
      if ((p.scope === "solver" || p.scope === "solve") && sid) {
        s.costBySolver = {
          ...s.costBySolver,
          [sid]: { ...cu, engine: s.lanes[sid]?.engine },
        };
        const history = s.costHistory[sid] ?? [];
        s.costHistory = {
          ...s.costHistory,
          [sid]: [...history, { ts: ev.ts, tokens: cu.tokensIn + cu.tokensOut }].slice(-120),
        };
        const all = Object.values(s.costBySolver);
        s.usd = all.reduce((a, c) => a + c.usd, 0);
        s.tokensIn = all.reduce((a, c) => a + c.tokensIn, 0);
        s.tokensOut = all.reduce((a, c) => a + c.tokensOut, 0);
      } else {
        const sum = Object.values(s.costBySolver);
        s.usd = Math.max(cu.usd, sum.reduce((a, c) => a + c.usd, 0));
        s.tokensIn = Math.max(cu.tokensIn, sum.reduce((a, c) => a + c.tokensIn, 0));
        s.tokensOut = Math.max(cu.tokensOut, sum.reduce((a, c) => a + c.tokensOut, 0));
      }
      if (p.run_total && typeof p.run_total === "object") {
        const observed = Object.values(s.costBySolver);
        s.usd = Math.max(Number(p.run_total.usd ?? 0), observed.reduce((a, c) => a + c.usd, 0));
        s.tokensIn = Math.max(Number(p.run_total.input_tokens ?? 0), observed.reduce((a, c) => a + c.tokensIn, 0));
        s.tokensOut = Math.max(Number(p.run_total.output_tokens ?? 0), observed.reduce((a, c) => a + c.tokensOut, 0));
      }
      break;
    }
    case EventType.GUIDANCE_INJECTED:
      pushChat(s, { role: "system", kind: "guidance", content: p.note ?? "(human guidance applied)", ts: ev.ts });
      break;
    case EventType.CONTROL_COMMAND: {
      const commandId = String(p.command_id ?? p.commandId ?? "");
      const requestId = p.request_id ?? p.requestId;
      const generation = Number(p.generation);
      const staleControlGeneration = Number.isFinite(generation)
        && generation < (s.controlGeneration ?? 0);
      if (Number.isFinite(generation) && generation >= 0) {
        s.controlGeneration = Math.max(s.controlGeneration ?? 0, generation);
      }
      let effectiveRow: ControlCommand | undefined;
      if (commandId) {
        const row: ControlCommand = {
          id: commandId,
          action: String(p.action ?? "hint"),
          target: String(p.target ?? "global"),
          status: (p.status ?? "received") as ControlCommandStatus,
          requestId: requestId ?? undefined,
          effect: (p.effect && typeof p.effect === "object") ? p.effect : undefined,
          detail: p.detail ? String(p.detail) : undefined,
          ts: ev.ts,
        };
        const existing = s.controlCommands.find((c) => c.id === commandId);
        const terminal = new Set<ControlCommandStatus>([
          "effect_observed", "partial", "failed", "unknown", "rejected",
        ]).has(row.status);
        const existingTerminal = existing && new Set<ControlCommandStatus>([
          "effect_observed", "partial", "failed", "unknown", "rejected",
        ]).has(existing.status);
        effectiveRow = existing && existingTerminal && !terminal
          ? existing : { ...existing, ...row };
        if (existing && terminal) {
          s.controlCommands = [
            ...s.controlCommands.filter((c) => c.id !== commandId),
            effectiveRow,
          ].slice(-200);
        } else {
          s.controlCommands = (existing
            ? s.controlCommands.map((c) => c.id === commandId ? effectiveRow! : c)
            : [...s.controlCommands, effectiveRow]).slice(-200);
        }
      }
      const decisionAction = String(p.action ?? "");
      const resolvedIds: string[] = [];
      if (requestId) resolvedIds.push(String(requestId));
      if (Array.isArray(p.effect?.result?.request_ids)) {
        for (const item of p.effect.result.request_ids) {
          const id = String(item ?? "").trim();
          if (id && !resolvedIds.includes(id)) resolvedIds.push(id);
        }
      }
      const effectKind = String(p.effect?.kind ?? "");
      const detailText = effectiveRow?.detail
        || (typeof p.error === "string" && p.error.trim() ? p.error.trim() : undefined)
        || (typeof p.effect?.detail === "string" && p.effect.detail.trim() ? p.effect.detail.trim() : undefined)
        || (typeof p.effect?.error === "string" && p.effect.error.trim() ? p.effect.error.trim() : undefined);
      if (
        effectiveRow
        && resolvedIds.length > 0
        && (decisionAction === "answer_decision" || decisionAction === "dismiss" || decisionAction === "dismiss_help")
      ) {
        const idSet = new Set(resolvedIds);
        const resolvedPrompts = new Set<string>();
        if (p.decision_closed === true && effectiveRow.status === "effect_observed") {
          for (const id of resolvedIds) {
            const req = s.hitlRequests.find((r) => r.id === id);
            const prev = s.hitlHistory.find((h) => h.id === id);
            if (req) resolvedPrompts.add(req.prompt);
            const target = req ?? prev;
            if (target) {
              s.hitlHistory = archiveHitlRequest(s.hitlHistory, target, {
                resolvedTs: prev?.resolvedTs || ev.ts,
                resolutionAction: prev?.resolutionAction || decisionAction,
                deliveryCommandId: effectiveRow.id,
                deliveryStatus: effectiveRow.status,
                deliveryDetail: detailText,
                deliveryMode: prev?.deliveryMode,
                continuationIntentId: prev?.continuationIntentId,
              });
            }
          }
          s.hitlRequests = s.hitlRequests.filter((r) => !idSet.has(r.id));
        } else {
          const deliveryStatus: ControlCommandStatus =
            p.decision_closed !== true && effectKind === "decision_delivery_unknown"
              ? "unknown"
              : effectiveRow.status;
          s.hitlRequests = s.hitlRequests.map((r) => {
            if (!idSet.has(r.id)) return r;
            resolvedPrompts.add(r.prompt);
            return {
              ...r,
              deliveryCommandId: effectiveRow.id,
              deliveryStatus,
              deliveryDetail: detailText ?? r.deliveryDetail,
              deliveryAction: decisionAction,
              deliveryUpdatedAt: ev.ts,
            };
          });
        }
        syncAwaitingOperator(s, resolvedPrompts);
      }
      if (decisionAction === "emergency_stop") {
        const guard = s.productionGuard ? { ...s.productionGuard } : defaultProductionGuardState(true);
        const statusToken = String(effectiveRow?.status ?? p.status ?? "");
        if (statusToken === "partial") guard.stop_status = "stop_partial";
        else if (statusToken === "unknown") guard.stop_status = "stop_unknown";
        else if (statusToken === "effect_observed") guard.stop_status = "stopped";
        else if (statusToken === "received" || statusToken === "persisted" || statusToken === "routed") {
          guard.stop_status = "emergency_stopping";
        } else {
          guard.stop_status = normalizeStopStatus(statusToken, guard.stop_status ?? "emergency_stopping");
        }
        guard.egress_state = "revoked";
        s.productionGuard = guard;
      }
      if (!staleControlGeneration) applyObservedControlState(s, p);
      break;
    }
    case EventType.HITL_REQUEST: {
      const need = (p.need ?? p.prompt ?? "needs your input").toString();
      const isEnv = p.kind === "env_down";
      const needKind = (p.need_kind ?? p.needKind) as HitlNeedKind | undefined;
      const requestId = String(p.request_id ?? p.requestId ?? p.id ?? gid("hitl"));
      const existingReq = s.hitlRequests.find((r) => r.id === requestId);
      const pausesBehavior = needKind ? needKind === "external_blocker" : (existingReq?.pausesBehavior ?? true);
      const row: HitlRequest = {
        ...(existingReq ?? {}),
        id: requestId,
        prompt: need,
        worker: p.worker ? String(p.worker) : existingReq?.worker,
        intentId: (p.intent_id ?? p.intentId) ? String(p.intent_id ?? p.intentId) : existingReq?.intentId,
        routeHash: (p.route_hash ?? p.routeHash) ? String(p.route_hash ?? p.routeHash) : existingReq?.routeHash,
        options: Array.isArray(p.options) ? p.options : (existingReq?.options ?? []),
        needKind: needKind ?? existingReq?.needKind,
        pausesBehavior,
        ts: existingReq?.ts ?? ev.ts,
      };
      s.hitlRequests = existingReq
        ? s.hitlRequests.map((r) => r.id === requestId ? row : r)
        : [...s.hitlRequests, row];
      if (pausesBehavior && classifyHitlRequest(row) === "pending" && !s.awaitingOperator) {
        s.awaitingOperator = row.prompt;
      }
      if (!existingReq) {
        const lead = isEnv ? "environment problem" : "needs input";
        pushChat(s, {
          role: "agent", solverId: sid || undefined, kind: "text",
          content: `${lead}: ${need}`, ts: ev.ts,
        });
      }
      break;
    }
    case EventType.HITL_RESPONSE: {
      const t = p.text ?? p.hint ?? "";
      pushChat(s, { role: "human", kind: "guidance", content: `/${p.action ?? "hint"}${t ? " " + t : ""}  → ${p.target ?? "global"}`, ts: ev.ts });
      applyObservedControlState(s, p);
      break;
    }
    case EventType.HITL_RESOLVED: {
      const requestIds: string[] = [];
      if (Array.isArray(p.request_ids)) {
        for (const item of p.request_ids) {
          const id = String(item ?? "").trim();
          if (id && !requestIds.includes(id)) requestIds.push(id);
        }
      }
      if (p.request_id) {
        const id = String(p.request_id).trim();
        if (id && !requestIds.includes(id)) requestIds.push(id);
      }
      if (requestIds.length > 0) {
        const idSet = new Set(requestIds);
        const resolvedPrompts = new Set<string>();
        const commandId = p.command_id ? String(p.command_id) : undefined;
        const resolutionAction = String(p.action ?? "answer_decision");
        const deliveryMode = p.delivery ? String(p.delivery) : undefined;
        const continuationIntentId = p.continuation_intent_id ? String(p.continuation_intent_id) : undefined;
        for (const id of requestIds) {
          const req = s.hitlRequests.find((r) => r.id === id);
          const prev = s.hitlHistory.find((h) => h.id === id);
          if (req) resolvedPrompts.add(req.prompt);
          const target = req ?? prev;
          if (target) {
            s.hitlHistory = archiveHitlRequest(s.hitlHistory, target, {
              resolvedTs: ev.ts,
              resolutionAction,
              deliveryCommandId: commandId ?? prev?.deliveryCommandId,
              deliveryStatus: "effect_observed",
              deliveryDetail: prev?.deliveryDetail,
              deliveryMode: deliveryMode ?? prev?.deliveryMode,
              continuationIntentId: continuationIntentId ?? prev?.continuationIntentId,
            });
          }
        }
        s.hitlRequests = s.hitlRequests.filter((r) => !idSet.has(r.id));
        syncAwaitingOperator(s, resolvedPrompts);
      }
      break;
    }
    case EventType.HITL_TRANSLATED: {
      const w = p.worker ? String(p.worker) : "";
      const raw = String(p.need ?? "");
      const zh = String(p.need_zh ?? "");
      if (zh) {
        s.hitlRequests = s.hitlRequests.map((r) =>
          (r.worker === w && r.prompt === raw) ? { ...r, promptZh: zh } : r);
        s.hitlHistory = s.hitlHistory.map((h) =>
          (h.worker === w && h.prompt === raw) ? { ...h, promptZh: zh } : h);
      }
      break;
    }
    case EventType.GRAPH_COMPACTED: {
      s.compactEpochs = (s.compactEpochs ?? 0) + 1;
      s.lastCompactTs = ev.ts;
      const retired = typeof p.retired_intents === "number" ? p.retired_intents : 0;
      pushChat(s, { role: "system", kind: "guidance",
        content: `graph compacted (${p.trigger ?? "no progress"}) — retired ${retired} stale intent(s)`,
        ts: ev.ts });
      break;
    }
    default:
      return undefined;
  }
  return s;
}
