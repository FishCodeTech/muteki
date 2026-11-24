// ai-design-check-ignore-file use-shared-components -- the Select trigger and the two-line title button are bespoke row controls
"use client";

import { Icon, type IconName } from "@muteki/ui";
import { Badge, IconButton, Select, Switch, type ListOption } from "@muteki/ui";
import { cn } from "@/lib/cn";
import { engineDisplayName, readyCatalog, useProviderDescriptors } from "@/lib/providerDescriptors";
import type { WorkerModelOptions, WorkerModelTestResult } from "@/lib/useRun";
import { CheckStatus } from "./CheckStatus";
import { ConcurrencyStepper, EffortSelect } from "./SeatFields";
import { canServeChannel, isOrdinarySeat, type Seat } from "./model";

const NEW_DEDICATED = "__new_dedicated__";

export type AuxiliaryKind = "review" | "verifier";

export function AuxiliaryRow({
  kind, title, description, icon, enabled, engineId, seats, models, effort, concurrency,
  testing, testResult, versionBlockedIds, onEnabled, onSelectSeat, onCreateDedicated, onEffort, onConcurrency, onTest, onOpenSettings,
}: {
  kind: AuxiliaryKind;
  title: string;
  description: string;
  icon: IconName;
  enabled: boolean;
  engineId: string;
  seats: Seat[];
  models: WorkerModelOptions;
  effort: string;
  concurrency?: number;
  testing: boolean;
  testResult?: WorkerModelTestResult;
  versionBlockedIds?: ReadonlySet<string>;
  onEnabled: (enabled: boolean) => void;
  onSelectSeat: (id: string) => void;
  onCreateDedicated: () => void;
  onEffort: (value: string) => void;
  onConcurrency?: (value: number) => void;
  onTest: () => void;
  onOpenSettings: () => void;
}) {
  const catalog = readyCatalog(useProviderDescriptors());
  const candidates = seats.filter((seat) => canServeChannel(seat, kind));
  const assigned = candidates.find((seat) => seat.id === engineId);
  const assignedBlocked = Boolean(assigned && versionBlockedIds?.has(assigned.id));
  const describe = (seat: Seat) => {
    const blocked = versionBlockedIds?.has(seat.id);
    return `${engineDisplayName(catalog, seat.engine)} · ${seat.model || "默认模型"}${blocked ? " · 版本过低（禁止出战）" : seat.enabled ? "" : " · 已停用"}`;
  };
  const options: ListOption[] = [
    ...candidates.map((seat) => ({
      value: seat.id,
      label: seat.label,
      description: describe(seat),
      disabled: !seat.enabled || Boolean(versionBlockedIds?.has(seat.id)),
      section: isOrdinarySeat(seat) ? "沿用出战 Worker" : "专用配置",
    })),
    { value: NEW_DEDICATED, label: "新建专用配置…", description: "单独的凭据与模型，不占用出战阵容", icon: "plus", section: "其他" },
  ];
  const effective = enabled && Boolean(assigned?.enabled) && !assignedBlocked;
  const unassigned = !assigned;
  return (
    <div className="flex flex-wrap items-center gap-x-3 gap-y-2 border-b border-cx-border-subtle px-4 py-3 last:border-b-0 hover:bg-cx-hover/60">
      <div className="order-1 flex min-w-0 flex-1 items-center gap-2 @[960px]:w-[184px] @[960px]:flex-none">
        <span className="grid size-8 shrink-0 place-items-center rounded-lg border border-dashed border-cx-border-strong text-cx-fg-3">
          <Icon name={icon} size={15} />
        </span>
        <button type="button" onClick={onOpenSettings} className="min-w-0 rounded-md text-left outline-none focus-visible:ring-2 focus-visible:ring-[var(--cx-focus)]">
          <div className="flex items-center gap-1.5">
            <span className={cn("truncate text-cx-sm font-medium", enabled ? "text-cx-fg" : "text-cx-fg-3")}>{title}</span>
            {assignedBlocked ? <Badge tone="danger">版本过低</Badge> : null}
          </div>
          <div className="truncate text-cx-xs text-cx-fg-3">{description}</div>
        </button>
      </div>

      <div className={cn(
        "order-3 grid w-full grid-cols-2 gap-2 transition-opacity @[620px]:grid-cols-[minmax(0,1fr)_minmax(0,1.3fr)_108px_96px] @[960px]:order-2 @[960px]:w-auto @[960px]:flex-1",
        !enabled && "opacity-60",
      )}>
        <div className="col-span-2 min-w-0">
          <Select
            value={assigned?.id ?? null}
            onChange={(value) => (value === NEW_DEDICATED ? onCreateDedicated() : onSelectSeat(value))}
            options={options}
            ariaLabel={`${title} 使用的 Worker`}
            popoverClassName="w-[320px]"
            trigger={(
              <button
                type="button"
                aria-label={`${title} 使用的 Worker`}
                className={cn(
                  "cx-press inline-flex h-8 w-full min-w-0 items-center gap-1.5 rounded-lg border bg-cx-elevated px-2.5 text-left text-cx-xs text-cx-fg hover:border-cx-border-strong",
                  unassigned && enabled ? "border-cx-warning" : "border-cx-border",
                )}
              >
                <Icon name="bot" size={12} className="shrink-0 text-cx-fg-4" />
                <span className={cn("min-w-0 flex-1 truncate", unassigned && "text-cx-fg-4")}>{assigned ? assigned.label : "选择 Worker"}</span>
                <Icon name="chevronsUpDown" size={12} className="shrink-0 text-cx-fg-4" />
              </button>
            )}
          />
        </div>
        <div className="min-w-0">
          <EffortSelect inherit engine={assigned?.engine || ""} model={assigned?.model || ""} options={models.models[assigned?.engine || ""] || []} value={effort || "inherit"} onChange={onEffort} />
        </div>
        <div className="min-w-0">
          {kind === "verifier" && onConcurrency ? (
            <ConcurrencyStepper min={0} value={concurrency ?? 0} zeroLabel="自动" ariaLabel="Verifier 并发子上限" onChange={onConcurrency} />
          ) : (
            <div className="inline-flex h-8 w-full items-center justify-center rounded-lg border border-cx-border-subtle text-cx-xs text-cx-fg-3" title="Review 同一时间只运行 1 个">并发 1</div>
          )}
        </div>
      </div>

      <div className="order-2 flex shrink-0 items-center gap-1 @[960px]:order-3">
        {assigned
          ? <CheckStatus enabled={effective} testing={testing} result={testResult} onCheck={assignedBlocked ? undefined : onTest} className="w-[84px] justify-start" />
          : <span className="inline-flex h-7 w-[84px] items-center px-1.5 text-cx-xs text-cx-fg-4">{enabled ? "待配置" : "已停用"}</span>}
        <Switch size="sm" checked={enabled} onCheckedChange={onEnabled} ariaLabel={`${title} 启用状态`} className="mx-1" />
        <IconButton icon="sliders" label={`${title} 高级设置`} onClick={onOpenSettings} />
      </div>
    </div>
  );
}
