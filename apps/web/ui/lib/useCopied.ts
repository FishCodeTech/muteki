"use client";
import { useCallback, useEffect, useRef, useState } from "react";

/**
 * Click-to-copy with transient visual feedback. Writes `text` to the clipboard
 * and flips `copied` true for `ms`, so a chip can briefly swap to a "copied"
 * state instead of copying silently (the old behaviour gave no confirmation).
 *
 *   const [copied, copy] = useCopied();
 *   <span onClick={() => copy(cmd)}>{copied ? "已复制" : id}</span>
 */
export function useCopied(ms = 1200): [boolean, (text: string) => void] {
  const [copied, setCopied] = useState(false);
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  useEffect(() => () => { if (timer.current) clearTimeout(timer.current); }, []);
  const copy = useCallback((text: string) => {
    if (!text) return;
    try { navigator.clipboard?.writeText(text); } catch { /* insecure context — best effort */ }
    setCopied(true);
    if (timer.current) clearTimeout(timer.current);
    timer.current = setTimeout(() => setCopied(false), ms);
  }, [ms]);
  return [copied, copy];
}
