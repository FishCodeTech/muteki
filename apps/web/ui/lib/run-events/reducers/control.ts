/** Domain reducer extracted from ../reduce.ts; behavior intentionally unchanged. */
import {
  EventType,
  type ControlCommand,
  type ControlCommandStatus,
  type DeckState,
  type HitlNeedKind,
  type HitlRequest,
  type MutekiEvent,
} from "../types";
import {
  applyObservedControlState,
  gid,
  pushChat,
} from "../helpers";

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
      // The payload carries the CUMULATIVE ledger totals for the scope it was
      // emitted at (the backend emits the most specific scope available — almost
      // always per-solver, since reason/coordinator/CLI calls all carry a
      // solver_id). usd and the token counts move together in one payload.
      const cu = {
        usd: typeof p.usd === "number" ? p.usd : 0,
        tokensIn: typeof p.input_tokens === "number" ? p.input_tokens : 0,
        tokensOut: typeof p.output_tokens === "number" ? p.output_tokens : 0,
        unpricedCalls: typeof p.unpriced_calls === "number" ? p.unpriced_calls : undefined,
      };
      if (p.scope === "solver" && sid) {
        // store this agent's running total, then re-sum across agents for the
        // headline. The lane's engine tags the row in the hover card.
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
        // global/challenge scope (rare) — take the payload as the headline total,
        // but never let it shrink below the per-agent sum we already have.
        const sum = Object.values(s.costBySolver);
        s.usd = Math.max(cu.usd, sum.reduce((a, c) => a + c.usd, 0));
        s.tokensIn = Math.max(cu.tokensIn, sum.reduce((a, c) => a + c.tokensIn, 0));
        s.tokensOut = Math.max(cu.tokensOut, sum.reduce((a, c) => a + c.tokensOut, 0));
      }
      if (p.run_total && typeof p.run_total === "object") {
        // Older servers restarted the in-memory run ledger at each execution
        // generation. Historical event replay must not erase the prior
        // generation's already-observed per-solver usage.
        const observed = Object.values(s.costBySolver);
        s.usd = Math.max(Number(p.run_total.usd ?? 0), observed.reduce((a, c) => a + c.usd, 0));
        s.tokensIn = Math.max(Number(p.run_total.input_tokens ?? 0), observed.reduce((a, c) => a + c.tokensIn, 0));
        s.tokensOut = Math.max(Number(p.run_total.output_tokens ?? 0), observed.reduce((a, c) => a + c.tokensOut, 0));
      }
      break;
    }
    case EventType.GUIDANCE_INJECTED:
      // the solver acknowledged a human command landed in its context
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
        // Never let a delayed PERSISTED/ROUTED event regress an already-terminal
        // receipt. A later terminal receipt may still refine UNKNOWN into an
        // observed result during reconciliation.
        effectiveRow = existing && existingTerminal && !terminal
          ? existing : { ...existing, ...row };
        if (existing && terminal) {
          // A late terminal receipt is the newest operator-relevant change. Move it
          // to the tail so the top-bar "latest command" cannot stay stuck on a newer
          // command's non-terminal routed receipt.
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
      if (p.decision_closed === true && requestId && effectiveRow
          && (decisionAction === "answer_decision" || decisionAction === "dismiss")) {
        // `decision_closed` is the durable DecisionAnswer companion fence. A
        // merely RECEIVED command (or a pre-persistence failure/rejection) did
        // not record the answer and must leave the card editable. Once fenced,
        // correlation survives replay/reconnect and UNKNOWN/PARTIAL means recover
        // this command rather than submit a duplicate human answer.
        s.hitlRequests = s.hitlRequests.map((r) => r.id === String(requestId) ? {
          ...r,
          deliveryCommandId: effectiveRow!.id,
          deliveryStatus: effectiveRow!.status,
          deliveryDetail: effectiveRow!.detail,
        } : r);
      }
      if (p.decision_closed === true && effectiveRow?.status === "effect_observed") {
        if (requestId) {
          s.hitlRequests = s.hitlRequests.filter((r) => r.id !== String(requestId));
        }
      }
      if (!staleControlGeneration) applyObservedControlState(s, p);
      break;
    }
    case EventType.HITL_REQUEST: {
      // a worker raised its hand: it needs a resource (need_input) or the env is
      // down (env_down). payload = {worker, need, kind}. Fall back to the older
      // prompt/options shape if present.
      const need = (p.need ?? p.prompt ?? "needs your input").toString();
      const isEnv = p.kind === "env_down";
      const needKind = (p.need_kind ?? p.needKind) as HitlNeedKind | undefined;
      const requestId = String(p.request_id ?? p.requestId ?? p.id ?? gid("hitl"));
      const row: HitlRequest = {
        id: requestId,
        prompt: need,
        worker: p.worker ? String(p.worker) : undefined,
        options: p.options ?? [],
        needKind,
        // F: only an external_blocker freezes the swarm for an answer; the rest
        // auto-resolve (lane lock / route suppress / candidate). default-on for
        // back-compat (no need_kind → treat as a blocker, as before).
        pausesBehavior: needKind ? needKind === "external_blocker" : true,
        ts: ev.ts,
      };
      const existing = s.hitlRequests.some((r) => r.id === requestId);
      s.hitlRequests = existing
        ? s.hitlRequests.map((r) => r.id === requestId ? { ...r, ...row } : r)
        : [...s.hitlRequests, row];
      const lead = isEnv ? "environment problem" : "needs input";
      pushChat(s, {
        role: "agent", solverId: sid || undefined, kind: "text",
        content: `${lead}: ${need}`, ts: ev.ts,
      });
      break;
    }
    case EventType.HITL_RESPONSE: {
      // human command was accepted by the backend — echo as a human chat bubble
      // (this is what makes the operator's reply show up as normal history that
      // scrolls up into the coordinator thread).
      const t = p.text ?? p.hint ?? "";
      pushChat(s, { role: "human", kind: "guidance", content: `/${p.action ?? "hint"}${t ? " " + t : ""}  → ${p.target ?? "global"}`, ts: ev.ts });
      // Compatibility path for old clients that receive lifecycle on the echo:
      // accepted/routed is not an effect; only an observed effect changes pause.
      applyObservedControlState(s, p);
      break;
    }
    case EventType.HITL_TRANSLATED: {
      // a zh translation of a worker's hand-raise arrived (async). Match it to the
      // pending card by (worker, raw need) and attach promptZh so the card swaps to
      // Chinese; the raw English stays available. Eventual-consistency: the card was
      // rendered first from HITL_REQUEST, this just enriches it.
      const w = p.worker ? String(p.worker) : "";
      const raw = String(p.need ?? "");
      const zh = String(p.need_zh ?? "");
      if (zh) {
        s.hitlRequests = s.hitlRequests.map((r) =>
          (r.worker === w && r.prompt === raw) ? { ...r, promptZh: zh } : r);
      }
      break;
    }
    case EventType.GRAPH_COMPACTED: {
      // H: a long-run compaction epoch landed — bump the counter + announce it.
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
