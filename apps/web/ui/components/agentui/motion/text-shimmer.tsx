// Vendored from AgentUI (https://www.agentui.pro), MIT License. See ./LICENSE.
import { cn } from "@/lib/cn";
import type { ElementType, ReactNode } from "react";
import {
  TEXT_SHIMMER_CLASS_NAME,
  TEXT_SHIMMER_KEYFRAMES,
  textShimmerStyle,
} from "@/components/agentui/lib/text-shimmer";

export interface TextShimmerProps {
  children: ReactNode;
  as?: ElementType;
  duration?: number;
  className?: string;
}

export function TextShimmer({ children, as: Comp = "span", duration = 2.5, className }: TextShimmerProps) {
  return (
    <>
      <style>
        {TEXT_SHIMMER_KEYFRAMES}
      </style>
      <Comp
        style={textShimmerStyle(duration)}
        className={cn(
          "inline-block",
          TEXT_SHIMMER_CLASS_NAME,
          className,
        )}
      >
        {children}
      </Comp>
    </>
  );
}
