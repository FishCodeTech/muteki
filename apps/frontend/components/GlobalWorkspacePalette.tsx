"use client";

import { useEffect, useId, useMemo, useState, type Dispatch, type SetStateAction } from "react";
import { usePathname, useRouter } from "next/navigation";
import { Dialog, Kbd, OptionList, SearchInput, useListKeyboard, type ListOption } from "@muteki/ui";
import type { IconName } from "@muteki/ui";
import { iconOf, publicDescriptionOf, recentStateLabel } from "@/lib/workspaceNavigation";
import type { WorkspaceOverview } from "@/lib/workspace-overview";
import { useSolveOnlyMode } from "@/lib/workspaceMode";
import { fetchConversationSearch, queryLooksSearchable, type ConversationSearchHit } from "@/lib/useConversation";
import { buildThreadMessageHref } from "@/lib/conversationDeepLink";

export function GlobalWorkspacePalette({ overview, open, setOpen }: { overview: WorkspaceOverview; open: boolean; setOpen: Dispatch<SetStateAction<boolean>> }) {
  const pathname = usePathname();
  const router = useRouter();
  const solveOnly = useSolveOnlyMode();
  const [query, setQuery] = useState("");
  const [bodyHits, setBodyHits] = useState<ConversationSearchHit[]>([]);
  const [bodyHitsLoading, setBodyHitsLoading] = useState(false);
  const localRunPalette = pathname.startsWith("/run/") || pathname === "/ctf" || pathname === "/pentest";
  const onChat = pathname.startsWith("/chat");

  const commands = useMemo(() => {
    const base: Array<{ id: string; label: string; detail: string; href: string; icon: IconName }> = overview.kinds.filter((kind) => !solveOnly || kind.aggregateType === "run").map((kind) => ({
      id: `workspace-${kind.id}`,
      label: `打开${kind.title}`,
      detail: publicDescriptionOf(kind),
      href: kind.route,
      icon: iconOf(kind),
    }));
    if (solveOnly) {
      base.push(
        { id: "task-workers", label: "CTF Worker 配置", detail: "出战池、运行环境与调度预算", href: "/ctf/workers", icon: "cpu" },
        { id: "task-local-pi", label: "本地 Pi 单会话配置", detail: "Pi 单会话默认模型与端点、兼容性检查、工具边界与内置 CTF Skills", href: "/ctf/workers?section=local_pi", icon: "terminal" },
        { id: "task-versions", label: "Agent 最低版本要求", detail: "CTF / Pentest 出战 Worker 最低版本基线与升级建议", href: "/ctf/workers?section=versions", icon: "shield" },
        { id: "task-credentials", label: "Agent 凭据", detail: "配置做题 Worker 使用的凭据", href: "/ctf/workers?section=credentials", icon: "lock" },
        { id: "settings-appearance", label: "显示模式", detail: "开启对话与比赛模式", href: "/settings/appearance", icon: "gear" },
      );
    } else base.push(
      { id: "usage", label: "全局用量", detail: "对话、任务与比赛 Token 统计", href: "/usage", icon: "rows" },
      { id: "settings-hub", label: "打开设置", detail: "设置中心：Agents、能力、运维与扩展", href: "/settings", icon: "gear" },
      { id: "settings-agents", label: "Agents", detail: "引擎登录、模型、最低版本基线与接入", href: "/settings/agents", icon: "plug" },
      { id: "settings-agent-versions", label: "Agent 最低版本支持设置", detail: "CTF / Pentest / SRC 模式与出战 Worker 版本准入和升级命令", href: "/settings/agents#setting-agents-version-policy", icon: "shield" },
      { id: "settings-capabilities", label: "能力管理", detail: "Conversation Thread 授权与全局 MCP/Skills，不是 Fact 图白名单", href: "/settings/capabilities", icon: "network" },
      { id: "task-workers", label: "CTF Worker 配置", detail: "出战池、最低版本要求、运行环境、调度预算与推理模型", href: "/ctf/workers", icon: "cpu" },
      { id: "task-local-pi", label: "本地 Pi 单会话配置", detail: "Pi 单会话默认模型与端点、兼容性检查、工具边界与内置 CTF Skills", href: "/ctf/workers?section=local_pi", icon: "terminal" },
      { id: "pentest-workers", label: "Pentest 渗透设置", detail: "渗透出战 Worker、Review / Verifier 与最低版本要求", href: "/pentest/workers", icon: "shield" },
      { id: "settings-appearance", label: "外观配色", detail: "主题、配色引擎与界面语言", href: "/settings/appearance", icon: "droplet" },
      { id: "settings-extensions", label: "扩展设置", detail: "安装、升级与回滚", href: "/settings/extensions", icon: "layers" },
      { id: "competition-credentials", label: "比赛平台凭据", detail: "连接、轮换、撤销与浏览器会话", href: "/competitions?focus=credentials", icon: "lock" },
      { id: "settings-operations", label: "运行诊断与维护", detail: "指标、回执、恢复与清理", href: "/settings/operations", icon: "radio" },
    );
    for (const item of overview.recent.filter((item) => !solveOnly || item.kindId === "single-security-task").slice(0, 8)) {
      base.push({
        id: `recent-${item.kindId}-${item.id}`,
        label: item.title,
        detail: `最近工作区 · ${recentStateLabel(item.status, item.kindId, item.running)}`,
        href: item.href,
        icon: item.kindId === "conversation" ? "terminal" : item.kindId === "competition" ? "grid" : "target",
      });
    }
    if (process.env.NODE_ENV !== "production") {
      base.push({
        id: "dev-ui-preview",
        label: "Muteki UI Preview",
        detail: "内部组件验收预览页 /dev/ui",
        href: "/dev/ui",
        icon: "layers",
      });
    }
    return base;
  }, [overview.kinds, overview.recent, solveOnly]);

  const filtered = useMemo(() => {
    const needle = query.trim().toLowerCase();
    return commands.filter((command) =>
      !needle || `${command.label} ${command.detail}`.toLowerCase().includes(needle),
    );
  }, [commands, query]);
  const bodyHitsReady = !bodyHitsLoading && bodyHits.length > 0;

  useEffect(() => {
    if (!open || !onChat) {
      setBodyHits([]);
      setBodyHitsLoading(false);
      return;
    }
    if (!queryLooksSearchable(query)) {
      setBodyHits([]);
      setBodyHitsLoading(false);
      return;
    }
    const controller = new AbortController();
    setBodyHitsLoading(true);
    const timer = window.setTimeout(() => {
      void fetchConversationSearch(query, {
        limit: 8,
        signal: controller.signal,
      }).then((result) => {
        if (controller.signal.aborted) return;
        setBodyHits(result.hits || []);
        setBodyHitsLoading(false);
      }).catch(() => {
        if (controller.signal.aborted) return;
        setBodyHits([]);
        setBodyHitsLoading(false);
      });
    }, 250);
    return () => {
      window.clearTimeout(timer);
      controller.abort();
    };
  }, [open, onChat, query]);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (open && (event.metaKey || event.ctrlKey) && /^[1-9]$/.test(event.key)) {
        const index = Number(event.key) - 1;
        if (bodyHitsReady && index < bodyHits.length) {
          const hit = bodyHits[index];
          if (!hit) return;
          event.preventDefault();
          setOpen(false);
          router.push(buildThreadMessageHref(hit.thread_id, hit.message_id));
          return;
        }
        const command = filtered[index];
        if (!command) return;
        event.preventDefault();
        setOpen(false);
        router.push(command.href);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [bodyHits, bodyHitsReady, filtered, localRunPalette, open, router, setOpen]);

  useEffect(() => {
    if (open) setQuery("");
  }, [open]);

  const hitOptions = useMemo<ListOption[]>(() => (bodyHitsReady ? bodyHits : []).map((hit, index) => ({
    value: `conv-hit:${hit.thread_id}:${hit.message_id}`,
    label: hit.thread_title || "未命名对话",
    description: hit.snippet.replaceAll("«", "").replaceAll("»", ""),
    icon: "messages" as IconName,
    section: "对话正文",
    trailing: index < 9 ? <Kbd>{`⌘${index + 1}`}</Kbd> : undefined,
  })), [bodyHits, bodyHitsReady]);
  const commandOptions = useMemo<ListOption[]>(() => filtered.map((command, index) => ({
    value: command.id,
    label: command.label,
    description: command.detail,
    icon: command.icon,
    section: "工作区、设置与最近任务",
    trailing: index < 9 && !bodyHitsReady ? <Kbd>{`⌘${index + 1}`}</Kbd> : undefined,
  })), [filtered, bodyHitsReady]);
  const options = useMemo(() => [...hitOptions, ...commandOptions], [hitOptions, commandOptions]);
  const listId = useId();

  const choose = (id: string) => {
    if (id.startsWith("conv-hit:")) {
      const hit = bodyHits.find((item) => `conv-hit:${item.thread_id}:${item.message_id}` === id);
      if (!hit) return;
      setOpen(false);
      router.push(buildThreadMessageHref(hit.thread_id, hit.message_id));
      return;
    }
    const command = filtered.find((item) => item.id === id);
    if (!command) return;
    setOpen(false);
    router.push(command.href);
  };
  const keyboard = useListKeyboard(options, choose);

  if (localRunPalette && !open) return null;

  const showBodySection = onChat && (bodyHitsLoading || hitOptions.length > 0 || queryLooksSearchable(query));

  return (
    <Dialog
      open={open}
      instant
      onOpenChange={setOpen}
      size="lg"
      placement="top"
      hideClose
      title="搜索工作区"
      testId="global-workspace-palette"
      bodyClassName="p-0"
    >
      <div className="flex min-h-0 flex-col">
        <div className="px-3 pb-2 pt-1">
          <SearchInput
            data-autofocus
            size="lg"
            value={query}
            onValueChange={(value) => { setQuery(value); keyboard.setActiveIndex(0); }}
            placeholder={onChat ? "搜索工作区、设置、最近任务或对话正文" : "搜索工作区、设置或最近任务"}
            aria-label="搜索工作区、设置或最近任务"
            autoComplete="off"
            spellCheck={false}
            role="combobox"
            aria-expanded
            aria-controls={listId}
            aria-activedescendant={keyboard.activeIndex >= 0 ? `${listId}-opt-${keyboard.activeIndex}` : undefined}
            onKeyDown={(event) => {
              if (event.nativeEvent.isComposing || event.metaKey || event.ctrlKey || event.altKey) return;
              keyboard.onKeyDown(event);
            }}
            shortcutHint={<Kbd>esc</Kbd>}
          />
        </div>
        {showBodySection && !hitOptions.length ? (
          <div className="px-5 pb-1 pt-1 text-cx-xs text-cx-fg-3">
            <div className="pb-1 text-cx-2xs font-medium text-cx-fg-4">对话正文</div>
            {bodyHitsLoading ? "正在搜索正文…" : "没有匹配的正文命中"}
          </div>
        ) : null}
        <OptionList
          id={listId}
          options={options}
          activeIndex={keyboard.activeIndex}
          onActiveIndexChange={keyboard.setActiveIndex}
          onSelect={choose}
          emptyText="没有匹配的工作区、设置或最近任务"
          className="max-h-[50vh] px-2 pb-2"
        />
      </div>
    </Dialog>
  );
}
