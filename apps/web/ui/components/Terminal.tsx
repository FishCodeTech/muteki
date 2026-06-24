"use client";

import { useEffect, useRef } from "react";
import { useT } from "@/lib/i18n";

/** Live sandbox stdout (§14.3 #4). A lightweight pre-based terminal; the
 *  package.json includes @xterm/xterm for a richer PTY view when wired to the
 *  WS endpoint, but the SSE TERMINAL_OUTPUT stream renders fine as text. */
export function Terminal({ lines }: { lines: string[] }) {
  const t = useT();
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (ref.current) ref.current.scrollTop = ref.current.scrollHeight;
  }, [lines]);
  return (
    <div className="term" ref={ref}>
      {lines.length === 0 ? <span style={{ color: "var(--dim)" }}>{t("empty.noOutput")}</span> : lines.join("")}
    </div>
  );
}
