"use client";

/* ─────────────────────────────────────────────────────────
 * DIFF TABLE — Compact unified diff viewer per file.
 *
 * Originally adapted from https://github.com/TurboKach/ai-native-react-components
 * (MIT, pinned 05dab2d2b5f1f3e40029776e339a486d70491079).
 * ───────────────────────────────────────────────────────── */

import React from "react";
import { cn } from "@/lib/cn";
import { Icon } from "@/components/Icon";
import { Button, CopyButton, DiffStat } from "@/components/chat/ui";

export interface DiffRow {
  type: "add" | "del" | "ctx" | "hunk";
  oldLine?: number;
  newLine?: number;
  content: string;
}

export interface DiffFile {
  path: string;
  additions?: number;
  deletions?: number;
  rows?: DiffRow[];
  raw?: string;
}

export interface DiffTableProps {
  title?: string;
  files?: DiffFile[];
  onSelectFile?: (file: DiffFile) => void;
  className?: string;
}

const HEADER_PREFIXES = ["diff --git", "index ", "--- ", "+++ ", "new file mode", "deleted file mode", "similarity index", "rename from", "rename to", "old mode", "new mode"];

function parseUnifiedDiff(diffText: string): DiffRow[] {
  const rows: DiffRow[] = [];
  let oldNum = 0;
  let newNum = 0;
  let inHunk = false;
  for (const line of diffText.replace(/\n$/, "").split("\n")) {
    if (line.startsWith("@@")) {
      const match = /@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/.exec(line);
      if (match) {
        oldNum = parseInt(match[1], 10);
        newNum = parseInt(match[2], 10);
      }
      inHunk = true;
      rows.push({ type: "hunk", content: line });
      continue;
    }
    if (!inHunk && HEADER_PREFIXES.some((prefix) => line.startsWith(prefix))) continue;
    if (line.startsWith("\\ No newline")) continue;
    if (line.startsWith("+")) rows.push({ type: "add", newLine: newNum++, content: line.slice(1) });
    else if (line.startsWith("-")) rows.push({ type: "del", oldLine: oldNum++, content: line.slice(1) });
    else rows.push({ type: "ctx", oldLine: oldNum++, newLine: newNum++, content: line.startsWith(" ") ? line.slice(1) : line });
  }
  return rows;
}

export function DiffTable({ title = "文件改动对比", files = [], onSelectFile, className = "" }: DiffTableProps) {
  if (!files.length) return null;

  return (
    <div className={cn("flex w-full flex-col gap-2.5", className)} aria-label={title}>
      {files.map((file, fileIdx) => {
        const rows = file.rows || (file.raw ? parseUnifiedDiff(file.raw) : []);
        const additions = file.additions ?? rows.filter((row) => row.type === "add").length;
        const deletions = file.deletions ?? rows.filter((row) => row.type === "del").length;
        return (
          <div key={file.path || fileIdx} className="overflow-hidden rounded-xl border border-cx-border bg-cx-elevated">
            <div className="flex h-9 items-center gap-2 border-b border-cx-border-subtle bg-cx-bg-subtle pl-3 pr-1.5">
              <Icon name="fileCode" size={13} className="shrink-0 text-cx-fg-4" />
              <span className="min-w-0 flex-1 truncate font-cx-mono text-[12px] font-medium text-cx-fg" title={file.path}>
                {file.path}
              </span>
              <DiffStat additions={additions} deletions={deletions} className="shrink-0 text-[12px]" />
              {file.raw ? <CopyButton text={file.raw} label="复制 Diff" /> : null}
              {onSelectFile ? (
                <Button size="xs" variant="ghost" onClick={() => onSelectFile(file)}>
                  查看详情
                </Button>
              ) : null}
            </div>
            {rows.length ? (
              <div className="cx-scroll max-h-72 overflow-auto font-cx-mono text-[12px] leading-[1.6]">
                <table className="w-full border-collapse">
                  <tbody>
                    {rows.map((row, rowIdx) => {
                      if (row.type === "hunk") {
                        return (
                          <tr key={rowIdx} className="bg-cx-accent-soft/60 text-cx-fg-3">
                            <td colSpan={3} className="px-3 py-0.5 text-[12px]">{row.content}</td>
                          </tr>
                        );
                      }
                      const isAdd = row.type === "add";
                      const isDel = row.type === "del";
                      return (
                        <tr key={rowIdx} className={cn(isAdd ? "bg-cx-add-bg" : isDel ? "bg-cx-del-bg" : "text-cx-fg-2")}>
                          <td className="w-10 select-none px-2 text-right text-[12px] text-cx-fg-4">{row.oldLine ?? ""}</td>
                          <td className="w-10 select-none px-2 text-right text-[12px] text-cx-fg-4">{row.newLine ?? ""}</td>
                          <td className="whitespace-pre pr-3 text-cx-fg">
                            <span className={cn("inline-block w-4 select-none", isAdd ? "text-cx-add" : isDel ? "text-cx-del" : "text-transparent")}>
                              {isAdd ? "+" : isDel ? "−" : " "}
                            </span>
                            {row.content}
                          </td>
                        </tr>
                      );
                    })}
                  </tbody>
                </table>
              </div>
            ) : null}
          </div>
        );
      })}
    </div>
  );
}
