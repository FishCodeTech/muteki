/** Domain reducer extracted from ../reduce.ts; behavior intentionally unchanged. */
import {
  EventType,
  type DeckState,
  type MutekiEvent,
} from "../types";
import {
  activityOnline,
  glueReasoningDelta,
  lane,
  lastOwnAgentIndex,
  pushChat,
  stripEnginePrefix,
  toolOutputLooksFailed,
  upsertStatusChat,
} from "../helpers";

function progressItems(value: unknown) {
  if (!Array.isArray(value)) return [];
  return value.flatMap((raw: any) => {
    if (!raw || typeof raw !== "object") return [];
    const text = String(raw.text ?? "").trim() || undefined;
    const textKey = String(raw.text_key ?? "").trim() || undefined;
    if (!text && !textKey) return [];
    const ref = raw.ref && typeof raw.ref === "object"
      ? {
          kind: String(raw.ref.kind ?? "event"),
          id: String(raw.ref.id ?? ""),
        }
      : undefined;
    return [{ text, textKey, ref }];
  });
}

function terminalSummary(summary: string, p: Record<string, any>, s: DeckState): string {
  // Older persisted final briefs inferred a count shortfall from solved=false.
  // Correct only that generated sentence; preserve authored summaries and the
  // recorded run outcome when replaying historical events.
  if (p.kind !== "final" || p.mode === "pentest" || s.mode === "pentest") return summary;
  const legacy = /^本轮执行已结束，已收集 (\d+) 个 Flag，未达到预期数量。$/.exec(summary);
  if (!legacy) return summary;
  const count = Number(legacy[1]);
  const prefix = `本轮执行已结束，已收集 ${count} 个 Flag`;
  if (s.platformConfirmationRequired) {
    return `${prefix}；需比赛平台确认后才能判定已解题。`;
  }
  if (s.multiFlag && s.expectedFlags <= 1) return `${prefix}。`;
  return count >= s.expectedFlags && s.expectedFlags > 0
    ? `${prefix}，已达到预期数量，但运行未标记为已解题。`
    : summary;
}

export function reduceConversation(ev: MutekiEvent, s: DeckState): DeckState | undefined {
  const p = ev.payload || {};
  switch (ev.event_type) {
    case EventType.REASONING_DELTA: {
      // Unscoped reasoning has no lane to attach to (see TEXT_MESSAGE_DELTA).
      if (!ev.solver_id) break;
      if (ev.solver_id === "reason" && p.main_thread !== true) break;
      const l = lane(s, ev.solver_id);
      const incoming = stripEnginePrefix(String(p.text ?? ""));
      s.lanes[l.solverId] = {
        ...l,
        reasoning: (l.reasoning + incoming).slice(-4000),
        status: "thinking",
        online: activityOnline(s),
        statusReason: undefined,
      };
      if (p.turn_end) {
        const idx = lastOwnAgentIndex(s.chat, l.solverId);
        if (idx >= 0 && s.chat[idx].kind === "reasoning") {
          const next = s.chat.slice();
          next[idx] = { ...next[idx], sealed: true };
          s.chat = next;
        }
        break;
      }
      const idx = lastOwnAgentIndex(s.chat, l.solverId);
      const last = idx >= 0 ? s.chat[idx] : undefined;
      if (last && last.kind === "reasoning" && !last.sealed && incoming) {
        const next = s.chat.slice();
        next[idx] = { ...last, content: glueReasoningDelta(last.content, incoming), ts: ev.ts };
        s.chat = next;
      } else if (incoming) {
        pushChat(s, { role: "agent", solverId: l.solverId, kind: "reasoning", content: incoming, ts: ev.ts });
      }
      break;
    }
    case EventType.TEXT_MESSAGE_DELTA: {
      // Unscoped text (legacy summarizer output) has no lane to attach to; its
      // content already lives in intent.summary / fact.summary via NODE_SUMMARIZED.
      if (!ev.solver_id) break;
      if (ev.solver_id === "reason" && p.main_thread !== true) break;
      const l = lane(s, ev.solver_id);
      s.lanes[l.solverId] = { ...l, online: activityOnline(s), statusReason: undefined };
      const mainThread = !!p.main_thread || l.statusReason === "standby";
      const incoming = stripEnginePrefix(String(p.text ?? ""));
      const idx = lastOwnAgentIndex(s.chat, l.solverId);
      const last = idx >= 0 ? s.chat[idx] : undefined;
      if (last && last.kind === "text" && !last.sealed && !!last.mainThread === mainThread && incoming) {
        const next = s.chat.slice();
        next[idx] = { ...last, content: glueReasoningDelta(last.content, incoming), ts: ev.ts };
        s.chat = next;
      } else if (incoming) {
        if (last && last.kind === "reasoning" && !last.sealed) {
          const next = s.chat.slice();
          next[idx] = { ...last, sealed: true };
          s.chat = next;
        }
        pushChat(s, { role: "agent", solverId: l.solverId, mainThread, kind: "text", content: incoming, ts: ev.ts });
      }
      break;
    }
    case EventType.PROGRESS_BRIEF: {
      const summary = terminalSummary(String(p.summary ?? "").trim(), p, s);
      const summarySource = String(p.summary_source ?? "");
      const summaryStatus = String(p.summary_status ?? "");
      const modelSummary = summarySource === "reason" && !!summary;
      if (!modelSummary && summaryStatus !== "failed" && summaryStatus !== "pending") break;
      const summaryError = String(p.summary_error ?? "Reason 未返回可用总结").trim();
      const summaryKey = summaryStatus === "failed"
        ? "progress.summaryFailed"
        : summaryStatus === "pending"
          ? "progress.summaryPending"
          : undefined;
      const briefId = String(p.brief_id ?? `progress-${ev.seq}`);
      const rawKind = String(p.kind ?? "periodic");
      const kind = ["periodic", "milestone", "blocker", "stalled", "final", "requested"].includes(rawKind)
        ? rawKind as "periodic" | "milestone" | "blocker" | "stalled" | "final" | "requested"
        : "periodic";
      const sections = p.sections && typeof p.sections === "object" ? p.sections : {};
      upsertStatusChat(s, `progress:${briefId}`, {
        role: "agent",
        solverId: "coordinator",
        mainThread: true,
        kind: "progress",
        content: summary || summaryError,
        ts: ev.ts,
        sealed: true,
        progressBrief: {
          id: briefId,
          kind,
          phase: String(p.phase ?? "running"),
          mode: p.mode === "pentest" ? "pentest" : "ctf",
          trigger: String(p.trigger ?? "automatic"),
          summary: modelSummary ? summary : undefined,
          summaryKey,
          summaryVars: summaryKey === "progress.summaryFailed" ? { detail: summaryError } : undefined,
          sourceFromSeq: Number(p.source_from_seq) || 0,
          sourceToSeq: Number(p.source_to_seq) || Number(ev.seq) || 0,
          sections: {
            confirmed: progressItems((sections as any).confirmed),
            active: progressItems((sections as any).active),
            blocked: progressItems((sections as any).blocked),
            next: progressItems((sections as any).next),
          },
        },
      });
      break;
    }
    case EventType.TOOL_CALL_START: {
      const l = lane(s, ev.solver_id);
      const command = String(p.tool ?? "tool");
      const sealIdx = lastOwnAgentIndex(s.chat, l.solverId);
      const sealLast = sealIdx >= 0 ? s.chat[sealIdx] : undefined;
      if (sealLast && (sealLast.kind === "reasoning" || sealLast.kind === "text") && !sealLast.sealed) {
        const sealed = s.chat.slice();
        sealed[sealIdx] = { ...sealLast, sealed: true };
        s.chat = sealed;
      }
      s.lanes[l.solverId] = {
        ...l,
        status: `tool: ${command}`,
        reasoning: "",
        online: activityOnline(s),
        statusReason: undefined,
      };
      pushChat(s, {
        role: "agent",
        solverId: l.solverId,
        kind: "tool",
        content: command,
        ts: ev.ts,
        toolPending: true,
      });
      break;
    }
    case EventType.TOOL_CALL_RESULT: {
      const l = lane(s, ev.solver_id);
      const res = p.result || {};
      const cond = (res.condensed ?? "").toString();
      const head = cond.split("\n")[0] || "(result)";
      const preview = cond.split("\n").slice(0, 40).join("\n").slice(0, 4000) || head;
      const failed = toolOutputLooksFailed(cond);
      s.lanes[l.solverId] = {
        ...l,
        toolLines: [...l.toolLines, head].slice(-12),
        online: activityOnline(s),
        statusReason: undefined,
      };
      const pendingIdx = s.chat.findIndex((m) => m.solverId === l.solverId && m.kind === "tool" && m.toolPending);
      if (pendingIdx >= 0) {
        const next = s.chat.slice();
        const open = next[pendingIdx];
        next[pendingIdx] = {
          ...open,
          ts: ev.ts,
          toolPending: false,
          toolOutput: preview,
          toolFailed: failed,
        };
        s.chat = next;
      } else if (preview) {
        pushChat(s, {
          role: "agent",
          solverId: l.solverId,
          kind: "tool",
          content: head,
          ts: ev.ts,
          toolOutput: preview,
          toolFailed: failed,
        });
      }
      break;
    }
    case EventType.TERMINAL_OUTPUT:
      s.terminal = [...s.terminal, p.text ?? ""].slice(-1000);
      break;
    default:
      return undefined;
  }
  return s;
}
