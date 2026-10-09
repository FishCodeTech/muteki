"use client";

import { usePathname } from "next/navigation";
import type { ReactNode } from "react";

import { LoginGate } from "@/components/LoginGate";
import { WorkspaceFrame } from "@/components/WorkspaceNav";
import { I18nProvider } from "@/lib/i18n";

/** Keep the workspace navigation mounted while the active workspace changes. */
export function WorkspaceAppShell({ children }: { children: ReactNode }) {
  const pathname = usePathname();
  // Workspace mode controls navigation visibility. Explicit URLs also come
  // from desktop navigation and deep links, so they must retain their target.
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
