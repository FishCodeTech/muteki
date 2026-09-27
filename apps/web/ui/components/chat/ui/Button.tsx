"use client";

import type { ComponentPropsWithRef, ReactNode } from "react";
import { cn } from "@/lib/cn";
import { Icon, type IconName } from "@/components/Icon";
import { ActionSwapIcon } from "@/components/agentui/motion/action-swap";
import { Tooltip } from "./Tooltip";
import type { Placement } from "./floating";
import { splitShortcut } from "./Kbd";

export type ButtonVariant = "primary" | "secondary" | "ghost" | "outline" | "soft" | "danger" | "danger-soft" | "link";
export type ButtonSize = "xs" | "sm" | "md" | "lg";

const VARIANTS: Record<ButtonVariant, string> = {
  primary:
    "bg-cx-accent text-cx-accent-fg shadow-[inset_0_1px_0_color-mix(in_srgb,white_18%,transparent),0_1px_2px_color-mix(in_srgb,var(--accent)_30%,transparent)] hover:bg-[color-mix(in_srgb,var(--accent)_88%,var(--ink))]",
  secondary:
    "bg-cx-elevated text-cx-fg border border-cx-border shadow-cx-xs hover:bg-[color-mix(in_srgb,var(--ink)_4%,var(--surface))] hover:border-cx-border-strong",
  ghost: "text-cx-fg-2 hover:bg-cx-hover hover:text-cx-fg data-[active=true]:bg-cx-active data-[active=true]:text-cx-fg",
  outline: "border border-cx-border text-cx-fg-2 hover:bg-cx-hover hover:text-cx-fg hover:border-cx-border-strong",
  soft: "bg-cx-hover text-cx-fg hover:bg-cx-active",
  danger: "bg-cx-danger text-white hover:bg-[color-mix(in_srgb,var(--red)_86%,black)]",
  "danger-soft": "bg-cx-danger-soft text-cx-danger hover:bg-[color-mix(in_srgb,var(--red)_18%,transparent)]",
  link: "text-cx-accent hover:underline underline-offset-4 px-0 h-auto",
};

const SIZES: Record<ButtonSize, string> = {
  xs: "h-6 gap-1 rounded-md px-2 text-[12px]",
  sm: "h-7 gap-1.5 rounded-lg px-2.5 text-[12.5px]",
  md: "h-8 gap-1.5 rounded-lg px-3 text-[13px]",
  lg: "h-10 gap-2 rounded-xl px-4 text-[14px]",
};

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
  /** Toggle/pressed styling for ghost buttons. */
  active?: boolean;
  tooltip?: ReactNode;
  shortcut?: string;
  tooltipPlacement?: Placement;
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
  className,
  children,
  disabled,
  type = "button",
  ...rest
}: ButtonProps) {
  const glyph = GLYPH[size];
  const button = (
    <button
      type={type}
      disabled={disabled || loading}
      aria-busy={loading || undefined}
      aria-pressed={active === undefined ? rest["aria-pressed"] : active}
      data-active={active ? "true" : undefined}
      className={cn(
        "cx-press relative inline-flex shrink-0 select-none items-center justify-center whitespace-nowrap font-medium outline-none",
        "disabled:pointer-events-none disabled:opacity-45",
        "focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--cx-focus)]",
        SIZES[size],
        iconOnly && ICON_SIZES[size],
        VARIANTS[variant],
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
        <span className="absolute -right-0.5 -top-0.5 flex h-[15px] min-w-[15px] items-center justify-center rounded-full bg-cx-accent px-1 text-[9.5px] font-semibold leading-none text-cx-accent-fg ring-2 ring-[var(--cx-bg)]">
          {badge}
        </span>
      ) : null}
    </Button>
  );
}
