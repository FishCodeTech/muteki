"use client";

import { useMemo } from "react";
import { SearchInput, StatusDot, Switch } from "@/components/chat/ui";
import { EngineLogo } from "@/components/EngineLogo";
import { cn } from "@/lib/cn";
import {
  installedVersion,
  statusTone,
  type Engine,
  type EngineSummary,
} from "./shared";

export function EngineList({
  engines,
  loading,
  query,
  onQueryChange,
  selectedEngine,
  selectedInstanceId,
  busyItem,
  onSelect,
  onToggle,
  useInstanceKeys = false,
  className,
}: {
  engines: EngineSummary[];
  loading: boolean;
  query: string;
  onQueryChange: (value: string) => void;
  selectedEngine: Engine | null;
  selectedInstanceId: string;
  busyItem: string;
  onSelect: (engine: Engine, instanceId: string) => void;
  onToggle: (engine: EngineSummary) => void;
  useInstanceKeys?: boolean;
  className?: string;
}) {
  const visible = useMemo(() => {
    const needle = query.trim().toLowerCase();
    if (!needle) return engines.map((item) => ({ item, instances: item.instances }));
    return engines.flatMap((item) => {
      const engineHit = `${item.label} ${item.engine} ${item.cliAdapterId}`.toLowerCase().includes(needle);
      const instances = engineHit
        ? item.instances
        : item.instances.filter((instance) => `${instance.label} ${instance.instance_id} ${instance.key}`.toLowerCase().includes(needle));
      return engineHit || instances.length ? [{ item, instances }] : [];
    });
  }, [engines, query]);

  return (
    <nav aria-label="Agent 引擎列表" className={cn("cx-settings-card flex min-w-0 flex-col", className)}>
      <div className="border-b border-cx-border-subtle p-2.5">
        <SearchInput value={query} onValueChange={onQueryChange} placeholder="搜索引擎或环境…" aria-label="搜索引擎或环境" />
      </div>
      <div className="flex flex-col gap-0.5 overflow-y-auto p-2 @3xl/agents:max-h-[calc(100dvh-260px)]">
        {loading ? (
          <p className="px-3 py-8 text-center text-[13px] text-cx-fg-3" role="status">正在读取引擎状态…</p>
        ) : visible.length ? visible.map(({ item, instances }) => {
          const engineSelected = selectedEngine === item.engine;
          const supported = item.supportStatus === "supported";
          return (
            <div key={item.engine}>
              <div
                className={cn(
                  "group flex items-center gap-1 rounded-xl pr-2 transition-colors",
                  engineSelected && !selectedInstanceId ? "bg-cx-active" : "hover:bg-cx-hover",
                  (!item.enabled || !supported) && "opacity-60",
                )}
                data-selected={engineSelected && !selectedInstanceId ? "true" : undefined}
              >
                <button
                  type="button"
                  onClick={() => onSelect(item.engine, "")}
                  aria-current={engineSelected ? "page" : undefined}
                  title={item.disabledReason || item.statusText}
                  className="flex min-w-0 flex-1 items-center gap-2.5 rounded-xl px-2 py-2 text-left outline-none focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]"
                >
                  <span className="grid size-8 shrink-0 place-items-center rounded-lg border border-cx-border bg-cx-bg">
                    <EngineLogo engine={item.engine} size={18} />
                  </span>
                  <span className="min-w-0 flex-1">
                    <span className="flex items-center gap-1.5">
                      <span className="truncate text-[13.5px] font-medium text-cx-fg">{item.label}</span>
                      {["warn", "bad"].includes(item.statusKind) ? (
                        <StatusDot tone={statusTone(item.statusKind)} label={item.statusText} />
                      ) : null}
                    </span>
                    <span className="mt-0.5 block truncate text-[11.5px] leading-4 text-cx-fg-3">
                      {supported ? installedVersion(item.version, item.primaryInstance?.health?.version_check) : "暂不支持"}
                    </span>
                  </span>
                </button>
                <Switch
                  size="sm"
                  checked={item.enabled}
                  disabled={!supported || busyItem === `toggle:${item.engine}`}
                  ariaLabel={item.disabledReason ? `${item.label} 暂不支持：${item.disabledReason}` : `${item.label} 启用状态`}
                  onCheckedChange={() => onToggle(item)}
                />
              </div>
              {instances.length > 1 ? (
                <div className="mb-1 ml-[42px] flex flex-col gap-0.5 border-l border-cx-border-subtle pl-2">
                  {instances.map((instance) => {
                    const instanceSelected = engineSelected && (selectedInstanceId === instance.key || selectedInstanceId === instance.instance_id);
                    const unhealthy = instance.health ? !instance.health.healthy : Boolean(instance.configured || instance.discovered);
                    return (
                      <button
                        key={instance.key}
                        type="button"
                        onClick={() => onSelect(item.engine, useInstanceKeys ? instance.key : instance.instance_id)}
                        aria-current={instanceSelected ? "page" : undefined}
                        className={cn(
                          "flex min-w-0 items-center gap-2 rounded-lg px-2 py-1.5 text-left outline-none transition-colors focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]",
                          instanceSelected ? "bg-cx-active" : "hover:bg-cx-hover",
                        )}
                      >
                        <span className="min-w-0 flex-1 truncate text-[12.5px] text-cx-fg-2">
                          {instance.label || instance.instance_id}
                        </span>
                        {!instance.enabled ? <span className="text-[11px] text-cx-fg-4">停用</span> : null}
                        {instance.enabled && unhealthy ? (
                          <StatusDot tone={instance.health ? "danger" : "warning"} label={instance.health?.detail || "等待探测"} />
                        ) : null}
                      </button>
                    );
                  })}
                </div>
              ) : null}
            </div>
          );
        }) : (
          <p className="px-3 py-8 text-center text-[13px] text-cx-fg-3">没有匹配的引擎。</p>
        )}
      </div>
    </nav>
  );
}
