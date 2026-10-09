"use client";

/* ─────────────────────────────────────────────────────────
 * CONVERSATION SHORTCUTS HELP — C34 keyboard shortcut reference.
 * Opened by the `?` key (when focus is not in an input field). The shared
 * Dialog owns focus trap, Escape / backdrop dismissal and focus restore (#134).
 * ───────────────────────────────────────────────────────── */

import React, { useEffect, useMemo, useState } from "react";
import { Dialog, EmptyState, Kbd, SearchInput, Shortcut, splitShortcut } from "@/components/chat/ui";
import { desktopChatBridge } from "@/lib/desktopChatBridge";
import { useLang } from "@/lib/i18n";
import { useChatPreferences, type SendKey } from "@/lib/chatPreferences";
import { useShortcutBindings, type ShortcutActionId } from "@/lib/shortcutBindings";

interface ShortcutRow {
  /** Alternative bindings, each in `mod+shift+k` form. */
  keys?: string[];
  /** Plain-text context label (used by the conflict notes instead of key caps). */
  label?: string;
  description: string;
  /** Keys come from the user's bindings / send-key preference instead of `keys`. */
  action?: ShortcutActionId | "send" | "newline";
}

interface ShortcutSection {
  title: string;
  wide?: boolean;
  rows: ShortcutRow[];
}

const SHORTCUT_SECTIONS: ShortcutSection[] = [
  {
    title: "导航",
    rows: [
      { action: "newChat", keys: ["mod+shift+n"], description: "新建对话" },
      { action: "search", keys: ["mod+k"], description: "命令面板（搜索 / 跳转会话）" },
      { action: "focusComposer", keys: ["/"], description: "聚焦输入框" },
      { keys: ["esc"], description: "关闭当前面板 / 返回输入框" },
    ],
  },
  {
    title: "侧栏",
    rows: [
      { action: "toggleSidebar", keys: ["mod+b"], description: "切换会话侧栏" },
      { keys: ["mod+1"], description: "跳到第 1–9 个对话（按住 ⌘ 显示编号）" },
      { keys: ["mod+shift+[", "mod+shift+]"], description: "上一个 / 下一个对话" },
      { keys: ["mod+shift+s"], description: "归置 / 移回当前对话" },
      { keys: ["mod+shift+p"], description: "置顶 / 取消置顶当前对话" },
      { keys: ["mod+z"], description: "撤销刚才的归置、稍后提醒或置顶" },
      { label: "⌘ / Shift + 点击", description: "多选对话，右键批量操作" },
      { label: "双击标题", description: "重命名对话" },
    ],
  },
  {
    title: "输入与发送",
    rows: [
      { action: "send", keys: ["enter"], description: "发送" },
      { action: "newline", keys: ["shift+enter"], description: "换行" },
      { keys: ["mod+enter"], description: "提交用户输入表单" },
      { keys: ["@"], description: "引用文件或能力" },
      { keys: ["/"], description: "命令（输入框开头）" },
      { keys: ["up", "down"], description: "浏览发送历史（输入框为空时）" },
    ],
  },
  {
    title: "面板",
    rows: [
      { action: "togglePanel", keys: ["mod+alt+b"], description: "打开 / 关闭工作面板" },
      { action: "panelDiff", keys: ["mod+alt+d"], description: "变更 (Diff)" },
      { action: "panelPreview", keys: ["mod+alt+p"], description: "预览" },
      { action: "panelFiles", keys: ["mod+alt+f"], description: "文件" },
      { action: "panelTerminal", keys: ["mod+alt+t"], description: "终端" },
      { action: "panelOverview", keys: ["mod+alt+o"], description: "概览" },
      { action: "toggleLog", keys: ["mod+j"], description: "执行日志" },
      { action: "modelPicker", keys: ["mod+shift+m"], description: "选择模型" },
      { action: "effortPicker", keys: ["mod+shift+e"], description: "思考强度" },
      { action: "accessPicker", keys: ["mod+shift+a"], description: "Agent 操作权限" },
      { action: "interactionMode", keys: ["shift+tab"], description: "切换规划模式（输入框聚焦时）" },
      { action: "help", keys: ["?"], description: "打开此帮助" },
    ],
  },
  {
    title: "面板启动器（启动器可见时）",
    rows: [
      { keys: ["d"], description: "变更" },
      { keys: ["b"], description: "预览" },
      { keys: ["f"], description: "文件" },
      { keys: ["t"], description: "终端" },
      { keys: ["o"], description: "概览" },
      { keys: ["p"], description: "计划" },
      { keys: ["a"], description: "Agents" },
      { keys: ["r"], description: "Pull request" },
    ],
  },
  {
    title: "阅读与审批",
    rows: [
      { keys: ["alt+up", "alt+down"], description: "上一轮 / 下一轮" },
      { label: "右侧小地图", description: "悬停预览提问，点击跳到该轮，按住拖动快速滚动" },
      { keys: ["y"], description: "批准（审批卡聚焦时）" },
      { keys: ["a"], description: "记住此选择（Runtime 提供时）" },
      { keys: ["n"], description: "拒绝（审批卡聚焦时）" },
    ],
  },
  {
    title: "冲突说明",
    wide: true,
    rows: [
      { label: "中文输入法 (IME)", description: "组字期间自动屏蔽 Enter 与所有单键快捷键，确认候选词不会误发送" },
      { label: "终端内", description: "键盘事件由终端捕获，不会触发全局快捷键；按 Esc 或点击外部即可退出" },
      { label: "浏览器快捷键", description: "⌘/Ctrl+K、⌘/Ctrl+Shift+N 等优先生效，已避开 ⌘/Ctrl+N（浏览器新窗口）" },
    ],
  },
];

const KEY_WORDS: Record<string, string> = {
  mod: "cmd ctrl ⌘",
  alt: "option alt ⌥",
  shift: "shift ⇧",
  enter: "enter return ↵",
  esc: "esc escape",
  up: "up ↑",
  down: "down ↓",
};

const ENGLISH: Record<string, string> = {
  "导航": "Navigation", "侧栏": "Sidebar", "跳到第 1–9 个对话（按住 ⌘ 显示编号）": "Jump to conversation 1–9 (hold ⌘ to show numbers)",
  "上一个 / 下一个对话": "Previous / next conversation", "归置 / 移回当前对话": "Settle / unsettle the current conversation",
  "置顶 / 取消置顶当前对话": "Pin / unpin the current conversation", "撤销刚才的归置、稍后提醒或置顶": "Undo the last settle, snooze, or pin",
  "⌘ / Shift + 点击": "⌘ / Shift + click", "多选对话，右键批量操作": "Select several conversations; right-click for bulk actions",
  "双击标题": "Double-click a title", "重命名对话": "Rename the conversation", "新建对话": "New chat", "命令面板（搜索 / 跳转会话）": "Search and switch conversations",
  "聚焦输入框": "Focus composer", "切换会话侧栏": "Toggle conversation sidebar", "关闭当前面板 / 返回输入框": "Close panel and return to composer",
  "输入与发送": "Compose and send", "发送": "Send", "换行": "New line", "提交用户输入表单": "Submit input form",
  "引用文件或能力": "Mention a file or capability", "命令（输入框开头）": "Command at the start of the composer",
  "浏览发送历史（输入框为空时）": "Recall sent messages when the composer is empty",
  "面板": "Panels", "打开 / 关闭工作面板": "Toggle work panel", "变更 (Diff)": "Changes (Diff)",
  "预览": "Preview", "文件": "Files", "终端": "Terminal", "概览": "Overview", "执行日志": "Execution log",
  "选择模型": "Select model", "打开此帮助": "Open shortcut help",
  "切换规划模式（输入框聚焦时）": "Toggle plan mode (while the composer is focused)",
  "面板启动器（启动器可见时）": "Panel launcher (when visible)",
  "阅读与审批": "Reading and approvals", "上一轮 / 下一轮": "Previous / next turn",
  "右侧小地图": "Minimap", "悬停预览提问，点击跳到该轮，按住拖动快速滚动": "Hover to preview a prompt, click to jump to its turn, drag to scrub",
  "批准（审批卡聚焦时）": "Approve (approval card focused)", "记住此选择（Runtime 提供时）": "Remember this choice (when the runtime offers it)",
  "拒绝（审批卡聚焦时）": "Deny (approval card focused)",
  "变更": "Changes", "计划": "Plan", "冲突说明": "Shortcut context", "中文输入法 (IME)": "Input method (IME)",
  "组字期间自动屏蔽 Enter 与所有单键快捷键，确认候选词不会误发送": "Enter and single-key shortcuts are ignored during composition, so confirming a candidate does not send the message.",
  "终端内": "Inside the terminal", "键盘事件由终端捕获，不会触发全局快捷键；按 Esc 或点击外部即可退出": "The terminal captures keyboard input; click outside to return to the chat controls.",
  "浏览器快捷键": "Browser shortcuts", "⌘/Ctrl+K、⌘/Ctrl+Shift+N 等优先生效，已避开 ⌘/Ctrl+N（浏览器新窗口）": "Chat uses ⌘/Ctrl+K and ⌘/Ctrl+Shift+N; ⌘/Ctrl+N remains the browser's new-window shortcut.",
};

function rowKeys(row: ShortcutRow, bindings: Record<ShortcutActionId, string>, sendKey: SendKey): string[] | undefined {
  if (row.action === "send") return [sendKey === "mod-enter" ? "mod+enter" : "enter"];
  if (row.action === "newline") return sendKey === "mod-enter" ? ["enter", "shift+enter"] : ["shift+enter"];
  if (row.action) return [bindings[row.action]];
  return row.keys;
}

function shortcutSections(native: boolean, en: boolean, bindings: Record<ShortcutActionId, string>, sendKey: SendKey): ShortcutSection[] {
  const copy = (value: string) => en ? ENGLISH[value] || value : value;
  const sections: ShortcutSection[] = SHORTCUT_SECTIONS.map(section => ({
    ...section, title: copy(section.title), rows: section.rows.map(row => ({
      ...row, keys: rowKeys(row, bindings, sendKey), description: copy(row.description), label: row.label ? copy(row.label) : undefined,
    })),
  }));
  if (native) {
    sections.unshift({ title: en ? "Desktop window" : "桌面窗口", rows: [
      { keys: ["mod+,"], description: en ? "Open settings" : "打开设置" },
      { keys: ["mod+r"], description: en ? "Save drafts and reload chat; reload a focused preview" : "保存草稿后刷新聊天；预览聚焦时刷新预览" },
      { keys: ["alt+left", "alt+right"], description: en ? "Navigate workspace history; navigate a focused preview" : "工作台后退 / 前进；预览聚焦时使用预览历史" },
    ] });
    sections[sections.length - 1] = { title: en ? "Shortcut context" : "冲突说明", wide: true, rows: [
      sections[sections.length - 1].rows[0],
      { label: en ? "Native commands" : "原生命令", description: en ? "Settings, search, sidebar, and reload are handled once by the desktop window, including when a preview or terminal has focus." : "设置、搜索、侧栏和刷新由桌面窗口统一处理；预览或终端聚焦时也只触发一次。" },
      { label: en ? "Terminal input" : "终端输入", description: en ? "Other keys go to the terminal. Click outside to return to chat; Escape remains terminal input." : "其余按键交给终端。点击外部返回聊天；Esc 保留为终端输入。" },
    ] };
  }
  return sections;
}

function rowHaystack(section: ShortcutSection, row: ShortcutRow): string {
  const keyText = (row.keys ?? [])
    .flatMap((binding) => binding.split("+").map((part) => `${part} ${KEY_WORDS[part] ?? ""}`))
    .join(" ");
  const glyphs = (row.keys ?? []).flatMap((binding) => splitShortcut(binding)).join(" ");
  return `${section.title} ${row.description} ${row.label ?? ""} ${keyText} ${glyphs}`.toLowerCase();
}

function RowKeys({ row }: { row: ShortcutRow }) {
  if (row.label) {
    return <Kbd tone="subtle" className="h-5 px-1.5 text-[12px]">{row.label}</Kbd>;
  }
  return (
    <span className="flex shrink-0 items-center gap-1">
      {(row.keys ?? []).map((binding, index) => (
        <React.Fragment key={binding}>
          {index > 0 ? <span className="text-[12px] text-cx-fg-4">/</span> : null}
          <Shortcut keys={binding} className="gap-[3px] [&>kbd]:h-5 [&>kbd]:min-w-5 [&>kbd]:text-[12px]" />
        </React.Fragment>
      ))}
    </span>
  );
}

export interface ConversationShortcutsHelpProps {
  open: boolean;
  onClose: () => void;
}

/** The shortcut reference without dialog chrome, for hosts that show it as a page. */
export function ConversationShortcutsList({ query = "", columns = 2 }: { query?: string; columns?: 1 | 2 }) {
  const { lang } = useLang();
  const en = lang === "en", native = Boolean(desktopChatBridge());
  const bindings = useShortcutBindings();
  const { sendKey } = useChatPreferences();
  const needle = query.trim().toLowerCase();
  const sections = useMemo(() => {
    const all = shortcutSections(native, en, bindings, sendKey);
    if (!needle) return all;
    return all
      .map((section) => ({ ...section, rows: section.rows.filter((row) => rowHaystack(section, row).includes(needle)) }))
      .filter((section) => section.rows.length);
  }, [needle, native, en, bindings, sendKey]);
  if (!sections.length) {
    return <EmptyState compact icon="search" title={en ? "No matching shortcuts" : "没有匹配的快捷键"} description={en ? `No actions match “${query.trim()}”.` : `未找到与 “${query.trim()}” 相关的操作。`} />;
  }
  return (
    <div className={columns === 2 ? "grid grid-cols-1 gap-x-8 gap-y-5 sm:grid-cols-2" : "flex flex-col gap-6"}>
      {sections.map((section) => (
        <section key={section.title} aria-label={section.title} className={section.wide && columns === 2 ? "sm:col-span-2" : undefined}>
          <h3 className="mb-1 text-[12px] font-semibold text-cx-fg-3">{section.title}</h3>
          <dl className="flex flex-col">
            {section.rows.map((row) => (
              row.label ? (
                <div key={row.label} className="flex items-start gap-3 border-b border-cx-border-subtle py-2 last:border-b-0">
                  <dt className="w-[132px] shrink-0 pt-px"><RowKeys row={row} /></dt>
                  <dd className="min-w-0 flex-1 text-[13px] leading-5 text-cx-fg-2">{row.description}</dd>
                </div>
              ) : (
                <div key={`${row.description}-${(row.keys ?? []).join(",")}`} className="flex min-h-8 items-center justify-between gap-4 border-b border-cx-border-subtle py-1 last:border-b-0">
                  <dt className="min-w-0 text-[13px] leading-5 text-cx-fg-2">{row.description}</dt>
                  <dd className="shrink-0"><RowKeys row={row} /></dd>
                </div>
              )
            ))}
          </dl>
        </section>
      ))}
    </div>
  );
}

export function ConversationShortcutsHelp({ open, onClose }: ConversationShortcutsHelpProps) {
  const [query, setQuery] = useState("");
  const { lang } = useLang();
  const en = lang === "en";

  useEffect(() => {
    if (open) setQuery("");
  }, [open]);

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => { if (!next) onClose(); }}
      size="lg"
      icon="keyboard"
      title={en ? "Keyboard shortcuts" : "键盘快捷键"}
      description={en ? "macOS uses ⌘ / ⌥; other systems use Ctrl / Alt." : "组合键在 macOS 上显示为 ⌘ / ⌥，其他系统为 Ctrl / Alt。"}
      testId="conversation-shortcuts-help"
      bodyClassName="pt-1"
    >
      <div className="flex flex-col gap-4 pb-3">
        <SearchInput
          value={query}
          onValueChange={setQuery}
          placeholder={en ? "Search shortcuts or actions…" : "搜索快捷键或操作…"}
          aria-label={en ? "Search shortcuts" : "搜索快捷键"}
          data-autofocus
          size="md"
        />
        <ConversationShortcutsList query={query} />
      </div>
    </Dialog>
  );
}
