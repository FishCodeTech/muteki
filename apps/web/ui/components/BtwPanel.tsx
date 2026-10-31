"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import {
  Drawer,
  Spinner,
} from "@heroui/react";
import { Icon } from "@/components/Icon";
import { MessageMarkdown } from "@/components/ai-native/message-markdown";
import { useT } from "@/lib/i18n";
import { apiFetch } from "@/lib/useRun";
import styles from "./BtwPanel.module.css";
import { FollowupWorkerSelect } from "./FollowupWorkerSelect";

/**
 * BTW side-query worker — a right-side drawer for read-only Q&A over a run.
 *
 * The operator asks quick questions ("summarize progress", "which worker is on
 * which line", ...) and gets a streamed answer from a one-shot side worker. It
 * never joins the swarm, consumes no max-worker slot, and writes no graph/cost
 * state; the worker process exits after the turn or an explicit stop.
 *
 * History is stored per run by the server. Only retained completed turns are
 * used as model context. Closing the drawer detaches the observer; background
 * execution continues and history polling restores progress on return.
 */

export interface BtwPanelProps {
  open: boolean;
  onClose: () => void;
  runId: string;
}

type Turn = {
  role: "user" | "assistant"; content: string; id?: string;
  created_at?: number; status?: string; engine?: string; model?: string;
};
type HistoryTurn = { id: string; question: string; answer: string; status: string;
  created_at: number; engine: string; model: string };

const QUICK_ASKS = [
  "总结当前进展",
  "当前有哪些 open intents?",
  "走过但失败的 dead-end 方向有哪些?",
  "目前有几条候选证据? 已验证几条?",
];

export function BtwPanel({ open, onClose, runId }: BtwPanelProps) {
  const t = useT();
  const [turns, setTurns] = useState<Turn[]>([]);
  const [input, setInput] = useState("");
  const [workerId, setWorkerId] = useState("");
  const [streaming, setStreaming] = useState(false);
  const [error, setError] = useState<string>("");
  const [progress, setProgress] = useState("正在启动旁路 Worker…");
  const abortRef = useRef<AbortController | null>(null);
  const scrollRef = useRef<HTMLDivElement | null>(null);
  const [historyLoading, setHistoryLoading] = useState(true);
  const [deleting, setDeleting] = useState(false);
  const historyAbortRef = useRef<AbortController | null>(null);
  const currentRunRef = useRef(runId);
  currentRunRef.current = runId;
  const openRef = useRef(open);
  openRef.current = open;

  const loadHistory = useCallback(async () => {
    historyAbortRef.current?.abort();
    const ctrl = new AbortController();
    historyAbortRef.current = ctrl;
    setHistoryLoading(true);
    try {
      const response = await apiFetch(`/api/runs/${runId}/btw/history`, { signal: ctrl.signal });
      if (!response.ok) throw new Error(`读取 BTW 历史失败 (${response.status})`);
      const data = await response.json();
      if (ctrl.signal.aborted || currentRunRef.current !== runId) return;
      if (!abortRef.current) {
        setStreaming(Boolean(data.active));
        if (data.active) {
          const phase = data.progress?.phase;
          setProgress(phase === "cleanup" ? "正在确认 Worker 退出并清理资源…" : phase === "tool" ? `正在执行工具（第 ${Number(data.progress.tool_count) || 1} 次）…`
            : phase === "tool_result" ? "工具已返回，正在整理回答…"
            : "BTW 正在后台运行，切换页面不会中断…");
        }
      }
      setTurns((data.turns as HistoryTurn[]).flatMap((row) => [
        { ...row, role: "user" as const, content: row.question },
        { ...row, role: "assistant" as const, content: row.answer },
      ]));
    } catch (e: any) {
      if (!ctrl.signal.aborted && currentRunRef.current === runId) setError(String(e.message || e));
    } finally {
      if (historyAbortRef.current === ctrl) {
        historyAbortRef.current = null;
        setHistoryLoading(false);
      }
    }
  }, [runId]);

  useEffect(() => {
    if (open) void loadHistory();
    return () => {
      historyAbortRef.current?.abort();
      abortRef.current?.abort();
      abortRef.current = null;
      setStreaming(false);
    };
  }, [open, runId, loadHistory]);


  // Observe an existing server job after navigating back; no second CLI launch.
  useEffect(() => {
    if (!open || !streaming) return;
    const timer = window.setInterval(() => {
      if (!abortRef.current && !historyAbortRef.current) void loadHistory();
    }, 1500);
    return () => window.clearInterval(timer);
  }, [open, streaming, loadHistory]);

  // Switching runs clears the transcript — never mix contexts across runs.
  useEffect(() => {
    setTurns([]);
    setInput("");
    setError("");
  }, [runId]);

  // Auto-scroll the conversation to the bottom as deltas stream in.
  useEffect(() => {
    if (scrollRef.current) {
      scrollRef.current.scrollTop = turns.length || streaming ? scrollRef.current.scrollHeight : 0;
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

  const deleteHistory = useCallback(async (id?: string) => {
    if (streaming || historyLoading || deleting) return;
    if (!window.confirm(id ? "删除这条 BTW 问答？删除后不会用于后续对话。" : "清空当前任务的全部 BTW 历史？此操作无法撤销。")) return;
    setDeleting(true);
    setError("");
    try {
      const response = await apiFetch(`/api/runs/${runId}/btw/history${id ? `/${id}` : ""}`, { method: "DELETE" });
      if (!response.ok) throw new Error(`删除失败 (${response.status})`);
      if (currentRunRef.current === runId) await loadHistory();
    } catch (e: any) {
      if (currentRunRef.current === runId) setError(String(e.message || e));
    } finally {
      setDeleting(false);
    }
  }, [runId, streaming, historyLoading, deleting, loadHistory]);

  const handleStop = useCallback(async () => {
    setProgress("正在停止…");
    try {
      const response = await apiFetch(`/api/runs/${runId}/btw/cancel`, { method: "POST" });
      if (!response.ok) throw new Error(`停止失败 (${response.status})`);
      if (currentRunRef.current === runId) {
        abortRef.current?.abort();
        if (!abortRef.current) await loadHistory();
      }
    } catch (e: any) {
      if (currentRunRef.current === runId) setError(String(e.message || e));
    }
  }, [runId, loadHistory]);

  const send = useCallback(
    async (question: string) => {
      const q = question.trim();
      if (!q || streaming || historyLoading || deleting || !runId || !workerId) return;
      setError("");
      // cancel any prior in-flight stream
      if (abortRef.current) abortRef.current.abort();
      const ctrl = new AbortController();
      abortRef.current = ctrl;

      setTurns((prev) => [
        ...prev,
        { role: "user", content: q },
        { role: "assistant", content: "" },
      ]);
      setInput("");
      setProgress("正在启动旁路 Worker…");
      setStreaming(true);

      try {
        const resp = await apiFetch(`/api/runs/${runId}/btw`, {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ question: q, worker_id: workerId }),
          signal: ctrl.signal,
        });
        if (abortRef.current !== ctrl || currentRunRef.current !== runId) return;
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
        streamLoop: for (;;) {
          const { done, value } = await reader.read();
          if (done || abortRef.current !== ctrl || currentRunRef.current !== runId) break;
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
            if (obj.history_id) {
              setTurns((prev) => prev.map((turn, i) => i >= prev.length - 2
                ? { ...turn, id: obj.history_id, created_at: Date.now() / 1000 } : turn));
            }
            if (obj.progress) {
              const phase = obj.progress.phase;
              const labels: Record<string, string> = {
                preparing: "正在准备旁路 Worker…", starting: "正在启动旁路 Worker…",
                running: "Worker 已启动，正在等待响应…",
                tool: `正在执行工具（第 ${Number(obj.progress.tool_count) || 1} 次）…`,
                tool_result: "工具已返回，等待 Worker 后续响应…",
                waiting: `Worker 仍在运行，已等待 ${Number(obj.progress.elapsed_seconds) || 0} 秒…`,
              };
              setProgress(labels[phase] || "正在处理…");
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
            if (typeof obj.answer === "string") {
              acc = obj.answer;
              const answer = obj.answer;
              setTurns((prev) => {
                const next = prev.slice();
                if (next[next.length - 1]?.role === "assistant") {
                  next[next.length - 1] = { role: "assistant", content: answer };
                }
                return next;
              });
            }
            if (obj.error) {
              setError(String(obj.error).slice(0, 300));
            }
            if (obj.done) {
              await reader.cancel();
              break streamLoop;
            }
          }
        }
      } catch (e: any) {
        if (e?.name === "AbortError") {
          // silent — operator closed / aborted
        } else if (abortRef.current === ctrl && currentRunRef.current === runId) {
          setError(String(e?.message || e).slice(0, 300));
        }
      } finally {
        if (abortRef.current === ctrl) {
          setStreaming(false);
          abortRef.current = null;
          if (openRef.current && currentRunRef.current === runId) await loadHistory();
        }
      }
    },
    [runId, workerId, streaming, historyLoading, deleting, loadHistory],
  );

  const userTurnCount = turns.filter((t) => t.role === "user").length;

  return (
    <Drawer.Backdrop
      isOpen={open}
      onOpenChange={(isOpen) => { if (!isOpen) onClose(); }}
      variant="blur"
      className="!z-[150] bg-black/40 backdrop-blur-sm"
    >
        <Drawer.Content placement="right" className="!z-[150]">
          <Drawer.Dialog aria-label={t("btw.title")} className={styles.dialog}>
            <Drawer.Header className={styles.header}>
              <div className={styles.headerMain}>
                <div className={styles.mark} aria-hidden="true"><Icon name="sparkles" size={18} /></div>
                <div className={styles.heading}>
                  <h2>顺嘴问</h2>
                  <p>切换页面仍会继续，返回后查看结果</p>
                </div>
                <div className={styles.headerActions}>
                  <button type="button" className={styles.iconButton} disabled={streaming || historyLoading || deleting} onClick={() => { setError(""); void loadHistory(); }} aria-label="刷新 BTW 历史" title="刷新历史"><Icon name="refresh" size={16} /></button>
                  {turns.length > 0 && <button type="button" className={styles.iconButton} disabled={streaming || historyLoading || deleting} onClick={() => void deleteHistory()} aria-label="清空 BTW 历史" title="清空 BTW 历史"><Icon name="trash" size={16} /></button>}
                  <button type="button" className={styles.iconButton} onClick={onClose} aria-label="关闭" title="关闭"><Icon name="x" size={17} /></button>
                </div>
              </div>
              <div className={styles.contextRow}>
                <span className={styles.contextDot} aria-hidden="true" />
                <span className={styles.contextRun} title={runId}>{runId}</span>
                <span className={styles.contextDivider} aria-hidden="true" />
                <span>只读旁路</span>
                <span className={styles.turnCount}>{userTurnCount} 轮</span>
              </div>
              {open && <FollowupWorkerSelect runId={runId} value={workerId} onChange={setWorkerId} disabled={streaming} />}
            </Drawer.Header>

            <Drawer.Body className={styles.body}>
              <div ref={scrollRef} className={styles.scroll} role="log" aria-label="顺嘴问对话" aria-live="polite">
                {historyLoading && turns.length === 0 ? <div role="status" className={styles.pending}>正在读取 BTW 历史…</div> : turns.length === 0 && !streaming ? (
                  <div className={styles.empty}>
                    <div className={styles.emptyMark} aria-hidden="true"><Icon name="sparkles" size={22} /></div>
                    <h3>有什么想快速确认的？</h3>
                    <p>询问进展、证据或下一步。问答按任务保存。关闭面板不会中断，点击停止才会取消。</p>
                    <div className={styles.suggestionHeading}>从这些问题开始</div>
                    <div className={styles.suggestions}>
                      {QUICK_ASKS.map((q) => <button key={q} type="button" onClick={() => void send(q)} className={styles.suggestion}><span>{q}</span><Icon name="arrowRight" size={15} /></button>)}
                    </div>
                  </div>
                ) : (
                  <div className={styles.messages}>
                    {turns.map((turn, i) => <div key={i} className={turn.role === "user" ? styles.userTurn : styles.assistantTurn}>
                      <div className={styles.turnLabel}>{turn.role === "user" ? "你" : "旁路 Worker"}
                        {turn.role === "user" && turn.created_at ? <time dateTime={new Date(turn.created_at * 1000).toISOString()}>{new Date(turn.created_at * 1000).toLocaleString()}</time> : null}
                        {turn.role === "user" && turn.id ? <button type="button" className={styles.deleteTurn} disabled={streaming || historyLoading || deleting} onClick={() => void deleteHistory(turn.id)} aria-label="删除这条问答" title="删除这条问答"><Icon name="trash" size={13} /></button> : null}
                        {turn.role === "assistant" && turn.engine ? <span>{turn.engine}{turn.model ? ` · ${turn.model}` : ""}</span> : null}
                        {turn.role === "assistant" && !streaming && turn.status && turn.status !== "completed" ? <span>{turn.status === "failed" ? "失败" : turn.status === "cancelled" ? "已停止" : "未完成"}</span> : null}
{turn.role === "assistant" && streaming && i === turns.length - 1 ? <span className={styles.generating}><Spinner size="sm" />生成中</span> : null}</div>
                      <div className={turn.role === "user" ? styles.userBubble : styles.assistantBubble}>
                        {turn.role === "user" ? turn.content : turn.content ? <MessageMarkdown text={turn.content} /> : streaming && i === turns.length - 1 ? <span className={styles.pending}>{progress}</span> : <span className={styles.pending}>（无回复内容）</span>}
                      </div>
                    </div>)}
                    {!streaming && <div className={styles.followUps} aria-label="快捷追问">{QUICK_ASKS.slice(0, 2).map((q) => <button key={q} type="button" onClick={() => void send(q)}>{q}</button>)}</div>}
                  </div>
                )}
                {streaming && <div className={styles.pending} role="status" aria-live="polite">{progress}</div>}
                {error && <div className={styles.error} role="alert"><Icon name="alert" size={16} /><span>{error}</span></div>}

              </div>
            </Drawer.Body>

            <Drawer.Footer className={styles.footer}>
              <div className={styles.composer}>
                <textarea
                  className={styles.textarea}
                  value={input}
                  onChange={(e) => setInput(e.target.value)}
                  placeholder={t("btw.placeholder") || "顺嘴问一句…"}
                  disabled={streaming || historyLoading || deleting}
                  rows={2}
                  onKeyDown={(e) => { if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) { e.preventDefault(); void send(input); } }}
                />
                <div className={styles.composerBottom}>
                  <span>Enter 发送 <span aria-hidden="true">·</span> Shift+Enter 换行</span>
                  {streaming ? <button type="button" className={styles.stopButton} onClick={handleStop}><Icon name="stop" size={14} />停止</button> : <button type="button" className={styles.sendButton} disabled={!input.trim() || !workerId || historyLoading || deleting} onClick={() => void send(input)}><span>{t("btw.send") || "发送"}</span><Icon name="send" size={14} /></button>}
                </div>
              </div>
            </Drawer.Footer>
          </Drawer.Dialog>
        </Drawer.Content>
    </Drawer.Backdrop>
  );
}
