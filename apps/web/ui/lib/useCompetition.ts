"use client";

/**
 * Competition 数据层（任务书 COMP-10，设计 14.3）。
 *
 * - 一条 SSE 只接收合并后的当前 snapshot；重连重新申请一次性票据。
 * - 历史事件仅在时间线按需分页读取，绝不用于覆盖当前实体状态。
 * - 进入子 Run 详情页时才订阅该 Run 的 SSE（/run/[id] 自己负责）；本 hook
 *   一场比赛只维护一条 SSE。
 * - 全部写操作走 COMP-09 端点，返回 CommandReceipt；receipt 同时追加为
 *   本地操作卡片（设计 14.4：结构化 command receipt 是实际操作记录）。
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { API, apiFetch } from "./useRun";
import { recordTaskReceipt } from "./task-center";
import {
  ActivityCard,
  ChatMessage,
  CompetitionDeck,
  CompetitionInfo,
  CompetitionPublicEvent,
  CompetitionSnapshotView,
  ConnectionView,
  applySnapshot,
  applySnapshotFrame,
  cardForEvent,
  emptyCompetitionDeck,
  localReceiptCard,
} from "./competition-events";

export interface CommandReceiptView {
  command_id: string;
  receipt_id?: string;
  state?: string; // accepted / completed / failed / conflict
  run_id?: string | null;
  aggregate?: { type?: string; id?: string } | null;
  event_cursor?: string | null;
  deduplicated?: boolean;
  next?: string[];
  effect_ids?: string[];
  error?: {
    code: string;
    message: string;
    category?: string;
    recovery_hint?: string;
    correlation_id?: string;
  } | null;
}

async function readReceipt(res: Response): Promise<CommandReceiptView> {
  const body = (await res.json().catch(() => ({}))) as {
    receipt?: CommandReceiptView;
    error?: { code: string; message: string; category?: string };
  };
  if (body.receipt) return body.receipt;
  if (!res.ok) {
    return {
      command_id: "",
      state: "failed",
      error: body.error ?? {
        code: `http_${res.status}`,
        message: `HTTP ${res.status}`,
      },
    };
  }
  return { command_id: "", state: "completed" };
}

const RECEIPT_TERMINAL_STATES = new Set(["completed", "failed", "conflict", "cancelled"]);

async function waitForTerminalReceipt(
  initial: CommandReceiptView,
  label: string,
): Promise<CommandReceiptView> {
  if (!initial.command_id || RECEIPT_TERMINAL_STATES.has(initial.state || "")) return initial;
  for (let attempt = 0; attempt < 40; attempt += 1) {
    await new Promise((resolve) => window.setTimeout(resolve, 250));
    const response = await apiFetch(`/api/receipts/${encodeURIComponent(initial.command_id)}`);
    if (!response.ok) continue;
    const body = (await response.json().catch(() => ({}))) as { receipt?: CommandReceiptView };
    if (!body.receipt) continue;
    recordTaskReceipt(body.receipt, label, "competition");
    if (RECEIPT_TERMINAL_STATES.has(body.receipt.state || "")) return body.receipt;
  }
  return initial;
}

function newCommandId(): string {
  return `cmd_ui_${Date.now().toString(36)}_${Math.random()
    .toString(36)
    .slice(2, 10)}`;
}

// ---------------------------------------------------------------------------
// 列表页数据（平台连接 + 比赛清单）
// ---------------------------------------------------------------------------

export async function fetchConnections(): Promise<ConnectionView[]> {
  const res = await apiFetch("/api/platform-connections");
  if (!res.ok) throw new Error(`加载平台连接失败（HTTP ${res.status}）`);
  const data = await res.json();
  return Array.isArray(data) ? (data as ConnectionView[]) : [];
}

export type PlatformKindView = {
  id: string;
  label: string;
  icon?: string;
  origin?: string;
  source?: string;
  extension_id?: string;
  state?: string;
};

export async function fetchPlatformKinds(): Promise<PlatformKindView[]> {
  const res = await apiFetch("/api/platform-kinds");
  if (!res.ok) throw new Error(`加载平台类型失败（HTTP ${res.status}）`);
  const data = (await res.json().catch(() => ({}))) as {
    kinds?: PlatformKindView[];
  };
  return Array.isArray(data.kinds) ? data.kinds : [];
}

export async function fetchCompetitions(): Promise<CompetitionInfo[]> {
  const res = await apiFetch("/api/competitions");
  if (!res.ok) throw new Error(`加载比赛列表失败（HTTP ${res.status}）`);
  const data = await res.json();
  return Array.isArray(data) ? (data as CompetitionInfo[]) : [];
}

/** 创建平台连接。凭据只提交给后端（secret:// 引用），前端不落任何存储。 */
export async function createConnection(input: {
  platform_kind: string;
  base_url: string;
  account_key: string;
  credential_ref?: string;
  credential?: string;
}): Promise<CommandReceiptView> {
  const res = await apiFetch("/api/platform-connections", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ ...input, command_id: newCommandId() }),
  });
  const receipt = await readReceipt(res);
  recordTaskReceipt(receipt, "创建平台连接", "competition");
  return receipt;
}

export async function probeConnection(
  connectionId: string,
): Promise<CommandReceiptView> {
  const res = await apiFetch(
    `/api/platform-connections/${encodeURIComponent(connectionId)}/probe`,
    {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ command_id: newCommandId() }),
    },
  );
  const initial = await readReceipt(res);
  const receipt = await waitForTerminalReceipt(initial, "测试平台连接");
  recordTaskReceipt(receipt, "测试平台连接", "competition");
  return receipt;
}

export type PlatformSecretMetadata = {
  reference: string;
  connection_id: string;
  key: string;
  created_at: string;
  updated_at: string;
  expires_at: string;
  rotation: number;
  status: string;
};

export type BrowserSessionStatus = {
  present: boolean;
  expired: boolean;
  imported_at?: string;
  expires_at?: string;
  cookies?: number;
  origins?: number;
};

export async function fetchPlatformSecrets(): Promise<PlatformSecretMetadata[]> {
  const res = await apiFetch("/api/platform-secrets");
  if (!res.ok) throw new Error(`加载平台凭据失败（HTTP ${res.status}）`);
  const data = await res.json();
  return Array.isArray(data) ? (data as PlatformSecretMetadata[]) : [];
}

export async function writePlatformSecret(
  connectionId: string,
  input: { credential: string; mode: "update" | "rotate"; expires_at?: string },
): Promise<CommandReceiptView> {
  const res = await apiFetch(
    `/api/platform-connections/${encodeURIComponent(connectionId)}/secret`,
    {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        credential: input.credential,
        mode: input.mode,
        expires_at: input.expires_at || "",
        command_id: newCommandId(),
      }),
    },
  );
  const receipt = await readReceipt(res);
  recordTaskReceipt(
    receipt,
    input.mode === "rotate" ? "轮换平台凭据" : "更新平台凭据",
    "competition",
  );
  return receipt;
}

export async function revokePlatformSecret(
  connectionId: string,
): Promise<CommandReceiptView> {
  const res = await apiFetch(
    `/api/platform-connections/${encodeURIComponent(connectionId)}/secret`,
    {
      method: "DELETE",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ confirm: true, command_id: newCommandId() }),
    },
  );
  const receipt = await readReceipt(res);
  recordTaskReceipt(receipt, "撤销平台凭据", "competition");
  return receipt;
}

export async function fetchBrowserSession(
  connectionId: string,
): Promise<BrowserSessionStatus> {
  const res = await apiFetch(
    `/api/platform-connections/${encodeURIComponent(connectionId)}/browser-session`,
  );
  const body = await res.json().catch(() => ({})) as {
    browser_session?: BrowserSessionStatus;
    error?: { message?: string };
  };
  if (!res.ok) {
    throw new Error(body.error?.message || `加载浏览器会话失败（HTTP ${res.status}）`);
  }
  return body.browser_session || { present: false, expired: false };
}

export async function writeBrowserSession(
  connectionId: string,
  input: { storage_state?: Record<string, unknown>; expires_at?: string; renew?: boolean },
): Promise<CommandReceiptView> {
  const res = await apiFetch(
    `/api/platform-connections/${encodeURIComponent(connectionId)}/browser-session`,
    {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        storage_state: input.storage_state || {},
        expires_at: input.expires_at || "",
        renew: Boolean(input.renew),
        command_id: newCommandId(),
      }),
    },
  );
  const receipt = await readReceipt(res);
  recordTaskReceipt(
    receipt,
    input.renew ? "续期浏览器会话" : "导入浏览器会话",
    "competition",
  );
  return receipt;
}

export async function revokeBrowserSession(
  connectionId: string,
): Promise<CommandReceiptView> {
  const res = await apiFetch(
    `/api/platform-connections/${encodeURIComponent(connectionId)}/browser-session`,
    {
      method: "DELETE",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ confirm: true, command_id: newCommandId() }),
    },
  );
  const receipt = await readReceipt(res);
  recordTaskReceipt(receipt, "撤销浏览器会话", "competition");
  return receipt;
}

/** 登记 / 同步一场比赛（competition.sync 命令，异步 receipt）。 */
export async function registerCompetition(input: {
  connection_id: string;
  external_competition_id: string;
  title?: string;
  description?: string;
  starts_at?: string;
  ends_at?: string;
}): Promise<CommandReceiptView> {
  const res = await apiFetch("/api/competitions", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ ...input, command_id: newCommandId() }),
  });
  const initial = await readReceipt(res);
  const receipt = await waitForTerminalReceipt(initial, "同步比赛");
  recordTaskReceipt(receipt, "同步比赛", "competition");
  return receipt;
}

/** 从本地清单移除平台连接（connection.unregister；历史数据保留）。 */
export async function unregisterConnection(
  connectionId: string,
): Promise<CommandReceiptView> {
  const res = await apiFetch(
    `/api/platform-connections/${encodeURIComponent(connectionId)}`,
    {
      method: "DELETE",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ confirm: true, command_id: newCommandId() }),
    },
  );
  const receipt = await readReceipt(res);
  recordTaskReceipt(receipt, "移除平台连接", "competition");
  return receipt;
}

/** 从本地清单移除比赛（competition.unregister；历史数据保留）。 */
export async function unregisterCompetition(
  competitionId: string,
): Promise<CommandReceiptView> {
  const res = await apiFetch(
    `/api/competitions/${encodeURIComponent(competitionId)}`,
    {
      method: "DELETE",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ confirm: true, command_id: newCommandId() }),
    },
  );
  const receipt = await readReceipt(res);
  recordTaskReceipt(receipt, "移除比赛", "competition");
  return receipt;
}

export async function updateCompetitionPolicy(
  competitionId: string,
  changes: Record<string, unknown>,
): Promise<CommandReceiptView> {
  const res = await apiFetch(
    `/api/competitions/${encodeURIComponent(competitionId)}/commands`,
    {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        command_type: "policy.update",
        command_id: newCommandId(),
        aggregate_type: "competition",
        aggregate_id: competitionId,
        payload: { competition_id: competitionId, ...changes },
      }),
    },
  );
  const receipt = await readReceipt(res);
  recordTaskReceipt(receipt, "设置比赛自动化策略", "competition");
  return receipt;
}

// ---------------------------------------------------------------------------
// 比赛页 hook
// ---------------------------------------------------------------------------

export function useCompetition(competitionId: string) {
  const [deck, setDeck] = useState<CompetitionDeck>(() =>
    emptyCompetitionDeck(competitionId),
  );
  const [connected, setConnected] = useState(false);
  const esRef = useRef<EventSource | null>(null);
  const [historyCards, setHistoryCards] = useState<ActivityCard[]>([]);
  const [historyLoading, setHistoryLoading] = useState(false);
  const [historyHasMore, setHistoryHasMore] = useState(true);
  const [historyError, setHistoryError] = useState("");
  const historyRef = useRef<{ id: string; before?: number | null; loading: boolean }>({ id: competitionId, loading: false });

  // 显式刷新仍只读取本地当前状态，不触发远端比赛同步。
  const refreshSnapshot = useCallback(async () => {
    if (!competitionId) return;
    try {
      const res = await apiFetch(
        `/api/competitions/${encodeURIComponent(competitionId)}`,
        { signal: AbortSignal.timeout(15000) },
      );
      if (!res.ok) throw new Error(`加载当前状态失败（HTTP ${res.status}）`);
      const data = (await res.json()) as CompetitionSnapshotView;
      setDeck((prev) => applySnapshot(prev, data));
    } catch (error) {
      setDeck((prev) => prev.competitionId !== competitionId ? prev : ({
        ...prev, protocolError: error instanceof Error ? error.message : "加载当前状态失败",
      }));
    }
  }, [competitionId]);

  useEffect(() => {
    esRef.current?.close();
    esRef.current = null;
    setDeck(emptyCompetitionDeck(competitionId));
    setConnected(false);
    historyRef.current = { id: competitionId, loading: false };
    setHistoryCards([]);
    setHistoryLoading(false);
    setHistoryHasMore(true);
    setHistoryError("");
    if (!competitionId) return;

    let cancelled = false;
    let retryTimer: ReturnType<typeof setTimeout> | null = null;
    let retryDelay = 500;
    let connecting = false;
    const retry = () => {
      if (cancelled || retryTimer) return;
      retryTimer = setTimeout(() => {
        retryTimer = null;
        void connect();
      }, retryDelay);
      retryDelay = Math.min(retryDelay * 2, 10000);
    };
    const connect = async () => {
      if (cancelled || connecting) return;
      connecting = true;
      esRef.current?.close();
      esRef.current = null;
      let ticket = "";
      try {
        const response = await apiFetch("/api/auth/ticket", {
          method: "POST", signal: AbortSignal.timeout(10000),
        });
        if (!response.ok) throw new Error(`连接认证失败（HTTP ${response.status}）`);
        ticket = String((await response.json()).ticket || "");
      } catch (error) {
        if (!cancelled) {
          setConnected(false);
          setDeck((prev) => ({ ...prev, protocolError: error instanceof Error ? error.message : "无法连接后端" }));
          retry();
        }
        return;
      } finally {
        connecting = false;
      }
      if (cancelled) return;
      const qs = ticket ? `?ticket=${encodeURIComponent(ticket)}` : "";
      const es = new EventSource(
        `${API}/api/competitions/${encodeURIComponent(competitionId)}/events${qs}`,
      );
      esRef.current = es;
      es.onerror = () => {
        if (cancelled) return;
        setConnected(false);
        es.close();
        retry();
      };
      // 收到可用快照才标记连接成功；TCP 已连接不代表数据已就绪。
      es.addEventListener("snapshot", (e) => {
        if (cancelled) return;
        try {
          const frame = JSON.parse((e as MessageEvent).data) as unknown;
          const parsed = applySnapshotFrame(emptyCompetitionDeck(competitionId), frame);
          if (parsed.protocolError || !parsed.snapshot) throw new Error(parsed.protocolError || "快照缺少当前比赛");
          setDeck((prev) => applySnapshotFrame(prev, frame));
          setConnected(true);
          retryDelay = 500;
        } catch (error) {
          setConnected(false);
          setDeck((prev) => ({
            ...prev,
            protocolError: error instanceof Error ? error.message : "snapshot 帧不是合法 JSON",
          }));
          es.close();
          retry();
        }
      });
      es.addEventListener("error", (e) => {
        // 服务端授权 / 校验失败的显式 error 帧（区别于传输层 onerror）。
        try {
          const data = (e as MessageEvent).data;
          if (!data) return;
          const body = JSON.parse(data) as {
            error?: { code?: string; message?: string };
          };
          if (body.error) {
            setDeck((prev) => ({
              ...prev,
              protocolError:
                body.error?.message || body.error?.code || "事件流错误",
            }));
          }
        } catch {
          /* ignore */
        }
      });
    };
    void connect();
    const onVisible = () => {
      if (document.visibilityState === "visible") {
        void refreshSnapshot();
        if (!esRef.current || esRef.current.readyState === EventSource.CLOSED) {
          if (retryTimer) clearTimeout(retryTimer);
          retryTimer = null;
          void connect();
        }
      }
    };
    document.addEventListener("visibilitychange", onVisible);

    return () => {
      cancelled = true;
      esRef.current?.close();
      esRef.current = null;
      if (retryTimer) clearTimeout(retryTimer);
      document.removeEventListener("visibilitychange", onVisible);
    };
  }, [competitionId, refreshSnapshot]);

  const loadHistory = useCallback(async (reset = false) => {
    const current = historyRef.current;
    if (current.id !== competitionId || current.loading || (!reset && current.before === null)) return;
    current.loading = true;
    setHistoryLoading(true);
    setHistoryError("");
    try {
      const before = reset ? undefined : current.before;
      const suffix = before ? `&before=${before}` : "";
      const res = await apiFetch(`/api/competitions/${encodeURIComponent(competitionId)}/events/history?limit=100${suffix}`, {
        signal: AbortSignal.timeout(15000),
      });
      if (!res.ok) throw new Error(`加载历史失败（HTTP ${res.status}）`);
      const data = await res.json() as { events: CompetitionPublicEvent[]; next_before: number | null };
      if (historyRef.current !== current) return;
      const cards = data.events.map(cardForEvent).filter((item): item is ActivityCard => item !== null);
      setHistoryCards((previous) => [...new Map([...(reset ? [] : previous), ...cards].map((item) => [item.key, item])).values()].sort((a, b) => b.seq - a.seq));
      current.before = data.next_before;
      setHistoryHasMore(data.next_before !== null);
    } catch (error) {
      if (historyRef.current === current) setHistoryError(error instanceof Error ? error.message : "加载历史失败");
    } finally {
      current.loading = false;
      if (historyRef.current === current) setHistoryLoading(false);
    }
  }, [competitionId]);

  // -- 写操作 ---------------------------------------------------------------

  const appendCard = useCallback((card: ActivityCard) => {
    setDeck((prev) => ({ ...prev, cards: [...prev.cards, card].slice(-300) }));
  }, []);

  /** 发送 typed command（/commands），receipt 追加为本地操作卡片。 */
  const sendCommand = useCallback(
    async (
      commandType: string,
      payload: Record<string, unknown>,
      opts?: { aggregateType?: string; aggregateId?: string },
    ): Promise<CommandReceiptView> => {
      const commandId = newCommandId();
      const res = await apiFetch(
        `/api/competitions/${encodeURIComponent(competitionId)}/commands`,
        {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            command_type: commandType,
            command_id: commandId,
            aggregate_type: opts?.aggregateType,
            aggregate_id: opts?.aggregateId,
            payload,
          }),
        },
      );
      const receipt = await readReceipt(res);
      recordTaskReceipt(receipt, commandType, "competition");
      appendCard(
        localReceiptCard({
          commandId: receipt.command_id || commandId,
          commandType,
          state: receipt.state ?? (res.ok ? "accepted" : "failed"),
          detail: receipt.error?.message,
          errorCode: receipt.error?.code,
          runId: receipt.run_id ?? undefined,
          deduplicated: receipt.deduplicated,
        }),
      );
      return receipt;
    },
    [competitionId, appendCard],
  );

  /** 比赛聊天：自然语言消息（可附结构化命令，设计 14.4）。 */
  const sendMessage = useCallback(
    async (
      text: string,
      command?: { command_type: string; payload?: Record<string, unknown> },
    ): Promise<void> => {
      const trimmed = text.trim();
      if (!trimmed && !command) return;
      if (trimmed) {
        const local: ChatMessage = {
          key: `local-msg-${Date.now()}`,
          seq: -1,
          author: "operator",
          text: trimmed,
          at: new Date().toISOString(),
          local: true,
        };
        setDeck((prev) => ({
          ...prev,
          messages: [...prev.messages, local].slice(-400),
        }));
      }
      const res = await apiFetch(
        `/api/competitions/${encodeURIComponent(competitionId)}/messages`,
        {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            text: trimmed,
            command_id: newCommandId(),
            ...(command ? { command } : {}),
          }),
        },
      );
      const body = (await res.json().catch(() => ({}))) as {
        message_receipt?: CommandReceiptView;
        command_receipt?: CommandReceiptView;
        error?: { code: string; message: string };
      };
      if (body.command_receipt) {
        const r = body.command_receipt;
        recordTaskReceipt(
          r,
          command?.command_type || "比赛工作台操作",
          "competition",
        );
        appendCard(
          localReceiptCard({
            commandId: r.command_id,
            commandType: command?.command_type ?? "",
            state: r.state ?? "accepted",
            detail: r.error?.message,
            errorCode: r.error?.code,
            runId: r.run_id ?? undefined,
            deduplicated: r.deduplicated,
          }),
        );
      }
      if (body.message_receipt) {
        recordTaskReceipt(body.message_receipt, "发送比赛消息", "competition");
      }
      if (!res.ok && body.error) {
        appendCard(
          localReceiptCard({
            commandId: "",
            commandType: "competition.message",
            state: "failed",
            detail: body.error.message,
            errorCode: body.error.code,
          }),
        );
      }
    },
    [competitionId, appendCard],
  );

  // -- 逐题 / 批量操作 -------------------------------------------------------

  const queueChallenges = useCallback(
    (challengeIds: string[], priority = 0) =>
      Promise.all(
        challengeIds.map((id) =>
          sendCommand("challenge.queue", { challenge_id: id, priority }),
        ),
      ),
    [sendCommand],
  );

  const selectChallenge = useCallback(
    (challengeId: string) =>
      sendCommand("challenge.select", { challenge_id: challengeId }),
    [sendCommand],
  );

  const skipChallenge = useCallback(
    (challengeId: string) =>
      sendCommand("challenge.skip", { challenge_id: challengeId }),
    [sendCommand],
  );

  const ensureInstance = useCallback(
    (challengeId: string, ttlSeconds?: number) =>
      sendCommand("instance.ensure", {
        challenge_id: challengeId,
        ...(ttlSeconds ? { ttl_seconds: ttlSeconds } : {}),
      }),
    [sendCommand],
  );

  const stopInstance = useCallback(
    (leaseId: string) =>
      sendCommand("instance.stop", { lease_id: leaseId }),
    [sendCommand],
  );

  const pauseBinding = useCallback(
    (challengeId: string) =>
      sendCommand("run_binding.pause", { challenge_id: challengeId }),
    [sendCommand],
  );

  const resolveBinding = useCallback(
    (challengeId: string) =>
      sendCommand("run_binding.resolve", { challenge_id: challengeId }),
    [sendCommand],
  );

  const approveCandidate = useCallback(
    (candidateId: string) =>
      sendCommand("platform_submission.approve", { candidate_id: candidateId }),
    [sendCommand],
  );

  const retrySubmission = useCallback(
    (submissionId: string) =>
      sendCommand("platform_submission.retry", { submission_id: submissionId }),
    [sendCommand],
  );

  const submitAnswer = useCallback(
    (challengeId: string, answer: string, answerSlot = 1) =>
      sendCommand("competition.submission.submit", {
        challenge_id: challengeId,
        answer,
        answer_slot: answerSlot,
      }),
    [sendCommand],
  );

  const manualOverride = useCallback(
    (challengeId: string, answer: string, confirmAnswer: string, answerSlot = 1) =>
      sendCommand("competition.submission.manual_override", {
        challenge_id: challengeId,
        answer,
        confirm_answer: confirmAnswer,
        answer_slot: answerSlot,
        acknowledge_unverified: true,
      }),
    [sendCommand],
  );

  // -- 策略与调度器 -----------------------------------------------------------

  const updatePolicy = useCallback(
    (changes: Record<string, unknown>) =>
      sendCommand("policy.update", {
        competition_id: competitionId,
        ...changes,
      }),
    [sendCommand, competitionId],
  );

  const schedulerControl = useCallback(
    (action: "start" | "pause" | "resume") =>
      sendCommand(`scheduler.${action}`, { competition_id: competitionId }),
    [sendCommand, competitionId],
  );

  /** 重新同步（competition.sync 经专用端点）。 */
  const syncNow = useCallback(async () => {
    const competition = deck.snapshot?.competition;
    if (!competition) return;
    const receipt = await registerCompetition({
      connection_id: competition.connection_id,
      external_competition_id: competition.external_competition_id,
    });
    await refreshSnapshot();
    return receipt;
  }, [deck.snapshot, refreshSnapshot]);

  return {
    deck,
    connected,
    historyCards,
    historyLoading,
    historyHasMore,
    historyError,
    loadHistory,
    refreshSnapshot,
    sendCommand,
    sendMessage,
    queueChallenges,
    selectChallenge,
    skipChallenge,
    ensureInstance,
    stopInstance,
    pauseBinding,
    resolveBinding,
    approveCandidate,
    retrySubmission,
    submitAnswer,
    manualOverride,
    updatePolicy,
    schedulerControl,
    syncNow,
  };
}
