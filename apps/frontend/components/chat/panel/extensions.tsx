"use client";

import { createContext, useContext, useSyncExternalStore, type ReactNode } from "react";
import type { IconName } from "@muteki/ui";
import { chatPanel, type ChatSurface } from "@/lib/chatPanelStore";

export type ExtensionSurface = Extract<ChatSurface, { kind: "extension" }>;

export interface PanelExtensionRenderProps {
  surface: ExtensionSurface;
  threadId: string;
  active: boolean;
  sheet: boolean;
}

export interface PanelExtension {
  key: string;
  label: string;
  icon: IconName;
  description?: string;
  shortcut?: string;
  render: (props: PanelExtensionRenderProps) => ReactNode;
  matches?: (key: string) => boolean;
  open?: (threadId: string) => void;
  onClose?: (surface: ExtensionSurface, threadId: string) => void;
}

export interface PanelExtensionConfig {
  extensions: PanelExtension[];
  exclusive?: boolean;
  focusLayout?: "dock";
  widthRatio?: { initial: number; mainMin: number; panelMin: number };
  launcherTitle?: string;
  launcherDescription?: string;
  sheetBreakpoint?: number;
  sheetWidth?: string | ((viewport: number) => string);
}

const RATIO_KEY = "muteki.preview.panel.ratio.v1";
let panelRatio: number | null = null;
let ratioHydrated = false;
const ratioListeners = new Set<() => void>();

function readRatio(): number | null {
  if (!ratioHydrated && typeof window !== "undefined") {
    ratioHydrated = true;
    try {
      const stored = Number(window.sessionStorage.getItem(RATIO_KEY));
      if (Number.isFinite(stored) && stored > 0 && stored < 1) panelRatio = stored;
    } catch {
    }
  }
  return panelRatio;
}

export const extensionPanelRatio = {
  get: readRatio,
  set(next: number | null) {
    panelRatio = next;
    try {
      if (next == null) window.sessionStorage.removeItem(RATIO_KEY);
      else window.sessionStorage.setItem(RATIO_KEY, String(next));
    } catch {
    }
    ratioListeners.forEach((listener) => listener());
  },
  subscribe(listener: () => void) {
    ratioListeners.add(listener);
    return () => { ratioListeners.delete(listener); };
  },
};

export function useExtensionPanelRatio(): number | null {
  return useSyncExternalStore(extensionPanelRatio.subscribe, readRatio, () => null);
}

export const PanelExtensionContext = createContext<PanelExtensionConfig | null>(null);
export const PanelSheetContext = createContext(false);

export function usePanelExtensions(): PanelExtensionConfig | null {
  return useContext(PanelExtensionContext);
}

export function ExtensionSurfaceHost({ surface, threadId, active }: { surface: ExtensionSurface; threadId: string; active: boolean }) {
  const config = usePanelExtensions();
  const sheet = useContext(PanelSheetContext);
  const extension = findPanelExtension(config, surface.key);
  if (!extension) {
    return (
      <div role="status" className="p-4 text-cx-sm text-cx-fg-3">
        这个视图只在提供它的页面中可用。可以关闭这个标签。
      </div>
    );
  }
  return <>{extension.render({ surface, threadId, active, sheet })}</>;
}

export function extensionSurface(key: string, title?: string): ExtensionSurface {
  return { id: `ext:${key}`, kind: "extension", key, title };
}

export function findPanelExtension(config: PanelExtensionConfig | null, key: string): PanelExtension | undefined {
  return config?.extensions.find((item) => item.key === key || item.matches?.(key));
}

export function notifyExtensionTabsClosed(config: PanelExtensionConfig | null, threadId: string, surfaces: ChatSurface[]) {
  for (const surface of surfaces) {
    if (surface.kind === "extension") findPanelExtension(config, surface.key)?.onClose?.(surface, threadId);
  }
}

export function openPanelWithExtensions(threadId: string, config: PanelExtensionConfig) {
  const current = chatPanel.thread(threadId);
  if (current.surfaces.length) { chatPanel.open(threadId); return; }
  const first = config.extensions[0];
  if (first) chatPanel.open(threadId, extensionSurface(first.key, first.label));
  else chatPanel.open(threadId);
}

export function togglePanelWithExtensions(threadId: string, config: PanelExtensionConfig) {
  if (chatPanel.thread(threadId).isOpen) chatPanel.close(threadId);
  else openPanelWithExtensions(threadId, config);
}
