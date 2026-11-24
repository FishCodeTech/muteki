"use client";

import { toast } from "@muteki/ui";
import { copyToClipboard } from "@/lib/clipboard";
import { useT } from "@/lib/i18n";

export function useCopyText(): (text: string) => void {
  const t = useT();
  return (text) => {
    void copyToClipboard(text).then((ok) => {
      toast(t(ok ? "common.copied" : "common.copyFailed"), {
        tone: ok ? "success" : "danger",
      });
    });
  };
}
