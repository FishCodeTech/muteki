"use client";

/**
 * C16 interactive PTY terminal for Conversation right-dock.
 * Ticketed WebSocket + xterm.js (client-only dynamic import — xterm touches `self`).
 * Cite selection → composer with provenance.
 */

import React, { useCallback, useEffect, useRef, useState } from "react";
import { cn } from "@/lib/cn";
import { Button, Callout, EmptyState, IconButton, Spinner, StatusDot } from "@/components/chat/ui";
import { API, apiFetch, authTicket } from "@/lib/useRun";
import { acceptTerminalAttachAttempt } from "@/lib/terminalAttachGeneration";
import { terminalWebSocketUrl } from "@/lib/terminalWebSocketUrl";
import { chatPanel } from "@/lib/chatPanelStore";
import { extractLocalUrls, isLocalPreviewUrl } from "@/lib/previewUrlDetect";

export type TerminalSessionInfo = {
  session_id: string;
  thread_id: string;
  workspace_id: string;
  root_path: string;
  cwd_label?: string;
  name: string;
  status: string;
  end_reason?: string | null;
  end_code?: string | null;
  cols: number;
  rows: number;
  history_bytes: number;
  history_truncated: boolean;
  workspace_diverged?: boolean;
};

type Props = {
  threadId: string;
  rootPath: string;
  onCiteToComposer?: (excerpt: string) => void;
};

type XtermTerminal = {
  cols: number;
  rows: number;
  open: (el: HTMLElement) => void;
  write: (data: string) => void;
  reset: () => void;
  dispose: () => void;
  focus: () => void;
  loadAddon: (addon: { activate: (term: XtermTerminal) => void; dispose?: () => void }) => void;
  onData: (cb: (data: string) => void) => { dispose: () => void };
  onSelectionChange: (cb: () => void) => { dispose: () => void };
  getSelection: () => string;
  options: { theme?: Record<string, string>; disableStdin?: boolean };
  buffer: { active: { getLine: (y: number) => { translateToString: (trimRight?: boolean) => string } | undefined } };
  registerLinkProvider: (provider: {
    provideLinks: (
      line: number,
      callback: (links: Array<{
        range: { start: { x: number; y: number }; end: { x: number; y: number } };
        text: string;
        activate: (event: MouseEvent, text: string) => void;
      }> | undefined) => void,
    ) => void;
  }) => { dispose: () => void };
};

const LINK_RE = /\bhttps?:\/\/[^\s"'`<>)\]}]+/g;

/** Resolve a CSS color expression (vars, color-mix) into a concrete rgb() the canvas can paint. */
function resolveColor(expression: string, fallback: string): string {
  const probe = document.createElement("span");
  probe.style.color = expression;
  probe.style.display = "none";
  document.body.appendChild(probe);
  const value = getComputedStyle(probe).color;
  probe.remove();
  return value || fallback;
}

function terminalTheme(): Record<string, string> {
  const dark = document.documentElement.classList.contains("dark");
  const base = dark
    ? {
      black: "#1e2127", red: "#ef6b73", green: "#8fce7c", yellow: "#e5c07b", blue: "#61afef", magenta: "#c678dd", cyan: "#56b6c2", white: "#d7dae0",
      brightBlack: "#5c6370", brightRed: "#ff8b92", brightGreen: "#a8e08f", brightYellow: "#f0d197", brightBlue: "#82c4ff", brightMagenta: "#d99cf0", brightCyan: "#79d1dc", brightWhite: "#ffffff",
    }
    : {
      black: "#24292f", red: "#cf222e", green: "#116329", yellow: "#9a6700", blue: "#0550ae", magenta: "#8250df", cyan: "#1b7c83", white: "#6e7781",
      brightBlack: "#57606a", brightRed: "#a40e26", brightGreen: "#1a7f37", brightYellow: "#633c01", brightBlue: "#0969da", brightMagenta: "#a475f9", brightCyan: "#3192aa", brightWhite: "#8c959f",
    };
  return {
    ...base,
    background: resolveColor(dark ? "color-mix(in srgb, black 30%, var(--surface))" : "color-mix(in srgb, var(--ink) 3%, var(--surface))", dark ? "#15171b" : "#f7f8fa"),
    foreground: resolveColor("var(--ink-2)", dark ? "#d7dde8" : "#24292f"),
    cursor: resolveColor("var(--accent)", "#56779f"),
    cursorAccent: resolveColor("var(--surface)", "#ffffff"),
    selectionBackground: resolveColor("color-mix(in srgb, var(--accent) 28%, transparent)", "rgba(86,119,159,.3)"),
  };
}

async function surfaceJson<T>(path: string, init?: RequestInit): Promise<T> {
  const response = await apiFetch(path, init);
  const data = await response.json().catch(() => ({}));
  if (!response.ok) {
    const message = data?.error?.message || data?.detail || `请求失败（${response.status}）`;
    throw new Error(String(message));
  }
  return data as T;
}

function statusLabel(session: TerminalSessionInfo | null): string {
  if (!session) return "未连接";
  if (session.workspace_diverged) return "工作区已变更";
  if (session.history_truncated && session.status === "running") return "运行中 · 历史已截断";
  if (session.status === "running") return "运行中";
  if (session.end_reason) return session.end_reason;
  if (session.status === "exited") return "已退出";
  if (session.status === "killed" || session.status === "closed") return "已关闭";
  if (session.status === "idle_timeout") return "空闲超时";
  return session.status;
}

function sessionEnded(session: TerminalSessionInfo | null): boolean {
  return Boolean(session && ["exited", "killed", "closed", "idle_timeout"].includes(session.status));
}

function sessionAcceptsInput(session: TerminalSessionInfo | null): boolean {
  return session?.status === "running" && !session.workspace_diverged;
}

function encodeStdin(data: string): string {
  const bytes = new TextEncoder().encode(data);
  let binary = "";
  bytes.forEach((b) => {
    binary += String.fromCharCode(b);
  });
  return btoa(binary);
}

function decodePayload(data: string): string {
  try {
    const binary = atob(data);
    const bytes = Uint8Array.from(binary, (c) => c.charCodeAt(0));
    return new TextDecoder().decode(bytes);
  } catch {
    return data;
  }
}

export function ConversationInteractiveTerminal({
  threadId,
  rootPath,
  onCiteToComposer,
}: Props) {
  const [sessions, setSessions] = useState<TerminalSessionInfo[]>([]);
  const [activeId, setActiveId] = useState<string>("");
  const [session, setSession] = useState<TerminalSessionInfo | null>(null);
  const [error, setError] = useState("");
  const [connection, setConnection] = useState<"disconnected" | "connecting" | "connected" | "closed">("disconnected");
  const [hasSelection, setHasSelection] = useState(false);
  const [ready, setReady] = useState(false);

  const hostRef = useRef<HTMLDivElement | null>(null);
  const termRef = useRef<XtermTerminal | null>(null);
  const fitRef = useRef<{ fit: () => void; dispose?: () => void } | null>(null);
  const socketRef = useRef<WebSocket | null>(null);
  const activeIdRef = useRef(activeId);
  const sessionRef = useRef(session);
  const disposedRef = useRef(false);
  const contextGenRef = useRef(0);
  const connectionRef = useRef(connection);
  /** Bumped on each attach/unmount so overlapping authTicket awaits cannot keep two sockets. */
  const attachGenRef = useRef(0);

  useEffect(() => {
    activeIdRef.current = activeId;
  }, [activeId]);

  const updateConnection = useCallback((value: typeof connection) => {
    connectionRef.current = value;
    if (termRef.current) termRef.current.options.disableStdin = value !== "connected" || !sessionAcceptsInput(sessionRef.current);
    setConnection(value);
  }, []);

  const reportUrls = useCallback((text: string) => {
    if (!text.includes("://")) return;
    for (const url of extractLocalUrls(text)) chatPanel.reportDetectedUrl(threadId, url, "terminal");
  }, [threadId]);

  const refreshList = useCallback(async () => {
    if (!rootPath) return [];
    const context = contextGenRef.current;
    const data = await surfaceJson<{ sessions: TerminalSessionInfo[] }>(
      `/api/threads/${threadId}/workspace/terminal/sessions`,
    );
    if (context !== contextGenRef.current || disposedRef.current) return [];
    setSessions(data.sessions || []);
    return data.sessions || [];
  }, [rootPath, threadId]);

  const disconnectSocket = useCallback(() => {
    const socket = socketRef.current;
    socketRef.current = null;
    if (socket && socket.readyState <= WebSocket.OPEN) {
      try {
        socket.close();
      } catch {
        // ignore
      }
    }
  }, []);

  const selectSession = useCallback((selected: TerminalSessionInfo) => {
    sessionRef.current = selected;
    if (activeIdRef.current !== selected.session_id) {
      attachGenRef.current += 1;
      disconnectSocket();
      updateConnection("disconnected");
      activeIdRef.current = selected.session_id;
    }
    if (termRef.current) termRef.current.options.disableStdin = connectionRef.current !== "connected" || !sessionAcceptsInput(selected);
    setActiveId(selected.session_id);
    setSession(selected);
  }, [disconnectSocket, updateConnection]);

  const attachSession = useCallback(
    async (sessionId: string, term: XtermTerminal) => {
      // #124: generation guard — init + activeId/ready effect used to race two attachSession
      // calls through await authTicket; the loser could leave an orphaned WebSocket whose
      // onmessage still wrote into the same xterm (printf once → two lines / key doubling).
      const gen = ++attachGenRef.current;
      disconnectSocket();
      updateConnection("connecting");
      setError("");
      try {
        const ticket = await authTicket();
        if (!acceptTerminalAttachAttempt(gen, attachGenRef.current, disposedRef.current)) return;
        const socket = new WebSocket(terminalWebSocketUrl(threadId, sessionId, ticket, window.location.href, API));
        if (!acceptTerminalAttachAttempt(gen, attachGenRef.current, disposedRef.current)) {
          try { socket.close(); } catch { /* ignore */ }
          return;
        }
        socketRef.current = socket;
        const ownsSocket = () =>
          acceptTerminalAttachAttempt(gen, attachGenRef.current, disposedRef.current) &&
          socketRef.current === socket && termRef.current === term && activeIdRef.current === sessionId;
        const disconnected = (message: string) => {
          if (!ownsSocket()) return;
          disconnectSocket();
          const ended = sessionEnded(sessionRef.current);
          updateConnection(ended ? "closed" : "disconnected");
          setError(ended ? "" : message);
        };
        socket.onmessage = (event) => {
          // Drop frames from a socket that is no longer the active attach.
          if (!ownsSocket()) return;
          try {
            const message = JSON.parse(String(event.data)) as {
              type: string;
              data?: string;
              truncated?: boolean;
              session?: TerminalSessionInfo;
              message?: string;
              history_truncated?: boolean;
            };
            if (message.type === "history") {
              term.reset();
              const text = decodePayload(message.data || "");
              if (text) term.write(text);
              reportUrls(text);
            } else if (message.type === "stdout" && message.data) {
              const text = decodePayload(message.data);
              term.write(text);
              reportUrls(text);
            } else if (message.type === "hello" || message.type === "status") {
              if (message.session) {
                sessionRef.current = message.session;
                setSession(message.session);
                const updated = message.session;
                setSessions((current) => current.map((row) => row.session_id === updated.session_id ? updated : row));
                term.options.disableStdin = connectionRef.current !== "connected" || !sessionAcceptsInput(updated);
              }
              if (message.message) setError(message.message);
            } else if (message.type === "truncated") {
              setSession((current) =>
                current
                  ? { ...current, history_truncated: Boolean(message.history_truncated ?? true) }
                  : current,
              );
            }
          } catch {
            // ignore malformed frames
          }
        };
        socket.onclose = () => {
          disconnected("终端连接已断开。历史输出已保留，可重新连接当前会话。");
        };
        socket.onerror = () => disconnected("终端连接失败，请重新连接。");
        socket.onopen = () => {
          if (!ownsSocket()) return;
          updateConnection("connected");
          const fit = fitRef.current;
          if (fit) {
            try {
              fit.fit();
              socket.send(JSON.stringify({ type: "resize", cols: term.cols, rows: term.rows }));
            } catch {
              // ignore fit errors on hidden panels
            }
          }
        };
      } catch (exc) {
        if (acceptTerminalAttachAttempt(gen, attachGenRef.current, disposedRef.current)) {
          updateConnection("disconnected");
          setError(exc instanceof Error ? exc.message : String(exc));
        }
      }
    },
    [disconnectSocket, reportUrls, threadId, updateConnection],
  );

  const ensureSession = useCallback(async () => {
    if (!rootPath) return "";
    const context = contextGenRef.current;
    const isCurrent = () => context === contextGenRef.current && !disposedRef.current;
    const listed = (await refreshList()) || [];
    if (!isCurrent()) return "";
    const running = listed.find((row) => row.status === "running" && !row.workspace_diverged);
    if (running) {
      selectSession(running);
      return running.session_id;
    }
    const created = await surfaceJson<TerminalSessionInfo>(
      `/api/threads/${threadId}/workspace/terminal/sessions`,
      {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ cols: 80, rows: 24 }),
      },
    );
    if (!isCurrent()) return "";
    await refreshList();
    if (!isCurrent()) return "";
    selectSession(created);
    return created.session_id;
  }, [refreshList, rootPath, selectSession, threadId]);

  useEffect(() => {
    contextGenRef.current += 1;
    disposedRef.current = false;
    activeIdRef.current = "";
    sessionRef.current = null;
    setActiveId("");
    setSession(null);
    setSessions([]);
    setError("");
    updateConnection("disconnected");
    if (!rootPath || !hostRef.current) return;

    let cancelled = false;
    let onDataDisp: { dispose: () => void } | null = null;
    let onSelDisp: { dispose: () => void } | null = null;
    let linkDisp: { dispose: () => void } | null = null;
    let themeObserver: MutationObserver | null = null;
    let ro: ResizeObserver | null = null;

    void (async () => {
      try {
        const [{ Terminal }, { FitAddon }] = await Promise.all([
          import("@xterm/xterm"),
          import("@xterm/addon-fit"),
          // eslint-disable-next-line @typescript-eslint/ban-ts-comment
          // @ts-ignore — xterm CSS resolved at runtime by Next.js bundler
          import("@xterm/xterm/css/xterm.css"),
        ]);
        if (cancelled || disposedRef.current || !hostRef.current) return;

        const term = new Terminal({
          convertEol: true,
          disableStdin: true,
          cursorBlink: true,
          fontSize: 12.5,
          lineHeight: 1.25,
          fontFamily: getComputedStyle(document.documentElement).getPropertyValue("--cx-font-mono").trim() || "ui-monospace, SFMono-Regular, Menlo, monospace",
          theme: terminalTheme(),
          scrollback: 5000,
        }) as unknown as XtermTerminal;
        const fit = new FitAddon();
        term.loadAddon(fit as unknown as { activate: (t: XtermTerminal) => void });
        term.open(hostRef.current);
        fit.fit();
        termRef.current = term;
        fitRef.current = fit;
        setReady(true);

        onDataDisp = term.onData((data) => {
          const socket = socketRef.current;
          if (disposedRef.current || termRef.current !== term || connectionRef.current !== "connected" || !sessionAcceptsInput(sessionRef.current) || !socket || socket.readyState !== WebSocket.OPEN) return;
          socket.send(JSON.stringify({ type: "stdin", data: encodeStdin(data) }));
        });
        onSelDisp = term.onSelectionChange(() => {
          setHasSelection(Boolean(term.getSelection()?.trim()));
        });
        linkDisp = term.registerLinkProvider({
          provideLinks: (y, callback) => {
            const text = term.buffer.active.getLine(y - 1)?.translateToString(true) || "";
            const links: Parameters<typeof callback>[0] = [];
            for (const match of text.matchAll(LINK_RE)) {
              const url = match[0].replace(/[.,;:!?]+$/, "");
              const start = (match.index ?? 0) + 1;
              links.push({
                range: { start: { x: start, y }, end: { x: start + url.length - 1, y } },
                text: url,
                activate: (event, target) => {
                  if (isLocalPreviewUrl(target) && !event.metaKey && !event.ctrlKey) chatPanel.openPreview(threadId, target);
                  else window.open(target, "_blank", "noopener,noreferrer");
                },
              });
            }
            callback(links.length ? links : undefined);
          },
        });
        themeObserver = new MutationObserver(() => {
          term.options.theme = terminalTheme();
        });
        themeObserver.observe(document.documentElement, { attributes: true, attributeFilter: ["class", "style", "data-theme"] });

        ro = new ResizeObserver(() => {
          try {
            fit.fit();
            const socket = socketRef.current;
            if (socket && socket.readyState === WebSocket.OPEN) {
              socket.send(JSON.stringify({ type: "resize", cols: term.cols, rows: term.rows }));
            }
          } catch {
            // panel may be hidden
          }
        });
        ro.observe(hostRef.current);

        // Only ensure a session id here. attachSession runs once from the
        // activeId + ready effect so we never open two overlapping sockets (#124).
        const sessionId = await ensureSession();
        if (!sessionId || cancelled || disposedRef.current) return;
      } catch (exc) {
        if (!cancelled && !disposedRef.current) {
          setError(exc instanceof Error ? exc.message : String(exc));
        }
      }
    })();

    return () => {
      cancelled = true;
      disposedRef.current = true;
      contextGenRef.current += 1;
      attachGenRef.current += 1;
      onDataDisp?.dispose();
      onSelDisp?.dispose();
      linkDisp?.dispose();
      themeObserver?.disconnect();
      ro?.disconnect();
      disconnectSocket();
      termRef.current?.dispose();
      termRef.current = null;
      fitRef.current = null;
      setReady(false);
    };
    // Re-init only when thread/workspace identity changes.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [threadId, rootPath]);

  useEffect(() => {
    if (!activeId || !termRef.current || disposedRef.current || !ready) return;
    void attachSession(activeId, termRef.current);
  }, [activeId, attachSession, ready]);

  const reconnect = () => {
    if (!activeId || !termRef.current || disposedRef.current || connectionRef.current !== "disconnected" || !sessionAcceptsInput(sessionRef.current)) return;
    void attachSession(activeId, termRef.current);
  };

  const createTab = async () => {
    const context = contextGenRef.current;
    setError("");
    try {
      const created = await surfaceJson<TerminalSessionInfo>(
        `/api/threads/${threadId}/workspace/terminal/sessions`,
        {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            cols: termRef.current?.cols || 80,
            rows: termRef.current?.rows || 24,
          }),
        },
      );
      if (context !== contextGenRef.current || disposedRef.current) return;
      await refreshList();
      if (context !== contextGenRef.current || disposedRef.current) return;
      selectSession(created);
    } catch (exc) {
      if (context === contextGenRef.current && !disposedRef.current) {
        setError(exc instanceof Error ? exc.message : String(exc));
      }
    }
  };

  const closeActive = async () => {
    if (!activeId) return;
    const context = contextGenRef.current;
    setError("");
    try {
      await surfaceJson(`/api/threads/${threadId}/workspace/terminal/sessions/${activeId}`, {
        method: "DELETE",
      });
      if (context !== contextGenRef.current || disposedRef.current) return;
      const listed = (await refreshList()) || [];
      if (context !== contextGenRef.current || disposedRef.current) return;
      const next = listed.find((row) => row.status === "running") || listed[0];
      if (next) {
        selectSession(next);
      } else {
        attachGenRef.current += 1;
        disconnectSocket();
        updateConnection("disconnected");
        activeIdRef.current = "";
        sessionRef.current = null;
        setActiveId("");
        setSession(null);
        termRef.current?.reset();
        await createTab();
      }
    } catch (exc) {
      if (context === contextGenRef.current && !disposedRef.current) {
        setError(exc instanceof Error ? exc.message : String(exc));
      }
    }
  };

  const interrupt = async () => {
    if (!activeId) return;
    const context = contextGenRef.current;
    try {
      const socket = socketRef.current;
      if (socket && socket.readyState === WebSocket.OPEN) {
        socket.send(JSON.stringify({ type: "interrupt" }));
      }
      await surfaceJson(
        `/api/threads/${threadId}/workspace/terminal/sessions/${activeId}/interrupt`,
        { method: "POST" },
      );
    } catch (exc) {
      if (context === contextGenRef.current && !disposedRef.current) {
        setError(exc instanceof Error ? exc.message : String(exc));
      }
    }
  };

  const citeSelection = () => {
    const text = termRef.current?.getSelection()?.trim() || "";
    if (!text || !onCiteToComposer) return;
    const header = [
      "【终端摘录",
      `Thread ${threadId}`,
      `session ${activeId || "unknown"}`,
      `cwd ${session?.cwd_label || session?.root_path || rootPath || "?"}`,
      "】",
    ].join(" · ");
    // Reserved for #21 structured kind: "terminal_excerpt"
    onCiteToComposer(`${header}\n${text}`);
  };

  if (!rootPath) {
    return <EmptyState icon="terminal" title="终端不可用" description="当前会话未绑定工作区，无法打开交互终端。" />;
  }

  const running = sessionAcceptsInput(session);
  const disconnected = connection === "disconnected" && Boolean(activeId);
  return (
    <div className="flex min-h-0 flex-1 flex-col" data-testid="conversation-interactive-terminal">
      <div className="flex h-10 shrink-0 items-center gap-1 border-b border-cx-border-subtle pl-1.5 pr-1.5">
        <div className="cx-no-scrollbar flex min-w-0 flex-1 items-center gap-0.5 overflow-x-auto" role="tablist" aria-label="终端会话">
          {sessions.map((row) => {
            const selected = row.session_id === activeId;
            const alive = row.status === "running";
            return (
              <button
                key={row.session_id}
                type="button"
                role="tab"
                aria-selected={selected}
                data-active={selected ? "true" : "false"}
                onClick={() => {
                  if (row.session_id === activeId) return;
                  selectSession(row);
                }}
                className={cn(
                  "cx-press inline-flex h-7 shrink-0 items-center gap-1.5 rounded-lg px-2 font-cx-mono text-[12px] transition-colors",
                  selected ? "bg-cx-active text-cx-fg" : "text-cx-fg-3 hover:bg-cx-hover hover:text-cx-fg",
                )}
              >
                <StatusDot tone={selected && disconnected ? "warning" : alive ? "success" : "neutral"} />
                {row.name || row.session_id.slice(0, 6)}
              </button>
            );
          })}
          <IconButton size="xs" icon="plus" label="新建终端会话" onClick={() => void createTab()} />
        </div>
        <Button size="xs" variant="ghost" icon="quote" disabled={!hasSelection || !onCiteToComposer} onClick={citeSelection} aria-label="加入对话">
          引用
        </Button>
        <IconButton size="sm" icon="stopCircle" label="发送 Ctrl+C" disabled={!activeId || connection !== "connected" || !running} onClick={() => void interrupt()} aria-label="中断" />
        <IconButton size="sm" icon="trash" label="关闭会话" className="hover:text-cx-danger" disabled={!activeId} onClick={() => void closeActive()} aria-label="关闭会话" />
      </div>
      <div className="flex h-7 shrink-0 items-center gap-2 border-b border-cx-border-subtle bg-cx-bg-subtle px-3 text-[11.5px] text-cx-fg-3" role="status">
        {connection === "connecting" ? <Spinner size={11} /> : <StatusDot tone={disconnected ? "warning" : connection === "connected" && running ? "success" : session?.workspace_diverged ? "warning" : "neutral"} />}
        <span className="shrink-0">{connection === "connecting" ? "连接中…" : connection === "connected" ? "已连接" : connection === "closed" ? "连接已结束" : activeId ? "连接已断开" : "未连接"}</span>
        {session ? <span className="shrink-0">{connection === "connected" || connection === "closed" ? "进程" : "上次进程状态"}：{statusLabel(session)}</span> : null}
        <code className="min-w-0 truncate font-cx-mono text-cx-fg-4" title={session?.root_path || rootPath}>{session?.cwd_label || session?.root_path || rootPath}</code>
        {session?.history_truncated ? <span className="ml-auto shrink-0 text-cx-fg-4">历史已截断</span> : null}
        {activeId && running && (connection === "disconnected" || connection === "connecting") ? (
          <Button size="xs" variant="ghost" icon="refresh" className="ml-auto" disabled={!ready || connection === "connecting"} onClick={reconnect} aria-label="重新连接">
            重新连接
          </Button>
        ) : null}
      </div>
      {error ? <div className="shrink-0 px-2 pt-2"><Callout tone="danger" onDismiss={() => setError("")}>{error}</Callout></div> : null}
      <div className="cx-terminal-host relative min-h-0 flex-1">
        <div className="absolute inset-0 px-2 pb-1 pt-2" ref={hostRef} data-c34-terminal="true" />
      </div>
    </div>
  );
}
