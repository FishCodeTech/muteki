"use client";

import { createContext, useContext, useSyncExternalStore, type ReactNode } from "react";
import type { IconName } from "@/components/Icon";
import { chatPanel, type ChatSurface } from "@/lib/chatPanelStore";

export type ExtensionSurface = Extract<ChatSurface, { kind: "extension" }>;

export interface PanelExtensionRenderProps {
  surface: ExtensionSurface;
  threadId: string;
  /** True while this surface's tab is the visible one. */
  active: boolean;
  /** True when the panel renders as an overlay sheet (narrow viewports). */
  sheet: boolean;
}

export interface PanelExtension {
  key: string;
  label: string;
  icon: IconName;
  description?: string;
  /** Binding in `mod+alt+1` notation, shown in the launcher and tooltips. */
  shortcut?: string;
  render: (props: PanelExtensionRenderProps) => ReactNode;
}

/**
 * Surfaces a host page adds to the right panel. Provided by ConversationShell
 * only for local previews, so regular chat panels never see it.
 */
export interface PanelExtensionConfig {
  extensions: PanelExtension[];
  /** Hide the built-in launcher entries; only the extensions can be opened. */
  exclusive?: boolean;
  /**
   * "dock": maximizing gives the panel the whole conversation area except the
   * composer, which stays docked under it; replies open as a layer above the
   * composer. The composer, draft and timeline are the same mounted elements.
   */
  focusLayout?: "dock";
  /** Side-by-side width as a share of the area right of the sidebar, kept apart from the regular chat panel width. */
  widthRatio?: { initial: number; mainMin: number; panelMin: number };
  launcherTitle?: string;
  launcherDescription?: string;
  /** Viewport width (px) at or below which the panel renders as a sheet. */
  sheetBreakpoint?: number;
  sheetWidth?: string | ((viewport: number) => string);
}

// Ratio of hosts using `widthRatio`; session-scoped so the regular chat
// panel width (persisted by chatPanelStore) is never touched.
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
      // Storage blocked: keep the default ratio.
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
      // ignore
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
/** Set by RightPanel: whether it currently renders as an overlay sheet. */
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
      <div role="status" className="p-4 text-[13px] text-cx-fg-3">
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
  return config?.extensions.find((item) => item.key === key);
}

/** Opens the panel; when it has nothing to show yet, opens the first extension instead of the built-in overview. */
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
