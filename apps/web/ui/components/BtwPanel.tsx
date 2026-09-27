"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import {
  Button,
  Card,
  Chip,
  Drawer,
  Kbd,
  ScrollShadow,
  Spinner,
  TextArea,
  Tooltip,
} from "@heroui/react";
import { Icon } from "@/components/Icon";
import { MessageMarkdown } from "@/components/ai-native/message-markdown";
import { useT } from "@/lib/i18n";
import { apiFetch } from "@/lib/useRun";

/**
 * BTW side-query worker — a right-side drawer for read-only Q&A over a run.
 *
 * The operator asks quick questions ("summarize progress", "which worker is on
 * which line", ...) and gets a streamed answer from a one-shot side worker. It
 * never joins the swarm, consumes no max-worker slot, and writes no graph/cost
 * state; the worker process dies after the turn or on disconnect.
 *
 * Multi-turn: the transcript lives ONLY in this component's local state. It is
 * sent with each request so every turn can cold-start a fresh worker without
 * losing conversational context. Closing the drawer (Esc / backdrop / button)
 * drops the whole transcript — nothing is persisted server-side. Switching
 * runs clears it (different runs' contexts must not mix).
 */

export interface BtwPanelProps {
  open: boolean;
  onClose: () => void;
  runId: string;
}

type Turn = { role: "user" | "assistant"; content: string };

const QUICK_ASKS = [
  "总结当前进展",
  "当前有哪些 open intents?",
  "走过但失败的 dead-end 方向有哪些?",
  "目前有几条候选证据? 已验证几条?",
];

// Rough transcript cap (chars). Server also caps; this is the client line of
// defense so a long conversation doesn't balloon the request body.
const MAX_TRANSCRIPT_CHARS = 200000;

export function BtwPanel({ open, onClose, runId }: BtwPanelProps) {
  const t = useT();
  const [turns, setTurns] = useState<Turn[]>([]);
  const [input, setInput] = useState("");
  const [streaming, setStreaming] = useState(false);
  const [error, setError] = useState<string>("");
  const abortRef = useRef<AbortController | null>(null);
  const scrollRef = useRef<HTMLDivElement | null>(null);

  // Switching runs clears the transcript — never mix contexts across runs.
  useEffect(() => {
    setTurns([]);
    setInput("");
    setError("");
  }, [runId]);

  // Auto-scroll the conversation to the bottom as deltas stream in.
  useEffect(() => {
    if (scrollRef.current) {
      scrollRef.current.scrollTop = scrollRef.current.scrollHeight;
    }
  }, [turns, streaming]);

  // Abort any in-flight stream when the drawer closes.
  useEffect(() => {
    if (!open && abortRef.current) {
      abortRef.current.abort();
      abortRef.current = null;
      setStreaming(false);
    }
  }, [open]);

  const handleClear = useCallback(() => {
    if (streaming && abortRef.current) {
      abortRef.current.abort();
      abortRef.current = null;
    }
    setTurns([]);
    setInput("");
    setError("");
    setStreaming(false);
  }, [streaming]);

  const handleStop = useCallback(() => {
    if (abortRef.current) {
      abortRef.current.abort();
      abortRef.current = null;
    }
    setStreaming(false);
  }, []);

  const send = useCallback(
    async (question: string) => {
      const q = question.trim();
      if (!q || streaming || !runId) return;
      setError("");
      // cancel any prior in-flight stream
      if (abortRef.current) abortRef.current.abort();
      const ctrl = new AbortController();
      abortRef.current = ctrl;

      const transcript = turns;
      setTurns((prev) => [
        ...prev,
        { role: "user", content: q },
        { role: "assistant", content: "" },
      ]);
      setInput("");
      setStreaming(true);

      try {
        const resp = await apiFetch(`/api/runs/${runId}/btw`, {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ question: q, transcript }),
          signal: ctrl.signal,
        });
        if (!resp.ok || !resp.body) {
          const txt = await resp.text().catch(() => "");
          setError(`请求失败 (${resp.status}): ${txt.slice(0, 160)}`);
          setStreaming(false);
          return;
        }
        const reader = resp.body.getReader();
        const dec = new TextDecoder();
        let buf = "";
        let acc = "";
        for (;;) {
          const { done, value } = await reader.read();
          if (done) break;
          buf += dec.decode(value, { stream: true });
          const frames = buf.split(/\r?\n\r?\n/);
          buf = frames.pop() || "";
          for (const frame of frames) {
            const m = frame.match(/^data: (.+)$/s);
            if (!m) continue;
            let obj: any;
            try {
              obj = JSON.parse(m[1]);
            } catch {
              continue;
            }
            if (obj.delta) {
              acc += obj.delta;
              const captured = acc;
              setTurns((prev) => {
                const next = prev.slice();
                const last = next[next.length - 1];
                if (last && last.role === "assistant") {
                  next[next.length - 1] = { role: "assistant", content: captured };
                }
                return next;
              });
            }
            if (obj.error) {
              setError(String(obj.error).slice(0, 300));
            }
            if (obj.done) {
              // stream end marker
            }
          }
        }
      } catch (e: any) {
        if (e?.name === "AbortError") {
          // silent — operator closed / aborted
        } else {
          setError(String(e?.message || e).slice(0, 300));
        }
      } finally {
        setStreaming(false);
        if (abortRef.current === ctrl) abortRef.current = null;
      }
    },
    [runId, streaming, turns],
  );

  // rough client-side transcript cap so the request body stays bounded
  const transcriptChars = turns.reduce((n, t) => n + t.content.length, 0);
  const overCap = transcriptChars > MAX_TRANSCRIPT_CHARS;
  const userTurnCount = turns.filter((t) => t.role === "user").length;

  return (
    <Drawer
      isOpen={open}
      onOpenChange={(isOpen) => {
        if (!isOpen) onClose();
      }}
    >
      <Drawer.Backdrop variant="blur" className="!z-[150] bg-black/40 backdrop-blur-sm">
        <Drawer.Content placement="right" className="!z-[150]">
          <Drawer.Dialog
            aria-label={t("btw.title")}
            className="!p-0 flex flex-col h-full w-full sm:w-[480px] md:w-[540px] max-w-[100vw] bg-surface/95 backdrop-blur-xl border-l border-line shadow-2xl text-ink outline-none overflow-hidden"
          >
            {/* Header */}
            <Drawer.Header className="flex flex-col gap-2.5 border-b border-line bg-inset/40 px-4 py-3 shrink-0">
            <div className="flex items-center justify-between gap-3">
              <div className="flex items-center gap-2.5 min-w-0">
                <div className="flex size-8 shrink-0 items-center justify-center rounded-xl bg-accent/10 text-accent border border-accent/20 shadow-sm">
                  <Icon name="sparkles" size={16} />
                </div>
                <div className="flex flex-col min-w-0">
                  <div className="flex items-center gap-2 flex-wrap">
                    <h2 className="text-[13.5px] font-semibold text-ink leading-tight truncate">
                      {t("btw.title")}
                    </h2>
                    <Chip size="sm" variant="soft" color="accent" className="text-[10px] px-1.5 py-0 h-4 shrink-0 font-medium">
                      旁路 Worker
                    </Chip>
                  </div>
                  {runId ? (
                    <span className="text-[11px] font-mono text-ink-3 truncate max-w-[220px]" title={runId}>
                      {runId}
                    </span>
                  ) : null}
                </div>
              </div>

              <div className="flex items-center gap-1 shrink-0">
                {turns.length > 0 && (
                  <Tooltip delay={300}>
                    <Tooltip.Trigger>
                      <Button
                        size="sm"
                        variant="ghost"
                        isIconOnly
                        isDisabled={streaming}
                        onPress={handleClear}
                        aria-label="清空对话"
                        className="flex size-7 items-center justify-center rounded-control text-ink-3 hover:bg-hover hover:text-ink transition-colors"
                      >
                        <Icon name="trash" size={14} />
                      </Button>
                    </Tooltip.Trigger>
                    <Tooltip.Content>清空对话记录</Tooltip.Content>
                  </Tooltip>
                )}

                <Button
                  size="sm"
                  variant="ghost"
                  isIconOnly
                  onPress={onClose}
                  aria-label="关闭"
                  className="flex size-7 items-center justify-center rounded-control text-ink-3 hover:bg-hover hover:text-ink transition-colors cursor-pointer"
                >
                  <Icon name="x" size={15} />
                </Button>
              </div>
            </div>

            <div className="flex items-center justify-between text-[11px] text-ink-3 bg-inset/60 px-2.5 py-1 rounded-control border border-line/60 select-none">
              <div className="flex items-center gap-1.5 truncate">
                <span className="size-1.5 rounded-full bg-green animate-pulse shrink-0" />
                <span className="truncate">只读旁路问答 · 不占用 Worker 槽位 · 关闭即释放</span>
              </div>
              <span className="font-mono text-[10px] opacity-75 shrink-0 ml-2">
                {userTurnCount} 轮
              </span>
            </div>
          </Drawer.Header>

          {/* Quick suggestions bar if conversation already has turns */}
          {turns.length > 0 && (
            <div className="flex items-center gap-1.5 px-4 py-2 border-b border-line/50 bg-inset/20 overflow-x-auto scrollbar-none shrink-0">
              <div className="flex items-center gap-1 text-[11px] font-medium text-ink-3 shrink-0 mr-1">
                <Icon name="sparkles" size={12} className="text-accent" />
                <span>快捷提问:</span>
              </div>
              <div className="flex items-center gap-1.5 flex-wrap sm:flex-nowrap">
                {QUICK_ASKS.map((q) => (
                  <Button
                    key={q}
                    size="sm"
                    variant="secondary"
                    isDisabled={streaming}
                    onPress={() => send(q)}
                    className="text-[11px] px-2.5 py-0.5 h-6 rounded-full border border-line hover:border-accent/40 hover:bg-accent/5 text-ink-2 whitespace-nowrap shrink-0 transition-colors"
                  >
                    {q}
                  </Button>
                ))}
              </div>
            </div>
          )}

          {/* Body: message stream */}
          <Drawer.Body className="flex-1 flex flex-col min-h-0 p-0 overflow-hidden bg-canvas/30">
            <ScrollShadow
              ref={scrollRef}
              className="flex-1 overflow-y-auto px-4 py-4 space-y-4"
              size={20}
            >
              {turns.length === 0 && !streaming && (
                <div className="flex flex-col items-center justify-center text-center px-4 py-8 my-auto">
                  <div className="size-12 rounded-2xl bg-accent/10 border border-accent/20 flex items-center justify-center text-accent mb-3 shadow-sm">
                    <Icon name="sparkles" size={22} />
                  </div>
                  <h3 className="text-[14px] font-semibold text-ink mb-1">
                    {t("btw.title")}
                  </h3>
                  <p className="text-[12px] text-ink-3 max-w-xs leading-relaxed mb-6">
                    随时向旁路观察员提问当前任务进展、已获证据或已排除死路。问答在隔离进程运行，不占用主解题槽位，关闭抽屉后自动释放。
                  </p>

                  <div className="w-full max-w-sm space-y-2 text-left">
                    <div className="text-[11px] font-medium text-ink-3 px-1 uppercase tracking-wider flex items-center gap-1.5">
                      <Icon name="sparkles" size={12} className="text-accent" />
                      <span>快捷提问建议</span>
                    </div>
                    <div className="grid grid-cols-1 gap-2">
                      {QUICK_ASKS.map((q) => (
                        <Card
                          key={q}
                          variant="secondary"
                          className="p-3 rounded-xl border border-line hover:border-accent/40 hover:bg-hover transition-all cursor-pointer group shadow-sm"
                          onClick={() => !streaming && send(q)}
                        >
                          <Card.Content className="flex items-center justify-between gap-2 p-0">
                            <span className="text-[12.5px] text-ink font-medium group-hover:text-accent transition-colors">
                              {q}
                            </span>
                            <Icon
                              name="arrowRight"
                              size={13}
                              className="text-ink-3 group-hover:text-accent group-hover:translate-x-0.5 transition-all shrink-0"
                            />
                          </Card.Content>
                        </Card>
                      ))}
                    </div>
                  </div>
                </div>
              )}

              {turns.map((turn, i) => (
                <div key={i} className="space-y-1.5">
                  {turn.role === "user" ? (
                    <div className="flex flex-col items-end gap-1 pl-8">
                      <div className="flex items-center gap-1.5 text-[11px] font-medium text-accent">
                        <span>你</span>
                      </div>
                      <div className="rounded-2xl rounded-tr-sm bg-accent/15 border border-accent/25 px-3.5 py-2.5 text-[13px] text-ink leading-relaxed shadow-sm break-words whitespace-pre-wrap max-w-full">
                        {turn.content}
                      </div>
                    </div>
                  ) : (
                    <div className="flex flex-col items-start gap-1 pr-2">
                      <div className="flex items-center gap-1.5 text-[11px] font-medium text-ink-3">
                        <div className="flex size-4 items-center justify-center rounded-full bg-accent/20 text-accent">
                          <Icon name="sparkles" size={10} />
                        </div>
                        <span>旁路观察员</span>
                        {streaming && i === turns.length - 1 && (
                          <span className="flex items-center gap-1 text-[10.5px] text-accent ml-1">
                            <Spinner size="sm" className="size-3" />
                            <span>生成中…</span>
                          </span>
                        )}
                      </div>
                      <Card
                        variant="default"
                        className="w-full rounded-2xl rounded-tl-sm border border-line bg-surface p-3.5 shadow-sm text-[13px] text-ink leading-relaxed"
                      >
                        <Card.Content className="p-0">
                          {turn.content ? (
                            <MessageMarkdown text={turn.content} />
                          ) : streaming && i === turns.length - 1 ? (
                            <div className="flex items-center gap-2 py-1 text-[12px] text-ink-3">
                              <span className="size-2 rounded-full bg-accent animate-pulse" />
                              <span>正在读取当前 Run 状态并生成分析…</span>
                            </div>
                          ) : (
                            <span className="text-ink-3 italic text-[12px]">（无回复内容）</span>
                          )}
                        </Card.Content>
                      </Card>
                    </div>
                  )}
                </div>
              ))}

              {error && (
                <Card
                  variant="secondary"
                  className="rounded-xl border border-red/30 bg-red/10 p-3 text-[12px] text-red shadow-sm flex items-start gap-2.5"
                >
                  <Card.Content className="flex items-start gap-2.5 p-0 w-full">
                    <Icon name="alert" size={16} className="shrink-0 mt-0.5" />
                    <div className="flex-1 min-w-0">
                      <div className="font-semibold mb-0.5">请求出错</div>
                      <div className="break-words leading-relaxed">{error}</div>
                    </div>
                  </Card.Content>
                </Card>
              )}

              {overCap && (
                <Card
                  variant="secondary"
                  className="rounded-xl border border-amber/30 bg-amber/10 p-3 text-[12px] text-amber shadow-sm flex items-start gap-2.5"
                >
                  <Card.Content className="flex items-start gap-2.5 p-0 w-full">
                    <Icon name="alert" size={16} className="shrink-0 mt-0.5" />
                    <div className="flex-1 min-w-0 leading-relaxed">
                      对话记录过长，建议点击右上角清空记录或关闭抽屉重新开始。
                    </div>
                  </Card.Content>
                </Card>
              )}
            </ScrollShadow>
          </Drawer.Body>

          {/* Footer: Modern Composer */}
          <Drawer.Footer className="flex flex-col gap-2 border-t border-line bg-surface/95 backdrop-blur-sm p-3.5 shrink-0">
            <div className="relative flex flex-col rounded-xl border border-line bg-field/60 focus-within:border-accent focus-within:ring-2 focus-within:ring-accent/20 focus-within:bg-surface transition-all p-2.5 shadow-sm">
              <TextArea
                className="w-full resize-none border-0 bg-transparent p-0 text-[13px] leading-relaxed text-ink placeholder:text-ink-3 focus:outline-none focus:ring-0 shadow-none min-h-[44px] max-h-[140px]"
                value={input}
                onChange={(e) => setInput(e.target.value)}
                placeholder={t("btw.placeholder") || "顺嘴问一句… (Enter 发送)"}
                disabled={streaming}
                rows={2}
                onKeyDown={(e) => {
                  if (e.key === "Enter" && !e.shiftKey) {
                    e.preventDefault();
                    send(input);
                  }
                }}
              />

              <div className="flex items-center justify-between pt-2 border-t border-line/40 mt-1.5">
                <div className="flex items-center gap-1.5 text-[10.5px] text-ink-3 select-none">
                  <Kbd variant="default" className="px-1 py-0.5 text-[9.5px]">Enter</Kbd>
                  <span>发送</span>
                  <span className="opacity-40">·</span>
                  <Kbd variant="default" className="px-1 py-0.5 text-[9.5px]">Shift+Enter</Kbd>
                  <span>换行</span>
                </div>

                <div className="flex items-center gap-2">
                  {streaming ? (
                    <Button
                      size="sm"
                      variant="danger-soft"
                      className="h-7 px-2.5 text-[11.5px] rounded-control font-medium flex items-center gap-1"
                      onPress={handleStop}
                    >
                      <Icon name="stop" size={12} />
                      <span>停止</span>
                    </Button>
                  ) : (
                    <Button
                      size="sm"
                      variant="primary"
                      isDisabled={!input.trim()}
                      onPress={() => send(input)}
                      className="h-7 px-3 text-[11.5px] rounded-control font-medium flex items-center gap-1.5 shadow-sm"
                    >
                      <span>{t("btw.send") || "发送"}</span>
                      <Icon name="send" size={12} />
                    </Button>
                  )}
                </div>
              </div>
            </div>
          </Drawer.Footer>
        </Drawer.Dialog>
      </Drawer.Content>
      </Drawer.Backdrop>
    </Drawer>
  );
}
