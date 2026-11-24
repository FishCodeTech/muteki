
export type QueueEditSaveStart =
  | { ok: true; text: string }
  | { ok: false; reason: "saving" | "empty" };

export function beginQueueEditSave(input: {
  saving: boolean;
  draft: string;
}): QueueEditSaveStart {
  if (input.saving) return { ok: false, reason: "saving" };
  const text = String(input.draft || "").trim();
  if (!text) return { ok: false, reason: "empty" };
  return { ok: true, text };
}

export function shouldExitQueueEditAfterSave(result: boolean | void): boolean {
  return result === true;
}
