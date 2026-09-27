"use client";
// Vendored from AgentUI (https://www.agentui.pro), MIT License. See ./LICENSE.


import { Check, Loader2, X } from "lucide-react";
import {
  AnimatePresence,
  motion,
  useReducedMotion,
  type Variants,
} from "motion/react";
import {
  forwardRef,
  type ReactNode,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
} from "react";
import { EASE_OUT, SPRING_SWAP } from "@/components/agentui/lib/ease";
import { Button, type ButtonProps } from "./base";

export type ButtonState = "idle" | "loading" | "success" | "error";

export interface StatefulButtonProps extends Omit<ButtonProps, "children"> {
  state?: ButtonState;
  children: ReactNode;
  loadingText?: ReactNode;
  successText?: ReactNode;
  errorText?: ReactNode;
  icon?: ReactNode;
}

const CASCADE_STAGGER = 0.025;
const CASCADE_OFFSET = 14;
const ROLL_BLUR = "blur(6px)";

const CASCADE_LETTER_VARIANTS: Variants = {
  initial: { opacity: 0, y: CASCADE_OFFSET, filter: ROLL_BLUR },
  static: { opacity: 1, y: 0, filter: "blur(0px)" },
  animate: (delay: number = 0) => ({
    opacity: 1,
    y: 0,
    filter: "blur(0px)",
    transition: {
      y: { ...SPRING_SWAP, delay },
      opacity: { duration: 0.2, ease: EASE_OUT, delay },
      filter: { duration: 0.2, ease: EASE_OUT, delay },
    },
  }),
  exit: (delay: number = 0) => ({
    opacity: 0,
    y: -CASCADE_OFFSET,
    filter: ROLL_BLUR,
    transition: { duration: 0.16, ease: EASE_OUT, delay: delay * 0.5 },
  }),
};

const ICON_VARIANTS: Variants = {
  // Width collapses too, so the icon adds/removes its own space smoothly
  // instead of popping the row width in a single frame.
  initial: { opacity: 0, width: 0, scale: 0.7, filter: ROLL_BLUR },
  animate: {
    opacity: 1,
    width: "1.5rem",
    scale: 1,
    filter: "blur(0px)",
    transition: SPRING_SWAP,
  },
  exit: {
    opacity: 0,
    width: 0,
    scale: 0.7,
    filter: ROLL_BLUR,
    transition: { duration: 0.16, ease: EASE_OUT },
  },
};

function IconSlot({ keyId, children }: { keyId: string; children: ReactNode }) {
  const reduce = useReducedMotion();
  const [mounted, setMounted] = useState(false);
  useEffect(() => setMounted(true), []);
  const motionReady = mounted && reduce === false;
  return (
    <motion.span
      key={keyId}
      variants={ICON_VARIANTS}
      initial={motionReady ? "initial" : false}
      animate={motionReady ? "animate" : { opacity: 1 }}
      exit={motionReady ? "exit" : { opacity: 0 }}
      transition={motionReady ? undefined : { duration: 0.15 }}
      className="inline-grid shrink-0 place-items-center overflow-hidden"
    >
      {children}
    </motion.span>
  );
}

function TextSlot({
  value,
  children,
}: {
  value: string;
  children: ReactNode;
}) {
  const reduce = useReducedMotion();
  const [mounted, setMounted] = useState(false);
  useEffect(() => setMounted(true), []);
  const motionReady = mounted && reduce === false;
  const measureRef = useRef<HTMLSpanElement>(null);
  const [width, setWidth] = useState<number>();
  const label = typeof children === "string" ? children : null;
  const cascade = label !== null;

  // Measure strings with the same per-letter layout as the cascade. Measuring
  // the whole string preserves kerning, which can make it narrower than the
  // inline-block letters and clip the final glyph during the width animation.
  useLayoutEffect(() => {
    const nextWidth = measureRef.current?.offsetWidth;
    if (!nextWidth) return;
    setWidth((current) => (current === nextWidth ? current : nextWidth));
  });

  return (
    <motion.span
      initial={false}
      animate={{ width }}
      transition={motionReady ? SPRING_SWAP : { duration: 0 }}
      className="relative inline-block overflow-hidden whitespace-nowrap align-bottom"
    >
      <span
        ref={measureRef}
        aria-hidden
        className="invisible inline-block whitespace-nowrap"
      >
        {cascade
          ? label.split("").map((char, index) => (
              <span
                // biome-ignore lint/suspicious/noArrayIndexKey: position is the slot identity.
                key={index}
                className="inline-block whitespace-pre"
              >
                {char}
              </span>
            ))
          : children}
      </span>

      {cascade ? (
        <>
          <span className="sr-only">{label}</span>
          <AnimatePresence initial={false}>
            <motion.span
              key={`cascade-${value}`}
              aria-hidden
              initial={motionReady ? "initial" : false}
              animate={motionReady ? "animate" : "static"}
              exit={motionReady ? "exit" : undefined}
              className="absolute left-0 top-0 inline-block whitespace-pre"
            >
              {label.split("").map((char, index) => (
                <motion.span
                  // biome-ignore lint/suspicious/noArrayIndexKey: position is the slot identity.
                  key={index}
                  custom={index * CASCADE_STAGGER}
                  variants={CASCADE_LETTER_VARIANTS}
                  className="inline-block whitespace-pre will-change-[opacity,filter,transform]"
                >
                  {char}
                </motion.span>
              ))}
            </motion.span>
          </AnimatePresence>
        </>
      ) : (
        <AnimatePresence initial={false}>
          <motion.span
            key={`text-${value}`}
            initial={
              motionReady
                ? { opacity: 0, y: 14, filter: ROLL_BLUR }
                : false
            }
            animate={
              motionReady
                ? { opacity: 1, y: 0, filter: "blur(0px)" }
                : { opacity: 1 }
            }
            exit={
              motionReady
                ? { opacity: 0, y: -14, filter: ROLL_BLUR }
                : { opacity: 0 }
            }
            transition={motionReady ? SPRING_SWAP : { duration: 0.15 }}
            className="absolute left-0 top-0 inline-block will-change-[opacity,filter,transform]"
          >
            {children}
          </motion.span>
        </AnimatePresence>
      )}
    </motion.span>
  );
}

export const StatefulButton = forwardRef<HTMLButtonElement, StatefulButtonProps>(function StatefulButton(
  {
    state = "idle",
    children,
    loadingText = "处理中",
    successText = "完成",
    errorText = "重试",
    icon,
    disabled,
    ...rest
  },
  ref,
) {
  const isBusy = state === "loading";
  const stateText =
    state === "loading"
      ? loadingText
      : state === "success"
        ? successText
        : state === "error"
        ? errorText
        : children;
  const textKey =
    typeof stateText === "string" ? `${state}-${stateText}` : state;

  return (
    <Button ref={ref} disabled={disabled || isBusy} aria-busy={isBusy} whileHover={undefined} {...rest}>
      <span
        aria-live="polite"
        className="relative inline-flex items-center justify-center overflow-hidden"
      >
        <AnimatePresence initial={false}>
          {state === "loading" ? (
            <IconSlot keyId="loading-icon">
              <Loader2 className="h-4 w-4 animate-spin" />
            </IconSlot>
          ) : null}
          {state === "success" ? (
            <IconSlot keyId="success-icon">
              <Check className="h-4 w-4" />
            </IconSlot>
          ) : null}
          {state === "error" ? (
            <IconSlot keyId="error-icon">
              <X className="h-4 w-4" />
            </IconSlot>
          ) : null}
        </AnimatePresence>

        <TextSlot value={textKey}>{stateText}</TextSlot>

        <AnimatePresence initial={false}>
          {state === "idle" && icon ? (
            <IconSlot keyId="idle-icon">{icon}</IconSlot>
          ) : null}
        </AnimatePresence>
      </span>
    </Button>
  );
});
