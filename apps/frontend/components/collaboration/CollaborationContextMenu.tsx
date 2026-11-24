"use client";

import { useEffect, useRef } from "react";
import { Menu, MenuItem, MenuSeparator } from "@muteki/ui";

import { COORDINATOR_ID, DECISION_ID, CHALLENGER_ID, INPUT_SOURCE_ID } from "@/lib/agentCollaboration";
import { copyToClipboard } from "@/lib/clipboard";
import { useT } from "@/lib/i18n";

export type CollaborationContextMenuState = {
  x: number;
  y: number;
  nodeId?: string;
  returnFocus?: HTMLElement | null;
};

export function CollaborationContextMenu({
  menu,
  agentOnline,
  onClose,
  onOpenWorker,
  onOpenTimeline,
  onKillWorker,
  onFocusRelated,
  onFit,
}: {
  menu: CollaborationContextMenuState;
  agentOnline?: boolean;
  onClose: () => void;
  onOpenWorker?: (id: string) => void;
  onOpenTimeline?: (id: string) => void;
  onKillWorker?: (id: string) => void;
  onFocusRelated: (id: string) => void;
  onFit: () => void;
}) {
  const t = useT();
  const nodeId = menu.nodeId;
  const isWorker = !!nodeId && ![COORDINATOR_ID, DECISION_ID, CHALLENGER_ID, INPUT_SOURCE_ID].includes(nodeId);
  const label = nodeId ? t("collab.menu.agent") : t("collab.menu.canvas");

  const anchorRef = useRef<HTMLElement | null>(null);
  anchorRef.current = menu.returnFocus ?? null;
  useEffect(() => () => {
    const anchor = anchorRef.current;
    window.requestAnimationFrame(() => {
      const active = document.activeElement;
      if (anchor?.isConnected && (!active || active === document.body || active.closest("[role='menu']"))) anchor.focus({ preventScroll: true });
    });
  }, []);

  return (
    <Menu open onOpenChange={(open) => { if (!open) onClose(); }} anchorPoint={menu} anchorRef={anchorRef} ariaLabel={label}>
      {nodeId && isWorker && onOpenWorker ? <MenuItem icon="panel" onSelect={() => onOpenWorker(nodeId)}>{t("collab.action.openWorker")}</MenuItem> : null}
      {nodeId && isWorker && onOpenTimeline ? <MenuItem icon="rows" onSelect={() => onOpenTimeline(nodeId)}>{t("collab.action.timeline")}</MenuItem> : null}
      {nodeId ? <MenuItem icon="network" onSelect={() => onFocusRelated(nodeId)}>{t("collab.action.focusRelated")}</MenuItem> : null}
      {nodeId ? <MenuItem icon="copy" onSelect={() => { void copyToClipboard(nodeId); }}>{t("collab.action.copyId")}</MenuItem> : null}
      {!nodeId ? <MenuItem icon="crosshair" onSelect={onFit}>{t("collab.fit")}</MenuItem> : null}
      {nodeId && isWorker && agentOnline && onKillWorker ? <><MenuSeparator /><MenuItem icon="stop" danger onSelect={() => onKillWorker(nodeId)}>{t("collab.action.stop")}</MenuItem></> : null}
    </Menu>
  );
}
