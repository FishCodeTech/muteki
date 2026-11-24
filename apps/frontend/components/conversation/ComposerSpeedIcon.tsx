"use client";

import { useId } from "react";
import { Icon } from "@muteki/ui";
import { cn } from "@/lib/cn";
import type { ConversationServiceTier } from "@/lib/useConversation";

export type ComposerSpeedLevel = "standard" | "fast" | "ultrafast";

export function serviceTierSpeedLevel(tier: ConversationServiceTier): ComposerSpeedLevel {
  return [tier.name, tier.id].some((value) => value.trim().toLowerCase() === "ultrafast") ? "ultrafast" : "fast";
}

export function ComposerSpeedIcon({ tier, level, size = 15, className }: {
  tier?: ConversationServiceTier;
  level?: ComposerSpeedLevel;
  size?: number;
  className?: string;
}) {
  const separationMaskId = useId().replace(/[^a-zA-Z0-9_-]/g, "_");
  const resolved = level ?? (tier ? serviceTierSpeedLevel(tier) : "standard");
  const ultrafast = resolved === "ultrafast";
  const filled = resolved !== "standard";
  const stackedBoltStyle = { fill: "currentColor", strokeWidth: (1.5 * 24) / size };
  return (
    <span aria-hidden className={cn("inline-flex shrink-0 items-center justify-center", className)} style={{ width: size, height: size }}>
      {!ultrafast ? <Icon name="zap" size={size} style={filled ? { fill: "currentColor" } : undefined} /> : (
        <svg width={size} height={size} viewBox="0 0 28 26" focusable="false">
          <defs>
            <mask id={separationMaskId} x="0" y="0" width="28" height="26" maskUnits="userSpaceOnUse">
              <rect width="28" height="26" fill="white" />
              <Icon name="zap" size={24} style={{ ...stackedBoltStyle, color: "black", strokeWidth: stackedBoltStyle.strokeWidth + 2 }} />
            </mask>
          </defs>
          <g mask={`url(#${separationMaskId})`}>
            <g transform="translate(4 1)">
              <Icon name="zap" size={24} style={stackedBoltStyle} />
            </g>
          </g>
          <Icon name="zap" size={24} style={stackedBoltStyle} />
        </svg>
      )}
    </span>
  );
}
