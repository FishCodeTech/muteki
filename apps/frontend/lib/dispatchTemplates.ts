import type { DispatchTemplate, WorkerSettings } from "./useRun";

export type RosterSeat = NonNullable<WorkerSettings["seats"]>[number];

export const DISPATCH_TEMPLATE_STORAGE = "muteki.dispatchTemplate";

const ORDINARY_ROLES = ["bootstrap", "explore", "respond", "solve", "scout"];

function ordinary(roles: string[] | undefined): boolean {
  return !roles?.length || roles.some((role) => ORDINARY_ROLES.includes(role));
}

export function rosterSlotEngines(seats: Pick<RosterSeat, "engine" | "capacity" | "enabled">[]): string[] {
  return seats
    .filter((seat) => seat.enabled !== false)
    .flatMap((seat) => Array.from({ length: Math.max(1, seat.capacity?.max_running || 1) }, () => seat.engine));
}

export function defaultRosterSeats(config: WorkerSettings): Pick<RosterSeat, "id" | "engine" | "model" | "capacity" | "enabled">[] {
  const selected = config.engines?.length ? new Set(config.engines) : null;
  const seats = config.seats?.length
    ? config.seats.filter((seat) => ordinary(seat.roles))
    : (config.worker_profiles || []).filter((profile) => ordinary(profile.roles)).map((profile) => ({
      id: profile.id,
      engine: profile.engine,
      model: profile.model,
      capacity: { max_running: profile.max_running, max_review_running: profile.max_review_running ?? 0 },
      enabled: profile.enabled,
    }));
  return seats.filter((seat) => seat.enabled !== false && (!selected || selected.has(seat.id) || selected.has(seat.engine)));
}

export function rosterSummary(
  seats: Pick<RosterSeat, "engine" | "model" | "capacity" | "enabled">[],
  engineName: (engine: string) => string,
): string {
  const groups = new Map<string, number>();
  for (const seat of seats) {
    if (seat.enabled === false) continue;
    const key = `${engineName(seat.engine)}${seat.model ? ` · ${seat.model}` : ""}`;
    groups.set(key, (groups.get(key) || 0) + Math.max(1, seat.capacity?.max_running || 1));
  }
  return [...groups.entries()].map(([key, count]) => (count > 1 ? `${key} ×${count}` : key)).join("、");
}

export function findDispatchTemplate(config: WorkerSettings | null, id: string): DispatchTemplate | undefined {
  return id ? config?.dispatch_templates?.find((template) => template.id === id) : undefined;
}
