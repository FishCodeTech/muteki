"use client";

import { useEffect, useMemo, useRef, useState } from "react";
import { Icon, type IconName } from "@/components/Icon";
import { useT, useLang } from "@/lib/i18n";
import type { RunSummary } from "@/lib/useRun";
import type { ArtifactView } from "@/components/ArtifactPanel";

/**
 * Cmd/Ctrl+K command palette — a Linear/Slack/VSCode-style centered overlay that
 * turns the deck's whole action surface into keyboard-reachable commands.
 *
 * Open/close + the Cmd+K shortcut are OWNED by the parent (page.tsx) so a single
 * global handler arbitrates with the panel single-key shortcuts and Esc layers.
 * This component is a pure modal: it renders nothing when `open` is false, builds
 * its command list from the props it's handed, fuzzy-filters on a flat query, and
 * runs the selected command (closing itself). The old Cmd+K "focus the composer"
 * affordance survives as the "focus composer" command + the bare "/" key.
 */

// One palette command. `keywords` widen fuzzy matching; `sub` is a muted second
// line (used for runs: category · status); `kbd` shows a direct-shortcut hint.
export interface Command {
  id: string;
  label: string;
  keywords?: string;
  sub?: string;
  icon: IconName;
  kbd?: string;
  section: string;
  run: () => void;
}

export interface PaletteData {
  open: boolean;
  onClose: () => void;
  /** the run currently selected (drives `when` for panel/worker commands). */
  started: boolean;
  running: boolean;
  runs: RunSummary[];
  activeRunId: string;
  // action callbacks (the same ones page.tsx already owns)
  onNewSolve: () => void;
  onOpenArtifact: (view: ArtifactView) => void;
  onSelectRun: (id: string) => void;
  onSpawnWorker: (engine?: string) => void;
  onOpenSettings: () => void;
}

const MAX_RUNS = 8; // cap the "switch run" matches so the list stays scannable

/** Focus whichever composer field is mounted (dispatch textarea / command input). */
function focusComposer() {
  const el = document.querySelector<HTMLElement>("[data-composer-input]");
  if (el) {
    el.focus();
    (el as HTMLInputElement).select?.();
  }
}

/** B: seed the composer with a `/<verb> ` prefix and focus it — keyboard-first way
 *  to start an operator directive. Uses the native value setter so React's
 *  controlled input picks up the change. */
function seedComposer(prefix: string) {
  const el = document.querySelector<HTMLInputElement | HTMLTextAreaElement>("[data-composer-input]");
  if (!el) return;
  const proto = el instanceof HTMLTextAreaElement
    ? window.HTMLTextAreaElement.prototype : window.HTMLInputElement.prototype;
  const setter = Object.getOwnPropertyDescriptor(proto, "value")?.set;
  setter?.call(el, prefix);
  el.dispatchEvent(new Event("input", { bubbles: true }));
  el.focus();
  const len = prefix.length;
  (el as HTMLInputElement).setSelectionRange?.(len, len);
}

export function CommandPalette(props: PaletteData) {
  const { open, onClose } = props;
  const t = useT();
  const { lang, setLang } = useLang();
  const [query, setQuery] = useState("");
  const [active, setActive] = useState(0);

  const inputRef = useRef<HTMLInputElement | null>(null);
  const listRef = useRef<HTMLDivElement | null>(null);
  const triggerRef = useRef<HTMLElement | null>(null);

  // Build the full command list (pre-filter). Only commands whose `when` holds
  // are included. Runs are appended as dynamic "switch run" commands.
  const commands = useMemo<Command[]>(() => {
    const list: Command[] = [];
    const PANEL = t("palette.sec.panels");
    const GENERAL = t("palette.sec.general");

    // — General —
    list.push({
      id: "new-solve", section: GENERAL, icon: "pencil",
      label: t("palette.cmd.newSolve"), keywords: "new solve dispatch 新建 解题 派发",
      run: props.onNewSolve,
    });

    // — Panels (only meaningful once a run has started) —
    if (props.started) {
      const panels: Array<[ArtifactView, string, IconName, string, string]> = [
        ["evidence", "palette.cmd.evidence", "list", "e", "evidence 证据 证据链"],
        ["workers", "palette.cmd.workers", "cpu", "w", "workers worker 详情"],
        ["graph", "palette.cmd.graph", "network", "g", "graph fact 事实 图"],
        ["timeline", "palette.cmd.timeline", "clock", "t", "timeline activity 活动 时间线"],
        ["blackboard", "palette.cmd.blackboard", "board", "b", "blackboard 黑板 知识"],
        ["findings", "palette.cmd.findings", "alert", "f", "findings review 审查"],
        ["credentials", "palette.cmd.credentials", "lock", "c", "credentials creds 凭据"],
        ["pocs", "palette.cmd.pocs", "terminal", "p", "poc payload 工具"],
        ["routes", "palette.cmd.routes", "network", "r", "routes branches 路线 分支"],
        ["directives", "palette.cmd.directives", "help", "d", "directives 指令"],
      ];
      for (const [view, key, icon, kbd, kw] of panels) {
        list.push({
          id: `panel-${view}`, section: PANEL, icon,
          label: t(key), keywords: kw, kbd,
          run: () => props.onOpenArtifact(view),
        });
      }
    }

    // — Spawn worker (only on a live run) —
    if (props.running) {
      list.push({
        id: "spawn-worker", section: GENERAL, icon: "cpu",
        label: t("palette.cmd.spawnWorker"), keywords: "spawn worker add engine 新增 派发",
        run: () => props.onSpawnWorker(),
      });
      // B: keyboard-first operator directive — seed `/directive ` then type the text.
      list.push({
        id: "send-directive", section: GENERAL, icon: "send",
        label: t("palette.cmd.directive"), keywords: "directive steer operator 指令 操作员 转向",
        run: () => seedComposer("/directive "),
      });
    }

    // — General: language / settings / focus —
    list.push({
      id: "toggle-lang", section: GENERAL, icon: "globe",
      label: t("palette.cmd.lang"), keywords: "language lang english chinese 中 英 语言 切换",
      run: () => setLang(lang === "zh" ? "en" : "zh"),
    });
    list.push({
      id: "open-settings", section: GENERAL, icon: "gear",
      label: t("palette.cmd.settings"), keywords: "settings worker roster 设置 引擎",
      run: props.onOpenSettings,
    });
    list.push({
      id: "focus-composer", section: GENERAL, icon: "send",
      label: t("palette.cmd.focus"), keywords: "focus composer input type 聚焦 输入", kbd: "/",
      run: focusComposer,
    });

    // — Switch run (dynamic) — list non-draft runs so typing filters to one.
    const RUNS = t("palette.sec.runs");
    for (const r of props.runs) {
      if (r.run_id === props.activeRunId) continue; // already here
      const status = t(`rail.status.${r.status}`) || r.status;
      list.push({
        id: `run-${r.run_id}`, section: RUNS, icon: "target",
        label: r.name || r.run_id,
        sub: t("palette.runMeta", { category: r.category || "—", status }),
        keywords: `${r.run_id} ${r.category} ${status} switch run 切换 解题`,
        run: () => props.onSelectRun(r.run_id),
      });
    }
    return list;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, props.started, props.running, props.runs, props.activeRunId, lang, t]);

  // Fuzzy filter = case-insensitive substring across label + keywords + sub. Run
  // commands are capped so a long history can't bury the static actions.
  const filtered = useMemo<Command[]>(() => {
    const q = query.trim().toLowerCase();
    let runMatches = 0;
    const out: Command[] = [];
    for (const c of commands) {
      const isRun = c.id.startsWith("run-");
      if (q) {
        const hay = `${c.label} ${c.keywords ?? ""} ${c.sub ?? ""}`.toLowerCase();
        if (!hay.includes(q)) continue;
      }
      if (isRun) {
        if (runMatches >= MAX_RUNS) continue;
        runMatches++;
      }
      out.push(c);
    }
    return out;
  }, [commands, query]);

  // reset query + selection each time the palette opens; capture the trigger so
  // focus can be restored on close.
  useEffect(() => {
    if (!open) return;
    triggerRef.current = (document.activeElement as HTMLElement) ?? null;
    setQuery("");
    setActive(0);
    // focus the search input on the next frame (after the modal paints)
    const id = window.requestAnimationFrame(() => inputRef.current?.focus());
    return () => {
      window.cancelAnimationFrame(id);
      // restore focus to whatever opened the palette
      triggerRef.current?.focus?.();
    };
  }, [open]);

  // keep the highlighted index in range as the filtered list shrinks/grows.
  useEffect(() => {
    setActive((i) => (filtered.length === 0 ? 0 : Math.min(i, filtered.length - 1)));
  }, [filtered.length]);

  // keep the highlighted row scrolled into view.
  useEffect(() => {
    if (!open) return;
    const el = listRef.current?.querySelector<HTMLElement>(`[data-idx="${active}"]`);
    el?.scrollIntoView({ block: "nearest" });
  }, [active, open]);

  if (!open) return null;

  const choose = (c: Command | undefined) => {
    if (!c) return;
    onClose();
    // run AFTER close so a command that moves focus (focus composer) wins the
    // focus-restore race in the open-effect cleanup.
    c.run();
  };

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === "ArrowDown") {
      e.preventDefault();
      setActive((i) => (filtered.length ? (i + 1) % filtered.length : 0));
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      setActive((i) => (filtered.length ? (i - 1 + filtered.length) % filtered.length : 0));
    } else if (e.key === "Enter") {
      e.preventDefault();
      choose(filtered[active]);
    } else if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      onClose();
    }
    // Tab is naturally trapped: the only focusable element is the search input.
  };

  // group consecutive items by section for header rendering (flat ranked list,
  // but we show a section label when it changes).
  let prevSection = "";

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div
        className="cmdk"
        role="dialog"
        aria-modal="true"
        aria-label={t("palette.title")}
        onClick={(e) => e.stopPropagation()}
        onKeyDown={onKeyDown}
      >
        <div className="cmdk-search">
          <Icon name="search" size={16} className="cmdk-search-ico" />
          <input
            ref={inputRef}
            className="cmdk-input"
            value={query}
            onChange={(e) => { setQuery(e.target.value); setActive(0); }}
            placeholder={t("palette.placeholder")}
            aria-label={t("palette.searchAria")}
            role="combobox"
            aria-expanded="true"
            aria-controls="cmdk-list"
            aria-activedescendant={filtered[active] ? `cmdk-opt-${filtered[active].id}` : undefined}
            autoComplete="off"
            spellCheck={false}
          />
          <kbd className="cmdk-esc">esc</kbd>
        </div>

        <div className="cmdk-list" id="cmdk-list" role="listbox" aria-label={t("palette.title")} ref={listRef}>
          {filtered.length === 0 ? (
            <div className="cmdk-empty">{t("palette.empty")}</div>
          ) : (
            filtered.map((c, i) => {
              const header = c.section !== prevSection ? c.section : null;
              prevSection = c.section;
              const selected = i === active;
              return (
                <div key={c.id}>
                  {header && <div className="cmdk-sec">{header}</div>}
                  <div
                    id={`cmdk-opt-${c.id}`}
                    data-idx={i}
                    role="option"
                    aria-selected={selected}
                    className={`cmdk-item${selected ? " sel" : ""}`}
                    onMouseMove={() => setActive(i)}
                    onClick={() => choose(c)}
                  >
                    <Icon name={c.icon} size={16} className="cmdk-item-ico" />
                    <span className="cmdk-item-body">
                      <span className="cmdk-item-label">{c.label}</span>
                      {c.sub && <span className="cmdk-item-sub">{c.sub}</span>}
                    </span>
                    {c.kbd && <kbd className="cmdk-item-kbd">{c.kbd}</kbd>}
                  </div>
                </div>
              );
            })
          )}
        </div>

        <div className="cmdk-foot">{t("palette.navHint")}</div>
      </div>
    </div>
  );
}

export default CommandPalette;
