const pendingCleanups = new WeakMap<HTMLElement, () => void>();

export function runInstantTransition(
  apply: () => void,
  target?: HTMLElement | null,
): void {
  const el = target ?? (typeof document !== "undefined" ? document.documentElement : null);
  const doc = el?.ownerDocument;
  const win = doc?.defaultView;
  if (!el || !doc || !win) {
    apply();
    return;
  }

  pendingCleanups.get(el)?.();
  el.dataset.instant = "true";

  try {
    apply();
    void el.offsetWidth;
  } finally {
    let raf1 = 0;
    let raf2 = 0;
    let timer = 0;
    let settled = false;

    const cancel = () => {
      if (raf1 && typeof win.cancelAnimationFrame === "function") win.cancelAnimationFrame(raf1);
      if (raf2 && typeof win.cancelAnimationFrame === "function") win.cancelAnimationFrame(raf2);
      if (timer) win.clearTimeout(timer);
      raf1 = 0;
      raf2 = 0;
      timer = 0;
      if (pendingCleanups.get(el) === cancel) pendingCleanups.delete(el);
    };

    const finish = () => {
      if (settled) return;
      settled = true;
      cancel();
      void el.offsetWidth;
      delete el.dataset.instant;
    };

    pendingCleanups.set(el, cancel);

    if (doc.visibilityState === "hidden" || typeof win.requestAnimationFrame !== "function") {
      timer = win.setTimeout(finish, 0);
    } else {
      raf1 = win.requestAnimationFrame(() => {
        raf1 = 0;
        raf2 = win.requestAnimationFrame(() => {
          raf2 = 0;
          finish();
        });
      });
      timer = win.setTimeout(finish, 120);
    }
  }
}
