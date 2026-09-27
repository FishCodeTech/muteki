// Vendored from AgentUI (https://www.agentui.pro), MIT License. See ./LICENSE.
import type { ReactNode } from "react";
import { TextShimmer } from "@/components/agentui/motion/text-shimmer";
import { cn } from "@/lib/cn";

export interface ThinkingShimmerProps {
  /** Loading message shown to the user. */
  children?: ReactNode;
  /** Seconds taken for one shimmer pass. */
  duration?: number;
  className?: string;
}

export function ThinkingShimmer({
  children = "思考中…",
  duration = 1.8,
  className,
}: ThinkingShimmerProps) {
  return (
    <TextShimmer
      as="span"
      duration={duration}
      className={cn("font-medium", className)}
    >
      {children}
    </TextShimmer>
  );
}
