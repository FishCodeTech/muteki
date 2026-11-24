"use client";

import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useRef,
  useState,
  type ReactElement,
  type ReactNode,
  type RefObject,
} from "react";
import { cn } from "../utils/cn";
import { Icon, type IconName } from "../icons/Icon";
import { Popover, type Placement } from "./Popover";
import { Shortcut } from "./Kbd";

const MenuContext = createContext<{ close: () => void } | null>(null);

export function isContextMenuKey(event: { key: string; shiftKey: boolean }) {
  return event.key === "ContextMenu" || (event.shiftKey && event.key === "F10");
}

/** Text editing and selected text keep the platform's copy/paste menu. */
export function shouldUseNativeContextMenu(target: EventTarget | null, objectRoot?: HTMLElement) {
  if (!(target instanceof Element)) return false;
  if (target.closest("input, textarea, select, [contenteditable]:not([contenteditable='false']), [role='textbox']")) return true;
  const nativeObject = target.closest("a[href], img, audio, video");
  if (objectRoot && nativeObject && nativeObject !== objectRoot) return true;
  const selection = window.getSelection();
  return Boolean(selection && !selection.isCollapsed && selection.containsNode(target, true));
}

function menuItems(root: HTMLElement): HTMLElement[] {
  return Array.from(root.querySelectorAll<HTMLElement>("[role^='menuitem']:not([aria-disabled='true'])"))
    .filter((el) => el.closest("[role='menu']") === root);
}

export function handleMenuKeyDown(event: React.KeyboardEvent<HTMLElement>) {
  const root = event.currentTarget;
  const items = menuItems(root);
  if (!items.length) return;
  const index = items.indexOf(document.activeElement as HTMLElement);
  const focusAt = (i: number) => items[(i + items.length) % items.length]?.focus();
  if (event.key === "ArrowDown") { event.preventDefault(); focusAt(index + 1); }
  else if (event.key === "ArrowUp") { event.preventDefault(); focusAt(index < 0 ? items.length - 1 : index - 1); }
  else if (event.key === "Home") { event.preventDefault(); focusAt(0); }
  else if (event.key === "End") { event.preventDefault(); focusAt(items.length - 1); }
  else if (event.key.length === 1 && /\S/.test(event.key) && !event.metaKey && !event.ctrlKey) {
    const char = event.key.toLowerCase();
    const start = index + 1;
    for (let offset = 0; offset < items.length; offset += 1) {
      const item = items[(start + offset) % items.length];
      if ((item.textContent || "").trim().toLowerCase().startsWith(char)) { item.focus(); break; }
    }
  }
}

export interface MenuProps {
  trigger?: ReactElement;
  open?: boolean;
  onOpenChange?: (open: boolean) => void;
  anchorPoint?: { x: number; y: number } | null;
  anchorRef?: RefObject<HTMLElement | null>;
  placement?: Placement;
  className?: string;
  ariaLabel?: string;
  children: ReactNode;
}

export function Menu({
  trigger,
  open,
  onOpenChange,
  anchorPoint,
  anchorRef,
  placement = "bottom-start",
  className,
  ariaLabel,
  children,
}: MenuProps) {
  return (
    <Popover
      trigger={trigger}
      open={open}
      onOpenChange={onOpenChange}
      anchorPoint={anchorPoint}
      anchorRef={anchorRef}
      placement={placement}
      role="menu"
      haspopup="menu"
      ariaLabel={ariaLabel}
      className={cn("min-w-[200px]", className)}
      onKeyDown={handleMenuKeyDown}
    >
      {({ close }) => (
        <MenuContext.Provider value={{ close }}>
          <div
            data-slot="menu-list"
            className="cx-scroll flex min-h-0 flex-auto flex-col overflow-y-auto overflow-x-hidden p-1"
          >
            {children}
          </div>
        </MenuContext.Provider>
      )}
    </Popover>
  );
}

export interface MenuItemProps {
  icon?: IconName;
  shortcut?: string;
  hint?: ReactNode;
  danger?: boolean;
  disabled?: boolean;
  checked?: boolean;
  keepOpen?: boolean;
  onSelect?: () => void;
  children: ReactNode;
  className?: string;
  description?: ReactNode;
  wrapDescription?: boolean;
  /** Mark destructive/high-risk action (`data-agent-confirm`). */
  agentConfirm?: boolean | string;
}

export function MenuItem({
  icon,
  shortcut,
  hint,
  danger,
  disabled,
  checked,
  keepOpen,
  onSelect,
  children,
  className,
  description,
  wrapDescription,
  agentConfirm,
}: MenuItemProps) {
  const ctx = useContext(MenuContext);
  const select = () => {
    if (disabled) return;
    onSelect?.();
    if (!keepOpen) ctx?.close();
  };
  return (
    <div
      role={checked === undefined ? "menuitem" : "menuitemcheckbox"}
      aria-checked={checked}
      aria-disabled={disabled || undefined}
      data-slot="menu-item"
      data-disabled={disabled ? "true" : undefined}
      data-agent-confirm={agentConfirm === true ? "true" : agentConfirm || undefined}
      tabIndex={-1}
      onClick={(event) => {
        event.stopPropagation();
        select();
      }}
      onKeyDown={(event) => {
        if (event.key === "Enter" || event.key === " ") { event.preventDefault(); select(); }
      }}
      onPointerMove={(event) => { if (!disabled) event.currentTarget.focus({ preventScroll: true }); }}
      className={cn(
        "group flex min-h-8 cursor-default select-none items-center gap-2.5 rounded-[8px] px-2 py-1.5 text-cx-sm leading-5 outline-none transition-colors duration-75",
        "text-cx-fg focus:bg-cx-hover",
        danger && "text-cx-danger focus:bg-cx-danger-soft",
        disabled && "pointer-events-none opacity-40",
        className,
      )}
    >
      {icon ? <Icon name={icon} size={15} className={cn("text-cx-fg-3 group-focus:text-cx-fg", danger && "text-cx-danger group-focus:text-cx-danger")} /> : null}
      <span className="flex min-w-0 flex-1 flex-col">
        <span className="truncate">{children}</span>
        {description ? <span className={cn("text-cx-xs leading-4 text-cx-fg-3", wrapDescription ? "whitespace-normal break-words" : "truncate")}>{description}</span> : null}
      </span>
      {hint ? <span className="text-cx-xs text-cx-fg-4">{hint}</span> : null}
      {shortcut ? <Shortcut keys={shortcut} tone="subtle" /> : null}
      {checked !== undefined ? (
        <Icon name="check" size={14} className={cn("text-cx-fg transition-opacity", checked ? "opacity-100" : "opacity-0")} />
      ) : null}
    </div>
  );
}

export function MenuSeparator() {
  return <div role="separator" data-slot="menu-separator" className="-mx-1 my-1 h-px bg-cx-border-subtle" />;
}

export function MenuLabel({ children, action }: { children: ReactNode; action?: ReactNode }) {
  return (
    <div data-slot="menu-label" className="flex items-center justify-between px-2 pb-1 pt-2 text-cx-2xs font-medium text-cx-fg-4">
      <span>{children}</span>
      {action}
    </div>
  );
}

/** Lets `MenuItem` / `MenuSub` close a menu that is rendered by a bare `Popover`. */
export function MenuScope({ close, children }: { close: () => void; children: ReactNode }) {
  return <MenuContext.Provider value={{ close }}>{children}</MenuContext.Provider>;
}

/** Nested menu opened on hover or ArrowRight. */
export function MenuSub({ label, icon, children, disabled }: { label: ReactNode; icon?: IconName; children: ReactNode; disabled?: boolean }) {
  const [open, setOpen] = useState(false);
  const parent = useContext(MenuContext);
  const hoverTimer = useRef<number | undefined>(undefined);
  useEffect(() => () => window.clearTimeout(hoverTimer.current), []);
  const closeAll = useCallback(() => { setOpen(false); parent?.close(); }, [parent]);
  return (
    <Popover
      open={open}
      onOpenChange={setOpen}
      placement="right-start"
      offset={4}
      role="menu"
      haspopup="menu"
      className="min-w-[200px]"
      onKeyDown={(event) => {
        if (event.key === "ArrowLeft") { event.preventDefault(); event.stopPropagation(); setOpen(false); return; }
        if (event.key === "Tab") closeAll();
        event.stopPropagation();
        handleMenuKeyDown(event);
      }}
      trigger={(
        <div
          role="menuitem"
          aria-disabled={disabled || undefined}
          tabIndex={-1}
          onPointerEnter={() => { if (disabled) return; window.clearTimeout(hoverTimer.current); hoverTimer.current = window.setTimeout(() => setOpen(true), 120); }}
          onPointerLeave={() => { window.clearTimeout(hoverTimer.current); hoverTimer.current = window.setTimeout(() => setOpen(false), 180); }}
          onPointerMove={(event) => { if (!disabled) event.currentTarget.focus({ preventScroll: true }); }}
          onKeyDown={(event) => {
            if (disabled) return;
            if (event.key === "ArrowRight" || event.key === "Enter") { event.preventDefault(); setOpen(true); }
          }}
          className={cn(
            "group flex min-h-8 cursor-default select-none items-center gap-2.5 rounded-[8px] px-2 py-1.5 text-cx-sm text-cx-fg outline-none focus:bg-cx-hover data-[open]:bg-cx-hover",
            disabled && "pointer-events-none opacity-40",
          )}
        >
          {icon ? <Icon name={icon} size={15} className="text-cx-fg-3" /> : null}
          <span className="flex-1 truncate">{label}</span>
          <Icon name="chevronRight" size={14} className="text-cx-fg-4" />
        </div>
      )}
    >
      <MenuContext.Provider value={{ close: closeAll }}>
        <div
          className="cx-scroll flex max-h-[360px] min-h-0 flex-auto flex-col overflow-y-auto overflow-x-hidden p-1"
          onPointerEnter={() => window.clearTimeout(hoverTimer.current)}
          onPointerLeave={() => { hoverTimer.current = window.setTimeout(() => setOpen(false), 180); }}
        >{children}</div>
      </MenuContext.Provider>
    </Popover>
  );
}

/** Object actions shared by right-click and the keyboard menu key. */
export function useContextMenu() {
  const [point, setPoint] = useState<{ x: number; y: number } | null>(null);
  const anchorRef = useRef<HTMLElement | null>(null);
  const rememberAnchor = (root: HTMLElement, target: EventTarget | null) => {
    const focused = target instanceof Element ? target.closest<HTMLElement>("button, a[href], [tabindex]") : null;
    anchorRef.current = focused && root.contains(focused)
      ? focused
      : root.matches("button, a[href], [tabindex]") ? root : root.querySelector<HTMLElement>("button:not([disabled]), a[href], [tabindex='0']");
  };
  const onContextMenu = useCallback((event: React.MouseEvent) => {
    if (event.defaultPrevented || (event.target instanceof Element && event.target.closest("[role='menu']")) || shouldUseNativeContextMenu(event.target, event.currentTarget as HTMLElement)) return;
    event.preventDefault();
    event.stopPropagation();
    rememberAnchor(event.currentTarget as HTMLElement, event.target);
    setPoint({ x: event.clientX, y: event.clientY });
  }, []);
  const onKeyDown = useCallback((event: React.KeyboardEvent<HTMLElement>) => {
    if (event.defaultPrevented || !isContextMenuKey(event) || (event.target instanceof Element && event.target.closest("[role='menu']")) || shouldUseNativeContextMenu(event.target, event.currentTarget)) return;
    event.preventDefault();
    event.stopPropagation();
    rememberAnchor(event.currentTarget, event.target);
    const rect = (anchorRef.current ?? event.currentTarget).getBoundingClientRect();
    setPoint({ x: rect.left, y: rect.bottom });
  }, []);
  useEffect(() => {
    if (!point) return;
    const dismiss = () => setPoint(null);
    const onScroll = (event: Event) => {
      if (event.target instanceof Element && event.target.closest("[role='menu']")) return;
      dismiss();
    };
    window.addEventListener("blur", dismiss);
    window.addEventListener("resize", dismiss);
    window.addEventListener("scroll", onScroll, true);
    return () => {
      window.removeEventListener("blur", dismiss);
      window.removeEventListener("resize", dismiss);
      window.removeEventListener("scroll", onScroll, true);
    };
  }, [point]);
  const render = (children: ReactNode, ariaLabel?: string, className?: string) => (
    <Menu open={Boolean(point)} onOpenChange={(next) => { if (!next) setPoint(null); }} anchorPoint={point} anchorRef={anchorRef} ariaLabel={ariaLabel} className={className}>
      {children}
    </Menu>
  );
  return { onContextMenu, onKeyDown, open: Boolean(point), close: () => setPoint(null), render };
}
