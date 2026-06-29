"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { Icon } from "@/components/Icon";
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
 * drops the whole transcript — nothing is
 * persisted server-side. Switching runs clears it (different runs' contexts
 * must not mix).
 *
 * Open/close is OWNED by page.tsx (same pattern as CommandPalette) so a single
 * global Esc handler arbitrates layering. This component is a pure modal: it
 * renders nothing when `open` is false.
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
const MAX_TRANSCRIPT_CHARS = 60000;

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

  // Esc closes (stopPropagation so it doesn't also close a panel beneath).
  const onKey = (e: React.KeyboardEvent) => {
    if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      onClose();
    }
  };

  const send = useCallback(
    async (question: string) => {
      const q = question.trim();
      if (!q || streaming || !runId) return;
      setError("");
      // cancel any prior in-flight stream (defensive — limiter cancels server-side too)
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
          // sse-starlette emits CRLF CRLF between frames; tolerate both.
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
          // silent — operator closed / re-asked
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

  if (!open) return null;

  // rough client-side transcript cap so the request body stays bounded
  const transcriptChars = turns.reduce((n, t) => n + t.content.length, 0);
  const overCap = transcriptChars > MAX_TRANSCRIPT_CHARS;

  return (
    <div className="modal-backdrop btw-backdrop" onClick={onClose} onKeyDown={onKey}>
      <div
        className="btw-drawer"
        role="dialog"
        aria-modal="true"
        aria-label="BTW observer"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="btw-head">
          <span className="btw-title">
            <Icon name="eye" size={15} /> {t("btw.title")}
          </span>
          <span className="btw-sub" title={runId}>{runId}</span>
          <span className="btw-spacer" />
          <button className="btw-x" onClick={onClose} aria-label="close" title="Esc">
            <Icon name="x" size={15} />
          </button>
        </div>

        <div className="btw-quick">
          {QUICK_ASKS.map((q) => (
            <button
              key={q}
              className="btw-quick-btn"
              disabled={streaming}
              onClick={() => send(q)}
              title={q}
            >
              {q}
            </button>
          ))}
        </div>

        <div className="btw-scroll" ref={scrollRef}>
          {turns.length === 0 && !streaming && (
            <div className="btw-empty">{t("btw.empty")}</div>
          )}
          {turns.map((turn, i) => (
            <div key={i} className={`btw-msg btw-${turn.role}`}>
              <div className="btw-msg-role">{turn.role === "user" ? "你" : "观察员"}</div>
              <div className="btw-msg-body">
                {turn.content || (turn.role === "assistant" && streaming ? "…" : "")}
              </div>
            </div>
          ))}
          {error && <div className="btw-error">{error}</div>}
          {overCap && (
            <div className="btw-error">transcript 过长，建议关闭抽屉重新开始。</div>
          )}
        </div>

        <div className="btw-input-row">
          <textarea
            className="btw-input"
            value={input}
            onChange={(e) => setInput(e.target.value)}
            placeholder={t("btw.placeholder")}
            disabled={streaming}
            rows={2}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !e.shiftKey) {
                e.preventDefault();
                send(input);
              }
            }}
          />
          <button
            className="btw-send"
            disabled={streaming || !input.trim()}
            onClick={() => send(input)}
            title="Enter 发送 / Shift+Enter 换行"
          >
            {streaming ? "…" : t("btw.send")}
          </button>
        </div>
      </div>
    </div>
  );
}
