"use client";

import { useEffect, useRef, useState } from "react";
import { cn } from "@/lib/cn";
import { Icon } from "@muteki/ui";
import { IconButton } from "@muteki/ui";
import { chatPanel } from "@/lib/chatPanelStore";
import { desktopChatBridge } from "@/lib/desktopChatBridge";
import { apiFetch } from "@/lib/serviceAuth";
import { shownNativePreview } from "@/components/chat/preview/NativePreview";

type ActivityItem = {
  key: string;
  kind: "browser" | "computer";
  label: string;
  detail: string;
  host: string;
  app_id: string;
  updated_at: number;
  picture: number | null;
};

const POLL_MS = 2000;
const PANEL_SHOT_MS = 4000;
const STACK = 3;
const OFFSET = { x: 26, y: 20 };

function itemTitle(item: ActivityItem) {
  if (item.kind === "computer") return item.label || item.app_id || "电脑";
  return item.label || "浏览器";
}

function hostLabel(item: ActivityItem) {
  if (item.kind === "computer") return "电脑操控";
  if (item.host === "headless") return "后台浏览器";
  if (item.host === "desktop_panel") return "右侧浏览器";
  return "浏览器";
}

export function AgentActivityDock({ threadId, running }: { threadId: string; running: boolean }) {
  const [turn, setTurn] = useState("");
  const [items, setItems] = useState<ActivityItem[]>([]);
  const [pictures, setPictures] = useState<Record<string, string>>({});
  const [dismissed, setDismissed] = useState("");
  const [expanded, setExpanded] = useState(false);
  const loaded = useRef(new Map<string, { version: number; url: string }>());

  useEffect(() => {
    const versions = loaded.current;
    const reset = () => {
      for (const { url } of versions.values()) if (url.startsWith("blob:")) URL.revokeObjectURL(url);
      versions.clear();
      setItems([]);
      setPictures({});
    };
    if (!running) { reset(); return; }
    reset();
    let stopped = false;
    let timer = 0;
    let lastPanelShot = 0;
    let currentTurn = "";
    const show = (key: string, version: number, url: string) => {
      const previous = versions.get(key);
      if (previous?.url.startsWith("blob:")) URL.revokeObjectURL(previous.url);
      versions.set(key, { version, url });
      setPictures((current) => ({ ...current, [key]: url }));
    };
    const poll = async () => {
      try {
        const response = await apiFetch(`/api/threads/${encodeURIComponent(threadId)}/activity`);
        if (!response.ok) throw new Error(`HTTP ${response.status}`);
        const data = await response.json() as { turn_id: string; items: ActivityItem[] };
        if (stopped) return;
        const nextTurn = data.turn_id || "";
        if (nextTurn !== currentTurn) {
          reset();
          lastPanelShot = 0;
          currentTurn = nextTurn;
        }
        setTurn(nextTurn);
        const rows = [...(data.items || [])].sort((a, b) => b.updated_at - a.updated_at);
        setItems(rows);
        for (const item of rows) {
          if (item.picture == null || (versions.get(item.key)?.version ?? -1) >= item.picture) continue;
          const picture = await apiFetch(`/api/threads/${encodeURIComponent(threadId)}/activity/picture?key=${encodeURIComponent(item.key)}`, { cache: "no-store" });
          if (!picture.ok || stopped) continue;
          const blob = await picture.blob();
          if (stopped) return;
          show(item.key, item.picture, URL.createObjectURL(blob));
        }
        const panel = rows.find((item) => item.host === "desktop_panel");
        const id = shownNativePreview(threadId);
        const bridge = desktopChatBridge();
        if (panel && id && bridge?.previewAction && Date.now() - lastPanelShot >= PANEL_SHOT_MS) {
          lastPanelShot = Date.now();
          const shot = await bridge.previewAction({ id, action: "screenshot" }).catch(() => null);
          if (shot?.data && !stopped) show(panel.key, Number.MAX_SAFE_INTEGER, `data:${shot.mime};base64,${shot.data}`);
        }
      } catch (error) {
        console.warn("chat.activity.poll_failed", error);
      } finally {
        if (!stopped) timer = window.setTimeout(() => void poll(), POLL_MS);
      }
    };
    void poll();
    return () => { stopped = true; window.clearTimeout(timer); reset(); };
  }, [running, threadId]);

  useEffect(() => () => {
    for (const { url } of loaded.current.values()) if (url.startsWith("blob:")) URL.revokeObjectURL(url);
  }, []);

  if (!running || !items.length || (turn && dismissed === turn)) return null;
  const open = (item: ActivityItem) => {
    if (item.host === "desktop_panel") chatPanel.openPreview(threadId);
    else setExpanded((value) => !value);
  };
  const card = (item: ActivityItem, large: boolean) => (
    <button
      type="button"
      onClick={() => open(item)}
      title={[itemTitle(item), item.detail].filter(Boolean).join("\n")}
      aria-label={`${hostLabel(item)}：${itemTitle(item)}`}
      className={cn(
        "group relative block overflow-hidden rounded-xl ring-1 ring-cx-ring bg-cx-elevated text-left shadow-cx-md outline-none",
        "focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]",
        large ? "h-[132px] w-[212px]" : "h-[104px] w-[168px]",
      )}
    >
      {pictures[item.key] ? (
        // eslint-disable-next-line @next/next/no-img-element
        <img src={pictures[item.key]} alt="" draggable={false} className="size-full object-cover object-top" />
      ) : (
        <span className="grid size-full place-items-center text-cx-fg-4">
          <Icon name={item.kind === "computer" ? "monitor" : "globe"} size={22} />
        </span>
      )}
      <span className="absolute inset-x-1.5 bottom-1.5 flex min-w-0 items-center gap-1 rounded-md bg-[color-mix(in_srgb,var(--cx-bg)_86%,transparent)] px-1.5 py-0.5 text-cx-2xs text-cx-fg-2 backdrop-blur-sm">
        <Icon name={item.kind === "computer" ? "monitor" : "globe"} size={11} className="shrink-0 text-cx-fg-3" />
        <span className="truncate">{itemTitle(item)}</span>
      </span>
    </button>
  );
  const stacked = items.slice(0, STACK);
  return (
    <div
      className="pointer-events-none absolute right-3 top-[calc(var(--cx-header-h)+10px)] z-20 flex flex-col items-end gap-1.5 max-md:hidden"
      data-testid="agent-activity-dock"
      aria-label="正在使用的浏览器和电脑应用"
      role="region"
    >
      <div className="pointer-events-auto flex items-center gap-0.5 rounded-full ring-1 ring-cx-ring bg-cx-elevated px-1 shadow-cx-xs">
        <span className="px-1.5 text-cx-xs text-cx-fg-3">{items.length} 个正在使用</span>
        <IconButton icon={expanded ? "chevronUp" : "chevronDown"} label={expanded ? "收起" : "展开全部"} size="xs" onClick={() => setExpanded((value) => !value)} />
        <IconButton icon="x" label="本回合不再显示" size="xs" onClick={() => setDismissed(turn)} />
      </div>
      {expanded ? (
        <div className="pointer-events-auto flex max-h-[calc(100vh-180px)] flex-col gap-2 overflow-y-auto pb-1">
          {items.map((item) => (
            <div key={item.key} className="flex flex-col items-end gap-0.5">
              {card(item, true)}
              <span className="max-w-[212px] truncate text-cx-2xs text-cx-fg-4">{hostLabel(item)}{item.detail ? ` · ${item.detail}` : ""}</span>
            </div>
          ))}
        </div>
      ) : (
        <div className="pointer-events-auto relative" style={{ width: 168 + (stacked.length - 1) * OFFSET.x, height: 104 + (stacked.length - 1) * OFFSET.y }}>
          {stacked.map((item, index) => (
            <div
              key={item.key}
              className="absolute transition-transform duration-200"
              style={{ right: index * OFFSET.x, top: index * OFFSET.y, zIndex: STACK - index, transform: `scale(${1 - index * 0.05})`, transformOrigin: "bottom left" }}
            >
              {card(item, false)}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
