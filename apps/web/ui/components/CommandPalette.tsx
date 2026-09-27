"use client";

import { useEffect, useMemo, useState } from "react";
import { Header, Kbd, ListBox, ListBoxItem, Modal, SearchField } from "@heroui/react";
import { Icon, type IconName } from "@/components/Icon";
import { useT, useLang } from "@/lib/i18n";
import type { RunSummary } from "@/lib/useRun";
import type { ArtifactView } from "@/lib/events";
import { panelHotkey } from "@/lib/runtimeTabs";

/**
 * Cmd/Ctrl+K command palette — a Linear/Slack/VSCode-style overlay built on
 * standard HeroUI components (Modal, SearchField, ListBox, Kbd).
 *
 * Open/close + the Cmd+K shortcut are OWNED by the parent (page.tsx) so a single
 * global handler arbitrates with panel single-key shortcuts and Esc layers.
 * This component is a pure modal: it renders nothing when `open` is false, builds
 * its command list from the props it's handed, fuzzy-filters on a flat query, and
 * runs the selected command (closing itself).
 */

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
  agents?: Array<{ id: string; title: string; engine: string; status?: string }>;
  onOpenAgent?: (id: string) => void;
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

/** Seed the composer with a `/<verb> ` prefix and focus it. */
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

  useEffect(() => {
    if (open) {
      setQuery("");
    }
  }, [open]);

  // Build the full command list (pre-filter).
  const commands = useMemo<Command[]>(() => {
    const list: Command[] = [];
    const PANEL = t("palette.sec.panels");
    const GENERAL = t("palette.sec.general");
    const WORKSPACES = "工作区";

    const navigate = (href: string) => () => window.location.assign(href);
    list.push(
      { id: "workspace-home", section: WORKSPACES, icon: "grid", label: "Muteki 首页", keywords: "home 首页", run: navigate("/") },
      { id: "workspace-chat", section: WORKSPACES, icon: "terminal", label: "对话工作区", keywords: "chat conversation 对话", run: navigate("/chat") },
      { id: "workspace-task", section: WORKSPACES, icon: "crosshair", label: "单题工作区", keywords: "task solve ctf pentest 单题", run: navigate("/task") },
      { id: "workspace-competition", section: WORKSPACES, icon: "grid", label: "比赛工作区", keywords: "competition contest 比赛", run: navigate("/competitions") },
    );

    // — General —
    list.push({
      id: "new-solve", section: GENERAL, icon: "pencil",
      label: t("palette.cmd.newSolve"), keywords: "new solve dispatch 新建 解题 派发",
      run: props.onNewSolve,
    });

    // — Panels (only meaningful once a run has started) —
    if (props.started) {
      // kbd hints come from lib/runtimeTabs.ts so they always match the global key handler.
      const panels: Array<[ArtifactView, string, IconName, string]> = [
        ["evidence", "palette.cmd.evidence", "list", "evidence 证据 证据链"],
        ["workers", "palette.cmd.workers", "cpu", "workers worker 详情"],
        ["collaboration", "palette.cmd.collaboration", "network", "agents collaboration 协作 拓扑 知识"],
        ["timeline", "palette.cmd.timeline", "clock", "timeline activity 活动 时间线"],
        ["findings", "palette.cmd.findings", "alert", "findings review 审查"],
        ["credentials", "palette.cmd.credentials", "lock", "credentials creds 凭据"],
        ["pocs", "palette.cmd.pocs", "terminal", "poc payload 工具"],
        ["routes", "palette.cmd.routes", "network", "routes branches 路线 分支"],
        ["directives", "palette.cmd.directives", "help", "directives 指令"],
      ];
      for (const [view, key, icon, kw] of panels) {
        list.push({
          id: `panel-${view}`, section: PANEL, icon,
          label: t(key), keywords: kw, kbd: panelHotkey(view),
          run: () => props.onOpenArtifact(view),
        });
      }
      list.push({
        id: "collab-search",
        section: PANEL,
        icon: "search",
        label: t("palette.cmd.collabSearch"),
        keywords: "collab search agents map 协作 搜索 /",
        kbd: "/",
        sub: t("palette.cmd.collabSearchHint"),
        run: () => {
          props.onOpenArtifact("collaboration");
          requestAnimationFrame(() => {
            document.querySelector<HTMLInputElement>(".collab-toolbar-search input")?.focus();
          });
        },
      });
      const AGENTS = t("palette.sec.agents");
      for (const agent of props.agents ?? []) {
        list.push({
          id: `agent-${agent.id}`,
          section: AGENTS,
          icon: "cpu",
          label: agent.title,
          sub: t("palette.agentMeta", { engine: agent.engine, status: agent.status || "—" }),
          keywords: `${agent.id} ${agent.engine} ${agent.title} ${agent.status ?? ""} agent worker 协作`,
          run: () => props.onOpenAgent?.(agent.id),
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
      list.push({
        id: "send-directive", section: GENERAL, icon: "send",
        label: t("palette.cmd.directive"), keywords: "directive assign steer operator 下达 指令 聚焦 转向 操作员",
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

    // — Switch run (dynamic) —
    const RUNS = t("palette.sec.runs");
    for (const r of props.runs) {
      if (r.run_id === props.activeRunId) continue;
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
  }, [open, props.started, props.running, props.runs, props.activeRunId, props.agents, lang, t]);

  // Fuzzy filter
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

  const choose = (c: Command | undefined) => {
    if (!c) return;
    onClose();
    c.run();
  };

  const grouped = useMemo(() => {
    const sections = new Map<string, Command[]>();
    for (const command of filtered) {
      const rows = sections.get(command.section) || [];
      rows.push(command);
      sections.set(command.section, rows);
    }
    return [...sections.entries()];
  }, [filtered]);

  return (
    <Modal
      isOpen={open}
      onOpenChange={(isOpen) => {
        if (!isOpen) onClose();
      }}
    >
      <Modal.Backdrop variant="opaque" isDismissable>
        <Modal.Container
          size="lg"
          placement="top"
          scroll="inside"
          className="pt-16 sm:pt-20 items-center"
        >
          <Modal.Dialog
            aria-label={t("palette.title")}
            className="w-full max-w-xl overflow-hidden p-0 rounded-2xl border border-line bg-surface shadow-overlay"
          >
            <Modal.Header className="p-3 border-b border-line">
              <div className="flex items-center gap-2 w-full">
                <SearchField
                  value={query}
                  onChange={setQuery}
                  aria-label={t("palette.searchAria")}
                  autoFocus
                  fullWidth
                  className="w-full"
                >
                  <SearchField.Group className="w-full h-10 border border-line bg-inset/50 rounded-control px-2.5 flex items-center gap-2">
                    <SearchField.SearchIcon>
                      <Icon name="search" size={16} className="text-muted shrink-0" />
                    </SearchField.SearchIcon>
                    <SearchField.Input
                      placeholder={t("palette.placeholder")}
                      autoComplete="off"
                      spellCheck={false}
                      className="text-sm text-ink placeholder:text-muted w-full"
                      onKeyDown={(e) => {
                        if (e.key === "Enter" && filtered.length > 0) {
                          e.preventDefault();
                          choose(filtered[0]);
                        }
                      }}
                    />
                    <SearchField.ClearButton aria-label="清除搜索">
                      <Icon name="x" size={14} />
                    </SearchField.ClearButton>
                  </SearchField.Group>
                </SearchField>
                <Kbd variant="default" className="shrink-0 text-[11px] font-mono px-1.5 py-0.5">
                  esc
                </Kbd>
              </div>
            </Modal.Header>

            <Modal.Body className="p-2 max-h-[50vh] overflow-y-auto">
              {filtered.length === 0 ? (
                <div className="py-10 text-center text-sm text-muted">
                  {t("palette.empty")}
                </div>
              ) : (
                <ListBox
                  aria-label={t("palette.title")}
                  onAction={(key) => choose(filtered.find((command) => command.id === String(key)))}
                  className="w-full p-0 flex flex-col gap-1"
                >
                  {grouped.map(([section, sectionCommands]) => (
                    <ListBox.Section key={section} className="flex flex-col gap-0.5">
                      <Header className="px-2.5 pt-2 pb-1 text-[11px] font-semibold uppercase tracking-wider text-muted select-none">
                        {section}
                      </Header>
                      {sectionCommands.map((command) => (
                        <ListBoxItem
                          id={command.id}
                          textValue={`${command.label} ${command.sub || ""}`}
                          key={command.id}
                          className="group flex w-full items-center justify-between gap-3 rounded-lg px-2.5 py-2 text-sm text-ink cursor-pointer hover:bg-hover data-[focused=true]:bg-hover data-[pressed=true]:scale-[0.99] transition-all outline-none"
                        >
                          <div className="flex items-center gap-2.5 min-w-0 flex-1">
                            <Icon
                              name={command.icon}
                              size={16}
                              className="shrink-0 text-muted group-hover:text-accent group-data-[focused=true]:text-accent transition-colors"
                            />
                            <div className="flex flex-col min-w-0 flex-1">
                              <span className="truncate font-medium text-ink group-hover:text-ink-strong">
                                {command.label}
                              </span>
                              {command.sub && (
                                <span className="truncate text-xs text-muted">
                                  {command.sub}
                                </span>
                              )}
                            </div>
                          </div>
                          {command.kbd && (
                            <Kbd variant="default" className="shrink-0 text-[11px] font-mono px-1.5 py-0.5">
                              {command.kbd}
                            </Kbd>
                          )}
                        </ListBoxItem>
                      ))}
                    </ListBox.Section>
                  ))}
                </ListBox>
              )}
            </Modal.Body>

            <Modal.Footer className="flex items-center justify-between border-t border-line bg-inset/30 px-3.5 py-2 text-[11px] text-muted font-mono select-none">
              <div className="flex items-center gap-3">
                <span className="inline-flex items-center gap-1.5">
                  <Kbd variant="default" className="px-1.5 py-0.5 text-[10px]">↑↓</Kbd>
                  <span>{lang === "zh" ? "选择" : "navigate"}</span>
                </span>
                <span className="opacity-40">·</span>
                <span className="inline-flex items-center gap-1.5">
                  <Kbd variant="default" className="px-1.5 py-0.5 text-[10px]">↵</Kbd>
                  <span>{lang === "zh" ? "执行" : "run"}</span>
                </span>
                <span className="opacity-40">·</span>
                <span className="inline-flex items-center gap-1.5">
                  <Kbd variant="default" className="px-1.5 py-0.5 text-[10px]">esc</Kbd>
                  <span>{lang === "zh" ? "关闭" : "close"}</span>
                </span>
              </div>
            </Modal.Footer>
          </Modal.Dialog>
        </Modal.Container>
      </Modal.Backdrop>
    </Modal>
  );
}
