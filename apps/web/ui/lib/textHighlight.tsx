"use client";

import { createContext, type ReactNode } from "react";

/** Deferred lowercase query shared by knowledge rows and agent cards. */
export const HighlightQueryContext = createContext("");

/** True when any part contains `q`; empty `q` matches everything. */
export function matchesSearch(parts: Array<string | undefined>, q: string): boolean {
  return !q || parts.filter(Boolean).join(" ").toLowerCase().includes(q);
}

/** Wrap every case-insensitive occurrence of `q` in `<mark>`. */
export function highlight(text: string, q: string): ReactNode {
  if (!q || !text) return text;
  const lower = text.toLowerCase();
  const nodes: ReactNode[] = [];
  let cursor = 0;
  let from = lower.indexOf(q);
  let key = 0;
  while (from >= 0) {
    if (from > cursor) nodes.push(text.slice(cursor, from));
    nodes.push(<mark key={key}>{text.slice(from, from + q.length)}</mark>);
    key += 1;
    cursor = from + q.length;
    from = lower.indexOf(q, cursor);
  }
  if (cursor === 0) return text;
  if (cursor < text.length) nodes.push(text.slice(cursor));
  return nodes;
}
