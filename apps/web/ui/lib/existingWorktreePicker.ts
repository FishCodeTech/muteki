/**
 * Pure helpers for /chat "已有 worktree" path picker (#202).
 * UI fetches via fetchProjectWorktrees; Shell binds with root_path.
 */

export interface WorktreePickerRow {
  path: string;
  branch: string | null;
  detached?: boolean;
  occupied?: boolean;
  occupant_count?: number;
  occupant_thread_ids?: string[];
}

export type WorktreeListPhase = "idle" | "loading" | "ready" | "error" | "empty";

export function normalizeWorktreePath(path: string): string {
  return String(path || "").trim().replace(/\/+$/, "");
}

export function isMainCheckoutPath(path: string, projectRootPath: string): boolean {
  const a = normalizeWorktreePath(path);
  const b = normalizeWorktreePath(projectRootPath);
  if (!a || !b) return false;
  return a === b;
}

/** Linked worktrees only — main checkout has its own "使用现有检出" mode. */
export function linkedWorktrees(
  rows: WorktreePickerRow[],
  projectRootPath: string,
): WorktreePickerRow[] {
  return rows.filter((row) => {
    const path = normalizeWorktreePath(row.path);
    if (!path) return false;
    return !isMainCheckoutPath(path, projectRootPath);
  });
}

export function worktreeListPhase(input: {
  modeIsExisting: boolean;
  hasProject: boolean;
  loading: boolean;
  error: string;
  linkedCount: number;
}): WorktreeListPhase {
  if (!input.modeIsExisting || !input.hasProject) return "idle";
  if (input.loading) return "loading";
  if (input.error) return "error";
  if (input.linkedCount === 0) return "empty";
  return "ready";
}

export function worktreeEmptyCopy(hasMainOnly: boolean): string {
  if (hasMainOnly) {
    return "当前项目只有主检出，没有可绑定的已有 worktree。请改用「使用现有检出」，或切换到「新建 worktree」。";
  }
  return "未找到可绑定的已有 worktree。请改用「使用现有检出」或「新建 worktree」。";
}

export function worktreeLoadFailCopy(error: string): string {
  const detail = String(error || "").trim();
  return detail
    ? `读取 worktree 列表失败：${detail}`
    : "读取 worktree 列表失败，请重试";
}

export function worktreeMissingSelectionCopy(): string {
  return "请先选择要绑定的已有 worktree 路径";
}

export function worktreeOccupiedCopy(row: WorktreePickerRow): string {
  const count = Number(row.occupant_count || 0);
  if (count > 0) {
    return `已被 ${count} 个会话占用；仍可绑定，但请确认不会交叉改写`;
  }
  return "该 worktree 正被其他会话占用；仍可绑定，但请确认不会交叉改写";
}

export function worktreeOptionLabel(row: WorktreePickerRow): string {
  const path = normalizeWorktreePath(row.path);
  const base = path.split("/").filter(Boolean).at(-1) || path;
  const branch = row.detached
    ? "detached"
    : (row.branch || "未知分支");
  const occ = row.occupied ? " · 占用" : "";
  return `${branch} — ${base}${occ}`;
}

/** Keep selection only when it still appears in the linked list. */
export function reconcileSelectedWorktreePath(
  selectedPath: string,
  linked: WorktreePickerRow[],
): string {
  const want = normalizeWorktreePath(selectedPath);
  if (!want) return "";
  const hit = linked.find((row) => normalizeWorktreePath(row.path) === want);
  return hit ? normalizeWorktreePath(hit.path) : "";
}

export function canBindExistingWorktree(selectedPath: string): boolean {
  return Boolean(normalizeWorktreePath(selectedPath));
}

/** Clear path when leaving existing_worktree or switching project. */
export function shouldClearExistingWorktreePath(input: {
  prevMode?: string;
  nextMode?: string;
  prevProjectId?: string;
  nextProjectId?: string;
}): boolean {
  if (
    input.prevProjectId !== undefined
    && input.nextProjectId !== undefined
    && input.prevProjectId !== input.nextProjectId
  ) {
    return true;
  }
  if (
    input.prevMode !== undefined
    && input.nextMode !== undefined
    && input.prevMode !== input.nextMode
  ) {
    return true;
  }
  return false;
}
