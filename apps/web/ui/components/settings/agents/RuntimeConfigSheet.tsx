"use client";

import { useState } from "react";
import { Badge, Button, Callout, Checkbox, Collapse, CopyButton, Select, Sheet, TextArea, TextField, Tooltip, type ListOption } from "@/components/chat/ui";
import { Icon } from "@/components/Icon";
import { cn } from "@/lib/cn";
import type { adapterDescriptor } from "@/lib/providerDescriptors";
import { EnvRefsEditor, envRefRows, envRefsPayload, type EnvRefRow } from "./EnvRefsEditor";
import {
  CAPABILITY_LABELS, installedVersion, runtimeLabel, versionCheckDetail, versionStatus,
  type EngineSummary, type RuntimeInstance, type TransportField,
} from "./shared";

const ACCESS_MODE_LABELS: Record<string, string> = {
  supervised: "严格监督", "auto-accept-edits": "自动接受编辑", auto: "自动审查", "full-access": "完全访问",
};

export type RuntimeValues = {
  binary_path: string; endpoint: string; transport: Record<string, string | number | boolean>;
  env_refs?: Record<string, string>; launch_args?: string[];
};

/** One argument per line; blank lines are ignored and the service validates the rest. */
const launchArgsPayload = (text: string): string[] => text.split("\n").map(line => line.trim()).filter(Boolean);

/** Binary path, endpoint, transport, environment and diagnostics of one runtime instance. Remount (key) per instance. */
export function RuntimeConfigSheet({
  open, onClose, engine, instance, adapter, fallbackKey, transportFields, connectionOptions, accessModes, envEditable, showAccessModes,
  probing, refreshing, onSwitchAdapter, onProbe, onSave,
}: {
  open: boolean;
  onClose: () => void;
  engine: EngineSummary;
  instance: RuntimeInstance | null;
  adapter: ReturnType<typeof adapterDescriptor>;
  fallbackKey: string;
  transportFields: Record<string, TransportField>;
  connectionOptions: ListOption[];
  accessModes: string[];
  envEditable: boolean;
  showAccessModes: boolean;
  probing: boolean;
  refreshing: boolean;
  onSwitchAdapter: (adapterId: string) => void;
  onProbe: () => void;
  onSave: (values: RuntimeValues, baseRevision: string) => Promise<string | void>;
}) {
  const [baseRevision, setBaseRevision] = useState(instance?.updated_at || "");
  const [binaryPath, setBinaryPath] = useState(instance?.binary_path || "");
  const [endpoint, setEndpoint] = useState(instance?.endpoint || "");
  const [transport, setTransport] = useState<Record<string, string | number | boolean>>({ ...(instance?.transport || {}) });
  const [envRefs, setEnvRefs] = useState<EnvRefRow[]>(() => envRefRows(instance?.env_refs));
  const [launchArgs, setLaunchArgs] = useState((instance?.launch_args || []).join("\n"));
  const [advanced, setAdvanced] = useState(false);
  const [capabilitiesOpen, setCapabilitiesOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const stale = (instance?.updated_at || "") !== baseRevision;
  const capabilityEntries = Object.entries(instance?.health?.capabilities || {});
  const transportEntries = Object.entries(transportFields);
  const health = instance?.health;

  const reload = () => {
    setBaseRevision(instance?.updated_at || ""); setBinaryPath(instance?.binary_path || ""); setEndpoint(instance?.endpoint || "");
    setTransport({ ...(instance?.transport || {}) }); setEnvRefs(envRefRows(instance?.env_refs)); setLaunchArgs((instance?.launch_args || []).join("\n"));
  };
  const save = async () => {
    const env = envRefsPayload(envRefs);
    if (envEditable && env.error) { setError(`环境变量 ${env.error}`); setAdvanced(true); return; }
    setBusy(true); setError("");
    try {
      const revision = await onSave({
        binary_path: binaryPath.trim(), endpoint: endpoint.trim(), transport,
        ...(envEditable ? { env_refs: env.refs, launch_args: launchArgsPayload(launchArgs) } : {}),
      }, baseRevision);
      if (revision) setBaseRevision(revision);
    } catch (cause) { setError(cause instanceof Error ? cause.message : String(cause)); } finally { setBusy(false); }
  };

  return (
    <Sheet
      open={open} onOpenChange={next => { if (!next) onClose(); }} width={500}
      title={`${engine.label} · 配置`} description={instance?.key || fallbackKey}
      footer={
        <div className="flex items-center gap-2">
          <Button size="sm" variant="outline" icon="refresh" loading={probing} disabled={refreshing} onClick={onProbe}>重新探测</Button>
          <span className="flex-1" />
          <Button size="sm" variant="ghost" onClick={onClose}>关闭</Button>
          <Button size="sm" variant="primary" icon="check" loading={busy} onClick={() => void save()}>保存</Button>
        </div>
      }
      bodyClassName="px-4 py-4"
    >
      <div className="flex flex-col gap-5">
        {error ? <Callout tone="danger" icon="alert" role="alert" onDismiss={() => setError("")}>{error}</Callout> : null}
        {stale ? <Callout tone="warning" icon="alert" role="alert" action={<Button size="xs" variant="outline" onClick={reload}>载入最新配置</Button>}>运行配置已在其它视图更新，请重新核对。</Callout> : null}

        <div className="flex items-start gap-2.5 rounded-xl border border-cx-border bg-cx-bg-subtle px-3 py-2.5">
          <Icon name={health?.healthy ? "checkCircle" : "info"} size={15} className={cn("mt-0.5 shrink-0", health?.healthy ? "text-cx-success" : "text-cx-fg-3")} />
          <div className="min-w-0 flex-1">
            <p className="flex flex-wrap items-center gap-1.5 text-[12.5px] font-medium text-cx-fg">
              {health?.healthy ? "运行环境健康" : "尚未得到健康结果"}
              {engine.version ? <code className="rounded-md bg-cx-code px-1.5 py-0.5 text-[11.5px] font-normal text-cx-fg-2" title={versionCheckDetail(health?.version_check)}>{installedVersion(engine.version, health?.version_check)}</code> : null}
              {instance ? <Badge tone="neutral">{runtimeLabel(instance)}</Badge> : null}
            </p>
            <p className="mt-0.5 text-[12px] leading-5 text-cx-fg-3">
              {health?.detail || instance?.auth.detail || "保存后执行探测，以读取版本、登录状态和运行能力。"}{instance ? ` ${versionStatus(health?.version_check)}` : ""}
            </p>
          </div>
        </div>

        {instance?.auth.login_command ? (
          <div className="flex min-w-0 items-center gap-2">
            <Icon name="terminal" size={14} className="shrink-0 text-cx-fg-3" />
            <span className="shrink-0 text-[12.5px] text-cx-fg-3">登录命令</span>
            <code className="min-w-0 flex-1 truncate rounded-md bg-cx-code px-2 py-1 text-[12px] text-cx-fg">{instance.auth.login_command}</code>
            <CopyButton text={instance.auth.login_command} label="复制登录命令" />
          </div>
        ) : null}

        {connectionOptions.length > 1 ? (
          <div>
            <span className="mb-1.5 block text-[12.5px] font-medium text-cx-fg-2">接入方式</span>
            <Select ariaLabel="接入方式" value={instance?.adapter_id || adapter?.adapter_id || ""} options={connectionOptions} onChange={onSwitchAdapter} className="w-full" />
          </div>
        ) : null}

        {showAccessModes && accessModes.length ? (
          <div>
            <span className="mb-1.5 block text-[12.5px] font-medium text-cx-fg-2">权限模式</span>
            <div aria-label="权限模式" className="flex flex-wrap gap-1.5">
              {accessModes.map(mode => {
                const available = adapter?.capabilities.access_modes.includes(mode);
                const note = adapter?.access_mode_notes[mode];
                const chip = <span tabIndex={0} className={cn("inline-flex h-6 items-center rounded-md px-2 text-[12px] outline-none focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]", available ? "bg-cx-hover text-cx-fg-2" : "bg-cx-hover/50 text-cx-fg-4 line-through")}>{ACCESS_MODE_LABELS[mode] || mode}</span>;
                return note ? <Tooltip key={mode} content={<span className="block max-w-[260px] whitespace-pre-wrap">{note}{available ? "" : "（不可用）"}</span>}>{chip}</Tooltip> : <span key={mode}>{chip}</span>;
              })}
            </div>
          </div>
        ) : null}

        <div className="grid gap-4 sm:grid-cols-2">
          <TextField label="二进制路径" placeholder="留空时自动探测" value={binaryPath} onChange={event => setBinaryPath(event.target.value)} />
          <TextField label="服务端点" placeholder="留空时使用本地进程" value={endpoint} onChange={event => setEndpoint(event.target.value)} />
        </div>

        {transportEntries.length || envEditable ? (
          <div>
            <button type="button" aria-expanded={advanced} onClick={() => setAdvanced(value => !value)}
              className="inline-flex items-center gap-1.5 rounded-md text-[12.5px] text-cx-fg-3 outline-none hover:text-cx-fg focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]">
              <Icon name="chevronDown" size={13} className={cn("transition-transform", advanced && "rotate-180")} />高级：传输参数、启动参数、环境变量
            </button>
            <Collapse open={advanced}>
              <div className="mt-3 flex flex-col gap-4">
                {transportEntries.length ? (
                  <div className="grid gap-4 sm:grid-cols-2">
                    {transportEntries.map(([key, field]) => field.type === "boolean" ? (
                      <Checkbox key={key} checked={Boolean(transport[key] ?? field.default)} onCheckedChange={selected => setTransport(value => ({ ...value, [key]: selected }))} label={field.title || key} />
                    ) : (
                      <TextField key={key} label={field.title || key} type={field.type === "integer" || field.type === "number" ? "number" : "text"}
                        value={String(transport[key] ?? field.default ?? "")}
                        onChange={event => setTransport(value => ({ ...value, [key]: field.type === "integer" || field.type === "number" ? Number(event.target.value) : event.target.value }))} />
                    ))}
                  </div>
                ) : null}
                {envEditable ? (
                  <TextArea label="启动参数" description="每行一个，原样插在可执行文件之后，不经过 shell。Claude 只支持 --flag [值]。" placeholder={"例如：\n--config\nmodel_reasoning_summary=detailed"}
                    rows={3} autoResize spellCheck={false} className="font-mono text-[12px]" value={launchArgs} onChange={event => setLaunchArgs(event.target.value)} />
                ) : null}
                {envEditable ? <EnvRefsEditor rows={envRefs} onChange={setEnvRefs} /> : null}
              </div>
            </Collapse>
          </div>
        ) : null}

        <div>
          <button type="button" aria-expanded={capabilitiesOpen} onClick={() => setCapabilitiesOpen(value => !value)}
            className="inline-flex items-center gap-1.5 rounded-md text-[12.5px] text-cx-fg-3 outline-none hover:text-cx-fg focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]">
            <Icon name="chevronDown" size={13} className={cn("transition-transform", capabilitiesOpen && "rotate-180")} />探测到的能力{capabilityEntries.length ? `（${capabilityEntries.length}）` : ""}
          </button>
          <Collapse open={capabilitiesOpen}>
            <div className="mt-2 grid gap-x-6 gap-y-0.5 sm:grid-cols-2">
              {capabilityEntries.length ? capabilityEntries.map(([key, value]) => (
                <div key={key} className="flex items-center gap-2 py-1">
                  <Icon name={value ? "checkCircle" : "minus"} size={14} className={value ? "shrink-0 text-cx-success" : "shrink-0 text-cx-fg-4"} />
                  <span className="min-w-0 flex-1 text-[12.5px] text-cx-fg-2">{CAPABILITY_LABELS[key] || key}</span>
                  <span className="shrink-0 text-[11.5px] text-cx-fg-4">{typeof value === "boolean" ? value ? "支持" : "未启用" : String(value)}</span>
                </div>
              )) : <p className="py-2 text-[12.5px] text-cx-fg-3">探测运行环境后显示能力。</p>}
            </div>
          </Collapse>
        </div>
      </div>
    </Sheet>
  );
}
