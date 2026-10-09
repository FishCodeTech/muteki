import type { SidebarPreferences } from "./useConversation";

type PreferenceValues = Omit<SidebarPreferences, "version">;

function sameList(left: string[], right: string[]): boolean {
  return left.length === right.length && left.every((value, index) => value === right[index]);
}

type TimeMap = Record<string, number>;

function sameTimeMap(left: TimeMap, right: TimeMap): boolean {
  const keys = Object.keys(left);
  return keys.length === Object.keys(right).length && keys.every((key) => left[key] === right[key]);
}

export function sameSidebarPreferences(
  left: PreferenceValues,
  right: PreferenceValues,
): boolean {
  return sameList(left.pinned_ids, right.pinned_ids)
    && sameList(left.thread_order, right.thread_order)
    && sameList(left.project_order, right.project_order)
    && left.sort_mode === right.sort_mode
    && left.pinned_sort_mode === right.pinned_sort_mode
    && left.group_mode === right.group_mode
    && sameTimeMap(left.settled_at, right.settled_at)
    && sameTimeMap(left.snoozed_until, right.snoozed_until);
}

/** Apply per-key additions, changes and removals made locally since `base`. */
export function rebaseTimeMap(base: TimeMap, local: TimeMap, remote: TimeMap): TimeMap {
  const result: TimeMap = { ...remote };
  for (const key of new Set([...Object.keys(base), ...Object.keys(local)])) {
    if (base[key] === local[key]) continue;
    if (local[key] === undefined) delete result[key];
    else result[key] = local[key];
  }
  return result;
}

/** Merge explicit membership changes; a reorder cannot resurrect a remotely removed ID. */
export function rebaseSidebarList(base: string[], local: string[], remote: string[]): string[] {
  if (sameList(base, local)) return [...remote];
  const old = new Set(base);
  const current = new Set(local);
  const removed = new Set(base.filter((id) => !current.has(id)));
  const added = local.filter((id) => !old.has(id));
  const result = [...new Set(remote.filter((id) => !removed.has(id)))];
  for (const id of added) if (!result.includes(id)) result.push(id);
  const oldCommon = base.filter((id) => current.has(id));
  const localCommon = local.filter((id) => old.has(id));
  if (!sameList(oldCommon, localCommon)) {
    const visible = new Set(result);
    const ordered = local.filter((id) => visible.has(id));
    const orderedSet = new Set(ordered);
    let index = 0;
    return result.map((id) => orderedSet.has(id) ? ordered[index++] : id);
  }
  return result;
}

/** Apply only fields changed locally since `base`, preserving unrelated remote edits. */
export function rebaseSidebarPreferences(
  base: PreferenceValues,
  local: PreferenceValues,
  remote: SidebarPreferences,
): SidebarPreferences {
  return {
    version: remote.version,
    pinned_ids: rebaseSidebarList(base.pinned_ids, local.pinned_ids, remote.pinned_ids),
    thread_order: rebaseSidebarList(base.thread_order, local.thread_order, remote.thread_order),
    project_order: rebaseSidebarList(base.project_order, local.project_order, remote.project_order),
    sort_mode: base.sort_mode === local.sort_mode ? remote.sort_mode : local.sort_mode,
    pinned_sort_mode: base.pinned_sort_mode === local.pinned_sort_mode
      ? remote.pinned_sort_mode : local.pinned_sort_mode,
    group_mode: base.group_mode === local.group_mode ? remote.group_mode : local.group_mode,
    settled_at: rebaseTimeMap(base.settled_at, local.settled_at, remote.settled_at),
    snoozed_until: rebaseTimeMap(base.snoozed_until, local.snoozed_until, remote.snoozed_until),
  };
}
