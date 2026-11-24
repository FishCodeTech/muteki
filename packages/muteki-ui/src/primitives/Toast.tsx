"use client";

import { useEffect, useState, useSyncExternalStore, type ReactNode } from "react";
import { AnimatePresence, motion } from "motion/react";
import { cn } from "../utils/cn";
import { useReducedMotion } from "../utils/reducedMotion";
import type { IconName } from "../icons/iconNames";
import { EASE_OUT, SPRING_PANEL } from "./motion";
import { Portal } from "./Portal";

function ToastSvg({ name, size = 16, className }: { name: IconName; size?: number; className?: string }) {
  const props = {
    width: size,
    height: size,
    viewBox: "0 0 24 24",
    fill: "none",
    stroke: "currentColor",
    strokeWidth: 1.5,
    strokeLinecap: "round" as const,
    strokeLinejoin: "round" as const,
    "aria-hidden": true as const,
    className,
    style: { flex: "none", display: "inline-flex" } as const,
  };
  switch (name) {
    case "checkCircle":
      return <svg {...props}><circle cx="12" cy="12" r="10" /><path d="m9 12 2 2 4-4" /></svg>;
    case "circleAlert":
      return <svg {...props}><circle cx="12" cy="12" r="10" /><line x1="12" x2="12" y1="8" y2="12" /><line x1="12" x2="12.01" y1="16" y2="16" /></svg>;
    case "alert":
      return <svg {...props}><path d="m21.73 18-8-14a2 2 0 0 0-3.48 0l-8 14A2 2 0 0 0 4 21h16a2 2 0 0 0 1.73-3" /><path d="M12 9v4" /><path d="M12 17h.01" /></svg>;
    case "cpu":
      return <svg {...props}><rect width="16" height="16" x="4" y="4" rx="2" /><rect width="6" height="6" x="9" y="9" rx="1" /><path d="M15 2v2M15 20v2M2 15h2M2 9h2M20 15h2M20 9h2M9 2v2M9 20v2" /></svg>;
    case "download":
      return <svg {...props}><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4" /><polyline points="7 10 12 15 17 10" /><line x1="12" x2="12" y1="15" y2="3" /></svg>;
    case "x":
      return <svg {...props}><path d="M18 6 6 18" /><path d="m6 6 12 12" /></svg>;
    default:
      return <svg {...props}><circle cx="12" cy="12" r="10" /><path d="M12 16v-4" /><path d="M12 8h.01" /></svg>;
  }
}

export type ToastTone = "neutral" | "success" | "danger" | "warning" | "info";

export interface ToastInput {
  title: ReactNode;
  description?: ReactNode;
  tone?: ToastTone;
  icon?: IconName;
  action?: { label: string; onClick: () => void };
  duration?: number;
}

export interface ToastItem extends ToastInput {
  id: number;
  tone: ToastTone;
  duration: number;
}

let items: ToastItem[] = [];
let seq = 0;
const listeners = new Set<() => void>();
const emit = () => listeners.forEach((listener) => listener());

let toasterSeq = 0;
const mountedToasters = new Set<number>();
const toasterOwnerListeners = new Set<() => void>();
const emitToasterOwners = () => toasterOwnerListeners.forEach((listener) => listener());

function activeToasterId(): number | null {
  for (const id of mountedToasters) return id;
  return null;
}

function enqueue(input: string | ToastInput, defaults?: Partial<ToastInput>): number {
  seq += 1;
  const normalized: ToastInput = typeof input === "string" ? { title: input } : input;
  const item: ToastItem = {
    tone: "neutral",
    duration: 3600,
    ...defaults,
    ...normalized,
    id: seq,
  };
  items = [...items, item].slice(-4);
  emit();
  return item.id;
}

export function dismissToast(id: number) {
  items = items.filter((item) => item.id !== id);
  emit();
}

export type ToastOptionsArg = Omit<ToastInput, "title">;

function toastFn(input: string | ToastInput, options?: ToastOptionsArg): number {
  const payload = typeof input === "string" ? { ...options, title: input } : { ...options, ...input };
  return enqueue(payload);
}

export const toast = Object.assign(toastFn, {
  success: (input: string | ToastInput, options?: ToastOptionsArg) =>
    enqueue(typeof input === "string" ? { ...options, title: input } : { ...options, ...input }, { tone: "success" }),
  danger: (input: string | ToastInput, options?: ToastOptionsArg) =>
    enqueue(typeof input === "string" ? { ...options, title: input } : { ...options, ...input }, { tone: "danger" }),
  error: (input: string | ToastInput, options?: ToastOptionsArg) =>
    enqueue(typeof input === "string" ? { ...options, title: input } : { ...options, ...input }, { tone: "danger" }),
  warning: (input: string | ToastInput, options?: ToastOptionsArg) =>
    enqueue(typeof input === "string" ? { ...options, title: input } : { ...options, ...input }, { tone: "warning" }),
  info: (input: string | ToastInput, options?: ToastOptionsArg) =>
    enqueue(typeof input === "string" ? { ...options, title: input } : { ...options, ...input }, { tone: "info" }),
  dismiss: dismissToast,
});

function useToasts(): ToastItem[] {
  return useSyncExternalStore(
    (listener) => { listeners.add(listener); return () => listeners.delete(listener); },
    () => items,
    () => items,
  );
}

const TONE_ICON: Record<ToastTone, { icon: IconName; className: string }> = {
  neutral: { icon: "info", className: "text-cx-fg-3" },
  info: { icon: "info", className: "text-[var(--ui-info)]" },
  success: { icon: "checkCircle", className: "text-cx-success" },
  danger: { icon: "circleAlert", className: "text-cx-danger" },
  warning: { icon: "alert", className: "text-cx-warning" },
};

function ToastCard({ item }: { item: ToastItem }) {
  useEffect(() => {
    if (!item.duration) return;
    const timer = window.setTimeout(() => dismissToast(item.id), item.duration);
    return () => window.clearTimeout(timer);
  }, [item.duration, item.id]);
  const tone = TONE_ICON[item.tone];
  return (
    <div
      role="status"
      data-slot="toast"
      data-tone={item.tone}
      className="pointer-events-auto flex w-[340px] max-w-[calc(100vw-32px)] items-start gap-2.5 rounded-[12px] bg-cx-overlay px-3.5 py-3 shadow-cx-pop dark:shadow-[var(--ui-sheen),var(--cx-shadow-pop)]"
    >
      <ToastSvg name={item.icon ?? tone.icon} size={16} className={cn("mt-[2px] shrink-0", tone.className)} />
      <div className="min-w-0 flex-1">
        <p className="text-cx-sm font-medium leading-5 text-cx-fg">{item.title}</p>
        {item.description ? <p className="mt-0.5 text-cx-xs leading-5 text-cx-fg-3">{item.description}</p> : null}
      </div>
      {item.action ? (
        <button
          type="button"
          onClick={() => { item.action?.onClick(); dismissToast(item.id); }}
          className="shrink-0 rounded-[6px] px-2 py-0.5 text-cx-xs font-medium text-cx-accent hover:bg-cx-accent-soft"
        >
          {item.action.label}
        </button>
      ) : null}
      <button
        type="button"
        aria-label="关闭通知"
        onClick={() => dismissToast(item.id)}
        className="-mr-1 grid size-5 shrink-0 place-items-center rounded text-cx-fg-4 hover:bg-cx-hover hover:text-cx-fg"
      >
        <ToastSvg name="x" size={12} />
      </button>
    </div>
  );
}

export function Toaster() {
  const [instanceId] = useState(() => ++toasterSeq);
  useEffect(() => {
    mountedToasters.add(instanceId);
    emitToasterOwners();
    return () => {
      mountedToasters.delete(instanceId);
      emitToasterOwners();
    };
  }, [instanceId]);
  const ownerId = useSyncExternalStore(
    (listener) => { toasterOwnerListeners.add(listener); return () => toasterOwnerListeners.delete(listener); },
    activeToasterId,
    () => null,
  );
  const list = useToasts();
  const reduced = useReducedMotion();
  if (ownerId !== instanceId) return null;
  return (
    <Portal unscoped>
      <div aria-live="polite" data-slot="toaster" className="pointer-events-none fixed bottom-4 right-4 z-(--z-toast) flex flex-col items-end gap-2">
        <AnimatePresence initial={false}>
          {list.map((item) => (
            <motion.div
              key={item.id}
              layout={!reduced}
              initial={reduced ? { opacity: 0 } : { opacity: 0, transform: "translateY(16px) scale(0.96)" }}
              animate={
                reduced
                  ? { opacity: 1, transition: { duration: 0.14, ease: EASE_OUT } }
                  : { opacity: 1, transform: "translateY(0px) scale(1)", transition: SPRING_PANEL }
              }
              exit={
                reduced
                  ? { opacity: 0, transition: { duration: 0.12 } }
                  : { opacity: 0, transform: "scale(0.96)", transition: { duration: 0.14, ease: EASE_OUT } }
              }
            >
              <ToastCard item={item} />
            </motion.div>
          ))}
        </AnimatePresence>
      </div>
    </Portal>
  );
}
