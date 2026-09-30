/**
 * Success / progress / sticky lifecycle for thread-scoped phase notices.
 * Pure module (no React) so Node self-checks can import without UI deps.
 */

/** Short toast window for success notices (approval allowed/denied, archive ok, …). */
export const SUCCESS_NOTICE_DISMISS_MS = 2800;

export type ThreadNoticeKind = "success" | "progress" | "sticky" | "warning" | "accepted";

/**
 * Separate lifecycle for the phase/success Callout channel:
 * - success → short auto-dismiss (pausable on hover/focus)
 * - progress → stay until overwritten / cleared by the pipeline
 * - sticky → stay until handled (recovery warnings, incomplete stream, …)
 *
 * Errors use the separate `error` banner and must never go through success
 * auto-clear. Untyped notices remain sticky until an explicit operation clears them.
 */
/** The initiating operation supplies the lifecycle; message language is display only. */
export function classifyThreadNotice(_text: string, kind: ThreadNoticeKind = "sticky"): ThreadNoticeKind {
  return kind;
}

export function shouldScheduleNoticeAutoClear(text: string, kind: ThreadNoticeKind = "sticky"): boolean {
  return Boolean(text) && kind === "success";
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
  let activeKind: ThreadNoticeKind = "sticky";
  let remaining = 0;
  let deadline = 0;
  let paused = false;

  const stopTimer = () => {
    token += 1;
    if (timer != null) {
      clearTimerFn(timer);
      timer = null;
    }
  };

  const arm = (text: string, delay: number) => {
    stopTimer();
    if (!shouldScheduleNoticeAutoClear(text, activeKind) || delay <= 0) {
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
    onNoticeChange(text: string, kind: ThreadNoticeKind = "sticky") {
      token += 1;
      stopTimer();
      activeKind = kind;
      paused = false;
      activeText = text;
      remaining = 0;
      if (!text) return;
      arm(text, dismissMs);
    },
    pause() {
      if (!shouldScheduleNoticeAutoClear(activeText, activeKind)) return;
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
      if (remaining > 0 && shouldScheduleNoticeAutoClear(activeText, activeKind)) {
        arm(activeText, remaining);
      }
    },
    dispose() {
      token += 1;
      stopTimer();
      activeText = "";
      activeKind = "sticky";
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
