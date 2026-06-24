"use client";

import { RefObject, useEffect, useRef } from "react";
import { animate, stagger } from "animejs";

type AnimeInstance = { revert?: () => void; cancel?: () => void };

type DeckMotionState = {
  runId: string;
  started: boolean;
  loading: boolean;
  artifactOpen: boolean;
  artifactView: string;
  chatLength: number;
  flagCount: number;
  cost: number;
};

function reducedMotion(): boolean {
  return typeof window !== "undefined"
    && window.matchMedia?.("(prefers-reduced-motion: reduce)").matches;
}

function scoped(root: HTMLElement, selector: string): HTMLElement[] {
  return Array.from(root.querySelectorAll<HTMLElement>(selector));
}

function stop(instances: AnimeInstance[]) {
  for (const instance of instances) {
    instance.revert?.();
    instance.cancel?.();
  }
}

function run(targets: HTMLElement[], params: Parameters<typeof animate>[1]): AnimeInstance | null {
  if (!targets.length) return null;
  return animate(targets, params) as AnimeInstance;
}

export function useDeckMotion(rootRef: RefObject<HTMLElement | null>, state: DeckMotionState) {
  const active = useRef<AnimeInstance[]>([]);
  const lastRun = useRef("");
  const lastArtifact = useRef("");
  const lastChatLength = useRef(0);
  const lastFlags = useRef(0);
  const lastCost = useRef(0);

  useEffect(() => () => stop(active.current), []);

  useEffect(() => {
    const root = rootRef.current;
    if (!root || reducedMotion()) return;
    stop(active.current);

    const shellPieces = scoped(root, ".motion-shell-piece");
    const railRows = scoped(root, ".motion-rail-item").slice(0, 10);
    const panels = scoped(root, ".motion-panel-btn");

    active.current = [
      run(shellPieces, {
        opacity: [0, 1],
        translateY: [10, 0],
        duration: 420,
        delay: stagger(48),
        ease: "outQuart",
        composition: "blend",
      }),
      run(railRows, {
        opacity: [0, 1],
        translateX: [-10, 0],
        duration: 320,
        delay: stagger(22),
        ease: "outQuart",
        composition: "blend",
      }),
      run(panels, {
        opacity: [0, 1],
        translateY: [6, 0],
        duration: 280,
        delay: stagger(18),
        ease: "outQuart",
        composition: "blend",
      }),
    ].filter((instance): instance is AnimeInstance => !!instance);
    lastRun.current = state.runId;
    lastArtifact.current = `${state.artifactOpen ? "open" : "closed"}:${state.artifactView}`;
    lastChatLength.current = state.chatLength;
    lastFlags.current = state.flagCount;
    lastCost.current = state.cost;
    // Mount choreography only. Runtime effects below own later state changes.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [rootRef]);

  useEffect(() => {
    const root = rootRef.current;
    if (!root || reducedMotion() || !state.runId || !state.started || state.loading || state.runId === lastRun.current) return;
    lastRun.current = state.runId;
    const targets = scoped(root, ".motion-run-enter");
    const instance = run(targets, {
      opacity: [0, 1],
      translateY: [12, 0],
      duration: 360,
      delay: stagger(42),
      ease: "outQuart",
      composition: "blend",
    });
    if (instance) active.current.push(instance);
  }, [rootRef, state.runId, state.started, state.loading]);

  useEffect(() => {
    const root = rootRef.current;
    const key = `${state.artifactOpen ? "open" : "closed"}:${state.artifactView}`;
    if (!root || reducedMotion() || key === lastArtifact.current) return;
    lastArtifact.current = key;
    const targets = scoped(root, state.artifactOpen ? ".motion-artifact" : ".motion-inspector");
    const instance = run(targets, {
      opacity: [0, 1],
      translateX: state.artifactOpen ? [14, 0] : [8, 0],
      duration: 260,
      ease: "outQuart",
      composition: "blend",
    });
    if (instance) active.current.push(instance);
  }, [rootRef, state.artifactOpen, state.artifactView]);

  useEffect(() => {
    const root = rootRef.current;
    if (!root || reducedMotion() || state.chatLength <= lastChatLength.current) {
      lastChatLength.current = state.chatLength;
      return;
    }
    lastChatLength.current = state.chatLength;
    const bubbles = scoped(root, ".coord-bubble").slice(-2);
    const instance = run(bubbles, {
      opacity: [0, 1],
      translateY: [8, 0],
      duration: 240,
      delay: stagger(26),
      ease: "outQuart",
      composition: "blend",
    });
    if (instance) active.current.push(instance);
  }, [rootRef, state.chatLength]);

  useEffect(() => {
    const root = rootRef.current;
    const changed = state.flagCount !== lastFlags.current || state.cost !== lastCost.current;
    if (!root || reducedMotion() || !changed) {
      lastFlags.current = state.flagCount;
      lastCost.current = state.cost;
      return;
    }
    lastFlags.current = state.flagCount;
    lastCost.current = state.cost;
    const targets = scoped(root, ".motion-feedback");
    const instance = run(targets, {
      scale: [1, 1.018, 1],
      duration: 360,
      delay: stagger(18),
      ease: "outQuart",
      composition: "blend",
    });
    if (instance) active.current.push(instance);
  }, [rootRef, state.flagCount, state.cost]);
}
