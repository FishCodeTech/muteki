/**
 * Serialize contenteditable composer DOM to plain text while restoring
 * block boundaries (`div`/`p`/…) and `<br>` as `\n`.
 *
 * Native multiline paste / `insertText` yields shapes like
 * `第一行<div>第二行</div><div>第三行</div>` or `a<div><br></div><div>b</div>`;
 * `textContent` collapses those. This walker inverts that mapping so copy,
 * draft, and send payload match the pasted source (not Chrome `innerText`,
 * which over-counts blank lines).
 *
 * Citation chips (`[data-node-id]`) stay structured — callers emit ref
 * segments separately.
 */

const BLOCK_TAGS = new Set([
  "ADDRESS",
  "ARTICLE",
  "ASIDE",
  "BLOCKQUOTE",
  "DIV",
  "DL",
  "FIELDSET",
  "FIGCAPTION",
  "FIGURE",
  "FOOTER",
  "FORM",
  "H1",
  "H2",
  "H3",
  "H4",
  "H5",
  "H6",
  "HEADER",
  "HR",
  "LI",
  "MAIN",
  "NAV",
  "OL",
  "P",
  "PRE",
  "SECTION",
  "TABLE",
  "TBODY",
  "TD",
  "TFOOT",
  "TH",
  "THEAD",
  "TR",
  "UL",
]);

/** Duck-typed node so Node self-checks can run without jsdom. */
export type ComposerDomNodeLike = {
  nodeType: number;
  textContent?: string | null;
  tagName?: string;
  childNodes?: ArrayLike<ComposerDomNodeLike>;
  dataset?: { nodeId?: string };
};

const TEXT_NODE = 3;
const ELEMENT_NODE = 1;

export function isComposerBlockTag(tagName: string | undefined): boolean {
  return BLOCK_TAGS.has(String(tagName || "").toUpperCase());
}

function cleanText(value: string | null | undefined): string {
  return String(value || "").replace(/\u200B/g, "");
}

function childrenOf(node: ComposerDomNodeLike): ComposerDomNodeLike[] {
  return Array.from(node.childNodes || []);
}

function isCitation(el: ComposerDomNodeLike): boolean {
  return Boolean(el.dataset?.nodeId);
}

/** Empty block used by browsers as a blank / trailing line (`<div><br></div>`). */
export function isComposerEmptyBlockPlaceholder(el: ComposerDomNodeLike): boolean {
  const kids = childrenOf(el).filter((n) => {
    if (n.nodeType === TEXT_NODE) return cleanText(n.textContent) !== "";
    return n.nodeType === ELEMENT_NODE;
  });
  if (kids.length === 0) return true;
  if (
    kids.length === 1
    && kids[0].nodeType === ELEMENT_NODE
    && String(kids[0].tagName || "").toUpperCase() === "BR"
  ) {
    return true;
  }
  return false;
}

/**
 * Serialize inside an element: text + `<br>` → `\n`, nested blocks get
 * boundary newlines. Empty `<div><br></div>` yields `""` here; the top-level
 * walker emits a single `\n` for placeholders.
 */
export function serializeComposerDomInner(el: ComposerDomNodeLike): string {
  if (isCitation(el)) return "";
  if (isComposerEmptyBlockPlaceholder(el)) return "";
  let out = "";
  for (const child of childrenOf(el)) {
    if (child.nodeType === TEXT_NODE) {
      out += cleanText(child.textContent);
      continue;
    }
    if (child.nodeType !== ELEMENT_NODE) continue;
    if (isCitation(child)) continue;
    const tag = String(child.tagName || "").toUpperCase();
    if (tag === "BR") {
      out += "\n";
      continue;
    }
    if (isComposerBlockTag(tag)) {
      if (isComposerEmptyBlockPlaceholder(child)) {
        out += "\n";
        continue;
      }
      if (out) out += "\n";
      out += serializeComposerDomInner(child);
      continue;
    }
    out += serializeComposerDomInner(child);
  }
  return out;
}

export type ComposerDomSegment =
  | { type: "text"; text: string }
  | { type: "ref"; nodeId: string };

/** True when prior segments have a chip or non-newline text (not only leading `\n`s). */
function hasNonNewlineContent(segments: ComposerDomSegment[]): boolean {
  return segments.some((seg) => (
    seg.type === "ref"
    || (seg.type === "text" && seg.text.replace(/\n/g, "").length > 0)
  ));
}

function appendText(segments: ComposerDomSegment[], text: string): void {
  if (!text) return;
  const prev = segments[segments.length - 1];
  if (prev?.type === "text") prev.text += text;
  else segments.push({ type: "text", text });
}

/**
 * Walk top-level composer children into text/ref segments, restoring
 * block/`br` newlines to match the inverse of Chrome `insertText` paste.
 * `known` maps citation node ids that should stay refs.
 */
export function readComposerDomSegments(
  root: ComposerDomNodeLike,
  known: Record<string, unknown> = {},
): ComposerDomSegment[] {
  const segments: ComposerDomSegment[] = [];
  for (const child of childrenOf(root)) {
    if (child.nodeType === TEXT_NODE) {
      const text = cleanText(child.textContent);
      if (!text && segments.length) continue;
      appendText(segments, text);
      continue;
    }
    if (child.nodeType !== ELEMENT_NODE) continue;
    const nodeId = child.dataset?.nodeId;
    if (nodeId && known[nodeId] !== undefined) {
      segments.push({ type: "ref", nodeId });
      continue;
    }
    const tag = String(child.tagName || "").toUpperCase();
    if (tag === "BR") {
      appendText(segments, "\n");
      continue;
    }
    if (isComposerBlockTag(tag)) {
      // Blank / trailing placeholder: exactly one `\n` (Chrome maps each
      // source newline-only line to `<div><br></div>`).
      if (isComposerEmptyBlockPlaceholder(child)) {
        appendText(segments, "\n");
        continue;
      }
      // Non-empty block: open a new line when prior non-newline content exists.
      // Empty placeholders already contributed `\n`; leading-only blanks must not
      // gain an extra boundary (`<div><br></div><div>hello</div>` → `\nhello`),
      // while `a<div><br></div><div>b</div>` still becomes `a\n\nb`.
      if (hasNonNewlineContent(segments)) {
        appendText(segments, "\n");
      }
    }
    // Native Enter and rich clipboard content may nest a citation inside a
    // block/span. Keep the ref segment instead of silently flattening it away.
    for (const nested of readComposerDomSegments(child, known)) {
      if (nested.type === "text") appendText(segments, nested.text);
      else segments.push(nested);
    }
  }
  if (!segments.length) segments.push({ type: "text", text: "" });
  return segments;
}

/** Flatten segments to plain marked text (refs as ⟦ref:id⟧). */
export function flattenComposerDomSegments(segments: ComposerDomSegment[]): string {
  let out = "";
  for (const seg of segments) {
    if (seg.type === "text") out += seg.text;
    else out += `⟦ref:${seg.nodeId}⟧`;
  }
  return out;
}

/** DOM selection offsets share the serializer's newline/ref-marker coordinate space. */
export function composerMarkedOffsetAtDomPoint(
  root: HTMLElement,
  container: Node,
  pointOffset: number,
): number {
  // Serialize a clone with a sentinel at the point. Keeping the full shape
  // matters: a prefix clone can turn an empty first <div> into a false newline.
  const original = container.nodeType === 1 ? container as Element : container.parentElement;
  const chip = original?.closest<HTMLElement>("[data-node-id]");
  if (chip && root.contains(chip) && chip.parentNode) {
    const index = Array.from(chip.parentNode.childNodes).indexOf(chip);
    return composerMarkedOffsetAtDomPoint(root, chip.parentNode, index + 1);
  }
  const path: number[] = [];
  let cursor: Node = container;
  while (cursor !== root && cursor.parentNode) {
    path.unshift(Array.from(cursor.parentNode.childNodes).indexOf(cursor as ChildNode));
    cursor = cursor.parentNode;
  }
  const clone = root.cloneNode(true) as HTMLElement;
  let target: Node = clone;
  for (const index of path) target = target.childNodes[index];
  let marker = "\uFDD0";
  while ((root.textContent || "").includes(marker)) marker += "\uFDD0";
  const range = root.ownerDocument.createRange();
  range.setStart(target, pointOffset);
  range.collapse(true);
  range.insertNode(root.ownerDocument.createTextNode(marker));
  const known: Record<string, true> = {};
  for (const ref of root.querySelectorAll<HTMLElement>("[data-node-id]")) {
    if (ref.dataset.nodeId) known[ref.dataset.nodeId] = true;
  }
  const flat = flattenComposerDomSegments(readComposerDomSegments(clone, known));
  return Math.max(0, flat.indexOf(marker));
}
