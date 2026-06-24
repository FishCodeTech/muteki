"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { Icon, type IconName } from "@/components/Icon";
import { useT } from "@/lib/i18n";

/**
 * Unified toast / action-feedback lane.
 *
 * Every operator-initiated mutation (spawn/kill a worker, rename, archive,
 * folder ops, …) confirms itself here — or reports failure — instead of the old
 * silent no-feedback path. One `.toast-lane` at the bottom-center stacks up to a
 * few toasts (newest on top); each auto-dismisses (longer when it carries an
 * undo so the operator has time to click it), can be dismissed manually (✕), and
 * pauses its timer on hover.
 *
 * The lane is the single `aria-live="polite"` region for these confirmations, so
 * screen readers announce each toast as it lands (no separate sr-only mirror).
 */

export type ToastVariant = "success" | "error" | "info";

/** One queued toast. `undo` (when present) renders the undo button + extends the
 *  auto-dismiss window. `icon` overrides the variant default. */
export interface Toast {
  id: number;
  msg: string;
  variant: ToastVariant;
  undo?: () => void;
  icon?: IconName;
}

/** What callers push — id is assigned internally. */
export type ToastInput = Omit<Toast, "id"> | (Omit<Toast, "id" | "variant"> & { variant?: ToastVariant });

const VARIANT_ICON: Record<ToastVariant, IconName> = {
  success: "check",
  error: "alert",
  info: "radio",
};

const PLAIN_MS = 3500; // auto-dismiss for a plain toast
const UNDO_MS = 6500; // longer when it has an undo to click
const MAX_VISIBLE = 3; // cap the stack so it never walls off the screen

let _seq = 0;
const nextId = () => ++_seq;

/**
 * Toast queue hook. `push` enqueues a toast (returns its id); `dismiss` removes
 * one. The owning component renders <ToastLane toasts={toasts} onDismiss={dismiss}/>.
 */
export function useToasts() {
  const [toasts, setToasts] = useState<Toast[]>([]);
  const dismiss = useCallback((id: number) => {
    setToasts((cur) => cur.filter((t) => t.id !== id));
  }, []);
  const push = useCallback((input: ToastInput): number => {
    const id = nextId();
    const toast: Toast = { variant: "info", ...input, id };
    // newest on top; cap the visible stack (oldest fall off the bottom).
    setToasts((cur) => [toast, ...cur].slice(0, MAX_VISIBLE));
    return id;
  }, []);
  return { toasts, push, dismiss };
}

/** A single toast row — owns its own auto-dismiss timer (paused on hover). */
function ToastRow({ toast, onDismiss }: { toast: Toast; onDismiss: (id: number) => void }) {
  const t = useT();
  const [paused, setPaused] = useState(false);
  const remainingRef = useRef(toast.undo ? UNDO_MS : PLAIN_MS);
  const startedRef = useRef(0);

  useEffect(() => {
    if (paused) return;
    startedRef.current = Date.now();
    const id = window.setTimeout(() => onDismiss(toast.id), remainingRef.current);
    return () => {
      window.clearTimeout(id);
      // bank the time already elapsed so resuming doesn't restart the full window
      remainingRef.current = Math.max(0, remainingRef.current - (Date.now() - startedRef.current));
    };
  }, [paused, toast.id, onDismiss]);

  const icon = toast.icon ?? VARIANT_ICON[toast.variant];
  return (
    <div
      className={`toast toast-${toast.variant}`}
      onMouseEnter={() => setPaused(true)}
      onMouseLeave={() => setPaused(false)}
    >
      <Icon name={icon} size={15} className="toast-ico" />
      <span className="toast-msg">{toast.msg}</span>
      {toast.undo && (
        <button
          className="toast-undo"
          onClick={() => { toast.undo?.(); onDismiss(toast.id); }}
        >
          {t("rail.toast.undo")}
        </button>
      )}
      <button
        className="toast-x"
        onClick={() => onDismiss(toast.id)}
        aria-label={t("toast.dismiss")}
      >
        <Icon name="x" size={13} />
      </button>
    </div>
  );
}

/** The fixed bottom-center lane. Single polite live-region for action feedback. */
export function ToastLane({ toasts, onDismiss }: { toasts: Toast[]; onDismiss: (id: number) => void }) {
  return (
    <div className="toast-lane" role="status" aria-live="polite" aria-atomic="false">
      {toasts.map((toast) => (
        <ToastRow key={toast.id} toast={toast} onDismiss={onDismiss} />
      ))}
    </div>
  );
}

export default ToastLane;
