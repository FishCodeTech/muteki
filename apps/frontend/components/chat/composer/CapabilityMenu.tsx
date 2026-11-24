"use client";

import { Fragment, useEffect, useLayoutEffect, useRef, useState } from "react";
import { ConversationRouteLink as Link } from "@/components/conversation/ConversationNavigation";
import { AnimatePresence, motion } from "motion/react";
import { cn } from "@/lib/cn";
import { Icon, type IconName } from "@muteki/ui";
import { EASE_OUT, Kbd, Skeleton, useReducedMotion } from "@muteki/ui";
import type {
  ComposerCapabilityItem,
  ComposerRuntimeState,
  ComposerTrigger,
  ComposerCapabilitySectionError,
} from "@/lib/composerCapabilities";
import {
  capabilityDisabled,
  capabilityIcon,
  capabilityLabel,
  capabilitySection,
  capabilityStatusLabel,
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
  errorDiagnostic?: string;
  onRetry?: () => void;
  sectionErrors?: ComposerCapabilitySectionError[];
  runtime: ComposerRuntimeState | null;
  activeIndex: number;
  onActiveIndexChange: (index: number) => void;
  onSelect: (item: ComposerCapabilityItem) => void;
  local?: {
    title: string;
    scopeLabel?: string;
    emptyLabel?: string;
    sectionOf: (item: ComposerCapabilityItem) => string;
    iconOf?: (item: ComposerCapabilityItem) => IconName | undefined;
  };
}

function CapabilityRows({
  id,
  trigger,
  items,
  activeIndex,
  onActiveIndexChange,
  onSelect,
  local,
}: Pick<CapabilityMenuProps, "id" | "items" | "activeIndex" | "onActiveIndexChange" | "onSelect" | "local"> & {
  trigger: ComposerTrigger;
}) {
  const sectionOf = (item: ComposerCapabilityItem) => (local ? local.sectionOf(item) : capabilitySection(trigger, item));
  return (
    <>
      {items.map((item, index) => {
        const section = sectionOf(item);
        const previous = index > 0 ? sectionOf(items[index - 1]) : "";
        const disabled = capabilityDisabled(item);
        const level = String(item.support_level || (disabled ? "unknown" : "supported"));
        const active = index === activeIndex;
        const levelLabel = ({ limited: "部分支持", unsupported: "暂不可用", unknown: "待验证", expired: "已过期" } as Record<string, string>)[level] || "";
        const meta = Array.from(new Set([capabilityStatusLabel(item.status), levelLabel].filter(Boolean))).join(" · ");
        const disabledReason = disabled
          ? [item.reason, item.alternative ? `替代：${item.alternative}` : ""].filter(Boolean).join("；")
          : "";
        return (
          <Fragment key={item.id}>
            {section !== previous ? (
              <div role="presentation" className="px-2.5 pb-1 pt-2.5 text-cx-xs font-medium text-cx-fg-4 first:pt-1.5">
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
              )}
            >
              <span
                className={cn(
                  "grid size-7 shrink-0 place-items-center rounded-lg bg-cx-hover text-cx-fg-3 transition-colors",
                  "group-data-[active=true]:bg-cx-elevated group-data-[active=true]:text-cx-fg group-data-[active=true]:shadow-cx-xs",
                  disabled && "opacity-50",
                )}
              >
                <Icon name={local?.iconOf?.(item) ?? capabilityIcon(item.kind, item.id)} size={14} />
              </span>
              <span className="flex min-w-0 flex-1 flex-col">
                <span className={cn("flex min-w-0 items-baseline gap-2", disabled && "opacity-50")}>
                  <span className="truncate text-cx-sm font-medium leading-5 text-cx-fg">{capabilityLabel(item)}</span>
                  {item.argument_hint ? (
                    <span className="truncate font-cx-mono text-cx-xs text-cx-fg-4">{item.argument_hint}</span>
                  ) : null}
                </span>
                {item.description || meta ? (
                  <span className={cn("truncate text-cx-xs leading-4 text-cx-fg-3", disabled && "opacity-50")}>
                    {item.description}
                    {item.description && meta ? " · " : ""}
                    {meta ? <span className="text-cx-fg-4">{meta}</span> : null}
                  </span>
                ) : null}
                {disabledReason ? (
                  <span data-capability-disabled-reason className="text-cx-xs leading-4 text-cx-fg-3">{disabledReason}</span>
                ) : null}
              </span>
              {item.source ? (
                <span
                  data-capability-source
                  title={item.source}
                  className="ml-2 max-w-[140px] shrink-0 truncate rounded-md bg-cx-hover px-1.5 py-0.5 text-cx-xs leading-4 text-cx-fg-3"
                >
                  {item.source}
                </span>
              ) : null}
            </div>
          </Fragment>
        );
      })}
    </>
  );
}

export function CapabilityMenu({
  id,
  trigger,
  adapterId,
  items,
  loading,
  error,
  errorDiagnostic,
  onRetry,
  sectionErrors = [],
  runtime,
  activeIndex,
  onActiveIndexChange,
  onSelect,
  local,
}: CapabilityMenuProps) {
  const reduced = useReducedMotion();
  const menuRef = useRef<HTMLDivElement>(null);
  const listRef = useRef<HTMLDivElement>(null);
  const [maxHeight, setMaxHeight] = useState(360);

  useLayoutEffect(() => {
    if (!trigger) return;
    const anchor = menuRef.current?.parentElement;
    if (!anchor) return;

    const boundaries: HTMLElement[] = [];
    for (let parent = anchor.parentElement; parent; parent = parent.parentElement) {
      if (getComputedStyle(parent).overflowY !== "visible") boundaries.push(parent);
    }
    const viewport = window.visualViewport;
    const measure = () => {
      const top = Math.max(
        viewport?.offsetTop ?? 0,
        ...boundaries.map((element) => element.getBoundingClientRect().top + element.clientTop),
      );
      setMaxHeight(Math.max(0, Math.min(360, Math.floor(anchor.getBoundingClientRect().top - top - 16))));
    };
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(anchor);
    boundaries.forEach((element) => observer.observe(element));
    window.addEventListener("resize", measure);
    window.addEventListener("scroll", measure, true);
    viewport?.addEventListener("resize", measure);
    viewport?.addEventListener("scroll", measure);
    return () => {
      observer.disconnect();
      window.removeEventListener("resize", measure);
      window.removeEventListener("scroll", measure, true);
      viewport?.removeEventListener("resize", measure);
      viewport?.removeEventListener("scroll", measure);
    };
  }, [trigger]);

  useEffect(() => {
    const list = listRef.current;
    const row = list?.querySelector<HTMLElement>(`[data-capability-index="${activeIndex}"]`);
    if (!list || !row) return;
    const bounds = list.getBoundingClientRect();
    const item = row.getBoundingClientRect();
    if (item.top < bounds.top) list.scrollTop -= bounds.top - item.top;
    else if (item.bottom > bounds.bottom) list.scrollTop += item.bottom - bounds.bottom;
  }, [activeIndex, items, maxHeight]);

  return (
    <AnimatePresence>
      {trigger ? (
        <motion.div
          ref={menuRef}
          key="capability-menu"
          className="absolute inset-x-0 bottom-[calc(100%+8px)] z-40 flex flex-col overflow-hidden rounded-2xl bg-cx-overlay text-cx-fg shadow-cx-pop"
          style={{ maxHeight, transformOrigin: "50% 100%" }}
          initial={reduced ? { opacity: 0 } : { opacity: 0, y: 6, scale: 0.985 }}
          animate={{ opacity: 1, y: 0, scale: 1, transition: { duration: 0.16, ease: EASE_OUT } }}
          exit={reduced ? { opacity: 0, transition: { duration: 0.08 } } : { opacity: 0, y: 4, scale: 0.99, transition: { duration: 0.1, ease: EASE_OUT } }}
          onMouseDown={(event) => event.preventDefault()}
        >
          <div className="flex h-9 shrink-0 items-center gap-2 border-b border-cx-border-subtle px-3">
            <Icon name={triggerIcon(trigger)} size={13} className="text-cx-fg-3" />
            <span className="text-cx-xs font-medium text-cx-fg-2">{local?.title ?? triggerLabel(trigger)}</span>
            {local ? (
              local.scopeLabel ? <span className="ml-auto truncate text-cx-xs text-cx-fg-4">{local.scopeLabel}</span> : null
            ) : (
              <>
                <span className="ml-auto truncate font-cx-mono text-cx-xs text-cx-fg-4">{adapterId.replace(/^cli\./, "")}</span>
                <Link href="/settings/agent-extensions" className="shrink-0 text-cx-xs text-cx-fg-3 hover:text-cx-fg" onMouseDown={(e) => e.stopPropagation()}>管理扩展</Link>
              </>
            )}
          </div>
          <div
            ref={listRef}
            id={id}
            role="listbox"
            aria-label={local?.title ?? triggerLabel(trigger)}
            aria-busy={loading || undefined}
            className="cx-scroll min-h-0 flex-1 overflow-y-auto overscroll-contain p-1"
          >
            {sectionErrors.length ? (
              <div role="status" className="space-y-2 border-b border-cx-border-subtle px-2.5 py-2 text-cx-xs text-cx-warning">
                {sectionErrors.map((entry, index) => <details key={`${entry.section}:${index}`}><summary>{entry.section}：{entry.message}</summary><pre className="mt-1 whitespace-pre-wrap break-words font-cx-mono text-cx-xs">{JSON.stringify(entry, null, 2)}</pre></details>)}
                {onRetry ? <button type="button" disabled={loading} className="rounded-md border border-cx-border px-2 py-1 text-cx-fg" onClick={onRetry}>重试失败来源</button> : null}
              </div>
            ) : null}
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
              <div role="status" className="flex items-start gap-2 px-2.5 py-3 text-cx-sm leading-5 text-cx-danger">
                <Icon name="circleAlert" size={14} className="mt-[3px] shrink-0" />
                <div className="min-w-0 flex-1 space-y-2">
                  <p>{error}</p>
                  {trigger === "@" ? <p className="text-cx-fg-3">引用目录尚未确认；文件面板可独立浏览工作目录。</p> : null}
                  {onRetry ? <button type="button" className="rounded-md border border-cx-border px-2 py-1 text-cx-fg" disabled={loading} onClick={onRetry}>重试能力目录</button> : null}
                  {errorDiagnostic ? <details className="text-cx-fg-3"><summary className="cursor-pointer">查看完整诊断</summary><pre className="mt-2 whitespace-pre-wrap break-words font-cx-mono text-cx-xs">{errorDiagnostic}</pre></details> : null}
                </div>
              </div>
            ) : items.length ? (
              <CapabilityRows
                id={id}
                trigger={trigger}
                items={items}
                activeIndex={activeIndex}
                onActiveIndexChange={onActiveIndexChange}
                onSelect={onSelect}
                local={local}
              />
            ) : (
              <div role="status" className="flex flex-col items-center gap-1.5 px-4 py-6 text-center">
                <Icon name="search" size={16} className="text-cx-fg-4" />
                <span className="text-cx-sm text-cx-fg-3">{local ? local.emptyLabel ?? "没有匹配项" : runtime?.diagnostics[0] || "当前 Agent 没有匹配项"}</span>
              </div>
            )}
          </div>
          <div className="flex h-8 shrink-0 items-center gap-3 border-t border-cx-border-subtle px-3 text-cx-xs text-cx-fg-4">
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
