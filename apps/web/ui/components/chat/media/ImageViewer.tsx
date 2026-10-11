"use client";

import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type KeyboardEvent as ReactKeyboardEvent,
  type PointerEvent as ReactPointerEvent,
  type ReactNode,
} from "react";
import { AnimatePresence, motion } from "motion/react";
import { cn } from "@/lib/cn";
import { Icon } from "@/components/Icon";
import {
  Button,
  IconButton,
  LayerContext,
  Menu,
  MenuItem,
  MenuSeparator,
  Portal,
  toast,
  useFocusTrap,
  useLayer,
  useRestoreFocus,
  useScrollLock,
  EASE_OUT,
  useReducedMotion,
} from "@/components/chat/ui";

export interface ViewerImage {
  /** Auth-safe URL (blob:, data:, or public http). */
  src: string;
  name: string;
  /** Authenticated download; falls back to downloading `src`. */
  onDownload?: (() => void) | null;
}

const MIN_SCALE = 0.05;
const MAX_SCALE = 8;
const ZOOM_STEPS = [0.05, 0.1, 0.25, 0.33, 0.5, 0.67, 0.75, 1, 1.25, 1.5, 2, 3, 4, 6, 8];

type Size = { w: number; h: number };
type View = { fitted: boolean; scale: number; x: number; y: number; smooth: boolean };

const FITTED: View = { fitted: true, scale: 1, x: 0, y: 0, smooth: false };

function clampScale(value: number): number {
  return Math.min(MAX_SCALE, Math.max(MIN_SCALE, value));
}

function nextStep(current: number, direction: 1 | -1): number {
  if (direction > 0) return ZOOM_STEPS.find((step) => step > current + 0.001) ?? MAX_SCALE;
  return [...ZOOM_STEPS].reverse().find((step) => step < current - 0.001) ?? MIN_SCALE;
}

export function downloadImage(image: ViewerImage) {
  if (image.onDownload) {
    image.onDownload();
    return;
  }
  const anchor = document.createElement("a");
  anchor.href = image.src;
  anchor.download = image.name.split("/").pop() || "image";
  anchor.rel = "noopener";
  anchor.click();
}

async function copyImage(src: string): Promise<void> {
  const blob = await (await fetch(src)).blob();
  // Clipboard image support is PNG-only in Chromium; re-encode anything else.
  let png = blob;
  if (blob.type !== "image/png") {
    const bitmap = await createImageBitmap(blob);
    const canvas = document.createElement("canvas");
    canvas.width = bitmap.width;
    canvas.height = bitmap.height;
    const context = canvas.getContext("2d");
    if (!context) throw new Error("无法创建画布");
    context.drawImage(bitmap, 0, 0);
    png = await new Promise<Blob>((resolve, reject) => canvas.toBlob(
      (value) => (value ? resolve(value) : reject(new Error("图片编码失败"))),
      "image/png",
    ));
  }
  await navigator.clipboard.write([new ClipboardItem({ "image/png": png })]);
}

/**
 * Fit-to-stage zoom/pan state. Fitting never upscales past 100%; pinch or
 * ⌘/Ctrl + wheel zooms around the cursor, plain wheel and drag pan when the
 * image overflows the stage.
 */
export function useImageStage(src: string, padding: number) {
  const stageRef = useRef<HTMLDivElement | null>(null);
  const [box, setBox] = useState<Size>({ w: 0, h: 0 });
  const [natural, setNatural] = useState<Size | null>(null);
  const [failed, setFailed] = useState(false);
  const [view, setView] = useState<View>(FITTED);
  const [dragging, setDragging] = useState(false);

  useEffect(() => {
    setNatural(null);
    setFailed(false);
    setView(FITTED);
  }, [src]);

  useLayoutEffect(() => {
    const stage = stageRef.current;
    if (!stage) return;
    const measure = () => setBox({ w: stage.clientWidth, h: stage.clientHeight });
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(stage);
    return () => observer.disconnect();
  }, []);

  const fitScale = natural && box.w > 0 && box.h > 0
    ? clampScale(Math.min(1, (box.w - padding * 2) / natural.w, (box.h - padding * 2) / natural.h))
    : 1;
  const scale = view.fitted ? fitScale : view.scale;

  const clampPan = useCallback((s: number, x: number, y: number) => {
    if (!natural) return { x: 0, y: 0 };
    const limitX = Math.max(0, (natural.w * s - box.w) / 2 + padding);
    const limitY = Math.max(0, (natural.h * s - box.h) / 2 + padding);
    return {
      x: Math.min(limitX, Math.max(-limitX, x)),
      y: Math.min(limitY, Math.max(-limitY, y)),
    };
  }, [natural, box.w, box.h, padding]);

  const pan = view.fitted ? { x: 0, y: 0 } : clampPan(scale, view.x, view.y);
  const overflows = Boolean(natural) && (natural!.w * scale > box.w + 0.5 || natural!.h * scale > box.h + 0.5);

  const live = useRef({ scale, pan, fitScale, overflows, clampPan });
  live.current = { scale, pan, fitScale, overflows, clampPan };

  /** `anchor` is relative to the stage centre, in CSS pixels. */
  const zoomTo = useCallback((target: number, anchor?: { x: number; y: number }, smooth = true) => {
    const { scale: current, pan: offset, clampPan: clamp } = live.current;
    const next = clampScale(target);
    const ax = anchor?.x ?? 0;
    const ay = anchor?.y ?? 0;
    const ratio = next / current;
    const moved = clamp(next, ax - (ax - offset.x) * ratio, ay - (ay - offset.y) * ratio);
    setView({ fitted: false, scale: next, x: moved.x, y: moved.y, smooth });
  }, []);

  const fit = useCallback(() => setView({ ...FITTED, smooth: true }), []);
  const zoomIn = useCallback(() => zoomTo(nextStep(live.current.scale, 1)), [zoomTo]);
  const zoomOut = useCallback(() => zoomTo(nextStep(live.current.scale, -1)), [zoomTo]);

  const anchorFor = (clientX: number, clientY: number) => {
    const rect = stageRef.current!.getBoundingClientRect();
    return { x: clientX - rect.left - rect.width / 2, y: clientY - rect.top - rect.height / 2 };
  };

  useEffect(() => {
    const stage = stageRef.current;
    if (!stage) return;
    const onWheel = (event: WheelEvent) => {
      const state = live.current;
      if (event.ctrlKey || event.metaKey) {
        event.preventDefault();
        // Trackpad pinch reports small deltas; clamp mouse-wheel notches.
        const delta = Math.max(-60, Math.min(60, event.deltaY));
        const rect = stage.getBoundingClientRect();
        zoomTo(state.scale * Math.exp(-delta * 0.01), {
          x: event.clientX - rect.left - rect.width / 2,
          y: event.clientY - rect.top - rect.height / 2,
        }, false);
        return;
      }
      if (!state.overflows) return;
      event.preventDefault();
      const moved = state.clampPan(state.scale, state.pan.x - event.deltaX, state.pan.y - event.deltaY);
      setView({ fitted: false, scale: state.scale, x: moved.x, y: moved.y, smooth: false });
    };
    stage.addEventListener("wheel", onWheel, { passive: false });
    return () => stage.removeEventListener("wheel", onWheel);
  }, [zoomTo]);

  const drag = useRef<{ id: number; x: number; y: number; ox: number; oy: number; moved: boolean } | null>(null);
  const dragMoved = useRef(false);

  const stageHandlers = {
    onPointerDown: (event: ReactPointerEvent<HTMLDivElement>) => {
      dragMoved.current = false;
      if (event.button !== 0 || !live.current.overflows) return;
      drag.current = { id: event.pointerId, x: event.clientX, y: event.clientY, ox: live.current.pan.x, oy: live.current.pan.y, moved: false };
      event.currentTarget.setPointerCapture(event.pointerId);
    },
    onPointerMove: (event: ReactPointerEvent<HTMLDivElement>) => {
      const state = drag.current;
      if (!state || state.id !== event.pointerId) return;
      const dx = event.clientX - state.x;
      const dy = event.clientY - state.y;
      if (!state.moved && Math.hypot(dx, dy) < 3) return;
      state.moved = true;
      dragMoved.current = true;
      setDragging(true);
      const { scale: current, clampPan: clamp } = live.current;
      const moved = clamp(current, state.ox + dx, state.oy + dy);
      setView({ fitted: false, scale: current, x: moved.x, y: moved.y, smooth: false });
    },
    onPointerUp: (event: ReactPointerEvent<HTMLDivElement>) => {
      if (drag.current?.id !== event.pointerId) return;
      drag.current = null;
      setDragging(false);
    },
    onPointerCancel: () => {
      drag.current = null;
      setDragging(false);
    },
    onDoubleClick: (event: React.MouseEvent<HTMLDivElement>) => {
      if (!natural) return;
      if (view.fitted) {
        const { fitScale: base } = live.current;
        zoomTo(base < 1 ? 1 : Math.min(MAX_SCALE, base * 2), anchorFor(event.clientX, event.clientY));
      } else {
        fit();
      }
    },
  };

  const keyboard = (event: ReactKeyboardEvent | KeyboardEvent): boolean => {
    if (event.altKey) return false;
    const key = event.key;
    if (key === "+" || key === "=") { zoomIn(); return true; }
    if (key === "-" || key === "_") { zoomOut(); return true; }
    if (key === "0") { fit(); return true; }
    if (key === "1" && !event.metaKey && !event.ctrlKey) { zoomTo(1); return true; }
    return false;
  };

  const imageStyle: React.CSSProperties = natural ? {
    width: natural.w * scale,
    height: natural.h * scale,
    transform: `translate(calc(-50% + ${pan.x}px), calc(-50% + ${pan.y}px))`,
    transition: view.smooth && !dragging ? "width 180ms ease, height 180ms ease, transform 180ms ease" : undefined,
  } : { transform: "translate(-50%, -50%)", opacity: 0 };

  return {
    stageRef,
    natural,
    failed,
    scale,
    fitted: view.fitted,
    overflows,
    dragging,
    dragMoved,
    imageStyle,
    stageHandlers,
    keyboard,
    zoomTo,
    zoomIn,
    zoomOut,
    fit,
    onImageLoad: (event: React.SyntheticEvent<HTMLImageElement>) => {
      const img = event.currentTarget;
      setNatural({ w: img.naturalWidth || 1, h: img.naturalHeight || 1 });
    },
    onImageError: () => setFailed(true),
  };
}

type ImageStage = ReturnType<typeof useImageStage>;

export function ZoomMenu({ stage, className }: { stage: ImageStage; className?: string }) {
  const percent = `${Math.round(stage.scale * 100)}%`;
  return (
    <Menu
      placement="bottom-end"
      ariaLabel="缩放"
      className="min-w-[180px]"
      trigger={(
        <Button size="sm" variant="ghost" iconRight="chevronDown" className={cn("cx-tabular min-w-[64px] justify-between", className)} aria-label={`缩放 ${percent}`} data-testid="image-zoom-menu">
          {percent}
        </Button>
      )}
    >
      <MenuItem icon="zoomIn" shortcut="+" keepOpen onSelect={stage.zoomIn}>放大</MenuItem>
      <MenuItem icon="zoomOut" shortcut="-" keepOpen onSelect={stage.zoomOut}>缩小</MenuItem>
      <MenuSeparator />
      <MenuItem checked={stage.fitted} shortcut="0" onSelect={stage.fit}>适应窗口</MenuItem>
      {[0.5, 1, 2].map((value) => (
        <MenuItem key={value} checked={!stage.fitted && Math.abs(stage.scale - value) < 0.001} shortcut={value === 1 ? "1" : undefined} onSelect={() => stage.zoomTo(value)}>
          {value * 100}%
        </MenuItem>
      ))}
    </Menu>
  );
}

/** Image surface with fit/zoom/pan; parent supplies the size. */
export function ImageStageView({
  stage,
  src,
  name,
  className,
  imageClassName,
  onBackgroundClick,
  children,
}: {
  stage: ImageStage;
  src: string;
  name: string;
  className?: string;
  imageClassName?: string;
  onBackgroundClick?: () => void;
  children?: ReactNode;
}) {
  return (
    <div
      ref={stage.stageRef}
      {...stage.stageHandlers}
      onClick={(event) => {
        if (stage.dragMoved.current) return;
        if (event.target === event.currentTarget) onBackgroundClick?.();
      }}
      className={cn(
        "relative min-h-0 touch-none select-none overflow-hidden",
        stage.overflows ? (stage.dragging ? "cursor-grabbing" : "cursor-grab") : undefined,
        className,
      )}
      data-testid="image-stage"
      data-zoom={stage.scale.toFixed(3)}
      data-fitted={stage.fitted ? "true" : "false"}
    >
      {stage.failed ? (
        <div className="absolute inset-0 grid place-items-center text-center text-[13px] text-cx-fg-3" role="alert">
          <span className="flex flex-col items-center gap-2">
            <Icon name="circleAlert" size={20} />
            图片无法显示：文件已损坏或格式不受支持
          </span>
        </div>
      ) : (
        // eslint-disable-next-line @next/next/no-img-element
        <img
          src={src}
          alt={name}
          draggable={false}
          onLoad={stage.onImageLoad}
          onError={stage.onImageError}
          style={stage.imageStyle}
          className={cn("pointer-events-none absolute left-1/2 top-1/2 max-w-none", imageClassName)}
        />
      )}
      {!stage.natural && !stage.failed ? (
        <div className="pointer-events-none absolute inset-0 grid place-items-center text-cx-fg-4">
          <Icon name="loader" size={18} className="animate-spin" />
        </div>
      ) : null}
      {children}
    </div>
  );
}

export function ImageLightbox({
  images,
  index,
  open,
  onOpenChange,
  onIndexChange,
}: {
  images: ViewerImage[];
  index: number;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onIndexChange?: (index: number) => void;
}) {
  const panelRef = useRef<HTMLDivElement | null>(null);
  const reduced = useReducedMotion();
  const [current, setCurrent] = useState(index);
  useEffect(() => { if (open) setCurrent(index); }, [open, index]);
  const safeIndex = Math.min(Math.max(current, 0), Math.max(images.length - 1, 0));
  const image = images[safeIndex];
  const layer = useLayer({ open, refs: [panelRef], outside: false, onDismiss: () => onOpenChange(false) });
  useRestoreFocus(open);
  useFocusTrap(open, panelRef);
  useScrollLock(open);
  useEffect(() => {
    if (!open) return;
    const frame = window.requestAnimationFrame(() => panelRef.current?.focus({ preventScroll: true }));
    return () => window.cancelAnimationFrame(frame);
  }, [open]);

  const go = (delta: number) => {
    if (images.length < 2) return;
    const next = (safeIndex + delta + images.length) % images.length;
    setCurrent(next);
    onIndexChange?.(next);
  };

  return (
    <Portal>
      <AnimatePresence>
        {open && image ? (
          <LayerContext.Provider value={layer}>
            <motion.div
              ref={panelRef}
              key="image-lightbox"
              role="dialog"
              aria-modal="true"
              aria-label={`查看图片 ${image.name}`}
              tabIndex={-1}
              data-cx-layer=""
              data-testid="image-lightbox"
              className="fixed inset-0 z-[1100] flex flex-col bg-[color-mix(in_srgb,var(--cx-bg)_82%,transparent)] text-cx-fg outline-none backdrop-blur-xl"
              initial={{ opacity: 0 }}
              animate={{ opacity: 1, transition: { duration: 0.18, ease: EASE_OUT } }}
              exit={{ opacity: 0, transition: { duration: 0.12 } }}
            >
              <LightboxBody
                key={image.src}
                image={image}
                position={images.length > 1 ? `${safeIndex + 1} / ${images.length}` : null}
                reduced={reduced}
                onClose={() => onOpenChange(false)}
                onPrev={images.length > 1 ? () => go(-1) : undefined}
                onNext={images.length > 1 ? () => go(1) : undefined}
              />
              {images.length > 1 ? (
                <div className="flex shrink-0 justify-center gap-2 overflow-x-auto px-6 pb-5" role="listbox" aria-label="图片列表">
                  {images.map((item, itemIndex) => (
                    <button
                      key={`${item.src}-${itemIndex}`}
                      type="button"
                      role="option"
                      aria-selected={itemIndex === safeIndex}
                      aria-label={item.name}
                      onClick={() => { setCurrent(itemIndex); onIndexChange?.(itemIndex); }}
                      className={cn(
                        "size-12 shrink-0 overflow-hidden rounded-lg bg-cx-sunken transition-[opacity,box-shadow]",
                        itemIndex === safeIndex
                          ? "opacity-100 shadow-[0_0_0_2px_var(--cx-accent)]"
                          : "opacity-60 shadow-[0_0_0_1px_var(--cx-border)] hover:opacity-100",
                      )}
                    >
                      {/* eslint-disable-next-line @next/next/no-img-element */}
                      <img src={item.src} alt="" draggable={false} className="size-full object-cover" />
                    </button>
                  ))}
                </div>
              ) : null}
            </motion.div>
          </LayerContext.Provider>
        ) : null}
      </AnimatePresence>
    </Portal>
  );
}

function LightboxBody({
  image,
  position,
  reduced,
  onClose,
  onPrev,
  onNext,
}: {
  image: ViewerImage;
  position: string | null;
  reduced: boolean;
  onClose: () => void;
  onPrev?: () => void;
  onNext?: () => void;
}) {
  const stage = useImageStage(image.src, 24);
  const [copying, setCopying] = useState(false);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.defaultPrevented || event.isComposing) return;
      if ((event.target as HTMLElement | null)?.closest?.("[role='menu']")) return;
      if (event.key === "ArrowLeft" && onPrev) { event.preventDefault(); onPrev(); return; }
      if (event.key === "ArrowRight" && onNext) { event.preventDefault(); onNext(); return; }
      if ((event.metaKey || event.ctrlKey) && (event.key === "=" || event.key === "+" || event.key === "-" || event.key === "0")) {
        event.preventDefault();
        if (event.key === "0") stage.fit();
        else if (event.key === "-") stage.zoomOut();
        else stage.zoomIn();
        return;
      }
      if (!event.metaKey && !event.ctrlKey && stage.keyboard(event)) event.preventDefault();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  });

  const copy = async () => {
    setCopying(true);
    try {
      await copyImage(image.src);
      toast({ title: "已复制图片" });
    } catch (error) {
      toast({ title: "复制图片失败", description: error instanceof Error ? error.message : String(error), tone: "danger" });
    } finally {
      setCopying(false);
    }
  };

  const dims = stage.natural ? `${stage.natural.w} × ${stage.natural.h}` : null;

  return (
    <>
      <header className="flex h-14 shrink-0 items-center gap-3 px-4" data-testid="image-lightbox-toolbar">
        <span className="flex min-w-0 flex-1 items-center gap-2 text-[13px]">
          <Icon name="image" size={15} className="shrink-0 text-cx-fg-3" />
          <span className="truncate font-medium text-cx-fg" title={image.name}>{image.name}</span>
          {dims ? <span className="cx-tabular shrink-0 text-cx-fg-4">{dims}</span> : null}
          {position ? <span className="cx-tabular shrink-0 text-cx-fg-4">· {position}</span> : null}
        </span>
        <span className="flex shrink-0 items-center gap-1 rounded-xl bg-cx-elevated/80 p-1 shadow-cx-sm ring-1 ring-cx-border-subtle">
          <IconButton size="sm" icon="zoomOut" label="缩小" shortcut="-" onClick={stage.zoomOut} />
          <ZoomMenu stage={stage} />
          <IconButton size="sm" icon="zoomIn" label="放大" shortcut="+" onClick={stage.zoomIn} />
        </span>
        <span className="flex shrink-0 items-center gap-1 rounded-xl bg-cx-elevated/80 p-1 shadow-cx-sm ring-1 ring-cx-border-subtle">
          <IconButton size="sm" icon={copying ? "loader" : "copy"} label="复制图片" disabled={copying} onClick={() => void copy()} />
          <IconButton size="sm" icon="download" label="下载" data-testid="image-lightbox-download" onClick={() => downloadImage(image)} />
          <IconButton size="sm" icon="x" label="关闭" shortcut="esc" data-testid="image-lightbox-close" onClick={onClose} />
        </span>
      </header>
      <motion.div
        className="relative flex min-h-0 flex-1"
        initial={reduced ? { opacity: 0 } : { opacity: 0, scale: 0.97 }}
        animate={{ opacity: 1, scale: 1, transition: { duration: 0.2, ease: EASE_OUT } }}
      >
        <ImageStageView
          stage={stage}
          src={image.src}
          name={image.name}
          className="flex-1"
          imageClassName="rounded-lg shadow-cx-pop ring-1 ring-cx-border-subtle"
          onBackgroundClick={onClose}
        />
        {onPrev ? (
          <IconButton size="lg" icon="chevronLeft" label="上一张" shortcut="left" onClick={onPrev}
            className="absolute left-4 top-1/2 -translate-y-1/2 rounded-full bg-cx-elevated/90 shadow-cx-md ring-1 ring-cx-border-subtle" />
        ) : null}
        {onNext ? (
          <IconButton size="lg" icon="chevronRight" label="下一张" shortcut="right" onClick={onNext}
            className="absolute right-4 top-1/2 -translate-y-1/2 rounded-full bg-cx-elevated/90 shadow-cx-md ring-1 ring-cx-border-subtle" />
        ) : null}
      </motion.div>
    </>
  );
}

/** Lightbox state helper for a single trigger or a gallery. */
export function useImageLightbox(images: ViewerImage[]) {
  const [state, setState] = useState<{ open: boolean; index: number }>({ open: false, index: 0 });
  const openAt = useCallback((index = 0) => setState({ open: true, index }), []);
  const element = (
    <ImageLightbox
      images={images}
      index={state.index}
      open={state.open}
      onOpenChange={(open) => setState((prev) => ({ ...prev, open }))}
      onIndexChange={(index) => setState((prev) => ({ ...prev, index }))}
    />
  );
  return { openAt, element };
}

/** Inline image that opens the lightbox on click (Markdown, tool output). */
export function ExpandableImage({ src, alt, name, className, onError, onDownload }: {
  src: string;
  alt: string;
  name?: string;
  className?: string;
  onError?: () => void;
  onDownload?: (() => void) | null;
}) {
  const lightbox = useImageLightbox([{ src, name: name || alt || "image", onDownload }]);
  return (
    <>
      <button
        type="button"
        aria-label={`查看图片 ${name || alt || ""}`.trim()}
        onClick={() => lightbox.openAt(0)}
        className="cx-md-img-button"
      >
        {/* eslint-disable-next-line @next/next/no-img-element */}
        <img src={src} alt={alt} loading="lazy" decoding="async" draggable={false} className={className} onError={onError} />
      </button>
      {lightbox.element}
    </>
  );
}
