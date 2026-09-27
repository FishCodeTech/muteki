"use client";

import { useCallback, useRef, useState } from "react";

function maxTs<T extends { ts: number }>(items: T[]): number {
  let max = 0;
  for (const item of items) if (item.ts > max) max = item.ts;
  return max;
}

/**
 * Watermark for a newest-first feed: items with ts > lastSeenTs are fresh.
 * Seeded to the opening snapshot so the first paint is not all-fresh.
 * Scroll-to-top, already-at-top arrivals, jump, or markSeen advance the watermark.
 */
export function useFeedUnread<T extends { ts: number }>(items: T[], resetKey: string) {
  const listRef = useRef<HTMLDivElement>(null);
  const itemsRef = useRef(items);
  itemsRef.current = items;

  const seed = useRef({ key: resetKey, ready: items.length > 0 });
  const [lastSeenTs, setLastSeenTs] = useState(() => maxTs(items));
  const [atTop, setAtTop] = useState(true);

  if (seed.current.key !== resetKey) {
    seed.current = { key: resetKey, ready: items.length > 0 };
    setLastSeenTs(maxTs(items));
    setAtTop(true);
  } else if (!seed.current.ready && items.length) {
    seed.current.ready = true;
    setLastSeenTs(maxTs(items));
  } else if (atTop) {
    const next = maxTs(items);
    if (next > lastSeenTs) setLastSeenTs(next);
  }

  const markSeen = useCallback(() => {
    setLastSeenTs((prev) => {
      const next = maxTs(itemsRef.current);
      return next > prev ? next : prev;
    });
  }, []);

  const onScroll = useCallback(() => {
    const node = listRef.current;
    if (!node) return;
    const top = node.scrollTop <= 8;
    setAtTop(top);
    if (top) markSeen();
  }, [markSeen]);

  const jumpToLatest = useCallback(() => {
    listRef.current?.scrollTo({ top: 0 });
    setAtTop(true);
    markSeen();
  }, [markSeen]);

  let freshCount = 0;
  for (const item of items) if (item.ts > lastSeenTs) freshCount += 1;

  return {
    listRef,
    lastSeenTs,
    freshCount,
    showJump: !atTop && freshCount > 0,
    onScroll,
    jumpToLatest,
    markSeen,
  };
}
