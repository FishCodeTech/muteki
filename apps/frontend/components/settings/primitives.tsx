"use client";

import { useId, useMemo, useRef, useState, type HTMLAttributes, type ComponentType, type MouseEvent, type ReactNode } from "react";
import { Icon, type IconName } from "@muteki/ui";
import { Input } from "@muteki/ui";
import { cn } from "@/lib/cn";
import { usePrefersReducedMotion } from "@/lib/usePrefersReducedMotion";


export function SettingsPage({ title, description, actions, wide = false, children }: {
  title: ReactNode; description?: ReactNode; actions?: ReactNode; wide?: boolean; children: ReactNode;
}) {
  return <div className={cn("cx-settings-page mx-auto flex w-full flex-col gap-8 px-8 pb-16 pt-10", wide ? "max-w-[1080px]" : "max-w-[960px]")}>
    <header className="flex items-start justify-between gap-4">
      <div className="min-w-0">
        <h1 className="text-cx-lg font-semibold leading-8 tracking-normal text-cx-fg">{title}</h1>
        {description ? <p className="mt-1 text-cx-sm leading-5 text-cx-fg-3">{description}</p> : null}
      </div>
      {actions ? <div className="flex shrink-0 items-center gap-2 pt-1">{actions}</div> : null}
    </header>
    {children}
  </div>;
}

export function SettingsCard({ className, ...props }: HTMLAttributes<HTMLDivElement>) {
  return <div {...props} className={cn("cx-settings-card", className)} />;
}

export function SettingsSection({ title, description, actions, children, className, bodyClassName, bare = false, anchor }: {
  title?: ReactNode; description?: ReactNode; actions?: ReactNode; children: ReactNode; className?: string; bodyClassName?: string;
  bare?: boolean;
  anchor?: string;
}) {
  return <section className={cn("flex flex-col gap-2", className)} data-settings-anchor={anchor} id={anchor ? `setting-${anchor}` : undefined}>
    {title || description || actions ? <div className="flex flex-wrap items-end justify-between gap-x-3 gap-y-2 px-1">
      <div className="min-w-0">
        {title ? <h2 className="text-cx-base font-semibold text-cx-fg">{title}</h2> : null}
        {description ? <p className="mt-0.5 text-cx-xs leading-5 text-cx-fg-3">{description}</p> : null}
      </div>
      {actions ? <div className="ml-auto flex shrink-0 items-center gap-2">{actions}</div> : null}
    </div> : null}
    {bare ? <div className={bodyClassName}>{children}</div> : <SettingsCard className={bodyClassName}>{children}</SettingsCard>}
  </section>;
}

export function SettingsRow({ title, description, control, stacked = false, children, leading, anchor }: {
  title: ReactNode; description?: ReactNode; control?: ReactNode; stacked?: boolean; children?: ReactNode; leading?: ReactNode;
  anchor?: string;
}) {
  return <div className="cx-settings-row border-b border-cx-border-subtle px-5 py-4 last:border-b-0" data-settings-anchor={anchor} id={anchor ? `setting-${anchor}` : undefined}>
    <div className={cn("cx-settings-row-layout flex gap-4", stacked ? "flex-col" : "items-center justify-between")}>
      <div className="flex min-w-0 flex-1 items-start gap-3">
        {leading ? <div className="shrink-0">{leading}</div> : null}
        <div className="min-w-0 flex-1">
          <div className="text-cx-sm font-medium leading-5 text-cx-fg">{title}</div>
          {description ? <div className="mt-1 text-cx-xs leading-5 text-cx-fg-3">{description}</div> : null}
        </div>
      </div>
      {control ? <div className={cn("min-w-0", stacked ? "w-full" : "shrink-0")}>{control}</div> : null}
    </div>
    {children ? <div className="mt-3">{children}</div> : null}
  </div>;
}

export function SettingsNote({ tone = "neutral", children }: { tone?: "neutral" | "danger"; children: ReactNode }) {
  return <p role={tone === "danger" ? "alert" : "status"} className={cn("px-1 text-cx-xs leading-5", tone === "danger" ? "text-cx-danger" : "text-cx-fg-3")}>{children}</p>;
}

export function SettingsEmpty({ children }: { children: ReactNode }) {
  return <p className="px-5 py-8 text-center text-cx-sm text-cx-fg-3">{children}</p>;
}

export function SettingsDisclosure({ title, meta, icon, anchor, defaultOpen = false, children }: {
  title: ReactNode; meta?: ReactNode; icon?: IconName; anchor?: string; defaultOpen?: boolean; children: ReactNode;
}) {
  const [open, setOpen] = useState(defaultOpen);
  const reduce = usePrefersReducedMotion();
  const contentId = useId();
  return (
    <div
      className="cx-settings-card"
      data-settings-anchor={anchor}
      id={anchor ? `setting-${anchor}` : undefined}
    >
      <button
        type="button"
        aria-expanded={open}
        aria-controls={contentId}
        onClick={() => setOpen((curr) => !curr)}
        className="flex min-h-[52px] w-full cursor-pointer items-center gap-3 px-5 py-3 text-left select-none hover:bg-cx-hover/60 focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]"
      >
        {icon ? <Icon name={icon} size={16} className="shrink-0 text-cx-fg-3" /> : null}
        <span className="min-w-0 flex-1 truncate text-cx-base font-semibold text-cx-fg">{title}</span>
        {meta ? <span className="flex shrink-0 items-center gap-2 text-cx-xs text-cx-fg-3">{meta}</span> : null}
        <Icon
          name="chevronDown"
          size={15}
          className={cn(
            "shrink-0 text-cx-fg-3 transition-transform ease-cx-out",
            reduce ? "duration-0" : "duration-[200ms]",
            open && "rotate-180",
          )}
        />
      </button>
      <div
        id={contentId}
        inert={!open || undefined}
        aria-hidden={!open || undefined}
        className={cn(
          "grid transition-[grid-template-rows,opacity] ease-cx-out",
          reduce ? "duration-0" : "duration-[200ms]",
          open ? "grid-rows-[1fr] opacity-100" : "grid-rows-[0fr] opacity-0",
        )}
      >
        <div className="min-h-0 overflow-hidden">
          <div className="border-t border-cx-border-subtle">{children}</div>
        </div>
      </div>
    </div>
  );
}

export interface SettingsNavLinkProps {
  href: string;
  className: string;
  "aria-current"?: "page";
  onClick?: (event: MouseEvent<HTMLElement>) => void;
  children: ReactNode;
}

export interface SettingsNavItem { id: string; href: string; label: string; icon: IconName; trailingIcon?: IconName; keywords?: string }
export interface SettingsNavGroup { id: string; label: string; items: SettingsNavItem[] }
export interface SettingsNavEntry { pageId: string; anchor: string; label: string; section: string; keywords: string }

export function revealSettingsAnchor(anchor: string, signal?: AbortSignal, onComplete?: () => void): () => void {
  const selector = `[data-settings-anchor="${CSS.escape(anchor)}"]`;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let frame = 0; let element: HTMLElement | null = null; let cancelled = false;
  const observer = new MutationObserver(() => locate());
  const cancel = () => {
    cancelled = true; observer.disconnect(); if (timer) clearTimeout(timer); cancelAnimationFrame(frame);
    element?.classList.remove("cx-settings-flash"); signal?.removeEventListener("abort", cancel);
  };
  const locate = () => {
    if (cancelled || signal?.aborted) { cancel(); return; }
    element = [...document.querySelectorAll<HTMLElement>(selector)].find(node => node.getClientRects().length > 0) ?? null;
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
  observer.observe(document.getElementById("settings-main") || document.body, {childList: true, subtree: true, attributes: true, attributeFilter: ["aria-busy", "style", "hidden"]});
  timer = setTimeout(locate, 60);
  return cancel;
}

const ITEM = "flex h-[36px] items-center gap-3 rounded-xl px-3 text-left text-cx-base text-cx-fg no-underline outline-none transition-colors focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]";

export function SettingsNav({ title, back, groups, entries = [], activeId, link: Link, onNavigate, onItemClick, collapsible = false, labels }: {
  title: ReactNode;
  back?: { label: string; href?: string; onClick?: () => void };
  groups: SettingsNavGroup[];
  entries?: SettingsNavEntry[];
  activeId?: string;
  link: ComponentType<SettingsNavLinkProps>;
  onNavigate: (href: string) => void;
  onItemClick?: () => void;
  collapsible?: boolean;
  labels: { search: string; clear: string; empty: string; nav: string; items?: string };
}) {
  const [query, setQuery] = useState("");
  const [expanded, setExpanded] = useState(false);
  const toggle = useRef<HTMLButtonElement>(null);
  const searchId = useId();
  const resultsId = useId();
  const close = () => {
    if (!expanded || !toggle.current?.getClientRects().length) return;
    setExpanded(false);
    toggle.current.focus();
  };
  const selectItem = () => {
    close();
    onItemClick?.();
  };
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
    selectItem();
  };
  const backClass = "flex h-[30px] w-fit items-center gap-1.5 rounded-lg px-2 text-cx-sm text-cx-fg-3 no-underline outline-none transition-colors hover:bg-cx-hover hover:text-cx-fg focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]";
  const backContent = back ? <><Icon name="arrowLeft" size={15} />{back.label}</> : null;

  return <nav className="cx-settings-nav" aria-label={labels.nav}
    data-expanded={collapsible ? expanded : undefined}
    onKeyDown={event => {
      if (event.key === "Escape" && !event.isPropagationStopped() && expanded) close();
    }}>
    <div className="cx-settings-nav-header flex flex-col gap-3 px-3 pb-3 pt-3">
      {back ? back.onClick || !back.href
        ? <button type="button" onClick={back.onClick} className={backClass}>{backContent}</button>
        : <Link href={back.href} className={backClass}>{backContent}</Link> : null}
      <div className="cx-settings-nav-title px-2 text-cx-lg font-semibold leading-7 text-cx-fg">{title}</div>
      {collapsible ? <button ref={toggle} type="button" className="cx-settings-nav-toggle"
        aria-expanded={expanded} aria-controls={`${searchId} ${resultsId}`} onClick={() => setExpanded(open => !open)}>
        {labels.nav}<Icon name="chevronDown" size={16} />
      </button> : null}
      <Input id={searchId} type="search" icon="search" className="cx-settings-search" value={query} onChange={event => setQuery(event.target.value)} placeholder={labels.search} aria-label={labels.search} autoComplete="off" spellCheck={false}
        onKeyDown={event => {
          const first = visible[0]?.items[0];
          if (event.key === "Enter" && matches[0] && !first) openEntry(matches[0]);
          else if (event.key === "Enter" && first) { onNavigate(first.href); selectItem(); }
          else if (event.key === "Escape" && query) { event.stopPropagation(); setQuery(""); }
        }}
        trailing={query ? <button type="button" aria-label={labels.clear} onClick={() => setQuery("")} className="grid size-6 place-items-center rounded-md text-cx-fg-4 hover:bg-cx-hover hover:text-cx-fg"><Icon name="x" size={13} /></button> : null} />
    </div>
    <div id={resultsId} className="cx-settings-nav-results flex min-h-0 flex-1 flex-col gap-5 overflow-y-auto px-3 pb-5 pt-1">
      {visible.map(group => <div key={group.id} className="flex flex-col gap-0.5">
        <div className="px-3 pb-1.5 text-cx-sm text-cx-fg-3">{group.label}</div>
        {group.items.map(item => {
          const current = item.id === activeId;
          return <Link key={item.id} href={item.href} aria-current={current ? "page" : undefined} onClick={event => {
            if (event.button === 0 && !event.metaKey && !event.ctrlKey && !event.shiftKey && !event.altKey) selectItem();
          }}
            className={cn(ITEM, current ? "bg-cx-active" : "hover:bg-cx-hover")}>
            <Icon name={item.icon} size={17} className={current ? "text-cx-fg" : "text-cx-fg-2"} />
            <span className="min-w-0 truncate">{item.label}</span>
            {item.trailingIcon ? <Icon name={item.trailingIcon} size={13} className="ml-auto shrink-0 text-cx-fg-3" /> : null}
          </Link>;
        })}
      </div>)}
      {matches.length ? <div className="flex flex-col gap-0.5" data-testid="settings-search-items">
        <div className="px-3 pb-1.5 text-cx-sm text-cx-fg-3">{labels.items || "设置项"}</div>
        {matches.map(entry => {
          const page = pages.get(entry.pageId)!;
          return <button key={`${entry.pageId}:${entry.anchor}`} type="button" onClick={() => openEntry(entry)}
            className={cn(ITEM, "h-auto min-h-[40px] items-start py-2 hover:bg-cx-hover")}>
            <Icon name={page.icon} size={15} className="mt-0.5 text-cx-fg-3" />
            <span className="flex min-w-0 flex-col">
              <span className="truncate text-cx-sm">{entry.label}</span>
              <span className="truncate text-cx-xs text-cx-fg-3">{page.label} · {entry.section}</span>
            </span>
          </button>;
        })}
      </div> : null}
      {!visible.length && !matches.length ? <p className="px-3 text-cx-sm text-cx-fg-3">{labels.empty}</p> : null}
    </div>
  </nav>;
}
