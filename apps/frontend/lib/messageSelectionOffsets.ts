export function sourceCodePointOffsets(source: string, offsets: { startOffset: number; endOffset: number }): { startOffset: number; endOffset: number } {
  return { startOffset: Array.from(source.slice(0, offsets.startOffset)).length, endOffset: Array.from(source.slice(0, offsets.endOffset)).length };
}

function textNodeBoundaries(raw: string, rendered: string, inlineCode: boolean): number[] | null {
  const shifted = (at: number) => Array.from({ length: rendered.length + 1 }, (_, index) => at + index);
  if (raw === rendered) return shifted(0);
  if (inlineCode) {
    const at = raw.indexOf(rendered);
    if (at >= 0 && /^`+\s*$/.test(raw.slice(0, at)) && /^\s*`+$/.test(raw.slice(at + rendered.length))) return shifted(at);
    return null;
  }
  let decoded = "";
  const boundaries = [0];
  for (let at = 0; at < raw.length;) {
    const start = at;
    let value = String.fromCodePoint(raw.codePointAt(at)!);
    at += value.length;
    if (value === "\\" && /[!-/:-@[-`{-~]/.test(raw[at] || "")) value = raw[at++];
    else if (value === "&") {
      const entity = /^&(?:#[xX][\da-fA-F]+|#\d+|[a-zA-Z][\da-zA-Z]*);/.exec(raw.slice(start))?.[0];
      if (entity) {
        const decoder = document.createElement("textarea");
        decoder.innerHTML = entity;
        value = decoder.value;
        at = start + entity.length;
      }
    } else if (value === "\r" && raw[at] === "\n") { value = "\n"; at++; }
    decoded += value;
    for (let index = 1; index <= value.length; index++) boundaries.push(index === value.length ? at : start + index);
  }
  return decoded === rendered ? boundaries : null;
}

function textNodeBoundary(raw: string, rendered: string, offset: number, inlineCode: boolean): number | null {
  return textNodeBoundaries(raw, rendered, inlineCode)?.[offset] ?? null;
}

function selectableLeaves(root: HTMLElement): HTMLElement[] {
  return Array.from(root.querySelectorAll<HTMLElement>("[data-md-text-start], [data-code-start]"))
    .filter((leaf) => !leaf.querySelector("[data-md-text-start], [data-code-start]") && leaf.dataset.codeLength !== "0");
}

function leafSourceSpan(leaf: HTMLElement, sourceText: string): { low: number; high: number } | null {
  const block = leaf.closest<HTMLElement>("[data-md-source-start]");
  const base = Number(block?.dataset.mdSourceStart);
  if (!block || !Number.isSafeInteger(base) || base < 0) return null;
  let low: number, high: number;
  if (leaf.hasAttribute("data-code-start")) {
    const scope = leaf.closest<HTMLElement>("[data-md-code]");
    if (!scope) return null;
    const scopeStart = base + Number(scope.dataset.mdLocalStart);
    const raw = sourceText.slice(scopeStart, base + Number(scope.dataset.mdLocalEnd));
    const code = scope.dataset.mdCode || "";
    const fence = /^[ \t]*(?:`{3,}|~{3,})[^\n]*\n/.exec(raw);
    const codeStart = fence ? fence[0].length : raw.indexOf(code);
    if (!code || codeStart < 0 || raw.slice(codeStart, codeStart + code.length) !== code) return null;
    low = scopeStart + codeStart + Number(leaf.dataset.codeStart);
    high = low + (leaf.textContent || "").length;
  } else {
    low = base + Number(leaf.dataset.mdTextStart);
    high = base + Number(leaf.dataset.mdTextEnd);
  }
  if (!Number.isSafeInteger(low) || !Number.isSafeInteger(high) || low < 0 || high > sourceText.length || low > high) return null;
  return { low, high };
}

function textPosition(element: Node, offset: number): { node: Node; offset: number } {
  const walker = document.createTreeWalker(element, NodeFilter.SHOW_TEXT);
  let remaining = offset;
  let last: Text | null = null;
  for (let node = walker.nextNode() as Text | null; node; node = walker.nextNode() as Text | null) {
    if (remaining <= node.data.length) return { node, offset: remaining };
    remaining -= node.data.length;
    last = node;
  }
  return last ? { node: last, offset: last.data.length } : { node: element, offset: element.childNodes.length };
}

export function messageOffsetsToRange(root: HTMLElement, sourceText: string, startOffset: number, endOffset: number): Range | null {
  if (!(endOffset > startOffset) || startOffset < 0) return null;
  const range = document.createRange();
  const rawView = root.querySelector<HTMLElement>("[data-message-source]");
  if (rawView) {
    const start = textPosition(rawView, startOffset), end = textPosition(rawView, endOffset);
    range.setStart(start.node, start.offset);
    range.setEnd(end.node, end.offset);
    return range.collapsed ? null : range;
  }
  let start: { node: Node; offset: number } | null = null;
  let end: { node: Node; offset: number } | null = null;
  for (const leaf of selectableLeaves(root)) {
    const span = leafSourceSpan(leaf, sourceText);
    if (!span || span.high <= startOffset || span.low >= endOffset) continue;
    const rendered = leaf.textContent || "";
    const map = textNodeBoundaries(sourceText.slice(span.low, span.high), rendered, leaf.tagName === "CODE");
    if (!map) continue;
    if (!start) {
      let at = 0;
      while (at < rendered.length && span.low + map[at] < startOffset) at++;
      start = textPosition(leaf, at);
    }
    let at = rendered.length;
    while (at > 0 && span.low + map[at] > endOffset) at--;
    end = textPosition(leaf, at);
  }
  if (!start || !end) return null;
  range.setStart(start.node, start.offset);
  range.setEnd(end.node, end.offset);
  return range.collapsed ? null : range;
}

export function displayOffsetsForLocator(
  text: string,
  sources: Array<{ messageId: string; start: number; end: number; sourceStart?: number }> | undefined,
  messageId: string,
  startOffset: number,
  endOffset: number,
): { startOffset: number; endOffset: number } | null {
  const units = (value: string, codePoints: number) => {
    let out = 0, count = 0;
    for (const char of value) {
      if (count >= codePoints) break;
      out += char.length;
      count++;
    }
    return count === codePoints ? out : null;
  };
  if (!sources) {
    const start = units(text, startOffset), end = units(text, endOffset);
    return start === null || end === null ? null : { startOffset: start, endOffset: end };
  }
  for (const span of sources) {
    const base = span.sourceStart || 0;
    if (span.messageId !== messageId || startOffset < base) continue;
    const tail = text.slice(span.start);
    const start = units(tail, startOffset - base), end = units(tail, endOffset - base);
    if (start === null || end === null || span.start + end > span.end) continue;
    return { startOffset: span.start + start, endOffset: span.start + end };
  }
  return null;
}

export function messageRangeOffsets(root: HTMLElement, range: Range, sourceText: string): { startOffset: number; endOffset: number; text: string } | null {
  if (!root.contains(range.startContainer) || !root.contains(range.endContainer)) return null;
  const elementFor = (node: Node) => node.nodeType === Node.ELEMENT_NODE ? node as Element : node.parentElement;
  const startElement = elementFor(range.startContainer), endElement = elementFor(range.endContainer);
  if (startElement?.closest("button,input,textarea") || endElement?.closest("button,input,textarea")) return null;
  const rawView = startElement?.closest<HTMLElement>("[data-message-source]");
  if (rawView && rawView === endElement?.closest("[data-message-source]")) {
    const before = range.cloneRange(); before.selectNodeContents(rawView); before.setEnd(range.startContainer, range.startOffset);
    const startOffset = before.toString().length;
    const text = range.toString();
    return { startOffset, endOffset: startOffset + text.length, text };
  }
  const boundaries: Array<{ start: number; end: number; leaf: HTMLElement }> = [];
  for (const leaf of selectableLeaves(root)) {
    if (!range.intersectsNode(leaf)) continue;
    const rendered = leaf.textContent || "";
    const offsetWithin = (node: Node, offset: number, fallback: number) => {
      if (!leaf.contains(node)) return fallback;
      const before = document.createRange();
      before.selectNodeContents(leaf); before.setEnd(node, offset);
      return before.toString().length;
    };
    const start = offsetWithin(range.startContainer, range.startOffset, 0);
    const end = offsetWithin(range.endContainer, range.endOffset, rendered.length);
    if (end <= start) continue;
    const span = leafSourceSpan(leaf, sourceText);
    if (!span) return null;
    const { low, high } = span;
    const raw = sourceText.slice(low, high);
    const sourceStart = textNodeBoundary(raw, rendered, start, leaf.tagName === "CODE");
    const sourceEnd = textNodeBoundary(raw, rendered, end, leaf.tagName === "CODE");
    if (sourceStart === null || sourceEnd === null) return null;
    boundaries.push({ start: low + sourceStart, end: low + sourceEnd, leaf });
  }
  if (boundaries.length) {
    const first = boundaries[0], last = boundaries.at(-1)!;
    const hasText = (range: Range) => {
      const fragment = range.cloneContents();
      fragment.querySelectorAll(".select-none, [aria-hidden='true'], [data-code-length='0']").forEach(node => node.remove());
      return Boolean(fragment.textContent?.trim());
    };
    if (!first.leaf.contains(range.startContainer)) {
      const before = range.cloneRange(); before.setEnd(first.leaf, 0);
      if (hasText(before)) return null;
    }
    if (!last.leaf.contains(range.endContainer)) {
      const after = range.cloneRange(); after.setStart(last.leaf, last.leaf.childNodes.length);
      if (hasText(after)) return null;
    }
    const startOffset = first.start, endOffset = last.end;
    return { startOffset, endOffset, text: sourceText.slice(startOffset, endOffset) };
  }
  return null;
}
