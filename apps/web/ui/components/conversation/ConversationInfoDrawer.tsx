"use client";

/* ─────────────────────────────────────────────────────────
 * CONVERSATION INFO DRAWER — on-demand session sheet: runtime metadata,
 * capability bindings, long-term memory, context window + usage/quota.
 * ───────────────────────────────────────────────────────── */

import React, { useEffect, useRef, useState } from "react";
import {
  Badge,
  Button,
  Callout,
  Checkbox,
  EmptyState,
  IconButton,
  Sheet,
  Skeleton,
  TabBar,
  TextArea,
  type Tone,
} from "@/components/chat/ui";
import { DialogSection, MetaList, MetaRow, MetricTile } from "@/components/chat/dialogs/parts";
import { apiFetch } from "@/lib/useRun";
import type {
  ConversationView,
  ConversationMemorySnapshot,
} from "@/lib/useConversation";
import { ContextWindowMeter } from "./ContextWindowMeter";
import { conversationStatsSummaryItems } from "./ConversationStatsBar";

export interface ConversationInfoDrawerProps {
  open: boolean;
  onClose: () => void;
  view: ConversationView | null;
  memory: ConversationMemorySnapshot | null;
  memoryLoading?: boolean;
  onAddMemory?: (content: string) => Promise<void>;
  onDeleteMemory?: (memoryId: string) => Promise<void>;
  onRefreshView?: () => void;
  className?: string;
}

type InfoTab = "overview" | "memory" | "context";

const TAB_LABEL: Record<InfoTab, string> = { overview: "概览", memory: "记忆", context: "上下文" };

type QuotaEntry = {
  credential_id: string;
  label: string;
  engine: string;
  quota_type: "subscription" | "api_key";
  status: string;
  remaining: number | null;
  total: number | null;
  reset_at: number | null;
  unknown_reason: string | null;
};

function formatDateTime(value?: string | null): string {
  if (!value) return "—";
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return value;
  return date.toLocaleString("zh-CN", { hour12: false });
}

function statusTone(status: string): Tone {
  const value = status.toLowerCase();
  if (/(fail|error)/.test(value)) return "danger";
  if (/(running|busy|starting|waiting)/.test(value)) return "running";
  if (/(pending|approval|input)/.test(value)) return "warning";
  if (/(idle|ready|complete|done)/.test(value)) return "success";
  return "neutral";
}

function quotaStatus(entry: QuotaEntry): { label: string; tone: Tone } {
  if (entry.status === "ok") return { label: "已知", tone: "success" };
  if (entry.status === "not_supported") return { label: "不支持", tone: "neutral" };
  return { label: "未知", tone: "warning" };
}

export function ConversationInfoDrawer({
  open,
  onClose,
  view,
  memory,
  memoryLoading = false,
  onAddMemory,
  onDeleteMemory,
  onRefreshView,
  className = "",
}: ConversationInfoDrawerProps) {
  const [tab, setTab] = useState<InfoTab>("overview");
  const [newMemoryText, setNewMemoryText] = useState("");
  const [consent, setConsent] = useState(false);
  const [busy, setBusy] = useState(false);
  const [confirmDeleteId, setConfirmDeleteId] = useState<string | null>(null);
  const [deletingId, setDeletingId] = useState<string | null>(null);
  const [retainedView, setRetainedView] = useState(view);

  const [quotaData, setQuotaData] = useState<QuotaEntry[] | null>(null);
  const [quotaError, setQuotaError] = useState("");
  const [quotaLoading, setQuotaLoading] = useState(false);
  const [quotaRefreshing, setQuotaRefreshing] = useState<string | null>(null);

  const loadQuota = async () => {
    if (quotaLoading) return;
    setQuotaLoading(true);
    try {
      const res = await apiFetch(`/api/usage/quota`);
      if (!res.ok) throw new Error(`额度读取失败（${res.status}）`);
      const json = await res.json() as { quota: QuotaEntry[] };
      setQuotaData(json.quota ?? []);
      setQuotaError("");
    } catch (e) {
      setQuotaError(e instanceof Error ? e.message : "额度读取失败");
    } finally {
      setQuotaLoading(false);
    }
  };

  const refreshQuotaEntry = async (credentialId: string) => {
    setQuotaRefreshing(credentialId);
    try {
      await apiFetch(`/api/usage/quota/${encodeURIComponent(credentialId)}/refresh`, { method: "POST" });
      await loadQuota();
    } catch {
    } finally {
      setQuotaRefreshing(null);
    }
  };

  // Quota is lazy: fetched the first time the context tab is shown.
  const prevTab = useRef(tab);
  useEffect(() => {
    if (tab === "context" && prevTab.current !== "context" && quotaData === null) {
      void loadQuota();
    }
    prevTab.current = tab;
  }, [tab]);

  useEffect(() => {
    if (view) setRetainedView(view);
  }, [view]);

  useEffect(() => {
    if (!open) setConfirmDeleteId(null);
  }, [open]);

  const activeView = view ?? retainedView;
  if (!activeView) return null;

  const handleAddMemory = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!newMemoryText.trim() || !consent || !onAddMemory || busy) return;
    setBusy(true);
    try {
      await onAddMemory(newMemoryText.trim());
      setNewMemoryText("");
      setConsent(false);
    } finally {
      setBusy(false);
    }
  };

  const handleDeleteMemory = async (memoryId: string) => {
    if (!onDeleteMemory) return;
    setDeletingId(memoryId);
    try {
      await onDeleteMemory(memoryId);
      setConfirmDeleteId(null);
    } finally {
      setDeletingId(null);
    }
  };

  const usage = (activeView.state.usage || {}) as Record<string, unknown>;
  const tokenCoverage = String(
    usage.token_coverage
    ?? activeView.statistics?.token_coverage
    ?? "missing",
  ) as "missing" | "partial" | "complete";
  const promptRaw = usage.prompt_tokens ?? usage.input_tokens;
  const completionRaw = usage.completion_tokens ?? usage.output_tokens;
  const promptTokens = promptRaw == null || promptRaw === "" ? null : Number(promptRaw);
  const completionTokens = completionRaw == null || completionRaw === "" ? null : Number(completionRaw);
  const promptValid = promptTokens != null && !Number.isNaN(promptTokens);
  const completionValid = completionTokens != null && !Number.isNaN(completionTokens);
  const formatToken = (value: number | null, valid: boolean) => {
    if (tokenCoverage === "missing" || !valid) return "未上报";
    return value!.toLocaleString();
  };
  const totalLabel = (() => {
    if (tokenCoverage === "missing") return "未上报";
    if (promptValid && completionValid) return (promptTokens! + completionTokens!).toLocaleString();
    return "数据不完整";
  })();
  const rawCost = usage.estimated_cost ?? usage.reported_cost ?? usage.cost_usd ?? usage.total_cost_usd;
  const costUsd = rawCost == null ? null : Number(rawCost);

  const contextWindow = activeView.context_window ?? null;
  const rtCaps = (activeView.runtime_connection?.capabilities ?? {}) as Record<string, unknown>;
  const compactionSupported = Boolean(rtCaps.compaction);
  const statsItems = conversationStatsSummaryItems(activeView.statistics, activeView.context_window);
  const memories = memory?.memories ?? [];
  const workspacePath = activeView.workspace?.root_path || "";
  const credential = activeView.runtime.credential_id || activeView.runtime.credential_ref || "";

  const overview = (
    <div className="flex flex-col gap-5">
      <div className="grid grid-cols-3 gap-2">
        <MetricTile label="累计 Token" value={totalLabel} tone={tokenCoverage === "complete" ? undefined : "neutral"} />
        <MetricTile label="轮次" value={(activeView.statistics?.turn_count ?? activeView.turns.length).toLocaleString()} />
        <MetricTile
          label="估算费用"
          value={costUsd == null || Number.isNaN(costUsd) ? "未定价" : `$${costUsd.toFixed(4)}`}
          tone={costUsd == null ? "neutral" : "success"}
        />
      </div>

      {statsItems.length ? (
        <div
          className="cx-tabular rounded-xl bg-cx-bg-subtle px-3 py-2 font-cx-mono text-[11.5px] leading-5 text-cx-fg-3"
          data-testid="c39-info-stats"
        >
          {statsItems.join("  ·  ")}
        </div>
      ) : null}

      <DialogSection title="会话" icon="messages">
        <MetaList>
          <MetaRow label="标题">{activeView.thread.title || "未命名对话"}</MetaRow>
          <MetaRow label="状态">
            <Badge tone={statusTone(activeView.state.status)} dot>{activeView.state.status}</Badge>
          </MetaRow>
          <MetaRow label="模式" mono>{activeView.thread.mode}</MetaRow>
          <MetaRow label="创建时间">{formatDateTime(activeView.thread.created_at)}</MetaRow>
          {activeView.thread.updated_at ? <MetaRow label="最近更新">{formatDateTime(activeView.thread.updated_at)}</MetaRow> : null}
          <MetaRow label="Thread ID" mono copy={activeView.thread.thread_id}>{activeView.thread.thread_id}</MetaRow>
          {activeView.agent_session?.external_session_id ? (
            <MetaRow label="外部 Session" mono copy={activeView.agent_session.external_session_id}>
              {activeView.agent_session.external_session_id}
            </MetaRow>
          ) : null}
          <MetaRow label="事件序列" mono>#{activeView.watermark}</MetaRow>
        </MetaList>
      </DialogSection>

      <DialogSection title="Agent Runtime" icon="cpu">
        <MetaList>
          <MetaRow label="模型">{activeView.runtime.model || "默认模型"}</MetaRow>
          {activeView.runtime.effort ? <MetaRow label="推理强度" mono>{activeView.runtime.effort}</MetaRow> : null}
          {activeView.runtime.access_mode ? <MetaRow label="访问模式" mono>{activeView.runtime.access_mode}</MetaRow> : null}
          <MetaRow label="Adapter" mono>{activeView.runtime.adapter_id}</MetaRow>
          <MetaRow label="实例 ID" mono copy={activeView.runtime.instance_id}>{activeView.runtime.instance_id}</MetaRow>
          <MetaRow label="凭据引用" mono>{credential || "默认"}</MetaRow>
        </MetaList>
      </DialogSection>

      {activeView.workspace ? (
        <DialogSection title="工作区" icon="folder">
          <MetaList>
            <MetaRow label="路径" mono copy={workspacePath}>{workspacePath || "—"}</MetaRow>
            <MetaRow label="类型" mono>{activeView.workspace.kind}</MetaRow>
          </MetaList>
        </DialogSection>
      ) : null}

      <DialogSection
        title="能力授权"
        icon="shield"
        meta={activeView.binding ? `Binding #${activeView.binding.binding_version} · ${activeView.binding.mode}` : undefined}
      >
        {activeView.binding ? (
          activeView.binding.tool_set?.length ? (
            <div className="flex flex-wrap gap-1">
              {activeView.binding.tool_set.map((tool) => (
                <Badge key={tool} className="font-cx-mono text-[11px]">{tool}</Badge>
              ))}
            </div>
          ) : (
            <p className="text-[12.5px] text-cx-fg-4">未授权工具</p>
          )
        ) : (
          <p className="text-[12.5px] text-cx-fg-4">当前没有激活的 CapabilityBinding</p>
        )}
        {activeView.grants && activeView.grants.length > 0 ? (
          <MetaList className="mt-2">
            {activeView.grants.map((grant) => (
              <MetaRow key={grant.grant_id} label="Grant" mono>
                <span className="flex items-center justify-between gap-2">
                  <span className="truncate">{grant.injection_kind}</span>
                  <span className="shrink-0 font-cx-sans text-cx-fg-4">
                    {grant.expires_at ? `到期 ${new Date(grant.expires_at).toLocaleTimeString("zh-CN", { hour12: false })}` : "会话期有效"}
                  </span>
                </span>
              </MetaRow>
            ))}
          </MetaList>
        ) : null}
      </DialogSection>
    </div>
  );

  const memoryPanel = (
    <div className="flex flex-col gap-5">
      {onAddMemory ? (
        <form noValidate onSubmit={handleAddMemory} className="flex flex-col gap-3 rounded-xl border border-cx-border-subtle bg-cx-bg-subtle p-3">
          <TextArea
            label="新增长期记忆"
            value={newMemoryText}
            onChange={(event) => setNewMemoryText(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) {
                event.preventDefault();
                event.currentTarget.form?.requestSubmit();
              }
            }}
            placeholder="例如：偏好的代码规范、环境要求或业务规则…"
            autoResize
            maxRows={8}
            rows={2}
            className="min-h-[64px] bg-cx-elevated"
          />
          <div className="flex items-center justify-between gap-3">
            <Checkbox checked={consent} onCheckedChange={setConsent} label="同意持久化存储" className="text-[12.5px]" />
            <Button
              type="submit"
              size="sm"
              variant="primary"
              icon="plus"
              loading={busy}
              disabled={!newMemoryText.trim() || !consent}
            >
              保存记忆
            </Button>
          </div>
        </form>
      ) : null}

      <DialogSection title="已保存记忆" icon="brain" meta={memories.length ? `${memories.length} 条` : undefined}>
        {memoryLoading ? (
          <div className="flex flex-col gap-2" aria-busy="true" aria-label="正在加载记忆">
            {[0, 1, 2].map((index) => (
              <div key={index} className="flex flex-col gap-2 rounded-xl border border-cx-border-subtle p-3">
                <Skeleton className="w-4/5" />
                <Skeleton className="h-2.5 w-1/3" />
              </div>
            ))}
          </div>
        ) : memories.length ? (
          <ul className="flex flex-col gap-1.5">
            {memories.map((item) => {
              const confirming = confirmDeleteId === item.memory_id;
              return (
                <li
                  key={item.memory_id}
                  className="group/memory flex items-start gap-2 rounded-xl border border-cx-border-subtle bg-cx-elevated px-3 py-2.5 transition-colors hover:border-cx-border"
                >
                  <div className="min-w-0 flex-1">
                    <p className="whitespace-pre-wrap break-words text-[13px] leading-5 text-cx-fg">{item.content}</p>
                    <div className="mt-1.5 flex items-center gap-1.5 text-[11.5px] text-cx-fg-4">
                      <Badge className="h-[18px] text-[11px]">{item.kind}</Badge>
                      <span className="cx-tabular">{new Date(item.created_at).toLocaleDateString("zh-CN")}</span>
                    </div>
                  </div>
                  {onDeleteMemory ? (
                    confirming ? (
                      <div className="flex shrink-0 items-center gap-1">
                        <Button size="xs" variant="ghost" onClick={() => setConfirmDeleteId(null)} disabled={deletingId === item.memory_id}>
                          取消
                        </Button>
                        <Button
                          size="xs"
                          variant="danger"
                          loading={deletingId === item.memory_id}
                          onClick={() => void handleDeleteMemory(item.memory_id)}
                          aria-label="确认删除此记忆"
                        >
                          删除
                        </Button>
                      </div>
                    ) : (
                      <IconButton
                        icon="trash"
                        label="删除此记忆"
                        size="xs"
                        className="shrink-0 opacity-0 transition-opacity hover:text-cx-danger focus-visible:opacity-100 group-hover/memory:opacity-100"
                        onClick={() => setConfirmDeleteId(item.memory_id)}
                      />
                    )
                  ) : null}
                </li>
              );
            })}
          </ul>
        ) : (
          <EmptyState compact icon="brain" title="暂无长期记忆" description="保存的偏好与约定会在后续轮次中自动带入上下文。" />
        )}
      </DialogSection>
    </div>
  );

  const contextPanel = (
    <div className="flex flex-col gap-5">
      <ContextWindowMeter
        threadId={activeView.thread.thread_id}
        contextWindow={contextWindow}
        compactionSupported={compactionSupported}
        onRefresh={onRefreshView}
      />

      <DialogSection
        title="累计消耗"
        icon="activity"
        meta={
          tokenCoverage === "partial"
            ? "已知累计 · 数据不完整 · 非窗口占用"
            : tokenCoverage === "missing"
              ? "未上报 · 非窗口占用"
              : "非当前窗口占用"
        }
      >
        <div className="grid grid-cols-2 gap-2">
          <MetricTile label="Prompt Tokens" value={formatToken(promptTokens, promptValid)} />
          <MetricTile label="Completion Tokens" value={formatToken(completionTokens, completionValid)} />
          <MetricTile label="累计 Token" value={totalLabel} />
          <MetricTile
            label="估算费用 (USD)"
            value={costUsd == null || Number.isNaN(costUsd) ? "未定价" : `$${costUsd.toFixed(4)}`}
            tone={costUsd == null ? "neutral" : "success"}
          />
        </div>
      </DialogSection>

      <DialogSection
        title="订阅额度"
        icon="gauge"
        actions={(
          <>
            <IconButton
              icon="refresh"
              label="刷新额度（只重读缓存，不启动 Agent）"
              size="xs"
              loading={quotaLoading}
              onClick={() => void loadQuota()}
            />
            <a
              href="/usage"
              className="inline-flex h-6 items-center gap-1 rounded-md px-1.5 text-[12px] font-medium text-cx-accent hover:bg-cx-hover"
              title="打开全局用量页面查看完整额度"
            >
              全局用量
            </a>
          </>
        )}
      >
        <p className="mb-2 text-[12px] leading-5 text-cx-fg-4">
          订阅额度与 Token 用量分开统计。CLI 引擎不主动上报额度；会话中引擎响应后自动更新。
        </p>
        {quotaError ? <Callout tone="danger">{quotaError}</Callout> : null}
        {quotaData === null && !quotaError ? (
          quotaLoading ? (
            <div className="flex flex-col gap-2" aria-busy="true" aria-label="正在读取额度">
              <Skeleton className="h-10 w-full rounded-xl" />
              <Skeleton className="h-10 w-full rounded-xl" />
            </div>
          ) : (
            <p className="text-[12px] text-cx-fg-4">切换到此标签页时自动加载。</p>
          )
        ) : null}
        {quotaData && quotaData.length === 0 ? (
          <p className="text-[12.5px] text-cx-fg-4">暂无已配置凭据账号。</p>
        ) : null}
        {quotaData && quotaData.length > 0 ? (
          <ul className="flex flex-col gap-1.5">
            {quotaData.map((entry) => {
              const status = quotaStatus(entry);
              return (
                <li key={entry.credential_id} className="rounded-xl border border-cx-border-subtle bg-cx-elevated px-3 py-2">
                  <div className="flex items-center gap-2">
                    <span className="min-w-0 flex-1 truncate text-[13px] font-medium text-cx-fg">{entry.label}</span>
                    <span className="font-cx-mono text-[11px] text-cx-fg-4">{entry.engine}</span>
                    <Badge tone={status.tone}>{status.label}</Badge>
                    {entry.quota_type === "subscription" ? (
                      <IconButton
                        icon="refresh"
                        size="xs"
                        label="刷新（不启动 Agent）"
                        loading={quotaRefreshing === entry.credential_id}
                        onClick={() => void refreshQuotaEntry(entry.credential_id)}
                      />
                    ) : null}
                  </div>
                  {entry.status === "ok" ? (
                    <div className="cx-tabular mt-1 flex flex-wrap gap-x-3 gap-y-0.5 text-[12px] text-cx-fg-3">
                      <span>剩余 <b className="font-semibold text-cx-fg">{entry.remaining?.toLocaleString() ?? "—"}</b></span>
                      {entry.total != null ? <span>总量 <b className="font-semibold text-cx-fg">{entry.total.toLocaleString()}</b></span> : null}
                      {entry.reset_at != null ? (
                        <span>重置 <b className="font-semibold text-cx-fg">{new Date(entry.reset_at * 1000).toLocaleString("zh-CN", { hour12: false })}</b></span>
                      ) : null}
                    </div>
                  ) : (
                    <p className="mt-1 text-[12px] text-cx-fg-4">
                      {entry.unknown_reason || (entry.quota_type === "api_key" ? "API 密钥账号无订阅窗口" : "未知")}
                    </p>
                  )}
                </li>
              );
            })}
          </ul>
        ) : null}
      </DialogSection>
    </div>
  );

  const body = (() => {
    switch (tab) {
      case "overview":
        return overview;
      case "memory":
        return memoryPanel;
      case "context":
        return contextPanel;
      default: {
        const exhaustive: never = tab;
        return exhaustive;
      }
    }
  })();

  return (
    <Sheet
      open={open}
      onOpenChange={(next) => { if (!next) onClose(); }}
      width={460}
      title="会话信息"
      description={activeView.thread.title || undefined}
      ariaLabel="会话信息与元数据"
      testId="conversation-info-drawer"
      className={`cx-info-sheet ${className}`.trim()}
      bodyClassName="flex flex-col"
    >
      <div className="sticky top-0 z-10 bg-cx-overlay px-3">
        <TabBar<InfoTab>
          value={tab}
          onChange={setTab}
          ariaLabel="会话信息分类"
          items={[
            { value: "overview", label: TAB_LABEL.overview, icon: "info" },
            { value: "memory", label: TAB_LABEL.memory, icon: "brain", count: memories.length || undefined },
            {
              value: "context",
              label: TAB_LABEL.context,
              icon: "gauge",
              alert: contextWindow?.total != null && contextWindow.limit ? contextWindow.total / contextWindow.limit >= 0.85 : false,
            },
          ]}
        />
      </div>
      <div role="tabpanel" aria-label={TAB_LABEL[tab]} className="px-4 pb-6 pt-4 text-[13px]">
        {body}
      </div>
    </Sheet>
  );
}
