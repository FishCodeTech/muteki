"use client";

import { useCallback, useEffect, useState, type KeyboardEvent, type PointerEvent } from "react";

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
    /* storage unavailable */
  }
}

function readCssWidth(el: HTMLElement, property: string, fallback: number): number {
  const parsed = parseFloat(getComputedStyle(el).getPropertyValue(property));
  return Number.isFinite(parsed) && parsed > 0 ? parsed : fallback;
}

function currentWidth(stored: number | null, shell: HTMLElement | null, property: string, fallback: number): number {
  return stored ?? (shell ? readCssWidth(shell, property, fallback) : fallback);
}

export function useCollaborationSidebarResize() {
  const [indexWidth, setIndexWidth] = useState<number | null>(null);
  const [detailWidth, setDetailWidth] = useState<number | null>(null);
  const [resizing, setResizing] = useState(false);

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

  const startIndexResize = useCallback((event: PointerEvent<HTMLDivElement>, shell: HTMLElement | null) => {
    event.preventDefault();
    const origin = currentWidth(indexWidth, shell, "--collab-index-w", COLLAB_INDEX_WIDTH_DEFAULT);
    const startX = event.clientX;
    setResizing(true);
    document.body.classList.add("collab-sidebar-resizing");
    const move = (pointer: globalThis.PointerEvent) => applyIndex(origin + (pointer.clientX - startX));
    const stop = () => {
      setResizing(false);
      document.body.classList.remove("collab-sidebar-resizing");
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", stop);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", stop);
    applyIndex(origin);
  }, [applyIndex, indexWidth]);

  const startDetailResize = useCallback((event: PointerEvent<HTMLDivElement>, shell: HTMLElement | null) => {
    event.preventDefault();
    const origin = currentWidth(detailWidth, shell, "--collab-detail-w", COLLAB_DETAIL_WIDTH_DEFAULT);
    const startX = event.clientX;
    setResizing(true);
    document.body.classList.add("collab-sidebar-resizing");
    const move = (pointer: globalThis.PointerEvent) => applyDetail(origin + (startX - pointer.clientX));
    const stop = () => {
      setResizing(false);
      document.body.classList.remove("collab-sidebar-resizing");
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", stop);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", stop);
    applyDetail(origin);
  }, [applyDetail, detailWidth]);

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
