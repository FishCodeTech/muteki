/**
 * Feedback contract for saving/clearing project conversation defaults (#190).
 * Failure must surface an error (no silent success); success returns a notice
 * and allows local projects state to align with the server persist.
 */

export type ProjectDefaultPersistOp = "set" | "clear";

export type ProjectDefaultPersistFeedback = {
  error: string;
  notice: string;
  /** When true, local projects settings may be patched to match the persist. */
  applyLocalProjects: boolean;
};

export function projectDefaultPersistFeedback(
  op: ProjectDefaultPersistOp,
  result: { ok: true } | { ok: false; error: unknown },
): ProjectDefaultPersistFeedback {
  if (result.ok) {
    return {
      error: "",
      notice: op === "set" ? "已设为项目默认" : "已清除项目默认",
      applyLocalProjects: true,
    };
  }
  const detail = result.error instanceof Error
    ? result.error.message
    : String(result.error ?? "");
  const trimmed = detail.trim();
  const prefix = op === "set" ? "保存项目默认失败" : "清除项目默认失败";
  return {
    error: trimmed ? `${prefix}：${trimmed}` : `${prefix}，请重试`,
    notice: "",
    applyLocalProjects: false,
  };
}
