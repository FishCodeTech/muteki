import { existsSync, readdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { dirname, join, relative, resolve } from "node:path";

import { Type } from "typebox";

type Priority = "high" | "normal" | "low";
type DraftOperation = Record<string, unknown> & { op: string };

const outputPath = process.env.MUTEKI_DECIDE_DRAFT_PATH || "";
const graphPath = process.env.MUTEKI_DECIDE_GRAPH_PATH || "";
const configuredMaxSteps = Number.parseInt(
  process.env.MUTEKI_DECIDE_MAX_STEPS || "4",
  10,
);
const maxSteps = Number.isFinite(configuredMaxSteps)
  ? Math.max(-1, configuredMaxSteps)
  : 4;
const challengerAudit = process.env.MUTEKI_DECIDE_CHALLENGER === "1";
const operations: DraftOperation[] = [];
const peekedSteps = new Set<string>();
let committed = false;
let nextDraftId = 0;

function listJsonlFiles(dir: string): string[] {
  const results: string[] = [];
  try {
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      const fullPath = join(dir, entry.name);
      if (entry.isDirectory()) {
        results.push(...listJsonlFiles(fullPath));
      } else if (entry.isFile() && entry.name.endsWith(".jsonl")) {
        results.push(fullPath);
      }
    }
  } catch {
    return results;
  }
  return results;
}

function extractPiJsonlProgress(agentStateDir: string): {
  started_ts?: number;
  tool_calls?: number;
  recent_traces?: Array<{ tool: string; command: string; output: string }>;
  latest_note?: string;
} {
  if (!agentStateDir || !existsSync(agentStateDir)) return {};
  const sessionsDir = join(agentStateDir, "sessions");
  const searchRoot = existsSync(sessionsDir) ? sessionsDir : agentStateDir;
  const candidates = listJsonlFiles(searchRoot)
    .map((filePath) => {
      try {
        const st = statSync(filePath);
        return { filePath, mtimeMs: st.mtimeMs };
      } catch {
        return null;
      }
    })
    .filter((item): item is { filePath: string; mtimeMs: number } => item !== null)
    .sort((a, b) => b.mtimeMs - a.mtimeMs);
  if (candidates.length === 0) return {};
  const latest = candidates[0];
  let rawLines: string[] = [];
  try {
    rawLines = readFileSync(latest.filePath, "utf8").split(/\r?\n/);
  } catch {
    return {};
  }
  let toolCalls = 0;
  let startedTs = 0;
  const callOrder: string[] = [];
  const callsById = new Map<string, { tool: string; command: string; output: string }>();
  let latestNote = "";
  for (const rawLine of rawLines) {
    if (!rawLine.trim()) continue;
    let row: any;
    try {
      row = JSON.parse(rawLine);
    } catch {
      continue;
    }
    if (!row || typeof row !== "object") continue;
    if (startedTs <= 0 && row.timestamp != null) {
      if (typeof row.timestamp === "number" && row.timestamp > 0) {
        startedTs = row.timestamp > 1e11 ? row.timestamp / 1000 : row.timestamp;
      } else if (typeof row.timestamp === "string" && row.timestamp.trim()) {
        const parsedMs = Date.parse(row.timestamp.trim());
        if (Number.isFinite(parsedMs) && parsedMs > 0) {
          startedTs = parsedMs / 1000;
        }
      }
    }
    if (String(row.type || "") === "message") {
      const msg = row.message;
      if (!msg || typeof msg !== "object") continue;
      const role = String(msg.role || "");
      const content = msg.content;
      if (role === "assistant" && Array.isArray(content)) {
        for (const block of content) {
          if (!block || typeof block !== "object") continue;
          const btype = String(block.type || "");
          if (btype === "text" || btype === "thinking") {
            const txt = String(block[btype] || block.text || "").replace(/\s+/g, " ").trim();
            if (txt) latestNote = txt.slice(0, 360);
          } else if (btype === "toolCall") {
            toolCalls += 1;
            const cid = String(block.id || `call_${toolCalls}`);
            const tname = String(block.name || "tool");
            const targs = block.arguments;
            let cmdStr = "";
            if (targs && typeof targs === "object") {
              cmdStr = String(
                targs.command || targs.path || targs.title || JSON.stringify(targs),
              ).trim();
            }
            callOrder.push(cid);
            callsById.set(cid, {
              tool: tname,
              command: cmdStr.replace(/\s+/g, " ").slice(0, 240),
              output: "(running...)",
            });
          }
        }
      } else if (role === "toolResult") {
        const cid = String(msg.toolCallId || "");
        const outParts: string[] = [];
        if (Array.isArray(content)) {
          for (const block of content) {
            if (block && typeof block === "object" && block.type === "text") {
              outParts.push(String(block.text || ""));
            }
          }
        } else if (typeof content === "string") {
          outParts.push(content);
        }
        const outText = outParts.join(" ").replace(/\s+/g, " ").trim();
        const existing = callsById.get(cid);
        if (existing) {
          existing.output = outText ? outText.slice(0, 280) : "(empty output)";
        }
      }
    }
  }
  const recentTraces = callOrder
    .slice(-6)
    .map((cid) => callsById.get(cid))
    .filter((item): item is { tool: string; command: string; output: string } => item !== undefined);
  return {
    started_ts: startedTs > 0 ? startedTs : undefined,
    tool_calls: toolCalls,
    recent_traces: recentTraces,
    latest_note: latestNote,
  };
}

function renderDoneStepBlock(row: any, headerNote = ""): string {
  const stepId = String(row?.id || "");
  const state = String(row?.state || "done");
  const workerId = String(row?.worker_id || "");
  const action = String(row?.action || "");
  let toolCalls = Number(row?.tool_calls || 0);
  let elapsedS = Number(row?.elapsed_seconds || 0);
  let traces: Array<{ tool?: string; command?: string; output?: string }> = Array.isArray(
    row?.recent_traces,
  )
    ? row.recent_traces
    : [];
  if (traces.length === 0 && graphPath && stepId) {
    const traceFile = join(dirname(graphPath), "..", "step_traces", `${stepId}.json`);
    if (existsSync(traceFile)) {
      try {
        const tf = JSON.parse(readFileSync(traceFile, "utf8"));
        if (Array.isArray(tf?.recent_traces)) traces = tf.recent_traces;
        if (Number(tf?.tool_calls || 0) > toolCalls) toolCalls = Number(tf.tool_calls);
        if (Number(tf?.elapsed_seconds || 0) > elapsedS) elapsedS = Number(tf.elapsed_seconds);
      } catch {
        // ignore
      }
    }
  }
  const toFacts: string[] = Array.isArray(row?.to)
    ? row.to.map((f: any) => String(f || "")).filter(Boolean)
    : [];
  const lines: string[] = [];
  if (headerNote) lines.push(headerNote);
  const wInfo = workerId ? `Worker ${workerId}，` : "";
  const elInfo = elapsedS > 0 ? `耗时 ${elapsedS.toFixed(0)}s，` : "";
  const toInfo = toFacts.length > 0 ? `，产出 ${toFacts.join(", ")}` : "";
  lines.push(`step: ${stepId}`);
  lines.push(`state: ${state} (已结案，${wInfo}${elInfo}共执行 ${toolCalls} 次工具调用${toInfo})`);
  lines.push(`action: ${action}`);
  if (traces.length > 0) {
    lines.push("executed_tool_calls:");
    traces.slice(-15).forEach((tr, i) => {
      if (!tr || typeof tr !== "object") return;
      const tname = String(tr.tool || "tool");
      const cmd = String(tr.command || "");
      const out = String(tr.output || "");
      lines.push(`  [${i + 1}] ${tname}: ${cmd}`);
      if (out) {
        lines.push(`      -> ${out}`);
      }
    });
  } else {
    lines.push("executed_tool_calls: (无已保存的工具输出记录)");
  }
  return lines.join("\n");
}

function runPeekStep(intentId: string): string {
  let target = String(intentId || "").trim();
  if (!target) return "intent_id 不能为空";
  let graph: any = { open_steps: [], done_steps: [], fact_to_step: {} };
  if (graphPath) {
    try {
      graph = JSON.parse(readFileSync(graphPath, "utf8"));
    } catch {
      graph = { open_steps: [], done_steps: [], fact_to_step: {} };
    }
  }
  const factToStep: Record<string, string> =
    graph?.fact_to_step && typeof graph.fact_to_step === "object" ? graph.fact_to_step : {};
  let headerNote = "";
  if (target.startsWith("fact_") && factToStep[target]) {
    const resolvedStep = String(factToStep[target]).trim();
    headerNote = `[${target} 由 ${resolvedStep} 产出]`;
    target = resolvedStep;
  }
  const openSteps = Array.isArray(graph?.open_steps) ? graph.open_steps : [];
  const row = openSteps.find(
    (item: any) => item && typeof item === "object" && String(item.id || "").trim() === target,
  );
  const doneSteps = Array.isArray(graph?.done_steps) ? graph.done_steps : [];
  if (!row) {
    const doneRow = doneSteps.find(
      (item: any) => item && typeof item === "object" && String(item.id || "").trim() === target,
    );
    if (doneRow) {
      let outText = renderDoneStepBlock(doneRow, headerNote);
      if (target.startsWith("step_")) {
        const altFact = "fact_" + target.slice(5);
        const altStep = String(factToStep[altFact] || "").trim();
        if (altStep && altStep !== target) {
          const altRow = doneSteps.find(
            (item: any) =>
              item && typeof item === "object" && String(item.id || "").trim() === altStep,
          );
          if (altRow) {
            outText +=
              "\n\n" +
              renderDoneStepBlock(
                altRow,
                `[附：${altFact} 实际由 ${altStep} 产出，以下一并附上 ${altStep} 的记录]`,
              );
          }
        }
      }
      return outText;
    }
    return `${target} 不在当前 open 或已结案 steps 中（ID 不存在）。`;
  }
  const action = String(row.action || "");
  if (!row.running) {
    return (
      (headerNote ? `${headerNote}\n` : "") +
      `step: ${target}\n` +
      `state: open (排队待执行，尚无 Worker 认领)\n` +
      `action: ${action}`
    );
  }
  const workerId = String(row.worker_id || "");
  const agentStateDir = String(row.agent_state_dir || "").trim();
  const liveJsonl = agentStateDir ? extractPiJsonlProgress(agentStateDir) : {};
  let stepTraceFileInfo: any = {};
  if (graphPath && target) {
    const tfPath = join(dirname(graphPath), "..", "step_traces", `${target}.json`);
    if (existsSync(tfPath)) {
      try {
        stepTraceFileInfo = JSON.parse(readFileSync(tfPath, "utf8"));
      } catch {
        stepTraceFileInfo = {};
      }
    }
  }
  const toolCalls = Math.max(
    Number(row.tool_calls || 0),
    Number(liveJsonl.tool_calls || 0),
    Number(stepTraceFileInfo.tool_calls || 0),
  );
  const startedAt = Number(
    row.started_at || liveJsonl.started_ts || stepTraceFileInfo.started_at || 0,
  );
  const elapsedS =
    startedAt > 0
      ? Math.max(0, Date.now() / 1000 - startedAt)
      : Math.max(0, Number(row.elapsed_seconds || 0));
  const traces: Array<{ tool?: string; command?: string; output?: string }> =
    liveJsonl.recent_traces && liveJsonl.recent_traces.length > 0
      ? liveJsonl.recent_traces
      : Array.isArray(stepTraceFileInfo.recent_traces) &&
          stepTraceFileInfo.recent_traces.length > 0
        ? stepTraceFileInfo.recent_traces
        : Array.isArray(row.recent_traces)
          ? row.recent_traces
          : [];
  const latestNote = String(liveJsonl.latest_note || row.latest_note || "").trim();
  const lines: string[] = [];
  if (headerNote) lines.push(headerNote);
  lines.push(
    `step: ${target}`,
    `state: running (Worker ${workerId || "active"} 正在执行中，已运行 ${elapsedS.toFixed(0)}s，已执行 ${toolCalls} 次工具调用)`,
    `action: ${action}`,
  );
  if (traces.length > 0) {
    lines.push("recent_tool_calls:");
    traces.slice(-15).forEach((tr, i) => {
      if (!tr || typeof tr !== "object") return;
      const tname = String(tr.tool || "tool");
      const cmd = String(tr.command || "");
      const out = String(tr.output || "");
      lines.push(`  [${i + 1}] ${tname}: ${cmd}`);
      if (out) {
        lines.push(`      -> ${out}`);
      }
    });
  } else {
    lines.push("recent_tool_calls: (尚无已完成工具输出)");
  }
  if (latestNote) {
    lines.push(`latest_progress: ${latestNote}`);
  }
  return lines.join("\n");
}

function renderDecidePreview(): string {
  let graph: any = {
    schema: "muteki.ctf-decide-graph.v1",
    max_intents: maxSteps,
    graph_seq: 0,
    fact_id_map: {},
    facts: [],
    goals: [],
    open_steps: [],
    reserved_goal_ids: [],
    resource_ids: [],
  };
  if (graphPath) {
    try {
      graph = JSON.parse(readFileSync(graphPath, "utf8"));
    } catch {
      // keep fallback empty snapshot
    }
  }
  const activeFactMap = new Map<string, number>();
  for (const [k, v] of Object.entries(graph.fact_id_map || {})) {
    if (String(k).trim()) activeFactMap.set(String(k), Number(v));
  }
  const seqToId = new Map<number, string>();
  for (const [fid, seq] of activeFactMap.entries()) {
    seqToId.set(seq, fid);
  }
  const knownGoals = new Map<string, { id: string; title: string; state: string }>();
  for (const row of Array.isArray(graph.goals) ? graph.goals : []) {
    if (row && typeof row === "object") {
      const gid = String(row.id || row.goal_id || "");
      if (gid) {
        knownGoals.set(gid, {
          id: gid,
          title: String(row.title || row.criterion || gid),
          state: String(row.state || "open"),
        });
      }
    }
  }
  if (!knownGoals.has("goal_final")) {
    knownGoals.set("goal_final", { id: "goal_final", title: "goal", state: "open" });
  }
  const reservedGoals = new Set<string>([
    ...knownGoals.keys(),
    ...(Array.isArray(graph.reserved_goal_ids) ? graph.reserved_goal_ids.map(String) : []),
  ]);
  const openSteps: any[] = Array.isArray(graph.open_steps) ? graph.open_steps : [];
  const activeOpenIds = new Set<string>(
    openSteps.map((row) => String(row?.id || "").trim()).filter(Boolean),
  );
  const droppedOpenIds = new Set<string>(
    operations
      .filter(
        (op) =>
          op.op === "drop_step" &&
          activeOpenIds.has(String(op.intent_id || op.step_id || "").trim()),
      )
      .map((op) => String(op.intent_id || op.step_id || "").trim()),
  );
  const rawMax = graph.max_intents !== undefined ? Number(graph.max_intents) : maxSteps;
  const baseMax = Number.isFinite(rawMax) ? rawMax : 4;
  const effectiveMax = baseMax >= 0 ? baseMax + droppedOpenIds.size : baseMax;
  const isPentest = String(graph.schema || "") === "muteki.pentest-decide-graph.v1";
  const hasResourceConstraint = Array.isArray(graph.resource_ids);
  const availableResources = new Set<string>(
    hasResourceConstraint ? graph.resource_ids.map((r: any) => String(r)).filter(Boolean) : [],
  );

  const initialFactMap = new Map<string, number>(activeFactMap);
  const sourcesValid = (raw: unknown): boolean => {
    if (!Array.isArray(raw)) return false;
    const knownSeqs = new Set(activeFactMap.values());
    return raw.every((item) => {
      const s = String(item);
      if (activeFactMap.has(s)) return true;
      if (typeof item !== "boolean" && /^\d+$/.test(s) && knownSeqs.has(Number(s))) return true;
      return false;
    });
  };
  const openSourcesValid = (raw: unknown): boolean => {
    if (!Array.isArray(raw)) return false;
    const knownSeqs = new Set(initialFactMap.values());
    return raw.every((item) => {
      const s = String(item);
      if (initialFactMap.has(s)) return true;
      if (typeof item !== "boolean" && /^\d+$/.test(s) && knownSeqs.has(Number(s))) return true;
      return false;
    });
  };
  const positiveFactIds = (raw: unknown, useInitial = false): number[] => {
    if (!Array.isArray(raw)) return [];
    const fmap = useInitial ? initialFactMap : activeFactMap;
    const out: number[] = [];
    for (const item of raw) {
      const s = String(item ?? "").trim();
      const seq = fmap.has(s) ? Number(fmap.get(s)) : Number(item);
      if (Number.isInteger(seq) && seq > 0 && !out.includes(seq)) {
        out.push(seq);
      }
    }
    return out;
  };

  const intents: Array<{ id: string; action: string; priority: string; from: number[] }> = [];
  const supersede: string[] = [];
  const reprioritize = new Map<string, string>();
  const supersedeFacts: Array<{ fact_id: string; fact_seq: number; by_fact_seq?: number }> = [];
  const subGoalOps: Array<{ op: string; goal_id: string; criterion?: string; evidence_fact_seqs?: number[] }> = [];
  const receipts: Array<{
    raw_index: number;
    op: string;
    outcome: string;
    reason_code: string;
    goal: string;
    model_intent_id: string;
  }> = [];

  const record = (
    index: number,
    op: string,
    outcome: string,
    reasonCode: string,
    goal = "",
    intentId = "",
  ) => {
    receipts.push({
      raw_index: index,
      op,
      outcome,
      reason_code: reasonCode,
      goal: goal.replace(/\s+/g, " ").trim(),
      model_intent_id: intentId,
    });
  };

  operations.forEach((operation, index) => {
    const op = String(operation?.op || "").trim();
    if (op === "open_step") {
      const action = String(operation.action || "").trim();
      if (!action) {
        record(index, op, "dropped", "missing_action");
        return;
      }
      const activeSeqs = new Set(activeFactMap.values());
      const activeDraftCount = intents.filter((it) =>
        it.from.every((seq) => activeSeqs.has(seq)),
      ).length;
      if (effectiveMax >= 0 && activeDraftCount >= effectiveMax) {
        record(index, op, "dropped", "max_intents_exceeded", action);
        return;
      }
      const expected = String(operation.expected_observable || operation.expectedObservable || "").trim();
      const stop = String(operation.stop_condition || operation.stopCondition || "").trim();
      const coverage = String(operation.coverage_key || operation.coverageKey || "").trim();
      if (isPentest && (!expected || !stop || !coverage)) {
        record(index, op, "dropped", "missing_step_contract", action);
        return;
      }
      if (!Array.isArray(operation.from)) {
        record(index, op, "dropped", "invalid_from", action);
        return;
      }
      if (!openSourcesValid(operation.from)) {
        record(index, op, "dropped", "unknown_fact", action);
        return;
      }
      const rawResources = operation.requires ?? [];
      if (
        !Array.isArray(rawResources) ||
        rawResources.some((item) => typeof item !== "string" || !item.trim())
      ) {
        record(index, op, "dropped", "invalid_resource", action);
        return;
      }
      if (hasResourceConstraint && rawResources.some((item) => !availableResources.has(item.trim()))) {
        record(index, op, "dropped", "unknown_resource", action);
        return;
      }
      const fromFacts = positiveFactIds(operation.from, true);
      let prio = String(operation.priority || "normal").trim().toLowerCase();
      if (!["high", "normal", "low"].includes(prio)) prio = "normal";
      const intentId = `D${intents.length + 1}`;
      intents.push({ id: intentId, action, priority: prio, from: fromFacts });
      record(index, op, "accepted", "accepted", action, intentId);
    } else if (op === "drop_step") {
      const intentId = String(operation.intent_id || operation.step_id || "").trim();
      if (!intentId) {
        record(index, op, "dropped", "missing_intent_id");
        return;
      }
      if (supersede.includes(intentId)) {
        record(index, op, "dropped", "duplicate_drop", "", intentId);
        return;
      }
      supersede.push(intentId);
      record(index, op, "accepted", "drop_step", "", intentId);
    } else if (op === "change_step_priority") {
      const intentId = String(operation.intent_id || "").trim();
      const prio = String(operation.priority || "").trim().toLowerCase();
      if (intentId && ["high", "normal", "low"].includes(prio)) {
        reprioritize.set(intentId, prio);
        record(index, op, "accepted", "change_step_priority", "", intentId);
      } else {
        record(index, op, "dropped", "invalid_priority", "", intentId);
      }
    } else if (op === "supersede_fact") {
      const rawFactId = String(operation.fact_id || "").trim();
      const reason = String(operation.reason || "").trim();
      const rawById = String(operation.by_fact_id || "").trim();
      const knownSeqs = new Set(activeFactMap.values());
      const targetSeq =
        Number(operation.fact_seq || 0) ||
        (rawFactId && rawFactId !== "fact_origin" ? Number(activeFactMap.get(rawFactId) || 0) : 0);
      const bySeq =
        Number(operation.by_fact_seq || 0) ||
        (rawById && rawById !== "fact_origin" ? Number(activeFactMap.get(rawById) || 0) : 0);
      const byRequested = Boolean(rawById) || Number(operation.by_fact_seq || 0) > 0;
      if (
        targetSeq <= 0 ||
        !knownSeqs.has(targetSeq) ||
        !reason ||
        (byRequested && (bySeq <= 0 || bySeq === targetSeq || !knownSeqs.has(bySeq)))
      ) {
        record(index, op, "dropped", "invalid_supersede_fact", rawFactId);
        return;
      }
      if (supersedeFacts.some((item) => item.fact_seq === targetSeq)) {
        record(index, op, "dropped", "duplicate_supersede_fact", rawFactId);
        return;
      }
      activeFactMap.delete(rawFactId);
      supersedeFacts.push({
        fact_id: rawFactId,
        fact_seq: targetSeq,
        by_fact_seq: bySeq > 0 ? bySeq : undefined,
      });
      record(index, op, "accepted", "supersede_fact", rawFactId);
    } else if (op === "add_sub_goal") {
      const criterion = String(operation.criterion || "").trim();
      if (!criterion) {
        record(index, op, "dropped", "missing_goal_criterion");
        return;
      }
      let num = reservedGoals.size - (reservedGoals.has("goal_final") ? 1 : 0) + 1;
      while (reservedGoals.has(`goal_${String(num).padStart(3, "0")}`)) num += 1;
      const goalId = `goal_${String(num).padStart(3, "0")}`;
      const requestedId = String(operation.goal_id || goalId);
      if (requestedId !== goalId) {
        record(index, op, "dropped", "stale_goal_id");
        return;
      }
      reservedGoals.add(goalId);
      knownGoals.set(goalId, { id: goalId, title: criterion, state: "open" });
      subGoalOps.push({ op, goal_id: goalId, criterion });
      record(index, op, "accepted", op, goalId);
    } else if (op === "satisfy_goal") {
      const goalId = String(operation.goal_id || "goal_final").trim();
      if (!knownGoals.has(goalId)) {
        record(index, op, "dropped", "unknown_goal", goalId);
        return;
      }
      const rawSources = operation.from ?? [];
      if (!sourcesValid(rawSources) || (goalId !== "goal_final" && (!Array.isArray(rawSources) || rawSources.length === 0))) {
        record(index, op, "dropped", "invalid_goal_evidence", goalId);
        return;
      }
      const reason = String(operation.reason || operation.why || "").trim();
      if (!reason) {
        record(index, op, "dropped", "missing_goal_reason", goalId);
        return;
      }
      const sources = positiveFactIds(rawSources);
      if (goalId !== "goal_final") {
        subGoalOps.push({ op, goal_id: goalId, evidence_fact_seqs: sources });
      }
      record(index, op, "accepted", "satisfy_goal", goalId);
    } else if (op === "commit") {
      record(index, op, "accepted", "commit");
    } else if (op === "preview" || op === "peek_step") {
      record(index, op, "ignored", "preview_not_an_op");
    } else {
      record(index, op || "unknown", "dropped", "unknown_op");
    }
  });

  const openSourcesRemainValid = (raw: unknown): boolean => sourcesValid(raw);
  const removedIntents = new Set<string>();
  let goalMet = false;
  for (const r of receipts) {
    if (r.outcome !== "accepted" || (r.op !== "open_step" && r.op !== "satisfy_goal")) continue;
    const opObj = operations[r.raw_index];
    if (r.op === "open_step") {
      if (!openSourcesRemainValid(opObj?.from ?? [])) {
        r.outcome = "dropped";
        r.reason_code = "superseded_fact_in_draft";
        removedIntents.add(r.model_intent_id);
      }
    } else if (!sourcesValid(opObj?.from ?? [])) {
      r.outcome = "dropped";
      r.reason_code = "superseded_fact_in_draft";
    } else if (r.goal === "goal_final") {
      goalMet = true;
    }
  }
  const finalIntents = intents.filter((it) => !removedIntents.has(it.id));
  const activeSeqs = new Set(activeFactMap.values());
  const validSubGoalOps = subGoalOps.filter(
    (item) =>
      item.op !== "satisfy_goal" ||
      (item.evidence_fact_seqs || []).every((seq) => activeSeqs.has(seq)),
  );

  const droppedSet = new Set(supersede);
  const revisedRefs = new Set<string>();
  for (const item of supersedeFacts) {
    revisedRefs.add(String(item.fact_id));
    revisedRefs.add(String(item.fact_seq));
  }
  const remaining: Array<{ id: string; action: string; priority: string; from: string[]; new: boolean }> = [];
  for (const row of openSteps) {
    if (!row || typeof row !== "object") continue;
    const stepId = String(row.id || "").trim();
    const rowFrom: unknown[] = Array.isArray(row.from) ? row.from : [];
    if (!stepId || droppedSet.has(stepId) || rowFrom.some((s) => revisedRefs.has(String(s)))) {
      continue;
    }
    remaining.push({
      id: stepId,
      action: String(row.action || ""),
      priority: reprioritize.get(stepId) || String(row.priority || "normal"),
      from: rowFrom.map(String),
      new: false,
    });
  }
  for (const it of finalIntents) {
    remaining.push({
      id: it.id,
      action: it.action,
      priority: it.priority,
      from: it.from.map((seq) => seqToId.get(seq) || String(seq)),
      new: true,
    });
  }

  const lines: string[] = [
    `# advisory snapshot seq=${graph.graph_seq || 0} draft_ops=${operations.length} (commit 以当时图重验为准)`,
  ];
  if (committed) lines.push("# draft already committed");
  if (goalMet) lines.push("# satisfy_goal: 按现有规则将接受达成标记，不是证据验证通过");
  lines.push("goals:");
  const projectedGoals = new Map<string, { id: string; title: string; state: string }>();
  for (const goal of Array.isArray(graph.goals) ? graph.goals : []) {
    if (goal && typeof goal === "object" && goal.id) {
      projectedGoals.set(String(goal.id), {
        id: String(goal.id),
        title: String(goal.title || goal.id),
        state: String(goal.state || "open"),
      });
    }
  }
  for (const opItem of validSubGoalOps) {
    if (opItem.op === "add_sub_goal") {
      projectedGoals.set(opItem.goal_id, {
        id: opItem.goal_id,
        title: String(opItem.criterion || opItem.goal_id),
        state: "open",
      });
    } else if (projectedGoals.has(opItem.goal_id)) {
      projectedGoals.get(opItem.goal_id)!.state = "satisfied";
    }
  }
  for (const [goalId, goal] of projectedGoals.entries()) {
    const state = goalMet && goalId === "goal_final" ? "satisfied" : String(goal.state || "open");
    lines.push(`  - ${goalId}: ${goal.title || goalId} [${state}]`);
  }
  lines.push("facts:");
  const revisedIds = new Set(supersedeFacts.map((item) => item.fact_id));
  const activeFacts = (Array.isArray(graph.facts) ? graph.facts : []).filter(
    (row: any) => row && typeof row === "object" && !revisedIds.has(String(row.id || "")),
  );
  if (activeFacts.length > 0) {
    for (const fact of activeFacts) {
      lines.push(`  - ${fact.id}: ${String(fact.title || "")}`);
    }
  } else {
    lines.push("  []");
  }
  lines.push("steps:");
  if (remaining.length > 0) {
    for (const step of remaining) {
      const mark = step.new ? " new" : "";
      const fromIds = step.from.join(",") || "-";
      lines.push(`  - ${step.id} [${step.priority}${mark}]: ${step.action}  from=${fromIds}`);
    }
  } else {
    lines.push("  []");
  }
  lines.push("changes:");
  lines.push(supersede.length > 0 ? `  drop: [${supersede.join(", ")}]` : "  drop: []");
  if (reprioritize.size > 0) {
    const changed = Array.from(reprioritize.entries())
      .map(([id, prio]) => `${id}->${prio}`)
      .join(", ");
    lines.push(`  priority: [${changed}]`);
  } else {
    lines.push("  priority: []");
  }
  const opened = finalIntents.map((it) => it.id);
  lines.push(opened.length > 0 ? `  open: [${opened.join(", ")}]` : "  open: []");
  lines.push("receipts:");
  if (receipts.length > 0) {
    for (const row of receipts) {
      const label = row.goal || row.model_intent_id || "";
      const suffix = label ? ` ${label}` : "";
      lines.push(`  - op[${row.raw_index}] ${row.op} ${row.outcome} ${row.reason_code}${suffix}`);
    }
  } else {
    lines.push("  []");
  }
  return lines.join("\n");
}

function snapshotGraphSeq(): number {
  if (!graphPath) return -1;
  try {
    const raw = JSON.parse(readFileSync(graphPath, "utf8"));
    const seq = Number(raw?.graph_seq);
    return Number.isInteger(seq) ? seq : -1;
  } catch {
    return -1;
  }
}

function persist() {
  if (!outputPath) {
    throw new Error("MUTEKI_DECIDE_DRAFT_PATH is required");
  }
  writeFileSync(
    outputPath,
    JSON.stringify({
      schema: "muteki.ctf-decide-draft.v1",
      committed,
      graph_seq: snapshotGraphSeq(),
      operations,
    }),
    { encoding: "utf8", mode: 0o600 },
  );
}

function rejectChallengerPlan() {
  if (!challengerAudit) return null;
  return text(
    "rejected: 挑战者回合不能取消或调整 Step。可以 supersede_fact、open_step 或 satisfy_goal，然后 commit。",
  );
}

function text(value: unknown) {
  return {
    content: [{ type: "text", text: String(value) }],
    details: { operationCount: operations.length, committed },
    get terminate() {
      return committed;
    },
  };
}

function contractError(detail: string) {
  return {
    isError: true,
    content: [{ type: "text", text: `open_step contract error: ${detail}` }],
  };
}

function normalizeCoverageKey(value: string): string {
  return String(value || "").trim().toLowerCase().replace(/[\s_]+/g, "-").replace(/^-+|-+$/g, "");
}

function sameCoverageSurface(left: string, right: string): boolean {
  const normA = normalizeCoverageKey(left);
  const normB = normalizeCoverageKey(right);
  return Boolean(normA && normB && normA === normB);
}

function sameQuestion(left: string, right: string): boolean {
  const a = String(left || "").trim().replace(/\s+/g, " ").toLowerCase();
  const b = String(right || "").trim().replace(/\s+/g, " ").toLowerCase();
  return Boolean(a && b && a === b);
}

function knownReferences(): {
  isPentest: boolean;
  facts: Map<string, number>;
  initialFacts: Map<string, number>;
  factStats: Map<number, { id: string; negative_outcomes: number; enabling_outcomes: number }>;
  cancelled: Array<{ seq?: number; question?: string; tested_scope?: string; coverage_key?: string; tool_calls?: number }>;
  resources: Set<string>;
  openSteps: Set<string>;
  runningSteps: Set<string>;
  activeRunningSteps: Set<string>;
  goals: Set<string>;
  reservedGoals: Set<string>;
} {
  if (!graphPath) throw new Error("MUTEKI_DECIDE_GRAPH_PATH is required");
  const graph = JSON.parse(readFileSync(graphPath, "utf8"));
  const facts = new Map<string, number>(Object.entries(graph.fact_id_map || {}).map(
    ([id, seq]) => [id, Number(seq)],
  ));
  const initialFacts = new Map<string, number>(facts);
  const factStats = new Map<number, { id: string; negative_outcomes: number; enabling_outcomes: number }>();
  for (const row of graph.facts || []) {
    if (row && typeof row.seq === "number") {
      factStats.set(Number(row.seq), {
        id: String(row.id || ""),
        negative_outcomes: Number(row.negative_outcomes || 0),
        enabling_outcomes: Number(row.enabling_outcomes || 0),
      });
    }
  }
  const goals = new Set<string>(["goal_final", ...(graph.goals || []).map((row: any) => String(row.id || row.goal_id || ""))]);
  const reservedGoals = new Set<string>([...goals, ...(graph.reserved_goal_ids || []).map(String)]);
  for (const operation of operations) {
    if (operation.op === "supersede_fact") {
      facts.delete(String(operation.fact_id));
    }
    if (operation.op === "add_sub_goal") {
      goals.add(String(operation.goal_id));
      reservedGoals.add(String(operation.goal_id));
    }
  }
  return {
    isPentest: String(graph.schema || "") === "muteki.pentest-decide-graph.v1",
    facts,
    initialFacts,
    factStats,
    cancelled: Array.isArray(graph.cancelled) ? graph.cancelled : [],
    goals,
    reservedGoals,
    resources: new Set((graph.resource_ids || []).map(String)),
    openSteps: new Set(
      (graph.open_steps || [])
        .map((item: any) => String(item?.id || ""))
        .filter(Boolean),
    ),
    runningSteps: new Set(
      (graph.open_steps || [])
        .filter((item: any) => Boolean(item?.running))
        .map((item: any) => String(item?.id || ""))
        .filter(Boolean),
    ),
    activeRunningSteps: new Set(
      (graph.open_steps || [])
        .filter(
          (item: any) =>
            Boolean(item?.running) &&
            (Boolean(item?.has_live_context) || Number(item?.tool_calls || 0) > 0),
        )
        .map((item: any) => String(item?.id || ""))
        .filter(Boolean),
    ),
  };
}

function referencesActive(operation: { from?: unknown }, facts: Map<string, number>): boolean {
  const sources = operation.from || [];
  if (!Array.isArray(sources)) return false;
  const activeSeqs = new Set(facts.values());
  return sources.every((value) => facts.has(String(value)) || activeSeqs.has(Number(value)));
}

function add(operation: DraftOperation) {
  if (committed) {
    throw new Error("draft is already committed");
  }
  operations.push(operation);
  persist();
  return text(`已记入：${operation.op}`);
}

export default function (pi: any) {
  const pluginPath = process.env.MUTEKI_DECIDE_PLUGIN_PATH;
  if (pluginPath) {
    const configured = JSON.parse(readFileSync(pluginPath, "utf8"));
    for (const tool of configured.tools || []) {
      pi.registerTool({
        name: tool.name,
        label: tool.name,
        description: tool.description || "Configured MCP tool",
        parameters: tool.input_schema,
        async execute(_id: string, args: Record<string, unknown>, signal?: AbortSignal) {
          const response = await fetch(configured.endpoint, {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ name: tool.name, arguments: args }),
            signal,
          });
          if (!response.ok) throw new Error(`MCP bridge returned ${response.status}`);
          const result = await response.json();
          return { ...result, details: result };
        },
      });
    }
  }
  let isPentestMode = false;
  try {
    isPentestMode = knownReferences().isPentest;
  } catch {
    isPentestMode = false;
  }
  const ctfOpenStepParams = Type.Object({
    action: Type.String({
      description:
        "一句话点明方向，不写分步方法，不复述 origin 或 goal 已有的信息。",
    }),
    requires_capabilities: Type.Optional(Type.Array(Type.String({
      description: "执行本 Step 需要的已配置工具或能力标识。",
    }))),
    from: Type.Array(
      Type.Union([Type.String(), Type.Integer({ minimum: 1 })]),
      { description: "本步骤直接依据的 fact id。" },
    ),
    priority: Type.Optional(
      Type.Union([
        Type.Literal("high"),
        Type.Literal("normal"),
        Type.Literal("low"),
      ]),
    ),
  });
  const pentestOpenStepParams = Type.Object({
    action: Type.String({
      description:
        "一句话点明方向，不写分步方法，不复述 origin 或 goal 已有的信息。",
    }),
    requires_capabilities: Type.Optional(Type.Array(Type.String({
      description: "执行本 Step 需要的已配置工具或能力标识。",
    }))),
    from: Type.Array(
      Type.Union([Type.String(), Type.Integer({ minimum: 1 })]),
      { description: "本步骤直接依据的 fact id。" },
    ),
    expected_observable: Type.Optional(Type.String({
      description: "本 Step 会产生什么可观察结果，以区分成功、失败或仍未判明。",
    })),
    expectedObservable: Type.Optional(Type.String({
      description: "expected_observable 的驼峰别名。",
    })),
    stop_condition: Type.Optional(Type.String({
      description: "何时停止本 Step；有界且可检验的条件。",
    })),
    stopCondition: Type.Optional(Type.String({
      description: "stop_condition 的驼峰别名。",
    })),
    coverage_key: Type.Optional(Type.String({
      description: "本 Step 覆盖的稳定问题标识；同一范围和假设沿用原值。",
    })),
    coverageKey: Type.Optional(Type.String({
      description: "coverage_key 的驼峰别名。",
    })),
    asset: Type.Optional(Type.String({ description: "Pentest: 本 Step 的完整授权目标 URL。" })),
    identity: Type.Optional(Type.String({ description: "Pentest: 测试使用的身份或匿名。" })),
    risk_tier: Type.Optional(Type.String({ description: "Pentest: passive 或 bounded_validation。" })),
    evidence_requirement: Type.Optional(Type.String({ description: "Pentest: 需要保存的工具证据。" })),
    authorization_version: Type.Optional(Type.Integer({ minimum: 1 })),
    requires: Type.Optional(Type.Array(Type.String({
      description: "图中已登记的 PoC Resource ID；不需要时省略。",
    }))),
    priority: Type.Optional(
      Type.Union([
        Type.Literal("high"),
        Type.Literal("normal"),
        Type.Literal("low"),
      ]),
    ),
  });
  pi.registerTool({
    name: "open_step",
    label: "创建步骤",
    description: "从若干已有事实出发，开一条眼前要取的探索方向。",
    executionMode: "sequential" as const,
    parameters: isPentestMode ? pentestOpenStepParams : ctfOpenStepParams,
    async execute(
      _id: string,
      args: {
        action: string; from: Array<string | number>; priority?: Priority;
        requires_capabilities?: string[];
        expected_observable?: string; expectedObservable?: string;
        stop_condition?: string; stopCondition?: string;
        coverage_key?: string; coverageKey?: string; requires?: string[];
        asset?: string; identity?: string; risk_tier?: string;
        evidence_requirement?: string; authorization_version?: number;
      },
    ) {
      let refs;
      try {
        refs = knownReferences();
      } catch (error) {
        return contractError(`graph snapshot unavailable: ${String(error)}`);
      }
      const count = operations.filter((item) => item.op === "open_step" && referencesActive(item, refs.facts)).length;
      const droppedOpenCount = new Set(
        operations
          .filter(
            (item) =>
              item.op === "drop_step" &&
              refs.openSteps.has(String(item.intent_id || "")),
          )
          .map((item) => String(item.intent_id || "")),
      ).size;
      const effectiveMaxSteps = maxSteps >= 0 ? maxSteps + droppedOpenCount : maxSteps;
      if (effectiveMaxSteps >= 0 && count >= effectiveMaxSteps) {
        throw new Error(`本轮最多创建 ${effectiveMaxSteps} 个步骤`);
      }
      if (typeof args.action !== "string" || !args.action.trim()) {
        return contractError("action must be non-empty");
      }
      const rawExpected =
        typeof args.expected_observable === "string" && args.expected_observable.trim()
          ? args.expected_observable.trim()
          : typeof args.expectedObservable === "string" && args.expectedObservable.trim()
            ? args.expectedObservable.trim()
            : "";
      const rawStop =
        typeof args.stop_condition === "string" && args.stop_condition.trim()
          ? args.stop_condition.trim()
          : typeof args.stopCondition === "string" && args.stopCondition.trim()
            ? args.stopCondition.trim()
            : "";
      const rawCoverage =
        typeof args.coverage_key === "string" && args.coverage_key.trim()
          ? args.coverage_key.trim()
          : typeof args.coverageKey === "string" && args.coverageKey.trim()
            ? args.coverageKey.trim()
            : "";
      if (refs.isPentest && (!rawExpected || !rawStop || !rawCoverage)) {
        return contractError("expected_observable, stop_condition and stable coverage_key are required");
      }
      const expectedObservable = rawExpected;
      const stopCondition = rawStop;
      const coverageKey = rawCoverage;
      const knownSeqs = new Set(refs.facts.values());
      const unknownFact = args.from.find((value) =>
        typeof value === "number"
          ? !knownSeqs.has(value)
          : !(refs.facts.has(String(value)) || knownSeqs.has(Number(value))));
      if (unknownFact !== undefined) {
        return contractError(`unknown Fact reference: ${String(unknownFact)}`);
      }
      if (refs.isPentest) {
        const unknownResource = (args.requires || []).find((id) => !refs.resources.has(id));
        if (unknownResource !== undefined) {
          return contractError(`unknown Resource reference: ${unknownResource}`);
        }
      }
      const citedSeqs = args.from
        .map((val) => (typeof val === "number" ? val : refs.facts.get(String(val)) ?? Number(val)))
        .filter((seq) => Number.isFinite(seq) && seq > 0);
      const draftId = `draft-${++nextDraftId}`;
      add({
        op: "open_step",
        draft_id: draftId,
        action: args.action,
        from: citedSeqs,
        expected_observable: expectedObservable,
        stop_condition: stopCondition,
        coverage_key: coverageKey,
        asset: args.asset || "",
        identity: args.identity || "",
        risk_tier: args.risk_tier || "",
        evidence_requirement: args.evidence_requirement || "",
        authorization_version: args.authorization_version || 0,
        requires: args.requires || [],
        requires_capabilities: args.requires_capabilities || [],
        priority: args.priority || "normal",
      });
      return text(`已记入：open_step（${draftId}）`);
    },
  });

  pi.registerTool({
    name: "peek_step",
    label: "查看步骤或事实执行记录",
    description:
      "只读查看某个步骤（正在运行 open 或已结案 done 的 step_xxx）或某条事实（fact_xxx）底层真实执行过的命令与工具输出。在判断在途方向进展、决定是否 drop_step、或核查某条 Fact 背后的真实探测样本与输出时调用。",
    executionMode: "sequential" as const,
    parameters: Type.Object({
      intent_id: Type.String({
        description: "图中 Step 的 id（如 step_002，支持 open 或 done）或 Fact 的 id（如 fact_003）。",
      }),
    }),
    async execute(_id: string, args: { intent_id: string }) {
      const intentId = String(args.intent_id || "").trim();
      if (!intentId) return contractError("intent_id must be non-empty");
      peekedSteps.add(intentId);
      return text(runPeekStep(intentId));
    },
  });

  pi.registerTool({
    name: "read",
    label: "读取共享工作区文件",
    description:
      "只读查看当前题目工作区（特别是 shared/ 目录）下的测试脚本、响应样本或结果文件。",
    executionMode: "sequential" as const,
    parameters: Type.Object({
      path: Type.String({
        description: "工作区内的相对路径（如 shared/step002_tpl_tester.py）。",
      }),
      offset: Type.Optional(Type.Integer({ minimum: 1 })),
      limit: Type.Optional(Type.Integer({ minimum: 1, maximum: 300 })),
    }),
    async execute(_id: string, args: { path: string; offset?: number; limit?: number }) {
      const rawPath = String(args.path || "").trim();
      if (!rawPath) return contractError("path must be non-empty");
      const cwd = process.cwd();
      const resolved = resolve(cwd, rawPath);
      const rel = relative(cwd, resolved);
      if (rel.startsWith("..") || rel.includes(".muteki") || resolved.startsWith("/proc") || resolved.startsWith("/var/lib/muteki-hosted/control")) {
        return text(`拒绝读取工作区外或受保护路径: ${rawPath}`);
      }
      if (!existsSync(resolved)) {
        return text(`文件不存在: ${rawPath}`);
      }
      try {
        const st = statSync(resolved);
        if (!st.isFile()) return text(`不是普通文件: ${rawPath}`);
        const content = readFileSync(resolved, "utf8");
        const allLines = content.split(/\r?\n/);
        const startLine = Math.max(1, Number(args.offset || 1));
        const maxLines = Math.min(300, Math.max(1, Number(args.limit || 120)));
        const slice = allLines.slice(startLine - 1, startLine - 1 + maxLines);
        const numbered = slice
          .map((line, i) => `${startLine + i}: ${line.slice(0, 500)}`)
          .join("\n")
          .slice(0, 12000);
        return text(
          `[file: ${rel} | lines ${startLine}-${Math.min(allLines.length, startLine + slice.length - 1)} of ${allLines.length}]\n${numbered}`,
        );
      } catch (err) {
        return text(`读取失败: ${String(err)}`);
      }
    },
  });

  pi.registerTool({
    name: "drop_step",
    label: "删除步骤",
    description:
      "删除已经没有必要的开放步骤；也可取消本轮已记入的 open_step 草稿（传 draft-N）。若该步骤正有 Worker 在后台执行且本轮尚未 peek_step，首次调用会先返回其实时进展供确认，再次调用即确认删除。",
    executionMode: "sequential" as const,
    parameters: Type.Object({
      intent_id: Type.String({
        description: "图中开放 Step 的 intent id，或本轮 open_step 回执中的 draft-N。",
      }),
      why: Type.Optional(Type.String()),
    }),
    async execute(_id: string, args: { intent_id: string; why?: string }) {
      const rejected = rejectChallengerPlan();
      if (rejected) return rejected;
      if (committed) {
        throw new Error("draft is already committed");
      }
      const intentId = String(args.intent_id || "").trim();
      const draftIndex = operations.findIndex(
        (item) => item.op === "open_step" && item.draft_id === intentId,
      );
      if (draftIndex >= 0) {
        operations.splice(draftIndex, 1);
        persist();
        return text(`已取消草稿 ${intentId}`);
      }
      let refs;
      try {
        refs = knownReferences();
      } catch {
        refs = null;
      }
      if (
        refs &&
        refs.activeRunningSteps.has(intentId) &&
        !peekedSteps.has(intentId)
      ) {
        peekedSteps.add(intentId);
        const liveReport = runPeekStep(intentId);
        return text(
          `[先看后删保护] ${intentId} 当前正由 Worker 执行中，本次尚未删除。其实时上下文与进展如下：\n\n` +
            `${liveReport}\n\n` +
            `请根据上述实时工作情况判断：若该 Worker 正在推进有效方向（或你只是想细化措辞），请保留它不要 drop；若看完后确认其方向已被新 Fact 解决或前提已被证伪，再次调用 drop_step(intent_id="${intentId}") 即可确认删除。`,
        );
      }
      return add({ op: "drop_step", ...args });
    },
  });

  pi.registerTool({
    name: "change_step_priority",
    label: "调整优先级",
    description: "调整一个开放步骤的优先级。",
    executionMode: "sequential" as const,
    parameters: Type.Object({
      intent_id: Type.String(),
      priority: Type.Union([
        Type.Literal("high"),
        Type.Literal("normal"),
        Type.Literal("low"),
      ]),
    }),
    async execute(_id: string, args: { intent_id: string; priority: Priority }) {
      const rejected = rejectChallengerPlan();
      if (rejected) return rejected;
      return add({ op: "change_step_priority", ...args });
    },
  });

  pi.registerTool({
    name: "add_sub_goal",
    label: "添加中间目标",
    description: "添加当前探索所需的中间目标。返回稳定 Goal ID，可用于 satisfy_goal；不会替代最终目标。",
    executionMode: "sequential" as const,
    parameters: Type.Object({ criterion: Type.String() }),
    async execute(_id: string, args: { criterion: string }) {
      const rejected = rejectChallengerPlan();
      if (rejected) return rejected;
      const criterion = String(args.criterion || "").trim();
      if (!criterion) return contractError("criterion must be non-empty");
      let refs;
      try {
        refs = knownReferences();
      } catch (error) {
        return contractError(`graph snapshot unavailable: ${String(error)}`);
      }
      let number = refs.reservedGoals.size - (refs.reservedGoals.has("goal_final") ? 1 : 0) + 1;
      while (refs.reservedGoals.has(`goal_${String(number).padStart(3, "0")}`)) number++;
      const goalId = `goal_${String(number).padStart(3, "0")}`;
      add({ op: "add_sub_goal", goal_id: goalId, criterion });
      return text(`已记入：add_sub_goal（${goalId}）`);
    },
  });

  pi.registerTool({
    name: "satisfy_goal",
    label: "确认完成",
    description: "图中的事实或平台结果已经满足 Goal 时标记完成。默认 goal_final；中间目标完成不会结束任务。",
    executionMode: "sequential" as const,
    parameters: Type.Object({
      goal_id: Type.Optional(Type.String()),
      reason: Type.String(),
      from: Type.Optional(
        Type.Array(Type.Union([Type.String(), Type.Integer({ minimum: 1 })])),
      ),
    }),
    async execute(
      _id: string,
      args: { goal_id?: string; reason: string; from?: Array<string | number> },
    ) {
      let refs;
      try {
        refs = knownReferences();
      } catch (error) {
        return contractError(`graph snapshot unavailable: ${String(error)}`);
      }
      const goalId = String(args.goal_id || "goal_final").trim();
      if (!refs.goals.has(goalId)) return contractError(`unknown Goal reference: ${goalId}`);
      if (goalId !== "goal_final" && !args.from?.length) return contractError("sub-goal satisfaction requires active Fact evidence");
      if (!String(args.reason || "").trim()) return contractError("reason must be non-empty");
      const fromSeqs = (args.from || [])
        .map((val) => (typeof val === "number" ? val : refs.facts.get(String(val)) ?? Number(val)))
        .filter((seq) => Number.isFinite(seq) && seq > 0);
      if (args.from) {
        const knownSeqs = new Set(refs.facts.values());
        const unknownFact = args.from.find((value) =>
          typeof value === "number"
            ? !knownSeqs.has(value)
            : !(refs.facts.has(String(value)) || knownSeqs.has(Number(value))));
        if (unknownFact !== undefined) {
          return contractError(`unknown Fact reference: ${String(unknownFact)}`);
        }
      }
      return add({
        op: "satisfy_goal",
        goal_id: goalId,
        reason: args.reason.trim(),
        from: fromSeqs,
      });
    },
  });

  pi.registerTool({
    name: "supersede_fact",
    label: "淘汰错误或过时事实",
    description:
      "当图中某条已有 Fact 被后续实测证伪、属于假阳性/诱饵，或已被更准确的新 Fact 取代时，将其标为 superseded 并移出活跃事实（不可用于 fact_origin）。",
    executionMode: "sequential" as const,
    parameters: Type.Object({
      fact_id: Type.String({
        description: "被淘汰的旧 Fact ID（如 fact_003，不可为 fact_origin）。",
      }),
      reason: Type.String({
        description: "淘汰该 Fact 的客观依据。",
      }),
      by_fact_id: Type.Optional(Type.String({
        description: "可选：替代它的新 Fact ID（如 fact_008）。",
      })),
    }),
    async execute(
      _id: string,
      args: { fact_id: string; reason: string; by_fact_id?: string },
    ) {
      let refs;
      try {
        refs = knownReferences();
      } catch (error) {
        return contractError(`graph snapshot unavailable: ${String(error)}`);
      }
      const targetId = String(args.fact_id || "").trim();
      if (!targetId || targetId === "fact_origin" || !refs.facts.has(targetId)) {
        return contractError(`unknown or non-revisable Fact reference: ${targetId}`);
      }
      const byId = String(args.by_fact_id || "").trim();
      if (byId && (byId === "fact_origin" || byId === targetId || !refs.facts.has(byId))) {
        return contractError(`invalid replacement Fact reference: ${byId}`);
      }
      if (!String(args.reason || "").trim()) {
        return contractError("reason must be non-empty");
      }
      return add({
        op: "supersede_fact",
        fact_id: targetId,
        fact_seq: refs.facts.get(targetId),
        reason: args.reason.trim(),
        by_fact_id: byId || "",
        by_fact_seq: byId ? refs.facts.get(byId) : null,
      });
    },
  });

  pi.registerTool({
    name: "preview",
    label: "预演投影",
    description:
      "复核叠加当前草稿后的图投影：只出 step/goal 结构与事实标题。校验不过的条目会标注；提交时仍会剔除。不写图、不提交。对照的是当时快照，commit 以当时图重验为准。简单批次不必调用。",
    executionMode: "sequential" as const,
    parameters: Type.Object({}),
    async execute() {
      persist();
      return text(renderDecidePreview());
    },
  });

  pi.registerTool({
    name: "commit",
    label: "提交规划",
    description:
      "提交本轮决定。操作简单、合法性一眼可辨时直接提交；含 satisfy_goal 或跨多条事实、可能被剔除的操作时先 preview。",
    executionMode: "sequential" as const,
    parameters: Type.Object({
      summary: Type.Optional(Type.String()),
      yield_visit: Type.Optional(Type.Boolean({
        description: "仅 CTF：没有剩余可执行 Step 或新假设时，说明依据并让出本次访问；不表示目标完成。",
      })),
    }),
    async execute(_id: string, args: { summary?: string; yield_visit?: boolean }) {
      if (committed) {
        return text("本轮已经提交");
      }
      let graph;
      try {
        graph = JSON.parse(readFileSync(graphPath, "utf8"));
      } catch (error) {
        return contractError(`graph snapshot unavailable: ${String(error)}`);
      }
      const dropped = new Set(operations.filter((item) => item.op === "drop_step")
        .map((item) => String(item.intent_id || "")));
      const refs = knownReferences();
      const existing = (graph.open_steps || []).filter(
        (item: { id?: string; from?: unknown }) => !dropped.has(String(item.id || ""))
          && referencesActive(item, refs.facts),
      );
      const proposed = operations.filter((item) => item.op === "open_step" && referencesActive(item, refs.facts));
      const goalSatisfied = operations.some((item) => item.op === "satisfy_goal"
        && (!item.goal_id || item.goal_id === "goal_final") && referencesActive(item, refs.facts));
      const yielding = args.yield_visit === true;
      if (yielding && (
        graph.schema !== "muteki.ctf-decide-graph.v1"
        || goalSatisfied || existing.length > 0 || proposed.length > 0
        || !args.summary?.trim()
      )) {
        return contractError("yield_visit requires CTF, a summary, and no remaining Step or completed Goal");
      }
      if (refs.isPentest && !challengerAudit && !goalSatisfied && existing.length === 0 && proposed.length === 0 && !yielding) {
        return contractError(
          "No Step remains; open an executable Step or satisfy goal_final with verified evidence",
        );
      }
      operations.push({ op: "commit", summary: args.summary || "", yield_visit: yielding });
      committed = true;
      persist();
      return text(`已提交 ${operations.length - 1} 项操作`);
    },
  });
}
