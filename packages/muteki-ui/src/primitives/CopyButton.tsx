"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { IconButton } from "./Button";
import { toast } from "./Toast";
import { cn } from "../utils/cn";
import { copyToClipboard } from "../utils/clipboard";

export function useCopy(timeout = 1600) {
  const [copied, setCopied] = useState(false);
  const timerRef = useRef<number | undefined>(undefined);

  useEffect(
    () => () => {
      if (timerRef.current) window.clearTimeout(timerRef.current);
    },
    [],
  );

  const copy = useCallback(
    async (text: string) => {
      if (!(await copyToClipboard(text))) {
        toast({ title: "复制失败", description: "请重试，或手动选中内容复制。", tone: "danger" });
        return false;
      }
      setCopied(true);
      if (timerRef.current) window.clearTimeout(timerRef.current);
      timerRef.current = window.setTimeout(() => setCopied(false), timeout);
      return true;
    },
    [timeout],
  );

  return { copied, copy };
}

export function CopyButton({
  text,
  label = "复制",
  size = "xs",
  className,
}: {
  text: string | (() => string);
  label?: string;
  size?: "xs" | "sm";
  className?: string;
}) {
  const { copied, copy } = useCopy();
  return (
    <IconButton
      size={size}
      icon={copied ? "check" : "copy"}
      label={copied ? "已复制" : label}
      className={cn(copied && "text-cx-success hover:text-cx-success", className)}
      onClick={() => void copy(typeof text === "function" ? text() : text)}
    />
  );
}
