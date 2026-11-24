
export const SUCCESS_NOTICE_DISMISS_MS = 2800;

export type ThreadNoticeKind = "success" | "progress" | "sticky" | "warning" | "accepted";

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
      clearNotice();
    }, delay);
  };

  return {
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
