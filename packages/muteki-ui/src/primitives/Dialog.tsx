"use client";

import {
  useCallback,
  useContext,
  useEffect,
  useRef,
  type ReactNode,
  type RefObject,
} from "react";
import {
  Dialog as UiDialog,
  DialogBackdrop,
  DialogContent,
  DialogDescription,
  DialogTitle,
} from "@ui-sdk/react/dialog";
import {
  Drawer as UiDrawer,
  DrawerBackdrop,
  DrawerContent,
  DrawerDescription,
  DrawerTitle,
} from "@ui-sdk/react/drawer";
import { Icon, type IconName } from "../icons/Icon";
import { cn } from "../utils/cn";
import { OverlayVisibilityContext } from "./Portal";
import { ensureImeCompositionTracking, guardOverlayDismiss } from "./_ime";

type DialogSize = "sm" | "md" | "lg" | "xl" | "full";

const SIZE: Record<DialogSize, string> = {
  sm: "w-[min(420px,calc(100vw-32px))]",
  md: "w-[min(520px,calc(100vw-32px))]",
  lg: "w-[min(680px,calc(100vw-32px))]",
  xl: "w-[min(880px,calc(100vw-32px))]",
  full: "w-[calc(100vw-48px)] h-[calc(100vh-48px)]",
};

const TONE_DEFAULT_ICON: Partial<Record<NonNullable<DialogProps["tone"]>, IconName>> = {
  danger: "alert",
  warning: "alert",
};

const TONE_ICON: Record<NonNullable<DialogProps["tone"]>, string> = {
  default: "bg-cx-hover text-cx-fg-2 border border-cx-border-subtle",
  accent: "bg-cx-accent-soft text-cx-accent border border-cx-accent-line/40",
  warning: "bg-cx-warning-soft text-cx-warning border border-[color-mix(in_srgb,var(--amber)_30%,transparent)]",
  danger: "bg-cx-danger-soft text-cx-danger border border-[color-mix(in_srgb,var(--red)_30%,transparent)]",
};

export interface DialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  title?: ReactNode;
  description?: ReactNode;
  icon?: IconName;
  tone?: "default" | "danger" | "warning" | "accent";
  size?: DialogSize;
  footer?: ReactNode;
  children?: ReactNode;
  className?: string;
  bodyClassName?: string;
  hideClose?: boolean;
  initialFocusRef?: RefObject<HTMLElement | null>;
  dismissable?: boolean;
  /** "top" pins the panel near the top edge so results can grow downward (command palettes). */
  placement?: "center" | "top";
  /** Skip enter/exit motion and backdrop blur (e.g. keyboard hotkey opens). */
  instant?: boolean;
  /** Accessible name when there is no visible title (e.g. command palettes). */
  ariaLabel?: string;
  testId?: string;
  /** Semantic Agent HITL high-risk confirmation marker (`data-agent-confirm`). */
  agentConfirm?: boolean | string;
}

export function Dialog({
  open: requestedOpen,
  onOpenChange,
  title,
  description,
  icon,
  tone = "default",
  size = "md",
  footer,
  children,
  className,
  bodyClassName,
  hideClose,
  initialFocusRef,
  dismissable = true,
  placement = "center",
  instant = false,
  ariaLabel,
  testId,
  agentConfirm,
}: DialogProps) {
  const visible = useContext(OverlayVisibilityContext);
  const open = requestedOpen && visible;
  const panelRef = useRef<HTMLDivElement | null>(null);
  const glyph = icon ?? TONE_DEFAULT_ICON[tone];

  useEffect(() => {
    ensureImeCompositionTracking();
  }, []);

  const resolveInitialFocus = useCallback((): HTMLElement | null | undefined => {
    if (initialFocusRef?.current) return initialFocusRef.current;
    const panel = panelRef.current;
    if (!panel) return undefined;
    const auto = panel.querySelector<HTMLElement>("[data-autofocus], [autofocus]");
    if (auto) return auto;
    const candidates = Array.from(
      panel.querySelectorAll<HTMLElement>(
        'a[href],button:not([disabled]),input:not([disabled]):not([type=hidden]),textarea:not([disabled]),select:not([disabled]),[tabindex]:not([tabindex="-1"]),[contenteditable="true"]',
      ),
    ).filter(
      (el) =>
        !el.hasAttribute("data-dialog-close") &&
        el.getAttribute("aria-hidden") !== "true" &&
        !el.closest("[inert]"),
    );
    return candidates[0] ?? panel;
  }, [initialFocusRef]);

  const animate = !instant;

  return (
    <UiDialog
      open={open}
      modal={true}
      closeOnOutsidePress={dismissable}
      onOpenChange={(nextOpen, details) => {
        if (guardOverlayDismiss(nextOpen, details)) return;
        onOpenChange(nextOpen);
      }}
    >
      <DialogBackdrop
        data-instant={instant ? "true" : undefined}
        className={cn(
          "fixed inset-0 z-(--z-overlay) bg-[color-mix(in_srgb,black_48%,transparent)]",
          !instant && "backdrop-blur-[3px]",
          animate && "muteki-backdrop-anim",
        )}
      />
      <DialogContent
        ref={panelRef}
        initialFocus={resolveInitialFocus}
        aria-label={title ? undefined : ariaLabel}
        data-cx-layer=""
        data-instant={instant ? "true" : undefined}
        data-testid={testId}
        data-agent-confirm={agentConfirm === true ? "true" : agentConfirm || undefined}
        className={cn(
          "cx-portal pointer-events-auto fixed inset-0 z-(--z-modal) flex h-fit flex-col overflow-hidden rounded-[14px] bg-cx-overlay p-0 text-cx-fg shadow-cx-pop outline-none",
          "dark:shadow-[inset_0_1px_0_color-mix(in_srgb,white_8%,transparent),var(--cx-shadow-pop)]",
          placement === "top"
            ? "mx-auto mt-[12vh] mb-auto max-h-[calc(100vh-12vh-24px)]"
            : "m-auto max-h-[calc(100vh-48px)]",
          SIZE[size],
          animate && "muteki-dialog-anim",
          className,
        )}
      >
        {title || !hideClose ? (
          <div className={cn("flex shrink-0 items-start gap-3 px-6 pt-5", children ? "pb-1" : "pb-2")}>
            {glyph ? (
              <span className={cn("grid size-9 shrink-0 place-items-center rounded-[10px]", TONE_ICON[tone])}>
                <Icon name={glyph} size={18} />
              </span>
            ) : null}
            <div className={cn("min-w-0 flex-1", glyph && "pt-0.5")}>
              {title ? (
                <DialogTitle className="text-cx-md font-semibold leading-6 tracking-[-0.012em] text-cx-fg">
                  {title}
                </DialogTitle>
              ) : null}
              {description ? (
                <DialogDescription className="mt-1 text-cx-sm leading-5 text-cx-fg-3">
                  {description}
                </DialogDescription>
              ) : null}
            </div>
            {!hideClose ? (
              <button
                type="button"
                aria-label="关闭"
                data-dialog-close=""
                onClick={() => onOpenChange(false)}
                className="cx-press -mr-2 -mt-1 inline-flex size-7 shrink-0 items-center justify-center rounded-lg text-cx-fg-3 outline-none hover:bg-cx-hover hover:text-cx-fg focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--cx-focus)]"
              >
                <Icon name="x" size={14} />
              </button>
            ) : null}
          </div>
        ) : null}
        {children ? (
          <div
            className={cn(
              "cx-scroll min-h-0 overflow-y-auto px-6 py-4 text-cx-sm leading-6 text-cx-fg-2",
              size === "full" ? "flex-1" : "flex-auto",
              bodyClassName,
            )}
          >
            {children}
          </div>
        ) : null}
        {footer ? (
          <div className="flex shrink-0 flex-wrap items-center justify-end gap-2 px-6 pb-5 pt-2">
            {footer}
          </div>
        ) : null}
      </DialogContent>
    </UiDialog>
  );
}

/** Convenience confirmation modal marked `data-agent-confirm` for high-risk or HITL confirmations. */
export function ConfirmDialog({
  open,
  onOpenChange,
  title,
  description,
  tone = "danger",
  confirmLabel = "确认",
  cancelLabel = "取消",
  confirmDisabled,
  loading,
  onConfirm,
  children,
  agentConfirm = true,
  testId,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  title: ReactNode;
  description?: ReactNode;
  tone?: "default" | "danger" | "warning" | "accent";
  confirmLabel?: ReactNode;
  cancelLabel?: ReactNode;
  confirmDisabled?: boolean;
  loading?: boolean;
  onConfirm: () => void;
  children?: ReactNode;
  agentConfirm?: boolean | string;
  testId?: string;
}) {
  return (
    <Dialog
      open={open}
      onOpenChange={onOpenChange}
      title={title}
      description={description}
      tone={tone}
      size="sm"
      dismissable={tone !== "danger"}
      agentConfirm={agentConfirm}
      testId={testId}
      footer={
        <>
          <button
            type="button"
            onClick={() => onOpenChange(false)}
            className="cx-press inline-flex h-8 items-center justify-center rounded-[8px] border border-cx-border bg-cx-elevated px-3 text-cx-sm font-medium text-cx-fg hover:border-cx-border-strong hover:bg-cx-hover focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--cx-focus)]"
          >
            {cancelLabel}
          </button>
          <button
            type="button"
            disabled={confirmDisabled || loading}
            data-agent-confirm={agentConfirm === true ? "true" : agentConfirm || undefined}
            onClick={onConfirm}
            className={cn(
              "cx-press inline-flex h-8 items-center justify-center gap-1.5 rounded-[8px] px-3 text-cx-sm font-medium focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--cx-focus)] disabled:opacity-50",
              tone === "danger"
                ? "bg-[var(--cx-danger-solid)] text-white hover:bg-[color-mix(in_srgb,var(--cx-danger-solid)_86%,black)]"
                : "bg-cx-accent text-cx-accent-fg shadow-[var(--ui-sheen)]",
            )}
          >
            {loading ? <Icon name="loader" size={14} className="cx-spin" /> : null}
            {confirmLabel}
          </button>
        </>
      }
    >
      {children}
    </Dialog>
  );
}

/**
 * Full-viewport modal surface backed by the unified `@ui-sdk` layer stack
 * (used by fullscreen viewers such as `ImageViewer` without a custom dismiss hook).
 */
export function ModalOverlay({
  open: requestedOpen,
  onOpenChange,
  ariaLabel,
  testId,
  className,
  dismissable = false,
  children,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  ariaLabel: string;
  testId?: string;
  className?: string;
  dismissable?: boolean;
  children: ReactNode;
}) {
  const visible = useContext(OverlayVisibilityContext);
  const open = requestedOpen && visible;
  const panelRef = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    ensureImeCompositionTracking();
  }, []);

  return (
    <UiDialog
      open={open}
      modal={true}
      closeOnOutsidePress={dismissable}
      onOpenChange={(nextOpen, details) => {
        if (guardOverlayDismiss(nextOpen, details)) return;
        onOpenChange(nextOpen);
      }}
    >
      <DialogContent
        ref={panelRef}
        initialFocus={() => panelRef.current ?? undefined}
        aria-label={ariaLabel}
        data-cx-layer=""
        data-testid={testId}
        className={cn(
          "cx-portal fixed inset-0 z-(--z-popover) flex h-dvh w-dvw flex-col border-0 p-0 text-cx-fg outline-none",
          "muteki-backdrop-anim",
          className,
        )}
      >
        {children}
      </DialogContent>
    </UiDialog>
  );
}

export interface SheetProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  side?: "right" | "left" | "bottom";
  title?: ReactNode;
  description?: ReactNode;
  headerActions?: ReactNode;
  width?: number | string;
  children?: ReactNode;
  footer?: ReactNode;
  className?: string;
  bodyClassName?: string;
  modal?: boolean;
  /** Close on pointer-down outside the panel. Defaults to `modal`. */
  dismissOnOutside?: boolean;
  /** Skip enter/exit motion and backdrop blur. */
  instant?: boolean;
  testId?: string;
  ariaLabel?: string;
}

export function Sheet({
  open: requestedOpen,
  onOpenChange,
  side = "right",
  title,
  description,
  headerActions,
  width = 440,
  children,
  footer,
  className,
  bodyClassName,
  modal = true,
  dismissOnOutside = modal,
  instant = false,
  testId,
  ariaLabel,
}: SheetProps) {
  const visible = useContext(OverlayVisibilityContext);
  const open = requestedOpen && visible;
  const panelRef = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    ensureImeCompositionTracking();
  }, []);

  const positionClass =
    side === "right"
      ? "ml-auto h-full border-l"
      : side === "left"
        ? "mr-auto h-full border-r"
        : "mt-auto w-full border-t rounded-t-2xl max-h-[85vh]";
  const sizeStyle =
    side === "bottom"
      ? undefined
      : { width: typeof width === "number" ? `min(${width}px, 100vw)` : width };

  return (
    <UiDrawer
      open={open}
      side={side}
      modal={modal}
      closeOnOutsidePress={dismissOnOutside}
      onOpenChange={(nextOpen, details) => {
        if (guardOverlayDismiss(nextOpen, details)) return;
        onOpenChange(nextOpen);
      }}
    >
      {modal ? (
        <DrawerBackdrop
          data-instant={instant ? "true" : undefined}
          className={cn(
            "fixed inset-0 z-(--z-overlay) bg-[color-mix(in_srgb,black_48%,transparent)]",
            !instant && "backdrop-blur-[3px] muteki-backdrop-anim",
          )}
        />
      ) : null}
      <DrawerContent
        ref={panelRef}
        initialFocus={() => panelRef.current?.querySelector<HTMLElement>("[data-autofocus], [autofocus]") ?? panelRef.current ?? undefined}
        aria-label={title ? undefined : ariaLabel}
        data-cx-layer=""
        data-instant={instant ? "true" : undefined}
        data-testid={testId}
        style={sizeStyle}
        className={cn(
          "cx-portal fixed z-(--z-modal) flex flex-col border-cx-border-strong bg-cx-overlay p-0 text-cx-fg shadow-cx-pop outline-none",
          positionClass,
          !instant &&
            (side === "right"
              ? "muteki-drawer-right-anim"
              : side === "left"
                ? "muteki-drawer-left-anim"
                : "muteki-drawer-bottom-anim"),
          className,
        )}
      >
        {title || headerActions ? (
          <div className="flex min-h-13 shrink-0 items-center gap-2 border-b border-cx-border-subtle px-4 py-2.5">
            <div className="min-w-0 flex-1">
              {title ? (
                <DrawerTitle className="truncate text-cx-base font-semibold text-cx-fg">
                  {title}
                </DrawerTitle>
              ) : null}
              {description ? (
                <DrawerDescription className="truncate text-cx-xs text-cx-fg-3">
                  {description}
                </DrawerDescription>
              ) : null}
            </div>
            {headerActions}
            <button
              type="button"
              aria-label="关闭"
              onClick={() => onOpenChange(false)}
              className="cx-press inline-flex size-7 shrink-0 items-center justify-center rounded-lg text-cx-fg-3 outline-none hover:bg-cx-hover hover:text-cx-fg focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--cx-focus)]"
            >
              <Icon name="x" size={14} />
            </button>
          </div>
        ) : null}
        <div className={cn("cx-scroll min-h-0 overflow-y-auto", side === "bottom" ? "flex-auto" : "flex-1", bodyClassName)}>{children}</div>
        {footer ? <div className="shrink-0 border-t border-cx-border-subtle px-4 py-3">{footer}</div> : null}
      </DrawerContent>
    </UiDrawer>
  );
}
