"use client";

import { useEffect, useSyncExternalStore, type ReactNode } from "react";
import { AnimatePresence, motion } from "motion/react";
import { cn } from "@/lib/cn";
import { Icon, type IconName } from "@/components/Icon";
import { Portal } from "./Portal";
import { SPRING_PANEL, useReducedMotion } from "./motion";

type ToastTone = "neutral" | "success" | "danger" | "warning";

export interface ToastItem {
  id: number;
  title: ReactNode;
  description?: ReactNode;
  tone: ToastTone;
  icon?: IconName;
  action?: { label: string; onClick: () => void };
  duration: number;
}

let items: ToastItem[] = [];
let seq = 0;
const listeners = new Set<() => void>();
const emit = () => listeners.forEach((listener) => listener());

export function toast(input: Omit<ToastItem, "id" | "tone" | "duration"> & { tone?: ToastTone; duration?: number }): number {
  seq += 1;
  const item: ToastItem = { tone: "neutral", duration: 3600, ...input, id: seq };
  items = [...items, item].slice(-4);
  emit();
  return item.id;
}

export function dismissToast(id: number) {
  items = items.filter((item) => item.id !== id);
  emit();
}

function useToasts(): ToastItem[] {
  return useSyncExternalStore(
    (listener) => { listeners.add(listener); return () => listeners.delete(listener); },
    () => items,
    () => items,
  );
}

const TONE_ICON: Record<ToastTone, { icon: IconName; className: string }> = {
  neutral: { icon: "info", className: "text-cx-fg-3" },
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
    <div className="pointer-events-auto flex w-[340px] max-w-[calc(100vw-32px)] items-start gap-2.5 rounded-xl bg-cx-overlay px-3.5 py-3 shadow-cx-pop">
      <Icon name={item.icon ?? tone.icon} size={16} className={cn("mt-[1px] shrink-0", tone.className)} />
      <div className="min-w-0 flex-1">
        <p className="text-[13px] font-medium leading-5 text-cx-fg">{item.title}</p>
        {item.description ? <p className="mt-0.5 text-[12.5px] leading-5 text-cx-fg-3">{item.description}</p> : null}
      </div>
      {item.action ? (
        <button
          type="button"
          onClick={() => { item.action?.onClick(); dismissToast(item.id); }}
          className="shrink-0 rounded-md px-2 py-0.5 text-[12.5px] font-medium text-cx-accent hover:bg-cx-accent-soft"
        >
          {item.action.label}
        </button>
      ) : null}
      <button type="button" aria-label="关闭通知" onClick={() => dismissToast(item.id)} className="-mr-1 grid size-5 shrink-0 place-items-center rounded text-cx-fg-4 hover:text-cx-fg">
        <Icon name="x" size={12} />
      </button>
    </div>
  );
}

export function Toaster() {
  const list = useToasts();
  const reduced = useReducedMotion();
  return (
    <Portal>
      <div aria-live="polite" className="pointer-events-none fixed bottom-4 right-4 z-[1300] flex flex-col items-end gap-2">
        <AnimatePresence initial={false}>
          {list.map((item) => (
            <motion.div
              key={item.id}
              layout={!reduced}
              initial={reduced ? { opacity: 0 } : { opacity: 0, y: 16, scale: 0.96 }}
              animate={{ opacity: 1, y: 0, scale: 1, transition: SPRING_PANEL }}
              exit={{ opacity: 0, scale: 0.96, transition: { duration: 0.14 } }}
            >
              <ToastCard item={item} />
            </motion.div>
          ))}
        </AnimatePresence>
      </div>
    </Portal>
  );
}
