"use client";

import { AnimatePresence, motion } from "motion/react";
import { cn } from "@/lib/cn";
import { useChatPreferences } from "@/lib/chatPreferences";
import { Icon } from "@/components/Icon";
import { IconButton, SPRING_SWAP, Tooltip, useReducedMotion } from "@/components/chat/ui";

export interface SubmitClusterProps {
  running: boolean;
  busy: boolean;
  /** Draft has text, refs or attachments. */
  hasDraft: boolean;
  canSend: boolean;
  canSteer: boolean;
  /** The send button and send key steer the current answer; the secondary button queues instead. */
  steerPrimary?: boolean;
  steerDisabledReason: string;
  steerAlternative: string;
  onSubmit: () => void;
  onSteer?: () => void;
  onStop?: () => void;
}

const ROUND = "relative grid size-8 shrink-0 place-items-center rounded-full outline-none focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--cx-focus)]";

export function SubmitCluster({
  running,
  busy,
  hasDraft,
  canSend,
  canSteer,
  steerPrimary = false,
  steerDisabledReason,
  steerAlternative,
  onSubmit,
  onSteer,
  onStop,
}: SubmitClusterProps) {
  const primarySteers = Boolean(running && steerPrimary && canSteer && onSteer);
  const reduced = useReducedMotion();
  const sendShortcut = useChatPreferences().sendKey === "mod-enter" ? "mod+enter" : "enter";
  const showStop = running && !hasDraft;
  const swap = reduced
    ? { initial: { opacity: 0 }, animate: { opacity: 1 }, exit: { opacity: 0, transition: { duration: 0.06 } } }
    : {
      initial: { opacity: 0, scale: 0.55, rotate: -30 },
      animate: { opacity: 1, scale: 1, rotate: 0, transition: SPRING_SWAP },
      exit: { opacity: 0, scale: 0.55, rotate: 30, transition: { duration: 0.1 } },
    };
  const steerBlockedHint = [steerDisabledReason, steerAlternative].filter(Boolean).join("；") || "当前不支持引导";

  return (
    <div className="flex shrink-0 items-center gap-1">
      <AnimatePresence initial={false}>
        {running && hasDraft ? (
          <motion.div
            key="running-extras"
            className="flex items-center gap-1"
            initial={reduced ? { opacity: 0 } : { opacity: 0, x: 6 }}
            animate={{ opacity: 1, x: 0, transition: { duration: 0.16 } }}
            exit={{ opacity: 0, transition: { duration: 0.1 } }}
          >
            <IconButton
              icon="stopCircle"
              label="停止执行"
              data-kind="stop"
              size="md"
              disabled={busy || !onStop}
              onClick={onStop}
              className="rounded-full"
            />
            {primarySteers ? (
              <Tooltip content="加入后续队列（不引导当前回答）">
                <button
                  type="button"
                  aria-label="加入后续消息"
                  data-tooltip="加入后续消息"
                  disabled={!canSend}
                  onClick={onSubmit}
                  className={cn(
                    "cx-press inline-flex h-8 items-center gap-1.5 rounded-full bg-cx-accent-soft px-3 text-[13px] font-medium text-cx-accent",
                    "hover:bg-[color-mix(in_srgb,var(--accent)_18%,transparent)] disabled:pointer-events-none disabled:opacity-45",
                    "outline-none focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--cx-focus)]",
                  )}
                >
                  排队
                </button>
              </Tooltip>
            ) : canSteer && onSteer ? (
              <Tooltip content="立即引导当前回答（不排队）">
                <button
                  type="button"
                  aria-label="引导当前回答"
                  data-tooltip="引导当前回答"
                  disabled={!canSend}
                  onClick={onSteer}
                  className={cn(
                    "cx-press inline-flex h-8 items-center gap-1.5 rounded-full bg-cx-accent-soft px-3 text-[13px] font-medium text-cx-accent",
                    "hover:bg-[color-mix(in_srgb,var(--accent)_18%,transparent)] disabled:pointer-events-none disabled:opacity-45",
                    "outline-none focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--cx-focus)]",
                  )}
                >
                  引导
                </button>
              </Tooltip>
            ) : (
              <Tooltip content={steerBlockedHint}>
                <button
                  type="button"
                  aria-label="当前不支持引导"
                  aria-disabled="true"
                  data-tooltip={steerBlockedHint}
                  data-capability-key="steer"
                  data-support-level="unsupported"
                  onClick={(event) => event.preventDefault()}
                  className="inline-flex h-8 cursor-not-allowed items-center gap-1.5 rounded-full px-3 text-[13px] font-medium text-cx-fg-4"
                >
                  引导
                </button>
              </Tooltip>
            )}
          </motion.div>
        ) : null}
      </AnimatePresence>
      <div className="grid size-8 place-items-center [&>*]:[grid-area:1/1]">
        <AnimatePresence initial={false}>
          {showStop ? (
            <Tooltip key="stop" content="停止执行">
              <motion.button
                type="button"
                aria-label="停止执行"
                data-kind="stop"
                disabled={busy || !onStop}
                onClick={onStop}
                {...swap}
                whileTap={reduced ? undefined : { scale: 0.92 }}
                className={cn(ROUND, "bg-cx-fg text-cx-bg hover:opacity-85 disabled:opacity-40")}
              >
                <Icon name="stop" size={11} filled />
              </motion.button>
            </Tooltip>
          ) : (
            <Tooltip key="send" content={primarySteers ? "引导当前回答" : running ? "加入后续队列" : "发送"} shortcut={sendShortcut}>
              <motion.button
                type="button"
                aria-label={primarySteers ? "引导当前回答" : running ? "加入后续消息" : "发送消息"}
                data-tooltip={primarySteers ? "引导当前回答" : running ? "加入后续消息" : undefined}
                data-kind={primarySteers ? "steer" : "send"}
                disabled={!canSend}
                onClick={primarySteers ? onSteer : onSubmit}
                {...swap}
                whileTap={reduced || !canSend ? undefined : { scale: 0.92 }}
                className={cn(
                  ROUND,
                  "transition-colors duration-150",
                  canSend
                    ? "bg-cx-accent text-cx-accent-fg shadow-[inset_0_1px_0_color-mix(in_srgb,white_18%,transparent)] hover:bg-[color-mix(in_srgb,var(--accent)_88%,var(--ink))]"
                    : "cursor-not-allowed bg-cx-active text-cx-fg-4",
                )}
              >
                <Icon name="arrowUp" size={16} />
              </motion.button>
            </Tooltip>
          )}
        </AnimatePresence>
      </div>
    </div>
  );
}
