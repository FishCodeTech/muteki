"use client";

import { createContext, useContext, useEffect, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";

/** Retained views deactivate their overlays, focus traps and scroll locks together. */
export const OverlayVisibilityContext = createContext(true);
const OverlayHostContext = createContext<{ scoped: boolean; host: HTMLElement | null }>({
  scoped: false,
  host: null,
});

export function ensurePortalRoot(): HTMLElement {
  let layer = document.getElementById("cx-portal-root");
  if (!layer) {
    layer = document.createElement("div");
    layer.id = "cx-portal-root";
    layer.className = "cx-portal";
    document.body.appendChild(layer);
  }
  return layer;
}

export function useOverlayContainer(): HTMLElement | null {
  const scopedContext = useContext(OverlayHostContext);
  const [defaultHost, setDefaultHost] = useState<HTMLElement | null>(null);
  useEffect(() => {
    if (scopedContext.scoped) return;
    setDefaultHost(ensurePortalRoot());
  }, [scopedContext.scoped]);
  return scopedContext.scoped ? scopedContext.host : defaultHost;
}

/**
 * Isolates portaled overlays inside a retained React `<Activity>` boundary.
 */
export function RetainedOverlayScope({
  scope,
  visible,
  children,
}: {
  scope: string;
  visible: boolean;
  children: ReactNode;
}) {
  const [root, setRoot] = useState<HTMLElement | null>(null);
  const [scopeHost, setScopeHost] = useState<HTMLDivElement | null>(null);
  useEffect(() => {
    setRoot(ensurePortalRoot());
  }, []);
  return (
    <OverlayHostContext.Provider value={{ scoped: true, host: scopeHost }}>
      <OverlayVisibilityContext.Provider value={visible}>
        {root ? createPortal(<div data-portal-scope={scope} hidden={!visible} ref={setScopeHost} />, root) : null}
        {children}
      </OverlayVisibilityContext.Provider>
    </OverlayHostContext.Provider>
  );
}

/** Renders into a shared `.cx-portal` layer so overlays inherit Muteki UI typography. */
export function Portal({ children, unscoped = false }: { children: ReactNode; unscoped?: boolean }) {
  const visible = useContext(OverlayVisibilityContext);
  const scopedContext = useContext(OverlayHostContext);
  const [defaultHost, setDefaultHost] = useState<HTMLElement | null>(null);
  useEffect(() => {
    if (!unscoped && scopedContext.scoped) return;
    setDefaultHost(ensurePortalRoot());
  }, [scopedContext.scoped, unscoped]);
  if (unscoped) return defaultHost ? createPortal(children, defaultHost) : null;
  if (scopedContext.scoped) return scopedContext.host && visible ? createPortal(children, scopedContext.host) : null;
  return defaultHost && visible ? createPortal(children, defaultHost) : null;
}
