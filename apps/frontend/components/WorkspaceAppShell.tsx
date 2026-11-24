"use client";

import { usePathname } from "next/navigation";
import { Fragment, Suspense, type ReactNode } from "react";

import { LoginGate } from "@/components/LoginGate";
import { WorkspaceFrame } from "@/components/WorkspaceNav";
import { I18nProvider } from "@/lib/i18n";
import { DesktopNavigationBridge } from "@/components/DesktopNavigationBridge";
import { useDesktopShell } from "@/lib/desktopShell";
import { useDesktopChrome } from "@/components/DesktopConnection";

export function WorkspaceAppShell({ children }: { children: ReactNode }) {
  const pathname = usePathname();
  const reloadKey = useDesktopShell();
  const desktop = useDesktopChrome();
  if (pathname === "/native-report-preview") return <I18nProvider>{children}</I18nProvider>;
  if ((pathname.startsWith("/settings") && !desktop?.native) || /^\/share\/[^/]+\/?$/.test(pathname)) return <Fragment key={reloadKey}>{children}</Fragment>;

  return (
    <I18nProvider key={reloadKey}>
      <Suspense fallback={null}><DesktopNavigationBridge /></Suspense>
      <LoginGate>
        <WorkspaceFrame>{children}</WorkspaceFrame>
      </LoginGate>
    </I18nProvider>
  );
}
