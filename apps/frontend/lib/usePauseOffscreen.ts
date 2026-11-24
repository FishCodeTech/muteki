"use client";

import { useEffect, type RefObject } from "react";

export function usePauseOffscreen(ref: RefObject<HTMLElement | null>): void {
  useEffect(() => {
    const el = ref.current;
    if (!el || typeof IntersectionObserver !== "function") return;

    const observer = new IntersectionObserver(
      (entries) => {
        for (const entry of entries) {
          el.dataset.offscreen = entry.isIntersecting ? "false" : "true";
        }
      },
      { threshold: 0 },
    );

    observer.observe(el);
    return () => {
      observer.disconnect();
      delete el.dataset.offscreen;
    };
  }, [ref]);
}
