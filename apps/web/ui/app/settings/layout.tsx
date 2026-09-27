"use client";

import type { ReactNode } from "react";
import { LoginGate } from "@/components/LoginGate";
import { SettingsHub } from "@/components/SettingsHub";
import { I18nProvider } from "@/lib/i18n";

export default function SettingsLayout({ children }: { children: ReactNode }) {
  return (
    <I18nProvider>
      <LoginGate>
        <SettingsHub>{children}</SettingsHub>
      </LoginGate>
    </I18nProvider>
  );
}
