"use client";

import { createContext, useCallback, useContext, useEffect, useRef, useState, type ReactNode } from "react";
import { desktopChatBridge, type DesktopNativeState } from "@/lib/desktopChatBridge";
import { admitsConnectionOwner } from "@/lib/requestOwnership";
import { resetAuthForServiceChange } from "@/lib/serviceAuth";
import { DesktopTitlebar, type DesktopHeaderSlotKey, type DesktopHeaderSlots } from "@/components/DesktopTitlebar";
import { Button, Dialog, TextField } from "@muteki/ui";

const EMPTY_HEADER_SLOTS: DesktopHeaderSlots = { chat: null, src: null, default: null };

type DesktopChrome = {
  native: boolean;
  origin: string;
  connected: boolean;
  headerSlot: HTMLElement | null;
  headerSlots: DesktopHeaderSlots;
  sidebarExpanded: boolean;
  setSidebarExpanded: (expanded: boolean) => void;
  configure: () => void;
  help: () => void;
};
const DesktopChromeContext = createContext<DesktopChrome | null>(null);
export const useDesktopChrome = () => useContext(DesktopChromeContext);

export function DesktopConnection({ children }: { children: ReactNode }) {
  const [ready, setReady] = useState(false);
  const [native, setNative] = useState(false);
  const [snapshot, setSnapshot] = useState<DesktopNativeState | null>(null);
  const [origin, setOrigin] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [headerSlots, setHeaderSlots] = useState<DesktopHeaderSlots>(EMPTY_HEADER_SLOTS);
  const setHeaderSlot = useCallback((key: DesktopHeaderSlotKey, element: HTMLDivElement | null) => {
    setHeaderSlots(current => current[key] === element ? current : { ...current, [key]: element });
  }, []);
  const [sidebarExpanded, setSidebarExpanded] = useState(true);
  const [helpOpen, setHelpOpen] = useState(false);
  const snapshotRef = useRef<DesktopNativeState | null>(null);
  const mountedRef = useRef(true);
  const stateEventsRef = useRef(0);
  const acceptSnapshot = (next: DesktopNativeState, replaceOrigin = false) => {
    if (!mountedRef.current) return;
    const previous = snapshotRef.current;
    if (!admitsConnectionOwner(previous, next)) return;
    snapshotRef.current = next;
    if (next.origin && typeof next.transportOrigin === "string") resetAuthForServiceChange(next.origin, next.transportOrigin);
    setSnapshot(next);
    setOrigin(current => replaceOrigin ? next.origin || current : current || next.origin || "");
    setReady(true);
  };
  useEffect(() => {
    mountedRef.current = true;
    const bridge = desktopChatBridge();
    if (!bridge?.getState) { setReady(true); return; }
    setNative(true);
    let active = true;
    const accept = (state: DesktopNativeState) => {
      if (!active) return;
      stateEventsRef.current += 1;
      acceptSnapshot(state);
    };
    const off = bridge.onState?.(accept);
    void bridge.getState().then((state) => {
      if (active && (stateEventsRef.current === 0 || (state.connectionVersion ?? 0) > (snapshotRef.current?.connectionVersion ?? 0))) acceptSnapshot(state);
    }).catch(cause => { if (active && stateEventsRef.current === 0) { setError(String(cause?.message || cause)); setReady(true); } });
    return () => { active = false; mountedRef.current = false; off?.(); };
  }, []);
  const perform = async (operation: "connect" | "local" | "configure" | "resume") => {
    const bridge = desktopChatBridge(); if (!bridge || busy) return;
    setBusy(true); setError("");
    const eventGeneration = stateEventsRef.current;
    try {
      const method = operation === "local" ? bridge.connectLocal : operation === "connect" ? () => bridge.connect?.(origin) : bridge[operation];
      if (!method) throw new Error("桌面服务连接接口尚未就绪");
      const state = await method();
      if (state && (eventGeneration === stateEventsRef.current || (state.connectionVersion ?? 0) > (snapshotRef.current?.connectionVersion ?? 0))) {
        acceptSnapshot(state, true);
      }
    } catch (cause) { if (mountedRef.current && eventGeneration === stateEventsRef.current) setError(cause instanceof Error ? cause.message : String((cause as { message?: string })?.message || cause)); }
    finally { setBusy(false); }
  };
  if (!ready) return <div role="status" style={{ padding: 32 }}>正在连接工作台…</div>;
  if (!native) return <>{children}</>;
  const configuring = snapshot?.configuring || !snapshot;
  const connectionKey = `${snapshot?.serviceId}:${snapshot?.identityId}:${snapshot?.connectionVersion}`;
  return <DesktopChromeContext.Provider value={{ native: true, origin: snapshot?.origin || "", connected: snapshot?.status === "connected", headerSlot: headerSlots.default, headerSlots, sidebarExpanded, setSidebarExpanded, configure: () => void perform("configure"), help: () => setHelpOpen(true) }}>
    <div className="desktop-window">
      <DesktopTitlebar setHeaderSlot={setHeaderSlot} configuring={configuring} sidebarExpanded={sidebarExpanded} />
      <div className="desktop-window-content" key={connectionKey} inert={configuring || undefined} aria-hidden={configuring || undefined}>{children}</div>
    </div>
    <Dialog
      open={configuring}
      title="打开你的工作台"
      description="使用本机服务，或连接已有服务。"
      size="sm"
      className="cx-root"
      dismissable={!!snapshot && !busy}
      hideClose={!snapshot || busy}
      onOpenChange={open => { if (!open && snapshot) void perform("resume"); }}
      footer={<>
        {snapshot && <Button type="button" variant="ghost" disabled={busy} onClick={() => void perform("resume")}>返回原工作台</Button>}
        <Button type="submit" form="desktop-connection-form" variant="primary" loading={busy} disabled={busy}>{busy ? "正在连接…" : "连接工作台"}</Button>
      </>}
    >
      <div className="flex flex-col gap-4">
        <div className="flex flex-col gap-2">
          <Button variant="secondary" disabled={busy} onClick={() => void perform("local")}>使用本机工作台</Button>
          <p className="text-cx-xs text-cx-fg-3">使用此 App 配套的本机服务和数据。</p>
        </div>
        <form id="desktop-connection-form" className="flex flex-col gap-3" onSubmit={event => { event.preventDefault(); void perform("connect"); }}>
          <TextField
            id="desktop-service-origin"
            label="或连接已有服务"
            type="url"
            required
            autoComplete="url"
            value={origin}
            onChange={event => setOrigin(event.target.value)}
            disabled={busy}
            placeholder="https://muteki.example.com"
            description="支持 HTTP 或 HTTPS。连接失败时保留原工作台。"
          />
          {(error || snapshot?.message) && <p role="alert" className="whitespace-pre-wrap text-cx-xs text-cx-danger">{error || snapshot?.message}</p>}
        </form>
      </div>
    </Dialog>
    <Dialog open={helpOpen} onOpenChange={setHelpOpen} title="Muteki Desktop" size="sm" className="cx-root" footer={<Button variant="secondary" onClick={() => setHelpOpen(false)}>返回工作台</Button>}>
        <div className="flex flex-col gap-2 text-cx-sm text-cx-fg-2">
          <p>对话、搜索和设置在本地窗口运行。预览在隔离的页面中打开；项目路径需要区分桌面客户端和服务端宿主。</p>
          <p>使用左侧导航切换工作区，通过服务连接入口选择本机工作台或已有服务。</p>
        </div>
    </Dialog>
  </DesktopChromeContext.Provider>;
}
