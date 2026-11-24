// ai-design-check-ignore-file use-shared-components -- picker triggers and the stepper are compact composite controls that share one field frame
// ai-design-check-ignore-file use-shared-looks -- the stepper frame is a control, not a card
"use client";

import { useMemo, useState, type ReactElement, type ReactNode } from "react";
import { Icon } from "@muteki/ui";
import { Select, type ListOption } from "@muteki/ui";
import { cn } from "@/lib/cn";
import { priceKey, priceLabel, useModelPrices } from "@/lib/modelPricing";
import { readyCatalog, useProviderDescriptors } from "@/lib/providerDescriptors";
import type { GlobalCredential, WorkerModelOptions, WorkerSettings } from "@/lib/useRun";
import {
  EFFORT_LABELS, credentialKindLabel, credentialsForEngine, effortDefinition, engineEffortLevels, findGlobalCredential, globalCredentialKey,
  globalCredentialUsable, modelDirectory, normalizeEffortForModel,
  type Credential, type Engine, type ModelOptionList, type Seat,
} from "./model";

const DEFAULT_MODEL_VALUE = "__runtime_default__";

type TriggerOptions = {
  label: ReactNode;
  placeholder: string;
  hasValue: boolean;
  invalid?: boolean;
  disabled?: boolean;
  ariaLabel: string;
  leading?: ReactNode;
  className?: string;
};

export function pickerTrigger({ label, placeholder, hasValue, invalid, disabled, ariaLabel, leading, className }: TriggerOptions): ReactElement {
  return (
    <button
      type="button"
      disabled={disabled}
      aria-label={ariaLabel}
      className={cn(
        "cx-press inline-flex h-8 w-full min-w-0 items-center gap-1.5 rounded-lg border bg-cx-elevated px-2.5 text-left text-cx-xs text-cx-fg",
        "hover:border-cx-border-strong disabled:cursor-not-allowed disabled:opacity-50 disabled:hover:border-cx-border",
        invalid ? "border-cx-warning" : "border-cx-border",
        className,
      )}
    >
      {leading}
      <span className={cn("min-w-0 flex-1 truncate", !hasValue && "text-cx-fg-4")}>{hasValue ? label : placeholder}</span>
      <Icon name="chevronsUpDown" size={12} className="shrink-0 text-cx-fg-4" />
    </button>
  );
}

export function FieldLabel({ children, className }: { children: ReactNode; className?: string }) {
  return <div className={cn("mb-1 text-cx-xs font-medium text-cx-fg-3", className)}>{children}</div>;
}

export function CredentialSelect({
  engine, accountKey, model, availableCredentials, backend, onBind, onModelChange, onOpenCredentials, className,
}: {
  engine: Engine;
  accountKey: string;
  model: string;
  availableCredentials: GlobalCredential[];
  backend: WorkerSettings["worker_backend"];
  onBind: (key: string, credential?: GlobalCredential) => void;
  onModelChange: (model: string) => void;
  onOpenCredentials: () => void;
  className?: string;
}) {
  const matching = useMemo(() => credentialsForEngine(availableCredentials, engine, backend), [availableCredentials, engine, backend]);
  const selected = findGlobalCredential(availableCredentials, engine, accountKey);
  const options: ListOption[] = matching.map((item) => ({
    value: item.id,
    label: item.label || item.id,
    description: `${credentialKindLabel(item)}${globalCredentialUsable(item) ? "" : " · 不可用"}`,
    disabled: !globalCredentialUsable(item),
  }));
  const choose = (credentialId: string) => {
    const next = availableCredentials.find((item) => item.id === credentialId);
    if (!globalCredentialUsable(next)) {
      onBind("");
      onModelChange("");
      return;
    }
    const nextModels = new Set(["", next.default_model || "", ...(next.models || [])]);
    if (!nextModels.has(model)) onModelChange(next.default_model || "");
    if (next.source === "system") {
      onBind("__system__");
      return;
    }
    onBind(next.account_id || next.id.replace(/^account:/, ""), next);
  };
  const missing = !selected || !globalCredentialUsable(selected);
  return (
    <Select
      value={selected?.id ?? null}
      onChange={choose}
      options={options}
      searchable={matching.length > 5}
      searchPlaceholder="搜索账号或接入点…"
      ariaLabel="Worker 凭据"
      popoverClassName="w-[300px]"
      emptyText="该引擎还没有凭据"
      trigger={pickerTrigger({
        label: selected ? selected.label || globalCredentialKey(selected) : "",
        placeholder: matching.length ? "选择凭据" : "无可用凭据",
        hasValue: Boolean(selected),
        invalid: missing,
        ariaLabel: "Worker 凭据",
        leading: <Icon name="lock" size={12} className="shrink-0 text-cx-fg-4" />,
        className,
      })}
      footer={(close) => (
        <button
          type="button"
          onClick={() => { close(); onOpenCredentials(); }}
          className="flex h-8 w-full items-center gap-2 rounded-lg px-2 text-left text-cx-xs text-cx-fg-3 hover:bg-cx-hover hover:text-cx-fg"
        >
          <Icon name="plus" size={13} />管理凭据
        </button>
      )}
    />
  );
}

export function ModelSelect({
  engine, accountKey, model, availableCredentials, onModelChange, className,
}: {
  engine: Engine;
  accountKey: string;
  model: string;
  availableCredentials: GlobalCredential[];
  onModelChange: (model: string) => void;
  className?: string;
}) {
  const selected = findGlobalCredential(availableCredentials, engine, accountKey);
  const [open, setOpen] = useState(false);
  const entries = useMemo(() => modelDirectory(selected, model), [selected, model]);
  const identities = useMemo(() => (open
    ? entries.filter((entry) => entry.id).map((entry) => ({ engine, model: entry.id, credential_id: selected?.id, provider_id: selected?.provider }))
    : []), [engine, entries, open, selected?.id, selected?.provider]);
  const pricing = useModelPrices(identities, open);
  const priceOf = (id: string) => priceLabel(pricing.values.get(priceKey({ engine, model: id, credential_id: selected?.id, provider_id: selected?.provider })), pricing.error, pricing.loading);
  const options: ListOption[] = entries.map((entry) => ({
    value: entry.id || DEFAULT_MODEL_VALUE,
    label: entry.id || "Runtime 默认模型",
    textValue: entry.id || "default 默认",
    description: entry.id ? `${entry.source} · ${open ? priceOf(entry.id) : "…"}` : "由 Worker 程序决定",
  }));
  const usable = globalCredentialUsable(selected);
  return (
    <Select
      value={model || DEFAULT_MODEL_VALUE}
      onChange={(value) => onModelChange(value === DEFAULT_MODEL_VALUE ? "" : value)}
      options={options}
      searchable
      searchPlaceholder="搜索模型…"
      emptyText="没有匹配的模型"
      open={open}
      onOpenChange={setOpen}
      popoverClassName="w-[340px]"
      ariaLabel="模型"
      disabled={!usable}
      trigger={pickerTrigger({
        label: model || "Runtime 默认模型",
        placeholder: "选择模型",
        hasValue: usable,
        disabled: !usable,
        ariaLabel: "模型",
        leading: <Icon name="sparkles" size={12} className="shrink-0 text-cx-fg-4" />,
        className,
      })}
    />
  );
}

export function EffortSelect({
  engine, model, options, value, onChange, inherit = false, className,
}: {
  engine: Engine;
  model: string;
  options: ModelOptionList;
  value: string;
  onChange: (value: string) => void;
  inherit?: boolean;
  className?: string;
}) {
  const catalog = readyCatalog(useProviderDescriptors());
  const levels = engineEffortLevels(catalog, engine);
  const definition = effortDefinition(levels, model, options);
  const selected = inherit
    ? (value === "inherit" || definition.levels.includes(value) ? value : "inherit")
    : normalizeEffortForModel(value, levels, model, options);
  const defaultLabel = definition.defaultLevel
    ? `默认 · ${EFFORT_LABELS[definition.defaultLevel] || definition.defaultLevel}`
    : "模型默认";
  const choices: ListOption[] = [
    inherit
      ? { value: "inherit", label: "继承 Worker", description: "沿用该 Worker 的推理强度" }
      : { value: "default", label: defaultLabel },
    ...definition.levels.map((level) => ({ value: level, label: EFFORT_LABELS[level] || level })),
  ];
  const current = choices.find((choice) => choice.value === selected);
  const disabled = !definition.supported;
  return (
    <Select
      value={selected}
      onChange={onChange}
      options={choices}
      ariaLabel={inherit ? "覆盖推理强度" : "推理强度"}
      disabled={disabled}
      popoverClassName="w-[200px]"
      trigger={pickerTrigger({
        label: current?.label,
        placeholder: "默认强度",
        hasValue: Boolean(current) && !disabled,
        disabled,
        ariaLabel: inherit ? "覆盖推理强度" : "推理强度",
        leading: <Icon name="brain" size={12} className="shrink-0 text-cx-fg-4" />,
        className,
      })}
    />
  );
}

export function ConcurrencyStepper({
  value, onChange, min = 1, max = 32, ariaLabel = "并发上限", zeroLabel, className,
}: {
  value: number;
  onChange: (next: number) => void;
  min?: number;
  max?: number;
  ariaLabel?: string;
  zeroLabel?: string;
  className?: string;
}) {
  const clamp = (next: number) => Math.max(min, Math.min(max, next));
  const showZero = zeroLabel && value === 0;
  return (
    <div className={cn("inline-flex h-8 w-full items-center rounded-lg border border-cx-border bg-cx-elevated text-cx-sm text-cx-fg hover:border-cx-border-strong focus-within:border-cx-border-strong", className)} role="group" aria-label={ariaLabel}>
      <button type="button" aria-label="减少" disabled={value <= min} onClick={() => onChange(clamp(value - 1))} className="grid h-full w-7 shrink-0 place-items-center rounded-l-lg text-cx-fg-3 hover:bg-cx-hover hover:text-cx-fg disabled:opacity-35 disabled:hover:bg-transparent">
        <Icon name="minus" size={12} />
      </button>
      {showZero ? (
        <span className="min-w-0 flex-1 text-center text-cx-fg-3">{zeroLabel}</span>
      ) : (
        <input
          inputMode="numeric"
          aria-label={ariaLabel}
          value={value}
          onChange={(event) => {
            const parsed = Number(event.target.value.replace(/[^0-9]/g, ""));
            if (Number.isFinite(parsed)) onChange(clamp(parsed));
          }}
          className="min-w-0 flex-1 bg-transparent text-center tabular-nums outline-none"
        />
      )}
      <button type="button" aria-label="增加" disabled={value >= max} onClick={() => onChange(clamp(value + 1))} className="grid h-full w-7 shrink-0 place-items-center rounded-r-lg text-cx-fg-3 hover:bg-cx-hover hover:text-cx-fg disabled:opacity-35 disabled:hover:bg-transparent">
        <Icon name="plus" size={12} />
      </button>
    </div>
  );
}

export type SeatFieldsProps = {
  seat: Seat;
  credentials: Credential[];
  availableCredentials: GlobalCredential[];
  models: WorkerModelOptions;
  backend: WorkerSettings["worker_backend"];
  accountKey: string;
  onUpdate: (patch: Partial<Seat>) => void;
  onBind: (key: string, credential?: GlobalCredential) => void;
  onOpenCredentials: () => void;
  labelled?: boolean;
};

export function SeatFields({ seat, availableCredentials, models, backend, accountKey, onUpdate, onBind, onOpenCredentials, labelled = false }: SeatFieldsProps) {
  const catalog = readyCatalog(useProviderDescriptors());
  const engine = seat.engine;
  const modelOptions = models.models[engine] || [];
  const changeModel = (model: string) => {
    if (model === (seat.model || "")) return;
    onUpdate({ model, reasoning_effort: normalizeEffortForModel(seat.reasoning_effort, engineEffortLevels(catalog, engine), model, modelOptions) });
  };
  const wrap = (label: string, control: ReactNode) => (labelled
    ? <div className="min-w-0"><FieldLabel>{label}</FieldLabel>{control}</div>
    : <div className="min-w-0">{control}</div>);
  return (
    <>
      {wrap("凭据", <CredentialSelect engine={engine} accountKey={accountKey} model={seat.model || ""} availableCredentials={availableCredentials} backend={backend} onBind={onBind} onModelChange={changeModel} onOpenCredentials={onOpenCredentials} />)}
      {wrap("模型", <ModelSelect engine={engine} accountKey={accountKey} model={seat.model || ""} availableCredentials={availableCredentials} onModelChange={changeModel} />)}
      {wrap("推理强度", <EffortSelect engine={engine} model={seat.model || ""} options={modelOptions} value={seat.reasoning_effort || "default"} onChange={(reasoning_effort) => onUpdate({ reasoning_effort })} />)}
    </>
  );
}
