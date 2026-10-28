"use client";

import { useId } from "react";
import { Icon } from "@/components/Icon";
import { cn } from "@/lib/cn";
import type { ConversationServiceTier } from "@/lib/useConversation";

/** One fixed icon box for standard, fast and ultrafast service tiers. */
export function ComposerSpeedIcon({ tier, size = 15, className }: {
  tier?: ConversationServiceTier;
  size?: number;
  className?: string;
}) {
  const separationMaskId = useId();
  const ultrafast = tier && [tier.name, tier.id].some((value) => value.trim().toLowerCase() === "ultrafast");
  // Keep the outline when filling: Icon's `filled` variant removes its stroke.
  // Stacked copies use 24-unit coordinates with the same proportions as the single icon.
  const stackedBoltStyle = { fill: "currentColor", strokeWidth: (1.5 * 24) / size };
  return (
    <span aria-hidden className={cn("inline-flex shrink-0 items-center justify-center", className)} style={{ width: size, height: size }}>
      {!ultrafast ? <Icon name="zap" size={size} style={tier ? { fill: "currentColor" } : undefined} /> : (
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
