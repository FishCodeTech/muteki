"use client";

import { useCallback, useLayoutEffect, useState, type RefObject } from "react";

import {
  COLLAB_MOBILE_MAX,
  COLLAB_OVERLAY_MAX,
  MINIMAP,
} from "@/lib/agentCollaborationLayout";
import { onPaneResizeEnd, paneResizing } from "@/lib/paneResize";

export type MobileView = "index" | "canvas" | "detail";
export type CollabLayoutMode = "wide" | "overlay" | "mobile";
export const MOBILE_VIEWS: MobileView[] = ["index", "canvas", "detail"];

export function modeFromWidth(width: number): CollabLayoutMode {
  if (width <= COLLAB_MOBILE_MAX) return "mobile";
  if (width <= COLLAB_OVERLAY_MAX) return "overlay";
  return "wide";
}

export function useCollaborationLayout(
  nodeCount: number,
  shellRef: RefObject<HTMLElement | null>,
  initial?: { indexOpen?: boolean; detailOpen?: boolean },
) {
  const [mode, setMode] = useState<CollabLayoutMode>("wide");
  const [shellWidth, setShellWidth] = useState(0);
  const [indexOpen, setIndexOpen] = useState(() => initial?.indexOpen ?? true);
  const [detailOpen, setDetailOpen] = useState(() => initial?.detailOpen ?? true);
  const [mobileView, setMobileView] = useState<MobileView>("canvas");
  const [miniMapPref, setMiniMapPref] = useState<boolean | null>(null);
  const [legendOpen, setLegendOpen] = useState(false);

  useLayoutEffect(() => {
    const element = shellRef.current;
    if (!element) return;
    let last: CollabLayoutMode = "wide";
    const apply = (width: number) => {
      if (!paneResizing()) setShellWidth(width);
      const next = modeFromWidth(width);
      if (next === last) return;
      last = next;
      setMode(next);
      setIndexOpen(next === "wide");
      setDetailOpen(next === "wide");
    };
    apply(element.getBoundingClientRect().width);
    const observer = new ResizeObserver((entries) => {
      apply(entries[0]?.contentRect.width ?? element.getBoundingClientRect().width);
    });
    observer.observe(element);
    const offResizeEnd = onPaneResizeEnd(() => apply(element.getBoundingClientRect().width));
    return () => { observer.disconnect(); offResizeEnd(); };
  }, [shellRef]);

  const overlay = mode === "overlay";
  const mobile = mode === "mobile";

  const openDetail = useCallback((view?: MobileView) => {
    setDetailOpen(true);
    if (overlay) setIndexOpen(false);
    if (mobile) setMobileView(view ?? "detail");
  }, [mobile, overlay]);

  const toggleIndex = useCallback(() => {
    setIndexOpen((open) => {
      const next = !open;
      if (next && overlay) setDetailOpen(false);
      return next;
    });
  }, [overlay]);

  const toggleDetail = useCallback(() => {
    setDetailOpen((open) => {
      const next = !open;
      if (next && overlay) setIndexOpen(false);
      return next;
    });
  }, [overlay]);

  const closeOverlays = useCallback(() => {
    setIndexOpen(false);
    setDetailOpen(false);
  }, []);

  const miniMapOpen = miniMapPref ?? nodeCount >= MINIMAP.autoShowNodes;
  const toggleMiniMap = useCallback(() => setMiniMapPref(!miniMapOpen), [miniMapOpen]);
  const toggleLegend = useCallback(() => setLegendOpen((open) => !open), []);

  return {
    mode,
    overlay,
    mobile,
    shellWidth,
    indexOpen,
    setIndexOpen,
    detailOpen,
    setDetailOpen,
    mobileView,
    setMobileView,
    openDetail,
    toggleIndex,
    toggleDetail,
    closeOverlays,
    miniMapOpen,
    toggleMiniMap,
    legendOpen,
    toggleLegend,
  };
}
