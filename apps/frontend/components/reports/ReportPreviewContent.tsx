"use client";

import { ChatMarkdown } from "@/components/chat/markdown/ChatMarkdown";
import { cn } from "@/lib/cn";
import { useT } from "@/lib/i18n";

export function ReportPreviewContent({ text, wide = false, onExpandImage }: {
  text: string;
  wide?: boolean;
  onExpandImage?: (src: string, name: string) => void;
}) {
  const t = useT();
  return (
    <div className={cn("mx-auto w-full px-6 py-5", wide ? "max-w-[820px] py-6 sm:px-10" : "max-w-[720px]")}>
      {text.trim() ? (
        <ChatMarkdown text={text} inlineImages annotateSourceOffsets={false} onExpandImage={onExpandImage} className="report-prose" />
      ) : (
        <p className="py-10 text-center text-cx-sm text-cx-fg-4">{t("reports.editor.emptyPreview")}</p>
      )}
    </div>
  );
}
