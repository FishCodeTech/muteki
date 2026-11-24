"use client";

import { Suspense, type ReactNode } from "react";
import { StudioPrototypeRoute } from "@/components/studio-prototype/StudioPrototype";

export default function StudioPrototypeLayout({ children }: { children: ReactNode }) {
  return (
    <Suspense fallback={null}>
      <StudioPrototypeRoute>{children}</StudioPrototypeRoute>
    </Suspense>
  );
}
