import type { SidebarPreferences } from "./useConversation";

type PreferenceValues = Omit<SidebarPreferences, "version">;

function sameList(left: string[], right: string[]): boolean {
  return left.length === right.length && left.every((value, index) => value === right[index]);
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
    && left.group_mode === right.group_mode;
}

/** Apply only fields changed locally since `base`, preserving unrelated remote edits. */
export function rebaseSidebarPreferences(
  base: PreferenceValues,
  local: PreferenceValues,
  remote: SidebarPreferences,
): SidebarPreferences {
  return {
    version: remote.version,
    pinned_ids: sameList(base.pinned_ids, local.pinned_ids)
      ? remote.pinned_ids : local.pinned_ids,
    thread_order: sameList(base.thread_order, local.thread_order)
      ? remote.thread_order : local.thread_order,
    project_order: sameList(base.project_order, local.project_order)
      ? remote.project_order : local.project_order,
    sort_mode: base.sort_mode === local.sort_mode ? remote.sort_mode : local.sort_mode,
    pinned_sort_mode: base.pinned_sort_mode === local.pinned_sort_mode
      ? remote.pinned_sort_mode : local.pinned_sort_mode,
    group_mode: base.group_mode === local.group_mode ? remote.group_mode : local.group_mode,
  };
}
