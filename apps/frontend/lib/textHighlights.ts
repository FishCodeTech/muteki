type HighlightRegistry = { set(name: string, value: unknown): void; delete(name: string): void };

const owners = new Map<string, Map<string, Range[]>>();

function registry(): { highlights: HighlightRegistry; Highlight: new (...ranges: Range[]) => unknown } | null {
  if (typeof window === "undefined") return null;
  const highlights = (globalThis.CSS as unknown as { highlights?: HighlightRegistry } | undefined)?.highlights;
  const Highlight = (globalThis as unknown as { Highlight?: new (...ranges: Range[]) => unknown }).Highlight;
  return highlights && Highlight ? { highlights, Highlight } : null;
}

export function textHighlightsSupported(): boolean {
  return registry() !== null;
}

export function setTextHighlight(name: string, owner: string, ranges: Range[]): void {
  const byOwner = owners.get(name) || new Map<string, Range[]>();
  if (ranges.length) byOwner.set(owner, ranges);
  else byOwner.delete(owner);
  owners.set(name, byOwner);
  const api = registry();
  if (!api) return;
  const all = Array.from(byOwner.values()).flat();
  if (all.length) api.highlights.set(name, new api.Highlight(...all));
  else api.highlights.delete(name);
}
