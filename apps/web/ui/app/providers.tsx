"use client";

import { Toast } from "@heroui/react";
import type { ReactNode } from "react";

export function Providers({ children }: { children: ReactNode }) {
  return (
    <>
      {children}
      <Toast.Provider
        maxVisibleToasts={3}
        placement="bottom"
      />
    </>
  );
}
