"use client";

import { useMemo } from "react";
import { Button, IconButton, SearchInput, StatusDot, Switch } from "@muteki/ui";
import { EngineLogo } from "@/components/EngineLogo";
import { Icon } from "@muteki/ui";
import { cn } from "@/lib/cn";
import {
  installedVersion,
  statusTone,
  type Engine,
  type EngineSummary,
  type RuntimeInstance,
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
  onToggleInstance,
  onAdd,
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
  onToggleInstance?: (engine: EngineSummary, instance: RuntimeInstance) => void;
  onAdd?: () => void;
  useInstanceKeys?: boolean;
  className?: string;
}) {
  const visible = useMemo(() => {
    const needle = query.trim().toLowerCase();
    if (!needle) return engines.map((item) => ({ item, instances: item.instances, instanceHit: false }));
    return engines.flatMap((item) => {
      const engineHit = `${item.label} ${item.engine} ${item.cliAdapterId}`.toLowerCase().includes(needle);
      const instances = engineHit
        ? item.instances
        : item.instances.filter((instance) => `${instance.label} ${instance.instance_id} ${instance.key}`.toLowerCase().includes(needle));
      return engineHit || instances.length ? [{ item, instances, instanceHit: !engineHit }] : [];
    });
  }, [engines, query]);

  return (
    <nav aria-label="Agent 引擎列表" className={cn("cx-settings-card flex min-w-0 flex-col", className)}>
      <div className="flex items-center gap-1.5 border-b border-cx-border-subtle p-2.5">
        <SearchInput value={query} onValueChange={onQueryChange} placeholder="搜索引擎或环境…" aria-label="搜索引擎或环境" className="min-w-0 flex-1" />
        {onAdd ? <IconButton icon="plus" label="添加 Agent" onClick={onAdd} /> : null}
      </div>
      <div className="flex flex-col gap-0.5 overflow-y-auto p-2 @3xl/agents:max-h-[calc(100dvh-260px)]">
        {loading ? (
          <p className="px-3 py-8 text-center text-cx-sm text-cx-fg-3" role="status">正在读取引擎状态…</p>
        ) : visible.length ? visible.map(({ item, instances, instanceHit }) => {
          const engineSelected = selectedEngine === item.engine;
          const supported = item.supportStatus === "supported";
          const showInstances = instanceHit ? instances.length > 0 : engineSelected && instances.length > 1;
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
                      <span className="truncate text-cx-sm font-medium text-cx-fg">{item.label}</span>
                      {["warn", "bad"].includes(item.statusKind) ? (
                        <StatusDot tone={statusTone(item.statusKind)} label={item.statusText} />
                      ) : null}
                    </span>
                    <span className="mt-0.5 block truncate text-cx-xs leading-4 text-cx-fg-3">
                      {supported ? installedVersion(item.version, item.primaryInstance?.health?.version_check) : "暂不支持"}
                      {supported && !showInstances && item.instances.length > 1 ? ` · ${item.instances.length} 个环境` : null}
                    </span>
                  </span>
                </button>
                <Switch
                  size="sm"
                  checked={item.enabled}
                  disabled={!supported || busyItem === `toggle:${item.primaryInstance?.key || item.engine}`}
                  ariaLabel={item.disabledReason ? `${item.label} 暂不支持：${item.disabledReason}` : `${item.label} 启用状态`}
                  onCheckedChange={() => onToggle(item)}
                />
              </div>
              {showInstances ? (
                <div className="mb-1 ml-[42px] flex flex-col gap-0.5 border-l border-cx-border-subtle pl-2">
                  {instances.map((instance) => {
                    const instanceSelected = engineSelected && (selectedInstanceId === instance.key || selectedInstanceId === instance.instance_id);
                    const unhealthy = instance.health ? !instance.health.healthy : Boolean(instance.configured || instance.discovered);
                    return (
                      <div
                        key={instance.key}
                        className={cn("flex min-w-0 items-center gap-1 rounded-lg pr-1.5 transition-colors", instanceSelected ? "bg-cx-active" : "hover:bg-cx-hover")}
                      >
                        <button
                          type="button"
                          onClick={() => onSelect(item.engine, useInstanceKeys ? instance.key : instance.instance_id)}
                          aria-current={instanceSelected ? "page" : undefined}
                          className="flex min-w-0 flex-1 items-center gap-2 rounded-lg px-2 py-1.5 text-left outline-none focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]"
                        >
                          <span className="min-w-0 flex-1 truncate text-cx-xs text-cx-fg-2">
                            {instance.label || instance.instance_id}
                          </span>
                          {!instance.enabled && !onToggleInstance ? <span className="text-cx-2xs text-cx-fg-4">停用</span> : null}
                          {instance.enabled && unhealthy ? (
                            <StatusDot tone={instance.health ? "danger" : "warning"} label={instance.health?.detail || "等待探测"} />
                          ) : null}
                        </button>
                        {onToggleInstance ? (
                          <Switch
                            size="sm"
                            checked={instance.enabled}
                            disabled={busyItem === `toggle:${instance.key}`}
                            ariaLabel={`${instance.label || instance.instance_id} 启用状态`}
                            onCheckedChange={() => onToggleInstance(item, instance)}
                          />
                        ) : null}
                      </div>
                    );
                  })}
                </div>
              ) : null}
            </div>
          );
        }) : query.trim() || !onAdd ? (
          <p className="px-3 py-8 text-center text-cx-sm text-cx-fg-3">没有匹配的引擎。</p>
        ) : (
          <div className="flex flex-col items-center gap-3 px-3 py-8 text-center">
            <p className="text-cx-sm text-cx-fg-3">还没有添加 Agent。</p>
            <Button size="sm" variant="primary" icon="plus" onClick={onAdd}>添加 Agent</Button>
          </div>
        )}
        {onAdd && !loading && visible.length ? (
          <button
            type="button"
            onClick={onAdd}
            className="mt-1 flex items-center gap-2.5 rounded-xl border border-dashed border-cx-border px-2 py-2 text-left text-cx-sm text-cx-fg-3 outline-none transition-colors hover:bg-cx-hover hover:text-cx-fg focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]"
          >
            <span className="grid size-8 shrink-0 place-items-center rounded-lg"><Icon name="plus" size={16} /></span>
            添加 Agent
          </button>
        ) : null}
      </div>
    </nav>
  );
}
