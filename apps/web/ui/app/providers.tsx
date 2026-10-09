"use client";

import { UiPreferencesSync } from "@/components/UiPreferencesSync";
import { Toast } from "@heroui/react";
import type { ReactNode } from "react";

export function Providers({ children }: { children: ReactNode }) {
  return (
    <>
      <UiPreferencesSync />
      {children}
      <Toast.Provider
        maxVisibleToasts={3}
        placement="bottom"
      />
    </>
  );
}
