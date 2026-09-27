"use client";

/**
 * Compact diff viewer for single artifacts (details drawer). Renders with the
 * same virtualized DiffList as the Diff surface so both look identical.
 */

import React, { useMemo, useState } from "react";
import { IconButton, SegmentedControl } from "@/components/chat/ui";
import type { DiffFileMeta, DiffLineAnnotation, DiffStaging } from "@/lib/conversationDiff";
import { DiffList } from "@/components/chat/diff/DiffList";
import { diffFileKey, filesFromMetas } from "@/components/chat/diff/files";
import type { DiffViewMode } from "@/components/chat/diff/model";

export type { DiffViewMode };

export interface ConversationDiffViewerProps {
  files: DiffFileMeta[];
  selectedKey: string;
  onSelectFile: (key: string) => void;
  patch: string;
  path: string;
  staging?: DiffStaging | string;
  binary?: boolean;
  oldPath?: string | null;
  viewMode: DiffViewMode;
  onViewModeChange: (mode: DiffViewMode) => void;
  wrap: boolean;
  onWrapChange: (wrap: boolean) => void;
  contextLines: number;
  onContextLinesChange: (lines: number) => void;
  emptyTitle?: string;
  emptyDetail?: string;
  baselineId?: string;
  fileAnnotationCount?: number;
  onAddAnnotation?: (annotation: Omit<DiffLineAnnotation, "id" | "createdAt" | "stale">) => void;
  /** Bounded height for embedded use; defaults to 560px. */
  maxHeight?: number | string;
  threadId?: string;
}

export function ConversationDiffViewer({
  files,
  patch,
  path,
  staging,
  binary,
  oldPath,
  viewMode,
  onViewModeChange,
  wrap,
  onWrapChange,
  contextLines,
  onContextLinesChange,
  emptyTitle = "没有可显示的变更",
  emptyDetail,
  baselineId,
  onAddAnnotation,
  maxHeight = 560,
  threadId,
}: ConversationDiffViewerProps) {
  const [collapsed, setCollapsed] = useState<ReadonlySet<string>>(() => new Set());
  const listFiles = useMemo(() => {
    if (files.length) return filesFromMetas(files, patch);
    if (!patch.trim()) return [];
    const meta: DiffFileMeta = {
      path: path || "change.diff",
      old_path: oldPath,
      status: "M",
      staging: (staging as DiffStaging) || "artifact",
      binary,
      patch,
    };
    return filesFromMetas([meta], patch);
  }, [files, patch, path, oldPath, staging, binary]);

  if (!listFiles.length) {
    return (
      <div className="flex flex-col items-center gap-1 px-4 py-8 text-center">
        <p className="text-[13px] font-medium text-cx-fg-2">{emptyTitle}</p>
        {emptyDetail ? <p className="text-[12px] text-cx-fg-4">{emptyDetail}</p> : null}
      </div>
    );
  }

  return (
    <div className="flex min-h-0 flex-col overflow-hidden rounded-xl border border-cx-border-subtle" data-testid="conversation-diff-viewer">
      <div className="flex h-9 shrink-0 items-center gap-1.5 border-b border-cx-border-subtle bg-cx-bg-subtle px-2">
        <span className="min-w-0 flex-1 truncate px-1 text-[12px] text-cx-fg-3">{listFiles.length} 个文件</span>
        <SegmentedControl
          size="xs"
          value={viewMode}
          onChange={onViewModeChange}
          ariaLabel="Diff 布局"
          options={[
            { value: "unified", icon: "alignJustify", ariaLabel: "合并视图" },
            { value: "split", icon: "columns", ariaLabel: "分栏视图" },
          ]}
        />
        <IconButton size="xs" icon="wrapText" label={wrap ? "不换行" : "自动换行"} active={wrap} onClick={() => onWrapChange(!wrap)} />
        <IconButton
          size="xs"
          icon={contextLines >= 999 ? "foldVertical" : "unfoldVertical"}
          label={contextLines >= 999 ? "折叠未变更行" : "显示全部上下文"}
          active={contextLines < 999}
          onClick={() => onContextLinesChange(contextLines >= 999 ? 3 : 999)}
        />
      </div>
      <DiffList
        files={listFiles}
        mode={viewMode}
        wrap={wrap}
        contextLines={contextLines}
        collapsed={collapsed}
        onToggleFile={(key) => setCollapsed((current) => {
          const next = new Set(current);
          if (next.has(key)) next.delete(key);
          else next.add(key);
          return next;
        })}
        threadId={threadId}
        baselineId={baselineId}
        onAddAnnotation={onAddAnnotation}
        maxHeight={maxHeight}
      />
    </div>
  );
}

export { diffFileKey };
