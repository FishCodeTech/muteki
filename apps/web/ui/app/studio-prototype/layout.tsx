"use client";

import { Suspense, type ReactNode } from "react";
import { StudioPrototypeRoute } from "@/components/studio-prototype/StudioPrototype";

// The layout owns the conversation frame, so switching threads or workspaces
// keeps the frame, its right panel and the local store mounted.
export default function StudioPrototypeLayout({ children }: { children: ReactNode }) {
  return (
    <Suspense fallback={null}>
      <StudioPrototypeRoute>{children}</StudioPrototypeRoute>
    </Suspense>
  );
}
