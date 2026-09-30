"use client";

import { useEffect, useRef } from "react";
import { desktopChatBridge, type DesktopPreviewEvent } from "@/lib/desktopChatBridge";

/** Native preview has an isolated browser process; it never shares the chat document. */
export function NativePreview({ surfaceId, threadId, url, active, revision, onEvent }: {
  surfaceId: string; threadId: string; url: string; active: boolean; revision: number;
  onEvent: (event: DesktopPreviewEvent) => void;
}) {
  const element = useRef<HTMLDivElement>(null);
  const currentId = useRef("");
  const latest = useRef({ url, active, revision, onEvent });
  latest.current = { url, active, revision, onEvent };
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
    let opening = false;
    const pending: DesktopPreviewEvent[] = [];
    const receive = (event: DesktopPreviewEvent) => {
      if (disposed || event.threadId !== threadId) return;
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
      if (!state.active || document.hidden || dialogVisible || rect.width < 1 || rect.height < 1) {
        ++requestGeneration;
        hidden = true;
        opening = false;
        pending.length = 0;
        void bridge.closePreview!({ id: currentId.current || undefined, surfaceId, hide: true }).catch((error) => receive({ id: currentId.current, threadId, url: state.url, status: "error", message: String(error) }));
        return;
      }
      hidden = false;
      const generation = ++requestGeneration;
      opening = true;
      const reload = state.revision !== lastRevision;
      lastRevision = state.revision;
      void bridge.openPreview!({ surfaceId, threadId, url: state.url, rect: { x: rect.x, y: rect.y, width: rect.width, height: rect.height }, reload }).then((opened) => {
        if (!opened || typeof opened.id !== "string" || !opened.id) throw new Error("desktop.preview.invalid_reply: 预览缺少身份");
        if (disposed) { void bridge.closePreview!({ id: opened.id, surfaceId }).catch((error) => console.error("desktop.preview.cleanup_failed", error)); return; }
        if (generation !== requestGeneration) {
          if (hidden) void bridge.closePreview!({ id: opened.id, surfaceId, hide: true }).catch((error) => console.error("desktop.preview.hide_failed", error));
          return;
        }
        opening = false;
        currentId.current = opened.id;
        for (const event of pending.splice(0)) receive(event);
      }).catch((error) => {
        if (!disposed && generation === requestGeneration) {
          opening = false;
          pending.length = 0;
          latest.current.onEvent({ id: currentId.current, threadId, url: state.url, status: "error", message: error instanceof Error ? error.message : String(error) });
        }
      });
    };
    const schedule = () => {
      cancelAnimationFrame(scheduled);
      scheduled = requestAnimationFrame(update);
    };
    updateRef.current = schedule;
    const resize = new ResizeObserver(schedule);
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
      resize.disconnect(); mutations.disconnect(); unsubscribe();
      window.removeEventListener("resize", schedule);
      window.removeEventListener("scroll", schedule, true);
      document.removeEventListener("visibilitychange", schedule);
      updateRef.current = () => {};
      void bridge.closePreview!({ id: currentId.current || undefined, surfaceId }).catch((error) => console.error("desktop.preview.cleanup_failed", error));
      currentId.current = "";
    };
  }, [surfaceId, threadId]);

  useEffect(() => { updateRef.current(); }, [url, active, revision]);
  return <div ref={element} className="h-full w-full bg-white" role="region" aria-label="隔离的桌面网页预览" />;
}
