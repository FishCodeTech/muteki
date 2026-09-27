"use client";

import { Dropdown } from "@heroui/react";

import { Icon } from "@/components/Icon";
import { COORDINATOR_ID, DECISION_ID, INPUT_SOURCE_ID } from "@/lib/agentCollaboration";
import { copyToClipboard } from "@/lib/clipboard";
import { useT } from "@/lib/i18n";

export type CollaborationContextMenuState = {
  x: number;
  y: number;
  nodeId?: string;
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
  const isWorker = !!nodeId && ![COORDINATOR_ID, DECISION_ID, INPUT_SOURCE_ID].includes(nodeId);
  const label = nodeId ? t("collab.menu.agent") : t("collab.menu.canvas");

  return (
    <Dropdown isOpen onOpenChange={(open) => { if (!open) onClose(); }}>
      <Dropdown.Trigger
        aria-label={label}
        className="collab-context-anchor"
        style={{ left: menu.x, top: menu.y }}
      />
      <Dropdown.Popover placement="bottom start" className="collab-context-menu">
        <Dropdown.Menu aria-label={label} onAction={(key) => {
          const id = nodeId;
          const actions: Record<string, () => void> = {
            openWorker: () => { if (id) onOpenWorker?.(id); },
            timeline: () => { if (id) onOpenTimeline?.(id); },
            stop: () => { if (id) onKillWorker?.(id); },
            copyId: () => { if (id) void copyToClipboard(id); },
            focusRelated: () => { if (id) onFocusRelated(id); },
            fit: () => onFit(),
          };
          onClose();
          actions[String(key)]?.();
        }}>
          {isWorker && onOpenWorker ? (
            <Dropdown.Item id="openWorker" textValue={t("collab.action.openWorker")}>
              <Icon name="panel" size={14} />{t("collab.action.openWorker")}
            </Dropdown.Item>
          ) : null}
          {isWorker && onOpenTimeline ? (
            <Dropdown.Item id="timeline" textValue={t("collab.action.timeline")}>
              <Icon name="rows" size={14} />{t("collab.action.timeline")}
            </Dropdown.Item>
          ) : null}
          {isWorker && agentOnline && onKillWorker ? (
            <Dropdown.Item id="stop" textValue={t("collab.action.stop")} className="danger">
              <Icon name="stop" size={14} />{t("collab.action.stop")}
            </Dropdown.Item>
          ) : null}
          {nodeId ? (
            <Dropdown.Item id="copyId" textValue={t("collab.action.copyId")}>
              <Icon name="copy" size={14} />{t("collab.action.copyId")}
            </Dropdown.Item>
          ) : null}
          {nodeId ? (
            <Dropdown.Item id="focusRelated" textValue={t("collab.action.focusRelated")}>
              <Icon name="network" size={14} />{t("collab.action.focusRelated")}
            </Dropdown.Item>
          ) : null}
          {!nodeId ? (
            <Dropdown.Item id="fit" textValue={t("collab.fit")}>
              <Icon name="crosshair" size={14} />{t("collab.fit")}
            </Dropdown.Item>
          ) : null}
        </Dropdown.Menu>
      </Dropdown.Popover>
    </Dropdown>
  );
}
