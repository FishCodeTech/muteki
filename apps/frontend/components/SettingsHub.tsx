"use client";
import { usePathname, useRouter, useSearchParams } from "next/navigation";
import { useEffect, useMemo, useState, type ReactNode } from "react";
import { SettingsHost } from "./settings/SettingsHost";
import { settingsPageFromPath } from "./settings/catalog";
import { useSolveOnlyMode } from "@/lib/workspaceMode";
import { useDesktopChrome } from "@/components/DesktopConnection";
export { settingsPageFromPath } from "./settings/catalog";
export function SettingsHub({children}: {children: ReactNode}) {
  const pathname = usePathname() || "/settings";
  const router = useRouter(); const params = useSearchParams();
  const solveOnly = useSolveOnlyMode();
  const desktop = useDesktopChrome();
  const [target, setTarget] = useState({hash: "", anchorRequestId: 0});
  useEffect(() => {
    const update = () => setTarget(value => ({hash: window.location.hash, anchorRequestId: value.anchorRequestId + 1}));
    update(); window.addEventListener("hashchange", update); window.addEventListener("popstate", update);
    return () => { window.removeEventListener("hashchange", update); window.removeEventListener("popstate", update); };
  }, [pathname, params]);
  const navigation = useMemo(() => {
    const settingsUrl = (href: string) => {
      const url = new URL(href, window.location.href);
      return url.origin === window.location.origin && settingsPageFromPath(url.pathname) ? url : null;
    };
    return {pathname, searchParams: new URLSearchParams(params.toString()), ...target,
      router: {push: (href: string) => {
        const url = new URL(href, window.location.href);
        if (url.pathname === pathname && url.search === window.location.search && url.hash) {
          window.history.pushState(window.history.state, "", href);
          setTarget(value => ({hash: url.hash, anchorRequestId: value.anchorRequestId + 1}));
        } else if (settingsUrl(href)) window.history.pushState(null, "", url.pathname + url.search + url.hash);
        else router.push(href, {scroll: !url.hash});
      }, replace: (href: string) => {
        const url = settingsUrl(href);
        if (url) window.history.replaceState(null, "", url.pathname + url.search + url.hash);
        else router.replace(href, {scroll: !new URL(href, window.location.href).hash});
      }},
    };
  }, [pathname, params, router, target]);
  return <SettingsHost client={desktop?.native ? "desktop" : "web"} desktop={desktop?.native ? { origin: desktop.origin, connected: desktop.connected, onConfigure: desktop.configure, onHelp: desktop.help } : undefined} navigation={navigation} back={{href: solveOnly ? "/ctf" : "/chat"}}>{children}</SettingsHost>;
}
