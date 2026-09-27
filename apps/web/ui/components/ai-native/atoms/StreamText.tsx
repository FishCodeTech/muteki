"use client";

/* ─────────────────────────────────────────────────────────
 * STREAM TEXT — plain-text stream with a trailing caret.
 *
 * Originally adapted from https://github.com/TurboKach/ai-native-react-components
 * (MIT, pinned 05dab2d2b5f1f3e40029776e339a486d70491079).
 * ───────────────────────────────────────────────────────── */

import { useEffect, useRef } from "react";

export function StreamText({
  text,
  isStreaming = false,
  onProgress,
  onDone,
}: {
  text: string;
  isStreaming?: boolean;
  onProgress?: () => void;
  onDone?: () => void;
}) {
  const progressFrame = useRef(0);

  useEffect(() => {
    if (!isStreaming) onDone?.();
  }, [isStreaming, onDone]);

  useEffect(() => {
    if (!isStreaming || !onProgress) return;
    cancelAnimationFrame(progressFrame.current);
    progressFrame.current = requestAnimationFrame(onProgress);
    return () => cancelAnimationFrame(progressFrame.current);
  }, [isStreaming, onProgress, text]);

  return (
    <span className="whitespace-pre-wrap break-words">
      <span>{text}</span>
      {isStreaming ? <span className="cx-caret" aria-hidden /> : null}
    </span>
  );
}
