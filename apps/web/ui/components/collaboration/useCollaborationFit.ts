"use client";

import { useReactFlow, type FitViewOptions } from "@xyflow/react";
import { useCallback, useEffect, useRef, useState, type RefObject } from "react";

import {
  COLLAB_DETAIL_W,
  COLLAB_INDEX_W,
  FIT_DELAY,
  FIT_MIN_ZOOM,
  MOTION,
  ZOOM,
  clampCollabPanelWidth,
} from "@/lib/agentCollaborationLayout";

export const FIT_VIEW_OPTIONS = { padding: 0.14, minZoom: FIT_MIN_ZOOM, maxZoom: ZOOM.fitMax } as const;

const FIT_PAD = 0.14;
const OVERLAY_FIT_GAP = 16;

function overlayFitPadding(
  overlay: boolean,
  indexOpen: boolean,
  detailOpen: boolean,
  containerWidth: number,
): NonNullable<FitViewOptions["padding"]> {
  if (!overlay || (!indexOpen && !detailOpen)) return FIT_PAD;
  return {
    top: FIT_PAD,
    bottom: FIT_PAD,
    left: indexOpen ? `${clampCollabPanelWidth(containerWidth, COLLAB_INDEX_W) + OVERLAY_FIT_GAP}px` : FIT_PAD,
    right: detailOpen ? `${clampCollabPanelWidth(containerWidth, COLLAB_DETAIL_W) + OVERLAY_FIT_GAP}px` : FIT_PAD,
  };
}

/**
 * initial → the first frame fit is ReactFlow's own `fitView` prop; requests are ignored.
 * idle    → nothing queued.
 * pending → a fit runs on the next animation frame once the nodes are measured.
 */
type FitPhase = "initial" | "idle" | "pending";

export function useCollaborationFit({
  flowReady,
  nodesInitialized,
  nodeCount,
  reduceMotion,
  containerRef,
  skipInitialFit = false,
  overlay = false,
  indexOpen = false,
  detailOpen = false,
  containerWidth = 0,
}: {
  flowReady: boolean;
  nodesInitialized: boolean;
  nodeCount: number;
  reduceMotion: boolean;
  containerRef: RefObject<HTMLElement | null>;
  skipInitialFit?: boolean;
  overlay?: boolean;
  indexOpen?: boolean;
  detailOpen?: boolean;
  containerWidth?: number;
}) {
  const { fitView } = useReactFlow();
  const [phase, setPhase] = useState<FitPhase>(skipInitialFit ? "idle" : "initial");
  // Padding is read at fit time so a queued frame sees the drawers as they are.
  const overlayFit = useRef({ overlay, indexOpen, detailOpen, containerWidth });
  overlayFit.current = { overlay, indexOpen, detailOpen, containerWidth };
  const overlayToggleSeen = useRef(false);

  // Several requests in one render collapse into a single pending fit.
  const requestFit = useCallback(() => {
    setPhase((current) => (current === "initial" ? current : "pending"));
  }, []);

  useEffect(() => {
    if (phase !== "initial" || !flowReady || !nodesInitialized || nodeCount === 0) return;
    setPhase("idle");
  }, [flowReady, nodeCount, nodesInitialized, phase]);

  useEffect(() => {
    if (phase !== "pending" || !flowReady || !nodesInitialized || nodeCount === 0) return;
    const frame = requestAnimationFrame(() => {
      setPhase("idle");
      const pad = overlayFit.current;
      void fitView({
        ...FIT_VIEW_OPTIONS,
        minZoom: pad.containerWidth > 0 && pad.containerWidth <= 560 ? ZOOM.min : FIT_MIN_ZOOM,
        padding: overlayFitPadding(pad.overlay, pad.indexOpen, pad.detailOpen, pad.containerWidth),
        duration: reduceMotion ? 0 : MOTION.fit,
      });
    });
    return () => cancelAnimationFrame(frame);
  }, [fitView, flowReady, nodeCount, nodesInitialized, phase, reduceMotion]);

  // Node-count changes (new workers) do not re-frame. Follow / outside-view
  // handling lives on the canvas so a manual viewport is not overwritten.

  // Overlay drawers do not resize the canvas. After mount, toggling them
  // re-fits with the padding above so the graph sits in the visible gap.
  useEffect(() => {
    if (!overlayToggleSeen.current) {
      overlayToggleSeen.current = true;
      return;
    }
    if (!overlay) return;
    requestFit();
  }, [detailOpen, indexOpen, overlay, requestFit]);

  // The canvas box changes size while the runtime panel slides open and when
  // docked side panels toggle; fit again once the size has settled. The
  // observer's first notification only reports the mount size and is skipped.
  useEffect(() => {
    const element = containerRef.current;
    if (!element || typeof ResizeObserver === "undefined") return;
    let timer = 0;
    let mounted = false;
    const observer = new ResizeObserver(() => {
      if (!mounted) {
        mounted = true;
        return;
      }
      window.clearTimeout(timer);
      timer = window.setTimeout(requestFit, FIT_DELAY.resizeSettle);
    });
    observer.observe(element);
    return () => {
      window.clearTimeout(timer);
      observer.disconnect();
    };
  }, [containerRef, requestFit]);

  return { requestFit };
}
