"use client";

import { useEffect, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";

/** Renders into a shared `.cx-portal` layer so overlays inherit chat typography. */
export function Portal({ children }: { children: ReactNode }) {
  const [host, setHost] = useState<HTMLElement | null>(null);
  useEffect(() => {
    let layer = document.getElementById("cx-portal-root");
    if (!layer) {
      layer = document.createElement("div");
      layer.id = "cx-portal-root";
      layer.className = "cx-portal";
      document.body.appendChild(layer);
    }
    setHost(layer);
  }, []);
  return host ? createPortal(children, host) : null;
}
