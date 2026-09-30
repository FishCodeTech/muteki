"use client";

import { createContext, useContext, useEffect, useId, useMemo, useRef, type RefObject } from "react";

const FOCUSABLE = [
  "a[href]",
  "button:not([disabled])",
  "input:not([disabled]):not([type=hidden])",
  "textarea:not([disabled])",
  "select:not([disabled])",
  "[tabindex]:not([tabindex='-1'])",
  "[contenteditable='true']",
].join(",");

export function focusableWithin(root: HTMLElement | null): HTMLElement[] {
  if (!root) return [];
  return Array.from(root.querySelectorAll<HTMLElement>(FOCUSABLE)).filter(
    (el) => !el.closest("[inert]") && el.getAttribute("aria-hidden") !== "true" && el.getClientRects().length > 0,
  );
}

/**
 * Layer tree: nested overlays (submenu in a menu, popover in a dialog) register
 * their DOM node with the parent so clicks inside a child never dismiss the
 * parent, and only the top-most open layer reacts to Escape.
 */
interface LayerNode {
  register: (ref: RefObject<HTMLElement | null>) => () => void;
  contains: (target: Node) => boolean;
}

export const LayerContext = createContext<LayerNode | null>(null);

const openStack: string[] = [];

export function useLayer({
  open,
  refs,
  onDismiss,
  escape = true,
  outside = true,
}: {
  open: boolean;
  refs: Array<RefObject<HTMLElement | null>>;
  onDismiss: (reason: "escape" | "outside") => void;
  escape?: boolean;
  outside?: boolean;
}): LayerNode {
  const id = useId();
  const parent = useContext(LayerContext);
  const children = useRef(new Set<RefObject<HTMLElement | null>>());
  const refsRef = useRef(refs);
  refsRef.current = refs;
  const dismissRef = useRef(onDismiss);
  dismissRef.current = onDismiss;

  const node = useMemo<LayerNode>(() => ({
    register: (ref) => {
      children.current.add(ref);
      return () => children.current.delete(ref);
    },
    contains: (target) => {
      if (refsRef.current.some((ref) => ref.current?.contains(target))) return true;
      for (const ref of children.current) if (ref.current?.contains(target)) return true;
      return false;
    },
  }), []);

  const floatingRef = refs[refs.length - 1];
  useEffect(() => {
    if (!open || !parent || !floatingRef) return;
    return parent.register(floatingRef);
  }, [open, parent, floatingRef]);

  useEffect(() => {
    if (!open) return;
    openStack.push(id);
    const onPointerDown = (event: PointerEvent) => {
      if (!outside || openStack[openStack.length - 1] !== id) return;
      const target = event.target as Node | null;
      if (!target || node.contains(target)) return;
      dismissRef.current("outside");
    };
    const onKeyDown = (event: KeyboardEvent) => {
      if (!escape || event.key !== "Escape") return;
      if (openStack[openStack.length - 1] !== id) return;
      event.stopImmediatePropagation();
      event.preventDefault();
      dismissRef.current("escape");
    };
    document.addEventListener("pointerdown", onPointerDown, true);
    window.addEventListener("keydown", onKeyDown, true);
    return () => {
      const index = openStack.lastIndexOf(id);
      if (index >= 0) openStack.splice(index, 1);
      document.removeEventListener("pointerdown", onPointerDown, true);
      window.removeEventListener("keydown", onKeyDown, true);
    };
  }, [open, id, node, escape, outside]);

  return node;
}

/** Traps Tab focus within `ref` while `active`. */
export function useFocusTrap(active: boolean, ref: RefObject<HTMLElement | null>) {
  useEffect(() => {
    if (!active) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key !== "Tab") return;
      const root = ref.current;
      if (!root) return;
      const nodes = focusableWithin(root);
      if (!nodes.length) {
        event.preventDefault();
        root.focus();
        return;
      }
      const first = nodes[0];
      const last = nodes[nodes.length - 1];
      const current = document.activeElement as HTMLElement | null;
      if (current && !root.contains(current) && current.closest(".cx-portal")) return;
      if (event.shiftKey && (current === first || !root.contains(current))) {
        event.preventDefault();
        last.focus();
      } else if (!event.shiftKey && (current === last || !root.contains(current))) {
        event.preventDefault();
        first.focus();
      }
    };
    document.addEventListener("keydown", onKeyDown);
    return () => document.removeEventListener("keydown", onKeyDown);
  }, [active, ref]);
}

let scrollLocks = 0;
let savedOverflow = "";
export function useScrollLock(active: boolean) {
  useEffect(() => {
    if (!active) return;
    if (scrollLocks === 0) {
      savedOverflow = document.body.style.overflow;
      document.body.style.overflow = "hidden";
    }
    scrollLocks += 1;
    return () => {
      scrollLocks -= 1;
      if (scrollLocks === 0) document.body.style.overflow = savedOverflow;
    };
  }, [active]);
}

/** Restores focus to whatever was focused before `active` became true. */
export function useRestoreFocus(active: boolean) {
  useEffect(() => {
    if (!active) return;
    const previous = document.activeElement as HTMLElement | null;
    return () => {
      if (previous && document.contains(previous)) {
        window.requestAnimationFrame(() => previous.focus({ preventScroll: true }));
      }
    };
  }, [active]);
}
