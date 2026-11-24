"use client";

import { useEffect, useId, useMemo, useState } from "react";
import { useRouter } from "next/navigation";
import { Dialog, Kbd, OptionList, SearchInput, useListKeyboard, type ListOption } from "@muteki/ui";
import type { IconName } from "@muteki/ui";
import { useT, useLang } from "@/lib/i18n";
import type { RunSummary } from "@/lib/useRun";
import type { ArtifactView } from "@/lib/events";
import { panelHotkey } from "@/lib/runtimeTabs";


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
  workspaceMode?: "ctf" | "pentest";
  open: boolean;
  onClose: () => void;
  started: boolean;
  running: boolean;
  runs: RunSummary[];
  activeRunId: string;
  onNewSolve: () => void;
  onOpenArtifact: (view: ArtifactView) => void;
  onSelectRun: (id: string) => void;
  onSpawnWorker: (engine?: string) => void;
  onOpenSettings: () => void;
  readAgents?: () => Array<{ id: string; title: string; engine: string; status?: string }>;
  onOpenAgent?: (id: string) => void;
  onOpenReport?: () => void;
  onOpenWriteup?: () => void;
  onOpenSideChat?: () => void;
}

const MAX_RUNS = 8;

const COMPOSER_FIELD = "[data-composer-input], [data-c34-composer]";

function focusComposer() {
  const el = document.querySelector<HTMLElement>(COMPOSER_FIELD);
  if (el) {
    el.focus();
    (el as HTMLInputElement).select?.();
  }
}

function seedComposer(prefix: string) {
  const el = document.querySelector<HTMLInputElement | HTMLTextAreaElement>(COMPOSER_FIELD);
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
  const { lang } = useLang();
  return (
    <Dialog
      open={props.open}
      ariaLabel={lang === "zh" ? "命令面板" : "Command palette"}
      instant
      onOpenChange={(isOpen) => {
        if (!isOpen) props.onClose();
      }}
      size="lg"
      placement="top"
      hideClose
      testId="command-palette"
      bodyClassName="p-0"
      footer={<PaletteHints />}
    >
      <CommandPaletteDialog {...props} />
    </Dialog>
  );
}

function PaletteHints() {
  const { lang } = useLang();
  return (
    <div className="mr-auto flex items-center gap-3 text-cx-2xs text-cx-fg-3 select-none">
      <span className="inline-flex items-center gap-1.5">
        <Kbd>↑↓</Kbd>
        <span>{lang === "zh" ? "选择" : "navigate"}</span>
      </span>
      <span className="opacity-40">·</span>
      <span className="inline-flex items-center gap-1.5">
        <Kbd>↵</Kbd>
        <span>{lang === "zh" ? "执行" : "run"}</span>
      </span>
      <span className="opacity-40">·</span>
      <span className="inline-flex items-center gap-1.5">
        <Kbd>esc</Kbd>
        <span>{lang === "zh" ? "关闭" : "close"}</span>
      </span>
    </div>
  );
}

function CommandPaletteDialog(props: PaletteData) {
  const { open, onClose } = props;
  const t = useT();
  const { lang, setLang } = useLang();
  const router = useRouter();
  const [query, setQuery] = useState("");

  useEffect(() => {
    if (open) {
      setQuery("");
    }
  }, [open]);

  const commands = useMemo<Command[]>(() => {
    const list: Command[] = [];
    const PANEL = t("palette.sec.panels");
    const GENERAL = t("palette.sec.general");
    const WORKSPACES = "工作区";

    const navigate = (href: string) => () => router.push(href);
    list.push(
      { id: "workspace-home", section: WORKSPACES, icon: "grid", label: "Muteki 首页", keywords: "home 首页", run: navigate("/") },
      { id: "workspace-chat", section: WORKSPACES, icon: "terminal", label: "对话工作区", keywords: "chat conversation 对话", run: navigate("/chat") },
      { id: "workspace-task", section: WORKSPACES, icon: "crosshair", label: "CTF 工作区", keywords: "task solve ctf 单题", run: navigate("/ctf") },
      { id: "workspace-pentest", section: WORKSPACES, icon: "target", label: "渗透测试工作区", keywords: "pentest 授权 渗透", run: navigate("/pentest") },
      { id: "workspace-competition", section: WORKSPACES, icon: "grid", label: "比赛工作区", keywords: "competition contest 比赛", run: navigate("/competitions") },
    );

    list.push({
      id: "new-solve", section: GENERAL, icon: "pencil",
      label: props.workspaceMode === "pentest" ? "新测试" : t("palette.cmd.newSolve"), keywords: props.workspaceMode === "pentest" ? "new pentest test 新建 测试" : "new solve dispatch 新建 解题 派发",
      run: props.onNewSolve,
    });

    if (props.started) {
      const panels: Array<[ArtifactView, string, IconName, string]> = [
        ["evidence", "palette.cmd.evidence", "list", "evidence 证据 证据链"],
        ["workers", "palette.cmd.workers", "cpu", "workers worker 详情"],
        ["collaboration", "palette.cmd.collaboration", "network", "agents collaboration 协作 拓扑 知识"],
        ["timeline", "palette.cmd.timeline", "clock", "timeline activity 活动 时间线"],
        ...(props.workspaceMode === "pentest" ? [] : [["findings", "palette.cmd.findings", "alert", "findings review 审查"]] as Array<[ArtifactView, string, IconName, string]>),
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
      if (props.onOpenWriteup) list.push({ id: "ctf-writeup", section: PANEL, icon: "rows", label: "查看 Writeup", keywords: "writeup 题解 报告", run: props.onOpenWriteup });
      if (props.onOpenSideChat) {
        list.push({ id: "run-side-chat", section: PANEL, icon: "messages", label: "打开侧边聊天", keywords: "side btw chat 侧边 聊天 顺嘴问", kbd: "⌘⇧/", run: props.onOpenSideChat });
      }
      if (props.workspaceMode === "pentest" && props.onOpenReport) {
        list.push({ id: "pentest-report", section: PANEL, icon: "rows", label: "测试报告", keywords: "report 报告", run: props.onOpenReport });
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
      for (const agent of props.readAgents?.() ?? []) {
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

    if (props.running) {
      list.push({
        id: "spawn-worker", section: GENERAL, icon: "cpu",
        label: t("palette.cmd.spawnWorker"), keywords: "spawn worker add engine 新增 派发",
        run: () => props.onSpawnWorker(),
      });
      list.push({
        id: "send-directive", section: GENERAL, icon: "send",
        label: "补充任务引导", keywords: "hint steer 引导",
        run: () => seedComposer("/hint "),
      });
    }

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
  }, [open, props.started, props.running, props.runs, props.activeRunId, props.readAgents, props.workspaceMode, props.onOpenReport, lang, t, router]);

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

  const options = useMemo<ListOption[]>(() => grouped.flatMap(([section, rows]) => rows.map((command) => ({
    value: command.id,
    label: command.label,
    description: command.sub,
    icon: command.icon,
    section,
    trailing: command.kbd ? <Kbd>{command.kbd}</Kbd> : undefined,
  }))), [grouped]);
  const listId = useId();
  const keyboard = useListKeyboard(options, (id) => choose(filtered.find((command) => command.id === id)));

  return (
    <div className="flex flex-col">
      <div className="p-3">
        <SearchInput
          data-autofocus
          size="lg"
          value={query}
          onValueChange={(value) => { setQuery(value); keyboard.setActiveIndex(0); }}
          aria-label={t("palette.searchAria")}
          placeholder={t("palette.placeholder")}
          autoComplete="off"
          spellCheck={false}
          role="combobox"
          aria-expanded
          aria-controls={listId}
          aria-activedescendant={keyboard.activeIndex >= 0 ? `${listId}-opt-${keyboard.activeIndex}` : undefined}
          onKeyDown={keyboard.onKeyDown}
          shortcutHint={<Kbd>esc</Kbd>}
        />
      </div>
      <OptionList
        id={listId}
        options={options}
        activeIndex={keyboard.activeIndex}
        onActiveIndexChange={keyboard.setActiveIndex}
        onSelect={(id) => choose(filtered.find((command) => command.id === id))}
        emptyText={t("palette.empty")}
        className="max-h-[50vh] px-2 pb-2 pt-0"
      />
    </div>
  );
}
