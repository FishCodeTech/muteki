"use client";

import { useEffect, useRef, useState, type RefObject } from "react";
import { desktopChatBridge, type DesktopReportPointer, type DesktopReportPreviewPayload } from "@/lib/desktopChatBridge";
import { ImageLightbox, type ViewerImage } from "@/components/chat/media/ImageViewer";
import { isRemoteMarkdownImage } from "@/components/chat/markdown/ChatImage";
import { INLINE_IMAGE_SRC } from "@/lib/reportMarkdown";

export function ReportNativePreview({ surfaceId, target, active, text, wide, sendScroll, onScroll, onError }: {
  surfaceId: string;
  target: RefObject<HTMLDivElement | null>;
  active: boolean;
  text: string;
  wide: boolean;
  sendScroll: RefObject<(ratio: number) => void>;
  onScroll: (ratio: number) => void;
  onError: (error: unknown) => void;
}) {
  const latest = useRef({ text, wide, active, onScroll, onError });
  latest.current = { text, wide, active, onScroll, onError };
  const wake = useRef(() => {});
  const [image, setImage] = useState<ViewerImage | null>(null);

  useEffect(() => {
    const bridge = desktopChatBridge();
    const element = target.current;
    if (!element || !bridge?.presentReportPreview || !bridge.hideReportPreview || !bridge.closeReportPreview || !bridge.scrollReportPreview || !bridge.onReportPreview) return;

    let frame = 0;
    let lastRect = "";
    let lastText: string | null = null;
    let lastWide = false;
    let lastTheme = "";
    let revision = 0;
    let rendered = 0;
    let showing = false;
    let stopped = false;
    let focusFrame = 0;
    let wheelFrame = 0;
    let wheelDelta = 0;
    let pointerFrame = 0;
    let pointerStart: { id: number; x: number; y: number } | null = null;
    let pointerMoved = false;
    let pendingPointer: PointerEvent | null = null;
    let desiredScroll: number | null = null;
    let lastScroll = 0;
    let restoreScroll: number | undefined;
    const fail = (error: unknown) => { if (!stopped) latest.current.onError(error); };

    const scroll = () => {
      if (rendered !== revision || desiredScroll === null || !showing) return;
      const ratio = desiredScroll;
      desiredScroll = null;
      void bridge.scrollReportPreview!(surfaceId, ratio).catch(fail);
    };
    sendScroll.current = (ratio) => {
      lastScroll = ratio;
      desiredScroll = ratio;
      scroll();
    };

    const off = bridge.onReportPreview(({ surfaceId: id, event }) => {
      if (id !== surfaceId || stopped) return;
      if (event.type === "rendered") {
        rendered = event.revision;
        if (rendered === revision) element.removeAttribute("aria-busy");
        scroll();
      } else if (event.type === "scroll" && showing && rendered === revision) {
        lastScroll = event.ratio;
        latest.current.onScroll(event.ratio);
      } else if (event.type === "link" && showing) {
        const url = new URL(event.href, window.location.href);
        if (url.protocol !== "http:" && url.protocol !== "https:") return;
        if (event.newWindow || url.origin !== location.origin) {
          void bridge.openExternal?.(url.href).catch((error) => console.warn("desktop.report_preview.link_failed", error));
        }
        else location.assign(url.href);
      } else if (event.type === "image" && showing && rendered === revision) {
        if (INLINE_IMAGE_SRC.test(event.src) || isRemoteMarkdownImage(event.src)) setImage({ src: event.src, name: event.name });
      } else if (event.type === "error") {
        fail(new Error("Native report preview could not load"));
      }
    });

    const update = () => {
      frame = 0;
      const pane = element.getBoundingClientRect();
      const dialogVisible = Array.from(document.querySelectorAll('[role="dialog"][aria-modal="true"]'))
        .some((dialog) => !dialog.contains(element) && dialog.getClientRects().length > 0);
      if (!latest.current.active || document.hidden || dialogVisible || pane.width < 1 || pane.height < 1) {
        const reset = !latest.current.active;
        if (reset) lastScroll = 0;
        if (showing) {
          showing = false;
          lastRect = "";
          if (reset) desiredScroll = null;
          void bridge.hideReportPreview!(surfaceId, reset).catch(fail);
        } else if (reset) {
          desiredScroll = 0;
        }
        return;
      }

      const rect = { x: pane.x, y: pane.y, width: pane.width, height: pane.height };
      const geometry = JSON.stringify(rect);
      const root = document.documentElement;
      const theme = root.dataset.theme === "light" ? "light" : "dark";
      const motion = root.dataset.motion || "system";
      const appearance: Array<[string, string]> = Array.from(root.style)
        .filter((name) => name.startsWith("--"))
        .map((name) => [name, root.style.getPropertyValue(name)]);
      const themeKey = JSON.stringify([theme, motion, root.lang, appearance]);
      const changed = lastText !== latest.current.text || lastWide !== latest.current.wide || lastTheme !== themeKey;
      if (showing && lastRect === geometry && !changed) return;
      showing = true;
      lastRect = geometry;
      const payload: DesktopReportPreviewPayload | undefined = changed
        ? { revision: ++revision, text: latest.current.text, wide: latest.current.wide, theme, motion, lang: root.lang, appearance, restoreScroll }
        : undefined;
      if (payload) element.setAttribute("aria-busy", "true");
      restoreScroll = undefined;
      lastText = latest.current.text;
      lastWide = latest.current.wide;
      lastTheme = themeKey;
      void bridge.presentReportPreview!({ surfaceId, rect, payload }).then(scroll).catch(fail);
    };
    const schedule = () => { if (!frame) frame = requestAnimationFrame(update); };
    const flush = () => {
      if (frame) {
        cancelAnimationFrame(frame);
        frame = 0;
      }
      update();
    };
    const restored = (event: PageTransitionEvent) => {
      if (!event.persisted) return;
      showing = false;
      rendered = 0;
      lastRect = "";
      lastText = null;
      restoreScroll = lastScroll;
      desiredScroll = null;
      schedule();
    };
    const focus = () => {
      if (!showing || rendered !== revision || focusFrame) return;
      focusFrame = requestAnimationFrame(() => {
        focusFrame = 0;
        if (showing) void bridge.focusReportPreview?.(surfaceId).catch(fail);
      });
    };
    const wheel = (event: WheelEvent) => {
      if (!showing || rendered !== revision || event.deltaY === 0 || event.ctrlKey || event.metaKey) return;
      event.preventDefault();
      wheelDelta += event.deltaY * (event.deltaMode === WheelEvent.DOM_DELTA_PAGE ? element.clientHeight : event.deltaMode === WheelEvent.DOM_DELTA_LINE ? 16 : 1);
      if (!wheelFrame) wheelFrame = requestAnimationFrame(() => {
        wheelFrame = 0;
        const delta = wheelDelta;
        wheelDelta = 0;
        if (showing) void bridge.wheelReportPreview?.(surfaceId, delta).catch(fail);
      });
    };
    const sendPointer = (phase: DesktopReportPointer["phase"], event: MouseEvent) => {
      if (!showing || rendered !== revision) return;
      const pane = element.getBoundingClientRect();
      void bridge.pointerReportPreview?.(surfaceId, {
        phase,
        x: event.clientX - pane.x,
        y: event.clientY - pane.y,
        count: event.detail,
        extend: event.shiftKey,
        newWindow: event.metaKey || event.ctrlKey || event.shiftKey,
      }).catch(fail);
    };
    const startPointer = (event: PointerEvent) => {
      if (event.button !== 0 || !showing || rendered !== revision) return;
      pointerStart = { id: event.pointerId, x: event.clientX, y: event.clientY };
      pointerMoved = false;
      focus();
      sendPointer("start", event);
    };
    const movePointer = (event: PointerEvent) => {
      if (pointerStart?.id !== event.pointerId) return;
      if (Math.hypot(event.clientX - pointerStart.x, event.clientY - pointerStart.y) > 3) pointerMoved = true;
      pendingPointer = event;
      if (!pointerFrame) pointerFrame = requestAnimationFrame(() => {
        pointerFrame = 0;
        if (pendingPointer) sendPointer("move", pendingPointer);
      });
    };
    const finishPointer = (event: PointerEvent) => {
      if (pointerStart?.id !== event.pointerId) return;
      pointerStart = null;
      pendingPointer = null;
      cancelAnimationFrame(pointerFrame);
      pointerFrame = 0;
      sendPointer("finish", event);
    };
    const click = (event: MouseEvent) => {
      if (!pointerMoved) sendPointer("click", event);
    };
    wake.current = flush;
    const size = new ResizeObserver(schedule);
    size.observe(element);
    const appearance = new MutationObserver(schedule);
    appearance.observe(document.documentElement, { attributes: true, attributeFilter: ["style", "data-theme", "data-motion", "lang"] });
    const overlays = new MutationObserver(schedule);
    overlays.observe(document.body, { childList: true, subtree: true, attributes: true, attributeFilter: ["aria-modal", "hidden"] });
    element.addEventListener("focus", focus);
    element.addEventListener("pointerdown", startPointer);
    element.addEventListener("click", click);
    element.addEventListener("wheel", wheel, { passive: false });
    window.addEventListener("pointermove", movePointer, true);
    window.addEventListener("pointerup", finishPointer, true);
    window.addEventListener("pointercancel", finishPointer, true);
    window.addEventListener("resize", schedule);
    window.addEventListener("pageshow", restored);
    document.addEventListener("visibilitychange", schedule);
    flush();
    return () => {
      stopped = true;
      cancelAnimationFrame(frame);
      cancelAnimationFrame(focusFrame);
      cancelAnimationFrame(wheelFrame);
      cancelAnimationFrame(pointerFrame);
      size.disconnect();
      appearance.disconnect();
      overlays.disconnect();
      window.removeEventListener("resize", schedule);
      window.removeEventListener("pageshow", restored);
      document.removeEventListener("visibilitychange", schedule);
      element.removeEventListener("focus", focus);
      element.removeEventListener("pointerdown", startPointer);
      element.removeEventListener("click", click);
      element.removeEventListener("wheel", wheel);
      window.removeEventListener("pointermove", movePointer, true);
      window.removeEventListener("pointerup", finishPointer, true);
      window.removeEventListener("pointercancel", finishPointer, true);
      off();
      element.removeAttribute("aria-busy");
      sendScroll.current = () => {};
      void bridge.closeReportPreview!(surfaceId).catch((error) => console.warn("desktop.report_preview.close_failed", error));
    };
  }, [surfaceId, target, sendScroll]);

  useEffect(() => { wake.current(); }, [text, wide, active]);

  return <ImageLightbox images={image ? [image] : []} index={0} open={Boolean(image)} onOpenChange={(open) => { if (!open) setImage(null); }} />;
}
