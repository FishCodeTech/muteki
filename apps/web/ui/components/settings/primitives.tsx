"use client";

import { useMemo, useState, type ComponentType, type MouseEvent, type ReactNode } from "react";
import { Icon, type IconName } from "@/components/Icon";
import { cn } from "@/lib/cn";

/* ─────────────────────────────────────────────────────────
 * Shared settings language for web and desktop: a quiet category column,
 * one centred page, rounded section cards and title/description rows.
 * ───────────────────────────────────────────────────────── */

export function SettingsPage({ title, description, actions, wide = false, children }: {
  title: ReactNode; description?: ReactNode; actions?: ReactNode; wide?: boolean; children: ReactNode;
}) {
  return <div className={cn("cx-settings-page mx-auto flex w-full flex-col gap-8 px-8 pb-16 pt-10", wide ? "max-w-[1080px]" : "max-w-[760px]")}>
    <header className="flex items-start justify-between gap-4">
      <div className="min-w-0">
        <h1 className="text-[22px] font-semibold leading-8 tracking-normal text-cx-fg">{title}</h1>
        {description ? <p className="mt-1 text-[14px] leading-6 text-cx-fg-3">{description}</p> : null}
      </div>
      {actions ? <div className="flex shrink-0 items-center gap-2 pt-1">{actions}</div> : null}
    </header>
    {children}
  </div>;
}

export function SettingsSection({ title, description, actions, children, className, bodyClassName, anchor }: {
  title?: ReactNode; description?: ReactNode; actions?: ReactNode; children: ReactNode; className?: string; bodyClassName?: string;
  /** Target for settings search (`SETTINGS_INDEX`). */
  anchor?: string;
}) {
  return <section className={cn("flex flex-col gap-2", className)} data-settings-anchor={anchor} id={anchor ? `setting-${anchor}` : undefined}>
    {title || description || actions ? <div className="flex items-end justify-between gap-3 px-1">
      <div className="min-w-0">
        {title ? <h2 className="text-[14px] font-medium text-cx-fg">{title}</h2> : null}
        {description ? <p className="mt-0.5 text-[13px] leading-5 text-cx-fg-3">{description}</p> : null}
      </div>
      {actions ? <div className="flex shrink-0 items-center gap-2">{actions}</div> : null}
    </div> : null}
    <div className={cn("cx-settings-card", bodyClassName)}>{children}</div>
  </section>;
}

/** One setting: copy on the left, the control on the right; stacks when `stacked`. */
export function SettingsRow({ title, description, control, stacked = false, children, leading, anchor }: {
  title: ReactNode; description?: ReactNode; control?: ReactNode; stacked?: boolean; children?: ReactNode; leading?: ReactNode;
  /** Target for settings search (`SETTINGS_INDEX`). */
  anchor?: string;
}) {
  return <div className="border-b border-cx-border-subtle px-5 py-4 last:border-b-0" data-settings-anchor={anchor} id={anchor ? `setting-${anchor}` : undefined}>
    <div className={cn("flex gap-4", stacked ? "flex-col" : "items-center justify-between")}>
      <div className="flex min-w-0 flex-1 items-start gap-3">
        {leading ? <div className="shrink-0">{leading}</div> : null}
        <div className="min-w-0 flex-1">
          <div className="text-[14px] font-medium leading-5 text-cx-fg">{title}</div>
          {description ? <div className="mt-1 text-[13px] leading-5 text-cx-fg-3">{description}</div> : null}
        </div>
      </div>
      {control ? <div className={cn("min-w-0", stacked ? "w-full" : "shrink-0")}>{control}</div> : null}
    </div>
    {children ? <div className="mt-3">{children}</div> : null}
  </div>;
}

export function SettingsNote({ tone = "neutral", children }: { tone?: "neutral" | "danger"; children: ReactNode }) {
  return <p role={tone === "danger" ? "alert" : "status"} className={cn("px-1 text-[13px] leading-5", tone === "danger" ? "text-cx-danger" : "text-cx-fg-3")}>{children}</p>;
}

/** Centred placeholder inside a settings card. */
export function SettingsEmpty({ children }: { children: ReactNode }) {
  return <p className="px-5 py-8 text-center text-[13px] text-cx-fg-3">{children}</p>;
}

export interface SettingsNavLinkProps {
  href: string;
  className: string;
  "aria-current"?: "page";
  onClick?: (event: MouseEvent<HTMLElement>) => void;
  children: ReactNode;
}

export interface SettingsNavItem { id: string; href: string; label: string; icon: IconName; keywords?: string }
export interface SettingsNavGroup { id: string; label: string; items: SettingsNavItem[] }
/** A single setting inside a page, already localised. */
export interface SettingsNavEntry { pageId: string; anchor: string; label: string; section: string; keywords: string }

/**
 * Scrolls to and flashes a setting once its page has rendered. Pages may be
 * lazy-loaded or waiting for service data. Wait for the page to finish loading
 * before scrolling, so newly inserted content cannot displace the target.
 */
export function revealSettingsAnchor(anchor: string, signal?: AbortSignal, onComplete?: () => void): () => void {
  const selector = `[data-settings-anchor="${CSS.escape(anchor)}"]`;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let frame = 0; let element: HTMLElement | null = null; let cancelled = false;
  // Lazy pages can also wait on service data. Observe mounts instead of burning animation
  // frames for four seconds and silently losing a slow, but valid, navigation target.
  const observer = new MutationObserver(() => locate());
  const cancel = () => {
    cancelled = true; observer.disconnect(); if (timer) clearTimeout(timer); cancelAnimationFrame(frame);
    element?.classList.remove("cx-settings-flash"); signal?.removeEventListener("abort", cancel);
  };
  const locate = () => {
    if (cancelled || signal?.aborted) { cancel(); return; }
    element = document.querySelector<HTMLElement>(selector);
    if (!element || element.closest('[aria-busy="true"]')) return;
    observer.disconnect();
    frame = requestAnimationFrame(() => { frame = requestAnimationFrame(() => {
      if (cancelled || signal?.aborted || !element?.isConnected) { cancel(); return; }
      const reduce = document.documentElement.dataset.motion === "reduce" || window.matchMedia?.("(prefers-reduced-motion: reduce)").matches;
      element.scrollIntoView({block: "center", behavior: reduce ? "auto" : "smooth"});
      element.classList.remove("cx-settings-flash"); void element.offsetWidth;
      element.classList.add("cx-settings-flash"); timer = setTimeout(() => { cancel(); onComplete?.(); }, 2000);
    }); });
  };
  signal?.addEventListener("abort", cancel, {once: true});
  observer.observe(document.getElementById("settings-main") || document.body, {childList: true, subtree: true, attributes: true, attributeFilter: ["aria-busy"]});
  timer = setTimeout(locate, 60);
  return cancel;
}

const ITEM = "flex h-[36px] items-center gap-3 rounded-xl px-3 text-left text-[14px] text-cx-fg no-underline outline-none transition-colors focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]";

export function SettingsNav({ title, back, groups, entries = [], activeId, link: Link, onNavigate, onItemClick, labels }: {
  title: ReactNode;
  back?: { label: string; href?: string; onClick?: () => void };
  groups: SettingsNavGroup[];
  /** Individual settings searchable from the nav; entries on pages missing from `groups` are ignored. */
  entries?: SettingsNavEntry[];
  activeId?: string;
  /** Host link: Next.js `Link` on web, a router-aware button on desktop. */
  link: ComponentType<SettingsNavLinkProps>;
  /** Used for keyboard submit (Enter opens the first match). */
  onNavigate: (href: string) => void;
  onItemClick?: () => void;
  labels: { search: string; clear: string; empty: string; nav: string; items?: string };
}) {
  const [query, setQuery] = useState("");
  const visible = useMemo(() => {
    const needle = query.trim().toLowerCase();
    if (!needle) return groups;
    return groups
      .map(group => ({ ...group, items: group.items.filter(item => `${item.label} ${item.keywords ?? ""} ${item.href}`.toLowerCase().includes(needle)) }))
      .filter(group => group.items.length);
  }, [groups, query]);
  const pages = useMemo(() => new Map(groups.flatMap(group => group.items.map(item => [item.id, item] as const))), [groups]);
  const matches = useMemo(() => {
    const needle = query.trim().toLowerCase();
    if (!needle) return [];
    const terms = needle.split(/\s+/);
    return entries
      .filter(entry => pages.has(entry.pageId))
      .filter(entry => { const hay = `${entry.label} ${entry.section} ${entry.keywords}`.toLowerCase(); return terms.every(term => hay.includes(term)); })
      .slice(0, 10);
  }, [entries, pages, query]);
  const openEntry = (entry: SettingsNavEntry) => {
    const page = pages.get(entry.pageId);
    if (!page) return;
    onNavigate(`${page.href}#setting-${entry.anchor}`);
    onItemClick?.();
  };
  const backClass = "flex h-[30px] w-fit items-center gap-1.5 rounded-lg px-2 text-[13px] text-cx-fg-3 no-underline outline-none transition-colors hover:bg-cx-hover hover:text-cx-fg focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]";
  const backContent = back ? <><Icon name="arrowLeft" size={15} />{back.label}</> : null;

  return <nav className="cx-settings-nav" aria-label={labels.nav}>
    <div className="flex flex-col gap-3 px-3 pb-3 pt-3">
      {back ? back.onClick || !back.href
        ? <button type="button" onClick={back.onClick} className={backClass}>{backContent}</button>
        : <Link href={back.href} className={backClass}>{backContent}</Link> : null}
      <div className="px-2 text-[20px] font-semibold leading-7 text-cx-fg">{title}</div>
      <label className="cx-settings-search">
        <Icon name="search" size={16} />
        <input type="search" value={query} onChange={event => setQuery(event.target.value)} placeholder={labels.search} aria-label={labels.search} autoComplete="off" spellCheck={false}
          onKeyDown={event => {
            const first = visible[0]?.items[0];
            if (event.key === "Enter" && matches[0] && !first) openEntry(matches[0]);
            else if (event.key === "Enter" && first) { onNavigate(first.href); onItemClick?.(); }
            else if (event.key === "Escape" && query) { event.stopPropagation(); setQuery(""); }
          }} />
        {query ? <button type="button" aria-label={labels.clear} onClick={() => setQuery("")} className="grid size-5 place-items-center rounded-full text-cx-fg-3 hover:bg-cx-active hover:text-cx-fg"><Icon name="x" size={12} /></button> : null}
      </label>
    </div>
    <div className="flex min-h-0 flex-1 flex-col gap-5 overflow-y-auto px-3 pb-5 pt-1">
      {visible.map(group => <div key={group.id} className="flex flex-col gap-0.5">
        <div className="px-3 pb-1.5 text-[13px] text-cx-fg-3">{group.label}</div>
        {group.items.map(item => {
          const current = item.id === activeId;
          return <Link key={item.id} href={item.href} aria-current={current ? "page" : undefined} onClick={onItemClick ? () => onItemClick() : undefined}
            className={cn(ITEM, current ? "bg-cx-active" : "hover:bg-cx-hover")}>
            <Icon name={item.icon} size={17} className={current ? "text-cx-fg" : "text-cx-fg-2"} />
            <span className="min-w-0 truncate">{item.label}</span>
          </Link>;
        })}
      </div>)}
      {matches.length ? <div className="flex flex-col gap-0.5" data-testid="settings-search-items">
        <div className="px-3 pb-1.5 text-[13px] text-cx-fg-3">{labels.items || "设置项"}</div>
        {matches.map(entry => {
          const page = pages.get(entry.pageId)!;
          return <button key={`${entry.pageId}:${entry.anchor}`} type="button" onClick={() => openEntry(entry)}
            className={cn(ITEM, "h-auto min-h-[40px] items-start py-2 hover:bg-cx-hover")}>
            <Icon name={page.icon} size={15} className="mt-0.5 text-cx-fg-3" />
            <span className="flex min-w-0 flex-col">
              <span className="truncate text-[13.5px]">{entry.label}</span>
              <span className="truncate text-[12px] text-cx-fg-3">{page.label} · {entry.section}</span>
            </span>
          </button>;
        })}
      </div> : null}
      {!visible.length && !matches.length ? <p className="px-3 text-[13px] text-cx-fg-3">{labels.empty}</p> : null}
    </div>
  </nav>;
}
