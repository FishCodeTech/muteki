export function sourceSelectionOffsets(input: { source: string; rendered: string; selected: string; renderedStart: number; renderedEnd: number; sourceBase?: number }): { startOffset: number; endOffset: number } | null {
  const { source, rendered, selected, renderedStart, renderedEnd } = input;
  if (!selected || rendered.slice(renderedStart, renderedEnd) !== selected) return null;
  const occurrences = (value: string): number[] => {
    const offsets: number[] = [];
    let offset = value.indexOf(selected);
    while (offset >= 0) { offsets.push(offset); offset = value.indexOf(selected, offset + 1); }
    return offsets;
  };
  const renderedOccurrences = occurrences(rendered), sourceOccurrences = occurrences(source);
  const occurrence = renderedOccurrences.indexOf(renderedStart);
  if (occurrence < 0 || renderedOccurrences.length !== sourceOccurrences.length) return null;
  const startOffset = sourceOccurrences[occurrence] + (input.sourceBase || 0);
  return { startOffset, endOffset: startOffset + selected.length };
}

/** The service slices Python strings, so persisted locators use code points. */
export function sourceCodePointOffsets(source: string, offsets: { startOffset: number; endOffset: number }): { startOffset: number; endOffset: number } {
  return { startOffset: Array.from(source.slice(0, offsets.startOffset)).length, endOffset: Array.from(source.slice(0, offsets.endOffset)).length };
}

/** Range boundaries, not text.indexOf, identify the selected occurrence. */
export function messageRangeOffsets(root: HTMLElement, range: Range, sourceText: string): { startOffset: number; endOffset: number; text: string } | null {
  if (!root.contains(range.startContainer) || !root.contains(range.endContainer)) return null;
  const elementFor = (node: Node) => node.nodeType === Node.ELEMENT_NODE ? node as Element : node.parentElement;
  const startElement = elementFor(range.startContainer), endElement = elementFor(range.endContainer);
  if (startElement?.closest("button,input,textarea") || endElement?.closest("button,input,textarea")) return null;
  let container: HTMLElement = root, sourceStart = 0, sourceEnd = sourceText.length;
  const startScope = startElement?.closest<HTMLElement>("[data-md-local-start][data-md-local-end]");
  const endScope = endElement?.closest<HTMLElement>("[data-md-local-start][data-md-local-end]");
  if (startScope && startScope === endScope) {
    const block = startScope.closest<HTMLElement>("[data-md-source-start]");
    const base = Number(block?.dataset.mdSourceStart);
    const low = Number(startScope.dataset.mdLocalStart), high = Number(startScope.dataset.mdLocalEnd);
    if (!Number.isSafeInteger(base) || base < 0 || !Number.isSafeInteger(low) || !Number.isSafeInteger(high) || low < 0 || high < low || base + high > sourceText.length) return null;
    container = startScope; sourceStart = base + low; sourceEnd = base + high;
  }
  const all = range.cloneRange(); all.selectNodeContents(container);
  const before = range.cloneRange(); before.selectNodeContents(container); before.setEnd(range.startContainer, range.startOffset);
  const selected = range.toString(), rendered = all.toString();
  const renderedStart = before.toString().length;
  // Cross-paragraph or transformed selections without source provenance are
  // accepted only when the rendered document is exactly the source document.
  if (container === root && rendered !== sourceText) {
    const first = sourceText.indexOf(selected);
    if (first < 0 || sourceText.indexOf(selected, first + 1) >= 0) return null;
    return { startOffset: first, endOffset: first + selected.length, text: selected };
  }
  const result = sourceSelectionOffsets({ source: sourceText.slice(sourceStart, sourceEnd), rendered, selected, renderedStart, renderedEnd: renderedStart + selected.length, sourceBase: sourceStart });
  return result ? { ...result, text: selected } : null;
}
