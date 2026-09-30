import { COMPOSER_DRAFT_STORAGE_KEY, COMPOSER_DRAFT_STORAGE_KEY_V1, normalizeComposerDraft, type ComposerDraft } from "./composerDraftStore";
import { COMPOSER_STASH_STORAGE_KEY } from "./composerRecallStore";
import { conversationStorageScope } from "./conversationStorageScope";
export type LegacyComposerEntry = { kind: "draft" | "stash"; key: string; label: string; snapshot: ComposerDraft };
/** Legacy records have no trustworthy service identity. Read only after explicit review. */
export function listLegacyComposerEntries(): LegacyComposerEntry[] {
  if (typeof window === "undefined" || !conversationStorageScope()) throw new Error("draft.legacy.scope_unverified: 请先确认当前服务身份");
  const entries: LegacyComposerEntry[] = [];
  const raw = window.localStorage.getItem(COMPOSER_DRAFT_STORAGE_KEY) ?? window.localStorage.getItem(COMPOSER_DRAFT_STORAGE_KEY_V1);
  if (raw) {
    const parsed: unknown = JSON.parse(raw);
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) throw new Error("draft.legacy.invalid: 旧草稿格式无效，原记录保留");
    const row = parsed as Record<string, unknown>, drafts = row.drafts || row;
    if (!drafts || typeof drafts !== "object" || Array.isArray(drafts)) throw new Error("draft.legacy.invalid: 旧草稿格式无效，原记录保留");
    for (const [key, value] of Object.entries(drafts)) { const snapshot = normalizeComposerDraft(value); if (snapshot) entries.push({ kind: "draft", key, label: key, snapshot }); }
  }
  const stash = window.localStorage.getItem(COMPOSER_STASH_STORAGE_KEY);
  if (stash) {
    const parsed: unknown = JSON.parse(stash);
    const rows = parsed && typeof parsed === "object" && !Array.isArray(parsed) ? (parsed as Record<string, unknown>).stashes : parsed;
    if (!Array.isArray(rows)) throw new Error("draft.legacy.stash_invalid: 旧暂存格式无效，原记录保留");
    for (const row of rows) { const snapshot = normalizeComposerDraft(row?.snapshot); if (snapshot) entries.push({ kind: "stash", key: String(row.id || row.draftKey || entries.length), label: String(row.name || "旧暂存"), snapshot }); }
  }
  return entries;
}
/** Copy content only. Never reuse legacy commands, server attachment hashes or launch permissions. */
export function prepareLegacyComposerCopy(entry: LegacyComposerEntry): ComposerDraft {
  const nodeIds = new Map<string, string>();
  const capabilityRefs = entry.snapshot.capabilityRefs.map((ref) => {
    const id = crypto.randomUUID(); nodeIds.set(ref.node_id || ref.id, id);
    return { ...ref, id, node_id: id, status: "stale" as const, status_reason: "旧记录没有服务归属证据，请在当前工作台重新选择引用", legacy_capability_id: undefined };
  });
  return {
    prompt: entry.snapshot.prompt,
    promptSegments: entry.snapshot.promptSegments?.map((segment) => segment.type === "text" ? { ...segment } : { type: "ref", nodeId: nodeIds.get(segment.nodeId) || segment.nodeId }),
    capabilityRefs,
    attachments: entry.snapshot.attachments.map((attachment) => ({ id: crypto.randomUUID(), name: attachment.name, size: attachment.size, mimeType: attachment.mimeType, needsReselect: true })),
    accessMode: "supervised", updatedAt: Date.now(),
  };
}
