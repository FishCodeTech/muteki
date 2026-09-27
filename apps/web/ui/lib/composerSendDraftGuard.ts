/**
 * #201 — Guard composer clears after async send so drafts typed during flight
 * are not wiped by the success callback.
 *
 * Busy only blocks submit; the editor stays editable. Capture a snapshot at
 * submit time and only clear (or strip submitted attachments) when the live
 * composer still matches that snapshot / draft key.
 */

export type SendDraftSnapshot = {
  draftKey: string;
  /** Trimmed plain text that was submitted. */
  promptText: string;
  /** Stable fingerprint of the prompt document at submit. */
  documentFingerprint: string;
  /** Attachment ids included in this submit (order-insensitive). */
  attachmentIds: string[];
};

export type LiveComposerSlice = {
  draftKey: string;
  promptText: string;
  documentFingerprint: string;
  attachmentIds: readonly string[];
};

export type SendSuccessComposerPlan =
  | { action: "noop" }
  | { action: "clear_all" }
  | { action: "preserve_edits"; removeAttachmentIds: string[] };

/** Stable fingerprint for equality checks (segments + sorted node ids). */
export function fingerprintPromptDocument(doc: {
  segments?: unknown;
  nodes?: Record<string, unknown>;
} | null | undefined): string {
  const segments = doc?.segments ?? [];
  const nodeIds = Object.keys(doc?.nodes || {}).sort();
  return JSON.stringify({ segments, nodeIds });
}

export function normalizeAttachmentIds(ids: Iterable<string | undefined | null>): string[] {
  const out: string[] = [];
  const seen = new Set<string>();
  for (const raw of ids) {
    const id = String(raw || "").trim();
    if (!id || seen.has(id)) continue;
    seen.add(id);
    out.push(id);
  }
  return out;
}

function sortedIdKey(ids: readonly string[]): string {
  return normalizeAttachmentIds(ids).slice().sort().join("\0");
}

export function captureSendDraftSnapshot(input: {
  draftKey: string;
  promptText: string;
  promptDocument: { segments?: unknown; nodes?: Record<string, unknown> };
  attachmentIds: Iterable<string | undefined | null>;
}): SendDraftSnapshot {
  return {
    draftKey: String(input.draftKey || "").trim(),
    promptText: String(input.promptText || "").trim(),
    documentFingerprint: fingerprintPromptDocument(input.promptDocument),
    attachmentIds: normalizeAttachmentIds(input.attachmentIds),
  };
}

export function liveComposerMatchesSnapshot(
  live: LiveComposerSlice,
  snapshot: SendDraftSnapshot,
): boolean {
  if (String(live.draftKey || "").trim() !== snapshot.draftKey) return false;
  if (String(live.promptText || "").trim() !== snapshot.promptText) return false;
  if (String(live.documentFingerprint || "") !== snapshot.documentFingerprint) return false;
  return sortedIdKey(live.attachmentIds) === sortedIdKey(snapshot.attachmentIds);
}

/**
 * After send success: decide whether to wipe the live composer, strip only
 * submitted attachments, or leave React state alone (switched draft).
 */
export function planComposerAfterSendSuccess(
  live: LiveComposerSlice,
  snapshot: SendDraftSnapshot,
): SendSuccessComposerPlan {
  const liveKey = String(live.draftKey || "").trim();
  if (!snapshot.draftKey || liveKey !== snapshot.draftKey) {
    return { action: "noop" };
  }
  if (liveComposerMatchesSnapshot(live, snapshot)) {
    return { action: "clear_all" };
  }
  return {
    action: "preserve_edits",
    removeAttachmentIds: [...snapshot.attachmentIds],
  };
}

/** Drop attachments that were part of the successful submit; keep newer ones. */
export function filterAttachmentsAfterSend<T extends { id?: string }>(
  attachments: readonly T[],
  removeIds: readonly string[],
): T[] {
  const drop = new Set(normalizeAttachmentIds(removeIds));
  if (drop.size === 0) return [...attachments];
  return attachments.filter((row) => {
    const id = String(row.id || "").trim();
    if (!id) return true;
    return !drop.has(id);
  });
}

/**
 * When the user switched away mid-send, clear the *old* draft storage only if
 * it still holds the submitted plain text + attachment ids (do not wipe B).
 */
export function shouldClearDraftStorageAfterSend(
  stored: {
    prompt?: string;
    attachments?: Array<{ id?: string }>;
  } | null | undefined,
  snapshot: SendDraftSnapshot,
): boolean {
  if (!stored || !snapshot.draftKey) return false;
  if (String(stored.prompt || "").trim() !== snapshot.promptText) return false;
  const storedIds = normalizeAttachmentIds(
    (stored.attachments || []).map((row) => row.id),
  );
  return sortedIdKey(storedIds) === sortedIdKey(snapshot.attachmentIds);
}

/**
 * Failure restore: put A back only when still on the same draft key and the
 * editor is unchanged (still A) or empty. Never overwrite newer input B.
 */
export function shouldRestoreDraftAfterSendFailure(
  live: LiveComposerSlice,
  snapshot: SendDraftSnapshot,
): boolean {
  const liveKey = String(live.draftKey || "").trim();
  if (!snapshot.draftKey || liveKey !== snapshot.draftKey) return false;
  if (liveComposerMatchesSnapshot(live, snapshot)) return true;
  const liveText = String(live.promptText || "").trim();
  const liveIds = normalizeAttachmentIds(live.attachmentIds);
  return liveText === "" && liveIds.length === 0;
}
