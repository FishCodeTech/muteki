"use client";

import { usePathname, useRouter, useSearchParams } from "next/navigation";
import { useEffect, useRef, useTransition } from "react";
import { isDesktopRemote, setDesktopWorkspaceActive } from "@/lib/desktopEnvironment";
import { useActiveRunWorkspace } from "@/lib/activeRunWorkspace";
import { runIdFromPathname } from "@/lib/runWorkspaceRoutes";

type NavigationRequest = { id: number; href: string };

/** A narrow navigation contract for the native activity rail. */
export function DesktopNavigationBridge() {
  const router = useRouter();
  const pathname = usePathname();
  const search = useSearchParams().toString();
  const activeRun = useActiveRunWorkspace();
  const activeRunRef = useRef(activeRun);
  activeRunRef.current = activeRun;
  const pending = useRef<NavigationRequest | null>(null);
  const [navigating, startNavigation] = useTransition();

  useEffect(() => {
    if (!isDesktopRemote()) return;
    const navigate = (event: Event) => {
      const request = (event as CustomEvent<NavigationRequest>).detail;
      if (!request || !Number.isSafeInteger(request.id) || typeof request.href !== "string") return;
      const url = new URL(request.href, window.location.origin);
      if (url.origin !== window.location.origin || !/^\/(ctf|pentest|competitions|run|task|usage)(\/|$)/.test(url.pathname)) return;
      event.preventDefault();
      pending.current = { id: request.id, href: url.pathname + url.search + url.hash };
      // The native shell owns its own push/replace/back/forward history.
      startNavigation(() => router.replace(url.pathname + url.search + url.hash));
    };
    window.addEventListener("muteki:desktop-navigate", navigate);
    const visibility = (event: Event) => {
      const active = (event as CustomEvent<{ active: boolean }>).detail?.active;
      if (typeof active !== "boolean") return;
      event.preventDefault();
      if (!active) pending.current = null;
      setDesktopWorkspaceActive(active);
    };
    // Let the native host route cross-mode links before Next replaces this tab's
    // page. In-mode links keep the usual client routing and browser semantics.
    const mode = (path: string) => {
      // Collaboration shares /run/ across CTF and Pentest; its live deck owns the mode.
      const activeRun = activeRunRef.current;
      if (/^\/run\/[^/]+\/collaboration\/?$/.test(path) && activeRun?.runId === runIdFromPathname(path)) return activeRun.mode;
      const first = path.split("/")[1];
      if (["chat", "settings", "ctf", "pentest", "competitions", "usage", "src"].includes(first)) return first;
      return first === "run" || first === "task" ? "ctf" : "";
    };
    const followLink = (event: MouseEvent) => {
      if (event.defaultPrevented || event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
      const link = event.target instanceof Element ? event.target.closest<HTMLAnchorElement>("a[href]") : null;
      if (!link || link.hasAttribute("download") || (link.target && link.target !== "_self")) return;
      const target = new URL(link.href, window.location.href);
      const targetMode = mode(target.pathname);
      if (target.origin !== window.location.origin || !targetMode || targetMode === mode(window.location.pathname)) return;
      event.preventDefault();
      event.stopPropagation();
      window.location.assign(target.href);
    };
    window.addEventListener("muteki:desktop-visibility", visibility);
    document.addEventListener("click", followLink, true);
    return () => {
      window.removeEventListener("muteki:desktop-navigate", navigate);
      window.removeEventListener("muteki:desktop-visibility", visibility);
      document.removeEventListener("click", followLink, true);
      pending.current = null;
    };
  }, [router]);

  useEffect(() => {
    const request = pending.current;
    if (!request || navigating || window.location.pathname !== pathname) return;
    const href = pathname + window.location.search + window.location.hash;
    pending.current = null;
    window.dispatchEvent(new CustomEvent("muteki:desktop-navigated", { detail: { id: request.id, href } }));
  }, [pathname, search, navigating]);

  return null;
}
