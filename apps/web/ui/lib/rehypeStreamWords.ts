/**
 * Streaming word reveal: wraps each word of the prose in a span so a word
 * that arrives fades in through the accent hues, the way a live answer reads.
 * Spans keep their position as text grows, so React keeps the words already
 * on screen and only new ones animate. Code, math and SVG stay untouched.
 */

interface HastText {
  type: "text";
  value: string;
}

interface HastElement {
  type: "element";
  tagName: string;
  properties?: Record<string, unknown>;
  children: HastNode[];
}

type HastNode = HastText | HastElement | { type: string; children?: HastNode[] };

const SKIP_TAGS = new Set(["code", "pre", "svg", "math", "script", "style", "kbd"]);
const CJK = /[\u2e80-\u9fff\uf900-\ufaff\uac00-\ud7af]/;

let segmenter: Intl.Segmenter | null | undefined;

function wordSegmenter(): Intl.Segmenter | null {
  if (segmenter === undefined) {
    try {
      segmenter = typeof Intl !== "undefined" && "Segmenter" in Intl ? new Intl.Segmenter("zh", { granularity: "word" }) : null;
    } catch {
      segmenter = null;
    }
  }
  return segmenter;
}

/** A run of CJK text cut into words; punctuation rides on the word before it. */
function cjkWords(chunk: string): string[] {
  const out: string[] = [];
  const seg = wordSegmenter();
  const parts = seg
    ? Array.from(seg.segment(chunk), (part) => ({ text: part.segment, word: part.isWordLike !== false }))
    : chunk.split(/(?=[\u2e80-\u9fff\uf900-\ufaff\uac00-\ud7af])/).map((text) => ({ text, word: CJK.test(text) }));
  for (const part of parts) {
    if (!part.word && out.length) out[out.length - 1] += part.text;
    else out.push(part.text);
  }
  return out;
}

/**
 * Reveal units: whitespace runs, then words. Latin text splits on spaces
 * only, so "instead," and "Ctrl-C" stay one word; CJK runs, which have no
 * spaces, split into dictionary words.
 */
export function streamTokens(text: string): string[] {
  const out: string[] = [];
  for (const chunk of text.split(/(\s+)/)) {
    if (!chunk) continue;
    if (!chunk.trim() || !CJK.test(chunk)) out.push(chunk);
    else out.push(...cjkWords(chunk));
  }
  return out;
}

const BOUNDARY_WINDOW = 48;
/** How far a `[` looks for its `]` before it is treated as plain text. */
const LINK_LABEL_LIMIT = 160;

/**
 * If `[from, end)` opens a Markdown link or image, the end of that link,
 * so its label lands whole and its address, which renders as nothing, never
 * holds the reveal. `text.length` while the link is still arriving.
 */
function linkEnd(text: string, from: number, end: number): number {
  const open = text.indexOf("[", from);
  if (open < 0 || open >= end) return end;
  const close = text.indexOf("]", open + 1);
  if (close < 0) return text.length - open <= LINK_LABEL_LIMIT ? text.length : end;
  if (close - open > LINK_LABEL_LIMIT) return end;
  if (close + 1 >= text.length) return text.length;
  if (text[close + 1] !== "(") return Math.max(end, close + 1);
  const target = text.indexOf(")", close + 2);
  if (target < 0) return text.length;
  let at = target + 1;
  while (at < text.length && /\s/.test(text[at])) at += 1;
  return Math.max(end, at);
}

const TABLE_DELIMITER = /^\s*\|?[\s:|-]*$/;

function lineAt(text: string, start: number): string {
  const end = text.indexOf("\n", start);
  return text.slice(start, end < 0 ? text.length : end);
}

function isTableRow(line: string): boolean {
  return line.trimStart().startsWith("|");
}

/**
 * A table's header row and its delimiter row each land as one unit: the
 * header is shown as a table as soon as its row is complete (see
 * `closeOpenTable`), and the delimiter, which renders as nothing, never holds
 * the reveal. `text.length` while the row is still arriving.
 */
function tableRowEnd(text: string, from: number, end: number): number {
  const lineStart = text.lastIndexOf("\n", from - 1) + 1;
  if (text.slice(lineStart, from).trim() || text[from] !== "|") return end;
  const previousStart = lineStart > 0 ? text.lastIndexOf("\n", lineStart - 2) + 1 : -1;
  const previous = previousStart >= 0 ? lineAt(text, previousStart) : "";
  const beforeStart = previousStart > 0 ? text.lastIndexOf("\n", previousStart - 2) + 1 : -1;
  const before = beforeStart >= 0 ? lineAt(text, beforeStart) : "";
  const header = !isTableRow(previous);
  const delimiter = isTableRow(previous) && !isTableRow(before) && TABLE_DELIMITER.test(lineAt(text, lineStart));
  if (!header && !delimiter) return end;
  const lineEnd = text.indexOf("\n", from);
  return lineEnd < 0 ? text.length : lineEnd + 1;
}

/** Shown text that ends on a complete table header, or partway into its delimiter, still renders as a table. */
export function closeOpenTable(text: string): string {
  const body = text.endsWith("\n") ? text.slice(0, -1) : text;
  const lines = body.split("\n");
  const last = lines[lines.length - 1] ?? "";
  const partialDelimiter = lines.length >= 2 && TABLE_DELIMITER.test(last) && isTableRow(lines[lines.length - 2]) && !isTableRow(lines[lines.length - 3] ?? "");
  const headIndex = partialDelimiter ? lines.length - 2 : lines.length - 1;
  const head = lines[headIndex] ?? "";
  if (!isTableRow(head) || !head.trimEnd().endsWith("|") || isTableRow(lines[headIndex - 1] ?? "")) return text;
  if (!partialDelimiter && !text.endsWith("\n")) return text;
  const columns = Math.max(1, head.trim().slice(1, -1).split("|").length);
  const delimiter = `|${" --- |".repeat(columns)}`;
  return `${lines.slice(0, headIndex + 1).join("\n")}\n${delimiter}`;
}

/** Shown text that stops inside a link address still renders as that link. */
export function closeOpenLink(text: string): string {
  return /\[[^\][\n]*\]\([^)\s]*$/.test(text) ? `${text})` : text;
}

/** True when `text` ends inside a link address, which renders as nothing and need not wait. */
export function endsInLinkTarget(text: string): boolean {
  return /\[[^\][\n]*\]\([^)\s]*$/.test(text);
}

/**
 * End of the next word at or after `from`, trailing whitespace included,
 * cut the same way the reveal spans are, so a paced reveal lands on whole
 * words instead of growing a word one character at a time. A Markdown link
 * counts as one word.
 */
export function nextWordEnd(text: string, from: number): number {
  if (from >= text.length) return text.length;
  let window = BOUNDARY_WINDOW;
  for (;;) {
    const slice = text.slice(from, from + window);
    const parts = streamTokens(slice);
    let at = from;
    let index = 0;
    while (index < parts.length && !parts[index].trim()) at += parts[index++].length;
    if (index < parts.length) at += parts[index++].length;
    // A word longer than the window (a long URL) would otherwise land in pieces.
    if (index >= parts.length && slice.length === window && from + window < text.length) {
      window *= 4;
      continue;
    }
    while (index < parts.length && !parts[index].trim()) at += parts[index++].length;
    return Math.min(text.length, tableRowEnd(text, from, linkEnd(text, from, at)));
  }
}

/** Characters of `[from, end)` that show on screen: a link address costs nothing. */
export function visibleLength(text: string, from: number, end: number): number {
  return text.slice(from, end).replace(/\]\([^)]*(\)|$)/g, "]").replace(/^\s*\|?[\s:|-]*$/gm, "").length;
}
function classList(element: HastElement): string[] {
  const value = element.properties?.className;
  return Array.isArray(value) ? value.map(String) : typeof value === "string" ? value.split(/\s+/) : [];
}

function skipped(element: HastElement): boolean {
  return SKIP_TAGS.has(element.tagName) || classList(element).some((name) => name.includes("math") || name.includes("katex"));
}

function wrapWords(value: string): HastNode[] {
  const out: HastNode[] = [];
  for (const segment of streamTokens(value)) {
    if (!segment) continue;
    if (!segment.trim()) {
      out.push({ type: "text", value: segment });
      continue;
    }
    out.push({ type: "element", tagName: "span", properties: { className: ["cx-stream-word"] }, children: [{ type: "text", value: segment }] });
  }
  return out;
}

function walk(node: HastNode): void {
  const children = (node as { children?: HastNode[] }).children;
  if (!children) return;
  const next: HastNode[] = [];
  for (const child of children) {
    if (child.type === "text") {
      next.push(...wrapWords((child as HastText).value));
    } else {
      if (child.type !== "element" || !skipped(child as HastElement)) walk(child);
      next.push(child);
    }
  }
  (node as { children: HastNode[] }).children = next;
}

export function rehypeStreamWords() {
  return (tree: HastNode) => {
    walk(tree);
  };
}
