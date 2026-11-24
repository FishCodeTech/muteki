"use client";

import type { ComponentPropsWithRef, ReactNode } from "react";
import { cn } from "../utils/cn";

export type CardVariant = "default" | "elevated" | "inset" | "interactive" | "ghost";

const CARD_VARIANTS: Record<CardVariant, string> = {
  default: "ui-surface-ring bg-cx-elevated",
  elevated: "bg-cx-overlay ring-1 ring-cx-ring-strong shadow-cx-sm dark:shadow-[var(--ui-sheen-strong),var(--cx-shadow-sm)]",
  inset: "border border-cx-border-subtle bg-cx-bg-subtle",
  interactive: "ui-surface-ring bg-cx-elevated transition-[background-color,box-shadow] hover:[--ui-surface-ring:var(--cx-ring-strong)] hover:bg-cx-hover/60",
  ghost: "border border-transparent bg-transparent",
};

export interface CardProps extends ComponentPropsWithRef<"div"> {
  variant?: CardVariant;
  selected?: boolean;
  children: ReactNode;
}

export function Card({ variant = "default", selected, className, children, ...rest }: CardProps) {
  return (
    <div
      data-slot="card"
      data-variant={variant}
      data-selected={selected ? "true" : undefined}
      className={cn(
        "flex flex-col rounded-[12px] text-cx-fg",
        CARD_VARIANTS[variant],
        selected && "border-cx-accent-line ring-1 ring-cx-accent-line [--ui-surface-ring:var(--cx-accent-line)]",
        className,
      )}
      {...rest}
    >
      {children}
    </div>
  );
}

export function CardHeader({ className, children, ...rest }: ComponentPropsWithRef<"div">) {
  return (
    <div data-slot="card-header" className={cn("flex items-start justify-between gap-3 px-4 pt-4 pb-2", className)} {...rest}>
      {children}
    </div>
  );
}

export function CardTitle({ className, children, ...rest }: ComponentPropsWithRef<"h3">) {
  return (
    <h3 data-slot="card-title" className={cn("text-cx-sm font-semibold leading-5 text-cx-fg", className)} {...rest}>
      {children}
    </h3>
  );
}

export function CardDescription({ className, children, ...rest }: ComponentPropsWithRef<"p">) {
  return (
    <p data-slot="card-description" className={cn("text-cx-xs leading-5 text-cx-fg-3", className)} {...rest}>
      {children}
    </p>
  );
}

export function CardBody({ className, children, ...rest }: ComponentPropsWithRef<"div">) {
  return (
    <div data-slot="card-body" className={cn("flex flex-col gap-2 px-4 py-3 text-cx-sm text-cx-fg-2", className)} {...rest}>
      {children}
    </div>
  );
}

export function CardFooter({ className, children, ...rest }: ComponentPropsWithRef<"div">) {
  return (
    <div data-slot="card-footer" className={cn("flex flex-wrap items-center justify-between gap-2 border-t border-cx-border-subtle px-4 py-3", className)} {...rest}>
      {children}
    </div>
  );
}
