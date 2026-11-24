"use client";

import { useCallback, useEffect, useRef, useState, type KeyboardEvent, type PointerEvent } from "react";
import { setPaneResizing } from "@/lib/paneResize";

export const COLLAB_SIDEBAR_MIN = 220;
export const COLLAB_SIDEBAR_MAX = 520;
export const COLLAB_INDEX_WIDTH_DEFAULT = 286;
export const COLLAB_DETAIL_WIDTH_DEFAULT = 372;
export const COLLAB_INDEX_WIDTH_KEY = "muteki.collab.indexWidth";
export const COLLAB_DETAIL_WIDTH_KEY = "muteki.collab.detailWidth";

export function clampCollabSidebar(width: number): number {
  return Math.round(Math.min(COLLAB_SIDEBAR_MAX, Math.max(COLLAB_SIDEBAR_MIN, width)));
}

function readStoredWidth(key: string): number | null {
  try {
    const raw = window.localStorage.getItem(key);
    if (raw == null) return null;
    const parsed = Number(raw);
    return Number.isFinite(parsed) ? clampCollabSidebar(parsed) : null;
  } catch {
    return null;
  }
}

function writeStoredWidth(key: string, width: number): void {
  try {
    window.localStorage.setItem(key, String(width));
  } catch {
  }
}

function readCssWidth(el: HTMLElement, property: string, fallback: number): number {
  const parsed = parseFloat(getComputedStyle(el).getPropertyValue(property));
  return Number.isFinite(parsed) && parsed > 0 ? parsed : fallback;
}

function currentWidth(stored: number | null, shell: HTMLElement | null, property: string, fallback: number): number {
  return stored ?? (shell ? readCssWidth(shell, property, fallback) : fallback);
}

export function useCollaborationSidebarResize(options?: {
  onCollapseIndex?: () => void;
  onCollapseDetail?: () => void;
}) {
  const [indexWidth, setIndexWidth] = useState<number | null>(null);
  const [detailWidth, setDetailWidth] = useState<number | null>(null);
  const [resizing, setResizing] = useState(false);
  const cleanupRef = useRef<(() => void) | null>(null);
  const collapseRef = useRef(options);
  collapseRef.current = options;

  useEffect(() => () => cleanupRef.current?.(), []);

  useEffect(() => {
    setIndexWidth(readStoredWidth(COLLAB_INDEX_WIDTH_KEY));
    setDetailWidth(readStoredWidth(COLLAB_DETAIL_WIDTH_KEY));
  }, []);

  useEffect(() => {
    if (indexWidth == null) return;
    writeStoredWidth(COLLAB_INDEX_WIDTH_KEY, indexWidth);
  }, [indexWidth]);

  useEffect(() => {
    if (detailWidth == null) return;
    writeStoredWidth(COLLAB_DETAIL_WIDTH_KEY, detailWidth);
  }, [detailWidth]);

  const applyIndex = useCallback((width: number) => {
    setIndexWidth(clampCollabSidebar(width));
  }, []);
  const applyDetail = useCallback((width: number) => {
    setDetailWidth(clampCollabSidebar(width));
  }, []);

  const startDrag = useCallback((
    event: PointerEvent<HTMLDivElement>,
    shell: HTMLElement | null,
    origin: number,
    sign: 1 | -1,
    openVar: "--collab-index-open-w" | "--collab-detail-open-w",
    commit: (width: number) => void,
    collapse: (() => void) | undefined,
  ) => {
    event.preventDefault();
    event.currentTarget.setPointerCapture?.(event.pointerId);
    cleanupRef.current?.();
    const startX = event.clientX;
    setResizing(true);
    setPaneResizing(true);
    let pendingRaw: number | null = origin;
    let lastRaw = origin;
    let frame = 0;
    const flush = () => {
      frame = 0;
      if (pendingRaw == null) return;
      const raw = pendingRaw;
      pendingRaw = null;
      lastRaw = raw;
      const shown = Math.max(0, Math.min(COLLAB_SIDEBAR_MAX, raw));
      shell?.style.setProperty(openVar, `${shown}px`);
    };
    flush();
    const move = (pointer: globalThis.PointerEvent) => {
      pendingRaw = Math.round(origin + sign * (pointer.clientX - startX));
      if (!frame) frame = requestAnimationFrame(flush);
    };
    const stop = () => {
      cleanupRef.current = null;
      cancelAnimationFrame(frame);
      flush();
      setResizing(false);
      setPaneResizing(false);
      if (lastRaw < 140 && collapse) {
        shell?.style.setProperty(openVar, `${clampCollabSidebar(origin)}px`);
        collapse();
      } else {
        const next = clampCollabSidebar(lastRaw);
        shell?.style.setProperty(openVar, `${next}px`);
        commit(next);
      }
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", stop);
      window.removeEventListener("pointercancel", stop);
    };
    cleanupRef.current = stop;
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", stop);
    window.addEventListener("pointercancel", stop);
  }, []);

  const startIndexResize = useCallback((event: PointerEvent<HTMLDivElement>, shell: HTMLElement | null) => {
    const origin = currentWidth(indexWidth, shell, "--collab-index-w", COLLAB_INDEX_WIDTH_DEFAULT);
    startDrag(event, shell, origin, 1, "--collab-index-open-w", applyIndex, collapseRef.current?.onCollapseIndex);
  }, [applyIndex, indexWidth, startDrag]);

  const startDetailResize = useCallback((event: PointerEvent<HTMLDivElement>, shell: HTMLElement | null) => {
    const origin = currentWidth(detailWidth, shell, "--collab-detail-w", COLLAB_DETAIL_WIDTH_DEFAULT);
    startDrag(event, shell, origin, -1, "--collab-detail-open-w", applyDetail, collapseRef.current?.onCollapseDetail);
  }, [applyDetail, detailWidth, startDrag]);

  const onIndexResizeKey = useCallback((event: KeyboardEvent<HTMLDivElement>, shell: HTMLElement | null) => {
    const origin = currentWidth(indexWidth, shell, "--collab-index-w", COLLAB_INDEX_WIDTH_DEFAULT);
    const step = event.shiftKey ? 32 : 12;
    if (event.key === "ArrowRight") { event.preventDefault(); applyIndex(origin + step); }
    if (event.key === "ArrowLeft") { event.preventDefault(); applyIndex(origin - step); }
    if (event.key === "Home") { event.preventDefault(); applyIndex(COLLAB_SIDEBAR_MIN); }
    if (event.key === "End") { event.preventDefault(); applyIndex(COLLAB_SIDEBAR_MAX); }
    if (event.key === "Enter") { event.preventDefault(); applyIndex(COLLAB_INDEX_WIDTH_DEFAULT); }
  }, [applyIndex, indexWidth]);

  const onDetailResizeKey = useCallback((event: KeyboardEvent<HTMLDivElement>, shell: HTMLElement | null) => {
    const origin = currentWidth(detailWidth, shell, "--collab-detail-w", COLLAB_DETAIL_WIDTH_DEFAULT);
    const step = event.shiftKey ? 32 : 12;
    if (event.key === "ArrowLeft") { event.preventDefault(); applyDetail(origin + step); }
    if (event.key === "ArrowRight") { event.preventDefault(); applyDetail(origin - step); }
    if (event.key === "Home") { event.preventDefault(); applyDetail(COLLAB_SIDEBAR_MIN); }
    if (event.key === "End") { event.preventDefault(); applyDetail(COLLAB_SIDEBAR_MAX); }
    if (event.key === "Enter") { event.preventDefault(); applyDetail(COLLAB_DETAIL_WIDTH_DEFAULT); }
  }, [applyDetail, detailWidth]);

  return {
    indexWidth,
    detailWidth,
    resizing,
    startIndexResize,
    startDetailResize,
    onIndexResizeKey,
    onDetailResizeKey,
    resetIndex: () => applyIndex(COLLAB_INDEX_WIDTH_DEFAULT),
    resetDetail: () => applyDetail(COLLAB_DETAIL_WIDTH_DEFAULT),
  };
}
