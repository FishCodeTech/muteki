
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
      if (isComposerEmptyBlockPlaceholder(child)) {
        appendText(segments, "\n");
        continue;
      }
      if (hasNonNewlineContent(segments)) {
        appendText(segments, "\n");
      }
    }
    for (const nested of readComposerDomSegments(child, known)) {
      if (nested.type === "text") appendText(segments, nested.text);
      else segments.push(nested);
    }
  }
  if (!segments.length) segments.push({ type: "text", text: "" });
  return segments;
}

export function flattenComposerDomSegments(segments: ComposerDomSegment[]): string {
  let out = "";
  for (const seg of segments) {
    if (seg.type === "text") out += seg.text;
    else out += `⟦ref:${seg.nodeId}⟧`;
  }
  return out;
}

export function composerMarkedOffsetAtDomPoint(
  root: HTMLElement,
  container: Node,
  pointOffset: number,
): number {
  if (container === root || container.parentNode === root) {
    let prefix = 0;
    let index = 0;
    let child: Node | null = root.firstChild;
    for (; child; child = child.nextSibling, index += 1) {
      if (container === root && index === pointOffset) return prefix;
      if (child.nodeType !== TEXT_NODE) break;
      if (child === container) return prefix + cleanText(child.textContent?.slice(0, pointOffset)).length;
      prefix += cleanText(child.textContent).length;
    }
    if (container === root && child === null) return prefix;
  }
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
