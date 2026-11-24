
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
const STREAM_WORD_WINDOW = 24;

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
const LINK_LABEL_LIMIT = 160;

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

export function closeOpenTable(text: string): string {
  const body = text.endsWith("\n") ? text.slice(0, -1) : text;
  const lastBreak = body.lastIndexOf("\n");
  const previousBreak = lastBreak > 0 ? body.lastIndexOf("\n", lastBreak - 1) : -1;
  const beforeBreak = previousBreak > 0 ? body.lastIndexOf("\n", previousBreak - 1) : -1;
  const last = body.slice(lastBreak + 1);
  const previous = lastBreak < 0 ? "" : body.slice(previousBreak + 1, lastBreak);
  const before = previousBreak < 0 ? "" : body.slice(beforeBreak + 1, previousBreak);
  const partialDelimiter = TABLE_DELIMITER.test(last) && isTableRow(previous) && !isTableRow(before);
  const head = partialDelimiter ? previous : last;
  if (!isTableRow(head) || !head.trimEnd().endsWith("|") || isTableRow(partialDelimiter ? before : previous)) return text;
  if (!partialDelimiter && !text.endsWith("\n")) return text;
  const columns = Math.max(1, head.trim().slice(1, -1).split("|").length);
  const delimiter = `|${" --- |".repeat(columns)}`;
  return `${partialDelimiter ? body.slice(0, lastBreak) : body}\n${delimiter}`;
}

export function closeOpenLink(text: string): string {
  return endsInLinkTarget(text) ? `${text})` : text;
}

export function endsInLinkTarget(text: string): boolean {
  const lastLine = text.slice(text.lastIndexOf("\n") + 1);
  return /\[[^\][\n]*\]\([^)\s]*$/.test(lastLine);
}

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
    if (index >= parts.length && slice.length === window && from + window < text.length) {
      window *= 4;
      continue;
    }
    while (index < parts.length && !parts[index].trim()) at += parts[index++].length;
    return Math.min(text.length, tableRowEnd(text, from, linkEnd(text, from, at)));
  }
}

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

function wrapWords(value: string, remember: (word: HastElement) => void): HastNode[] {
  const out: HastNode[] = [];
  for (const segment of streamTokens(value)) {
    if (!segment) continue;
    if (!segment.trim()) {
      out.push({ type: "text", value: segment });
      continue;
    }
    const word: HastElement = { type: "element", tagName: "span", children: [{ type: "text", value: segment }] };
    remember(word);
    out.push(word);
  }
  return out;
}

function walk(node: HastNode, remember: (word: HastElement) => void): void {
  const children = (node as { children?: HastNode[] }).children;
  if (!children) return;
  const next: HastNode[] = [];
  for (const child of children) {
    if (child.type === "text") {
      next.push(...wrapWords((child as HastText).value, remember));
    } else {
      if (child.type !== "element" || !skipped(child as HastElement)) walk(child, remember);
      next.push(child);
    }
  }
  (node as { children: HastNode[] }).children = next;
}

export function rehypeStreamWords() {
  return (tree: HastNode) => {
    const recent: HastElement[] = [];
    let index = 0;
    walk(tree, word => {
      recent[index % STREAM_WORD_WINDOW] = word;
      index += 1;
    });
    for (const word of recent) word.properties = { className: ["cx-stream-word"] };
  };
}
