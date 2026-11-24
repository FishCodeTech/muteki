"use client";

import {
  cloneElement,
  createContext,
  useCallback,
  useContext,
  useEffect,
  useId,
  useMemo,
  useState,
  type ComponentPropsWithRef,
  type ReactElement,
  type ReactNode,
  type Ref,
} from "react";
import { motion, type HTMLMotionProps } from "motion/react";
import { cn } from "../utils/cn";
import { useReducedMotion } from "../utils/reducedMotion";
import { Icon } from "../icons/Icon";
import { EASE_OUT, SPRING_LAYOUT } from "../primitives/motion";

export type MessageFrom = "user" | "assistant";
export type MessageSide = "start" | "end";
export type MessageBubbleVariant = "solid" | "soft" | "tint" | "outline" | "ghost" | "danger";
export type MessageBubbleAlign = "start" | "end";

const MessageSideContext = createContext<MessageSide | undefined>(undefined);
const MessageContext = createContext<{ from: MessageFrom }>({ from: "assistant" });

interface MessageBubbleContextValue {
  align?: MessageBubbleAlign;
  animateIn: boolean;
  variant: MessageBubbleVariant;
}

const MessageBubbleContext = createContext<MessageBubbleContextValue>({
  animateIn: true,
  variant: "soft",
});

const MessageBubbleLayoutContext = createContext<{
  notifyLayout: () => void;
  registerCollapsible: () => () => void;
}>({
  notifyLayout: () => {},
  registerCollapsible: () => () => {},
});

function mergeRefs<T>(...refs: Array<Ref<T> | undefined>) {
  return (node: T | null) => {
    for (const ref of refs) {
      if (typeof ref === "function") ref(node);
      else if (ref) (ref as { current: T | null }).current = node;
    }
  };
}

const MESSAGE_POP_UP = {
  type: "spring",
  stiffness: 480,
  damping: 32,
  mass: 0.62,
} as const;

const BUBBLE_CONTENT_REVEAL = {
  duration: 0.12,
  ease: EASE_OUT,
  delay: 0.04,
} as const;

export interface MessageProps
  extends Omit<ComponentPropsWithRef<typeof motion.article>, "children"> {
  from: MessageFrom;
  animateIn?: boolean;
  children: ReactNode;
}

export interface MessageGroupProps extends ComponentPropsWithRef<"div"> {
  spacing?: "compact" | "default";
}

export interface MessageAvatarProps extends ComponentPropsWithRef<"div"> {
  placeholder?: boolean;
}

export type MessageContentProps = ComponentPropsWithRef<"div">;
export type MessageHeaderProps = ComponentPropsWithRef<"div">;
export type MessageFooterProps = ComponentPropsWithRef<"div">;
export type MessageMarkerProps = ComponentPropsWithRef<"div">;

export interface MessageTypingProps extends ComponentPropsWithRef<"span"> {
  label?: string;
}

export function Message({
  from,
  animateIn = false,
  children,
  className,
  initial,
  animate,
  transition,
  exit,
  style,
  ...props
}: MessageProps) {
  const reduced = useReducedMotion();
  return (
    <MessageSideContext.Provider value={from === "user" ? "end" : "start"}>
      <MessageContext.Provider value={{ from }}>
        <motion.article
          data-slot="message"
          data-from={from}
          aria-label={props["aria-label"] ?? `${from} message`}
          initial={
            initial ??
            (animateIn && !reduced ? { opacity: 0, transform: "translateY(6px) scale(0.96)" } : false)
          }
          animate={
            animate ??
            (animateIn && !reduced
              ? { opacity: 1, transform: "translateY(0px) scale(1)" }
              : { opacity: 1 })
          }
          exit={
            exit ??
            (reduced
              ? { opacity: 0 }
              : { opacity: 0, transform: "translateY(-3px) scale(0.99)" })
          }
          transition={transition ?? (reduced ? { duration: 0.12 } : MESSAGE_POP_UP)}
          style={{
            transformOrigin: from === "user" ? "100% 100%" : "0% 100%",
            ...style,
          }}
          className={cn(
            "group/message flex w-full items-start gap-2",
            from === "user" ? "flex-row-reverse" : "flex-row",
            className,
          )}
          {...props}
        >
          {children}
        </motion.article>
      </MessageContext.Provider>
    </MessageSideContext.Provider>
  );
}

export function MessageGroup({ spacing = "compact", className, ...props }: MessageGroupProps) {
  return (
    <div
      data-slot="message-group"
      className={cn("flex w-full flex-col", spacing === "compact" ? "gap-1.5" : "gap-4", className)}
      {...props}
    />
  );
}

export function MessageAvatar({ placeholder = false, children, className, ...props }: MessageAvatarProps) {
  return (
    <div
      data-slot="message-avatar"
      aria-hidden={placeholder || undefined}
      className={cn(
        "grid size-7 shrink-0 place-items-center overflow-hidden rounded-full bg-cx-hover text-cx-xs font-medium text-cx-fg-3 [&_img]:size-full [&_img]:object-cover [&_svg]:size-3.5",
        placeholder && "invisible",
        className,
      )}
      {...props}
    >
      {children}
    </div>
  );
}

export function MessageContent({ className, ...props }: MessageContentProps) {
  const { from } = useContext(MessageContext);
  return (
    <div
      data-slot="message-content"
      className={cn("flex min-w-0 flex-1 flex-col gap-1.5", from === "user" ? "items-end" : "items-start", className)}
      {...props}
    />
  );
}

export function MessageHeader({ className, ...props }: MessageHeaderProps) {
  const { from } = useContext(MessageContext);
  return (
    <div
      data-slot="message-header"
      className={cn(
        "flex items-center gap-1.5 px-1 text-cx-xs leading-none text-cx-fg-3",
        from === "user" ? "justify-end" : "justify-start",
        className,
      )}
      {...props}
    />
  );
}

export function MessageFooter({ className, ...props }: MessageFooterProps) {
  const { from } = useContext(MessageContext);
  return (
    <div
      data-slot="message-footer"
      className={cn(
        "flex min-h-5 items-center gap-1 px-1 text-cx-xs text-cx-fg-3",
        from === "user" ? "justify-end" : "justify-start",
        className,
      )}
      {...props}
    />
  );
}

export function MessageMarker({ className, ...props }: MessageMarkerProps) {
  return (
    <div
      data-slot="message-marker"
      className={cn(
        "mx-auto flex w-fit max-w-[88%] items-center gap-1.5 rounded-full bg-cx-hover/70 px-2.5 py-1 text-center text-cx-xs text-cx-fg-3",
        className,
      )}
      {...props}
    />
  );
}

export function MessageTyping({ label = "正在回复", className, ...props }: MessageTypingProps) {
  const reduced = useReducedMotion();
  return (
    <span data-slot="message-typing" className={cn("inline-flex h-5 items-center gap-1", className)} {...props}>
      <span className="sr-only">{label}</span>
      {[0, 1, 2].map((index) => (
        <motion.span
          key={index}
          aria-hidden="true"
          className="size-1 rounded-full bg-current"
          animate={reduced ? { opacity: 0.45 } : { opacity: [0.3, 1, 0.3], transform: ["translateY(0px)", "translateY(-2px)", "translateY(0px)"] }}
          transition={reduced ? undefined : { duration: 1.1, repeat: Number.POSITIVE_INFINITY, delay: index * 0.14 }}
        />
      ))}
    </span>
  );
}

export interface MessageBubbleProps extends Omit<HTMLMotionProps<"div">, "children"> {
  variant?: MessageBubbleVariant;
  align?: MessageBubbleAlign;
  animateIn?: boolean;
  children?: ReactNode;
}

export interface MessageBubbleContentProps extends ComponentPropsWithRef<"div"> {
  render?: ReactElement;
}

export interface MessageBubbleGroupProps extends ComponentPropsWithRef<"div"> {
  spacing?: "compact" | "default";
}

export interface MessageBubbleCollapsibleProps extends ComponentPropsWithRef<"div"> {
  open?: boolean;
  defaultOpen?: boolean;
  onOpenChange?: (open: boolean) => void;
  collapsedLines?: 2 | 3 | 4 | 5 | 6;
  moreLabel?: ReactNode;
  lessLabel?: ReactNode;
  contentClassName?: string;
  triggerClassName?: string;
  children?: ReactNode;
}

export function MessageBubble({
  variant = "soft",
  align,
  animateIn = false,
  className,
  children,
  initial,
  animate,
  exit,
  transition,
  layout,
  ...props
}: MessageBubbleProps) {
  const reduced = useReducedMotion();
  const messageSide = useContext(MessageSideContext);
  const resolvedAlign = align ?? messageSide ?? "start";

  return (
    <MessageBubbleContext.Provider value={{ align: resolvedAlign, animateIn, variant }}>
      <motion.div
        data-slot="message-bubble"
        data-align={resolvedAlign}
        data-variant={variant}
        layout={layout}
        initial={initial ?? false}
        animate={animate}
        exit={
          exit ??
          (reduced ? { opacity: 0 } : { opacity: 0, transform: "translateY(-3px) scale(0.99)" })
        }
        transition={transition ?? (reduced ? { duration: 0.12 } : SPRING_LAYOUT)}
        className={cn(
          "group/bubble flex w-full flex-col",
          resolvedAlign === "end" ? "items-end" : "items-start",
          className,
        )}
        {...props}
      >
        {children}
      </motion.div>
    </MessageBubbleContext.Provider>
  );
}

function bubbleContentClass(variant: MessageBubbleVariant, interactive: boolean) {
  return cn(
    "relative z-0 min-w-9 max-w-[82%] rounded-[14px] px-3.5 py-2.5 text-cx-sm leading-6 text-cx-fg",
    "[&_a]:font-medium [&_a]:underline [&_a]:underline-offset-4 [&_code]:rounded [&_code]:bg-cx-bg/60 [&_code]:px-1 [&_code]:py-0.5 [&_code]:font-cx-mono [&_code]:text-[0.9em] [&_ol]:my-2 [&_ol]:list-decimal [&_ol]:pl-5 [&_p+p]:mt-2 [&_pre]:my-2 [&_pre]:overflow-x-auto [&_pre]:rounded-[10px] [&_pre]:bg-cx-bg/60 [&_pre]:p-3 [&_ul]:my-2 [&_ul]:list-disc [&_ul]:pl-5",
    variant === "solid" && "text-cx-bg",
    variant === "ghost" && "w-full max-w-none rounded-none px-0 py-0",
    variant === "danger" && "text-cx-danger",
    interactive &&
      "cursor-pointer text-left outline-none transition-[background-color,color,transform] duration-150 hover:brightness-[0.98] focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)] active:scale-[0.99]",
  );
}

function bubbleSurfaceClass(variant: MessageBubbleVariant, align: MessageBubbleAlign) {
  return cn(
    "pointer-events-none absolute inset-0 -z-10 rounded-[inherit]",
    align === "end" ? "origin-bottom-right" : "origin-bottom-left",
    variant === "solid" && "bg-cx-fg",
    variant === "soft" && "ui-surface-ring bg-cx-elevated",
    variant === "tint" && "border border-cx-accent-line/35 bg-cx-accent-soft",
    variant === "outline" && "border border-cx-border-strong bg-cx-bg",
    variant === "danger" && "border border-cx-danger/30 bg-cx-danger-soft",
  );
}

export function MessageBubbleContent({ render, className, children, ref, ...props }: MessageBubbleContentProps) {
  const reduced = useReducedMotion();
  const { align = "start", animateIn, variant } = useContext(MessageBubbleContext);
  const [layoutVersion, setLayoutVersion] = useState(0);
  const [collapsibleCount, setCollapsibleCount] = useState(0);
  const notifyLayout = useCallback(() => setLayoutVersion((version) => version + 1), []);
  const registerCollapsible = useCallback(() => {
    setCollapsibleCount((count) => count + 1);
    return () => setCollapsibleCount((count) => Math.max(0, count - 1));
  }, []);
  const layoutContext = useMemo(() => ({ notifyLayout, registerCollapsible }), [notifyLayout, registerCollapsible]);
  const layoutEnabled = !reduced && (collapsibleCount > 0 || layoutVersion > 0);
  const interactive = render?.type === "button" || render?.type === "a";
  const classes = cn(bubbleContentClass(variant, interactive), className);

  const composedChildren = (
    <>
      {variant !== "ghost" ? (
        <motion.span
          aria-hidden="true"
          layout={layoutEnabled ? "size" : false}
          layoutDependency={layoutVersion}
          initial={animateIn && !reduced ? { opacity: 0 } : false}
          animate={{ opacity: 1 }}
          transition={
            reduced
              ? { duration: 0 }
              : { opacity: { duration: 0.12, ease: EASE_OUT }, layout: SPRING_LAYOUT }
          }
          className={bubbleSurfaceClass(variant, align)}
        />
      ) : null}
      <MessageBubbleLayoutContext.Provider value={layoutContext}>
        <motion.div
          initial={animateIn ? { opacity: 0 } : false}
          animate={{ opacity: 1 }}
          transition={reduced ? { duration: 0.12, ease: EASE_OUT } : BUBBLE_CONTENT_REVEAL}
          className="relative"
        >
          {children}
        </motion.div>
      </MessageBubbleLayoutContext.Provider>
    </>
  );

  if (render) {
    const child = render as ReactElement<Record<string, unknown> & { className?: string; ref?: Ref<HTMLElement> }>;
    return cloneElement(child, {
      ...props,
      ref: mergeRefs(child.props.ref, ref as Ref<HTMLElement> | undefined),
      className: cn(classes, child.props.className),
      children: composedChildren,
      "data-slot": "message-bubble-content",
    });
  }

  return (
    <div ref={ref} data-slot="message-bubble-content" className={classes} {...props}>
      {composedChildren}
    </div>
  );
}

export function MessageBubbleGroup({ spacing = "compact", className, ...props }: MessageBubbleGroupProps) {
  return (
    <div
      data-slot="message-bubble-group"
      className={cn("flex w-full flex-col", spacing === "compact" ? "gap-1.5" : "gap-3", className)}
      {...props}
    />
  );
}

const LINE_CLAMP_CLASS = {
  2: "line-clamp-2",
  3: "line-clamp-3",
  4: "line-clamp-4",
  5: "line-clamp-5",
  6: "line-clamp-6",
} as const;

export function MessageBubbleCollapsible({
  open,
  defaultOpen = false,
  onOpenChange,
  collapsedLines = 4,
  moreLabel = "展开",
  lessLabel = "收起",
  contentClassName,
  triggerClassName,
  className,
  children,
  ...props
}: MessageBubbleCollapsibleProps) {
  const contentId = useId();
  const { notifyLayout, registerCollapsible } = useContext(MessageBubbleLayoutContext);
  const [internalOpen, setInternalOpen] = useState(defaultOpen);
  const currentOpen = open ?? internalOpen;

  useEffect(() => registerCollapsible(), [registerCollapsible]);

  const setOpen = useCallback(
    (next: boolean) => {
      notifyLayout();
      if (open === undefined) setInternalOpen(next);
      onOpenChange?.(next);
    },
    [notifyLayout, onOpenChange, open],
  );

  return (
    <div
      data-slot="message-bubble-collapsible"
      data-state={currentOpen ? "open" : "closed"}
      className={cn("w-full", className)}
      {...props}
    >
      <div
        id={contentId}
        className={cn(
          !currentOpen && LINE_CLAMP_CLASS[collapsedLines],
          !currentOpen && "[mask-image:linear-gradient(to_bottom,#000_68%,transparent_100%)]",
          contentClassName,
        )}
      >
        {children}
      </div>
      <button
        type="button"
        aria-expanded={currentOpen}
        aria-controls={contentId}
        onClick={() => setOpen(!currentOpen)}
        className={cn(
          "mt-2 inline-flex h-7 items-center gap-1 rounded-full px-2 text-cx-xs font-medium text-cx-fg-3 outline-none transition-colors hover:bg-cx-hover hover:text-cx-fg focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]",
          triggerClassName,
        )}
      >
        <span>{currentOpen ? lessLabel : moreLabel}</span>
        <Icon
          name="chevronDown"
          size={13}
          className={cn("transition-transform duration-150 ease-cx-out", currentOpen && "rotate-180")}
        />
      </button>
    </div>
  );
}
