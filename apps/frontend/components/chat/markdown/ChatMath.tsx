"use client";

import { useMemo } from "react";
import { renderChatMath } from "@/lib/chatMath";
import { MathFormula } from "@muteki/ui/engines/math-view";

export function ChatMath({ value, source, display }: { value: string; source: string; display: boolean }) {
  const html = useMemo(() => renderChatMath(value, display), [value, display]);
  return (
    <MathFormula
      value={value}
      source={source}
      display={display}
      renderedHtml={html}
    />
  );
}
