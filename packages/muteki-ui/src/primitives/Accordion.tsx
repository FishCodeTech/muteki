"use client";

import type { ReactNode } from "react";
import {
  Accordion as UiAccordion,
  AccordionHeader,
  AccordionItem as UiAccordionItem,
  AccordionPanel,
  AccordionTrigger as UiAccordionTrigger,
} from "@ui-sdk/react/accordion";
import { cn } from "../utils/cn";
import { Icon } from "../icons/Icon";

export interface AccordionProps {
  value?: string[];
  defaultValue?: string[];
  onValueChange?: (value: string[]) => void;
  multiple?: boolean;
  collapsible?: boolean;
  disabled?: boolean;
  className?: string;
  children: ReactNode;
}

export function Accordion({
  value,
  defaultValue,
  onValueChange,
  multiple = false,
  collapsible = true,
  disabled,
  className,
  children,
}: AccordionProps) {
  return (
    <UiAccordion
      value={value}
      defaultValue={defaultValue}
      onValueChange={(next) => onValueChange?.(next)}
      multiple={multiple}
      collapsible={collapsible}
      disabled={disabled}
      data-slot="accordion"
      className={cn("flex flex-col divide-y divide-cx-border-subtle rounded-[12px] border border-cx-border bg-cx-elevated", className)}
    >
      {children}
    </UiAccordion>
  );
}

export function AccordionItem({
  value,
  disabled,
  className,
  children,
}: {
  value: string;
  disabled?: boolean;
  className?: string;
  children: ReactNode;
}) {
  return (
    <UiAccordionItem value={value} disabled={disabled} data-slot="accordion-item" className={cn("group flex flex-col", className)}>
      {children}
    </UiAccordionItem>
  );
}

export function AccordionTrigger({
  children,
  className,
  trailing,
}: {
  children: ReactNode;
  className?: string;
  trailing?: ReactNode;
}) {
  return (
    <AccordionHeader className="flex">
      <UiAccordionTrigger
        data-slot="accordion-trigger"
        className={cn(
          "flex w-full flex-1 items-center justify-between gap-3 px-4 py-3 text-left text-cx-sm font-medium text-cx-fg outline-none transition-colors",
          "hover:bg-cx-hover focus-visible:outline-2 focus-visible:outline-offset-[-2px] focus-visible:outline-[var(--cx-focus)]",
          "disabled:cursor-not-allowed disabled:opacity-50",
          className,
        )}
      >
        <span className="min-w-0 flex-1">{children}</span>
        {trailing}
        <Icon
          name="chevronDown"
          size={15}
          data-slot="accordion-indicator"
          className="shrink-0 text-cx-fg-3 transition-transform duration-150 ease-cx-out group-data-[open]:rotate-180 [[aria-expanded='true']_&]:rotate-180"
        />
      </UiAccordionTrigger>
    </AccordionHeader>
  );
}

export function AccordionContent({
  children,
  className,
}: {
  children: ReactNode;
  className?: string;
}) {
  return (
    <AccordionPanel className="overflow-hidden">
      <div className={cn("px-4 pb-3.5 pt-0.5 text-cx-sm leading-6 text-cx-fg-2", className)}>
        {children}
      </div>
    </AccordionPanel>
  );
}
