"use client";

import { useLayoutEffect, useRef, useState, type MouseEvent } from "react";
import { ReportPreviewContent } from "@/components/reports/ReportPreviewContent";
import type { DesktopReportPointer, DesktopReportPreviewPayload } from "@/lib/desktopChatBridge";
import { runWithoutThemeTransitions } from "@/lib/themeTransition";

type ReportWindow = Window & {
  webkit?: { messageHandlers?: { mutekiReport?: { postMessage: (message: string) => void } } };
  __mutekiReceiveReport?: (payload: DesktopReportPreviewPayload) => void;
  __mutekiClearReport?: () => void;
  __mutekiScrollReport?: (ratio: number) => void;
  __mutekiWheelReport?: (delta: number) => void;
  __mutekiFocusReport?: () => void;
  __mutekiPointerReport?: (pointer: DesktopReportPointer) => void;
};

function publish(event: object) {
  (window as ReportWindow).webkit?.messageHandlers?.mutekiReport?.postMessage(JSON.stringify(event));
}

function followLink(link: HTMLAnchorElement, newWindow: boolean) {
  const href = link.getAttribute("href");
  if (!href) return;
  if (href.startsWith("#")) {
    document.getElementById(href.slice(1))?.scrollIntoView();
  } else {
    publish({ type: "link", href, newWindow: newWindow || link.target === "_blank" });
  }
}

function expandImage(src: string, name: string) {
  publish({ type: "image", src, name });
}

export default function NativeReportPreviewPage() {
  const [payload, setPayload] = useState<DesktopReportPreviewPayload | null>(null);
  const element = useRef<HTMLDivElement>(null);
  const assignedScroll = useRef<number | null>(null);

  useLayoutEffect(() => {
    const host = window as ReportWindow;
    if (!host.webkit?.messageHandlers?.mutekiReport) return;
    let anchor: { node: Node; offset: number } | null = null;
    host.__mutekiReceiveReport = (next) => {
      anchor = null;
      const root = document.documentElement;
      const themeChanged =
        root.dataset.theme !== next.theme
        || root.classList.contains("dark") !== (next.theme === "dark")
        || root.classList.contains("light") !== (next.theme === "light");
      const commit = () => {
        root.dataset.theme = next.theme;
        root.dataset.motion = next.motion;
        root.classList.toggle("dark", next.theme === "dark");
        root.classList.toggle("light", next.theme === "light");
        root.lang = next.lang;
        for (const [token, value] of next.appearance) root.style.setProperty(token, value);
      };
      if (themeChanged) runWithoutThemeTransitions(commit, root);
      else commit();
      setPayload(next);
    };
    host.__mutekiScrollReport = (ratio) => {
      const scroller = element.current;
      if (!scroller) return;
      scroller.scrollTop = ratio * (scroller.scrollHeight - scroller.clientHeight);
      assignedScroll.current = scroller.scrollTop;
    };
    host.__mutekiWheelReport = (delta) => element.current?.scrollBy({ top: delta });
    host.__mutekiFocusReport = () => element.current?.focus({ preventScroll: true });
    host.__mutekiPointerReport = ({ phase, x, y, count, extend, newWindow }) => {
      const scroller = element.current;
      const selection = window.getSelection();
      if (!scroller || !selection) return;
      const px = Math.max(0, Math.min(x, scroller.clientWidth - 1));
      const py = Math.max(0, Math.min(y, scroller.clientHeight - 1));
      if (phase === "click") {
        const target = document.elementFromPoint(px, py);
        if (count && count >= 3) {
          const block = target?.closest("p, li, h1, h2, h3, h4, h5, h6, pre, td, th");
          if (block) selection.selectAllChildren(block);
        } else if (count === 2) {
          selection.modify("move", "backward", "word");
          selection.modify("extend", "forward", "word");
        } else {
          const control = target?.closest<HTMLAnchorElement | HTMLButtonElement>("a[href], button:enabled");
          if (control instanceof HTMLAnchorElement) followLink(control, Boolean(newWindow));
          else control?.click();
        }
        return;
      }

      const caret = document.caretRangeFromPoint(px, py);
      if (!caret || !scroller.contains(caret.startContainer)) return;
      if (phase === "start") {
        anchor = extend && selection.anchorNode
          ? { node: selection.anchorNode, offset: selection.anchorOffset }
          : { node: caret.startContainer, offset: caret.startOffset };
      }
      if (anchor?.node.isConnected) {
        selection.setBaseAndExtent(anchor.node, anchor.offset, caret.startContainer, caret.startOffset);
      }
      if (phase === "finish") anchor = null;
    };
    host.__mutekiClearReport = () => {
      anchor = null;
      assignedScroll.current = null;
      if (element.current) element.current.scrollTop = 0;
      setPayload(null);
    };
    publish({ type: "ready" });
    return () => {
      delete host.__mutekiReceiveReport;
      delete host.__mutekiClearReport;
      delete host.__mutekiScrollReport;
      delete host.__mutekiWheelReport;
      delete host.__mutekiFocusReport;
      delete host.__mutekiPointerReport;
    };
  }, []);

  useLayoutEffect(() => {
    if (!payload || !element.current) return;
    const scroller = element.current;
    const height = scroller.scrollHeight;
    if (payload.restoreScroll !== undefined) {
      scroller.scrollTop = payload.restoreScroll * (height - scroller.clientHeight);
      assignedScroll.current = scroller.scrollTop;
    }
    publish({ type: "rendered", revision: payload.revision, height });
  }, [payload]);

  const handleLink = (event: MouseEvent) => {
    if (!(event.target instanceof Element)) return;
    const link = event.target.closest<HTMLAnchorElement>("a[href]");
    if (!link) return;
    event.preventDefault();
    followLink(link, event.metaKey || event.ctrlKey || event.shiftKey);
  };

  return (
    <div
      ref={element}
      tabIndex={0}
      className="cx-scroll h-dvh w-full overflow-y-auto bg-cx-bg"
      onClickCapture={handleLink}
      onScroll={(event) => {
        const scroller = event.currentTarget;
        if (scroller.scrollTop === assignedScroll.current) {
          assignedScroll.current = null;
          return;
        }
        assignedScroll.current = null;
        const max = scroller.scrollHeight - scroller.clientHeight;
        publish({ type: "scroll", ratio: max > 0 ? Math.max(0, Math.min(1, scroller.scrollTop / max)) : 0 });
      }}
    >
      {payload && <ReportPreviewContent text={payload.text} wide={payload.wide} onExpandImage={expandImage} />}
    </div>
  );
}
