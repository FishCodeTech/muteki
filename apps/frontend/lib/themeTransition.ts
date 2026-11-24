export const THEME_SWITCHING_CLASS = "theme-switching";

const pendingCleanups = new WeakMap<HTMLElement, () => void>();

export function runWithoutThemeTransitions(
  apply: () => void,
  root: HTMLElement = document.documentElement,
): void {
  const doc = root.ownerDocument;
  const win = doc?.defaultView;
  if (!doc || !win) {
    apply();
    return;
  }

  pendingCleanups.get(root)?.();
  root.classList.add(THEME_SWITCHING_CLASS);

  try {
    apply();
    void win.getComputedStyle(doc.body ?? root).opacity;
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
      if (pendingCleanups.get(root) === cancel) pendingCleanups.delete(root);
    };

    const finish = () => {
      if (settled) return;
      settled = true;
      cancel();
      void win.getComputedStyle(doc.body ?? root).opacity;
      root.classList.remove(THEME_SWITCHING_CLASS);
    };

    pendingCleanups.set(root, cancel);

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
      timer = win.setTimeout(finish, 250);
    }
  }
}
