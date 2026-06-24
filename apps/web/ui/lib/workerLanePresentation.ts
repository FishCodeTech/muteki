export type WorkerLanePresentationInput = {
  solved?: boolean;
  status?: string;
  statusReason?: string;
  paused?: boolean;
};

export type LaneStatusToken =
  | { kind: "i18n"; key: string }
  | { kind: "raw"; label: string };

export function stripToolPrefix(s: string): string {
  return s.replace(/^tool:\s*/i, "").replace(/^[▶↳]\s*/, "").trim();
}

export function compactLaneStatusToken(lane: WorkerLanePresentationInput, online: boolean): LaneStatusToken {
  if (lane.solved) return { kind: "i18n", key: "worker.solved" };
  if (!online) return { kind: "i18n", key: "workerDock.offline" };
  // I: surface paused/stalled lifecycle states distinctly from plain online/busy.
  if (lane.paused) return { kind: "i18n", key: "worker.paused" };
  const raw = (lane.statusReason || lane.status || "").trim();
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

export function latestLaneActivity(status: string | undefined, statusReason: string | undefined, tools: string[]): string {
  const source = /^tool:\s*/i.test(status || "") ? status : (statusReason || tools[tools.length - 1] || "");
  return stripToolPrefix(source || "");
}
