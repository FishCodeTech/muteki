"use client";

import { useCallback, useEffect, useLayoutEffect, useRef, useState, type CSSProperties } from "react";
import { cn } from "@/lib/cn";
import { Icon } from "@/components/Icon";
import {
  Button,
  Callout,
  IconButton,
  Menu,
  MenuItem,
  MenuLabel,
  MenuSeparator,
  Spinner,
} from "@/components/chat/ui";
import type { SurfaceProps } from "@/components/chat/panel/types";
import { chatPanel, usePreviewRequest, type ChatSurface } from "@/lib/chatPanelStore";
import { isLocalPreviewUrl, normalizePreviewUrl } from "@/lib/previewUrlDetect";
import { classifyBrowserEmbedLoad, inspectIframeAccess } from "@/lib/browserEmbedLoad";
import { browserPreviewActionCopy, readIframeLocation } from "@/lib/browserLocation";
import {
  beginNavGeneration,
  commitNavDocument,
  isStaleDocumentForPendingNav,
  retainFailedTargetOnTimeout,
  resolveErrorRecoveryUrl,
  retryNavAfterTimeout,
  shouldAcceptSpaFrameHref,
  shouldPollSpaLocation,
} from "@/lib/previewNavOwnership";
import { buildWorkspaceBrowserProxyPath, unwrapWorkspaceBrowserProxyHref } from "@/lib/workspaceBrowserProxy";
import { AddressBar } from "@/components/chat/preview/AddressBar";
import { PreviewStart } from "@/components/chat/preview/PreviewStart";
import {
  DEVICE_PRESETS,
  ZOOM_OPTIONS,
  devicePreset,
  deviceSize,
  readDeviceSettings,
  writeDeviceSettings,
  type DeviceSettings,
} from "@/components/chat/preview/devices";

type PreviewSurfaceModel = Extract<ChatSurface, { kind: "preview" }>;
type LoadState = "idle" | "loading" | "loaded" | "loaded_uncertain" | "timeout";
/** Why the next iframe load happens: our own navigation (replace history entry) or an in-page one (push). */
type NavKind = "push" | "traverse" | "reload" | "replace";

interface NavHistory {
  entries: string[];
  index: number;
}

const LOAD_TIMEOUT_MS = 12_000;
const SLOW_LOAD_MS = 2_500;
const STAGE_PADDING = 24;
const ENV_NOTICE_KEY = "muteki.chat.preview.envNotice.dismissed";

const BACKEND_ORIGIN = (process.env.NEXT_PUBLIC_MUTEKI_API || "").replace(/\/+$/, "") || null;
const BACKEND_IS_REMOTE = Boolean(BACKEND_ORIGIN && !/^https?:\/\/(localhost|127\.0\.0\.1)(:\d+)?/i.test(BACKEND_ORIGIN));
const ENV_HINT = BACKEND_IS_REMOTE
  ? `localhost 指向此浏览器所在设备（客户端），而非执行机。访问执行机上的服务请使用执行机实际地址（如 ${BACKEND_ORIGIN}）。`
  : "localhost 指向此浏览器所在设备（客户端）。本地运行时两者相同。";

function mountSrc(url: string): string {
  return buildWorkspaceBrowserProxyPath(url) ?? url;
}

function pushEntry(history: NavHistory, url: string): NavHistory {
  if (history.entries[history.index] === url) return history;
  const entries = [...history.entries.slice(0, history.index + 1), url].slice(-50);
  return { entries, index: entries.length - 1 };
}

function replaceEntry(history: NavHistory, url: string): NavHistory {
  if (history.index < 0) return { entries: [url], index: 0 };
  if (history.entries[history.index] === url) return history;
  const entries = [...history.entries];
  entries[history.index] = url;
  return { entries, index: history.index };
}

function readFrameTitle(frame: HTMLIFrameElement): string | undefined {
  try {
    return frame.contentDocument?.title?.trim() || undefined;
  } catch {
    return undefined;
  }
}

function frameIsReadable(frame: HTMLIFrameElement | null): boolean {
  if (!frame) return false;
  try {
    return Boolean(frame.contentDocument && frame.contentWindow?.location.href && frame.contentWindow.location.href !== "about:blank");
  } catch {
    return false;
  }
}

function openExternal(url: string) {
  if (url) window.open(url, "_blank", "noopener,noreferrer");
}

function EmbedHint({ url }: { url: string }) {
  return (
    <Callout
      tone="warning"
      role="status"
      title="页面可能设置了 X-Frame-Options 或 CSP，拒绝在 iframe 中嵌入。"
      className="shadow-cx-md"
      action={<Button size="xs" variant="secondary" icon="externalLink" onClick={() => openExternal(url)}>在浏览器中打开</Button>}
    >
      请点击「在浏览器中打开」直接访问，或确认目标服务允许嵌入。
    </Callout>
  );
}

function DeviceMenu({ settings, onChange }: { settings: DeviceSettings; onChange: (next: DeviceSettings) => void }) {
  const preset = devicePreset(settings.device);
  const responsive = preset.id === "responsive";
  return (
    <Menu
      placement="bottom-end"
      ariaLabel="设备与缩放"
      className="min-w-[220px]"
      trigger={<IconButton icon={preset.icon} label="设备与缩放" active={!responsive} />}
    >
      <MenuLabel>设备</MenuLabel>
      {DEVICE_PRESETS.map((item) => (
        <MenuItem
          key={item.id}
          icon={item.icon}
          checked={settings.device === item.id}
          hint={item.width ? `${item.width} × ${item.height}` : undefined}
          onSelect={() => onChange({ ...settings, device: item.id })}
        >
          {item.label}
        </MenuItem>
      ))}
      <MenuSeparator />
      <MenuItem
        icon="rotateCw"
        keepOpen
        disabled={responsive}
        checked={settings.rotated}
        onSelect={() => onChange({ ...settings, rotated: !settings.rotated })}
      >
        横屏
      </MenuItem>
      <MenuLabel>缩放</MenuLabel>
      {ZOOM_OPTIONS.map((item) => (
        <MenuItem
          key={String(item.value)}
          icon={item.value === "fit" ? "maximize" : "zoomIn"}
          keepOpen
          disabled={responsive}
          checked={settings.zoom === item.value}
          onSelect={() => onChange({ ...settings, zoom: item.value })}
        >
          {item.label}
        </MenuItem>
      ))}
    </Menu>
  );
}

export function PreviewSurface({ surface, threadId, active, onCiteToComposer }: SurfaceProps<PreviewSurfaceModel>) {
  const initialUrl = surface.url ?? "";
  const [url, setUrl] = useState(initialUrl);
  const [draft, setDraft] = useState(initialUrl);
  const [iframeSrc, setIframeSrc] = useState(() => (initialUrl ? mountSrc(initialUrl) : ""));
  const [frameKey, setFrameKey] = useState(0);
  const [loadState, setLoadState] = useState<LoadState>(initialUrl ? "loading" : "idle");
  const [slow, setSlow] = useState(false);
  const [trackable, setTrackable] = useState(true);
  const [hintOpen, setHintOpen] = useState(false);
  const [history, setHistory] = useState<NavHistory>(() => ({ entries: initialUrl ? [initialUrl] : [], index: initialUrl ? 0 : -1 }));
  const [device, setDevice] = useState<DeviceSettings>(readDeviceSettings);
  const [stage, setStage] = useState({ width: 0, height: 0 });
  const [envDismissed, setEnvDismissed] = useState(true);
  const [failedTarget, setFailedTarget] = useState<string | null>(null);

  const iframeRef = useRef<HTMLIFrameElement | null>(null);
  const addressRef = useRef<HTMLInputElement | null>(null);
  const startInputRef = useRef<HTMLInputElement | null>(null);
  const stageRef = useRef<HTMLDivElement | null>(null);
  const timeoutRef = useRef<number | undefined>(undefined);
  const slowRef = useRef<number | undefined>(undefined);
  const navKindRef = useRef<NavKind | null>(initialUrl ? "push" : null);
  const lastTargetRef = useRef(initialUrl ? mountSrc(initialUrl) : "");
  // #208: requested / in-flight / committed / failed ownership (generation-gated)
  const navGenerationRef = useRef(initialUrl ? 1 : 0);
  const pendingTargetRef = useRef<string | null>(initialUrl || null);
  const committedDocUrlRef = useRef("");
  const failedTargetRef = useRef<string | null>(null);
  const navCommittedRef = useRef(!initialUrl);
  const aliveRef = useRef(true);
  const urlRef = useRef(url);
  const draftRef = useRef(draft);
  urlRef.current = url;
  draftRef.current = draft;
  const hasUrl = Boolean(url);
  const errorRecoveryUrl = resolveErrorRecoveryUrl(failedTarget, url);

  const actionCopy = browserPreviewActionCopy(trackable);

  useEffect(() => {
    aliveRef.current = true;
    return () => { aliveRef.current = false; };
  }, []);

  useEffect(() => {
    try {
      setEnvDismissed(window.localStorage.getItem(ENV_NOTICE_KEY) === "1");
    } catch {
      setEnvDismissed(false);
    }
  }, []);

  const clearTimers = useCallback(() => {
    window.clearTimeout(timeoutRef.current);
    window.clearTimeout(slowRef.current);
    timeoutRef.current = undefined;
    slowRef.current = undefined;
  }, []);

  const beginLoad = useCallback(() => {
    clearTimers();
    setLoadState("loading");
    setSlow(false);
    setHintOpen(false);
    slowRef.current = window.setTimeout(() => setSlow(true), SLOW_LOAD_MS);
    timeoutRef.current = window.setTimeout(() => {
      const failed = retainFailedTargetOnTimeout(pendingTargetRef.current, urlRef.current);
      failedTargetRef.current = failed || null;
      setFailedTarget(failed || null);
      // Keep address on the failed request target (do not let later SPA ticks reclaim old doc).
      if (failed) {
        setUrl(failed);
        setDraft(failed);
        chatPanel.updatePreview(threadId, surface.id, { url: failed });
      }
      setLoadState("timeout");
      setSlow(false);
    }, LOAD_TIMEOUT_MS);
  }, [clearTimers, surface.id, threadId]);

  useEffect(() => {
    if (initialUrl) beginLoad();
    return clearTimers;
    // Mount-only: the initial URL starts loading with the first render.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const mount = useCallback((target: string, kind: NavKind, replacePending = false) => {
    navKindRef.current = kind;
    const src = mountSrc(target);
    lastTargetRef.current = src;
    const frame = iframeRef.current;
    // Readable (same-origin/proxied) frames navigate in place: no remount and
    // no extra entry in the top-level joint session history.
    if (!replacePending && frameIsReadable(frame)) {
      try {
        if (kind === "reload") {
          // Only reload when the readable doc already matches the target.
          // Timeout retry must not reload an old A while recovering B (#208).
          const loc = readIframeLocation(frame!);
          const real = loc.kind === "known" ? unwrapWorkspaceBrowserProxyHref(loc.href) : null;
          if (real && real === target) {
            frame!.contentWindow!.location.reload();
            return;
          }
          frame!.contentWindow!.location.replace(src);
          return;
        }
        frame!.contentWindow!.location.replace(src);
        return;
      } catch {
        // fall through to a remount
      }
    }
    // Retire an in-flight frame before React mounts its replacement. Its late
    // load event cannot claim the generation belonging to the new target.
    iframeRef.current = null;
    setIframeSrc(src);
    setFrameKey((value) => value + 1);
  }, []);

  const commit = useCallback((next: string, kind: NavKind) => {
    const replacePending = pendingTargetRef.current !== null;
    const started = beginNavGeneration(navGenerationRef.current, next);
    navGenerationRef.current = started.generation;
    pendingTargetRef.current = started.pendingTarget;
    navCommittedRef.current = started.navCommitted;
    failedTargetRef.current = null;
    setFailedTarget(null);
    setUrl(next);
    setDraft(next);
    setTrackable(true);
    chatPanel.updatePreview(threadId, surface.id, { url: next, title: undefined });
    beginLoad();
    mount(next, kind, replacePending);
  }, [beginLoad, mount, surface.id, threadId]);

  const navigate = useCallback((input: string) => {
    const next = normalizePreviewUrl(input);
    if (!next) return;
    setHistory((current) => pushEntry(current, next));
    chatPanel.rememberUrl(threadId, next);
    commit(next, "push");
  }, [commit, threadId]);

  const traverse = useCallback((delta: -1 | 1) => {
    const index = history.index + delta;
    const target = history.entries[index];
    if (!target) return;
    setHistory({ entries: history.entries, index });
    commit(target, "traverse");
  }, [commit, history]);

  const reload = useCallback(() => {
    // #208: after timeout, retry the failed target with replace — never reload old readable A.
    const retry = failedTargetRef.current
      ? retryNavAfterTimeout(failedTargetRef.current, urlRef.current)
      : null;
    if (retry) {
      const started = beginNavGeneration(navGenerationRef.current, retry.target);
      navGenerationRef.current = started.generation;
        pendingTargetRef.current = started.pendingTarget;
      navCommittedRef.current = started.navCommitted;
      failedTargetRef.current = null;
      setFailedTarget(null);
      setUrl(retry.target);
      setDraft(retry.target);
      chatPanel.updatePreview(threadId, surface.id, { url: retry.target, title: undefined });
      beginLoad();
      mount(retry.target, "replace", true);
      return;
    }
    if (!urlRef.current) return;
    const replacePending = pendingTargetRef.current !== null;
    const started = beginNavGeneration(navGenerationRef.current, urlRef.current);
    navGenerationRef.current = started.generation;
    pendingTargetRef.current = started.pendingTarget;
    navCommittedRef.current = started.navCommitted;
    beginLoad();
    mount(urlRef.current, "reload", replacePending);
  }, [beginLoad, mount, surface.id, threadId]);

  const stop = useCallback(() => {
    clearTimers();
    try {
      iframeRef.current?.contentWindow?.stop();
    } catch {
      // cross-origin frames can't be stopped; just drop the loading state
    }
    setSlow(false);
    // Mid-nav stop: keep pending target / address; do not mark committed (#208).
    setLoadState("loaded_uncertain");
  }, [clearTimers]);

  const navigateRef = useRef(navigate);
  navigateRef.current = navigate;

  const request = usePreviewRequest(threadId);
  useEffect(() => {
    if (!request || request.surfaceId !== surface.id) return;
    chatPanel.consumePreviewRequest(threadId, request.nonce);
    navigateRef.current(request.url);
  }, [request, surface.id, threadId]);

  /** Pull location/title from a readable frame. A null `kind` means an in-page navigation (pushes history). */
  const syncFromFrame = useCallback((frame: HTMLIFrameElement, kind: NavKind | null) => {
    const loc = readIframeLocation(frame);
    if (loc.kind !== "known") return false;
    const real = unwrapWorkspaceBrowserProxyHref(loc.href);
    const title = readFrameTitle(frame);
    const changed = real !== urlRef.current;
    if (draftRef.current === urlRef.current) setDraft(real);
    setUrl(real);
    setTrackable(true);
    setHistory((current) => (kind ? replaceEntry(current, real) : pushEntry(current, real)));
    if (changed && !kind) chatPanel.rememberUrl(threadId, real);
    chatPanel.updatePreview(threadId, surface.id, { url: real, title });
    return true;
  }, [surface.id, threadId]);

  const handleLoad = (event: React.SyntheticEvent<HTMLIFrameElement>) => {
    const frame = event.currentTarget;
    if (frame !== iframeRef.current) return;
    const eventGeneration = Number(frame.dataset.navGeneration);
    const loc = readIframeLocation(frame);
    const frameHref = loc.kind === "known" ? unwrapWorkspaceBrowserProxyHref(loc.href) : null;
    if (
      isStaleDocumentForPendingNav({
        frameHref,
        pendingTarget: pendingTargetRef.current,
        committedDocUrl: committedDocUrlRef.current,
        navGeneration: navGenerationRef.current,
        eventGeneration,
      })
    ) {
      // Old readable A fired (or a superseded generation) while pending B/C — ignore.
      return;
    }
    clearTimers();
    setSlow(false);
    const kind = navKindRef.current;
    navKindRef.current = null;
    if (!syncFromFrame(frame, kind)) {
      setTrackable(false);
      const fallback = unwrapWorkspaceBrowserProxyHref(lastTargetRef.current) || urlRef.current;
      if (draftRef.current === urlRef.current) setDraft(fallback);
      setUrl(fallback);
      chatPanel.updatePreview(threadId, surface.id, { url: fallback, title: undefined });
      const committed = commitNavDocument({
        generation: navGenerationRef.current,
        eventGeneration,
        docUrl: fallback,
      });
      if (committed.ok) {
        committedDocUrlRef.current = committed.committedDocUrl;
        pendingTargetRef.current = committed.pendingTarget;
        failedTargetRef.current = committed.failedTarget;
        navCommittedRef.current = committed.navCommitted;
        setFailedTarget(committed.failedTarget);
      }
    } else {
      const committed = commitNavDocument({
        generation: navGenerationRef.current,
        eventGeneration,
        docUrl: frameHref || urlRef.current,
      });
      if (committed.ok) {
        committedDocUrlRef.current = committed.committedDocUrl;
        pendingTargetRef.current = committed.pendingTarget;
        failedTargetRef.current = committed.failedTarget;
        navCommittedRef.current = committed.navCommitted;
        setFailedTarget(committed.failedTarget);
      }
      try {
        frame.contentWindow?.addEventListener("pagehide", () => {
          if (aliveRef.current && !navKindRef.current) beginLoad();
        });
      } catch {
        // frame became cross-origin between checks
      }
    }
    const { loadState: next, autoExpandBlockedHint } = classifyBrowserEmbedLoad(inspectIframeAccess(frame));
    setLoadState(next);
    if (autoExpandBlockedHint) setHintOpen(true);
  };

  // SPA route changes (pushState) fire no load event; poll only after current nav commits (#208).
  useEffect(() => {
    if (
      !shouldPollSpaLocation({
        active,
        trackable,
        hasUrl: Boolean(url),
        loadState,
        navCommitted: navCommittedRef.current,
      })
    ) {
      return;
    }
    const timer = window.setInterval(() => {
      // Re-check ownership each tick — timeout/stop/new nav may have flipped refs.
      if (
        !shouldPollSpaLocation({
          active: true,
          trackable: true,
          hasUrl: Boolean(urlRef.current),
          loadState: "loaded", // interval only exists while effect's loadState allowed it; block via refs
          navCommitted: navCommittedRef.current,
        })
      ) {
        return;
      }
      if (pendingTargetRef.current || failedTargetRef.current) return;
      const frame = iframeRef.current;
      if (!frame) return;
      const loc = readIframeLocation(frame);
      if (loc.kind !== "known") return;
      const real = unwrapWorkspaceBrowserProxyHref(loc.href);
      if (
        shouldAcceptSpaFrameHref({
          frameHref: real,
          currentUrl: urlRef.current,
          navCommitted: navCommittedRef.current,
          pendingTarget: pendingTargetRef.current,
        })
      ) {
        syncFromFrame(frame, null);
        committedDocUrlRef.current = real;
        return;
      }
      const title = readFrameTitle(frame);
      if (title && title !== surface.title) chatPanel.updatePreview(threadId, surface.id, { title });
    }, 1000);
    return () => window.clearInterval(timer);
  }, [active, loadState, surface.id, surface.title, syncFromFrame, threadId, trackable, url]);

  useEffect(() => {
    if (!active) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (!(event.metaKey || event.ctrlKey) || event.shiftKey || event.altKey || event.code !== "KeyL") return;
      const input = addressRef.current ?? startInputRef.current;
      if (!input) return;
      event.preventDefault();
      input.focus();
      input.select();
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [active]);

  useLayoutEffect(() => {
    const el = stageRef.current;
    if (!el || typeof ResizeObserver === "undefined") return;
    const measure = () => setStage({ width: el.clientWidth, height: el.clientHeight });
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(el);
    return () => observer.disconnect();
  }, [hasUrl]);

  const updateDevice = (next: DeviceSettings) => {
    setDevice(next);
    writeDeviceSettings(next);
  };

  const cite = () => {
    if (!url || !onCiteToComposer) return;
    onCiteToComposer(actionCopy.citeLine(url, new Date().toLocaleString()));
  };

  if (!url) {
    return <PreviewStart threadId={threadId} active={active} inputRef={startInputRef} onNavigate={navigate} />;
  }

  const size = deviceSize(device);
  const scale = size && stage.width
    ? device.zoom === "fit"
      ? Math.max(0.1, Math.min(1, (stage.width - STAGE_PADDING * 2) / size.width, (stage.height - STAGE_PADDING * 2 - 24) / size.height))
      : device.zoom / 100
    : 1;
  const loading = loadState === "loading";
  const showEnvNotice = !envDismissed && isLocalPreviewUrl(url);

  const frameStyle: CSSProperties = size
    ? { width: Math.round(size.width * scale), height: Math.round(size.height * scale) }
    : { position: "absolute", inset: 0 };
  const canvasStyle: CSSProperties = size
    ? { width: size.width, height: size.height, transform: `scale(${scale})`, transformOrigin: "0 0" }
    : { width: "100%", height: "100%" };

  return (
    <div className="flex min-h-0 flex-1 flex-col" data-testid="preview-surface">
      <div className="relative flex h-10 shrink-0 items-center gap-0.5 border-b border-cx-border-subtle px-1.5">
        <IconButton icon="arrowLeft" label="后退" disabled={history.index <= 0} onClick={() => traverse(-1)} />
        <IconButton icon="arrowRight" label="前进" disabled={history.index >= history.entries.length - 1} onClick={() => traverse(1)} />
        {loading ? (
          <IconButton icon="x" label="停止加载" onClick={stop} />
        ) : (
          <IconButton icon="refresh" label={actionCopy.refreshAriaLabel} onClick={reload} />
        )}
        <div className="mx-1 flex min-w-0 flex-1">
          <AddressBar
            value={draft}
            committed={url}
            trackable={trackable}
            inputRef={addressRef}
            onValueChange={setDraft}
            onSubmit={(value) => { if (value.trim()) navigate(value); else setDraft(url); }}
          />
        </div>
        {onCiteToComposer ? <IconButton icon="quote" label="引用到输入框" tooltip={actionCopy.citeAriaLabel} onClick={cite} /> : null}
        <IconButton icon="externalLink" label="在浏览器中打开" onClick={() => openExternal(errorRecoveryUrl)} />
        <DeviceMenu settings={device} onChange={updateDevice} />
        {loading ? (
          <div className="pointer-events-none absolute inset-x-0 -bottom-px h-[2px] overflow-hidden" aria-hidden>
            <div className="cx-preview-progress h-full w-full bg-cx-accent" />
          </div>
        ) : null}
      </div>

      {showEnvNotice ? (
        <div role="note" className="flex shrink-0 items-start gap-2 border-b border-cx-border-subtle bg-cx-bg-subtle py-1.5 pl-3 pr-1.5 text-[11.5px] leading-[18px] text-cx-fg-3">
          <Icon name="info" size={12} className="mt-[3px] shrink-0 text-cx-fg-4" />
          <span className="min-w-0 flex-1">{ENV_HINT}</span>
          <button
            type="button"
            aria-label="不再提示"
            onClick={() => {
              setEnvDismissed(true);
              try { window.localStorage.setItem(ENV_NOTICE_KEY, "1"); } catch { /* storage blocked */ }
            }}
            className="grid size-5 shrink-0 place-items-center rounded-md text-cx-fg-4 hover:bg-cx-hover hover:text-cx-fg"
          >
            <Icon name="x" size={11} />
          </button>
        </div>
      ) : null}

      <div
        ref={stageRef}
        className={cn("group/stage relative min-h-0 flex-1", size ? "cx-preview-stage cx-scroll overflow-auto" : "overflow-hidden")}
      >
        <div className={size ? "flex min-h-full w-max min-w-full flex-col items-center justify-center gap-2.5 p-6" : "absolute inset-0"}>
          <div
            className={cn("relative shrink-0 overflow-hidden bg-white", size && "rounded-xl border border-cx-border shadow-cx-md")}
            style={frameStyle}
          >
            <div style={canvasStyle}>
              <iframe
                ref={iframeRef}
                key={frameKey}
                data-nav-generation={navGenerationRef.current}
                src={iframeSrc}
                title="工作区浏览器预览"
                sandbox="allow-forms allow-modals allow-popups allow-same-origin allow-scripts"
                onLoad={handleLoad}
                className="block h-full w-full border-0 bg-white"
              />
            </div>
          </div>
          {size ? (
            <p className="cx-tabular select-none text-[11.5px] text-cx-fg-4">
              {size.width} × {size.height} · {Math.round(scale * 100)}%
            </p>
          ) : null}
        </div>

        {loading && slow ? (
          <div className="pointer-events-none absolute inset-x-0 bottom-4 flex flex-col items-center gap-2 px-4" aria-live="polite">
            {hintOpen ? <div className="pointer-events-auto w-full max-w-[420px]"><EmbedHint url={url} /></div> : null}
            <div className="cx-animate-in pointer-events-auto flex h-8 items-center gap-2 rounded-full bg-cx-overlay pl-3 pr-1 text-[12px] text-cx-fg-2 shadow-cx-pop">
              <Spinner size={12} />
              正在加载…
              <Button size="xs" variant="ghost" className="rounded-full" onClick={() => setHintOpen((value) => !value)}>
                无法嵌入？
              </Button>
            </div>
          </div>
        ) : null}

        {loadState === "timeout" ? (
          <div role="alert" className="cx-animate-in absolute inset-0 z-10 grid place-items-center bg-[color-mix(in_srgb,var(--cx-bg)_88%,transparent)] p-6 backdrop-blur-[2px]">
            <div className="flex w-full max-w-[380px] flex-col items-center text-center">
              <span className="mb-3 grid size-11 place-items-center rounded-2xl bg-cx-warning-soft text-cx-warning">
                <Icon name="alert" size={20} />
              </span>
              <p className="text-[14px] font-medium text-cx-fg">加载超时或无法连接</p>
              <p className="mt-1 text-[12.5px] leading-5 text-cx-fg-3">目标服务可能尚未启动、地址有误，或拒绝在面板中嵌入。</p>
              <div className="mt-4 flex items-center gap-2">
                <Button size="sm" variant="secondary" icon="refresh" onClick={reload}>重试</Button>
                <Button size="sm" variant="ghost" icon="externalLink" onClick={() => openExternal(errorRecoveryUrl)}>在浏览器中打开</Button>
              </div>
              <Button size="xs" variant="link" className="mt-3 text-cx-fg-3" onClick={() => setHintOpen((value) => !value)}>
                可能拒绝嵌入？
              </Button>
              {hintOpen ? <div className="mt-3 w-full text-left"><EmbedHint url={errorRecoveryUrl} /></div> : null}
            </div>
          </div>
        ) : null}

        {loadState === "loaded" || loadState === "loaded_uncertain" ? (
          <div className="pointer-events-none absolute bottom-3 right-3 flex max-w-[calc(100%-24px)] flex-col items-end gap-2">
            {hintOpen ? <div className="pointer-events-auto w-[min(420px,calc(100vw-48px))]"><EmbedHint url={url} /></div> : null}
            <Button
              size="xs"
              variant="secondary"
              className={cn("pointer-events-auto rounded-full transition-opacity duration-200", hintOpen ? "opacity-100" : "opacity-0 focus-visible:opacity-100 group-hover/stage:opacity-100")}
              onClick={() => setHintOpen((value) => !value)}
            >
              {hintOpen ? "收起" : "内容空白？可能被拒绝嵌入"}
            </Button>
          </div>
        ) : null}
      </div>
    </div>
  );
}
