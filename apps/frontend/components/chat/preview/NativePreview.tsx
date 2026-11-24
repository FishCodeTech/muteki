"use client";

import { useEffect, useRef } from "react";
import { desktopChatBridge, type DesktopPreviewEvent } from "@/lib/desktopChatBridge";
import { onPaneResizeEnd, paneResizing } from "@/lib/paneResize";

type Hole = { x: number; y: number; width: number; height: number; radius: number };

const transparent = (color: string) => color === "transparent" || /^rgba\(.*,\s*0\)$/.test(color);

function paints(el: Element, style: CSSStyleDeclaration) {
  if (el instanceof HTMLImageElement || el instanceof HTMLCanvasElement || el instanceof HTMLVideoElement || el instanceof SVGSVGElement) return true;
  if (!transparent(style.backgroundColor) || style.backgroundImage !== "none" || style.boxShadow !== "none") return true;
  if (style.backdropFilter && style.backdropFilter !== "none") return true;
  if (parseFloat(style.borderTopWidth) > 0 && !transparent(style.borderTopColor)) return true;
  return Array.from(el.childNodes).some((child) => child.nodeType === Node.TEXT_NODE && Boolean(child.textContent?.trim()));
}

function collect(el: Element, out: DOMRect[], budget: { left: number }) {
  if (--budget.left < 0) return;
  const style = getComputedStyle(el);
  if (style.display === "none" || style.visibility === "hidden" || Number(style.opacity) === 0) return;
  const rect = el.getBoundingClientRect();
  if (rect.width >= 1 && rect.height >= 1 && (paints(el, style) || budget.left === 0)) { out.push(rect); return; }
  for (const child of Array.from(el.children)) collect(child, out, budget);
}

const positioned = (el: Element) => { const position = getComputedStyle(el).position; return position === "absolute" || position === "fixed"; };

function overlaps(target: Element, area: DOMRect) {
  const rect = target.getBoundingClientRect();
  return rect.width >= 1 && rect.height >= 1
    && rect.left < area.right && area.left < rect.right
    && rect.top < area.bottom && area.top < rect.bottom;
}

function sameHoles(a: Hole[], b: Hole[]) {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i += 1) {
    const x = a[i], y = b[i];
    if (x.x !== y.x || x.y !== y.y || x.width !== y.width || x.height !== y.height || x.radius !== y.radius) return false;
  }
  return true;
}

function infiniteAnimation(el: Element) {
  return getComputedStyle(el).animationIterationCount.split(",").some((value) => value.trim() === "infinite");
}

function findHoles(node: HTMLElement, area: DOMRect, remembered: Set<Element>, probe: boolean): Hole[] {
  const boxes: Array<{ rect: DOMRect; el: Element }> = [];
  const add = (el: Element) => {
    const rects: DOMRect[] = [];
    collect(el, rects, { left: 200 });
    for (const rect of rects) boxes.push({ rect, el });
  };
  for (const child of Array.from(document.body.children)) {
    if (child.contains(node) || child instanceof HTMLScriptElement || child instanceof HTMLStyleElement) continue;
    add(child);
  }
  for (let current: Element = node; current.parentElement && current !== document.body; current = current.parentElement) {
    for (const sibling of Array.from(current.parentElement.children)) {
      if (sibling !== current && positioned(sibling)) add(sibling);
    }
  }
  for (const el of Array.from(remembered)) {
    if (!el.isConnected || el.contains(node)) { remembered.delete(el); continue; }
    const before = boxes.length;
    add(el);
    if (boxes.length === before) remembered.delete(el);
  }
  if (probe) {
    const STEP = 24;
    const inBoxes = (x: number, y: number) => boxes.some(({ rect }) => x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom);
    const bottom = Math.min(area.bottom, window.innerHeight), right = Math.min(area.right, window.innerWidth);
    for (let y = Math.max(area.top, 0) + STEP / 2; y < bottom; y += STEP) {
      for (let x = Math.max(area.left, 0) + STEP / 2; x < right; x += STEP) {
        if (inBoxes(x, y)) continue;
        const hit = document.elementFromPoint(x, y);
        if (!hit || hit === node || node.contains(hit)) continue;
        let cover: Element = hit;
        for (let el = hit.parentElement; el && !el.contains(node); el = el.parentElement) {
          const rect = el.getBoundingClientRect();
          if (positioned(el) && x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom) cover = el;
        }
        remembered.add(cover);
        boxes.push({ rect: cover.getBoundingClientRect(), el: cover });
      }
    }
  }
  const holes: Hole[] = [];
  for (const { rect, el } of boxes) {
    const left = Math.max(rect.left, area.left), top = Math.max(rect.top, area.top);
    const width = Math.min(rect.right, area.right) - left, height = Math.min(rect.bottom, area.bottom) - top;
    if (width < 1 || height < 1) continue;
    holes.push({ x: left, y: top, width, height, radius: parseFloat(getComputedStyle(el).borderTopLeftRadius) || 0 });
  }
  return disjoint(holes);
}

const intersects = (a: Hole, b: Hole) => a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height;

function subtract(a: Hole, b: Hole): Hole[] {
  if (!intersects(a, b)) return [a];
  const out: Hole[] = [];
  const top = Math.max(a.y, b.y), bottom = Math.min(a.y + a.height, b.y + b.height);
  if (b.y > a.y) out.push({ x: a.x, y: a.y, width: a.width, height: b.y - a.y, radius: 0 });
  if (b.y + b.height < a.y + a.height) out.push({ x: a.x, y: bottom, width: a.width, height: a.y + a.height - bottom, radius: 0 });
  if (b.x > a.x) out.push({ x: a.x, y: top, width: b.x - a.x, height: bottom - top, radius: 0 });
  if (b.x + b.width < a.x + a.width) out.push({ x: b.x + b.width, y: top, width: a.x + a.width - b.x - b.width, height: bottom - top, radius: 0 });
  return out;
}

function disjoint(holes: Hole[]): Hole[] {
  const out: Hole[] = [];
  for (const hole of holes) {
    const overlapping = holes.some((other) => other !== hole && intersects(hole, other));
    let pieces = [overlapping ? { ...hole, radius: 0 } : hole];
    for (const placed of out) pieces = pieces.flatMap((piece) => subtract(piece, placed));
    out.push(...pieces);
  }
  return out;
}

const PROBE_GAP = 200;

const shownPreviews = new Map<string, string>();

export function shownNativePreview(threadId: string): string | undefined {
  return shownPreviews.get(threadId);
}

export function NativePreview({ surfaceId, threadId, url, active, revision, persistent = false, viewport = null, cornerRadius = 0, onEvent, onShown }: {
  surfaceId: string; threadId: string; url: string; active: boolean; revision: number;
  persistent?: boolean;
  viewport?: { width: number; height: number } | null;
  cornerRadius?: number;
  onEvent: (event: DesktopPreviewEvent) => void;
  onShown?: (id: string) => void;
}) {
  const element = useRef<HTMLDivElement>(null);
  const currentId = useRef("");
  const latest = useRef({ url, active, revision, persistent, viewport, cornerRadius, onEvent, onShown });
  latest.current = { url, active, revision, persistent, viewport, cornerRadius, onEvent, onShown };
  const updateRef = useRef<() => void>(() => {});

  useEffect(() => {
    const bridge = desktopChatBridge();
    if (!bridge?.openPreview || !bridge.closePreview || !bridge.onPreview) {
      latest.current.onEvent({ id: "", threadId, url: latest.current.url, status: "error", message: "desktop.preview.unavailable: 桌面预览传输未配置" });
      return;
    }
    let disposed = false;
    let scheduled = 0;
    let lastRevision = latest.current.revision;
    let requestGeneration = 0;
    let hidden = false;
    let needsShown = true;
    let opening = false;
    const remembered = new Set<Element>();
    let probeDue = false;
    let lastProbe = -Infinity;
    let probeTimer = 0;
    let lastOpened: { x: number; y: number; width: number; height: number; viewportWidth: number; viewportHeight: number; cornerRadius: number; holes: Hole[]; url: string; persistent: boolean } | null = null;
    const pending: DesktopPreviewEvent[] = [];
    const receive = (event: DesktopPreviewEvent) => {
      if (disposed || event.threadId !== threadId) return;
      if (event.status === "pointerdown") {
        const node = element.current;
        if (node) for (const type of ["pointerdown", "mousedown"]) node.dispatchEvent(new (type === "pointerdown" && typeof PointerEvent === "function" ? PointerEvent : MouseEvent)(type, { bubbles: true, cancelable: true, composed: true }));
        return;
      }
      if (opening || !currentId.current) { pending.push(event); return; }
      if (event.id === currentId.current) latest.current.onEvent(event);
    };
    const unsubscribe = bridge.onPreview(receive);
    const update = () => {
      if (disposed) return;
      const node = element.current;
      if (!node) return;
      const state = latest.current;
      const rect = node.getBoundingClientRect();
      const dialogVisible = Array.from(document.querySelectorAll<HTMLElement>('[role="dialog"][aria-modal="true"]'))
        .some((dialog) => dialog.getClientRects().length > 0 && !dialog.contains(node));
      const onScreen = Math.min(rect.right, window.innerWidth) - Math.max(rect.left, 0) >= 1
        && Math.min(rect.bottom, window.innerHeight) - Math.max(rect.top, 0) >= 1;
      if (!state.active || document.hidden || dialogVisible || rect.width < 1 || rect.height < 1 || !onScreen) {
        ++requestGeneration;
        hidden = true;
        needsShown = true;
        opening = false;
        lastOpened = null;
        pending.length = 0;
        if (shownPreviews.get(threadId) === currentId.current) shownPreviews.delete(threadId);
        void bridge.closePreview!({ id: currentId.current || undefined, surfaceId, hide: true }).catch((error) => receive({ id: currentId.current, threadId, url: state.url, status: "error", message: String(error) }));
        return;
      }
      hidden = false;
      const reused = paneResizing() ? lastOpened?.holes : undefined;
      const probe = probeDue && !reused;
      if (probe) { probeDue = false; lastProbe = performance.now(); }
      const holes = reused ?? findHoles(node, rect, remembered, probe);
      const reload = state.revision !== lastRevision;
      lastRevision = state.revision;
      const viewportWidth = state.viewport?.width ?? rect.width;
      const viewportHeight = state.viewport?.height ?? rect.height;
      const nextCornerRadius = state.cornerRadius ?? 0;
      const next = { x: rect.x, y: rect.y, width: rect.width, height: rect.height, viewportWidth, viewportHeight, cornerRadius: nextCornerRadius, holes, url: state.url, persistent: state.persistent };
      if (
        !reload
        && lastOpened
        && lastOpened.x === next.x
        && lastOpened.y === next.y
        && lastOpened.width === next.width
        && lastOpened.height === next.height
        && lastOpened.viewportWidth === next.viewportWidth
        && lastOpened.viewportHeight === next.viewportHeight
        && lastOpened.cornerRadius === next.cornerRadius
        && lastOpened.url === next.url
        && lastOpened.persistent === next.persistent
        && sameHoles(lastOpened.holes, holes)
      ) return;
      lastOpened = next;
      const generation = ++requestGeneration;
      opening = true;
      void bridge.openPreview!({
        surfaceId,
        threadId,
        url: state.url,
        rect: { x: rect.x, y: rect.y, width: rect.width, height: rect.height },
        viewport: state.viewport ? { width: state.viewport.width, height: state.viewport.height } : undefined,
        cornerRadius: nextCornerRadius,
        holes,
        reload,
        persistent: state.persistent,
      }).then((opened) => {
        if (!opened || typeof opened.id !== "string" || !opened.id) throw new Error("desktop.preview.invalid_reply: 预览缺少身份");
        if (disposed) { void bridge.closePreview!({ id: opened.id, surfaceId }).catch((error) => console.error("desktop.preview.cleanup_failed", error)); return; }
        if (generation !== requestGeneration) {
          if (hidden) void bridge.closePreview!({ id: opened.id, surfaceId, hide: true }).catch((error) => console.error("desktop.preview.hide_failed", error));
          return;
        }
        opening = false;
        currentId.current = opened.id;
        shownPreviews.set(threadId, opened.id);
        if (needsShown) {
          needsShown = false;
          latest.current.onShown?.(opened.id);
        }
        for (const event of pending.splice(0)) receive(event);
      }).catch((error) => {
        if (!disposed && generation === requestGeneration) {
          opening = false;
          lastOpened = null;
          pending.length = 0;
          latest.current.onEvent({ id: currentId.current, threadId, url: state.url, status: "error", message: error instanceof Error ? error.message : String(error) });
        }
      });
    };
    const run = () => { scheduled = 0; update(); };
    const schedule = () => {
      if (!scheduled) scheduled = requestAnimationFrame(run);
    };
    const resized = () => {
      cancelAnimationFrame(scheduled);
      run();
    };
    const offResizeEnd = onPaneResizeEnd(schedule);
    updateRef.current = schedule;
    const running = new Map<Element, Set<string>>();
    let tracking = 0;
    let trackUntil = 0;
    const track = () => {
      update();
      tracking = running.size && performance.now() < trackUntil ? requestAnimationFrame(track) : 0;
      if (!tracking) running.clear();
    };
    const LAYOUT = /^(width|height|min-width|max-width|left|right|top|bottom|inset|margin|padding|flex|grid|transform|translate|scale)/;
    const ancestor = (event: TransitionEvent) => {
      const node = element.current;
      const target = event.target;
      if (!node || !(target instanceof Element)) return null;
      if (target.contains(node)) return LAYOUT.test(event.propertyName) ? target : null;
      return LAYOUT.test(event.propertyName) && overlaps(target, node.getBoundingClientRect()) ? target : null;
    };
    const follow = (event: TransitionEvent) => {
      const target = ancestor(event);
      if (!target) return;
      const properties = running.get(target) ?? new Set<string>();
      properties.add(event.propertyName);
      running.set(target, properties);
      trackUntil = performance.now() + 2000;
      if (!tracking) tracking = requestAnimationFrame(track);
    };
    const settle = (event: TransitionEvent) => {
      const target = ancestor(event);
      const properties = target && running.get(target);
      if (!properties) return;
      properties.delete(event.propertyName);
      if (!properties.size) running.delete(target);
      if (!running.size) schedule();
    };
    const animating = (event: AnimationEvent) => {
      const target = event.target;
      const node = element.current;
      if (!(target instanceof Element) || !node || target.contains(node) || node.contains(target)) return;
      if (infiniteAnimation(target) || !overlaps(target, node.getBoundingClientRect())) return;
      const properties = running.get(target) ?? new Set<string>();
      properties.add(`animation:${event.animationName}`);
      running.set(target, properties);
      trackUntil = performance.now() + 2000;
      if (!tracking) tracking = requestAnimationFrame(track);
    };
    const animated = (event: AnimationEvent) => {
      const target = event.target;
      const properties = target instanceof Element ? running.get(target) : undefined;
      if (properties) {
        properties.delete(`animation:${event.animationName}`);
        if (!properties.size) running.delete(target as Element);
      }
      if (properties) schedule();
    };
    const requestProbe = () => {
      const wait = lastProbe + PROBE_GAP - performance.now();
      if (wait > 0) {
        if (!probeTimer) probeTimer = window.setTimeout(() => { probeTimer = 0; requestProbe(); }, wait);
        return;
      }
      probeDue = true;
      schedule();
    };
    const input = () => {
      requestProbe();
      window.setTimeout(requestProbe, 250);
      window.setTimeout(requestProbe, 620);
    };
    document.addEventListener("transitionrun", follow, true);
    document.addEventListener("transitionend", settle, true);
    document.addEventListener("transitioncancel", settle, true);
    document.addEventListener("animationstart", animating, true);
    document.addEventListener("animationend", animated, true);
    document.addEventListener("animationcancel", animated, true);
    for (const type of ["pointerup", "keyup", "focusin"]) document.addEventListener(type, input, true);
    const resize = new ResizeObserver(resized);
    if (element.current) resize.observe(element.current);
    const mutations = new MutationObserver(schedule);
    mutations.observe(document.body, { childList: true, subtree: true, attributes: true, attributeFilter: ["aria-modal", "hidden", "style", "class"] });
    window.addEventListener("resize", schedule);
    window.addEventListener("scroll", schedule, true);
    document.addEventListener("visibilitychange", schedule);
    schedule();
    return () => {
      disposed = true;
      ++requestGeneration;
      cancelAnimationFrame(scheduled);
      cancelAnimationFrame(tracking);
      window.clearTimeout(probeTimer);
      document.removeEventListener("transitionrun", follow, true);
      document.removeEventListener("transitionend", settle, true);
      document.removeEventListener("transitioncancel", settle, true);
      document.removeEventListener("animationstart", animating, true);
      document.removeEventListener("animationend", animated, true);
      document.removeEventListener("animationcancel", animated, true);
      for (const type of ["pointerup", "keyup", "focusin"]) document.removeEventListener(type, input, true);
      resize.disconnect(); mutations.disconnect(); unsubscribe(); offResizeEnd();
      window.removeEventListener("resize", schedule);
      window.removeEventListener("scroll", schedule, true);
      document.removeEventListener("visibilitychange", schedule);
      updateRef.current = () => {};
      if (shownPreviews.get(threadId) === currentId.current) shownPreviews.delete(threadId);
      void bridge.closePreview!({ id: currentId.current || undefined, surfaceId }).catch((error) => console.error("desktop.preview.cleanup_failed", error));
      currentId.current = "";
    };
  }, [surfaceId, threadId]);

  useEffect(() => { updateRef.current(); }, [url, active, revision, persistent, viewport?.width, viewport?.height, cornerRadius]);
  return <div ref={element} className="h-full w-full bg-white" role="region" aria-label="隔离的桌面网页预览" />;
}
