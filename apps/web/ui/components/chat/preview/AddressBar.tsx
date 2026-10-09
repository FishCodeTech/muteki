"use client";

import { useRef, useState, type RefObject } from "react";
import { cn } from "@/lib/cn";
import { Icon } from "@/components/Icon";
import { splitPreviewUrl } from "@/lib/previewUrlDetect";

/**
 * Browser-style address field: shows host-emphasized URL at rest, the raw
 * editable text on focus (select-all), Enter commits, Esc reverts.
 */
export function AddressBar({
  value,
  committed,
  onValueChange,
  onSubmit,
  inputRef,
  trackable,
}: {
  value: string;
  committed: string;
  onValueChange: (value: string) => void;
  onSubmit: (value: string) => void;
  inputRef: RefObject<HTMLInputElement | null>;
  trackable: boolean;
}) {
  const [focused, setFocused] = useState(false);
  const selectOnMouseUp = useRef(false);
  const parts = splitPreviewUrl(value);
  const showPretty = !focused && Boolean(value) && value === committed;

  return (
    <div
      className={cn(
        "group/address relative flex h-7 min-w-0 flex-1 items-center rounded-lg transition-[background-color,box-shadow] duration-150",
        focused
          ? "bg-cx-elevated shadow-[0_0_0_1px_var(--cx-border-strong)]"
          : "bg-cx-hover hover:bg-cx-active",
      )}
    >
      <Icon
        name={parts.secure ? "lock" : "globe"}
        size={12}
        className={cn("pointer-events-none absolute left-2.5 shrink-0", parts.secure ? "text-cx-success" : "text-cx-fg-4")}
      />
      <input
        ref={inputRef}
        value={value}
        spellCheck={false}
        autoCapitalize="off"
        autoCorrect="off"
        aria-label="浏览器地址"
        placeholder="输入 localhost 地址或网址"
        title={!trackable && committed ? "页内地址不可跟踪（跨源页面），显示的是初始地址" : undefined}
        onChange={(event) => onValueChange(event.target.value)}
        onFocus={(event) => {
          setFocused(true);
          event.currentTarget.select();
          selectOnMouseUp.current = true;
        }}
        onMouseUp={(event) => {
          if (!selectOnMouseUp.current) return;
          selectOnMouseUp.current = false;
          event.preventDefault();
        }}
        onBlur={() => { setFocused(false); selectOnMouseUp.current = false; }}
        onKeyDown={(event) => {
          if (event.key === "Enter") {
            event.preventDefault();
            onSubmit(value);
            event.currentTarget.blur();
          } else if (event.key === "Escape") {
            event.preventDefault();
            event.stopPropagation();
            if (value !== committed) {
              onValueChange(committed);
              requestAnimationFrame(() => inputRef.current?.select());
            } else {
              event.currentTarget.blur();
            }
          }
        }}
        className={cn(
          "h-full w-full min-w-0 rounded-lg bg-transparent pl-7 pr-2.5 font-cx-sans text-[13px] outline-none placeholder:text-cx-fg-4",
          showPretty ? "text-transparent caret-transparent selection:bg-transparent" : "text-cx-fg",
        )}
      />
      {showPretty ? (
        <span aria-hidden className="pointer-events-none absolute inset-y-0 left-7 right-2.5 flex items-center overflow-hidden whitespace-nowrap text-[13px]">
          {!parts.secure && parts.scheme ? <span className="text-cx-fg-4">{parts.scheme}</span> : null}
          <span className="font-medium text-cx-fg">{parts.host}</span>
          <span className="truncate text-cx-fg-3">{parts.rest}</span>
        </span>
      ) : null}
    </div>
  );
}
