import {
  hitlDeliveryState,
  type ChatMessage,
  type DeckState,
  type SolverLane,
  type TaskContractView,
  type WorkerConnectionKind,
  type WorkerLaneRole,
} from "./types";

export function taskContractView(challenge: any): TaskContractView | undefined {
  if (!challenge || typeof challenge !== "object") return undefined;
  const raw = challenge.task_contract;
  if (raw && typeof raw === "object") {
    const completion = raw.completion_contract || raw.completion || {};
    return {
      rawInstruction: String(raw.raw_instruction || challenge.description || ""),
      mode: raw.mode === "pentest" ? "pentest" : "ctf",
      title: String(raw.title || challenge.name || ""),
      category: String(raw.category || challenge.category || "misc"),
      attachments: Array.isArray(raw.attachments) ? raw.attachments.map((item: any) => {
        const path = typeof item === "string" ? item : String(item?.path || "");
        const name = typeof item === "string" ? path.split("/").pop() || path : String(item?.name || path.split("/").pop() || "");
        return {
          path,
          name,
          size: Number(item?.size || 0),
          summary: typeof item === "string" ? name : String(item?.summary || name),
        };
      }) : [],
      executionTarget: String(raw.execution_target || raw.pentest?.target || challenge.target || "") || undefined,
      authorizationScope: String(raw.authorization_scope || raw.pentest?.authorization?.scope?.join?.(", ") || challenge.scope || ""),
      completion: {
        kind: completion.kind === "flag" ? "ctf_flag"
          : completion.kind === "goal" && raw.mode === "pentest" ? "outcome"
          : ["ctf_flag", "outcome", "count", "coverage"].includes(String(completion.kind))
            ? completion.kind : "ctf_flag",
        goal: String(completion.goal || ""),
        taskType: String(completion.task_type || ""),
        quantity: typeof completion.quantity === "number" ? completion.quantity : undefined,
        flagFormat: String(completion.flag_format || ""),
        flagFormatHint: String(completion.flag_format_hint || ""),
        expectedFlags: Number(completion.expected_flags || 1),
        multiFlag: completion.multi_flag === true,
        findingClass: String(completion.finding_class || "generic"),
        outcomePredicate: String(completion.outcome_predicate || "first_valid_report"),
        collectUntilCoverage: completion.collect_until_coverage === true,
      },
    };
  }
  const mode = challenge.mode === "pentest" ? "pentest" : "ctf";
  const engagement = challenge.engagement || {};
  const kind = mode === "ctf" ? "ctf_flag"
    : ["outcome", "count", "coverage"].includes(String(engagement.completion_kind))
      ? engagement.completion_kind
      : engagement.quantity === "recon" || engagement.collect_until_coverage ? "coverage"
        : engagement.quantity === "collect" ? "count" : "outcome";
  return {
    rawInstruction: String(challenge.description || challenge.goal || ""),
    mode,
    title: String(challenge.name || ""),
    category: String(challenge.category || "misc"),
    attachments: Array.isArray(challenge.attachments) ? challenge.attachments.map((path: unknown) => ({
      path: String(path), name: String(path).split("/").pop() || String(path), size: 0,
      summary: String(path).split("/").pop() || String(path),
    })) : [],
    executionTarget: String(challenge.target || "") || undefined,
    authorizationScope: String(challenge.scope || ""),
    completion: {
      kind,
      goal: String(challenge.goal || ""),
      taskType: String(engagement.finding_class || challenge.category || ""),
      quantity: mode === "ctf"
        ? Number(challenge.expected_flags || 1)
        : Number(engagement.expected_findings || 1),
      flagFormat: String(challenge.flag_format || ""),
      flagFormatHint: String(challenge.flag_format_hint || challenge.flag_format_wrapper || ""),
      expectedFlags: Number(challenge.expected_flags || 1),
      multiFlag: challenge.multi_flag === true,
      findingClass: String(engagement.finding_class || "generic"),
      outcomePredicate: String(engagement.outcome_predicate || "first_valid_report"),
      collectUntilCoverage: engagement.collect_until_coverage === true,
    },
  };
}

export function readPlatformConfirmationRequired(challenge: unknown): boolean {
  if (!challenge || typeof challenge !== "object") return false;
  const row = challenge as Record<string, unknown>;
  if (row.platform_confirmation_required === true) return true;
  return false;
}

export function mergeFlags(s: DeckState, flags?: string | string[] | null): void {
  const list = typeof flags === "string" ? [flags] : (flags ?? []);
  const invalidated = new Set(s.invalidatedFlags);
  const added = list.filter((f, index) => !invalidated.has(f) && !s.flags.includes(f) && list.indexOf(f) === index);
  if (added.length) s.flags = [...s.flags, ...added];
  if (s.flags.length && s.flag === undefined) s.flag = s.flags[0];
}

export function isInvalidatedFlag(s: DeckState, flag?: string | null): boolean {
  return flag != null && s.invalidatedFlags.includes(flag);
}

export function invalidateFlag(s: DeckState, flag?: string | null): void {
  const bad = flag ?? null;
  if (bad !== null) {
    if (!s.invalidatedFlags.includes(bad)) s.invalidatedFlags = [...s.invalidatedFlags, bad];
    s.flags = s.flags.filter((f) => f !== bad);
  } else {
    const added = s.flags.filter((f) => !s.invalidatedFlags.includes(f));
    if (added.length) s.invalidatedFlags = [...s.invalidatedFlags, ...added];
    s.flags = [];
  }
  s.flag = s.flags[0];
  if (!s.flags.length) s.solved = false;
  for (const id of Object.keys(s.lanes)) {
    const l = s.lanes[id];
    if (bad === null || l.flag === bad || (l.solved && !s.flags.length)) {
      s.lanes[id] = {
        ...l,
        solved: false,
        flag: l.flag === bad ? undefined : l.flag,
        status: l.online ? l.status : "done",
        statusReason: l.statusReason === "solved" ? undefined : l.statusReason,
      };
    }
  }
}

export function lane(state: DeckState, sid?: string | null): SolverLane {
  const id = sid || "solver";
  if (!state.lanes[id]) {
    state.lanes[id] = {
      solverId: id,
      reasoning: "",
      toolLines: [],
      status: "thinking",
      solved: false,
      online: true,
    };
  }
  return state.lanes[id];
}

export function reviewRoleFromPhase(phase?: string | null): WorkerLaneRole | undefined {
  const token = (phase || "").toLowerCase();
  if (token.includes("review")) return "review";
  if (token.includes("verifier")) return "verifier";
  return undefined;
}

export function roleFromWorkerRole(workerRole?: unknown): WorkerLaneRole | undefined {
  const raw = (workerRole ?? "").toString().toLowerCase();
  if (!raw) return undefined;
  if (raw === "review") return "review";
  if (raw === "verifier") return "verifier";
  return "worker";
}

export function asWorkerConnection(value: unknown): WorkerConnectionKind | undefined {
  const raw = String(value || "");
  if (raw === "official" || raw === "custom_endpoint" || raw === "system") return raw;
  return undefined;
}

export function identityFromPayload(p: Record<string, any>): Partial<SolverLane> {
  return {
    profileId: String(p.profile_id || "").trim() || undefined,
    profileLabel: String(p.profile_label || "").trim() || undefined,
    model: String(p.model || "").trim() || undefined,
    accountId: String(p.account_id || "").trim() || undefined,
    endpointHost: String(p.endpoint_host || "").trim() || undefined,
    connection: asWorkerConnection(p.connection),
    provider: String(p.provider || "").trim() || undefined,
  };
}

export function withLaneIdentity(l: SolverLane, p: Record<string, any>): SolverLane {
  const patch = identityFromPayload(p);
  return {
    ...l,
    profileId: patch.profileId || l.profileId,
    profileLabel: patch.profileLabel || l.profileLabel,
    model: patch.model || l.model,
    accountId: patch.accountId || l.accountId,
    endpointHost: patch.endpointHost || l.endpointHost,
    connection: patch.connection || l.connection,
    provider: patch.provider || l.provider,
  };
}

export function activityOnline(state: DeckState): boolean {
  return !state.finished;
}

let _gid = 0;
export function gid(prefix: string): string {
  _gid += 1;
  return `${prefix}_${_gid}`;
}

export function seedGid(existingIds: Iterable<string>): void {
  for (const id of existingIds) {
    const match = /_(\d+)$/.exec(String(id));
    if (match) _gid = Math.max(_gid, Number(match[1]));
  }
}

const CHAT_CAP = 4000;
export function pushChat(s: DeckState, msg: Omit<ChatMessage, "id">): void {
  s.chat = [...s.chat, { ...msg, id: gid("c") }].slice(-CHAT_CAP);
}

export function upsertStatusChat(
  s: DeckState,
  statusKey: string,
  msg: Omit<ChatMessage, "id" | "statusKey">,
): void {
  const index = s.chat.findIndex((item) => item.statusKey === statusKey);
  if (index < 0) {
    pushChat(s, { ...msg, statusKey });
    return;
  }
  const next = s.chat.slice();
  next[index] = { ...next[index], ...msg, statusKey };
  s.chat = next;
}

export const FLAG_SUBMISSION_REJECTION_I18N: Record<string, string> = {
  empty_candidate: "sys.flagSubmissionRejected.emptyCandidate",
  invalidated_rejected: "sys.flagSubmissionRejected.invalidated",
  operator_context_rejected: "sys.flagSubmissionRejected.operatorContext",
  provenance_rejected: "sys.flagSubmissionRejected.provenance",
  provenance_launder_rejected: "sys.flagSubmissionRejected.launder",
  format_invalid: "sys.flagSubmissionRejected.formatInvalid",
  format_rejected: "sys.flagSubmissionRejected.formatRejected",
  token_strength_rejected: "sys.flagSubmissionRejected.tokenStrength",
  placeholder_rejected: "sys.flagSubmissionRejected.placeholder",
  origin_rejected: "sys.flagSubmissionRejected.origin",
  authority_rejected: "sys.flagSubmissionRejected.authority",
  validation_error: "sys.flagSubmissionRejected.validationError",
  decision_persistence_error: "sys.flagSubmissionRejected.persistence",
  submission_conflict: "sys.flagSubmissionRejected.conflict",
  submission_missing: "sys.flagSubmissionRejected.missing",
  invalid_request: "sys.flagSubmissionRejected.invalidRequest",
};

const ENGINE_TAG = /\[(?:claude|codex|cursor|pi|omp|kimi|grok|opencode|dsh|oh-my-pi|ohmypi)\]\s+/gi;

export function stripEnginePrefix(text: string): string {
  if (!text) return text;
  return text.replace(ENGINE_TAG, "");
}

export function glueReasoningDelta(prev: string, incoming: string): string {
  const next = stripEnginePrefix(incoming);
  if (!prev) return next.replace(/\n$/, "");
  if (next.startsWith("\n") || next.startsWith(" ")) return (prev + next).slice(-6000);
  const token = next.endsWith("\n") && next.length <= 24 && !next.slice(0, -1).includes("\n")
    ? next.slice(0, -1)
    : next;
  if (prev.endsWith("\n") && token.length <= 24 && !token.includes("\n")) {
    return (prev.slice(0, -1) + token).slice(-6000);
  }
  return (prev + token).slice(-6000);
}

export function toolOutputLooksFailed(text: string): boolean {
  const s = text.trim();
  if (!s) return false;
  if (/\bexit(?:ed)?(?:\s+with)?(?:\s*code)?\s*[:=]?\s*(?!0\b)(?:[1-9]\d*)\b/i.test(s)) return true;
  if (/\b(?:command not found|permission denied|no such file or directory)\b/i.test(s)) return true;
  return false;
}

export function lastOwnAgentIndex(chat: ChatMessage[], solverId: string): number {
  for (let i = chat.length - 1; i >= 0; i--) {
    if (chat[i].role === "agent" && chat[i].solverId === solverId) return i;
  }
  return -1;
}

export function userPromptBubble(ch: Record<string, any> | undefined): string {
  if (!ch) return "";
  const lines: string[] = [];
  const desc = (ch.description || "").trim();
  if (desc) lines.push(desc);
  if (ch.mode === "pentest") return desc;
  const ctx: string[] = [];
  if (ch.target && !desc.includes(String(ch.target))) ctx.push(`target: ${ch.target}`);
  const atts: unknown = ch.attachments;
  if (Array.isArray(atts) && atts.length) {
    const names = atts.map((p) => String(p).split("/").pop()).filter(Boolean) as string[];
    if (!names.every((name) => desc.includes(name))) ctx.push(`attachments: ${names.join(", ")}`);
  }
  if (ctx.length) lines.push(ctx.join("\n"));
  return lines.join("\n\n").trim();
}

export function applyObservedControlState(s: DeckState, p: Record<string, any>): void {
  if (p.status !== "effect_observed") return;
  const effect = (p.effect && typeof p.effect === "object") ? p.effect : {};
  const effectKind = String(effect.kind ?? p.effect_kind ?? "").toLowerCase();
  const runtimeControl = effectKind === "runtime_frozen" || effectKind === "runtime_thawed";
  const runWide = effect.run_wide === true || /^(global|run:|challenge:)/.test(String(p.target ?? ""));
  const held = effectKind === "run_quiesced" || effectKind === "run_frozen" || (effectKind === "runtime_frozen" && runWide);
  const released = effectKind === "run_resumed" || effectKind === "run_thawed" || (effectKind === "runtime_thawed" && runWide);
  if (effectKind === "workers_frozen" || effectKind === "workers_thawed" || runtimeControl) {
    const frozen = effectKind === "workers_frozen" || effectKind === "runtime_frozen";
    const targetIds = Array.isArray(p.target_ids)
      ? p.target_ids.map((id: unknown) => String(id)) : Array.isArray(effect.targets) ? effect.targets.map(String) : [];
    for (const id of targetIds) {
      const current = lane(s, id);
      s.lanes[id] = { ...current, paused: frozen };
    }
  }
  if (held) {
    s.awaitingOperator = String(effect.reason ?? p.detail
      ?? (effectKind === "run_frozen" || effectKind === "runtime_frozen"
        ? "operator froze the swarm" : "operator paused the swarm"));
  } else if (released) {
    const pending = s.hitlRequests.find((r) => (r.pausesBehavior ?? true) && hitlDeliveryState(r).phase === "open");
    s.awaitingOperator = pending?.prompt;
  }
}
