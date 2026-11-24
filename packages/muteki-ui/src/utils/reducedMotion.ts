"use client";

import { useSyncExternalStore } from "react";

const QUERY = "(prefers-reduced-motion: reduce)";

function subscribe(onChange: () => void): () => void {
  if (typeof window === "undefined") return () => {};
  const media = window.matchMedia(QUERY);
  media.addEventListener("change", onChange);
  const observer = new MutationObserver(onChange);
  observer.observe(document.documentElement, {
    attributes: true,
    attributeFilter: ["data-motion"],
  });
  window.addEventListener("muteki:motion-change", onChange);
  return () => {
    media.removeEventListener("change", onChange);
    observer.disconnect();
    window.removeEventListener("muteki:motion-change", onChange);
  };
}

export function prefersReducedMotion(): boolean {
  if (typeof window === "undefined") return false;
  return (
    document.documentElement.dataset.motion === "reduce" ||
    window.matchMedia(QUERY).matches
  );
}

const readServerPreference = () => false;

/** Honors both OS `prefers-reduced-motion` and `<html data-motion="reduce">`. */
export function useReducedMotion(): boolean {
  return useSyncExternalStore(subscribe, prefersReducedMotion, readServerPreference);
}

export { useReducedMotion as usePrefersReducedMotion };
