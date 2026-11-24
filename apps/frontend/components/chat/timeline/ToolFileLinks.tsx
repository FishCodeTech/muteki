"use client";

import { createContext, useContext } from "react";
import { Icon } from "@muteki/ui";
import { previewKindFromName, type ToolFile } from "@/lib/resourcePreview";

export const OpenToolFile = createContext<((file: ToolFile) => void) | null>(null);

export function ToolFileLinks({ files }: { files?: ToolFile[] }) {
  const open = useContext(OpenToolFile);
  if (!open || !files?.length) return null;
  return (
    <div className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1" data-testid="tool-file-links">
      {files.map((file) => {
        const mediaType = "media_type" in file ? file.media_type : undefined;
        const image = previewKindFromName(file.name, mediaType) === "image";
        const hasSavedCopy = file.kind === "workspace" && files.some((item) =>
          item.name === file.name && (item.kind === "artifact" || item.kind === "recorded"));
        const detail = file.kind === "workspace" && file.line ? `:${file.line}` : "";
        const label = `${file.name}${detail}${hasSavedCopy ? "（当前文件）" : ""}`;
        return (
          <button
            key={JSON.stringify(file)}
            type="button"
            data-testid="tool-file-link"
            title={label}
            onClick={() => open(file)}
            className="inline-flex h-7 min-w-0 max-w-full items-center gap-1.5 rounded-md px-1 text-cx-sm text-cx-accent hover:bg-cx-hover hover:underline focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-cx-focus"
          >
            <Icon name={image ? "image" : "fileCode"} size={14} className="shrink-0" />
            <span className="truncate">{label}</span>
          </button>
        );
      })}
    </div>
  );
}
