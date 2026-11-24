import type { DeckState } from "./events";
import { currentGenWorkerIds } from "./run-events/selectors";
import type { WorkerSettings } from "./useRun";

export type DispatchRosterRow = {
  key: string; engine: string; label: string; model?: string;
  state: "configured" | "online" | "finished" | "offline" | "degraded";
  detail?: string;
};
export function isDispatchDraft(runId: string): boolean {
  return !runId || runId.startsWith("draft-");
}
export function dispatchRoster(deck: DeckState, settings: WorkerSettings | null): DispatchRosterRow[] {
  if (isDispatchDraft(deck.runId)) {
    if (!settings) return [];
    const selected = settings.overrides?.[deck.category]?.engines ?? settings.engines;
    const allProfiles = settings.seats?.length
      ? settings.seats.map(seat => ({ ...seat, name: seat.label }))
      : settings.worker_profiles || [];
    const profiles = allProfiles.filter(profile => profile.enabled !== false);
    if (allProfiles.length) {
      const rows = new Map<string, DispatchRosterRow>();
      for (const ref of selected || profiles.map(profile => profile.id)) {
        const exact = profiles.find(profile => profile.id === ref || profile.name === ref);
        for (const profile of exact ? [exact] : profiles.filter(profile => profile.engine === ref)) {
          rows.set(profile.id, { key: profile.id, engine: profile.engine,
            label: ("label" in profile ? String(profile.label || "") : "") || profile.name || profile.id,
            model: profile.model, state: "configured" });
        }
      }
      return [...rows.values()];
    }
    return [...new Set(selected || [])].map(engine => ({ key: engine, engine, label: engine, state: "configured" }));
  }
  const rows: DispatchRosterRow[] = currentGenWorkerIds(deck).map(id => {
    const lane = deck.lanes[id];
    const reason = deck.degradedEngines[lane?.profileId || ""] || deck.degradedEngines[lane?.engine || ""];
    return { key: id, engine: lane?.engine || id, label: lane?.profileLabel || id, model: lane?.model,
      state: reason ? "degraded" : deck.finished || lane?.finishedAt || lane?.solved ? "finished" : lane?.online ? "online" : "offline",
      detail: reason || lane?.statusReason };
  });
  for (const [key, detail] of Object.entries(deck.degradedEngines)) {
    if (rows.some(row => row.key === key || row.engine === key || deck.lanes[row.key]?.profileId === key)) continue;
    rows.push({ key, engine: key, label: key, state: "degraded", detail });
  }
  return rows;
}
