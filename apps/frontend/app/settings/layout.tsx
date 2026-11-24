"use client";

import type { ReactNode } from "react";
import { LoginGate } from "@/components/LoginGate";
import { SettingsHub } from "@/components/SettingsHub";
import { I18nProvider } from "@/lib/i18n";
import { useDesktopChrome } from "@/components/DesktopConnection";

export default function SettingsLayout({ children }: { children: ReactNode }) {
  const desktop = useDesktopChrome();
  if (desktop?.native) return <SettingsHub>{children}</SettingsHub>;
  return (
    <I18nProvider>
      <LoginGate>
        <SettingsHub>{children}</SettingsHub>
      </LoginGate>
    </I18nProvider>
  );
}
