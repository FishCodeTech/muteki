"use client";

import { AnimatePresence, motion } from "motion/react";
import { useDesktopChrome } from "@/components/DesktopConnection";
import { cn } from "@/lib/cn";
import { useChatPreferences, type SendKey } from "@/lib/chatPreferences";
import { Icon } from "@muteki/ui";
import { IconButton, SPRING_SWAP, Tooltip, useReducedMotion } from "@muteki/ui";

export interface SubmitClusterProps {
  running: boolean;
  busy: boolean;
  hasDraft: boolean;
  canSend: boolean;
  sendKey?: SendKey;
  submitLabel?: string;
  submitDescriptionId?: string;
  onSubmit: () => void;
  onStop?: () => void;
}

const ROUND = "relative grid size-8 shrink-0 place-items-center rounded-full outline-none focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--cx-focus)]";

export function SubmitCluster({
  running,
  busy,
  hasDraft,
  canSend,
  sendKey,
  submitLabel,
  submitDescriptionId,
  onSubmit,
  onStop,
}: SubmitClusterProps) {
  const reduced = useReducedMotion();
  const desktop = useDesktopChrome();
  const disableScaleTap = reduced || Boolean(desktop?.native);
  const chatSendKey = useChatPreferences().sendKey;
  const sendShortcut = (sendKey ?? chatSendKey) === "mod-enter" ? "mod+enter" : "enter";
  const showStop = running && !hasDraft;
  const swap = reduced
    ? { initial: { opacity: 0 }, animate: { opacity: 1 }, exit: { opacity: 0, transition: { duration: 0.06 } } }
    : {
      initial: { opacity: 0, transform: "scale(0.94)" },
      animate: { opacity: 1, transform: "scale(1)", transition: SPRING_SWAP },
      exit: { opacity: 0, transform: "scale(0.94)", transition: { duration: 0.1 } },
    };

  return (
    <div className="flex shrink-0 items-center gap-1">
      {running && hasDraft ? (
        <IconButton icon="stopCircle" label="停止执行" data-kind="stop" size="md"
          disabled={busy || !onStop} onClick={onStop} className="rounded-full" />
      ) : null}
      <div className="cx-composer-send-slot grid size-8 place-items-center [&>*]:[grid-area:1/1]">
        <AnimatePresence initial={false}>
          {showStop ? (
            <Tooltip key="stop" content="停止执行">
              <motion.button
                type="button"
                aria-label="停止执行"
                data-kind="stop"
                data-variant="primary"
                disabled={busy || !onStop}
                onClick={onStop}
                {...swap}
                whileTap={disableScaleTap ? undefined : { transform: "scale(0.96)" }}
                className={cn("cx-press", ROUND, "bg-cx-fg text-cx-bg hover:opacity-85 disabled:opacity-40")}
              >
                <Icon name="stop" size={11} filled />
              </motion.button>
            </Tooltip>
          ) : (
            <Tooltip key="send" content={submitLabel || (running ? "加入后续队列" : "发送")} shortcut={sendShortcut}>
              <motion.button
                type="button"
                aria-label={submitLabel || (running ? "加入后续消息" : "发送消息")}
                aria-describedby={submitDescriptionId}
                data-tooltip={running ? submitLabel || "加入后续消息" : undefined}
                data-kind="send"
                data-variant="primary"
                disabled={!canSend}
                onClick={onSubmit}
                {...swap}
                whileTap={disableScaleTap || !canSend ? undefined : { transform: "scale(0.96)" }}
                className={cn(
                  "cx-press",
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
