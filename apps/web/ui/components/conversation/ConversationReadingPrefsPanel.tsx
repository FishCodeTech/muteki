"use client";

import React, { useEffect, useState, type ReactNode } from "react";
import { cn } from "@/lib/cn";
import { SegmentedControl, Slider, Switch } from "@/components/chat/ui";
import {
  readConversationReadingPrefs,
  subscribeConversationReadingPrefs,
  writeConversationReadingPrefs,
  type ConversationContentWidth,
  type ConversationFontScale,
  type ConversationReadingPrefs,
} from "@/lib/conversationReadingPrefs";

const FONT_SCALES: ConversationFontScale[] = ["sm", "md", "lg"];
const FONT_LABEL: Record<ConversationFontScale, string> = { sm: "小", md: "标准", lg: "大" };

function PrefRow({
  label,
  value,
  testId,
  children,
  compact,
}: {
  label: string;
  value?: ReactNode;
  testId: string;
  children: ReactNode;
  compact: boolean;
}) {
  return (
    <div className={cn("cx-reading-pref-row flex flex-col", compact ? "gap-1.5" : "gap-2")} data-testid={testId}>
      <div className="flex items-center justify-between gap-3">
        <span className="text-[12.5px] font-medium text-cx-fg-2">{label}</span>
        {value ? <span className="cx-tabular text-[12px] text-cx-fg-4">{value}</span> : null}
      </div>
      {children}
    </div>
  );
}

export function ConversationReadingPrefsPanel({
  className = "",
  compact = false,
}: {
  className?: string;
  compact?: boolean;
}) {
  const [prefs, setPrefs] = useState<ConversationReadingPrefs>(() => readConversationReadingPrefs());

  useEffect(() => subscribeConversationReadingPrefs(setPrefs), []);

  const fontIndex = Math.max(0, FONT_SCALES.indexOf(prefs.fontScale));

  return (
    <section
      className={cn(
        "cx-reading-prefs flex flex-col",
        compact ? "is-compact w-full min-w-[248px] max-w-[360px] gap-3.5" : "max-w-[440px] gap-4",
        className,
      )}
      data-testid="c39-reading-prefs"
      aria-label="对话阅读偏好"
    >
      {!compact ? (
        <header className="flex flex-col gap-1">
          <h3 className="text-[14px] font-semibold text-cx-fg">对话阅读</h3>
          <p className="text-[12.5px] leading-5 text-cx-fg-3">调整字号、密度与正文宽度；不依赖浏览器整体缩放，偏好保存在本浏览器。</p>
        </header>
      ) : null}

      <PrefRow label="正文字号" value={FONT_LABEL[prefs.fontScale]} testId="c39-font-scale" compact={compact}>
        <label className="flex items-center gap-2.5">
          <span className="sr-only">正文字号</span>
          <span aria-hidden className="w-3 text-center text-[11px] font-semibold text-cx-fg-4">A</span>
          <Slider
            className="min-w-0 flex-1 gap-0"
            min={0}
            max={FONT_SCALES.length - 1}
            step={1}
            value={fontIndex}
            onValueChange={(index) => writeConversationReadingPrefs({ fontScale: FONT_SCALES[index] ?? "md" })}
          />
          <span aria-hidden className="w-3 text-center text-[15px] font-semibold text-cx-fg-3">A</span>
        </label>
      </PrefRow>

      <PrefRow label="正文宽度" testId="c39-content-width" compact={compact}>
        <SegmentedControl<ConversationContentWidth>
          value={prefs.contentWidth}
          onChange={(contentWidth) => writeConversationReadingPrefs({ contentWidth })}
          ariaLabel="正文宽度"
          className="w-full [&>button]:flex-1"
          options={[
            { value: "narrow", label: "窄", icon: "minimize" },
            { value: "default", label: "默认", icon: "alignJustify" },
            { value: "wide", label: "宽", icon: "maximize" },
          ]}
        />
      </PrefRow>

      <label className="cx-reading-pref-row block cursor-pointer" data-testid="c39-density">
        <Switch
          checked={prefs.density === "compact"}
          onCheckedChange={(on) => writeConversationReadingPrefs({ density: on ? "compact" : "comfortable" })}
          label="紧凑密度"
          description="缩小消息与卡片间距，一屏显示更多内容"
          size={compact ? "sm" : "md"}
        />
      </label>
    </section>
  );
}
