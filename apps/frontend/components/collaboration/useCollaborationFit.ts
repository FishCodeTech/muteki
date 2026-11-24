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

const FIT_PAD = 0.14;
const FIT_BOTTOM_INSET = "150px";
export const FIT_VIEW_OPTIONS = { padding: { top: FIT_PAD, bottom: FIT_BOTTOM_INSET, left: FIT_PAD, right: FIT_PAD }, minZoom: FIT_MIN_ZOOM, maxZoom: 1.3 } as const;
const OVERLAY_FIT_GAP = 16;

function overlayFitPadding(
  overlay: boolean,
  indexOpen: boolean,
  detailOpen: boolean,
  containerWidth: number,
): NonNullable<FitViewOptions["padding"]> {
  if (!overlay || (!indexOpen && !detailOpen)) return { top: FIT_PAD, bottom: FIT_BOTTOM_INSET, left: FIT_PAD, right: FIT_PAD };
  return {
    top: FIT_PAD,
    bottom: FIT_BOTTOM_INSET,
    left: indexOpen ? `${clampCollabPanelWidth(containerWidth, COLLAB_INDEX_W) + OVERLAY_FIT_GAP}px` : FIT_PAD,
    right: detailOpen ? `${clampCollabPanelWidth(containerWidth, COLLAB_DETAIL_W) + OVERLAY_FIT_GAP}px` : FIT_PAD,
  };
}

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
  const overlayFit = useRef({ overlay, indexOpen, detailOpen, containerWidth });
  overlayFit.current = { overlay, indexOpen, detailOpen, containerWidth };
  const overlayToggleSeen = useRef(false);

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


  useEffect(() => {
    if (!overlayToggleSeen.current) {
      overlayToggleSeen.current = true;
      return;
    }
    if (!overlay) return;
    requestFit();
  }, [detailOpen, indexOpen, overlay, requestFit]);

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
