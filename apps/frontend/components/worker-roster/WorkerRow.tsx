// ai-design-check-ignore-file use-shared-components -- the drag handle and the two-line name button are bespoke row controls
// ai-design-check-ignore-file no-hover-only-content -- the drag handle is also revealed by focus and keyboard (Alt+arrows) reorder
"use client";

import { useState, type DragEvent, type ReactNode } from "react";
import { EngineLogo } from "@/components/EngineLogo";
import { Icon } from "@muteki/ui";
import { Badge, Button, IconButton, Menu, MenuItem, MenuSeparator, Switch, useContextMenu } from "@muteki/ui";
import { copyToClipboard } from "@/lib/clipboard";
import { cn } from "@/lib/cn";
import type { WorkerVersionIssue } from "@/lib/agentVersionPolicy";
import { engineDisplayName, readyCatalog, useProviderDescriptors } from "@/lib/providerDescriptors";
import type { WorkerModelTestResult } from "@/lib/useRun";
import { CheckStatus } from "./CheckStatus";
import { ConcurrencyStepper, SeatFields, type SeatFieldsProps } from "./SeatFields";

export type RowDragHandlers = {
  dragging: boolean;
  dropTarget: boolean;
  onDragStart: (event: DragEvent<HTMLElement>) => void;
  onDragEnter: () => void;
  onDragOver: (event: DragEvent<HTMLElement>) => void;
  onDrop: (event: DragEvent<HTMLElement>) => void;
  onDragEnd: () => void;
  onMove: (direction: -1 | 1) => void;
  disabled: boolean;
};

export type WorkerRowProps = Omit<SeatFieldsProps, "labelled"> & {
  testing: boolean;
  testResult?: WorkerModelTestResult;
  versionIssue?: WorkerVersionIssue | null;
  highlighted: boolean;
  isReview: boolean;
  isVerifier: boolean;
  drag: RowDragHandlers;
  onToggleEnabled: () => void;
  onTest: () => void;
  onOpenDetail: () => void;
  onOpenVersions?: () => void;
  onDuplicate: () => void;
  onDelete: () => void;
  onSetReview: () => void;
  onSetVerifier: () => void;
};

export function RowMenuItems({ enabled, testing, versionBlocked, isReview, isVerifier, onOpenDetail, onDuplicate, onTest, onToggleEnabled, onSetReview, onSetVerifier, onDelete }: {
  enabled: boolean;
  testing: boolean;
  versionBlocked?: boolean;
  isReview: boolean;
  isVerifier: boolean;
  onOpenDetail: () => void;
  onDuplicate: () => void;
  onTest: () => void;
  onToggleEnabled: () => void;
  onSetReview: () => void;
  onSetVerifier: () => void;
  onDelete: () => void;
}): ReactNode {
  return (
    <>
      <MenuItem icon="pencil" onSelect={onOpenDetail}>详细设置</MenuItem>
      <MenuItem icon="plug" disabled={testing || !enabled || Boolean(versionBlocked)} onSelect={onTest}>{testing ? "正在自检" : "立即自检"}</MenuItem>
      <MenuItem icon="copy" onSelect={onDuplicate}>复制</MenuItem>
      <MenuItem icon={enabled ? "pause" : "play"} disabled={!enabled && Boolean(versionBlocked)} onSelect={onToggleEnabled}>{enabled ? "停用" : versionBlocked ? "版本过低（禁止启用）" : "启用"}</MenuItem>
      <MenuSeparator />
      <MenuItem icon="eye" checked={isReview} disabled={!isReview && Boolean(versionBlocked)} onSelect={onSetReview}>用作 Review Worker</MenuItem>
      <MenuItem icon="check" checked={isVerifier} disabled={!isVerifier && Boolean(versionBlocked)} onSelect={onSetVerifier}>用作 Verifier Worker</MenuItem>
      <MenuSeparator />
      <MenuItem icon="trash" danger onSelect={onDelete}>移除</MenuItem>
    </>
  );
}

export function WorkerRow(props: WorkerRowProps) {
  const {
    seat, testing, testResult, versionIssue, highlighted, isReview, isVerifier, drag,
    onToggleEnabled, onTest, onOpenDetail, onOpenVersions, onDuplicate, onDelete, onSetReview, onSetVerifier, onUpdate,
  } = props;
  const catalog = readyCatalog(useProviderDescriptors());
  const [armed, setArmed] = useState(false);
  const [copiedUpgrade, setCopiedUpgrade] = useState(false);
  const ctx = useContextMenu();
  const failed = seat.enabled && !testing && testResult && !testResult.ok;
  const menu = (
    <RowMenuItems
      enabled={seat.enabled} testing={testing} versionBlocked={Boolean(versionIssue)} isReview={isReview} isVerifier={isVerifier}
      onOpenDetail={onOpenDetail} onDuplicate={onDuplicate} onTest={onTest} onToggleEnabled={onToggleEnabled}
      onSetReview={onSetReview} onSetVerifier={onSetVerifier} onDelete={onDelete}
    />
  );
  const roleHint = [isReview ? "Review" : "", isVerifier ? "Verifier" : ""].filter(Boolean).join(" · ");

  return (
    <div
      data-seat-id={seat.id}
      draggable={armed && !drag.disabled}
      onContextMenu={ctx.onContextMenu}
      onKeyDown={ctx.onKeyDown}
      onDragStart={drag.onDragStart}
      onDragEnter={drag.onDragEnter}
      onDragOver={drag.onDragOver}
      onDrop={drag.onDrop}
      onDragEnd={() => { setArmed(false); drag.onDragEnd(); }}
      className={cn(
        "group relative flex flex-wrap items-center gap-x-3 gap-y-2 border-b border-cx-border-subtle px-4 py-3 transition-colors duration-200 last:border-b-0",
        "hover:bg-cx-hover/60",
        highlighted && "bg-cx-accent-soft",
        drag.dragging && "opacity-45",
        drag.dropTarget && "shadow-[inset_0_2px_0_var(--cx-accent)]",
      )}
    >
      <div className="order-1 flex min-w-0 flex-1 items-center gap-2 @[960px]:w-[184px] @[960px]:flex-none">
        <button
          type="button"
          aria-label={`拖动调整 ${seat.label} 的优先级`}
          title={drag.disabled ? "搜索时不能排序" : "拖动调整优先级（也可按 Alt + ↑/↓）"}
          disabled={drag.disabled}
          onPointerDown={() => setArmed(true)}
          onPointerUp={() => setArmed(false)}
          onKeyDown={(event) => {
            if (!event.altKey) return;
            if (event.key === "ArrowUp") { event.preventDefault(); drag.onMove(-1); }
            if (event.key === "ArrowDown") { event.preventDefault(); drag.onMove(1); }
          }}
          className="absolute left-0.5 top-3 grid h-8 w-3.5 cursor-grab place-items-center rounded text-cx-fg-4 opacity-0 hover:text-cx-fg-2 focus-visible:opacity-100 group-focus-within:opacity-100 group-hover:opacity-100 disabled:cursor-default disabled:opacity-0 active:cursor-grabbing"
        >
          <Icon name="gripVertical" size={14} />
        </button>
        <span className="grid size-8 shrink-0 place-items-center rounded-lg border border-cx-border bg-cx-bg">
          <EngineLogo engine={seat.engine} size={18} />
        </span>
        <button type="button" onClick={onOpenDetail} className="min-w-0 rounded-md text-left outline-none focus-visible:ring-2 focus-visible:ring-[var(--cx-focus)]" title="打开详细设置">
          <div className="flex items-center gap-1.5">
            <span className={cn("truncate text-cx-sm font-medium", seat.enabled ? "text-cx-fg" : "text-cx-fg-3")}>{seat.label}</span>
            {versionIssue ? <Badge tone="danger">版本过低</Badge> : null}
          </div>
          <div className="truncate text-cx-xs text-cx-fg-3">{engineDisplayName(catalog, seat.engine)}{roleHint ? ` · ${roleHint}` : ""}</div>
        </button>
      </div>

      <div className={cn(
        "order-3 grid w-full grid-cols-2 gap-2 transition-opacity @[620px]:grid-cols-[minmax(0,1fr)_minmax(0,1.3fr)_108px_96px] @[960px]:order-2 @[960px]:w-auto @[960px]:flex-1",
        !seat.enabled && "opacity-60",
      )}>
        <SeatFields {...props} />
        <div className="min-w-0">
          <ConcurrencyStepper value={seat.capacity.max_running} onChange={(next) => onUpdate({ capacity: { ...seat.capacity, max_running: next } })} />
        </div>
      </div>

      <div className="order-2 flex shrink-0 items-center gap-1 @[960px]:order-3">
        <CheckStatus enabled={seat.enabled} testing={testing} result={testResult} onCheck={onTest} className="w-[84px] justify-start" />
        <Switch size="sm" checked={seat.enabled} onCheckedChange={onToggleEnabled} ariaLabel={`${seat.label} 启用状态`} className="mx-1" />
        <Menu trigger={<IconButton icon="more" label={`${seat.label} 更多操作`} noTooltip />} placement="bottom-end" ariaLabel={`${seat.label} 操作`}>{menu}</Menu>
      </div>

      {versionIssue ? (
        <div className="order-4 flex w-full flex-wrap items-center gap-2 pl-10 text-cx-xs text-cx-danger">
          <Icon name="circleAlert" size={13} className="shrink-0" />
          <span className="min-w-0 flex-1" title={versionIssue.detail}>
            {versionIssue.detail}
          </span>
          {versionIssue.upgradeCommand ? (
            <Button
              size="xs"
              variant="ghost"
              icon={copiedUpgrade ? "check" : "copy"}
              onClick={() => void copyToClipboard(versionIssue.upgradeCommand).then((ok) => {
                if (ok) {
                  setCopiedUpgrade(true);
                  window.setTimeout(() => setCopiedUpgrade(false), 1800);
                }
              })}
            >
              {copiedUpgrade ? "已复制" : "复制升级命令"}
            </Button>
          ) : null}
          {onOpenVersions ? (
            <Button size="xs" variant="ghost" onClick={onOpenVersions}>版本设置</Button>
          ) : null}
        </div>
      ) : null}
      {failed && !versionIssue ? (
        <div className="order-4 flex w-full items-center gap-2 pl-10 text-cx-xs text-cx-danger">
          <Icon name="circleAlert" size={13} className="shrink-0" />
          <span className="min-w-0 flex-1 truncate" title={testResult.detail}>{testResult.detail || "真实模型请求失败"}</span>
          <Button size="xs" variant="ghost" onClick={onOpenDetail}>查看日志</Button>
          <Button size="xs" variant="ghost" onClick={onTest}>重试</Button>
        </div>
      ) : null}
      {ctx.render(menu, `${seat.label} 快捷操作`)}
    </div>
  );
}
