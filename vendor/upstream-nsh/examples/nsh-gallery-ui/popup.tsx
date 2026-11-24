// The popup families' one surface and the motion they come in with.
import type { JSX, MotionSpec, ViewStyle } from "nsh";
import { createMotion, Easing, View } from "nsh";
import { finish } from "./theme";

/** The side of its trigger a popup is on. */
export type PopupSide = "top" | "bottom" | "left" | "right";

/**
 * The surface every anchored popup shares (menus, popovers, a select's
 * list): the floating tier, one edge, a soft shadow, 10-point corners and
 * 4 points around its rows (Design Guides, "Surfaces and elevation").
 */
export const popupSurface = (): ViewStyle => ({ borderRadius: 10, borderWidth: 1, padding: 4, ...finish.menu });

/** Where a popup grows from: the edge that faces its trigger. */
const origins: Record<PopupSide, string> = { bottom: "top", top: "bottom", left: "right", right: "left" };

/**
 * How a popup comes in, as shadcn/ui's popups do (`fade-in-0 zoom-in-95
 * slide-in-from-*`, gpui-component's 150 ms): it fades up from nothing,
 * grows from 96 % and slides the last `distance` points out of its
 * trigger's edge, decelerating into place. Reduced motion lands it at once.
 */
export function popIn(side: PopupSide = "bottom", distance = 4): MotionSpec {
  const away = side === "top" || side === "left" ? distance : -distance;
  const axis = side === "top" || side === "bottom" ? "translateY" : "translateX";
  return {
    duration: 150,
    easing: Easing.bezier(0.16, 1, 0.3, 1),
    keyframes: { opacity: [0, 1], scale: [0.96, 1], [axis]: [away, 0] },
  };
}

/**
 * A popup's surface that comes in with `popIn` each time it is made: the
 * content of a `Popover`, a `Menu` or a `Submenu`, which make it on every
 * opening. The look is `class` and `style` (the popup's own box keeps
 * none, so the shadow stays in register with the moving panel); `side` is
 * the trigger's side it opens on.
 */
export function PopIn(props: { side?: PopupSide; class?: string; style?: ViewStyle; children: JSX.Element }) {
  const m = createMotion({ enter: popIn(props.side) });
  return (
    <View class={props.class} style={[props.style ?? null, { transformOrigin: origins[props.side ?? "bottom"], ...m.style }]}>
      {props.children}
    </View>
  );
}

/**
 * A surface that stays made between showings (a `Tooltip`'s bubble): hidden
 * while it is away, `show` plays `popIn` as it comes back. Call `show` from
 * the popup's `onOpen` and `hide` from its `onClose`.
 */
export function createPresence(side: PopupSide = "top") {
  const m = createMotion({ initial: { opacity: 0 } });
  return {
    style: { transformOrigin: origins[side], ...m.style } satisfies ViewStyle,
    show: () => void m.play(popIn(side, 3)),
    hide: () => {
      m.stop();
      m.opacity.set(0);
    },
  };
}
