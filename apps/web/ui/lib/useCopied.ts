"use client";
import { useCallback, useEffect, useRef, useState } from "react";
import { copyToClipboard } from "@/lib/clipboard";

/**
 * Click-to-copy with transient visual feedback. Flips `copied` true only after
 * the browser accepts the write (async clipboard or textarea fallback), so a chip
 * never says "copied" when the clipboard write was actually rejected.
 *
 *   const [copied, copy] = useCopied();
 *   <span onClick={() => copy(cmd)}>{copied ? "已复制" : id}</span>
 */
export function useCopied(ms = 1200): [boolean, (text: string) => void] {
  const [copied, setCopied] = useState(false);
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const mounted = useRef(true);
  useEffect(() => () => {
    mounted.current = false;
    if (timer.current) clearTimeout(timer.current);
  }, []);
  const copy = useCallback((text: string) => {
    if (!text) return;
    void copyToClipboard(text).then((ok) => {
      if (!ok || !mounted.current) return;
      setCopied(true);
      if (timer.current) clearTimeout(timer.current);
      timer.current = setTimeout(() => setCopied(false), ms);
    });
  }, [ms]);
  return [copied, copy];
}
