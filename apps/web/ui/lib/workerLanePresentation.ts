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

function stripToolPrefix(s: string): string {
  return s.replace(/^tool:\s*/i, "").replace(/^[▶↳]\s*/, "").trim();
}

function compactLaneStatusToken(lane: WorkerLanePresentationInput, online: boolean): LaneStatusToken {
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

/** HeroUI chip color for a lane status — the roster's only status signal. */
export type LaneStatusTone = "success" | "warning" | "danger" | "accent" | "default";

export function laneStatusTone(kind: LaneStatusKind): LaneStatusTone {
  switch (kind) {
    case "solved":
      return "success";
    case "stalled":
    case "error":
      return "danger";
    case "paused":
    case "thinking":
      return "warning";
    case "running-tool":
      return "accent";
    case "waiting":
    case "online":
    case "offline":
      return "default";
    default: {
      const _exhaustive: never = kind;
      return _exhaustive;
    }
  }
}

const ERROR_REASONS = new Set(["timeout", "oom", "error", "budget"]);

export function laneStatusKind(lane: WorkerLanePresentationInput, online: boolean): LaneStatusKind {
  if (lane.solved) return "solved";
  const raw = (lane.statusReason || lane.status || "").trim();
  if (!online) {
    // A worker that exited on a solve reads 已解出; keep the tone in step with
    // that label instead of falling through to the neutral offline grey.
    if (raw === "solved") return "solved";
    return ERROR_REASONS.has(raw) ? "error" : "offline";
  }
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

/**
 * Secondary activity text for a roster row. The status chip is the single
 * status signal, so a reason it already renders (`finished` next to 已退出) is
 * dropped here and the worker's last real tool line is shown instead.
 */
export function laneActivityDetail(
  lane: WorkerLanePresentationInput,
  online: boolean,
  tools: string[],
): string {
  const status = (lane.status || "").trim();
  if (/^tool:\s*/i.test(status)) return stripToolPrefix(status);
  const lastTool = stripToolPrefix(tools[tools.length - 1] || "");
  const reason = stripToolPrefix((lane.statusReason || "").trim());
  if (!reason) return lastTool;
  const token = compactLaneStatusToken(lane, online);
  const restated = token.kind === "raw"
    ? token.label.toLowerCase() === reason.toLowerCase()
    : TERMINAL_REASON_I18N[reason] === token.key;
  return restated ? lastTool : reason;
}
