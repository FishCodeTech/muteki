"use client";

import { useMemo, useState } from "react";
import { Badge, Button, Dialog, Select, TextField, type ListOption } from "@muteki/ui";
import { EngineLogo } from "@/components/EngineLogo";
import { cn } from "@/lib/cn";
import { descriptorForEngine, type ProviderDescriptorCatalog } from "@/lib/providerDescriptors";
import type { Engine, EngineSummary, RuntimeInstance } from "./shared";

const INSTANCE_ID = /^[a-zA-Z][a-zA-Z0-9_-]*$/;
const MAX_INSTANCE_ID = 64;

export type AddAgentInput = { engine: Engine; adapterId: string; instanceId: string; label: string };

type Candidate = {
  item: EngineSummary;
  installed: "yes" | "no" | "unknown";
  version: string;
};

export function AddAgentDialog({
  open, onOpenChange, engines, instances, descriptors, initialEngine, onAdd,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  engines: EngineSummary[];
  instances: RuntimeInstance[];
  descriptors: ProviderDescriptorCatalog | null;
  initialEngine: Engine | null;
  onAdd: (input: AddAgentInput) => Promise<void>;
}) {
  const [engine, setEngine] = useState<Engine | null>(initialEngine);
  const [step, setStep] = useState<"pick" | "identity">(initialEngine ? "identity" : "pick");

  const candidates = useMemo<Candidate[]>(() => engines.map((item) => {
    const identity = descriptorForEngine(descriptors, item.engine)?.identity;
    const row = instances.find((instance) => instance.engine === item.engine
      && instance.adapter_id === identity?.default_adapter_id && instance.instance_id === "default");
    const health = row?.health;
    const installed: Candidate["installed"] = health ? (health.healthy ? "yes" : "no") : "unknown";
    return {
      item,
      installed,
      version: health?.runtime_version || "",
    };
  }).sort((left, right) => {
    const rank = (entry: Candidate) => entry.item.supportStatus !== "supported" ? 3
      : entry.item.added ? 2 : entry.installed === "yes" ? 0 : 1;
    return rank(left) - rank(right);
  }), [descriptors, engines, instances]);

  const chosen = candidates.find((entry) => entry.item.engine === engine) || null;

  return (
    <Dialog
      open={open}
      onOpenChange={onOpenChange}
      size="lg"
      icon="plus"
      tone="accent"
      title={step === "pick" ? "添加 Agent" : `添加 ${chosen?.item.label || "Agent"}`}
      description={step === "pick"
        ? "选择要在对话中使用的 Agent。添加后可在设置里配置、停用或移除。"
        : "为这个实例起个名字。同一引擎可以添加多个实例，各自保存运行配置。"}
      dismissable={false}
      testId="add-agent-dialog"
      footer={step === "pick" ? (
        <>
          <Button variant="ghost" onClick={() => onOpenChange(false)}>取消</Button>
          <Button variant="primary" disabled={!chosen} onClick={() => setStep("identity")}>下一步</Button>
        </>
      ) : null}
    >
      {step === "pick" ? (
        <div role="radiogroup" aria-label="选择 Agent 引擎" className="grid grid-cols-1 gap-2 py-1 sm:grid-cols-2">
          {candidates.map(({ item, installed, version }) => {
            const supported = item.supportStatus === "supported";
            const selected = engine === item.engine;
            const detail = !supported ? "暂不支持"
              : installed === "yes" ? `已检测到安装${version ? ` · ${version}` : ""}`
                : installed === "no" ? "未检测到可用安装" : "尚未探测";
            return (
              <button
                key={item.engine}
                type="button"
                role="radio"
                aria-checked={selected}
                disabled={!supported}
                onClick={() => setEngine(item.engine)}
                onDoubleClick={() => { if (supported) { setEngine(item.engine); setStep("identity"); } }}
                className={cn(
                  "flex min-w-0 items-center gap-3 rounded-xl border px-3 py-2.5 text-left outline-none transition-colors focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]",
                  selected ? "border-cx-accent bg-cx-accent-soft" : "border-cx-border hover:bg-cx-hover",
                  !supported && "cursor-not-allowed opacity-50",
                )}
              >
                <span className="grid size-9 shrink-0 place-items-center rounded-lg border border-cx-border bg-cx-bg">
                  <EngineLogo engine={item.engine} size={20} />
                </span>
                <span className="min-w-0 flex-1">
                  <span className="flex items-center gap-1.5">
                    <span className="truncate text-cx-sm font-medium text-cx-fg">{item.label}</span>
                    {item.added ? <Badge tone="neutral" className="h-[18px] px-1.5 text-cx-2xs">已添加</Badge> : null}
                  </span>
                  <span className={cn("mt-0.5 block truncate text-cx-xs leading-4", installed === "yes" && supported ? "text-cx-success" : "text-cx-fg-3")}>{detail}</span>
                </span>
              </button>
            );
          })}
        </div>
      ) : chosen ? (
        <IdentityStep
          key={chosen.item.engine}
          candidate={chosen}
          instances={instances}
          descriptors={descriptors}
          onBack={initialEngine ? undefined : () => setStep("pick")}
          onCancel={() => onOpenChange(false)}
          onAdd={onAdd}
        />
      ) : null}
    </Dialog>
  );
}

function IdentityStep({ candidate, instances, descriptors, onBack, onCancel, onAdd }: {
  candidate: Candidate;
  instances: RuntimeInstance[];
  descriptors: ProviderDescriptorCatalog | null;
  onBack?: () => void;
  onCancel: () => void;
  onAdd: (input: AddAgentInput) => Promise<void>;
}) {
  const { item } = candidate;
  const descriptor = descriptorForEngine(descriptors, item.engine);
  const defaultAdapter = descriptor?.identity.default_adapter_id || "";
  const engineRows = useMemo(() => instances.filter((row) => row.engine === item.engine), [instances, item.engine]);
  const adapterOptions = useMemo<ListOption[]>(() => (descriptor?.adapters || [])
    .filter((adapter) => (adapter.role === "default" || adapter.role === "variant")
      && engineRows.some((row) => row.adapter_id === adapter.adapter_id))
    .map((adapter) => ({
      value: adapter.adapter_id,
      label: `${adapter.transport_kind.toUpperCase()}${adapter.adapter_id === defaultAdapter ? "（默认）" : ""}`,
      description: adapter.notes || adapter.adapter_id,
    })), [defaultAdapter, descriptor?.adapters, engineRows]);
  const [adapterId, setAdapterId] = useState(defaultAdapter || String(adapterOptions[0]?.value || ""));

  const taken = (adapter: string) => new Set(engineRows
    .filter((row) => row.adapter_id === adapter && (row.configured || item.defaultListed || adapter !== defaultAdapter))
    .map((row) => row.instance_id));
  const suggestedId = (adapter: string) => {
    const used = taken(adapter);
    if (!used.has("default")) return "default";
    for (let index = 2; ; index += 1) if (!used.has(`${item.engine}-${index}`)) return `${item.engine}-${index}`;
  };
  const initialId = suggestedId(adapterId);
  const [instanceId, setInstanceId] = useState(initialId);
  const [label, setLabel] = useState(initialId === "default" ? item.label : `${item.label} ${initialId.split("-").pop()}`);
  const [touchedId, setTouchedId] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");

  const trimmedId = instanceId.trim();
  const idError = !trimmedId ? "请填写实例 ID。"
    : trimmedId.length > MAX_INSTANCE_ID ? `实例 ID 最长 ${MAX_INSTANCE_ID} 个字符。`
      : !INSTANCE_ID.test(trimmedId) ? "以字母开头，只能包含字母、数字、- 和 _。"
        : taken(adapterId).has(trimmedId) ? "该引擎已有同名实例，请换一个 ID。" : "";

  const submit = async () => {
    if (idError || busy) { setTouchedId(true); return; }
    setBusy(true); setError("");
    try {
      await onAdd({ engine: item.engine, adapterId, instanceId: trimmedId, label: label.trim() || item.label });
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : String(failure));
      setBusy(false);
    }
  };

  return (
    <form
      className="flex flex-col gap-4 py-1"
      onSubmit={(event) => { event.preventDefault(); void submit(); }}
    >
      <div className="flex items-center gap-3 rounded-xl border border-cx-border-subtle bg-cx-bg-subtle px-3 py-2.5">
        <span className="grid size-9 shrink-0 place-items-center rounded-lg border border-cx-border bg-cx-bg"><EngineLogo engine={item.engine} size={20} /></span>
        <span className="min-w-0 flex-1">
          <span className="block truncate text-cx-sm font-medium text-cx-fg">{item.label}</span>
          <span className="block truncate text-cx-xs text-cx-fg-3">
            {candidate.installed === "yes" ? `已检测到安装${candidate.version ? ` · ${candidate.version}` : ""}`
              : candidate.installed === "no" ? "未检测到可用安装，添加后可在配置中指定可执行文件" : "添加后会自动探测本机安装"}
          </span>
        </span>
      </div>
      <TextField
        label="显示名称"
        value={label}
        maxLength={80}
        data-autofocus=""
        onChange={(event) => setLabel(event.target.value)}
        placeholder={item.label}
      />
      <TextField
        label="实例 ID"
        value={instanceId}
        onChange={(event) => { setInstanceId(event.target.value); setTouchedId(true); }}
        onBlur={() => setTouchedId(true)}
        error={touchedId ? idError : ""}
        description="用于区分同一引擎的多个实例，添加后不可修改。"
        spellCheck={false}
        autoComplete="off"
      />
      {adapterOptions.length > 1 ? (
        <div className="flex flex-col gap-1.5">
          <span className="text-cx-xs font-medium text-cx-fg-2">连接方式</span>
          <Select
            value={adapterId}
            onChange={(value) => {
              const next = String(value);
              setAdapterId(next);
              if (!touchedId) setInstanceId(suggestedId(next));
            }}
            options={adapterOptions}
            ariaLabel="连接方式"
          />
        </div>
      ) : null}
      {error ? <p role="alert" className="text-cx-xs text-cx-danger">{error}</p> : null}
      <div className="-mx-5 -mb-3 flex flex-wrap items-center justify-end gap-2 border-t border-cx-border-subtle bg-cx-bg-subtle px-5 py-3">
        {onBack ? <Button type="button" variant="ghost" className="mr-auto" icon="arrowLeft" disabled={busy} onClick={onBack}>上一步</Button> : null}
        <Button type="button" variant="ghost" disabled={busy} onClick={onCancel}>取消</Button>
        <Button type="submit" variant="primary" icon="plus" loading={busy} disabled={Boolean(idError)}>添加</Button>
      </div>
    </form>
  );
}
