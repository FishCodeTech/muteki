"use client";

import { Fragment, useEffect, useRef } from "react";
import Link from "next/link";
import { AnimatePresence, motion } from "motion/react";
import { cn } from "@/lib/cn";
import { Icon } from "@/components/Icon";
import { EASE_OUT, Kbd, Skeleton, useReducedMotion } from "@/components/chat/ui";
import type {
  ComposerCapabilityItem,
  ComposerRuntimeState,
  ComposerTrigger,
} from "@/lib/composerCapabilities";
import {
  capabilityDisabled,
  capabilityIcon,
  capabilityLabel,
  capabilitySection,
  triggerIcon,
  triggerLabel,
} from "./capabilityMeta";

export interface CapabilityMenuProps {
  id: string;
  trigger: ComposerTrigger | null;
  adapterId: string;
  items: ComposerCapabilityItem[];
  loading: boolean;
  error: string;
  runtime: ComposerRuntimeState | null;
  activeIndex: number;
  onActiveIndexChange: (index: number) => void;
  onSelect: (item: ComposerCapabilityItem) => void;
}

function CapabilityRows({
  id,
  trigger,
  items,
  activeIndex,
  onActiveIndexChange,
  onSelect,
}: Pick<CapabilityMenuProps, "id" | "items" | "activeIndex" | "onActiveIndexChange" | "onSelect"> & {
  trigger: ComposerTrigger;
}) {
  return (
    <>
      {items.map((item, index) => {
        const section = capabilitySection(trigger, item);
        const previous = index > 0 ? capabilitySection(trigger, items[index - 1]) : "";
        const disabled = capabilityDisabled(item);
        const level = String(item.support_level || (disabled ? "unknown" : "supported"));
        const active = index === activeIndex;
        const meta = [
          item.status && item.status !== "available" ? item.status : "",
          ({ limited: "部分支持", unsupported: "暂不可用", unknown: "待验证", expired: "已过期" } as Record<string, string>)[level] || "",
          disabled && item.reason ? item.reason : "",
        ].filter(Boolean).join(" · ");
        return (
          <Fragment key={item.id}>
            {section !== previous ? (
              <div role="presentation" className="px-2.5 pb-1 pt-2.5 text-[11px] font-medium text-cx-fg-4 first:pt-1.5">
                {section}
              </div>
            ) : null}
            <div
              id={`${id}-${index}`}
              role="option"
              aria-selected={active}
              aria-disabled={disabled || undefined}
              data-capability-index={index}
              data-capability-kind={item.kind}
              data-capability-id={item.id}
              data-support-level={level}
              data-invocable={disabled ? "false" : "true"}
              data-active={active || undefined}
              title={disabled ? [item.reason, item.alternative].filter(Boolean).join("；") : undefined}
              onPointerMove={() => { if (!active) onActiveIndexChange(index); }}
              onMouseDown={(event) => event.preventDefault()}
              onClick={() => onSelect(item)}
              className={cn(
                "group flex min-h-10 cursor-default select-none items-center gap-2.5 rounded-lg px-2 py-1.5",
                "data-[active=true]:bg-cx-hover",
                disabled && "opacity-50",
              )}
            >
              <span
                className={cn(
                  "grid size-7 shrink-0 place-items-center rounded-lg bg-cx-hover text-cx-fg-3 transition-colors",
                  "group-data-[active=true]:bg-cx-elevated group-data-[active=true]:text-cx-fg group-data-[active=true]:shadow-cx-xs",
                )}
              >
                <Icon name={capabilityIcon(item.kind)} size={14} />
              </span>
              <span className="flex min-w-0 flex-1 flex-col">
                <span className="flex min-w-0 items-baseline gap-2">
                  <span className="truncate text-[13px] font-medium leading-5 text-cx-fg">{capabilityLabel(item)}</span>
                  {item.argument_hint ? (
                    <span className="truncate font-cx-mono text-[11px] text-cx-fg-4">{item.argument_hint}</span>
                  ) : null}
                </span>
                {item.description || meta ? (
                  <span className="truncate text-[12px] leading-4 text-cx-fg-3">
                    {item.description}
                    {item.description && meta ? " · " : ""}
                    {meta ? <span className="text-cx-fg-4">{meta}</span> : null}
                  </span>
                ) : null}
              </span>
              <span className="ml-2 max-w-[120px] shrink-0 truncate text-[11px] text-cx-fg-4">{item.source}</span>
            </div>
          </Fragment>
        );
      })}
    </>
  );
}

/** Slash / mention / skill picker. Focus stays in the editor; rows are tracked via aria-activedescendant. */
export function CapabilityMenu({
  id,
  trigger,
  adapterId,
  items,
  loading,
  error,
  runtime,
  activeIndex,
  onActiveIndexChange,
  onSelect,
}: CapabilityMenuProps) {
  const reduced = useReducedMotion();
  const listRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    listRef.current
      ?.querySelector<HTMLElement>(`[data-capability-index="${activeIndex}"]`)
      ?.scrollIntoView({ block: "nearest" });
  }, [activeIndex, items]);

  return (
    <AnimatePresence>
      {trigger ? (
        <motion.div
          key="capability-menu"
          className="absolute inset-x-0 bottom-[calc(100%+8px)] z-40 flex max-h-[360px] flex-col overflow-hidden rounded-2xl bg-cx-overlay text-cx-fg shadow-cx-pop"
          style={{ transformOrigin: "50% 100%" }}
          initial={reduced ? { opacity: 0 } : { opacity: 0, y: 6, scale: 0.985 }}
          animate={{ opacity: 1, y: 0, scale: 1, transition: { duration: 0.16, ease: EASE_OUT } }}
          exit={reduced ? { opacity: 0, transition: { duration: 0.08 } } : { opacity: 0, y: 4, scale: 0.99, transition: { duration: 0.1, ease: EASE_OUT } }}
          onMouseDown={(event) => event.preventDefault()}
        >
          <div className="flex h-9 shrink-0 items-center gap-2 border-b border-cx-border-subtle px-3">
            <Icon name={triggerIcon(trigger)} size={13} className="text-cx-fg-3" />
            <span className="text-[12px] font-medium text-cx-fg-2">{triggerLabel(trigger)}</span>
            <span className="ml-auto truncate font-cx-mono text-[11px] text-cx-fg-4">{adapterId.replace(/^cli\./, "")}</span>
            <Link href="/settings/chat-plugins" className="shrink-0 text-[11px] text-cx-fg-3 hover:text-cx-fg" onMouseDown={(e) => e.stopPropagation()}>管理插件</Link>
          </div>
          <div
            ref={listRef}
            id={id}
            role="listbox"
            aria-label={triggerLabel(trigger)}
            aria-busy={loading || undefined}
            className="cx-scroll min-h-0 max-h-[320px] flex-1 overflow-y-auto overscroll-contain p-1"
          >
            {loading && !items.length ? (
              <div role="status" aria-label="正在读取当前 Agent 的能力" className="flex flex-col gap-1 p-1">
                {[0, 1, 2].map((row) => (
                  <div key={row} className="flex items-center gap-2.5 px-1 py-1.5">
                    <Skeleton className="size-7 rounded-lg" />
                    <div className="flex flex-1 flex-col gap-1.5">
                      <Skeleton className="h-3 w-1/3" />
                      <Skeleton className="h-2.5 w-2/3" />
                    </div>
                  </div>
                ))}
              </div>
            ) : error ? (
              <div role="status" className="flex items-start gap-2 px-2.5 py-3 text-[12.5px] leading-5 text-cx-danger">
                <Icon name="circleAlert" size={14} className="mt-[3px] shrink-0" />
                <span className="min-w-0">{error}</span>
              </div>
            ) : items.length ? (
              <CapabilityRows
                id={id}
                trigger={trigger}
                items={items}
                activeIndex={activeIndex}
                onActiveIndexChange={onActiveIndexChange}
                onSelect={onSelect}
              />
            ) : (
              <div role="status" className="flex flex-col items-center gap-1.5 px-4 py-6 text-center">
                <Icon name="search" size={16} className="text-cx-fg-4" />
                <span className="text-[12.5px] text-cx-fg-3">{runtime?.diagnostics[0] || "当前 Agent 没有匹配项"}</span>
              </div>
            )}
          </div>
          <div className="flex h-8 shrink-0 items-center gap-3 border-t border-cx-border-subtle px-3 text-[11px] text-cx-fg-4">
            {runtime?.refresh_status === "failed" ? (
              <span role="status" data-capability-refresh="failed" className="flex min-w-0 items-center gap-1.5 text-cx-danger">
                <Icon name="circleAlert" size={12} />
                <span className="truncate">
                  {runtime.last_error
                    || runtime.diagnostics[0]
                    || "Runtime 能力刷新失败"}
                </span>
              </span>
            ) : runtime?.stale ? (
              <span role="status" data-capability-stale="true" className="flex min-w-0 items-center gap-1.5 text-cx-warning">
                <Icon name="info" size={12} />
                <span className="truncate">
                  {runtime.refresh_status === "refreshing"
                    ? "能力更新中，过期项不会标记为确定支持"
                    : "能力目录尚未确认，过期项不会标记为确定支持"}
                </span>
              </span>
            ) : (
              <span className="flex items-center gap-1"><Kbd tone="subtle">↑</Kbd><Kbd tone="subtle">↓</Kbd> 选择</span>
            )}
            <span className="ml-auto flex shrink-0 items-center gap-1"><Kbd tone="subtle">Tab</Kbd> 确认</span>
            <span className="flex shrink-0 items-center gap-1"><Kbd tone="subtle">Esc</Kbd> 关闭</span>
          </div>
        </motion.div>
      ) : null}
    </AnimatePresence>
  );
}
