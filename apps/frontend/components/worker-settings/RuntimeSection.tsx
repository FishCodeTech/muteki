// ai-design-check-ignore-file use-shared-looks -- the image check chip is a small inline status box
"use client";

import { Badge, Button, Callout, IconButton, Input, Select, StatusDot, type Tone } from "@muteki/ui";
import { SettingsRow, SettingsSection } from "@/components/settings/primitives";
import { compareEngineVersion, engineVersionRow, useAgentVersionPolicy } from "@/lib/agentVersionPolicy";
import type { RunnerNode } from "@/lib/runnerNodes";
import type { WorkerImageStatus, WorkerSettings } from "@/lib/useRun";
import { ChoiceCards } from "./ChoiceCards";

const WORKER_IMAGE_ENV = "MUTEKI_WORKER_IMAGE";

type Backend = WorkerSettings["worker_backend"];
type Network = WorkerSettings["worker_network"];
type Scope = WorkerSettings["worker_container_scope"];
export type WorkerQuotas = { memory: string; cpus: string; pids: string; output: string; disk: string };
export const QUOTA_FIELDS: { key: keyof WorkerQuotas; setting: keyof NonNullable<WorkerSettings["worker_resource_effective"]>; title: string; description: string; placeholder: string }[] = [
  { key: "memory", setting: "worker_memory", title: "内存上限", description: "每个 Worker 容器的内存上限，例如 4g、512m", placeholder: "4g" },
  { key: "cpus", setting: "worker_cpus", title: "CPU 上限", description: "可使用的 CPU 核数，可填小数，例如 2 或 1.5", placeholder: "2" },
  { key: "pids", setting: "worker_pids_limit", title: "进程与线程上限", description: "容器内进程和线程总数上限", placeholder: "1024" },
  { key: "output", setting: "worker_output_limit", title: "单次输出上限", description: "单条命令保存的输出上限，超出会终止并保存已捕获的内容", placeholder: "64m" },
  { key: "disk", setting: "worker_disk_limit", title: "工作区磁盘上限", description: "Worker 工作区可写入的总量", placeholder: "8g" },
];

const NETWORKS: { value: Network; label: string; detail: string }[] = [
  { value: "bridge", label: "Bridge", detail: "Docker bridge 网络；Compose 部署下可能加入共享网络" },
  { value: "host", label: "Host", detail: "直接使用宿主机网络，Worker 可访问宿主机上的所有端口" },
];

function ImageCheck({ label, value, tone }: { label: string; value: string; tone: Tone }) {
  return (
    <div className="flex min-w-0 items-center gap-2 rounded-lg border border-cx-border-subtle bg-cx-bg px-3 py-2">
      <StatusDot tone={tone} />
      <span className="min-w-0 flex-1 truncate text-cx-sm text-cx-fg-2">{label}</span>
      <span className="shrink-0 truncate text-cx-xs text-cx-fg-3" title={value}>{value}</span>
    </div>
  );
}

function ImageCapabilities({ status }: { status: WorkerImageStatus }) {
  const versionPolicy = useAgentVersionPolicy();
  const caps = status.capabilities;
  const facts = [
    status.architecture ? `架构 ${status.architecture}` : "",
    status.size_bytes ? `大小 ${(status.size_bytes / 1024 ** 3).toFixed(2)} GiB` : "",
    status.created ? `构建于 ${new Date(status.created).toLocaleString()}` : "",
  ].filter(Boolean).join(" · ");
  if (!caps) return facts ? <p className="text-cx-xs text-cx-fg-3">{facts}</p> : null;
  if (!caps.declared) return <p className="text-cx-xs text-cx-fg-3">{facts}{facts ? " · " : ""}镜像未声明能力清单，引擎和工具以启动时实际检查为准。</p>;
  if (caps.error) return <Callout tone="warning" title="镜像能力清单不可读">{caps.error}</Callout>;
  const engines = Object.entries(caps.engines ?? {});
  const browser = Object.entries(caps.browser ?? {});
  const tools = caps.tools && typeof caps.tools === "object" ? Object.entries(caps.tools as Record<string, unknown>) : [];
  const outdatedEngines: string[] = [];
  return (
    <div className="flex flex-col gap-2">
      {facts ? <p className="text-cx-xs text-cx-fg-3">{facts}</p> : null}
      {engines.length ? (
        <div className="flex flex-wrap items-center gap-1.5">
          <span className="text-cx-xs text-cx-fg-3">Agent 引擎</span>
          {engines.map(([name, row]) => {
            const policyRow = engineVersionRow(versionPolicy.data, name);
            const minVer = policyRow?.min_version || "";
            const isBelow = Boolean(row.available && row.version && minVer && compareEngineVersion(name, row.version, minVer) === -1);
            if (isBelow) outdatedEngines.push(`${name} (${row.version} < ${minVer})`);
            return (
              <Badge key={name} tone={!row.available ? "danger" : isBelow ? "warning" : "success"}>
                <span title={isBelow ? `当前镜像版本 ${row.version} 低于最低要求 ${minVer}（容器模式下禁止设置出战 Worker）` : row.version ?? undefined}>
                  {isBelow ? `${name} (${row.version} < ${minVer})` : name}
                </span>
              </Badge>
            );
          })}
        </div>
      ) : null}
      {outdatedEngines.length ? (
        <p className="text-cx-xs text-cx-warning">
          镜像内 {outdatedEngines.join("、")} 低于最低版本，请重建 / 拉取新版镜像或切换为本地运行。
        </p>
      ) : null}
      {browser.length ? (
        <div className="flex flex-wrap items-center gap-1.5">
          <span className="text-cx-xs text-cx-fg-3">浏览器</span>
          {browser.map(([name, ok]) => <Badge key={name} tone={ok ? "success" : "danger"}>{name}</Badge>)}
        </div>
      ) : null}
      {tools.length ? (
        <div className="flex flex-wrap items-center gap-1.5">
          <span className="text-cx-xs text-cx-fg-3">常用工具</span>
          {tools.map(([name, path]) => <Badge key={name} tone={path ? "neutral" : "warning"}>{path ? name : `${name}（缺）`}</Badge>)}
        </div>
      ) : null}
      <p className="text-cx-xs text-cx-fg-4">能力清单在镜像构建时生成，仅记录构建时成功启动的组件。</p>
    </div>
  );
}

export function RuntimeSection({
  backend, network, effectiveNetwork, networkError, containerScope, seatCount,
  imageStatus, imageLoading, pulling, inContainer,
  nodeId, nodes, ociRuntime, quotas, quotaDefaults, migrationNotices,
  onBackend, onNetwork, onContainerScope, onRefreshImage, onPullImage,
  onNodeId, onOciRuntime, onQuota, onOpenNodes,
}: {
  backend: Backend;
  network: Network;
  effectiveNetwork?: string;
  networkError?: string;
  containerScope: Scope;
  seatCount: number;
  imageStatus: WorkerImageStatus | null;
  imageLoading: boolean;
  pulling: boolean;
  inContainer: boolean;
  nodeId: string;
  nodes: RunnerNode[] | null;
  ociRuntime: string;
  quotas: WorkerQuotas;
  quotaDefaults?: WorkerSettings["worker_resource_effective"];
  migrationNotices: NonNullable<WorkerSettings["migration_notices"]>;
  onBackend: (backend: Backend) => void;
  onNetwork: (network: Network) => void;
  onContainerScope: (scope: Scope) => void;
  onRefreshImage: () => void;
  onPullImage: () => void;
  onNodeId: (nodeId: string) => void;
  onOciRuntime: (runtime: string) => void;
  onQuota: (key: keyof WorkerQuotas, value: string) => void;
  onOpenNodes: () => void;
}) {
  const versionStatus = imageStatus?.version.status;
  const checking = (ok: boolean, okText: string, badText: string): { value: string; tone: Tone } =>
    imageLoading ? { value: "检查中", tone: "neutral" } : ok ? { value: okText, tone: "success" } : { value: badText, tone: "danger" };
  const daemon = checking(Boolean(imageStatus?.daemon.ok), "可用", "不可用");
  const pulled = checking(Boolean(imageStatus?.pulled.ok), "已存在", "未找到");
  const version: { value: string; tone: Tone } = imageLoading
    ? { value: "检查中", tone: "neutral" }
    : { value: imageStatus?.version.actual || "未标记", tone: versionStatus === "match" ? "success" : versionStatus === "mismatch" ? "danger" : "neutral" };
  const networkDetail = NETWORKS.find((option) => option.value === network)?.detail;
  const remote = nodeId !== "local";
  const selectedNode = nodes?.find((node) => node.id === nodeId);
  const nodeOptions = [
    { value: "local", label: "本机" },
    ...(nodes ?? []).filter((node) => node.state === "active" || node.id === nodeId).map((node) => ({
      value: node.id,
      label: `${node.name}${node.state !== "active" ? "（不可调度）" : node.connected ? "" : "（离线）"}`,
    })),
  ];
  const nodeDescription = !remote
    ? "Worker 容器在 Web 服务所在的机器上运行。"
    : !selectedNode ? "所选节点已不存在，新任务会被拒绝，请改选其他节点。"
      : selectedNode.state !== "active" ? "所选节点未处于可调度状态，新任务会被拒绝。"
        : selectedNode.connected ? "Worker 容器在该远程节点上运行，使用该节点的 Docker 和镜像。"
          : "节点当前离线；在它重新连接前，新任务会启动失败。";

  return (
    <div className="flex flex-col gap-8">
      {inContainer ? (
        <Callout tone="warning" icon="layers" title="当前只能使用容器 Worker">当前部署无法直接调用宿主机 CLI。</Callout>
      ) : null}
      {migrationNotices.map((notice) => (
        <Callout key={notice.field} tone="warning" title="旧配置需要更新">{notice.detail}</Callout>
      ))}

      <SettingsSection
        title="执行环境"
        description="决定 Worker 在哪里运行。切换后需要保存才会用于下次任务。"
        actions={<Badge>{seatCount} 个 Worker</Badge>}
        bare
      >
        <div>
          <ChoiceCards<Backend>
            ariaLabel="执行环境"
            value={backend}
            onChange={onBackend}
            options={[
              { value: "local", icon: "terminal", title: "本地运行", description: inContainer ? "当前部署不支持宿主机 CLI" : remote ? "远程节点只支持容器运行" : "使用宿主机 CLI，可继承系统登录", disabled: inContainer || remote },
              { value: "container", icon: "layers", title: "容器运行", description: "隔离运行，需要绑定可注入凭据；可选择本机或远程节点" },
            ]}
          />
        </div>
      </SettingsSection>

      {backend === "container" ? (
        <>
          <SettingsSection title="执行节点" actions={<Button size="xs" variant="ghost" iconRight="arrowRight" onClick={onOpenNodes}>管理节点</Button>}>
            <SettingsRow
              title="新任务运行在"
              description={nodeDescription}
              control={<Select<string> ariaLabel="执行节点" className="w-52" value={nodeId} options={nodeOptions} onChange={onNodeId} />}
            />
            {!nodes?.length ? (
              <SettingsRow title="还没有远程节点" description="在“执行节点”中开启远程接入并注册其他机器后，可以把 Worker 放到那台机器上运行。" />
            ) : null}
          </SettingsSection>

          <SettingsSection title="容器设置" actions={<Badge tone="accent">Docker</Badge>}>
            <SettingsRow
              title="容器作用域"
              description={containerScope === "shared" ? "互信任务共用一个容器和资源上限，可互相读取文件和凭据" : "每个任务使用独立容器"}
              control={
                <Select<Scope>
                  ariaLabel="容器作用域"
                  className="w-44"
                  value={containerScope}
                  onChange={onContainerScope}
                  options={[{ value: "run", label: "每个任务独立" }, { value: "shared", label: "互信任务共享" }]}
                />
              }
            />
            <SettingsRow
              title="Worker 权限"
              description="所有 Agent 及其工具直接以容器内 root 执行，可安装软件、扫描网络和调试程序。"
              control={<Badge tone="accent">root</Badge>}
            />
            <SettingsRow
              title="容器网络"
              description={`${networkDetail ?? ""}${effectiveNetwork && effectiveNetwork !== network ? ` · 实际生效：${effectiveNetwork}` : ""}${networkError ? ` · ${networkError}` : ""}`}
              control={
                <Select<Network>
                  ariaLabel="容器网络"
                  className="w-44"
                  value={network}
                  onChange={onNetwork}
                  options={NETWORKS.map(({ value, label }) => ({ value, label }))}
                />
              }
            />
            <SettingsRow
              title="OCI 运行时"
              description={<>留空使用 Docker 默认运行时。填写 runsc（gVisor）、kata 等时，启动前会检查{remote ? "远程节点" : "本机"} Docker 是否已注册该运行时，未注册会拒绝启动。</>}
              control={<Input className="w-44 font-cx-mono" size="sm" aria-label="OCI 运行时" value={ociRuntime} placeholder="默认" maxLength={64} onChange={(event) => onOciRuntime(event.target.value.trim())} />}
            />
            <div className="px-5 py-3">
              <Callout tone="neutral" title="同一容器内的 Worker 不互相隔离">
                同一任务（或共享作用域下互信任务）的 Worker 以同一系统用户运行，可以读取彼此注入的凭据和工作文件。不要把不互信的账号放进同一个任务。
              </Callout>
            </div>
          </SettingsSection>

          <SettingsSection title="容器配额" description="留空使用默认值；默认值来自镜像类型或 MUTEKI_WORKER_* 环境变量。修改只影响之后创建的容器。">
            {QUOTA_FIELDS.map((field) => (
              <SettingsRow
                key={field.key}
                title={field.title}
                description={`${field.description}${quotaDefaults ? `；当前生效 ${String(quotaDefaults[field.setting])}` : ""}`}
                control={<Input className="w-32 font-cx-mono" size="sm" aria-label={field.title} value={quotas[field.key]} placeholder={quotaDefaults ? String(quotaDefaults[field.setting]) : field.placeholder} onChange={(event) => onQuota(field.key, event.target.value)} />}
              />
            ))}
          </SettingsSection>

          <SettingsSection
            title="Worker 镜像"
            description={remote
              ? "远程节点使用它自己的 Docker 镜像；下面显示的是本机镜像状态。请在远程节点上用 docker pull 准备同一镜像。"
              : <>可用 <code className="rounded bg-cx-hover px-1 py-px font-cx-mono text-cx-xs">{WORKER_IMAGE_ENV}</code> 指定镜像。</>}
            actions={<IconButton icon="refresh" label="刷新镜像状态" disabled={imageLoading || pulling} onClick={onRefreshImage} />}
          >
            <div className="flex flex-col gap-3 p-4">
              <code className="block truncate rounded-lg border border-cx-border-subtle bg-cx-code px-3 py-2 font-cx-mono text-cx-xs text-cx-fg-2" title={imageStatus?.image}>
                {imageStatus?.image || "读取中…"}
              </code>
              <div className="grid gap-2 @[560px]:grid-cols-3">
                <ImageCheck label="Docker 服务" {...daemon} />
                <ImageCheck label="本地镜像" {...pulled} />
                <ImageCheck label="镜像版本" {...version} />
              </div>
              {versionStatus === "mismatch" && imageStatus?.version.expected ? (
                <Callout tone="warning" title={`期望版本 ${imageStatus.version.expected}`}>当前镜像版本与期望版本不一致，建议重新拉取镜像。</Callout>
              ) : null}
              {imageStatus?.pulled.ok && !imageLoading ? <ImageCapabilities status={imageStatus} /> : null}
              <div>
                <Button size="sm" variant="primary" icon="download" loading={pulling} disabled={pulling || !imageStatus?.daemon.ok} onClick={onPullImage}>
                  {pulling ? "拉取中…" : "拉取镜像"}
                </Button>
              </div>
            </div>
          </SettingsSection>
        </>
      ) : null}
    </div>
  );
}
