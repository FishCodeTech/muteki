// ai-design-check-ignore-file use-shared-looks -- the codebase has no shared Heading/Tile; these are small in-sheet group labels and a key-value box
"use client";

import { useState, type ReactNode } from "react";
import { EngineLogo } from "@/components/EngineLogo";
import { Icon } from "@muteki/ui";
import { ModelTestTerminal } from "@/components/ModelTestTerminal";
import { NumberField } from "@/components/NumberField";
import { Button, Callout, Input, Select, Sheet, Switch, type ListOption } from "@muteki/ui";
import { copyToClipboard } from "@/lib/clipboard";
import { cn } from "@/lib/cn";
import {
  useAgentVersionPolicy, workerVersionIssue, type PolicyModeId,
} from "@/lib/agentVersionPolicy";
import {
  descriptorForEngine, engineDisplayName, readyCatalog, useProviderDescriptors,
} from "@/lib/providerDescriptors";
import type { GlobalCredential, WorkerModelOptions, WorkerModelTestResult, WorkerSettings } from "@/lib/useRun";
import { CheckStatus } from "./CheckStatus";
import { FieldLabel, SeatFields } from "./SeatFields";
import {
  credentialKey, engineLocalOnly, engineOf, engineProtocolLabel, findGlobalCredential, globalCredentialUsable, isOrdinarySeat, workerEngines,
  type Credential, type ModelDiscoveryOutcome, type ReviewPolicy, type Seat, type VerifierPolicy,
} from "./model";

function Group({ title, hint, children }: { title: string; hint?: ReactNode; children: ReactNode }) {
  return (
    <section className="flex flex-col gap-3">
      <div className="flex items-baseline justify-between gap-2">
        <h3 className="text-cx-xs font-medium text-cx-fg-3">{title}</h3>
        {hint ? <span className="text-cx-xs text-cx-fg-4">{hint}</span> : null}
      </div>
      {children}
    </section>
  );
}

function SwitchLine({ title, description, checked, onChange }: { title: string; description?: string; checked: boolean; onChange: (next: boolean) => void }) {
  return (
    <div className="flex items-center justify-between gap-4">
      <div className="min-w-0">
        <div className="text-cx-sm text-cx-fg">{title}</div>
        {description ? <div className="text-cx-xs leading-4 text-cx-fg-3">{description}</div> : null}
      </div>
      <Switch size="sm" checked={checked} onCheckedChange={onChange} ariaLabel={title} />
    </div>
  );
}

function NumberLine({ title, description, ...field }: { title: string; description?: string; value: number; min?: number; max?: number; suffix?: string; onChange: (next: number) => void }) {
  return (
    <div className="flex items-center justify-between gap-4">
      <div className="min-w-0">
        <div className="text-cx-sm text-cx-fg">{title}</div>
        {description ? <div className="text-cx-xs leading-4 text-cx-fg-3">{description}</div> : null}
      </div>
      <div className="w-[132px] shrink-0">
        <NumberField ariaLabel={title} min={field.min} max={field.max} suffix={field.suffix} value={field.value} onChange={(next) => field.onChange(Number(next) || 0)} />
      </div>
    </div>
  );
}

type BindingProps = {
  credentials: Credential[];
  availableCredentials: GlobalCredential[];
  models: WorkerModelOptions;
  backend: WorkerSettings["worker_backend"];
  mode?: PolicyModeId;
  nodeId?: string;
  discovering: boolean;
  onDiscoverModels: (credentialId: string, engine: string) => Promise<ModelDiscoveryOutcome>;
  onOpenCredentials: () => void;
  onOpenVersions?: () => void;
};

function SeatBinding({
  seat, onUpdate, onBind, onEngine, ...binding
}: BindingProps & {
  seat: Seat;
  onUpdate: (patch: Partial<Seat>) => void;
  onBind: (key: string, credential?: GlobalCredential) => void;
  onEngine: (engine: string) => void;
}) {
  const catalog = readyCatalog(useProviderDescriptors());
  const versionPolicy = useAgentVersionPolicy();
  const [discovery, setDiscovery] = useState<ModelDiscoveryOutcome | null>(null);
  const [copiedUpgrade, setCopiedUpgrade] = useState(false);
  const engine = engineOf(seat.engine);
  const currentVersionIssue = workerVersionIssue(versionPolicy.data, engine, binding.backend, binding.mode, binding.nodeId);
  const credential = binding.credentials.find((item) => item.id === seat.credential_id);
  const accountKey = credentialKey(credential);
  const selected = findGlobalCredential(binding.availableCredentials, engine, accountKey);
  const usable = globalCredentialUsable(selected);
  const canDiscover = descriptorForEngine(catalog, engine)?.models.method === "cli";
  const engineOptions: ListOption[] = workerEngines(catalog).map((item) => {
    const verIssue = workerVersionIssue(versionPolicy.data, item, binding.backend, binding.mode, binding.nodeId);
    return {
      value: item,
      label: engineDisplayName(catalog, item),
      description: engineLocalOnly(catalog, item)
        ? "仅支持本地运行"
        : verIssue
          ? `${engineProtocolLabel(catalog, item)} · 版本过低（${verIssue.installedVersion} < ${verIssue.minVersion}，禁止出战）`
          : engineProtocolLabel(catalog, item),
      leading: <EngineLogo engine={item} size={16} />,
      disabled: (engineLocalOnly(catalog, item) && binding.backend !== "local") || (item !== engine && Boolean(verIssue)),
    };
  });
  return (
    <>
      {currentVersionIssue ? (
        <Callout
          tone="danger"
          icon="alert"
          title="低于最低支持版本 · 禁止设置出战 Worker"
          action={binding.onOpenVersions ? (
            <Button size="xs" variant="outline" onClick={binding.onOpenVersions}>版本要求</Button>
          ) : undefined}
        >
          <div className="flex flex-col gap-1.5 text-cx-xs leading-5">
            <span>
              当前版本 <code className="font-cx-mono">{currentVersionIssue.installedVersion}</code> 低于最低要求 <code className="font-cx-mono">{currentVersionIssue.minVersion}</code>。
              {currentVersionIssue.backend === "container" ? currentVersionIssue.containerUpgradeNote : currentVersionIssue.upgradeNote}
            </span>
            {currentVersionIssue.upgradeCommand ? (
              <div className="flex flex-wrap items-center gap-1.5">
                <code className="rounded bg-cx-code px-1.5 py-0.5 font-cx-mono text-cx-2xs">{currentVersionIssue.upgradeCommand}</code>
                <Button
                  size="xs"
                  variant="ghost"
                  icon={copiedUpgrade ? "check" : "copy"}
                  onClick={async () => {
                    if (await copyToClipboard(currentVersionIssue.upgradeCommand)) {
                      setCopiedUpgrade(true);
                      window.setTimeout(() => setCopiedUpgrade(false), 1800);
                    }
                  }}
                >
                  {copiedUpgrade ? "已复制" : "复制升级命令"}
                </Button>
              </div>
            ) : null}
          </div>
        </Callout>
      ) : null}

      <Group title="基本信息">
        <div>
          <FieldLabel>名称</FieldLabel>
          <Input value={seat.label} onChange={(event) => onUpdate({ label: event.target.value })} aria-label="名称" />
        </div>
        <div>
          <FieldLabel>Worker 程序</FieldLabel>
          <Select
            value={engine}
            onChange={onEngine}
            options={engineOptions}
            ariaLabel="Worker 程序"
            className="w-full"
            popoverClassName="w-[320px]"
          />
        </div>
      </Group>

      <Group title="模型与连接">
        <div className="grid grid-cols-1 gap-3">
          <SeatFields
            labelled seat={seat} credentials={binding.credentials} availableCredentials={binding.availableCredentials}
            models={binding.models} backend={binding.backend} accountKey={accountKey}
            onUpdate={onUpdate} onBind={onBind} onOpenCredentials={binding.onOpenCredentials}
          />
        </div>
        <div>
          <FieldLabel>手动输入模型 ID</FieldLabel>
          <Input
            value={seat.model || ""}
            placeholder="留空使用 Runtime 默认模型"
            aria-label="模型 ID"
            onChange={(event) => onUpdate({ model: event.target.value.trim() })}
            disabled={!usable}
          />
          <p className="mt-1 text-cx-xs text-cx-fg-4">自定义端点可直接填写服务实际支持的模型 ID。</p>
        </div>
        {selected ? (
          <div className="rounded-lg border border-cx-border-subtle bg-cx-bg-subtle px-3 py-2 text-cx-xs text-cx-fg-3">
            <div className="flex justify-between gap-3"><span>服务地址</span><span className="min-w-0 truncate text-cx-fg-2" title={selected.base_url || "默认服务地址"}>{selected.base_url || "默认服务地址"}</span></div>
            <div className="mt-1 flex justify-between gap-3"><span>协议</span><span className="text-cx-fg-2">{engineProtocolLabel(catalog, engine) || "—"}</span></div>
            <div className="mt-1 flex justify-between gap-3"><span>状态</span><span className={usable ? "text-cx-fg-2" : "text-cx-danger"}>{usable ? "可用" : "不可用"}</span></div>
          </div>
        ) : null}
        <div className="flex items-center gap-2">
          <Button
            size="sm" variant="outline" icon="refresh" loading={binding.discovering}
            disabled={!canDiscover || !usable || !selected}
            onClick={async () => { if (selected) { setDiscovery(null); setDiscovery(await binding.onDiscoverModels(selected.id, engine)); } }}
          >
            {canDiscover ? "刷新模型列表" : "不支持刷新"}
          </Button>
          <Button size="sm" variant="ghost" icon="arrowUpRight" onClick={binding.onOpenCredentials}>管理凭据</Button>
        </div>
        {discovery ? (
          <p className={cn("flex items-start gap-1.5 text-cx-xs", discovery.ok ? "text-cx-success" : "text-cx-danger")}>
            <Icon name={discovery.ok ? "check" : "alert"} size={13} className="mt-0.5 shrink-0" />{discovery.detail}
          </p>
        ) : null}
      </Group>
    </>
  );
}

function CheckGroup({ title, enabled, testing, result, blocker, onTest }: {
  title: string;
  enabled: boolean;
  testing: boolean;
  result?: WorkerModelTestResult;
  blocker?: string;
  onTest: () => void;
}) {
  return (
    <Group title={title}>
      <div className="flex items-center justify-between gap-3">
        <CheckStatus enabled={enabled} testing={testing} result={result} className="-ml-1.5" />
        <Button size="sm" variant="outline" icon="plug" loading={testing} disabled={testing || Boolean(blocker)} onClick={onTest}>
          {blocker || "真实模型自检"}
        </Button>
      </div>
      {testing || result ? <ModelTestTerminal testing={testing} result={result ?? null} /> : null}
    </Group>
  );
}

export function SeatSheet({
  seat, open, onOpenChange, testing, testResult, onUpdate, onBind, onEngine, onTest, onDuplicate, onDelete, ...binding
}: BindingProps & {
  seat: Seat | null;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  testing: boolean;
  testResult?: WorkerModelTestResult;
  onUpdate: (patch: Partial<Seat>) => void;
  onBind: (key: string, credential?: GlobalCredential) => void;
  onEngine: (engine: string) => void;
  onTest: () => void;
  onDuplicate: () => void;
  onDelete: () => void;
}) {
  const versionPolicy = useAgentVersionPolicy();
  const credential = seat ? binding.credentials.find((item) => item.id === seat.credential_id) : undefined;
  const selected = seat ? findGlobalCredential(binding.availableCredentials, engineOf(seat.engine), credentialKey(credential)) : null;
  const verIssue = seat ? workerVersionIssue(versionPolicy.data, engineOf(seat.engine), binding.backend, binding.mode, binding.nodeId) : null;
  const blocker = verIssue
    ? "版本低于最低要求，请先升级"
    : !globalCredentialUsable(selected)
      ? "请先选择可用凭据"
      : selected.connection === "custom_endpoint" && !seat?.model?.trim() ? "请先选择模型" : "";
  return (
    <Sheet
      open={open && Boolean(seat)}
      onOpenChange={onOpenChange}
      modal={false}
      dismissOnOutside
      width={440}
      title={seat?.label}
      description="Worker 详细设置"
      bodyClassName="px-4 py-4"
      footer={seat ? (
        <div className="flex items-center justify-between gap-2 px-4 py-3">
          <Button size="sm" variant="ghost" icon="copy" onClick={onDuplicate}>复制</Button>
          <Button size="sm" variant="danger-soft" icon="trash" onClick={onDelete}>移除</Button>
        </div>
      ) : undefined}
    >
      {seat ? (
        <div className="flex flex-col gap-6">
          <SeatBinding seat={seat} onUpdate={onUpdate} onBind={onBind} onEngine={onEngine} {...binding} />
          <CheckGroup title="自检" enabled={seat.enabled} testing={testing} result={testResult} blocker={blocker} onTest={onTest} />
        </div>
      ) : null}
    </Sheet>
  );
}

export function AuxiliarySheet({
  kind, open, onOpenChange, seats, review, verifier, testing, testResult,
  onReview, onVerifier, onSeatUpdate, onSeatBind, onSeatEngine, onEditOrdinary, onTest, ...binding
}: BindingProps & {
  kind: "review" | "verifier";
  open: boolean;
  onOpenChange: (open: boolean) => void;
  seats: Seat[];
  review: ReviewPolicy;
  verifier: VerifierPolicy;
  testing: boolean;
  testResult?: WorkerModelTestResult;
  onReview: (patch: Partial<ReviewPolicy>) => void;
  onVerifier: (patch: Partial<VerifierPolicy>) => void;
  onSeatUpdate: (id: string, patch: Partial<Seat>) => void;
  onSeatBind: (id: string, key: string, credential?: GlobalCredential) => void;
  onSeatEngine: (id: string, engine: string) => void;
  onEditOrdinary: (id: string) => void;
  onTest: () => void;
}) {
  const versionPolicy = useAgentVersionPolicy();
  const isReview = kind === "review";
  const policy = isReview ? review : verifier;
  const seat = seats.find((item) => item.id === policy.engine) ?? null;
  const dedicated = Boolean(seat && !isOrdinarySeat(seat));
  const credential = seat ? binding.credentials.find((item) => item.id === seat.credential_id) : undefined;
  const selected = seat ? findGlobalCredential(binding.availableCredentials, engineOf(seat.engine), credentialKey(credential)) : null;
  const verIssue = seat ? workerVersionIssue(versionPolicy.data, engineOf(seat.engine), binding.backend, binding.mode, binding.nodeId) : null;
  const blocker = !seat
    ? "请先选择 Worker"
    : verIssue
      ? "版本低于最低要求，请先升级"
      : !globalCredentialUsable(selected) ? "请先选择可用凭据" : "";
  const update = (patch: Partial<ReviewPolicy & VerifierPolicy>) => (isReview ? onReview(patch as Partial<ReviewPolicy>) : onVerifier(patch as Partial<VerifierPolicy>));
  const fallbackKey = isReview ? "allow_review_fallback" : "allow_verifier_fallback";
  const fallbackOn = Boolean((policy as Record<string, unknown>)[fallbackKey]);
  return (
    <Sheet
      open={open}
      onOpenChange={onOpenChange}
      modal={false}
      dismissOnOutside
      width={440}
      title={isReview ? "Review Worker" : "Verifier Worker"}
      description={isReview ? "审查建议 · BTW 自动模式候选" : "独立复现已提交的漏洞报告"}
      bodyClassName="px-4 py-4"
    >
      <div className="flex flex-col gap-6">
        <Callout tone="neutral" icon="info">
          {isReview ? "仅 Pentest 生效，占用 1 个总并发名额。" : "仅 Pentest 生效，独立复现报告并占用总并发名额。"}
        </Callout>

        <Group title="运行策略">
          <NumberLine
            title="超时" suffix="秒" min={isReview ? 30 : 60} max={isReview ? 90 : undefined}
            value={policy.timeout ?? (isReview ? 90 : 240)}
            onChange={(next) => update({ timeout: Math.max(isReview ? 30 : 60, isReview ? Math.min(90, next || 90) : next || 240) })}
          />
          <SwitchLine title="不可用时降级" description={`改用下一个健康的 ${isReview ? "Review" : "Verifier"} 配置`} checked={fallbackOn} onChange={(value) => update({ [fallbackKey]: value })} />
        </Group>

        {isReview ? (
          <Group title="触发条件">
            <SwitchLine title="候选结果突然增加" checked={Boolean(review.on_candidate_spike)} onChange={(value) => onReview({ on_candidate_spike: value })} />
            <SwitchLine title="证据出现冲突" checked={Boolean((review as Record<string, unknown>).on_evidence_conflict)} onChange={(value) => onReview({ on_evidence_conflict: value } as Partial<ReviewPolicy>)} />
            <NumberLine title="连续无成果 Worker" min={0} value={review.after_fruitless_workers ?? 0} onChange={(next) => onReview({ after_fruitless_workers: Math.max(0, next) })} />
            <NumberLine title="重复分支阈值" min={0} value={review.after_duplicate_intents ?? 0} onChange={(next) => onReview({ after_duplicate_intents: Math.max(0, next) })} />
            <NumberLine title="候选结果阈值" min={1} value={review.candidate_spike_threshold ?? 5} onChange={(next) => onReview({ candidate_spike_threshold: Math.max(1, next) })} />
          </Group>
        ) : null}

        {seat ? (dedicated ? (
          <SeatBinding
            seat={seat} {...binding}
            onUpdate={(patch) => onSeatUpdate(seat.id, patch)}
            onBind={(key, global) => onSeatBind(seat.id, key, global)}
            onEngine={(engine) => onSeatEngine(seat.id, engine)}
          />
        ) : (
          <Group title="连接与模型">
            <p className="text-cx-xs leading-5 text-cx-fg-3">沿用「{seat.label}」的凭据、模型和运行环境，只可单独覆盖推理强度。</p>
            <div><Button size="sm" variant="outline" iconRight="chevronRight" onClick={() => onEditOrdinary(seat.id)}>编辑 {seat.label}</Button></div>
          </Group>
        )) : null}

        <CheckGroup title="自检" enabled={Boolean(policy.enabled && seat?.enabled)} testing={testing} result={testResult} blocker={blocker} onTest={onTest} />
      </div>
    </Sheet>
  );
}
