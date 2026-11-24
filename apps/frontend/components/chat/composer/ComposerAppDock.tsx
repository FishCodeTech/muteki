"use client";

import { useEffect, useState, type KeyboardEvent } from "react";
import { Icon } from "@muteki/ui";
import { IconButton } from "@muteki/ui";
import { cn } from "@/lib/cn";
import type { ComposerCapabilityItem } from "@/lib/composerCapabilities";
import {
  fetchMacApps,
  MAC_APP_DRAG_MIME,
  macAppCapability,
  type MacAppItem,
} from "@/lib/macApps";

type DockState =
  | { phase: "loading" }
  | { phase: "ready"; apps: MacAppItem[] }
  | { phase: "unavailable"; reason: string }
  | { phase: "error"; message: string };

export interface ComposerAppDockProps {
  onPick: (item: ComposerCapabilityItem) => void;
  onClose: () => void;
}

export function ComposerAppDock({ onPick, onClose }: ComposerAppDockProps) {
  const [state, setState] = useState<DockState>({ phase: "loading" });
  const [revision, setRevision] = useState(0);

  useEffect(() => {
    const controller = new AbortController();
    setState({ phase: "loading" });
    fetchMacApps(controller.signal).then((inventory) => {
      if (controller.signal.aborted) return;
      setState(inventory.available
        ? { phase: "ready", apps: inventory.apps }
        : { phase: "unavailable", reason: inventory.reason });
    }).catch((error: unknown) => {
      if (controller.signal.aborted) return;
      setState({ phase: "error", message: error instanceof Error ? error.message : String(error) });
    });
    return () => controller.abort();
  }, [revision]);

  const docked = state.phase === "ready" ? state.apps.filter((app) => app.in_dock) : [];
  const others = state.phase === "ready" ? state.apps.filter((app) => !app.in_dock) : [];

  return (
    <div
      className="mb-2 rounded-2xl ring-1 ring-cx-ring bg-cx-elevated px-2.5 pb-1.5 pt-1.5 shadow-cx-xs"
      data-testid="composer-app-dock"
      onKeyDown={(event) => {
        if (event.key !== "Escape") return;
        event.stopPropagation();
        onClose();
      }}
    >
      <div className="flex items-center gap-1.5 pl-1 text-cx-xs text-cx-fg-3">
        <Icon name="mousePointer" size={13} className="shrink-0 text-cx-fg-4" />
        <span className="min-w-0 flex-1 truncate">拖到输入框或点击，指定电脑操控的目标应用</span>
        <IconButton
          icon="refresh"
          label="刷新应用列表"
          size="xs"
          tooltipPlacement="top"
          loading={state.phase === "loading"}
          onClick={() => setRevision((n) => n + 1)}
        />
        <IconButton icon="x" label="收起应用坞" size="xs" tooltipPlacement="top" onClick={onClose} />
      </div>
      {state.phase === "ready" ? (
        state.apps.length ? (
          <div role="toolbar" aria-label="本机应用" className="cx-scroll mt-1 flex items-start gap-0.5 overflow-x-auto pb-0.5">
            {docked.map((app) => <DockApp key={app.id} app={app} onPick={onPick} />)}
            {docked.length && others.length ? (
              <span aria-hidden="true" className="mx-1 mt-1.5 h-9 w-px shrink-0 bg-cx-border" />
            ) : null}
            {others.map((app) => <DockApp key={app.id} app={app} onPick={onPick} />)}
          </div>
        ) : (
          <p className="px-1 py-2 text-cx-xs text-cx-fg-3">没有读到 Dock 或正在运行的应用。</p>
        )
      ) : null}
      {state.phase === "loading" ? (
        <p className="px-1 py-2 text-cx-xs text-cx-fg-3" role="status">正在读取本机应用…</p>
      ) : null}
      {state.phase === "unavailable" ? (
        <p className="px-1 py-2 text-cx-xs text-cx-fg-3" role="status">{state.reason}</p>
      ) : null}
      {state.phase === "error" ? (
        <p className="break-words px-1 py-2 text-cx-xs text-cx-danger" role="alert">{state.message}</p>
      ) : null}
    </div>
  );
}

function DockApp({ app, onPick }: { app: MacAppItem; onPick: (item: ComposerCapabilityItem) => void }) {
  const pick = () => onPick(macAppCapability(app));
  const onKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    if (event.key !== "Enter" && event.key !== " ") return;
    event.preventDefault();
    pick();
  };
  return (
    <div
      role="button"
      tabIndex={0}
      draggable
      title={`${app.name}${app.running ? " · 正在运行" : ""}`}
      aria-label={`指定 ${app.name} 为电脑操控目标应用`}
      data-bundle-id={app.target?.bundle_id}
      onClick={pick}
      onKeyDown={onKeyDown}
      onDragStart={(event) => {
        event.dataTransfer.setData(MAC_APP_DRAG_MIME, JSON.stringify(macAppCapability(app)));
        event.dataTransfer.setData("text/plain", app.name);
        event.dataTransfer.effectAllowed = "copy";
        const image = event.currentTarget.querySelector("img");
        if (image) event.dataTransfer.setDragImage(image, 18, 18);
      }}
      className={cn(
        "cx-press flex w-[54px] shrink-0 cursor-grab select-none flex-col items-center rounded-xl px-1 pb-1 pt-1 outline-none",
        "hover:bg-cx-hover active:cursor-grabbing",
        "focus-visible:outline-2 focus-visible:outline-offset-1 focus-visible:outline-[var(--cx-focus)]",
      )}
    >
      {app.icon ? (
        // eslint-disable-next-line @next/next/no-img-element -- data URL rendered by the service host
        <img src={app.icon} alt="" width={36} height={36} draggable={false} className="size-9" />
      ) : (
        <span className="flex size-9 items-center justify-center rounded-lg bg-cx-active text-cx-base font-semibold text-cx-fg-2">
          {app.name.slice(0, 1)}
        </span>
      )}
      <span aria-hidden="true" className={cn("mt-0.5 size-1 rounded-full", app.running ? "bg-cx-fg-3" : "bg-transparent")} />
      <span className="mt-0.5 w-full truncate text-center text-cx-2xs leading-4 text-cx-fg-3">{app.name}</span>
    </div>
  );
}
