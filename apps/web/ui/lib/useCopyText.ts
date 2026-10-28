"use client";

import { toast } from "@heroui/react/toast";
import { copyToClipboard } from "@/lib/clipboard";
import { useT } from "@/lib/i18n";

/** Copy in the input gesture, then report the browser's actual result. */
export function useCopyText(): (text: string) => void {
  const t = useT();
  return (text) => {
    void copyToClipboard(text).then((ok) => {
      toast(t(ok ? "common.copied" : "common.copyFailed"), {
        variant: ok ? "success" : "danger",
      });
    });
  };
}
