"use client";

import { useMemo, useState } from "react";
import { EngineLogo } from "@/components/EngineLogo";
import { Icon } from "@muteki/ui";
import { Badge, Button, Callout, Dialog, StatusDot, Switch, type Tone } from "@muteki/ui";
import { SettingsRow, SettingsSection } from "@/components/settings/primitives";
import { copyToClipboard } from "@/lib/clipboard";
import { cn } from "@/lib/cn";
import {
  compareEngineVersion,
  loadAgentVersionPolicy,
  saveAgentVersionPolicy,
  useAgentVersionPolicy,
  type AgentEngineVersionRow,
  type EngineBackendVersionStatus,
  type PolicyModeId,
} from "@/lib/agentVersionPolicy";

export interface AgentVersionSupportSectionProps {
  focusMode?: PolicyModeId;
  activeBackend?: "local" | "container";
  onOpenRuntime?: () => void;
  workerSettingsHref?: string;
}

function isBelow(row: AgentEngineVersionRow, target: EngineBackendVersionStatus): boolean {
  return Boolean(target.installed_version && compareEngineVersion(row.engine, target.installed_version, row.min_version) === -1);
}

function versionCell(row: AgentEngineVersionRow, target: EngineBackendVersionStatus): { text: string; tone: Tone } {
  if (target.status === "not_worker") return { text: "—", tone: "neutral" };
  if (target.status === "not_in_image") return { text: "未内置", tone: "neutral" };
  if (target.status === "image_unavailable") return { text: "镜像未就绪", tone: "warning" };
  if (target.status === "missing") return { text: "未安装", tone: "warning" };
  if (!target.installed_version) return { text: "未知", tone: "neutral" };
  return { text: target.installed_version, tone: isBelow(row, target) ? "danger" : "success" };
}

const MODE_ROWS: { mode: PolicyModeId; label: string }[] = [
  { mode: "ctf", label: "CTF" },
  { mode: "pentest", label: "Pentest" },
  { mode: "src", label: "SRC" },
];

export function AgentVersionSupportSection({
  focusMode,
  activeBackend,
  onOpenRuntime,
  workerSettingsHref,
}: AgentVersionSupportSectionProps) {
  const policyState = useAgentVersionPolicy();
  const snapshot = policyState.data;
  const [savingPolicy, setSavingPolicy] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  const [copiedKey, setCopiedKey] = useState("");
  const [notice, setNotice] = useState<{ tone: "success" | "warning" | "danger"; text: string } | null>(null);

  const belowCount = useMemo(
    () => (snapshot?.engines ?? []).filter(
      (row) => isBelow(row, row.local) || (row.worker_capable && isBelow(row, row.container)),
    ).length,
    [snapshot],
  );

  const copyCmd = async (key: string, command: string) => {
    const ok = await copyToClipboard(command);
    if (ok) {
      setCopiedKey(key);
      window.setTimeout(() => setCopiedKey((cur) => (cur === key ? "" : cur)), 1800);
    }
  };

  const savePolicy = async (patch: Parameters<typeof saveAgentVersionPolicy>[0]) => {
    setSavingPolicy(true);
    setNotice(null);
    try {
      await saveAgentVersionPolicy(patch);
    } catch (err) {
      setNotice({ tone: "danger", text: err instanceof Error ? err.message : String(err) });
    } finally {
      setSavingPolicy(false);
    }
  };

  const handleRefresh = async () => {
    setRefreshing(true);
    setNotice(null);
    try {
      await loadAgentVersionPolicy({ fresh: true });
    } catch (err) {
      setNotice({ tone: "danger", text: err instanceof Error ? err.message : String(err) });
    } finally {
      setRefreshing(false);
    }
  };

  if (!snapshot && policyState.status === "error") {
    return (
      <Callout
        tone="danger"
        icon="alert"
        title="Agent 版本读取失败"
        action={<Button size="sm" variant="outline" icon="refresh" onClick={() => void handleRefresh()}>重试</Button>}
      >
        {policyState.error}
      </Callout>
    );
  }

  const enforced = Boolean(snapshot?.policy.enforce);
  const engines = snapshot?.engines ?? [];
  const showContainer = engines.some((row) => row.worker_capable);
  const gridCols = showContainer
    ? "@[560px]:grid-cols-[minmax(0,1.25fr)_minmax(6.5rem,9rem)_minmax(6.5rem,9rem)_minmax(5.5rem,7.5rem)]"
    : "@[560px]:grid-cols-[minmax(0,1.25fr)_minmax(6.5rem,9.5rem)_minmax(6.5rem,9.5rem)]";

  return (
    <div className="flex flex-col gap-6" data-testid="agent-version-support-section">
      {notice ? (
        <Callout tone={notice.tone} icon="alert" onDismiss={() => setNotice(null)}>
          {notice.text}
        </Callout>
      ) : null}

      <SettingsSection
        title="Agent 版本"
        description={
          snapshot
            ? belowCount > 0 ? `${belowCount} 个引擎低于最低版本` : "全部满足最低版本"
            : "正在检测…"
        }
        actions={
          <div className="flex flex-wrap items-center gap-2">
            {workerSettingsHref ? (
              <a
                href={workerSettingsHref}
                className="cx-press inline-flex h-7 items-center gap-1 rounded-lg border border-cx-border bg-cx-elevated px-2.5 text-cx-xs font-medium text-cx-fg no-underline hover:border-cx-border-strong"
              >
                出战 Worker
                <Icon name="arrowUpRight" size={12} />
              </a>
            ) : null}
            <Button
              size="sm"
              variant="outline"
              icon="refresh"
              loading={refreshing || policyState.status === "loading"}
              disabled={savingPolicy}
              onClick={() => void handleRefresh()}
            >
              重新检测
            </Button>
          </div>
        }
      >
        <div className="@container">
          <div className={cn("hidden gap-3 border-b border-cx-border-subtle px-4 py-2 text-cx-xs text-cx-fg-3 @[560px]:grid", gridCols)}>
            <span>引擎</span>
            <span>最低版本</span>
            <span>本机</span>
            {showContainer ? <span>容器镜像</span> : null}
          </div>
          <div className="divide-y divide-cx-border-subtle">
            {engines.map((row) => {
              const local = versionCell(row, row.local);
              const container = versionCell(row, row.container);
              const localBelow = isBelow(row, row.local);
              const containerBelow = row.worker_capable && isBelow(row, row.container);
              const highlighted = activeBackend === "container"
                ? containerBelow
                : activeBackend === "local"
                  ? localBelow
                  : localBelow || containerBelow;

              return (
                <div
                  key={row.engine}
                  data-engine-version-row={row.engine}
                  className={cn("px-4 py-2.5", highlighted && enforced ? "bg-cx-danger/5" : undefined)}
                >
                  <div className={cn("grid grid-cols-2 items-center gap-x-4 gap-y-1.5 text-cx-sm tabular-nums @[560px]:gap-3", gridCols)}>
                    <span className="col-span-2 flex min-w-0 items-center gap-2.5 @[560px]:col-span-1">
                      <EngineLogo engine={row.engine} size={16} />
                      <span className="truncate font-medium text-cx-fg">{row.display_name}</span>
                    </span>
                    <span className="inline-flex min-w-0 items-center gap-1.5">
                      <span className="shrink-0 text-cx-xs text-cx-fg-3 @[560px]:hidden">最低</span>
                      <code className="truncate whitespace-nowrap font-cx-mono text-cx-fg-2" title={row.min_version}>{row.min_version}</code>
                    </span>
                    <span className="inline-flex min-w-0 items-center gap-1.5">
                      <span className="shrink-0 text-cx-xs text-cx-fg-3 @[560px]:hidden">本机</span>
                      <StatusDot tone={local.tone} />
                      <code className="truncate font-cx-mono text-cx-fg-2" title={local.text}>{local.text}</code>
                    </span>
                    {showContainer ? (
                      <span className="inline-flex min-w-0 items-center gap-1.5">
                        <span className="shrink-0 text-cx-xs text-cx-fg-3 @[560px]:hidden">容器</span>
                        {row.worker_capable ? <StatusDot tone={container.tone} /> : null}
                        <code className="truncate font-cx-mono text-cx-fg-2">{row.worker_capable ? container.text : "—"}</code>
                      </span>
                    ) : null}
                  </div>

                  {(localBelow || containerBelow) && row.upgrade_command ? (
                    <div className="mt-2 flex flex-wrap items-center gap-2 text-cx-xs">
                      <span className={localBelow ? "text-cx-danger" : "text-cx-warning"}>
                        {localBelow ? "本机" : "容器镜像"}版本过低
                      </span>
                      <code className="rounded bg-cx-code px-2 py-0.5 font-cx-mono text-cx-xs text-cx-fg">
                        {row.upgrade_command}
                      </code>
                      <Button
                        size="xs"
                        variant="ghost"
                        icon={copiedKey === row.engine ? "check" : "copy"}
                        onClick={() => void copyCmd(row.engine, row.upgrade_command)}
                      >
                        {copiedKey === row.engine ? "已复制" : "复制"}
                      </Button>
                      {containerBelow && !localBelow && onOpenRuntime && activeBackend === "container" ? (
                        <Button size="xs" variant="ghost" onClick={onOpenRuntime}>
                          切换为本地运行
                        </Button>
                      ) : null}
                    </div>
                  ) : null}
                </div>
              );
            })}
          </div>
        </div>
      </SettingsSection>

      <SettingsSection title="低版本拦截">
        <SettingsRow
          title="拦截低于最低版本的 Agent"
          description="关闭后仅提示，不阻止启动。"
          control={
            <Switch
              size="sm"
              checked={enforced}
              disabled={!snapshot || savingPolicy}
              onCheckedChange={(val) => void savePolicy({ enforce: val })}
              ariaLabel="拦截低于最低版本的 Agent"
            />
          }
        />
        {MODE_ROWS.map(({ mode, label }) => (
          <SettingsRow
            key={mode}
            title={
              <span className="inline-flex items-center gap-2">
                <span>{label}</span>
                {focusMode === mode ? <Badge tone="accent">当前模式</Badge> : null}
              </span>
            }
            control={
              <Switch
                size="sm"
                checked={Boolean(snapshot?.policy.modes[mode])}
                disabled={!snapshot || !enforced || savingPolicy}
                onCheckedChange={(val) => void savePolicy({ modes: { [mode]: val } })}
                ariaLabel={`${label} 版本拦截`}
              />
            }
          />
        ))}
      </SettingsSection>
    </div>
  );
}


export function SrcVersionPolicyButton({ workerSettingsHref = "/pentest/workers?return=/src" }: { workerSettingsHref?: string }) {
  const [open, setOpen] = useState(false);
  const policyState = useAgentVersionPolicy();
  const snapshot = policyState.data;
  const enforced = Boolean(snapshot?.policy.enforce && snapshot?.policy.modes.src);
  const belowCount = useMemo(() => {
    if (!snapshot) return 0;
    return snapshot.engines.filter(
      (row) =>
        (row.local.installed_version && compareEngineVersion(row.engine, row.local.installed_version, row.min_version) === -1)
        || (row.worker_capable && row.container.installed_version && compareEngineVersion(row.engine, row.container.installed_version, row.min_version) === -1),
    ).length;
  }, [snapshot]);

  return (
    <>
      <Button
        size="xs"
        variant="ghost"
        icon="shield"
        onClick={() => setOpen(true)}
        data-testid="src-version-policy-button"
      >
        <span>版本支持</span>
        {belowCount > 0 ? (
          <Badge tone={enforced ? "warning" : "neutral"}>{belowCount} 待升级</Badge>
        ) : null}
      </Button>
      <Dialog
        open={open}
        onOpenChange={setOpen}
        size="lg"
        title="Agent 版本"
        footer={<Button variant="primary" onClick={() => setOpen(false)}>完成</Button>}
      >
        <div className="max-h-[72vh] overflow-y-auto pr-1">
          <AgentVersionSupportSection focusMode="src" workerSettingsHref={workerSettingsHref} />
        </div>
      </Dialog>
    </>
  );
}

export function SrcVersionPolicyCard({ workerSettingsHref = "/pentest/workers?return=/src" }: { workerSettingsHref?: string }) {
  const [open, setOpen] = useState(false);
  const policyState = useAgentVersionPolicy();
  const snapshot = policyState.data;
  const enforced = Boolean(snapshot?.policy.enforce && snapshot?.policy.modes.src);
  const localBelow = useMemo(
    () => (snapshot?.engines ?? []).filter(
      (row) => row.local.installed_version && compareEngineVersion(row.engine, row.local.installed_version, row.min_version) === -1,
    ),
    [snapshot],
  );
  const containerBelow = useMemo(
    () => (snapshot?.engines ?? []).filter(
      (row) => row.worker_capable && row.container.installed_version && compareEngineVersion(row.engine, row.container.installed_version, row.min_version) === -1,
    ),
    [snapshot],
  );

  return (
    <>
      <div
        className="mt-3 flex flex-wrap items-start justify-between gap-3 rounded-lg border border-cx-border bg-cx-bg-subtle px-3 py-2.5 text-cx-xs text-cx-fg-2"
        data-testid="src-version-policy-card"
      >
        <div className="min-w-0 flex-1 leading-5">
          <div className="flex flex-wrap items-center gap-2 font-medium text-cx-fg">
            <Icon name="shield" size={13} className="shrink-0 text-cx-accent" />
            <span>Agent 最低版本</span>
            <Badge tone={enforced ? "success" : "neutral"}>
              {enforced ? "拦截已开启" : "仅提示"}
            </Badge>
          </div>
          {localBelow.length > 0 ? (
            <p className="mt-1 text-cx-danger">
              本机低于最低版本：{localBelow.map((r) => r.display_name).join("、")}
            </p>
          ) : containerBelow.length > 0 ? (
            <p className="mt-1 text-cx-warning">
              容器镜像低于最低版本：{containerBelow.map((r) => r.display_name).join("、")}
            </p>
          ) : null}
        </div>
        <div className="flex shrink-0 flex-wrap items-center gap-1.5">
          <Button size="xs" variant="outline" onClick={() => setOpen(true)}>
            查看版本
          </Button>
          <a
            href={workerSettingsHref}
            className="cx-press inline-flex h-6 items-center gap-1 rounded-md border border-cx-border bg-cx-elevated px-2 text-cx-xs font-medium text-cx-fg no-underline hover:border-cx-border-strong"
          >
            出战 Worker 配置
            <Icon name="arrowUpRight" size={11} />
          </a>
        </div>
      </div>
      <Dialog
        open={open}
        onOpenChange={setOpen}
        size="lg"
        title="Agent 版本"
        footer={<Button variant="primary" onClick={() => setOpen(false)}>完成</Button>}
      >
        <div className="max-h-[72vh] overflow-y-auto pr-1">
          <AgentVersionSupportSection focusMode="src" workerSettingsHref={workerSettingsHref} />
        </div>
      </Dialog>
    </>
  );
}
