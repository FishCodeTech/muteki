"use client";

/**
 * C32 — ExportDialog
 *
 * Exports a conversation thread as Markdown or JSONL. Shows a snapshot
 * preview (turn / message / artifact counts) and redaction options before
 * the download. A separate dialog creates scoped, expiring read-only shares.
 */

import React, { useEffect, useRef, useState } from "react";
import { cn } from "@/lib/cn";
import { Icon, type IconName } from "@/components/Icon";
import { Button, Callout, Checkbox, Dialog, toast } from "@/components/chat/ui";
import type { ConversationView } from "@/lib/useConversation";
import { apiFetch, currentAuthScope } from "@/lib/useRun";
import { conversationExportFilename, conversationExportMetadata } from "@/lib/conversationExport";
import { ShareDialog } from "./ShareDialog";

export interface ExportDialogProps {
  open: boolean;
  onClose: () => void;
  view: ConversationView;
}

type ExportFormat = "markdown" | "jsonl";

const FORMATS: Array<{ value: ExportFormat; label: string; ext: string; icon: IconName; description: string }> = [
  {
    value: "markdown",
    label: "Markdown",
    ext: ".md",
    icon: "book",
    description: "当前分支的阅读快照，含完整正文、工具摘要和附件关联索引。",
  },
  {
    value: "jsonl",
    label: "JSONL",
    ext: ".jsonl",
    icon: "braces",
    description: "JSONL v2：完整消息、轮次、工具与来源关联。二进制附件另行下载。",
  },
];

export function ExportDialog({ open, onClose, view }: ExportDialogProps) {
  const [shareOpen, setShareOpen] = useState(false);
  const [format, setFormat] = useState<ExportFormat>("markdown");
  const [excludeTools, setExcludeTools] = useState(false);
  const [excludePaths, setExcludePaths] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");

  const stats = view.statistics;
  const threadId = view.thread.thread_id;
  const title = view.thread.title || "conversation";
  const msgCount = view.messages.length;
  const turnCount = stats?.turn_count ?? view.turns.length;
  const artifactCount = view.artifacts?.length ?? 0;
  const watermark = view.watermark;
  const controller = useRef<AbortController | null>(null);
  const current = useRef({ threadId, open }); current.current = { threadId, open };
  useEffect(() => { setError(""); setBusy(false); setShareOpen(false); return () => { controller.current?.abort(); controller.current = null; }; }, [threadId, open]);

  const handleDownload = async () => {
    if (controller.current) return;
    const request = new AbortController(); controller.current = request;
    const authScope = currentAuthScope();
    setError("");
    setBusy(true);
    try {
      const params = new URLSearchParams({
        format,
        exclude_tools: excludeTools ? "1" : "0",
        exclude_paths: excludePaths ? "1" : "0",
      });
      // apiFetch carries Bearer/ticket auth when MUTEKI_WEB_PASSWORD is set; raw fetch omits it.
      const res = await apiFetch(
        `/api/threads/${encodeURIComponent(threadId)}/export?${params}`,
        { signal: request.signal },
      );
      if (!res.ok) {
        const body = await res.json().catch(() => ({})) as Record<string, unknown>;
        const err = body.error as Record<string, unknown> | undefined;
        throw new Error((err?.message as string) || `导出失败（HTTP ${res.status}）`);
      }
      const metadata = conversationExportMetadata(res.headers);
      if (excludePaths && metadata.path_redaction !== "lexical_best_effort") throw new Error("服务未确认路径脱敏，请重试并检查服务版本。");
      const blob = await res.blob();
      if (!blob.size) throw new Error("导出文件为空，未保存。");
      if (request.signal.aborted || current.current.threadId !== threadId || !current.current.open || currentAuthScope() !== authScope) throw new Error("会话或服务范围已改变，旧导出未保存。");
      const filename = conversationExportFilename({ contentDisposition: res.headers.get("Content-Disposition"), title, format, excludePaths, date: new Date() });
      const objUrl = URL.createObjectURL(blob);
      const a = document.createElement("a");
      a.href = objUrl;
      a.download = filename;
      a.click();
      window.setTimeout(() => URL.revokeObjectURL(objUrl), 60_000);
      toast({ title: `已导出当前分支完整快照：${metadata.message_count} 条消息、${metadata.turn_count} 轮、${metadata.tool_count} 条工具记录，水位 #${metadata.watermark}`, tone: "success", icon: "download" });
      onClose();
    } catch (exc) {
      if (current.current.threadId === threadId && current.current.open && !request.signal.aborted) setError(exc instanceof Error ? exc.message : "导出失败");
    } finally {
      setBusy(false);
      if (controller.current === request) controller.current = null;
    }
  };

  const counts: Array<[string, string]> = [
    ["轮次", String(turnCount)],
    ["已加载消息", String(msgCount)],
    ["已加载附件", String(artifactCount)],
    ["当前视图水位", `#${watermark}`],
  ];

  return (
    <>
    <Dialog
      open={open && !shareOpen}
      onOpenChange={(next) => { if (!next && !busy) onClose(); }}
      size="md"
      icon="download"
      tone="accent"
      title="导出会话"
      description="导出当前分支的完整历史。服务在请求时生成一致快照；以下是当前加载视图，完整数量和水位会在下载响应中校验。"
      dismissable={!busy}
      testId="conversation-export-dialog"
      footer={(
        <>
          <Button className="mr-auto" variant="ghost" disabled={busy} onClick={() => setShareOpen(true)}>分享只读快照</Button>
          <Button variant="ghost" onClick={onClose} disabled={busy}>取消</Button>
          <Button
            variant="primary"
            icon="download"
            loading={busy}
            onClick={() => void handleDownload()}
            data-testid="conversation-export-confirm"
          >
            {busy ? "导出中…" : "导出"}
          </Button>
        </>
      )}
    >
      <div className="flex flex-col gap-5 pb-2">
        <dl className="grid grid-cols-4 overflow-hidden rounded-xl border border-cx-border-subtle bg-cx-bg-subtle">
          {counts.map(([label, value]) => (
            <div key={label} className="flex flex-col gap-0.5 border-r border-cx-border-subtle px-3 py-2.5 last:border-r-0">
              <dt className="text-[11.5px] text-cx-fg-4">{label}</dt>
              <dd className="cx-tabular truncate font-cx-mono text-[14px] font-semibold text-cx-fg">{value}</dd>
            </div>
          ))}
        </dl>

        <section className="flex flex-col gap-2">
          <h3 className="text-[12.5px] font-semibold text-cx-fg-2">导出格式</h3>
          <div role="radiogroup" aria-label="导出格式" className="grid grid-cols-2 gap-2">
            {FORMATS.map((option) => {
              const selected = option.value === format;
              return (
                <button
                  key={option.value}
                  type="button"
                  role="radio"
                  aria-checked={selected}
                  disabled={busy}
                  onClick={() => setFormat(option.value)}
                  onKeyDown={(event) => {
                    if (!["ArrowLeft", "ArrowRight", "ArrowUp", "ArrowDown"].includes(event.key)) return;
                    event.preventDefault();
                    const index = FORMATS.findIndex((item) => item.value === format);
                    const step = event.key === "ArrowLeft" || event.key === "ArrowUp" ? -1 : 1;
                    const next = FORMATS[(index + step + FORMATS.length) % FORMATS.length];
                    setFormat(next.value);
                    (event.currentTarget.parentElement?.querySelector<HTMLElement>(`[data-format="${next.value}"]`))?.focus();
                  }}
                  tabIndex={selected ? 0 : -1}
                  data-format={option.value}
                  className={cn(
                    "cx-export-format group relative flex flex-col items-start gap-2 rounded-xl border px-3 py-3 text-left transition-[border-color,background-color,box-shadow] duration-150",
                    selected
                      ? "border-cx-border-strong bg-cx-selected"
                      : "border-cx-border bg-cx-elevated hover:border-cx-border-strong hover:bg-cx-hover",
                  )}
                >
                  <span className="flex w-full items-center gap-2">
                    <span
                      className={cn(
                        "grid size-7 place-items-center rounded-lg",
                        selected ? "bg-cx-fg text-cx-bg" : "bg-cx-hover text-cx-fg-2",
                      )}
                    >
                      <Icon name={option.icon} size={14} />
                    </span>
                    <span className="min-w-0 flex-1 truncate text-[13.5px] font-semibold text-cx-fg">
                      {option.label}
                      <span className="ml-1.5 font-cx-mono text-[11.5px] font-normal text-cx-fg-4">{option.ext}</span>
                    </span>
                    <span
                      aria-hidden
                      className={cn(
                        "grid size-4 shrink-0 place-items-center rounded-full border transition-colors",
                        selected ? "border-cx-fg bg-cx-fg text-cx-bg" : "border-cx-border-strong",
                      )}
                    >
                      {selected ? <Icon name="check" size={10} /> : null}
                    </span>
                  </span>
                  <span className="text-[12px] leading-[18px] text-cx-fg-3">{option.description}</span>
                </button>
              );
            })}
          </div>
        </section>

        <section className="flex flex-col gap-3">
          <h3 className="text-[12.5px] font-semibold text-cx-fg-2">脱敏选项</h3>
          <Checkbox
            checked={excludeTools}
            disabled={busy}
            onCheckedChange={setExcludeTools}
            label="排除工具调用参数"
            description="保留工具名称、结果摘要与来源事件；移除调用参数。"
          />
          <Checkbox
            checked={excludePaths}
            disabled={busy}
            onCheckedChange={setExcludePaths}
            label="脱敏绝对路径"
            description="按词法规则尽力替换路径；存在歧义，分享前请检查内容。"
          />
        </section>
        <p className="text-[12px] text-cx-fg-4">完整范围指当前分支的消息与关联记录。二进制附件仅保留名称、哈希和来源索引，需另行下载。</p>

        {error ? <Callout tone="danger" role="alert" title="导出失败">{error}</Callout> : null}
      </div>
    </Dialog>
    <ShareDialog open={open && shareOpen} onClose={() => setShareOpen(false)} threadId={threadId} />
    </>
  );
}
