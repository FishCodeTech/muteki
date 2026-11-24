import { promptDocumentFromDraft } from "./composerContextDoc";

export type SendDraftSnapshot = {
  draftKey: string;
  promptText: string;
  documentFingerprint: string;
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

export function fingerprintPromptDocument(doc: {
  segments?: unknown;
  nodes?: Record<string, unknown>;
} | null | undefined): string {
  const segments = doc?.segments ?? [];
  const nodes = Object.fromEntries(Object.entries(doc?.nodes || {}).sort(([left], [right]) => left.localeCompare(right)));
  return JSON.stringify({ segments, nodes });
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

export function shouldClearDraftStorageAfterSend(
  stored: {
    prompt?: string;
    promptSegments?: Array<{ type: "text"; text: string } | { type: "ref"; nodeId: string }>;
    capabilityRefs?: unknown[];
    attachments?: Array<{ id?: string }>;
  } | null | undefined,
  snapshot: SendDraftSnapshot,
): boolean {
  if (!stored || !snapshot.draftKey) return false;
  if (String(stored.prompt || "").trim() !== snapshot.promptText) return false;
  if (fingerprintPromptDocument(promptDocumentFromDraft({ prompt: stored.prompt || "", promptSegments: stored.promptSegments, capabilityRefs: stored.capabilityRefs || [] })) !== snapshot.documentFingerprint) return false;
  const storedIds = normalizeAttachmentIds(
    (stored.attachments || []).map((row) => row.id),
  );
  return sortedIdKey(storedIds) === sortedIdKey(snapshot.attachmentIds);
}
