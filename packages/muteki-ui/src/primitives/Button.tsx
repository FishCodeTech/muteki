"use client";

import type { ComponentPropsWithRef, ReactNode } from "react";
import { cn } from "../utils/cn";
import { Icon, type IconName } from "../icons/Icon";
import { ActionSwapIcon } from "./ActionSwap";
import { Tooltip } from "./Tooltip";
import type { Placement } from "./Popover";
import { splitShortcut } from "./Kbd";

export type ButtonVariant = "primary" | "secondary" | "ghost" | "outline" | "soft" | "danger" | "danger-soft" | "link" | "unstyled";
export type ButtonSize = "xs" | "sm" | "md" | "lg";

const VARIANTS: Record<ButtonVariant, string> = {
  primary:
    "bg-cx-accent text-cx-accent-fg border border-transparent shadow-[inset_0_1px_0_color-mix(in_srgb,white_22%,transparent),0_1px_2px_color-mix(in_srgb,var(--accent)_32%,transparent)] hover:bg-[color-mix(in_srgb,var(--accent)_88%,var(--ink))]",
  secondary:
    "ui-surface-tactile bg-cx-elevated text-cx-fg border border-cx-border hover:bg-[color-mix(in_srgb,var(--ink)_4%,var(--surface))] hover:border-cx-border-strong",
  ghost:
    "text-cx-fg-2 border border-transparent hover:bg-cx-hover hover:text-cx-fg data-[active=true]:bg-cx-active data-[active=true]:text-cx-fg",
  outline:
    "border border-cx-border bg-transparent text-cx-fg-2 hover:bg-cx-hover hover:text-cx-fg hover:border-cx-border-strong",
  soft: "border border-transparent bg-cx-hover text-cx-fg hover:bg-cx-active",
  danger:
    "bg-[var(--cx-danger-solid)] text-white border border-transparent shadow-[inset_0_1px_0_color-mix(in_srgb,white_18%,transparent)] hover:bg-[color-mix(in_srgb,var(--cx-danger-solid)_86%,black)]",
  "danger-soft":
    "border border-[color-mix(in_srgb,var(--red)_22%,transparent)] bg-cx-danger-soft text-cx-danger hover:bg-[color-mix(in_srgb,var(--red)_18%,transparent)]",
  link: "border border-transparent text-cx-accent hover:underline underline-offset-4 px-0 h-auto",
  unstyled: "",
};

const SIZES: Record<ButtonSize, string> = {
  xs: "h-6 gap-1 rounded-[6px] px-2 text-cx-xs",
  sm: "h-7 gap-1.5 rounded-[7px] px-2.5 text-cx-xs",
  md: "h-8 gap-1.5 rounded-[8px] px-3 text-cx-sm",
  lg: "h-10 gap-2 rounded-[10px] px-4 text-cx-base",
};

const SOLID = new Set<ButtonVariant>(["primary", "danger"]);
const DISABLED_SOLID = "bg-cx-active text-cx-fg-4 shadow-none border-transparent";

const ICON_SIZES: Record<ButtonSize, string> = {
  xs: "w-6 px-0",
  sm: "w-7 px-0",
  md: "w-8 px-0",
  lg: "w-10 px-0",
};

const GLYPH: Record<ButtonSize, number> = { xs: 13, sm: 14, md: 15, lg: 16 };


export interface ButtonProps extends Omit<ComponentPropsWithRef<"button">, "children"> {
  variant?: ButtonVariant;
  size?: ButtonSize;
  iconOnly?: boolean;
  icon?: IconName;
  iconRight?: IconName;
  loading?: boolean;
  /** Toggle/pressed styling for toggle/ghost buttons. */
  active?: boolean;
  tooltip?: ReactNode;
  shortcut?: string;
  tooltipPlacement?: Placement;
  /** Semantic Agent HITL / destructive confirmation marker (`data-agent-confirm`). */
  agentConfirm?: boolean | string;
  children?: ReactNode;
}

export function Button({
  variant = "ghost",
  size = "md",
  iconOnly,
  icon,
  iconRight,
  loading,
  active,
  tooltip,
  shortcut,
  tooltipPlacement,
  agentConfirm,
  className,
  children,
  disabled,
  type = "button",
  ...rest
}: ButtonProps) {
  const glyph = GLYPH[size];
  const isDisabled = Boolean(disabled || loading);
  const button = (
    <button
      type={type}
      disabled={isDisabled}
      aria-busy={loading || undefined}
      aria-pressed={active === undefined ? rest["aria-pressed"] : active}
      data-slot="button"
      data-variant={variant}
      data-size={size}
      data-active={active ? "true" : undefined}
      data-disabled={isDisabled ? "true" : undefined}
      data-agent-confirm={agentConfirm === true ? "true" : agentConfirm || undefined}
      className={cn(
        variant === "unstyled"
          ? "outline-none focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--cx-focus)]"
          : [
              "cx-press relative inline-flex shrink-0 select-none items-center justify-center whitespace-nowrap font-medium outline-none",
              "disabled:pointer-events-none",
              "focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--cx-focus)]",
              SIZES[size],
              iconOnly && ICON_SIZES[size],
              VARIANTS[variant],
              loading ? "opacity-80" : disabled ? (SOLID.has(variant) ? DISABLED_SOLID : "opacity-50") : null,
            ],
        className,
      )}
      {...rest}
    >
      {loading || icon ? (
        <ActionSwapIcon value={loading ? "__loading" : icon ?? ""}>
          {loading ? <Icon name="loader" size={glyph} className="cx-spin" /> : icon ? <Icon name={icon} size={glyph} /> : null}
        </ActionSwapIcon>
      ) : null}
      {children}
      {iconRight && !loading ? <Icon name={iconRight} size={glyph - 1} className="opacity-70" /> : null}
    </button>
  );
  if (!tooltip) return button;
  return (
    <Tooltip content={tooltip} shortcut={shortcut ? splitShortcut(shortcut) : undefined} placement={tooltipPlacement}>
      {button}
    </Tooltip>
  );
}

export interface IconButtonProps extends Omit<ButtonProps, "iconOnly" | "icon" | "children"> {
  icon: IconName;
  label: string;
  /** Hide the tooltip (label is still exposed via aria-label). */
  noTooltip?: boolean;
  badge?: ReactNode;
}

export function IconButton({ icon, label, noTooltip, badge, size = "sm", className, ...rest }: IconButtonProps) {
  return (
    <Button
      {...rest}
      size={size}
      iconOnly
      icon={icon}
      aria-label={label}
      tooltip={noTooltip ? undefined : rest.tooltip ?? label}
      className={cn("text-cx-fg-3 hover:text-cx-fg", className)}
    >
      {badge !== undefined && badge !== null && badge !== false ? (
        <span className="absolute -right-0.5 -top-0.5 flex h-[15px] min-w-[15px] items-center justify-center rounded-full bg-cx-accent px-1 text-cx-2xs font-semibold leading-none text-cx-accent-fg ring-2 ring-[var(--cx-bg)]">
          {badge}
        </span>
      ) : null}
    </Button>
  );
}

export function ButtonGroup({ children, className }: { children: ReactNode; className?: string }) {
  return (
    <div
      role="group"
      data-slot="button-group"
      className={cn("inline-flex items-center -space-x-px [&>[data-slot=button]]:rounded-none [&>[data-slot=button]:first-child]:rounded-l-[8px] [&>[data-slot=button]:last-child]:rounded-r-[8px] [&>[data-slot=button]:focus-visible]:z-10", className)}
    >
      {children}
    </div>
  );
}
