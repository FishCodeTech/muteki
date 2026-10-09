"use client";

import type { ComposerStashEntry } from "@/lib/composerRecallStore";
import { Button, Dialog, EmptyState, IconButton, TextField } from "@/components/chat/ui";
import { Icon } from "../Icon";

export function ComposerStashModal({
  open,
  name,
  onNameChange,
  stashes,
  canSave,
  onClose,
  onSave,
  onRestore,
  onDelete,
}: {
  open: boolean;
  name: string;
  onNameChange: (value: string) => void;
  stashes: ComposerStashEntry[];
  canSave: boolean;
  onClose: () => void;
  onSave: () => void;
  onRestore: (stashId: string) => void;
  onDelete: (stashId: string) => void;
}) {
  const saveDisabled = !canSave || !name.trim();
  return (
    <Dialog
      open={open}
      onOpenChange={(next) => { if (!next) onClose(); }}
      title="草稿暂存"
      description="保存当前正文、引用和附件。恢复到其他项目时会提示重新关联，过期附件不会被丢弃。"
      icon="bookmark"
      tone="accent"
      size="md"
      testId="composer-stash-dialog"
      footer={(
        <>
          <Button variant="ghost" onClick={onClose}>关闭</Button>
          <Button variant="primary" data-testid="composer-stash-save" disabled={saveDisabled} onClick={onSave}>
            暂存当前草稿
          </Button>
        </>
      )}
    >
      <form
        onSubmit={(event) => {
          event.preventDefault();
          if (!saveDisabled) onSave();
        }}
      >
        <TextField
          label="暂存名称"
          value={name}
          onChange={(event) => onNameChange(event.target.value)}
          data-testid="composer-stash-name"
          data-autofocus
          placeholder={canSave ? "给这份草稿起个名字" : "输入框为空，无法暂存"}
        />
      </form>
      <div className="mt-4">
        <p className="mb-2 text-[12px] font-medium text-cx-fg-3">已暂存（{stashes.length}）</p>
        <ul className="m-0 flex list-none flex-col gap-1 p-0" data-testid="composer-stash-list">
          {stashes.length ? stashes.map((item) => (
            <li
              key={item.id}
              className="group flex items-center gap-3 rounded-xl border border-cx-border-subtle px-3 py-2 transition-colors hover:border-cx-border hover:bg-cx-bg-subtle"
              data-testid="composer-stash-item"
            >
              <span className="grid size-8 shrink-0 place-items-center rounded-lg bg-cx-hover text-cx-fg-3">
                <Icon name="file" size={14} />
              </span>
              <div className="min-w-0 flex-1">
                <strong className="block truncate text-[13px] font-medium text-cx-fg">{item.name}</strong>
                <span className="block truncate text-[12px] text-cx-fg-4">
                  {item.projectId ? `项目 ${item.projectId.slice(0, 12)}` : "未绑定项目"}
                  {item.snapshot.attachments.length ? ` · ${item.snapshot.attachments.length} 个附件` : ""}
                  {item.snapshot.capabilityRefs.length ? ` · ${item.snapshot.capabilityRefs.length} 处引用` : ""}
                </span>
              </div>
              <Button size="xs" variant="secondary" data-testid="composer-stash-restore" onClick={() => onRestore(item.id)}>
                恢复
              </Button>
              <IconButton
                size="xs"
                icon="trash"
                label={`删除 ${item.name}`}
                className="hover:text-cx-danger"
                onClick={() => onDelete(item.id)}
              />
            </li>
          )) : (
            <li>
              <EmptyState compact icon="bookmark" title="还没有暂存草稿" description="按 ⌘S / Ctrl+S 可快速暂存当前输入。" />
            </li>
          )}
        </ul>
      </div>
    </Dialog>
  );
}
