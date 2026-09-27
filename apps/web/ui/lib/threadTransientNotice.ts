/**
 * Success / progress / sticky lifecycle for thread-scoped phase notices.
 * Pure module (no React) so Node self-checks can import without UI deps.
 */

/** Short toast window for success notices (approval allowed/denied, archive ok, …). */
export const SUCCESS_NOTICE_DISMISS_MS = 2800;

export type ThreadNoticeKind = "success" | "progress" | "sticky";

/**
 * Separate lifecycle for the phase/success Callout channel:
 * - success → short auto-dismiss (pausable on hover/focus)
 * - progress → stay until overwritten / cleared by the pipeline
 * - sticky → stay until handled (recovery warnings, incomplete stream, …)
 *
 * Errors use the separate `error` banner and must never go through success
 * auto-clear; classify anything failure-like in this channel as sticky.
 */
export function classifyThreadNotice(text: string): ThreadNoticeKind {
  const t = text.trim();
  if (!t) return "sticky";

  if (
    t.includes("正在")
    || t.includes("核对回执")
    || t.includes("消息已受理")
    || t.includes("Agent 仍可能在执行中")
  ) {
    return "progress";
  }

  if (
    t.includes("失败")
    || t.includes("无法")
    || t.includes("不可用")
    || t.includes("需重新")
    || t.includes("需确认")
    || t.includes("可能不完整")
    || t.includes("没有可归档")
    || t.includes("不能发送")
  ) {
    return "sticky";
  }

  return "success";
}

export function shouldScheduleNoticeAutoClear(text: string): boolean {
  return classifyThreadNotice(text) === "success";
}

export type NoticeAutoDismissTimers = {
  now?: () => number;
  setTimer?: (fn: () => void, ms: number) => ReturnType<typeof setTimeout>;
  clearTimer?: (id: ReturnType<typeof setTimeout>) => void;
};

/**
 * Pure-ish controller: schedules clear for success notices only.
 * Errors / progress / sticky never schedule. Consecutive success replaces
 * the prior timer (no stacking). Pause/resume keeps remaining time.
 */
export function createNoticeAutoDismissController(
  clearNotice: () => void,
  opts: NoticeAutoDismissTimers & { dismissMs?: number } = {},
) {
  const dismissMs = opts.dismissMs ?? SUCCESS_NOTICE_DISMISS_MS;
  const now = opts.now ?? (() => Date.now());
  const setTimer = opts.setTimer ?? ((fn, ms) => setTimeout(fn, ms));
  const clearTimerFn = opts.clearTimer ?? ((id) => clearTimeout(id));

  let timer: ReturnType<typeof setTimeout> | null = null;
  let token = 0;
  let activeText = "";
  let remaining = 0;
  let deadline = 0;
  let paused = false;

  const stopTimer = () => {
    if (timer != null) {
      clearTimerFn(timer);
      timer = null;
    }
  };

  const arm = (text: string, delay: number) => {
    stopTimer();
    if (!shouldScheduleNoticeAutoClear(text) || delay <= 0) {
      remaining = 0;
      return;
    }
    const myToken = ++token;
    activeText = text;
    remaining = delay;
    deadline = now() + delay;
    timer = setTimer(() => {
      if (myToken !== token) return;
      timer = null;
      remaining = 0;
      activeText = "";
      // clearNotice only drops auto-dismissable success copy still in the store.
      clearNotice();
    }, delay);
  };

  return {
    /** Call whenever the scoped notice string changes (including ""). */
    onNoticeChange(text: string) {
      stopTimer();
      paused = false;
      activeText = text;
      remaining = 0;
      if (!text) return;
      arm(text, dismissMs);
    },
    pause() {
      if (!shouldScheduleNoticeAutoClear(activeText)) return;
      if (paused) return;
      paused = true;
      if (timer != null) {
        remaining = Math.max(0, deadline - now());
        stopTimer();
      }
    },
    resume() {
      if (!paused) return;
      paused = false;
      if (remaining > 0 && shouldScheduleNoticeAutoClear(activeText)) {
        arm(activeText, remaining);
      }
    },
    dispose() {
      token += 1;
      stopTimer();
      activeText = "";
      remaining = 0;
      paused = false;
    },
    /** Test helpers */
    getRemainingMs() {
      if (paused) return remaining;
      if (timer == null) return 0;
      return Math.max(0, deadline - now());
    },
    isPaused() {
      return paused;
    },
  };
}
