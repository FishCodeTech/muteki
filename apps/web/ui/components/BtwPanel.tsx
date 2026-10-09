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
                  <p>针对当前解题的临时问答</p>
                </div>
                <div className={styles.headerActions}>
                  {turns.length > 0 && <button type="button" className={styles.iconButton} disabled={streaming} onClick={handleClear} aria-label="清空对话" title="清空对话"><Icon name="trash" size={16} /></button>}
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
            </Drawer.Header>

            <Drawer.Body className={styles.body}>
              <div ref={scrollRef} className={styles.scroll} role="log" aria-label="顺嘴问对话" aria-live="polite">
                {turns.length === 0 && !streaming ? (
                  <div className={styles.empty}>
                    <div className={styles.emptyMark} aria-hidden="true"><Icon name="sparkles" size={22} /></div>
                    <h3>有什么想快速确认的？</h3>
                    <p>询问进展、证据或下一步。回答来自临时旁路 Worker，关闭面板后不会保留。</p>
                    <div className={styles.suggestionHeading}>从这些问题开始</div>
                    <div className={styles.suggestions}>
                      {QUICK_ASKS.map((q) => <button key={q} type="button" onClick={() => void send(q)} className={styles.suggestion}><span>{q}</span><Icon name="arrowRight" size={15} /></button>)}
                    </div>
                  </div>
                ) : (
                  <div className={styles.messages}>
                    {turns.map((turn, i) => <div key={i} className={turn.role === "user" ? styles.userTurn : styles.assistantTurn}>
                      <div className={styles.turnLabel}>{turn.role === "user" ? "你" : "旁路 Worker"}{turn.role === "assistant" && streaming && i === turns.length - 1 ? <span className={styles.generating}><Spinner size="sm" />生成中</span> : null}</div>
                      <div className={turn.role === "user" ? styles.userBubble : styles.assistantBubble}>
                        {turn.role === "user" ? turn.content : turn.content ? <MessageMarkdown text={turn.content} /> : streaming && i === turns.length - 1 ? <span className={styles.pending}>正在读取当前运行状态…</span> : <span className={styles.pending}>（无回复内容）</span>}
                      </div>
                    </div>)}
                    {!streaming && <div className={styles.followUps} aria-label="快捷追问">{QUICK_ASKS.slice(0, 2).map((q) => <button key={q} type="button" onClick={() => void send(q)}>{q}</button>)}</div>}
                  </div>
                )}
                {error && <div className={styles.error} role="alert"><Icon name="alert" size={16} /><span>{error}</span></div>}
                {overCap && <div className={styles.warning} role="status">对话记录过长，请清空记录后继续提问。</div>}
              </div>
            </Drawer.Body>

            <Drawer.Footer className={styles.footer}>
              <div className={styles.composer}>
                <textarea
                  className={styles.textarea}
                  value={input}
                  onChange={(e) => setInput(e.target.value)}
                  placeholder={t("btw.placeholder") || "顺嘴问一句…"}
                  disabled={streaming}
                  rows={2}
                  onKeyDown={(e) => { if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) { e.preventDefault(); void send(input); } }}
                />
                <div className={styles.composerBottom}>
                  <span>Enter 发送 <span aria-hidden="true">·</span> Shift+Enter 换行</span>
                  {streaming ? <button type="button" className={styles.stopButton} onClick={handleStop}><Icon name="stop" size={14} />停止</button> : <button type="button" className={styles.sendButton} disabled={!input.trim() || overCap} onClick={() => void send(input)}><span>{t("btw.send") || "发送"}</span><Icon name="send" size={14} /></button>}
                </div>
              </div>
            </Drawer.Footer>
          </Drawer.Dialog>
        </Drawer.Content>
    </Drawer.Backdrop>
  );
}
