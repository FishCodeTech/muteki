export type WorkerLanePresentationInput = {
  solved?: boolean;
  status?: string;
  statusReason?: string;
  paused?: boolean;
};

export type LaneStatusToken =
  | { kind: "i18n"; key: string }
  | { kind: "raw"; label: string };

const TERMINAL_REASON_I18N: Record<string, string> = {
  solved: "worker.solved",
  timeout: "worker.timeout",
  oom: "worker.oom",
  cancelled: "worker.cancelled",
  steered: "worker.steered",
  error: "worker.error",
  budget: "worker.budget",
  killed: "worker.killed",
  stuck: "worker.stalled",
  finished: "worker.exited",
  done: "worker.exited",
  exited: "worker.exited",
};

export function stripToolPrefix(s: string): string {
  return s.replace(/^tool:\s*/i, "").replace(/^[▶↳]\s*/, "").trim();
}

export function compactLaneStatusToken(lane: WorkerLanePresentationInput, online: boolean): LaneStatusToken {
  if (lane.solved) return { kind: "i18n", key: "worker.solved" };
  const raw = (lane.statusReason || lane.status || "").trim();
  if (!online) {
    const terminalKey = TERMINAL_REASON_I18N[raw];
    if (terminalKey) return { kind: "i18n", key: terminalKey };
    return { kind: "i18n", key: "workerDock.offline" };
  }
  // I: surface paused/stalled lifecycle states distinctly from plain online/busy.
  if (lane.paused) return { kind: "i18n", key: "worker.paused" };
  if (raw === "stalled") return { kind: "i18n", key: "worker.stalled" };
  if (/^tool:\s*/i.test(raw)) return { kind: "i18n", key: "wlane.runningTool" };
  if (!raw || raw === "waiting") return { kind: "i18n", key: "wlane.waiting" };
  if (raw === "done" || raw === "finished") return { kind: "i18n", key: "workerDock.online" };
  return raw.length > 22 ? { kind: "i18n", key: "workerDock.online" } : { kind: "raw", label: raw };
}

export function compactLaneStatus(
  lane: WorkerLanePresentationInput,
  online: boolean,
  t: (key: string) => string,
): string {
  const token = compactLaneStatusToken(lane, online);
  return token.kind === "i18n" ? t(token.key) : token.label;
}

export type LaneStatusKind =
  | "solved"
  | "paused"
  | "stalled"
  | "running-tool"
  | "waiting"
  | "thinking"
  | "online"
  | "offline"
  | "error";

const ERROR_REASONS = new Set(["timeout", "oom", "error", "budget"]);

export function laneStatusKind(lane: WorkerLanePresentationInput, online: boolean): LaneStatusKind {
  if (lane.solved) return "solved";
  const raw = (lane.statusReason || lane.status || "").trim();
  if (!online) return ERROR_REASONS.has(raw) ? "error" : "offline";
  if (lane.paused) return "paused";
  if (raw === "stalled" || raw === "stuck") return "stalled";
  if (/^tool:\s*/i.test(raw)) return "running-tool";
  if (!raw || raw === "waiting") return "waiting";
  if (raw === "done" || raw === "finished") return "online";
  return "thinking";
}

export type RosterGroup = "live" | "issue" | "done";

export function rosterGroup(lane: WorkerLanePresentationInput, online: boolean): RosterGroup {
  const kind = laneStatusKind(lane, online);
  if (kind === "paused" || kind === "stalled" || kind === "error") return "issue";
  if (kind === "offline" || kind === "solved") return "done";
  return "live";
}

export function isAnomalyLane(lane: WorkerLanePresentationInput, online: boolean): boolean {
  return rosterGroup(lane, online) === "issue";
}

export function latestLaneActivity(status: string | undefined, statusReason: string | undefined, tools: string[]): string {
  const source = /^tool:\s*/i.test(status || "") ? status : (statusReason || tools[tools.length - 1] || "");
  return stripToolPrefix(source || "");
}
