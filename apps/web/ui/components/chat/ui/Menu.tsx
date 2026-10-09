"use client";

import {
  createContext,
  useCallback,
  useContext,
  useRef,
  useState,
  type ReactElement,
  type ReactNode,
} from "react";
import { cn } from "@/lib/cn";
import { Icon, type IconName } from "@/components/Icon";
import { Popover } from "./Popover";
import type { Placement } from "./floating";
import { Shortcut } from "./Kbd";

const MenuContext = createContext<{ close: () => void } | null>(null);

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
  placement?: Placement;
  className?: string;
  ariaLabel?: string;
  children: ReactNode;
}

export function Menu({ trigger, open, onOpenChange, anchorPoint, placement = "bottom-start", className, ariaLabel, children }: MenuProps) {
  return (
    <Popover
      trigger={trigger}
      open={open}
      onOpenChange={onOpenChange}
      anchorPoint={anchorPoint}
      placement={placement}
      role="menu"
      haspopup="menu"
      ariaLabel={ariaLabel}
      className={cn("min-w-[200px] p-1", className)}
      onKeyDown={handleMenuKeyDown}
    >
      {({ close }) => (
        <MenuContext.Provider value={{ close }}>
          <div className="cx-scroll flex min-h-0 flex-col overflow-y-auto overflow-x-hidden">{children}</div>
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
  /** Keep the menu open after selection (toggles). */
  keepOpen?: boolean;
  onSelect?: () => void;
  children: ReactNode;
  className?: string;
  description?: ReactNode;
}

export function MenuItem({ icon, shortcut, hint, danger, disabled, checked, keepOpen, onSelect, children, className, description }: MenuItemProps) {
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
      tabIndex={-1}
      onClick={select}
      onKeyDown={(event) => {
        if (event.key === "Enter" || event.key === " ") { event.preventDefault(); select(); }
      }}
      onPointerMove={(event) => { if (!disabled) event.currentTarget.focus({ preventScroll: true }); }}
      className={cn(
        "group flex min-h-8 cursor-default select-none items-center gap-2.5 rounded-lg px-2 py-1.5 text-[13px] leading-5 outline-none",
        "text-cx-fg focus:bg-cx-hover",
        danger && "text-cx-danger focus:bg-cx-danger-soft",
        disabled && "opacity-40",
        className,
      )}
    >
      {icon ? <Icon name={icon} size={15} className={cn("text-cx-fg-3 group-focus:text-cx-fg", danger && "text-cx-danger group-focus:text-cx-danger")} /> : null}
      <span className="flex min-w-0 flex-1 flex-col">
        <span className="truncate">{children}</span>
        {description ? <span className="truncate text-[11.5px] leading-4 text-cx-fg-3">{description}</span> : null}
      </span>
      {hint ? <span className="text-[12px] text-cx-fg-4">{hint}</span> : null}
      {shortcut ? <Shortcut keys={shortcut} tone="subtle" /> : null}
      {checked !== undefined ? (
        <Icon name="check" size={14} className={cn("text-cx-fg transition-opacity", checked ? "opacity-100" : "opacity-0")} />
      ) : null}
    </div>
  );
}

export function MenuSeparator() {
  return <div role="separator" className="-mx-1 my-1 h-px bg-cx-border-subtle" />;
}

export function MenuLabel({ children, action }: { children: ReactNode; action?: ReactNode }) {
  return (
    <div className="flex items-center justify-between px-2 pb-1 pt-2 text-[11px] font-medium text-cx-fg-4">
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
  const closeAll = useCallback(() => { setOpen(false); parent?.close(); }, [parent]);
  return (
    <Popover
      open={open}
      onOpenChange={setOpen}
      placement="right-start"
      offset={4}
      role="menu"
      haspopup="menu"
      className="min-w-[200px] p-1"
      onKeyDown={(event) => {
        if (event.key === "ArrowLeft") { event.preventDefault(); event.stopPropagation(); setOpen(false); return; }
        handleMenuKeyDown(event);
      }}
      trigger={(
        <div
          role="menuitem"
          aria-disabled={disabled || undefined}
          tabIndex={-1}
          onPointerEnter={() => { window.clearTimeout(hoverTimer.current); hoverTimer.current = window.setTimeout(() => setOpen(true), 120); }}
          onPointerLeave={() => window.clearTimeout(hoverTimer.current)}
          onPointerMove={(event) => event.currentTarget.focus({ preventScroll: true })}
          onKeyDown={(event) => {
            if (event.key === "ArrowRight" || event.key === "Enter") { event.preventDefault(); setOpen(true); }
          }}
          className={cn(
            "group flex min-h-8 cursor-default select-none items-center gap-2.5 rounded-lg px-2 py-1.5 text-[13px] text-cx-fg outline-none focus:bg-cx-hover data-[state=open]:bg-cx-hover",
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
        <div className="cx-scroll flex max-h-[360px] flex-col overflow-y-auto overflow-x-hidden">{children}</div>
      </MenuContext.Provider>
    </Popover>
  );
}

/** Right-click menu: wrap any region; menu opens at the pointer. */
export function useContextMenu() {
  const [point, setPoint] = useState<{ x: number; y: number } | null>(null);
  const onContextMenu = useCallback((event: React.MouseEvent) => {
    event.preventDefault();
    setPoint({ x: event.clientX, y: event.clientY });
  }, []);
  const render = (children: ReactNode, ariaLabel?: string) => (
    <Menu open={Boolean(point)} onOpenChange={(next) => { if (!next) setPoint(null); }} anchorPoint={point} ariaLabel={ariaLabel}>
      {children}
    </Menu>
  );
  return { onContextMenu, open: Boolean(point), close: () => setPoint(null), render };
}
