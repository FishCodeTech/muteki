"use client";

import { usePathname, useRouter } from "next/navigation";
import { useEffect, useState } from "react";
import type { ReactNode } from "react";

import { LoginGate } from "@/components/LoginGate";
import { WorkspaceFrame } from "@/components/WorkspaceNav";
import { I18nProvider } from "@/lib/i18n";
import { useSolveOnlyMode } from "@/lib/workspaceMode";

function allowedInSolveOnly(pathname: string): boolean {
  return pathname === "/"
    || pathname === "/ctf" || pathname.startsWith("/ctf/")
    || pathname.startsWith("/run/")
    || pathname === "/pentest" || pathname.startsWith("/pentest/")
    || pathname === "/settings/appearance";
}

/** Keep the workspace navigation mounted while the active workspace changes. */
export function WorkspaceAppShell({ children }: { children: ReactNode }) {
  const pathname = usePathname();
  const router = useRouter();
  const solveOnly = useSolveOnlyMode();
  const [hydrated, setHydrated] = useState(false);
  useEffect(() => setHydrated(true), []);
  const restricted = hydrated && solveOnly && !allowedInSolveOnly(pathname);
  useEffect(() => {
    if (restricted) router.replace(pathname.startsWith("/settings") ? "/settings/appearance" : "/ctf");
  }, [pathname, restricted, router]);
  if (restricted) return <div role="status" className="workspace-mode-redirect">正在返回做题模式…</div>;
  // Settings has its own full-page navigation and auth/i18n layout.
  if (pathname.startsWith("/settings") || /^\/share\/[^/]+\/?$/.test(pathname)) return <>{children}</>;

  return (
    <I18nProvider>
      <LoginGate>
        <WorkspaceFrame>{children}</WorkspaceFrame>
      </LoginGate>
    </I18nProvider>
  );
}
