"use client";
import { usePathname, useRouter, useSearchParams } from "next/navigation";
import { useEffect, useMemo, useState, type ReactNode } from "react";
import { SettingsHost } from "./settings/SettingsHost";
import { useSolveOnlyMode } from "@/lib/workspaceMode";
export { settingsPageFromPath } from "./settings/catalog";
export function SettingsHub({children}: {children: ReactNode}) {
  const pathname = usePathname() || "/settings";
  const router = useRouter(); const params = useSearchParams();
  const solveOnly = useSolveOnlyMode();
  const [target, setTarget] = useState({hash: "", anchorRequestId: 0});
  useEffect(() => {
    const update = () => setTarget(value => ({hash: window.location.hash, anchorRequestId: value.anchorRequestId + 1}));
    update(); window.addEventListener("hashchange", update); window.addEventListener("popstate", update);
    return () => { window.removeEventListener("hashchange", update); window.removeEventListener("popstate", update); };
  }, [pathname, params]);
  const navigation = useMemo(() => ({pathname, searchParams: new URLSearchParams(params.toString()), ...target,
    router: {push: (href: string) => {
      const url = new URL(href, window.location.href);
      if (url.pathname === pathname && url.search === window.location.search && url.hash) {
        window.history.pushState(window.history.state, "", href);
        setTarget(value => ({hash: url.hash, anchorRequestId: value.anchorRequestId + 1}));
      } else router.push(href, {scroll: !url.hash});
    }, replace: (href: string) => router.replace(href, {scroll: !new URL(href, window.location.href).hash})},
  }), [pathname, params, router, target]);
  return <SettingsHost client="web" navigation={navigation} back={{href: solveOnly ? "/ctf" : "/chat"}}>{children}</SettingsHost>;
}
