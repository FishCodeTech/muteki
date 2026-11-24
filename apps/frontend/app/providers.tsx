"use client";

import { useEffect, type ReactNode } from "react";
import { MotionConfig } from "motion/react";
import { Toaster } from "@muteki/ui";
import { UiPreferencesSync } from "@/components/UiPreferencesSync";
import { usePrefersReducedMotion } from "@/lib/usePrefersReducedMotion";

export function Providers({ children }: { children: ReactNode }) {
  const reduced = usePrefersReducedMotion();

  useEffect(() => {
    const syncVisibility = () => {
      document.documentElement.dataset.pageHidden = document.hidden ? "true" : "false";
    };
    syncVisibility();
    document.addEventListener("visibilitychange", syncVisibility);
    return () => {
      document.removeEventListener("visibilitychange", syncVisibility);
      delete document.documentElement.dataset.pageHidden;
    };
  }, []);

  return (
    <MotionConfig reducedMotion={reduced ? "always" : "never"}>
      <UiPreferencesSync />
      {children}
      <Toaster />
    </MotionConfig>
  );
}
