"use client";

import { LoginGate } from "@/components/LoginGate";
import { WorkerOrchestration } from "@/components/WorkerOrchestration";
import { I18nProvider } from "@/lib/i18n";

export default function WorkerSettingsPage() {
  return (
    <I18nProvider>
      <LoginGate>
        <WorkerOrchestration />
      </LoginGate>
    </I18nProvider>
  );
}

