"use client";

import Link from "next/link";
import { ControlReviewRecords } from "./ControlReviewRecords";
import { DEFAULT_SUBAGENT_POLICY, SubagentPolicySettings } from "./SubagentPolicySettings";
import { useCallback, useEffect, useMemo, useState } from "react";
import {
  Badge,
  Button,
  SearchInput,
  Switch,
  TabBar,
  toast,
} from "@muteki/ui";
import { SettingsDisclosure, SettingsEmpty, SettingsNote, SettingsRow, SettingsSection } from "@/components/settings/primitives";
import { SettingsLink } from "@/components/settings/SettingsHost";
import {
  fetchControlActivity,
  fetchControlReviews,
  fetchControlSettings,
  updateControlSettings,
  type ControlActivityRow,
  type RecordedReviewRow,
  type SubagentPolicy,
} from "@/lib/controlGrants";

const ACTION_LABELS: Record<string, string> = {
  request_grant: "申请控制授权",
  grant_approved: "批准授权申请",
  grant_declined: "拒绝授权申请",
  grant_set: "修改单对话授权",
  settings_changed: "更新控制设置",
  settings_patched: "Agent 更新非敏感控制配置",
  reference_thread: "建立跨对话正式引用",
  record_review: "写入对话审阅结论",
  settle_self_armed: "预约自动结案 (settle_self)",
  settle_self_cancelled: "取消预约自动结案",
  self_settle_triggered: "触发自动结案",
  update_threads: "批量更新对话生命周期",
  delete_thread: "永久删除归档对话",
  muteki_start_thread: "委派线程 / 新建线程",
  muteki_send_message: "跨对话发送 / 插话 / 定时消息",
  muteki_cancel_thread: "取消委派线程",
  control_thread: "控制对话运行状态",
  respond_input: "回复对话结构化提问",
  respond_approval: "回复对话执行审批",
  rewind_thread: "回滚对话历史轮次",
  move_thread: "迁移对话所属项目",
  git: "执行对话 Git 操作",
  pull_request: "执行 Pull Request 操作",
  terminal: "操控对话交互终端",
};

type SettingsResponse = Awaited<ReturnType<typeof fetchControlSettings>>;

export function ControlSettings() {
  const [enabled, setEnabled] = useState(true);
  const [subagentPolicy, setSubagentPolicy] = useState<SubagentPolicy>(DEFAULT_SUBAGENT_POLICY);
  const [reviews, setReviews] = useState<RecordedReviewRow[]>([]);
  const [canonicalReviewCount, setCanonicalReviewCount] = useState(0);
  const [activity, setActivity] = useState<ControlActivityRow[]>([]);

  const [auditTab, setAuditTab] = useState<"activity" | "reviews">("activity");
  const [activityQuery, setActivityQuery] = useState("");

  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState("");
  const [revision, setRevision] = useState(0);

  const applySettingsResponse = useCallback((settings: SettingsResponse, withPolicy: boolean) => {
    setEnabled(settings.enabled !== false);
    if (withPolicy) {
      setSubagentPolicy(settings.subagent_policy ?? DEFAULT_SUBAGENT_POLICY);
    }
  }, []);

  const load = useCallback(
    async (withPolicy = true) => {
      try {
        const [settings, actRows, reviewRows] = await Promise.all([
          fetchControlSettings(),
          fetchControlActivity(80),
          fetchControlReviews(40),
        ]);
        applySettingsResponse(settings, withPolicy);
        setActivity(actRows);
        setReviews(reviewRows.reviews);
        setCanonicalReviewCount(reviewRows.canonicalCount);
        setError("");
      } catch (cause) {
        setError(cause instanceof Error ? cause.message : "读取设置失败");
      } finally {
        setLoading(false);
      }
    },
    [applySettingsResponse],
  );

  useEffect(() => {
    void load();
  }, [load, revision]);

  const toggleMasterSwitch = async (nextEnabled: boolean) => {
    setSaving(true);
    setError("");
    try {
      const updated = await updateControlSettings({ enabled: nextEnabled });
      applySettingsResponse(updated, false);
      window.dispatchEvent(new Event("muteki:chat-plugins-changed"));
      toast({ title: nextEnabled ? "已启用协作网关" : "已停用协作网关", tone: "success" });
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : "切换失败");
    } finally {
      setSaving(false);
    }
  };

  const saveChildPolicy = async (policy: SubagentPolicy) => {
    setSaving(true);
    setError("");
    try {
      const updated = await updateControlSettings({ subagent_policy: policy });
      setSubagentPolicy(updated.subagent_policy);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : "保存委派线程设置失败");
    } finally {
      setSaving(false);
    }
  };

  const filteredActivity = useMemo(() => {
    const q = activityQuery.trim().toLowerCase();
    if (!q) return activity;
    return activity.filter((row) => {
      const label = ACTION_LABELS[row.action] ?? row.action;
      return `${label} ${row.action} ${row.actor_thread_id} ${row.target_thread_id} ${JSON.stringify(row.detail)}`
        .toLowerCase()
        .includes(q);
    });
  }, [activity, activityQuery]);

  const disabled = loading || saving;

  return (
    <div className="flex flex-col gap-8" aria-busy={loading || saving}>
      {error ? (
        <div className="flex items-center justify-between gap-3">
          <SettingsNote tone="danger">{error}</SettingsNote>
          {loading ? null : (
            <Button variant="secondary" size="sm" onClick={() => setRevision((value) => value + 1)}>
              重试
            </Button>
          )}
        </div>
      ) : null}

      <SettingsSection anchor="control-master">
        <SettingsRow
          title="允许线程使用 muteki-collab"
          description={enabled ? "线程的模型可以查看其他线程、整理线程、委派子线程并互发消息。" : "线程现在不能使用协作与跨对话工具；重新开启即恢复。"}
          control={
            <Switch
              ariaLabel="启用 muteki-collab"
              checked={enabled}
              disabled={loading || saving}
              onCheckedChange={(on) => void toggleMasterSwitch(on)}
            />
          }
        />
      </SettingsSection>

      <SubagentPolicySettings
        anchor="control-subagent-policy"
        policy={subagentPolicy}
        disabled={disabled}
        onChange={(policy) => void saveChildPolicy(policy)}
      >
        <SettingsRow
          title="多 Agent 编排"
          control={
            <SettingsLink
              href="/settings/orchestration"
              className="inline-flex h-8 items-center gap-1 rounded-lg border border-cx-border bg-cx-elevated px-3 text-cx-xs font-medium text-cx-fg hover:bg-cx-hover"
            >
              配置 →
            </SettingsLink>
          }
        />
      </SubagentPolicySettings>

      <SettingsDisclosure
        anchor="control-audit"
        icon="history"
        title="审计记录"
        meta={`${activity.length} 条操作 · ${reviews.length + canonicalReviewCount} 条审阅`}
      >
        <div className="flex flex-wrap items-center justify-between gap-3 border-b border-cx-border-subtle px-5 pt-1">
          <TabBar
            ariaLabel="审计视图"
            className="border-b-0"
            value={auditTab}
            onChange={setAuditTab}
            items={[
              { value: "activity", label: "操作", count: activity.length },
              { value: "reviews", label: "审阅", count: reviews.length + canonicalReviewCount },
            ]}
          />
          {auditTab === "activity" ? (
            <SearchInput
              className="mb-2 w-full sm:mb-0 sm:w-[240px]"
              value={activityQuery}
              onValueChange={setActivityQuery}
              placeholder="筛选"
            />
          ) : null}
        </div>
        <div className="cx-scroll max-h-[420px] overflow-y-auto">
          {auditTab === "activity" ? (
            filteredActivity.length === 0 ? (
              <SettingsEmpty>没有记录</SettingsEmpty>
            ) : (
              filteredActivity.map((row, index) => (
                <SettingsRow
                  key={`${row.at}-${index}`}
                  title={ACTION_LABELS[row.action] ?? row.action}
                  description={
                    <span className="font-mono text-cx-xs">
                      {row.actor_thread_id === "user" ? "用户" : row.actor_thread_id.slice(0, 12)}
                      {row.target_thread_id && row.target_thread_id !== row.actor_thread_id
                        ? ` → ${row.target_thread_id.slice(0, 12)}`
                        : ""}
                    </span>
                  }
                  control={
                    <time className="cx-tabular text-cx-xs text-cx-fg-3" dateTime={row.at}>
                      {new Date(row.at).toLocaleString()}
                    </time>
                  }
                />
              ))
            )
          ) : (
            <><ControlReviewRecords hasOtherReviews={reviews.length > 0} />{reviews.map((item) => (
              <SettingsRow
                key={item.thread_id}
                title={
                  <span className="flex min-w-0 items-center gap-2">
                    <Link
                      href={`/chat/${encodeURIComponent(item.thread_id)}`}
                      className="truncate text-cx-fg hover:text-cx-accent hover:underline"
                    >
                      {item.thread_title || item.thread_id}
                    </Link>
                    <Badge tone={item.settled ? "success" : "neutral"}>
                      {item.review.verdict || (item.settled ? "已结案" : "已审阅")}
                    </Badge>
                  </span>
                }
                description={
                  <span className="flex flex-col gap-1">
                    <span className="line-clamp-2">{item.review.summary}</span>
                    {item.review.evidence_anchors.length > 0 ? (
                      <span className="flex flex-wrap gap-1">
                        {item.review.evidence_anchors.map((anchor) => (
                          <code key={anchor} className="rounded bg-cx-hover px-1.5 py-0.5 text-cx-2xs text-cx-fg-3">
                            {anchor}
                          </code>
                        ))}
                      </span>
                    ) : null}
                  </span>
                }
                control={
                  <time className="cx-tabular text-cx-xs text-cx-fg-3" dateTime={item.review.recorded_at}>
                    {new Date(item.review.recorded_at).toLocaleString()}
                  </time>
                }
              />
            ))}</>
          )}
        </div>
      </SettingsDisclosure>
    </div>
  );
}
