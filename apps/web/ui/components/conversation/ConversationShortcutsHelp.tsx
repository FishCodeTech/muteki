"use client";

/* ─────────────────────────────────────────────────────────
 * CONVERSATION SHORTCUTS HELP — C34 keyboard shortcut reference.
 * Opened by the `?` key (when focus is not in an input field). The shared
 * Dialog owns focus trap, Escape / backdrop dismissal and focus restore (#134).
 * ───────────────────────────────────────────────────────── */

import React, { useEffect, useMemo, useState } from "react";
import { Dialog, EmptyState, Kbd, SearchInput, Shortcut, splitShortcut } from "@/components/chat/ui";

interface ShortcutRow {
  /** Alternative bindings, each in `mod+shift+k` form. */
  keys?: string[];
  /** Plain-text context label (used by the conflict notes instead of key caps). */
  label?: string;
  description: string;
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
      { keys: ["mod+shift+n"], description: "新建对话" },
      { keys: ["mod+k"], description: "命令面板（搜索 / 跳转会话）" },
      { keys: ["/"], description: "聚焦输入框" },
      { keys: ["esc"], description: "关闭当前面板 / 返回输入框" },
    ],
  },
  {
    title: "输入与发送",
    rows: [
      { keys: ["enter"], description: "发送" },
      { keys: ["shift+enter"], description: "换行" },
      { keys: ["mod+enter"], description: "提交用户输入表单" },
      { keys: ["@"], description: "引用文件或能力" },
      { keys: ["/"], description: "命令（输入框开头）" },
      { keys: ["up", "down"], description: "浏览发送历史（输入框为空时）" },
    ],
  },
  {
    title: "面板",
    rows: [
      { keys: ["mod+alt+b"], description: "打开 / 关闭工作面板" },
      { keys: ["mod+alt+d"], description: "变更 (Diff)" },
      { keys: ["mod+alt+p"], description: "预览" },
      { keys: ["mod+alt+f"], description: "文件" },
      { keys: ["mod+alt+t"], description: "终端" },
      { keys: ["mod+alt+o"], description: "概览" },
      { keys: ["mod+j"], description: "执行日志" },
      { keys: ["mod+shift+m"], description: "选择模型" },
      { keys: ["?"], description: "打开此帮助" },
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

function rowHaystack(section: ShortcutSection, row: ShortcutRow): string {
  const keyText = (row.keys ?? [])
    .flatMap((binding) => binding.split("+").map((part) => `${part} ${KEY_WORDS[part] ?? ""}`))
    .join(" ");
  const glyphs = (row.keys ?? []).flatMap((binding) => splitShortcut(binding)).join(" ");
  return `${section.title} ${row.description} ${row.label ?? ""} ${keyText} ${glyphs}`.toLowerCase();
}

function RowKeys({ row }: { row: ShortcutRow }) {
  if (row.label) {
    return <Kbd tone="subtle" className="h-5 px-1.5 text-[11.5px]">{row.label}</Kbd>;
  }
  return (
    <span className="flex shrink-0 items-center gap-1">
      {(row.keys ?? []).map((binding, index) => (
        <React.Fragment key={binding}>
          {index > 0 ? <span className="text-[11px] text-cx-fg-4">/</span> : null}
          <Shortcut keys={binding} className="gap-[3px] [&>kbd]:h-5 [&>kbd]:min-w-5 [&>kbd]:text-[11px]" />
        </React.Fragment>
      ))}
    </span>
  );
}

export interface ConversationShortcutsHelpProps {
  open: boolean;
  onClose: () => void;
}

export function ConversationShortcutsHelp({ open, onClose }: ConversationShortcutsHelpProps) {
  const [query, setQuery] = useState("");
  const needle = query.trim().toLowerCase();

  useEffect(() => {
    if (open) setQuery("");
  }, [open]);

  const sections = useMemo(() => {
    if (!needle) return SHORTCUT_SECTIONS;
    return SHORTCUT_SECTIONS
      .map((section) => ({ ...section, rows: section.rows.filter((row) => rowHaystack(section, row).includes(needle)) }))
      .filter((section) => section.rows.length);
  }, [needle]);

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => { if (!next) onClose(); }}
      size="lg"
      icon="keyboard"
      title="键盘快捷键"
      description="组合键在 macOS 上显示为 ⌘ / ⌥，其他系统为 Ctrl / Alt。"
      testId="conversation-shortcuts-help"
      bodyClassName="pt-1"
    >
      <div className="flex flex-col gap-4 pb-3">
        <SearchInput
          value={query}
          onValueChange={setQuery}
          placeholder="搜索快捷键或操作…"
          aria-label="搜索快捷键"
          data-autofocus
          size="md"
        />
        {sections.length ? (
          <div className="grid grid-cols-1 gap-x-8 gap-y-5 sm:grid-cols-2">
            {sections.map((section) => (
              <section key={section.title} aria-label={section.title} className={section.wide ? "sm:col-span-2" : undefined}>
                <h3 className="mb-1 text-[12px] font-semibold text-cx-fg-3">{section.title}</h3>
                <dl className="flex flex-col">
                  {section.rows.map((row) => (
                    row.label ? (
                      <div
                        key={row.label}
                        className="flex items-start gap-3 border-b border-cx-border-subtle py-2 last:border-b-0"
                      >
                        <dt className="w-[132px] shrink-0 pt-px">
                          <RowKeys row={row} />
                        </dt>
                        <dd className="min-w-0 flex-1 text-[13px] leading-5 text-cx-fg-2">{row.description}</dd>
                      </div>
                    ) : (
                      <div
                        key={`${row.description}-${(row.keys ?? []).join(",")}`}
                        className="flex min-h-8 items-center justify-between gap-4 border-b border-cx-border-subtle py-1 last:border-b-0"
                      >
                        <dt className="min-w-0 text-[13px] leading-5 text-cx-fg-2">{row.description}</dt>
                        <dd className="shrink-0">
                          <RowKeys row={row} />
                        </dd>
                      </div>
                    )
                  ))}
                </dl>
              </section>
            ))}
          </div>
        ) : (
          <EmptyState compact icon="search" title="没有匹配的快捷键" description={`未找到与 “${query.trim()}” 相关的操作。`} />
        )}
      </div>
    </Dialog>
  );
}
