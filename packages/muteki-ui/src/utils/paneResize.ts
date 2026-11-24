const listeners = new Set<() => void>();
let active = 0;
let shield: HTMLDivElement | null = null;

function ensureShield(): HTMLDivElement | null {
  if (typeof document === "undefined") return null;
  if (!shield || !shield.isConnected) {
    shield = document.createElement("div");
    shield.setAttribute("aria-hidden", "true");
    shield.style.cssText =
      "position:fixed;inset:0;z-index:9999;cursor:col-resize;user-select:none;-webkit-user-select:none;display:none;";
    (document.querySelector(".workspace-frame") ?? document.body)?.appendChild(shield);
  }
  return shield;
}

export function paneResizing(): boolean {
  return active > 0;
}

export function setPaneResizing(on: boolean): void {
  active = Math.max(0, active + (on ? 1 : -1));
  const node = ensureShield();
  if (node) node.style.display = active > 0 ? "block" : "none";
  if (active || typeof window === "undefined") return;
  window.requestAnimationFrame(() => {
    if (!active) for (const listener of [...listeners]) listener();
  });
}

export function onPaneResizeEnd(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}
