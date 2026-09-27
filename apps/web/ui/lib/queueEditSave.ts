/**
 * #195 — Queue-item edit save must not discard the draft when the update fails.
 *
 * ConversationQueue used to fire onUpdate and immediately exit edit mode.
 * Shell never reported success, so a failed fetch left the editor closed and
 * the next edit re-seeded from the stale item.text.
 *
 * Pure helpers: decide whether a save attempt may start, and whether edit mode
 * should exit after the awaitable onUpdate settles.
 */

export type QueueEditSaveStart =
  | { ok: true; text: string }
  | { ok: false; reason: "saving" | "empty" };

/** Gate a save click: block double-submit and blank drafts. */
export function beginQueueEditSave(input: {
  saving: boolean;
  draft: string;
}): QueueEditSaveStart {
  if (input.saving) return { ok: false, reason: "saving" };
  const text = String(input.draft || "").trim();
  if (!text) return { ok: false, reason: "empty" };
  return { ok: true, text };
}

/**
 * Exit edit mode only when onUpdate explicitly reports success (`true`).
 * `false` / `void` / rejection-handled-as-false keep the draft for retry.
 */
export function shouldExitQueueEditAfterSave(result: boolean | void): boolean {
  return result === true;
}
