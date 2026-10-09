"use client";

import React, { useCallback, useEffect, useRef, useState } from "react";
import { AnimatePresence, motion } from "motion/react";
import { cn } from "@/lib/cn";
import { Icon } from "../Icon";
import { Button, IconButton, SPRING_LAYOUT, useReducedMotion } from "@/components/chat/ui";
import type { ConversationQueueItem } from "@/lib/useConversation";
import { ContextNodeChip } from "./ComposerPromptDocument";
import { normalizeContextNode } from "@/lib/composerContextDoc";
import { loadQueueEditDraft, saveQueueEditDraft } from "@/lib/queueEditDraftStore";
import { conversationStorageScope } from "@/lib/conversationStorageScope";
import {
  beginQueueEditSave,
  shouldExitQueueEditAfterSave,
} from "@/lib/queueEditSave";

export interface ConversationQueueProps {
  threadId: string;
  items: ConversationQueueItem[];
  paused?: boolean;
  pauseReason?: string;
  busy?: boolean;
  canSteer?: boolean;
  runningTurnId?: string | null;
  /** Awaitable: resolve `true` only when the queue item was updated successfully. */
  onUpdate: (queueId: string, text: string) => boolean | Promise<boolean>;
  onDelete: (queueId: string) => void;
  onRebind?: (queueId: string) => void;
  onReorder: (queueIds: string[]) => void;
  onPause: () => boolean | Promise<boolean>;
  onRestoreAsDraft?: (text: string) => void;
  onResume: () => void;
  onSteer: (queueId: string) => void;
  onJumpToContext?: (payload: {
    messageId: string;
    startOffset?: number;
    endOffset?: number;
  }) => void;
}

function pauseLabel(reason: string): string {
  if (reason === "turn_failed") return "上一轮执行失败，后续消息已暂停";
  if (reason === "turn_interrupted") return "上一轮被中断，后续消息已暂停";
  if (reason === "dispatch_failed") return "队首消息发送失败，队列已暂停";
  if (reason === "process_restart_with_active_turn") return "服务重启后需要确认是否继续发送";
  if (reason === "thread_archived") return "对话已归档，后续消息已暂停";
  return "后续消息已暂停";
}

export function ConversationQueue({
  threadId,
  items,
  paused = false,
  pauseReason = "",
  busy = false,
  canSteer = false,
  runningTurnId,
  onUpdate,
  onDelete,
  onRebind,
  onReorder,
  onPause,
  onRestoreAsDraft,
  onResume,
  onSteer,
  onJumpToContext,
}: ConversationQueueProps) {
  const [editingId, setEditingId] = useState("");
  const [draft, setDraft] = useState("");
  const [saving, setSaving] = useState(false);
  const [preparingEdit, setPreparingEdit] = useState(false);
  const [editError, setEditError] = useState("");
  const editScope = `${conversationStorageScope()}::${threadId}`;
  const editScopeRef = useRef(editScope);
  editScopeRef.current = editScope;
  const itemsRef = useRef(items);
  itemsRef.current = items;
  const [draggingId, setDraggingId] = useState("");
  const savingRef = useRef(false);
  const editButtonRefs = useRef(new Map<string, HTMLButtonElement | null>());
  const pendingFocusIdRef = useRef("");
  const reduced = useReducedMotion();

  useEffect(() => {
    const saved = loadQueueEditDraft(threadId);
    setEditingId(saved?.queueId || ""); setDraft(saved?.text || "");
    setEditError(saved ? "已恢复尚未提交的队列编辑；队列确认暂停后才能继续保存" : "");
    setPreparingEdit(false); setSaving(false); savingRef.current = false;
  }, [threadId, editScope]);

  const startEdit = useCallback(async (item: ConversationQueueItem) => {
    if (busy || preparingEdit || item.status === "dispatching") return;
    const owner = editScope;
    const ownerScope = conversationStorageScope();
    setPreparingEdit(true); setEditError("");
    try {
      if (!paused && await onPause() !== true) { setEditError("暂停尚未确认，未开放编辑；队列仍可能按原正文派发"); return; }
      if (editScopeRef.current !== owner || ownerScope !== conversationStorageScope()) return;
      const latest = itemsRef.current.find((row) => row.queue_id === item.queue_id);
      if (!latest || !["queued", "failed"].includes(latest.status)) { setEditError("原队列消息已开始派发，不能再编辑；请检查当前轮次"); return; }
      const saved = loadQueueEditDraft(threadId);
      const text = saved?.queueId === item.queue_id ? saved.text : latest.text;
      setDraft(text); setEditingId(item.queue_id);
      const result = saveQueueEditDraft(threadId, { queueId: item.queue_id, text, updatedAt: Date.now() });
      if (!result.persisted) setEditError(result.error || "编辑仅保留在当前窗口");
    } catch (error) { if (editScopeRef.current === owner) setEditError(error instanceof Error ? error.message : String(error)); }
    finally { if (editScopeRef.current === owner) setPreparingEdit(false); }
  }, [busy, onPause, paused, preparingEdit, threadId, editScope]);

  // Edit button unmounts while editing; restore focus after it remounts on cancel.
  useEffect(() => {
    if (editingId) return;
    const id = pendingFocusIdRef.current;
    if (!id) return;
    pendingFocusIdRef.current = "";
    const button = editButtonRefs.current.get(id);
    try {
      button?.focus();
    } catch {
      /* ignore focus failures */
    }
  }, [editingId]);

  const cancelEdit = useCallback(() => {
    if (savingRef.current) return;
    const id = editingId;
    if (id) pendingFocusIdRef.current = id;
    saveQueueEditDraft(threadId, null);
    setEditingId("");
    setDraft("");
    setEditError("");
  }, [editingId, threadId]);

  const saveEdit = useCallback(
    async (queueId: string) => {
      const current = itemsRef.current.find((item) => item.queue_id === queueId);
      if (!paused || !current || !["queued", "failed"].includes(current.status)) { setEditError("原消息已开始派发或队列未暂停，修改仍保留为未提交草稿"); return; }
      const owner = editScope;
      const ownerScope = conversationStorageScope();
      const start = beginQueueEditSave({ saving: savingRef.current, draft });
      if (!start.ok) return;
      savingRef.current = true;
      setSaving(true);
      try {
        const result = await onUpdate(queueId, start.text);
        if (editScopeRef.current !== owner || ownerScope !== conversationStorageScope()) return;
        if (shouldExitQueueEditAfterSave(result)) {
          saveQueueEditDraft(threadId, null);
          setEditingId("");
          setDraft("");
        }
        // Failure / throw: keep editingId + draft so the user can retry in place.
      } catch {
        /* Shell surfaces the error; draft stays for in-place retry. */
      } finally {
        if (editScopeRef.current === owner && ownerScope === conversationStorageScope()) { savingRef.current = false; setSaving(false); }
      }
    },
    [draft, onUpdate, paused, threadId, editScope],
  );

  if (!items.length && !editingId && !editError) return null;
  const reorderLocked = items.some((item) => item.status === "dispatching");

  const move = (queueId: string, offset: -1 | 1) => {
    const index = items.findIndex((item) => item.queue_id === queueId);
    const target = index + offset;
    if (index < 0 || target < 0 || target >= items.length) return;
    const next = items.map((item) => item.queue_id);
    [next[index], next[target]] = [next[target], next[index]];
    onReorder(next);
  };

  const dropBefore = (targetId: string) => {
    if (!draggingId || draggingId === targetId) return;
    const next = items.map((item) => item.queue_id);
    const from = next.indexOf(draggingId);
    const to = next.indexOf(targetId);
    if (from < 0 || to < 0) return;
    next.splice(from, 1);
    next.splice(to, 0, draggingId);
    setDraggingId("");
    onReorder(next);
  };

  return (
    <section
      className="cx-animate-in mx-2 mb-[-1px] overflow-hidden rounded-t-2xl border border-b-0 border-cx-border bg-cx-bg-subtle"
      aria-label={`后续消息队列，共 ${items.length} 条`}
    >
      <header className="flex h-9 items-center gap-2 pl-3 pr-1.5">
        <Icon name="rows" size={13} className="shrink-0 text-cx-fg-4" />
        <h2 className="shrink-0 text-[13px] font-medium text-cx-fg-2">排队 {items.length} 条</h2>
        <p className={cn("min-w-0 flex-1 truncate text-[12px]", paused ? "text-cx-warning" : "text-cx-fg-4")}>
          {paused ? pauseLabel(pauseReason) : "当前回答结束后按顺序自动发送"}
        </p>
        {paused ? (
          <Button size="xs" variant="primary" icon="play" disabled={busy || Boolean(editingId) || preparingEdit} onClick={onResume}>
            继续发送
          </Button>
        ) : (
          <IconButton size="xs" icon="pause" label="暂停自动发送后续消息" disabled={busy} onClick={onPause} />
        )}
      </header>

      {editError ? <p role="alert" className="mx-3 mb-2 whitespace-pre-wrap break-words text-xs text-cx-warning">{editError}</p> : null}
      {editingId && !items.some((item) => item.queue_id === editingId) ? <div className="mx-3 mb-3 space-y-2"><p className="text-xs text-cx-warning">原队列消息已经派发或移除，下面的修改尚未发送。</p><textarea aria-label="未提交的队列修改" readOnly value={draft} className="w-full rounded border border-cx-border bg-cx-elevated p-2 text-sm" />{onRestoreAsDraft ? <Button size="xs" onClick={() => { onRestoreAsDraft(draft); cancelEdit(); }}>放入当前输入框</Button> : null}<Button size="xs" variant="ghost" onClick={cancelEdit}>放弃这份修改</Button></div> : null}
      <ol className="m-0 flex list-none flex-col p-0 pb-1">
        <AnimatePresence initial={false}>
          {items.map((item, index) => {
            const editing = editingId === item.queue_id;
            const fixed = item.status === "dispatching";
            const steerAllowed = Boolean(
              canSteer
              && runningTurnId
              && !fixed
              && !item.attachments.length
              && !item.capability_refs.length,
            );
            return (
              <motion.li
                key={item.queue_id}
                layout={!reduced}
                transition={SPRING_LAYOUT}
                initial={reduced ? { opacity: 0 } : { opacity: 0, height: 0 }}
                animate={reduced ? { opacity: 1 } : { opacity: 1, height: "auto" }}
                exit={reduced ? { opacity: 0 } : { opacity: 0, height: 0, transition: { duration: 0.16 } }}
                draggable={!busy && !reorderLocked && !fixed && !editing}
                onDragStart={() => setDraggingId(item.queue_id)}
                onDragEnd={() => setDraggingId("")}
                onDragOver={(event) => {
                  if (!reorderLocked) event.preventDefault();
                }}
                onDrop={() => {
                  if (!reorderLocked) dropBefore(item.queue_id);
                }}
                className={cn(
                  "group mx-1 flex items-start gap-1.5 rounded-lg px-1.5 py-1.5 transition-colors",
                  draggingId === item.queue_id ? "bg-cx-selected opacity-60" : "hover:bg-cx-hover",
                )}
              >
                <span
                  className={cn(
                    "mt-0.5 grid size-5 shrink-0 place-items-center rounded-md text-cx-fg-4",
                    !fixed && !reorderLocked && "cursor-grab",
                  )}
                  aria-hidden="true"
                >
                  <Icon name="gripVertical" size={12} className="hidden group-hover:block" />
                  <span className="cx-tabular text-[12px] group-hover:hidden">{index + 1}</span>
                </span>

                <div className="min-w-0 flex-1">
                  {editing ? (
                    <textarea
                      aria-label="编辑后续消息"
                      rows={2}
                      autoFocus
                      value={draft}
                      disabled={saving || !paused || fixed}
                      onChange={(event) => { const text = event.target.value; setDraft(text); const result = saveQueueEditDraft(threadId, { queueId: item.queue_id, text, updatedAt: Date.now() }); if (!result.persisted) setEditError(result.error || "编辑保存失败"); }}
                      onKeyDown={(event) => {
                        if (event.key === "Enter" && !event.shiftKey && !event.nativeEvent.isComposing) {
                          event.preventDefault();
                          void saveEdit(item.queue_id);
                        } else if (event.key === "Escape") {
                          event.preventDefault();
                          event.stopPropagation();
                          cancelEdit();
                        }
                      }}
                      className="w-full resize-none rounded-lg border border-cx-border-strong bg-cx-elevated px-2 py-1.5 text-[13px] leading-5 text-cx-fg outline-none disabled:opacity-60"
                    />
                  ) : (
                    <p className="line-clamp-2 whitespace-pre-wrap break-words pt-0.5 text-[13px] leading-5 text-cx-fg-2">
                      {item.text}
                    </p>
                  )}
                  {(fixed || item.status === "failed" || item.attachments.length || item.capability_refs.length) ? (
                    <div className="mt-1 flex flex-wrap items-center gap-1.5 text-[12px] text-cx-fg-4">
                      {fixed ? <span className="text-cx-accent">正在发送…</span> : null}
                      {item.status === "failed" ? (
                        <span role="status" className="basis-full break-words text-cx-danger">
                          发送失败：{String(item.error?.message || item.error?.detail || item.error?.code || "未知错误，请重试或重新选择接入点")}
                        </span>
                      ) : null}
                      {item.status === "failed" && onRebind ? (
                        <Button size="xs" variant="secondary" disabled={busy} onClick={() => onRebind(item.queue_id)}>
                          按当前模型重新绑定
                        </Button>
                      ) : null}
                      {item.attachments.length ? (
                        <span className="inline-flex items-center gap-1"><Icon name="paperclip" size={11} />{item.attachments.length}</span>
                      ) : null}
                      {item.capability_refs.map((raw, refIndex) => {
                        const node = normalizeContextNode(raw);
                        if (!node) {
                          const name = String((raw as { name?: string }).name || `引用 ${refIndex + 1}`);
                          return <span key={refIndex} className="rounded-md bg-cx-hover px-1.5 text-cx-fg-3">@{name}</span>;
                        }
                        return (
                          <ContextNodeChip
                            key={node.node_id}
                            node={node}
                            onJump={
                              node.kind === "message_span" && node.locator.message_id
                                ? () => onJumpToContext?.({
                                  messageId: String(node.locator.message_id),
                                  startOffset: node.locator.start_offset,
                                  endOffset: node.locator.end_offset,
                                })
                                : undefined
                            }
                          />
                        );
                      })}
                    </div>
                  ) : null}
                </div>

                <div className={cn("flex shrink-0 items-center gap-0.5", !editing && "opacity-0 transition-opacity group-hover:opacity-100 group-focus-within:opacity-100 [@media(hover:none)]:opacity-100")}>
                  {editing ? (
                    <>
                      <IconButton
                        size="xs"
                        icon="check"
                        label="保存队列消息"
                        disabled={busy || saving || fixed || !paused || !draft.trim()}
                        loading={saving}
                        className="text-cx-accent"
                        onClick={() => {
                          void saveEdit(item.queue_id);
                        }}
                      />
                      <IconButton
                        size="xs"
                        icon="x"
                        label="取消编辑"
                        disabled={saving}
                        onClick={cancelEdit}
                      />
                    </>
                  ) : (
                    <>
                      {steerAllowed ? (
                        <Button size="xs" variant="soft" icon="target" disabled={busy} onClick={() => onSteer(item.queue_id)} aria-label="将这条消息用于引导当前回答">
                          引导
                        </Button>
                      ) : null}
                      <IconButton size="xs" icon="arrowUp" label="上移" disabled={busy || reorderLocked || fixed || index === 0} onClick={() => move(item.queue_id, -1)} />
                      <IconButton size="xs" icon="arrowDown" label="下移" disabled={busy || reorderLocked || fixed || index === items.length - 1} onClick={() => move(item.queue_id, 1)} />
                      <IconButton
                        size="xs"
                        icon="pencil"
                        label="编辑"
                        disabled={busy || fixed || preparingEdit}
                        ref={(node) => {
                          editButtonRefs.current.set(
                            item.queue_id,
                            node as HTMLButtonElement | null,
                          );
                        }}
                        onClick={() => { void startEdit(item); }}
                      />
                      <IconButton size="xs" icon="trash" label="删除" disabled={busy || fixed} className="hover:text-cx-danger" onClick={() => onDelete(item.queue_id)} />
                    </>
                  )}
                </div>
              </motion.li>
            );
          })}
        </AnimatePresence>
      </ol>
    </section>
  );
}
