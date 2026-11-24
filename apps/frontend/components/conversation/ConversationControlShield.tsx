"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { Icon } from "@muteki/ui";
import { ConversationRouteLink } from "./ConversationNavigation";
import {
  Badge,
  Button,
  IconButton,
  Popover,
  Spinner,
  toast,
} from "@muteki/ui";
import {
  cancelSelfSettle,
  controlErrorText,
  dismissSettleSuggestion,
  fetchControlThread,
  settleControlThread,
  removeThreadReference,
  type ControlThreadGrant,
} from "@/lib/controlGrants";

const SETTLE_WHEN_LABELS: Record<string, string> = {
  turn_end: "当前轮次结束后自动结案",
  on_turn_end: "当前轮次结束后自动结案",
  pr_merged: "关联 PR 合并后自动结案",
  on_pr_merged: "关联 PR 合并后自动结案",
  pr_closed: "关联 PR 关闭后自动结案",
  on_pr_closed: "关联 PR 关闭后自动结案",
};

export function ConversationControlShield({
  threadId,
  watermark,
  open,
  onOpenChange: setOpen,
}: {
  threadId?: string;
  watermark?: number;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const activeThread = useRef(threadId);
  activeThread.current = threadId;
  const [state, setState] = useState<ControlThreadGrant | null>(null);
  const [loading, setLoading] = useState(false);
  const [busy, setBusy] = useState("");

  const refresh = useCallback(async () => {
    if (!threadId) return;
    setLoading(true);
    try {
      const next = await fetchControlThread(threadId);
      if (activeThread.current === threadId) setState(next);
    } catch {
    } finally {
      setLoading(false);
    }
  }, [threadId]);

  useEffect(() => {
    setState(null);
  }, [threadId]);

  useEffect(() => {
    if (!threadId) return;
    void refresh();
  }, [threadId, watermark, refresh]);

  useEffect(() => {
    if (open && threadId) {
      void refresh();
    }
  }, [open, threadId, refresh]);

  if (!threadId) return null;

  const hasSelfSettle = Boolean(state?.self_settle || (state?.settle_suggestion && !state.lifecycle?.settled));
  const hasLifecycleFlag = Boolean(
    state?.lifecycle?.settled || state?.lifecycle?.follow_up || state?.lifecycle?.snoozed_until,
  );
  const hasContent = hasLifecycleFlag
    || Boolean(state?.settle_suggestion && !state.lifecycle.settled)
    || Boolean(state?.self_settle)
    || Boolean(state?.references?.length)
    || Boolean(state?.review);
  const needsAttention = hasSelfSettle || (state?.references.length ?? 0) > 0 || Boolean(state?.review) || hasLifecycleFlag;

  const handleCancelSelfSettle = async () => {
    setBusy("cancel_settle");
    try {
      await cancelSelfSettle(threadId);
      await refresh();
      toast({ title: "已取消自动结案预约", tone: "success" });
    } catch (cause) {
      toast({ title: cause instanceof Error ? cause.message : "取消预约失败", tone: "danger" });
    } finally {
      setBusy("");
    }
  };

  const handleSettleSuggestion = async (settle: boolean) => {
    setBusy(settle ? "settle" : "dismiss");
    try {
      if (settle) await settleControlThread(threadId);
      else await dismissSettleSuggestion(threadId);
      await refresh();
      toast({ title: settle ? "线程已收尾" : "已暂不收尾这个线程", tone: "success" });
    } catch (cause) {
      toast({ title: settle ? "收尾失败" : "操作失败", description: controlErrorText(cause, "请稍后再试"), tone: "danger" });
    } finally {
      setBusy("");
    }
  };

  const handleRemoveRef = async (targetThreadId: string) => {
    setBusy(targetThreadId);
    try {
      await removeThreadReference(threadId, targetThreadId);
      await refresh();
    } catch (cause) {
      toast({ title: cause instanceof Error ? cause.message : "移除引用失败", tone: "danger" });
    } finally {
      setBusy("");
    }
  };

  if (!needsAttention && !open) return null;

  return (
    <Popover
      open={open}
      onOpenChange={setOpen}
      placement="bottom-end"
      ariaLabel="跨对话协同与状态"
      className="w-[368px] max-w-[calc(100vw-24px)] rounded-2xl border-0 bg-cx-overlay p-3.5 shadow-cx-pop"
      trigger={
        <div className="relative inline-flex">
          <IconButton
            icon="shield"
            label="跨对话编排与生命周期状态"
            active={open || hasSelfSettle}
          />
          {hasSelfSettle ? (
            <span className="pointer-events-none absolute right-1 top-1 size-2 rounded-full bg-cx-accent ring-2 ring-cx-bg" />
          ) : null}
        </div>
      }
    >
      <div className="flex flex-col gap-3">
        <div className="flex items-center justify-between gap-2">
          <div className="flex items-center gap-1.5">
            <Icon name="shield" size={14} className="text-cx-accent" />
            <span className="text-cx-sm font-semibold text-cx-fg">跨对话协同与生命周期</span>
          </div>
          <ConversationRouteLink
            href="/settings/control"
            onClick={() => setOpen(false)}
            className="inline-flex items-center gap-1 text-cx-xs font-medium text-cx-fg-3 hover:text-cx-fg"
          >
            全局设置
            <Icon name="arrowUpRight" size={12} />
          </ConversationRouteLink>
        </div>

        {!state && loading ? (
          <div className="flex items-center gap-2 py-4 text-cx-xs text-cx-fg-3">
            <Spinner size={13} />
            正在读取对话协作状态…
          </div>
        ) : state ? (
          <>
            {hasLifecycleFlag ? (
              <div className="flex flex-wrap gap-1 rounded-xl border border-cx-border-subtle bg-cx-bg-subtle/65 p-2.5">
                {state.lifecycle?.settled ? <Badge tone="success">已结案 (Settled)</Badge> : null}
                {state.lifecycle?.follow_up ? <Badge tone="warning">待跟进 (Follow-up)</Badge> : null}
                {state.lifecycle?.snoozed_until ? <Badge tone="neutral">已暂缓</Badge> : null}
              </div>
            ) : null}

            {state.settle_suggestion && !state.lifecycle.settled ? (
              <div className="flex flex-col gap-2 rounded-xl border border-cx-accent-line bg-cx-accent-soft/45 p-2.5">
                <div className="flex items-start gap-1.5">
                  <Icon name="checkCircle" size={13} className="mt-[3px] shrink-0 text-cx-accent" />
                  <div className="min-w-0">
                    <div className="text-cx-xs font-semibold text-cx-fg">可以收尾这个线程</div>
                    <div className="text-cx-xs leading-4 text-cx-fg-3">
                      它符合 设置 → 对话 → 线程收尾 中的规则。收尾后线程会结案，并停止对拉取请求的监控。
                    </div>
                  </div>
                </div>
                <div className="flex gap-1.5">
                  <Button
                    size="xs"
                    variant="primary"
                    loading={busy === "settle"}
                    disabled={busy !== ""}
                    onClick={() => void handleSettleSuggestion(true)}
                  >
                    收尾
                  </Button>
                  <Button
                    size="xs"
                    variant="ghost"
                    loading={busy === "dismiss"}
                    disabled={busy !== ""}
                    onClick={() => void handleSettleSuggestion(false)}
                  >
                    暂不收尾
                  </Button>
                </div>
              </div>
            ) : null}
            {state.self_settle ? (
              <div className="flex flex-col gap-1.5 rounded-xl border border-cx-accent-line bg-cx-accent-soft/45 p-2.5">
                <div className="flex items-center justify-between gap-2">
                  <span className="text-cx-xs font-semibold text-cx-fg">
                    已预约自动结案：{SETTLE_WHEN_LABELS[state.self_settle.when] || state.self_settle.when}
                  </span>
                  <Button
                    size="xs"
                    variant="secondary"
                    loading={busy === "cancel_settle"}
                    disabled={busy !== ""}
                    onClick={() => void handleCancelSelfSettle()}
                  >
                    取消预约
                  </Button>
                </div>
                <div className="truncate text-cx-xs text-cx-fg-3" title={state.self_settle.consent_quote}>
                  核验原话：「{state.self_settle.consent_quote}」
                </div>
              </div>
            ) : null}

            {state.references && state.references.length > 0 ? (
              <div className="flex flex-col gap-1.5">
                <div className="text-cx-xs font-medium text-cx-fg-3">
                  跨对话正式引用 ({state.references.length})
                </div>
                <div className="flex max-h-36 flex-col gap-1 overflow-y-auto pr-0.5">
                  {state.references.map((ref) => (
                    <div
                      key={ref.target_thread_id}
                      className="flex items-center justify-between gap-2 rounded-lg border border-cx-border-subtle bg-cx-bg-subtle/50 px-2.5 py-1.5"
                    >
                      <div className="min-w-0 flex-1">
                        <div className="flex items-center gap-1.5">
                          <ConversationRouteLink
                            href={`/chat/${ref.target_thread_id}`}
                            onClick={() => setOpen(false)}
                            className="truncate text-cx-xs font-medium text-cx-fg hover:text-cx-accent hover:underline"
                          >
                            @{ref.target_title || ref.target_thread_id.slice(0, 8)}
                          </ConversationRouteLink>
                          {ref.boundary_notice ? (
                            <span title={ref.boundary_notice}>
                              <Badge tone="warning">跨边界</Badge>
                            </span>
                          ) : null}
                        </div>
                        {ref.anchors.length > 0 ? (
                          <div className="truncate font-mono text-cx-2xs text-cx-fg-4">
                            {ref.anchors.join(", ")}
                          </div>
                        ) : null}
                      </div>
                      <IconButton
                        icon="x"
                        size="sm"
                        label="移除正式引用"
                        disabled={busy !== ""}
                        onClick={() => void handleRemoveRef(ref.target_thread_id)}
                      />
                    </div>
                  ))}
                </div>
              </div>
            ) : null}

            {state.review ? (
              <div className="rounded-xl border border-cx-border-subtle bg-cx-bg-subtle/40 px-2.5 py-2">
                <div className="flex items-center justify-between gap-2">
                  <span className="text-cx-xs font-semibold text-cx-fg-3">最近审阅结论</span>
                  <Badge tone="neutral">{state.review.verdict}</Badge>
                </div>
                <div className="mt-1 line-clamp-2 text-cx-xs text-cx-fg-2">
                  {state.review.summary}
                </div>
              </div>
            ) : null}

            {!hasContent ? (
              <div className="rounded-xl border border-cx-border-subtle bg-cx-bg-subtle/65 px-3 py-2.5 text-cx-xs leading-5 text-cx-fg-3">
                当前对话暂无生命周期标记、跨对话引用或审阅结论。
              </div>
            ) : null}
          </>
        ) : (
          <div className="rounded-xl border border-cx-border-subtle bg-cx-bg-subtle/65 px-3 py-2.5 text-cx-xs leading-5 text-cx-fg-3">
            暂未获取到对话协作状态。
          </div>
        )}
      </div>
    </Popover>
  );
}
