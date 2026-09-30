const FENCE = /^\s{0,3}(`{3,}|~{3,})/;

/** A lexical Markdown check, excluding fenced and inline code literals. */
export function hasCrossBlockReferences(text: string): boolean {
  let fence: { char: string; size: number } | null = null;
  let inlineTicks = 0;
  for (const line of text.split("\n")) {
    const marker = FENCE.exec(line)?.[1];
    if (marker) {
      if (!fence) fence = { char: marker[0], size: marker.length };
      else if (marker[0] === fence.char && marker.length >= fence.size && !line.trim().replace(/[`~]/g, "")) fence = null;
      continue;
    }
    if (fence || /^ {4}|^\t/.test(line)) continue;
    let visible = "";
    for (let offset = 0; offset < line.length; offset++) {
      if (line[offset] === "\\") { if (!inlineTicks) visible += line[offset] + (line[++offset] || ""); continue; }
      if (line[offset] === "`") {
        let end = offset + 1; while (line[end] === "`") end++;
        const size = end - offset;
        if (!inlineTicks) inlineTicks = size; else if (inlineTicks === size) inlineTicks = 0;
        offset = end - 1; continue;
      }
      if (!inlineTicks) visible += line[offset];
    }
    if (/\[\^[^\]]+\]|^\s{0,3}\[[^\]]+\]:/.test(visible)) return true;
  }
  return false;
}

export function splitMarkdownBlocks(text: string, resolveCrossBlockReferences = false): string[] {
  if (!text) return [];
  // Cross-document relationships are resolved once after streaming finishes.
  if (resolveCrossBlockReferences && hasCrossBlockReferences(text)) return [text];
  const lines = text.split("\n"), blocks: string[] = [];
  let current: string[] = [], fence: { char: string; size: number } | null = null, mathFence = 0;
  let container = "";
  const containerOf = (line: string): string => {
    const ordered = /^\s{0,3}\d+([.)])\s+/.exec(line);
    if (ordered) return `ordered:${ordered[1]}`;
    const unordered = /^\s{0,3}([-+*])\s+/.exec(line);
    if (unordered) return `unordered:${unordered[1]}`;
    return /^\s{0,3}>/.test(line) ? "quote" : "";
  };
  for (let index = 0; index < lines.length; index++) {
    const line = lines[index];
    const math = !fence ? /^\s{0,3}(\${2,})(.*)$/.exec(line) : null;
    if (math) {
      if (!mathFence && !math[2].includes(math[1])) mathFence = math[1].length;
      else if (mathFence && math[1].length >= mathFence && !math[2].trim()) mathFence = 0;
      current.push(line); continue;
    }
    const marker = !mathFence ? FENCE.exec(line)?.[1] : null;
    if (marker) {
      if (!fence) fence = { char: marker[0], size: marker.length };
      else if (marker[0] === fence.char && marker.length >= fence.size && !line.trim().replace(/[`~]/g, "")) fence = null;
      current.push(line); continue;
    }
    if (!fence && !mathFence && !line.trim() && current.length) {
      let next = index + 1; while (next < lines.length && !lines[next].trim()) next++;
      const upcoming = lines[next] || "";
      if (/^\s{2,}\S|^\s*\|/.test(upcoming) || (container && containerOf(upcoming) === container)) { current.push(line); continue; }
      blocks.push(current.join("\n")); current = []; container = ""; continue;
    }
    if (!line.trim() && !current.length) continue;
    if (!fence && !mathFence) container = containerOf(line) || container;
    current.push(line);
  }
  if (current.length) blocks.push(current.join("\n"));
  return blocks;
}
