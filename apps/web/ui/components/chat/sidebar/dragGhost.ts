/** Floating copy of a sidebar row (or a whole folder) that follows the pointer while reordering. */
export interface DragGhost {
  move: (x: number, y: number) => void;
  /** Fade out after a successful drop. */
  drop: () => void;
  /** Glide back to the source position (or just fade) when the drag ends without a move. */
  cancel: () => void;
}

const GHOST_MAX_HEIGHT = 216;
const GHOST_CLASS = [
  "pointer-events-none fixed left-0 top-0 z-[1200] overflow-hidden rounded-[10px]",
  "bg-cx-overlay/90 shadow-cx-pop backdrop-blur-md",
].join(" ");

function portalHost(): HTMLElement {
  let layer = document.getElementById("cx-portal-root");
  if (!layer) {
    layer = document.createElement("div");
    layer.id = "cx-portal-root";
    layer.className = "cx-portal";
    document.body.appendChild(layer);
  }
  return layer;
}

/** Drop ids, sidebar hooks and ARIA wiring so the copy is never mistaken for a real row. */
function scrub(root: HTMLElement): void {
  for (const node of [root, ...Array.from(root.querySelectorAll<HTMLElement>("*"))]) {
    for (const name of node.getAttributeNames()) {
      if (
        name === "id" || name === "tabindex" || name === "data-menu-open"
        || name.startsWith("aria-") || name.startsWith("data-sidebar") || name.startsWith("data-cx-")
      ) {
        node.removeAttribute(name);
      }
    }
  }
}

export function createDragGhost(source: HTMLElement, pointerX: number, pointerY: number, reduced: boolean): DragGhost {
  const rect = source.getBoundingClientRect();
  const offsetX = pointerX - rect.left;
  const offsetY = pointerY - rect.top;

  const clone = source.cloneNode(true) as HTMLElement;
  scrub(clone);
  clone.style.opacity = "1";
  clone.style.transform = "none";

  const ghost = document.createElement("div");
  ghost.className = GHOST_CLASS;
  ghost.setAttribute("aria-hidden", "true");
  ghost.inert = true;
  ghost.style.width = `${rect.width}px`;
  ghost.style.maxHeight = `${GHOST_MAX_HEIGHT}px`;
  ghost.style.willChange = "transform";
  if (rect.height > GHOST_MAX_HEIGHT) {
    const mask = `linear-gradient(to bottom, #000 calc(100% - 48px), transparent)`;
    ghost.style.maskImage = mask;
    ghost.style.webkitMaskImage = mask;
  }
  ghost.appendChild(clone);

  let x = rect.left;
  let y = rect.top;
  const place = () => { ghost.style.transform = `translate3d(${x}px, ${y}px, 0)`; };
  place();
  portalHost().appendChild(ghost);
  if (!reduced) {
    ghost.animate(
      [{ opacity: 0.6, scale: "0.98" }, { opacity: 1, scale: "1.02" }],
      { duration: 140, easing: "cubic-bezier(0.16, 1, 0.3, 1)", fill: "forwards" },
    );
  }

  let finished = false;
  const remove = () => ghost.remove();
  const finish = (keyframes: Keyframe[], duration: number) => {
    if (finished) return;
    finished = true;
    if (reduced) {
      remove();
      return;
    }
    ghost.getAnimations().forEach((animation) => animation.cancel());
    const animation = ghost.animate(keyframes, { duration, easing: "cubic-bezier(0.16, 1, 0.3, 1)", fill: "forwards" });
    animation.onfinish = remove;
    animation.oncancel = remove;
    // Animations stall when the document stops producing frames (hidden tab); never leave the copy behind.
    window.setTimeout(remove, duration + 100);
  };

  return {
    move: (nextX, nextY) => {
      if (finished) return;
      x = nextX - offsetX;
      y = nextY - offsetY;
      place();
    },
    drop: () => finish([{ opacity: 1, scale: "1.02" }, { opacity: 0, scale: "1" }], 120),
    cancel: () => {
      const home = source.isConnected ? source.getBoundingClientRect() : null;
      if (!home) {
        finish([{ opacity: 1 }, { opacity: 0 }], 100);
        return;
      }
      finish([
        { transform: `translate3d(${x}px, ${y}px, 0)`, scale: "1.02", opacity: 1 },
        { transform: `translate3d(${home.left}px, ${home.top}px, 0)`, scale: "1", opacity: 0.35 },
      ], 180);
    },
  };
}
