/**
 * C09 structured composer context document.
 *
 * Document order is an ordered list of text segments and ref nodes. Wire
 * payloads keep user text separate from capability_refs (schema v2 nodes with
 * locator + snapshot). Clipboard uses a plaintext marker round-trip plus an
 * optional custom MIME type when the browser allows it.
 */

export const COMPOSER_CONTEXT_SCHEMA = 2 as const;
export const COMPOSER_CONTEXT_CLIPBOARD_MIME = "application/x-muteki-composer-context+json";
export const COMPOSER_CONTEXT_MARKER_RE = /⟦ref:([A-Za-z0-9_-]+)⟧/g;
const MAX_SNAPSHOT_CHARS = 4000;

export type ComposerContextStatus = "ok" | "stale" | "missing" | "forbidden";

export type ComposerContextKind =
  | "command"
  | "skill"
  | "mcp"
  | "plugin"
  | "file"
  | "thread"
  | "message_span"
  | "tool_excerpt";

export interface ComposerContextLocator {
  relative_path?: string;
  start_line?: number;
  end_line?: number;
  workspace_id?: string;
  thread_id?: string;
  message_id?: string;
  turn_id?: string;
  /** UTF-16 code unit offsets into the source message text. */
  start_offset?: number;
  end_offset?: number;
  tool_call_id?: string;
  event_id?: string;
}

export interface ComposerContextSnapshot {
  label: string;
  text: string;
  captured_at: string;
  content_hash?: string;
}

export interface ComposerContextNode {
  node_id: string;
  kind: ComposerContextKind;
  locator: ComposerContextLocator;
  snapshot: ComposerContextSnapshot;
  status: ComposerContextStatus;
  status_reason?: string;
  /** Opaque catalog id for legacy mcp/skill/file/thread chips. */
  legacy_capability_id?: string;
  context_schema?: typeof COMPOSER_CONTEXT_SCHEMA;
  // Wire / legacy chip fields mirrored for send + draft persistence.
  id: string;
  name: string;
  description: string;
  source: string;
  scope: string;
}

export type PromptDocSegment =
  | { type: "text"; text: string }
  | { type: "ref"; nodeId: string };

export type PromptDocument = {
  segments: PromptDocSegment[];
  nodes: Record<string, ComposerContextNode>;
};

export function newContextNodeId(): string {
  if (typeof crypto !== "undefined" && typeof crypto.randomUUID === "function") {
    return crypto.randomUUID();
  }
  return `ctx_${Date.now().toString(36)}_${Math.random().toString(36).slice(2, 10)}`;
}

export function capSnapshotText(text: string, max = MAX_SNAPSHOT_CHARS): string {
  const value = String(text || "");
  if (value.length <= max) return value;
  return `${value.slice(0, max)}\n…`;
}

export function isInlineContextKind(kind: string): boolean {
  return kind === "file" || kind === "message_span" || kind === "tool_excerpt";
}

export function isStructuredContextNode(value: unknown): value is ComposerContextNode {
  if (!value || typeof value !== "object") return false;
  const row = value as Record<string, unknown>;
  return Boolean(
    String(row.node_id || "").trim()
    && String(row.kind || "").trim()
    && row.locator
    && typeof row.locator === "object"
    && row.snapshot
    && typeof row.snapshot === "object",
  );
}

export function emptyPromptDocument(): PromptDocument {
  return { segments: [{ type: "text", text: "" }], nodes: {} };
}

export function documentFromPromptAndRefs(
  prompt: string,
  refs: unknown[],
): PromptDocument {
  const nodes: Record<string, ComposerContextNode> = {};
  const inline: ComposerContextNode[] = [];
  for (const ref of refs) {
    const node = normalizeContextNode(ref);
    if (!node) continue;
    nodes[node.node_id] = node;
    if (isInlineContextKind(node.kind)) inline.push(node);
  }

  const text = String(prompt || "");
  if (!inline.length) {
    return { segments: [{ type: "text", text }], nodes };
  }

  // Prefer marker round-trip when present; otherwise append inline nodes after text.
  COMPOSER_CONTEXT_MARKER_RE.lastIndex = 0;
  if (COMPOSER_CONTEXT_MARKER_RE.test(text)) {
    return parseMarkedPrompt(text, nodes);
  }

  const segments: PromptDocSegment[] = [];
  if (text) segments.push({ type: "text", text });
  for (const node of inline) {
    segments.push({ type: "ref", nodeId: node.node_id });
  }
  if (!segments.length) segments.push({ type: "text", text: "" });
  return { segments, nodes };
}

/** Rebuild a prompt document from a persisted draft snapshot. */
export function promptDocumentFromDraft(draft: {
  prompt: string;
  promptSegments?: PromptDocSegment[];
  capabilityRefs: unknown[];
}): PromptDocument {
  if (draft.promptSegments?.length) {
    const nodes: Record<string, ComposerContextNode> = {};
    for (const ref of draft.capabilityRefs) {
      const node = normalizeContextNode(ref);
      if (node) nodes[node.node_id] = node;
    }
    return {
      segments: draft.promptSegments.map((segment) => (
        segment.type === "text"
          ? { type: "text", text: segment.text }
          : { type: "ref", nodeId: segment.nodeId }
      )),
      nodes,
    };
  }
  return documentFromPromptAndRefs(draft.prompt, draft.capabilityRefs);
}

export function normalizeContextNode(value: unknown): ComposerContextNode | null {
  if (!value || typeof value !== "object") return null;
  const row = value as Record<string, unknown>;
  const kind = String(row.kind || "").trim() as ComposerContextKind;
  const legacyId = String(row.legacy_capability_id || row.id || "").trim();
  const name = String(row.name || "").trim();
  if (!kind || (!legacyId && !String(row.node_id || "").trim()) || !name) return null;

  const locatorRaw = (
    row.locator && typeof row.locator === "object" && !Array.isArray(row.locator)
      ? row.locator
      : {}
  ) as Record<string, unknown>;
  const snapshotRaw = (
    row.snapshot && typeof row.snapshot === "object" && !Array.isArray(row.snapshot)
      ? row.snapshot
      : {}
  ) as Record<string, unknown>;

  const nodeId = String(row.node_id || "").trim() || newContextNodeId();
  const id = legacyId || nodeId;
  const description = String(row.description || locatorRaw.relative_path || snapshotRaw.text || "");
  const source = String(row.source || "");
  const scope = String(row.scope || "");
  const statusRaw = String(row.status || "ok").trim() as ComposerContextStatus;
  const status: ComposerContextStatus = (
    statusRaw === "stale" || statusRaw === "missing" || statusRaw === "forbidden"
      ? statusRaw
      : "ok"
  );

  const locator: ComposerContextLocator = {};
  const relativePath = String(locatorRaw.relative_path || "").trim()
    || (kind === "file" ? description : "");
  if (relativePath) locator.relative_path = relativePath;
  const workspaceId = String(locatorRaw.workspace_id || "").trim();
  if (workspaceId) locator.workspace_id = workspaceId;
  const threadId = String(locatorRaw.thread_id || "").trim();
  if (threadId) locator.thread_id = threadId;
  const messageId = String(locatorRaw.message_id || "").trim();
  if (messageId) locator.message_id = messageId;
  const turnId = String(locatorRaw.turn_id || "").trim();
  if (turnId) locator.turn_id = turnId;
  for (const key of ["start_line", "end_line", "start_offset", "end_offset"] as const) {
    const num = Number(locatorRaw[key]);
    if (Number.isFinite(num) && num >= 0) locator[key] = Math.floor(num);
  }
  const toolCallId = String(locatorRaw.tool_call_id || "").trim();
  if (toolCallId) locator.tool_call_id = toolCallId;
  const eventId = String(locatorRaw.event_id || "").trim();
  if (eventId) locator.event_id = eventId;

  const label = String(snapshotRaw.label || name || kind).trim() || name;
  const snapText = capSnapshotText(String(snapshotRaw.text || description || name));
  const capturedAt = String(snapshotRaw.captured_at || "").trim() || new Date().toISOString();
  const contentHash = String(snapshotRaw.content_hash || "").trim();

  const snapshot: ComposerContextSnapshot = {
    label,
    text: snapText,
    captured_at: capturedAt,
    ...(contentHash ? { content_hash: contentHash } : {}),
  };

  const statusReason = String(row.status_reason || "").trim();
  return {
    node_id: nodeId,
    kind,
    locator,
    snapshot,
    status,
    ...(statusReason ? { status_reason: statusReason } : {}),
    ...(legacyId ? { legacy_capability_id: legacyId } : {}),
    context_schema: COMPOSER_CONTEXT_SCHEMA,
    id,
    name,
    description,
    source,
    scope,
  };
}

export function flattenPromptDocument(doc: PromptDocument): string {
  let out = "";
  for (const segment of doc.segments) {
    if (segment.type === "text") out += segment.text;
    else out += `⟦ref:${segment.nodeId}⟧`;
  }
  return out;
}

export function plainTextFromDocument(doc: PromptDocument): string {
  let out = "";
  for (const segment of doc.segments) {
    if (segment.type === "text") out += segment.text;
    else {
      const node = doc.nodes[segment.nodeId];
      out += node ? `@${node.snapshot.label || node.name}` : "";
    }
  }
  return out;
}

export function refsFromDocument(doc: PromptDocument): ComposerContextNode[] {
  const ordered: ComposerContextNode[] = [];
  const seen = new Set<string>();
  for (const segment of doc.segments) {
    if (segment.type !== "ref") continue;
    const node = doc.nodes[segment.nodeId];
    if (!node || seen.has(node.node_id)) continue;
    seen.add(node.node_id);
    ordered.push(node);
  }
  for (const node of Object.values(doc.nodes)) {
    if (seen.has(node.node_id)) continue;
    if (isInlineContextKind(node.kind)) continue;
    ordered.push(node);
  }
  return ordered;
}

/**
 * Drop inline citation nodes that are no longer reachable from segments.
 * Strip chips (skill/mcp/plugin/thread) may live in `nodes` without a ref
 * segment — those are kept. Prevents invisible/orphan file refs after a
 * DOM desync (#206).
 */
export function pruneUnreachableInlineNodes(doc: PromptDocument): PromptDocument {
  const referenced = new Set<string>();
  for (const segment of doc.segments) {
    if (segment.type === "ref") referenced.add(segment.nodeId);
  }
  let changed = false;
  const nodes: Record<string, ComposerContextNode> = {};
  for (const [id, node] of Object.entries(doc.nodes)) {
    if (referenced.has(id) || !isInlineContextKind(node.kind)) {
      nodes[id] = node;
    } else {
      changed = true;
    }
  }
  return changed ? { segments: doc.segments, nodes } : doc;
}

export function wireCapabilityRefs(doc: PromptDocument): Array<Record<string, unknown>> {
  return refsFromDocument(doc).map((node) => ({
    context_schema: COMPOSER_CONTEXT_SCHEMA,
    node_id: node.node_id,
    id: node.id,
    kind: node.kind,
    name: node.name,
    description: node.description,
    source: node.source,
    scope: node.scope,
    locator: { ...node.locator },
    snapshot: { ...node.snapshot },
    status: node.status,
    ...(node.status_reason ? { status_reason: node.status_reason } : {}),
    ...(node.legacy_capability_id
      ? { legacy_capability_id: node.legacy_capability_id }
      : {}),
  }));
}

export function parseMarkedPrompt(
  marked: string,
  knownNodes: Record<string, ComposerContextNode> = {},
): PromptDocument {
  const segments: PromptDocSegment[] = [];
  const nodes: Record<string, ComposerContextNode> = { ...knownNodes };
  let last = 0;
  const text = String(marked || "");
  COMPOSER_CONTEXT_MARKER_RE.lastIndex = 0;
  let match: RegExpExecArray | null = COMPOSER_CONTEXT_MARKER_RE.exec(text);
  while (match) {
    const start = match.index;
    if (start > last) {
      segments.push({ type: "text", text: text.slice(last, start) });
    }
    const nodeId = match[1];
    if (nodes[nodeId]) {
      segments.push({ type: "ref", nodeId });
    } else {
      segments.push({ type: "text", text: match[0] });
    }
    last = start + match[0].length;
    match = COMPOSER_CONTEXT_MARKER_RE.exec(text);
  }
  if (last < text.length) segments.push({ type: "text", text: text.slice(last) });
  if (!segments.length) segments.push({ type: "text", text: "" });
  return pruneUnreachableInlineNodes({ segments, nodes });
}

export function insertNodeAtCaret(
  doc: PromptDocument,
  node: ComposerContextNode,
  caretTextOffset?: number,
): PromptDocument {
  const nextNodes = { ...doc.nodes, [node.node_id]: node };
  const flat = flattenPromptDocument(doc);
  const offset = Math.max(0, Math.min(
    caretTextOffset === undefined ? flat.length : caretTextOffset,
    flat.length,
  ));
  // Insert relative to marked flat string, then re-parse so adjacent text splits correctly.
  const marked = `${flat.slice(0, offset)}⟦ref:${node.node_id}⟧${flat.slice(offset)}`;
  return parseMarkedPrompt(marked, nextNodes);
}

export function removeNode(doc: PromptDocument, nodeId: string): PromptDocument {
  const nodes = { ...doc.nodes };
  delete nodes[nodeId];
  const segments = doc.segments.filter(
    (segment) => !(segment.type === "ref" && segment.nodeId === nodeId),
  );
  if (!segments.length) return { segments: [{ type: "text", text: "" }], nodes };
  // Merge adjacent text segments.
  const merged: PromptDocSegment[] = [];
  for (const segment of segments) {
    const prev = merged[merged.length - 1];
    if (segment.type === "text" && prev?.type === "text") {
      prev.text += segment.text;
    } else {
      merged.push(segment.type === "text" ? { ...segment } : { ...segment });
    }
  }
  return { segments: merged, nodes };
}

/**
 * Apply a strip-catalog chip (skill / MCP / plugin / thread) in one step:
 * drop the active @/$ token and keep the chip on the same document.
 * Two-step "add node then replace token from a stale snapshot" drops the chip
 * and empties the editor when the token was the only text (#118).
 */
export function applyCatalogCapability(
  doc: PromptDocument,
  item: {
    id: string;
    kind: ComposerContextKind;
    name: string;
    description?: string;
    source?: string;
    scope?: string;
  },
  token?: { start: number; end: number } | null,
): PromptDocument {
  let next = doc;
  if (token) {
    const flat = flattenPromptDocument(doc);
    const start = Math.max(0, Math.min(token.start, flat.length));
    const end = Math.max(start, Math.min(token.end, flat.length));
    const nextFlat = `${flat.slice(0, start)}${flat.slice(end)}`;
    const rebuilt = documentFromPromptAndRefs(nextFlat, refsFromDocument(doc));
    next = { ...rebuilt, nodes: { ...doc.nodes, ...rebuilt.nodes } };
  }
  const already = refsFromDocument(next).some(
    (ref) => ref.id === item.id || ref.legacy_capability_id === item.id,
  );
  if (already) return next;
  const node = normalizeContextNode({
    node_id: `strip_${item.id}`,
    id: item.id,
    kind: item.kind,
    name: item.name,
    description: item.description || item.name,
    source: item.source || "",
    scope: item.scope || "",
    legacy_capability_id: item.id,
    locator: {},
    snapshot: {
      label: item.name,
      text: capSnapshotText(item.description || item.name),
      captured_at: new Date().toISOString(),
    },
    status: "ok",
  });
  if (!node) return next;
  return { ...next, nodes: { ...next.nodes, [node.node_id]: node } };
}

export function createFileContextNode(input: {
  id: string;
  name: string;
  relativePath: string;
  source?: string;
  scope?: string;
  workspaceId?: string;
  startLine?: number;
  endLine?: number;
  snapshotText?: string;
  contentHash?: string;
}): ComposerContextNode {
  const label = input.startLine && input.endLine
    ? `${input.name}:${input.startLine}-${input.endLine}`
    : input.name;
  return normalizeContextNode({
    node_id: newContextNodeId(),
    id: input.id,
    kind: "file",
    name: input.name,
    description: input.relativePath,
    source: input.source || "当前项目",
    scope: input.scope || "project",
    legacy_capability_id: input.id,
    locator: {
      relative_path: input.relativePath,
      ...(input.workspaceId ? { workspace_id: input.workspaceId } : {}),
      ...(input.startLine ? { start_line: input.startLine } : {}),
      ...(input.endLine ? { end_line: input.endLine } : {}),
    },
    snapshot: {
      label,
      text: capSnapshotText(input.snapshotText || input.relativePath),
      captured_at: new Date().toISOString(),
      ...(input.contentHash ? { content_hash: input.contentHash } : {}),
    },
    status: "ok",
  }) as ComposerContextNode;
}

export function createMessageSpanNode(input: {
  threadId: string;
  messageId: string;
  turnId?: string;
  startOffset: number;
  endOffset: number;
  text: string;
  streaming?: boolean;
}): ComposerContextNode {
  const excerpt = capSnapshotText(input.text);
  const label = input.turnId ? `回答 · ${input.turnId.slice(0, 8)}` : "回答摘录";
  return normalizeContextNode({
    node_id: newContextNodeId(),
    id: `message_span:${input.messageId}:${input.startOffset}-${input.endOffset}`,
    kind: "message_span",
    name: label,
    description: excerpt.slice(0, 180),
    source: "对话",
    scope: "thread",
    locator: {
      thread_id: input.threadId,
      message_id: input.messageId,
      ...(input.turnId ? { turn_id: input.turnId } : {}),
      start_offset: input.startOffset,
      end_offset: input.endOffset,
    },
    snapshot: {
      label: input.streaming ? `${label}（流式）` : label,
      text: excerpt,
      captured_at: new Date().toISOString(),
    },
    status: "ok",
  }) as ComposerContextNode;
}

/**
 * Slice a prompt document by marked-flat [start, end) offsets (same space as
 * flattenPromptDocument / insertNodeAtCaret). Text is substring-clipped; ref
 * chips are atomic and included when their marker range overlaps the window.
 * Only nodes referenced by the sliced segments are retained (strip chips that
 * are not in the selection are dropped). Collapsed / empty → empty document.
 */
export function slicePromptDocument(
  doc: PromptDocument,
  start: number,
  end: number,
): PromptDocument {
  const flat = flattenPromptDocument(doc);
  const s = Math.max(0, Math.min(start, flat.length));
  const e = Math.max(s, Math.min(end, flat.length));
  if (s >= e) return emptyPromptDocument();

  const segments: PromptDocSegment[] = [];
  const nodes: Record<string, ComposerContextNode> = {};
  let cursor = 0;
  for (const segment of doc.segments) {
    if (segment.type === "text") {
      const segStart = cursor;
      const segEnd = cursor + segment.text.length;
      const overlapStart = Math.max(s, segStart);
      const overlapEnd = Math.min(e, segEnd);
      if (overlapStart < overlapEnd) {
        segments.push({
          type: "text",
          text: segment.text.slice(overlapStart - segStart, overlapEnd - segStart),
        });
      }
      cursor = segEnd;
      continue;
    }
    const marker = `⟦ref:${segment.nodeId}⟧`;
    const segStart = cursor;
    const segEnd = cursor + marker.length;
    if (segStart < e && segEnd > s) {
      segments.push({ type: "ref", nodeId: segment.nodeId });
      const node = doc.nodes[segment.nodeId];
      if (node) nodes[segment.nodeId] = node;
    }
    cursor = segEnd;
  }
  if (!segments.length) segments.push({ type: "text", text: "" });
  return { segments, nodes };
}

export type ClipboardContextPayload = {
  version: typeof COMPOSER_CONTEXT_SCHEMA;
  document: PromptDocument;
};

export function serializeClipboardPayload(doc: PromptDocument): string {
  const payload: ClipboardContextPayload = {
    version: COMPOSER_CONTEXT_SCHEMA,
    document: {
      segments: doc.segments.map((segment) => (
        segment.type === "text"
          ? { type: "text", text: segment.text }
          : { type: "ref", nodeId: segment.nodeId }
      )),
      nodes: { ...doc.nodes },
    },
  };
  return JSON.stringify(payload);
}

export function parseClipboardPayload(raw: string): PromptDocument | null {
  try {
    const parsed: unknown = JSON.parse(raw);
    if (!parsed || typeof parsed !== "object") return null;
    const row = parsed as Record<string, unknown>;
    const docRaw = row.document && typeof row.document === "object"
      ? row.document as Record<string, unknown>
      : row;
    const nodesRaw = (
      docRaw.nodes && typeof docRaw.nodes === "object" && !Array.isArray(docRaw.nodes)
        ? docRaw.nodes
        : {}
    ) as Record<string, unknown>;
    const nodes: Record<string, ComposerContextNode> = {};
    for (const [key, value] of Object.entries(nodesRaw)) {
      const node = normalizeContextNode(value);
      if (node) nodes[key] = node;
    }
    const segmentsRaw = Array.isArray(docRaw.segments) ? docRaw.segments : [];
    const segments: PromptDocSegment[] = [];
    for (const item of segmentsRaw) {
      if (!item || typeof item !== "object") continue;
      const seg = item as Record<string, unknown>;
      if (seg.type === "ref") {
        const nodeId = String(seg.nodeId || "").trim();
        if (nodeId && nodes[nodeId]) segments.push({ type: "ref", nodeId });
        continue;
      }
      if (seg.type === "text") {
        segments.push({ type: "text", text: String(seg.text || "") });
      }
    }
    if (!segments.length && Object.keys(nodes).length === 0) {
      // Plaintext marker fallback
      return parseMarkedPrompt(String(raw || ""), {});
    }
    if (!segments.length) segments.push({ type: "text", text: "" });
    return { segments, nodes };
  } catch {
    return parseMarkedPrompt(String(raw || ""), {});
  }
}

/**
 * Insert plain text into the marked flat string at [start, end), replacing any
 * selection. Offsets are marked-document indices (same space as insertNodeAtCaret
 * / mergeClipboardIntoDocument). Defaults to append at end when omitted.
 */
export function insertPlainTextAtMarkedRange(
  doc: PromptDocument,
  text: string,
  start?: number,
  end?: number,
): PromptDocument {
  const flat = flattenPromptDocument(doc);
  const s = Math.max(0, Math.min(start === undefined ? flat.length : start, flat.length));
  const e = Math.max(s, Math.min(end === undefined ? s : end, flat.length));
  return parseMarkedPrompt(
    `${flat.slice(0, s)}${text}${flat.slice(e)}`,
    doc.nodes,
  );
}

export function mergeClipboardIntoDocument(
  current: PromptDocument,
  pasted: PromptDocument,
  caretMarkedOffset?: number,
): PromptDocument {
  const nodes = { ...current.nodes, ...pasted.nodes };
  const insertMarked = flattenPromptDocument(pasted);
  const flat = flattenPromptDocument(current);
  const offset = Math.max(0, Math.min(
    caretMarkedOffset === undefined ? flat.length : caretMarkedOffset,
    flat.length,
  ));
  return parseMarkedPrompt(
    `${flat.slice(0, offset)}${insertMarked}${flat.slice(offset)}`,
    nodes,
  );
}

/**
 * message_span offsets are UTF-16 code units (JS string indices).
 * For BMP text (incl. CJK) this matches code points.
 */
export function sliceByUtf16(text: string, start: number, end: number): string {
  const s = Math.max(0, Math.min(start, text.length));
  const e = Math.max(s, Math.min(end, text.length));
  return text.slice(s, e);
}
