"use client";

import { IconButton, Menu, MenuItem, MenuLabel, MenuSeparator } from "@/components/chat/ui";

export interface ComposerAddMenuProps {
  onAddAttachment?: () => void;
  /** Inserts `/` or `@` at the caret; omitted when the capability catalog is unavailable. */
  onInsertTrigger?: (trigger: "/" | "@") => void;
  onOpenStash?: () => void;
  stashCount?: number;
}

export function ComposerAddMenu({ onAddAttachment, onInsertTrigger, onOpenStash, stashCount = 0 }: ComposerAddMenuProps) {
  if (!onAddAttachment && !onInsertTrigger && !onOpenStash) return null;
  return (
    <Menu
      placement="top-start"
      ariaLabel="添加内容"
      className="min-w-[232px]"
      trigger={(
        <IconButton
          icon="plus"
          label="添加文件、命令或上下文"
          size="md"
          tooltipPlacement="top"
          className="rounded-full text-cx-fg-2 data-[state=open]:bg-cx-active data-[state=open]:text-cx-fg"
        />
      )}
    >
      {onAddAttachment ? (
        <MenuItem icon="paperclip" hint="也可拖拽或粘贴" onSelect={onAddAttachment}>
          添加文件或图片
        </MenuItem>
      ) : null}
      {onInsertTrigger ? (
        <>
          {onAddAttachment ? <MenuSeparator /> : null}
          <MenuLabel>插入</MenuLabel>
          <MenuItem icon="slash" shortcut="/" onSelect={() => onInsertTrigger("/")}>
            命令或 Skill
          </MenuItem>
          <MenuItem icon="at" shortcut="@" onSelect={() => onInsertTrigger("@")}>
            引用文件或对话
          </MenuItem>
        </>
      ) : null}
      {onOpenStash ? (
        <>
          <MenuSeparator />
          <MenuItem
            icon="archive"
            shortcut="mod+s"
            hint={stashCount > 0 ? `${stashCount} 条` : undefined}
            onSelect={onOpenStash}
          >
            草稿暂存
          </MenuItem>
        </>
      ) : null}
    </Menu>
  );
}
