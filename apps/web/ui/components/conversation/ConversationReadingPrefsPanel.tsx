"use client";

import React, { useEffect, useState, type ReactNode } from "react";
import { cn } from "@/lib/cn";
import { SegmentedControl, Slider, Switch } from "@/components/chat/ui";
import { useT } from "@/lib/i18n";
import {
  readConversationReadingPrefs,
  subscribeConversationReadingPrefs,
  writeConversationReadingPrefs,
  type ConversationContentWidth,
  type ConversationFontScale,
  type ConversationReadingPrefs,
} from "@/lib/conversationReadingPrefs";

const FONT_SCALES: ConversationFontScale[] = ["sm", "md", "lg"];

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
  hideIntro = false,
}: {
  className?: string;
  compact?: boolean;
  hideIntro?: boolean;
}) {
  const t = useT();
  const [prefs, setPrefs] = useState<ConversationReadingPrefs>(() => readConversationReadingPrefs());

  useEffect(() => subscribeConversationReadingPrefs(setPrefs), []);

  const fontIndex = Math.max(0, FONT_SCALES.indexOf(prefs.fontScale));
  const fontLabel = t(`readingPrefs.font.${prefs.fontScale}`);

  return (
    <section
      className={cn(
        "cx-reading-prefs flex flex-col",
        compact ? "is-compact w-full min-w-[248px] max-w-[360px] gap-3.5" : "max-w-[440px] gap-4",
        className,
      )}
      data-testid="c39-reading-prefs"
      aria-label={t("readingPrefs.aria")}
    >
      {!compact && !hideIntro ? (
        <header className="flex flex-col gap-1">
          <h3 className="text-[14px] font-semibold text-cx-fg">{t("readingPrefs.title")}</h3>
          <p className="text-[12.5px] leading-5 text-cx-fg-3">{t("readingPrefs.hint")}</p>
        </header>
      ) : null}

      <PrefRow label={t("readingPrefs.fontScale")} value={fontLabel} testId="c39-font-scale" compact={compact}>
        <label className="flex items-center gap-2.5">
          <span className="sr-only">{t("readingPrefs.fontScale")}</span>
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

      <PrefRow label={t("readingPrefs.contentWidth")} testId="c39-content-width" compact={compact}>
        <SegmentedControl<ConversationContentWidth>
          value={prefs.contentWidth}
          onChange={(contentWidth) => writeConversationReadingPrefs({ contentWidth })}
          ariaLabel={t("readingPrefs.contentWidth")}
          className="w-full [&>button]:flex-1"
          options={[
            { value: "narrow", label: t("readingPrefs.width.narrow"), icon: "minimize" },
            { value: "default", label: t("readingPrefs.width.default"), icon: "alignJustify" },
            { value: "wide", label: t("readingPrefs.width.wide"), icon: "maximize" },
          ]}
        />
      </PrefRow>

      <label className="cx-reading-pref-row block cursor-pointer" data-testid="c39-density">
        <Switch
          checked={prefs.density === "compact"}
          onCheckedChange={(on) => writeConversationReadingPrefs({ density: on ? "compact" : "comfortable" })}
          label={t("readingPrefs.density")}
          description={t("readingPrefs.densityHint")}
          size={compact ? "sm" : "md"}
        />
      </label>
    </section>
  );
}
