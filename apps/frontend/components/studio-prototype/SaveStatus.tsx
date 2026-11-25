"use client";

import { useEffect, useRef, useState, type ReactNode } from "react";
import { cn } from "@/lib/cn";
import { Icon, type IconName } from "@muteki/ui";
import { Button, Callout, Dialog, EmptyState, Popover, SegmentedControl, Spinner, toast } from "@muteki/ui";
import { StudioBadge } from "./parts";
import {
  CONTENT_SLICE_KEYS,
  exportWorkspaceArchive,
  importWorkspaceArchive,
  inspectConflictSlices,
  isBackendPersistence,
  loadLatest,
  overwriteSaved,
  resolveConflictBySlices,
  syncWithServer,
  retryLoad,
  saveNow,
  switchStudioPersistenceTask,
  syncWithSimulatedServer,
  useSaveStatus,
  type ConflictChoice,
  type SavePhase,
  type SaveStatus,
  type SliceKey,
} from "./persistence";
import { formatTime } from "./records";
import { studio, useStudioState } from "./store";

const PHASE: Record<SavePhase, { label: string; icon: IconName; className: string }> = {
  loading: { label: "正在读取", icon: "loader", className: "text-cx-fg-3" },
  saved: { label: "已保存", icon: "checkCircle", className: "text-cx-fg-3" },
  pending: { label: "待保存", icon: "circleDashed", className: "text-cx-fg-3" },
  saving: { label: "正在保存", icon: "loader", className: "text-cx-fg-3" },
  error: { label: "保存失败", icon: "circleAlert", className: "text-cx-danger" },
  conflict: { label: "其他窗口有新改动", icon: "alert", className: "text-cx-warning" },
  readError: { label: "读取失败，未保存", icon: "circleAlert", className: "text-cx-danger" },
  unavailable: { label: "无法保存", icon: "alert", className: "text-cx-warning" },
};

const WHAT_IS_SAVED =
  "资产、发现、任务、计划、报告、独立证据、监控变化、权限确认、对话及处理记录均实时保存在本地 IndexedDB，并支持「云端快照同步（模拟）」与「跨设备存档 JSON 导出/导入」及「多窗口逐模块冲突合并」。";

function useSaveFeedback(status: SaveStatus) {
  const previous = useRef<SavePhase>("loading");
  useEffect(() => {
    const before = previous.current;
    previous.current = status.phase;
    if (before === status.phase) return;
    if (status.phase === "error") {
      const targetHint = isBackendPersistence()
        ? "未能同步到服务端，改动还保留在当前页面里。"
        : "改动还在这个页面里，没有写入浏览器。";
      const pauseHint = status.message.includes("暂停") ? " 请先恢复任务再试。" : "";
      toast({ title: "保存失败", description: `${status.message}${targetHint}${pauseHint}`, tone: "danger", duration: 8000, action: { label: "重试", onClick: saveNow } });
    } else if (status.phase === "readError") {
      toast({ title: "读取保存的进度失败", description: status.message, tone: "danger", duration: 0, action: { label: "重新读取", onClick: retryLoad } });
    } else if (status.phase === "unavailable") {
      toast({ title: "无法在这个浏览器里保存", description: status.message, tone: "warning", duration: 0 });
    } else if (status.phase === "conflict") {
      toast({
        title: "其他窗口保存了新改动",
        description: "自动保存已暂停。可点击右下角保存状态打开「逐模块冲突合并」保留两边改动。",
        tone: "warning",
        duration: 8000,
        action: { label: "载入最新", onClick: loadLatest },
      });
    } else if (status.phase === "saved" && (before === "error" || before === "conflict" || before === "readError")) {
      toast({ title: "已重新保存", description: status.savedAt ? `保存于 ${formatTime(status.savedAt)}` : undefined, tone: "success", duration: 2400 });
    }
  }, [status.message, status.phase, status.savedAt]);
}

export function TaskPersistenceGate({ taskId, threadId, projectId, title, goal, children }: {
  taskId: string; threadId: string; projectId: string; title: string; goal: string; children: ReactNode;
}) {
  const status = useSaveStatus();
  const [activatedTaskId, setActivatedTaskId] = useState("");
  useEffect(() => {
    switchStudioPersistenceTask(taskId, () => studio.initForTask({ taskId, threadId, projectId, title, goal }));
    setActivatedTaskId(taskId);
  }, [taskId, threadId, projectId, title, goal]);
  return <PersistenceShell status={activatedTaskId === taskId ? status : { ...status, phase: "loading" }}>{children}</PersistenceShell>;
}

function PersistenceShell({ status, children }: { status: ReturnType<typeof useSaveStatus>; children: ReactNode }) {
  useSaveFeedback(status);
  if (status.phase === "loading") {
    return (
      <div className="grid h-full min-h-0 w-full place-items-center p-6" data-testid="studio-loading" role="status">
        <EmptyState icon="database" title="正在读取保存的进度…" description="资产、发现、任务、报告和对话保存在工作台存储中。" action={<Spinner size={16} />} />
      </div>
    );
  }
  return <>{children}</>;
}

function ConflictMergeDialog({ onOpenChange }: { onOpenChange: (open: boolean) => void }) {
  const [snapshot, setSnapshot] = useState<Awaited<ReturnType<typeof inspectConflictSlices>> | null>(null);
  const [readError, setReadError] = useState("");
  const [attempt, setAttempt] = useState(0);
  const savedAt = snapshot?.savedAt;
  const [choices, setChoices] = useState<Record<Exclude<SliceKey, "ui">, ConflictChoice>>(() => {
    const init = {} as Record<Exclude<SliceKey, "ui">, ConflictChoice>;
    CONTENT_SLICE_KEYS.forEach((k) => { init[k] = "union"; });
    return init;
  });

  useEffect(() => {
    let active = true;
    setSnapshot(null);
    setReadError("");
    void inspectConflictSlices().then(
      (result) => { if (active) setSnapshot(result); },
      (error: unknown) => { if (active) setReadError(error instanceof Error ? error.message || "读取失败" : String(error)); },
    );
    return () => { active = false; };
  }, [attempt]);

  const setAll = (mode: ConflictChoice) => {
    const next = {} as Record<Exclude<SliceKey, "ui">, ConflictChoice>;
    CONTENT_SLICE_KEYS.forEach((k) => { next[k] = mode; });
    setChoices(next);
  };

  const [applying, setApplying] = useState(false);

  const applyMerge = async () => {
    if (!snapshot) return;
    setApplying(true);
    try {
      const res = await resolveConflictBySlices(choices, snapshot.remoteData);
      if (!res.ok) {
        toast({
          title: "合并保存未完成",
          description: res.error || "服务器拒绝了当前保存，请检查任务状态或重试。",
          tone: "danger",
        });
        return;
      }
      onOpenChange(false);
      toast({
        title: "已完成逐模块冲突合并并保存",
        description: "两边的增量记录已按所选策略合并写入。",
        tone: "success",
      });
    } catch (error) {
      toast({
        title: "合并保存失败",
        description: error instanceof Error ? error.message : String(error),
        tone: "danger",
      });
    } finally {
      setApplying(false);
    }
  };

  return (
    <Dialog
      open
      onOpenChange={onOpenChange}
      title="多标签页 / 跨窗口冲突逐模块合并"
      description={savedAt ? `对比当前页面与已保存版本（${formatTime(savedAt)}），支持按模块选择保留或「智能合并两边新增与修改」。` : "按模块对比当前页面与存储中的版本并逐项选择合并方式。"}
      icon="gitCompare"
      size="lg"
      testId="studio-conflict-merge-dialog"
      footer={(
        <>
          <div className="mr-auto flex flex-wrap items-center gap-1.5">
            <Button size="sm" variant="ghost" onClick={() => setAll("union")}>全部设为合并</Button>
            <Button size="sm" variant="ghost" onClick={() => setAll("local")}>全部用本页</Button>
            <Button size="sm" variant="ghost" onClick={() => setAll("remote")}>全部用另一窗口</Button>
          </div>
          <Button variant="ghost" disabled={applying} onClick={() => onOpenChange(false)}>取消</Button>
          <Button variant="primary" icon="check" disabled={applying || !snapshot} onClick={() => void applyMerge()}>
            {applying ? "正在保存…" : "应用合并并保存"}
          </Button>
        </>
      )}
    >
      <div className="flex flex-col gap-3">
        <Callout tone="accent" icon="sparkles">
          「智能合并」会按记录唯一 ID 合并当前标签页与另一窗口的新增/更新资产、发现、任务、报告和历史记录，避免整份二选一丢失任一窗口的工作。
        </Callout>
        <div className="flex flex-col divide-y divide-cx-border-subtle rounded-xl border border-cx-border-subtle">
          {readError ? (
            <Callout role="alert" tone="danger">
              无法读取已保存的版本：{readError}
              <Button size="sm" variant="ghost" onClick={() => setAttempt((value) => value + 1)}>重新读取</Button>
            </Callout>
          ) : !snapshot && <span role="status">正在读取已保存的版本…</span>}
          {snapshot?.diffs.map((item) => (
            <div key={item.key} className="flex flex-wrap items-center justify-between gap-2 px-3 py-2.5">
              <div className="flex min-w-0 flex-1 flex-col gap-0.5">
                <div className="flex items-center gap-1.5">
                  <span className="text-cx-base font-medium text-cx-fg">{item.label}</span>
                  {item.localDirty ? <StudioBadge tone="warning">本页有未保存修改</StudioBadge> : <StudioBadge>一致</StudioBadge>}
                </div>
                <span className="text-cx-xs text-cx-fg-3">
                  本页：{item.localSummary} · 存储版本：{item.remoteSummary}
                </span>
              </div>
              <SegmentedControl<ConflictChoice>
                value={choices[item.key] ?? "union"}
                onChange={(val) => setChoices((prev) => ({ ...prev, [item.key]: val }))}
                size="md"
                ariaLabel={`${item.label}合并策略`}
                options={[
                  { value: "union", label: "智能合并" },
                  { value: "local", label: "用本页" },
                  { value: "remote", label: "用另一窗口" },
                ]}
              />
            </div>
          ))}
        </div>
      </div>
    </Dialog>
  );
}

function downloadArchive() {
  const json = exportWorkspaceArchive();
  const url = URL.createObjectURL(new Blob([json], { type: "application/json;charset=utf-8" }));
  const link = document.createElement("a");
  link.href = url;
  link.download = `muteki-src-workbench-archive-${new Date().toISOString().slice(0, 10)}.json`;
  document.body.appendChild(link);
  link.click();
  link.remove();
  window.setTimeout(() => URL.revokeObjectURL(url), 0);
  toast({
    title: "已导出跨设备工作台存档",
    description: "包含全部资产、发现、任务、计划、报告与对话，可在任意设备或浏览器导入恢复。",
    tone: "success",
  });
}

function StatusDetail({ status, close, onOpenMerge }: { status: SaveStatus; close: () => void; onOpenMerge: () => void }) {
  const state = useStudioState();
  const [confirmOverwrite, setConfirmOverwrite] = useState(false);
  const fileRef = useRef<HTMLInputElement | null>(null);
  const run = (action: () => void) => () => { action(); close(); };

  let title: ReactNode = PHASE[status.phase].label;
  let body: ReactNode = isBackendPersistence()
    ? "业务记录由服务端 SRC 接口实时持久化，非核心工作台偏好保存在任务快照与本机 UI 存储。"
    : WHAT_IS_SAVED;
  let actions: ReactNode = null;
  if (status.phase === "saved") {
    title = status.savedAt ? `已保存 · ${formatTime(status.savedAt)}` : "自动保存已开启";
  } else if (status.phase === "pending" || status.phase === "saving") {
    title = status.phase === "saving" ? "正在保存…" : "有改动等待保存";
    body = "改动会在停顿片刻后自动保存到本地与模拟云端快照。";
    actions = status.phase === "pending" ? <Button size="sm" variant="secondary" onClick={run(saveNow)}>立即保存</Button> : null;
  } else if (status.phase === "error") {
    body = <>{status.message}<br />改动还在这个页面里，关闭或刷新之前请重试保存。</>;
    actions = <Button size="sm" variant="primary" icon="retry" onClick={run(saveNow)}>重试保存</Button>;
  } else if (status.phase === "conflict") {
    body = <>{status.message}<br />自动保存已暂停，你可以逐模块合并两边改动，或者选择保留其中一份。</>;
    actions = (
      <>
        <Button size="sm" variant="primary" icon="gitCompare" onClick={run(onOpenMerge)}>逐模块对照合并…</Button>
        <Button size="sm" variant="secondary" onClick={run(loadLatest)}>载入另一窗口</Button>
        <Button size="sm" variant="ghost" onClick={run(overwriteSaved)}>用本页覆盖</Button>
      </>
    );
  } else if (status.phase === "readError") {
    body = <>{status.message}{status.unsaved ? <><br />这个页面之后的改动还没有保存。</> : null}</>;
    actions = confirmOverwrite ? (
      <>
        <Button size="sm" variant="danger" onClick={run(overwriteSaved)}>确认覆盖已保存的内容</Button>
        <Button size="sm" variant="ghost" onClick={() => setConfirmOverwrite(false)}>取消</Button>
      </>
    ) : (
      <>
        <Button size="sm" variant="primary" icon="retry" onClick={run(retryLoad)}>重新读取</Button>
        <Button size="sm" variant="ghost" onClick={() => setConfirmOverwrite(true)}>改为保存这个页面的内容…</Button>
      </>
    );
  } else if (status.phase === "unavailable") {
    body = status.message;
  }

  const triggerCloudSync = () => {
    if (isBackendPersistence()) {
      close();
      void syncWithServer().then((res) => {
        toast({
          title: res.conflict ? "服务器和本地都有改动" : "已与云端工作台快照同步",
          description: res.conflict ? "可点击右下角保存状态打开「逐模块冲突合并」。" : (res.savedAt ? `最新服务器保存于 ${formatTime(res.savedAt)}` : "服务器上还没有保存的工作台快照。"),
          tone: res.conflict ? "warning" : "success",
        });
      }).catch((error: unknown) => {
        toast({ title: "云端同步失败", description: error instanceof Error ? error.message : String(error), tone: "danger" });
      });
      return;
    }
    try {
      const res = syncWithSimulatedServer();
      close();
      toast({
        title: `已同步至云端工作台快照（版本 rev-${res.syncRev}）`,
        description: `设备：${state.serverSync.deviceName} · 同步于 ${formatTime(res.savedAt)}`,
        tone: "success",
      });
    } catch (error) {
      toast({ title: "快照同步失败", description: error instanceof Error ? error.message : String(error), tone: "danger" });
    }
  };

  return (
    <div className="flex flex-col gap-2.5" data-testid="studio-save-detail">
      <div className="flex items-center justify-between gap-2">
        <p className="text-cx-base font-medium text-cx-fg">{title}</p>
        <StudioBadge tone="accent">云端同步 rev-{state.serverSync.syncRev}</StudioBadge>
      </div>
      <p className="text-cx-sm leading-5 text-cx-fg-2">{body}</p>
      <div className="rounded-xl border border-cx-border-subtle bg-cx-bg-subtle px-3 py-2 text-cx-xs leading-5 text-cx-fg-3">
        <div>跨设备/服务同步状态：最近同步于 {formatTime(state.serverSync.lastSyncedAt)}</div>
        <div>终端标识：{state.serverSync.deviceName}</div>
      </div>
      {actions ? <div className="flex flex-wrap items-center gap-1.5 pt-0.5">{actions}</div> : null}
      <div className="flex flex-wrap items-center gap-1.5 border-t border-cx-border-subtle pt-2">
        <Button size="sm" variant="secondary" icon="refresh" onClick={triggerCloudSync}>
          同步到云端快照
        </Button>
        <Button size="sm" variant="ghost" icon="gitCompare" onClick={run(onOpenMerge)}>
          多窗口合并
        </Button>
        <Button size="sm" variant="ghost" icon="download" onClick={run(downloadArchive)}>
          导出跨设备存档
        </Button>
        <Button size="sm" variant="ghost" icon="upload" onClick={() => fileRef.current?.click()}>
          导入存档
        </Button>
        <input
          ref={fileRef}
          type="file"
          accept=".json,application/json"
          className="hidden"
          onChange={(e) => {
            const file = e.target.files?.[0];
            e.target.value = "";
            if (!file) return;
            void file.text().then((text) => {
              const res = importWorkspaceArchive(text);
              if (res.ok) {
                close();
                toast({ title: "已恢复跨设备存档", description: res.summary, tone: "success" });
              } else {
                toast({ title: "导入存档失败", description: res.error, tone: "danger" });
              }
            });
          }}
        />
      </div>
    </div>
  );
}

export function StudioSaveIndicator() {
  const status = useSaveStatus();
  const [mergeOpen, setMergeOpen] = useState(false);
  const meta = PHASE[status.phase];
  const busy = status.phase === "saving" || status.phase === "loading";
  const label = status.phase === "saved" && !status.savedAt ? "自动保存" : meta.label;
  return (
    <>
      <Popover
        placement="top-start"
        ariaLabel="保存与跨设备同步状态"
        className="w-[min(420px,92vw)] p-3"
        trigger={(
          <button
            type="button"
            aria-label={`保存状态：${label}`}
            className={cn(
              "inline-flex h-[32px] min-w-0 shrink-0 items-center gap-1.5 rounded-lg px-2 text-cx-sm outline-none hover:bg-cx-hover focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]",
              meta.className,
            )}
            data-testid="studio-save-status"
            data-phase={status.phase}
          >
            <Icon name={meta.icon} size={14} className={cn("shrink-0", busy && "cx-spin")} />
            <span className="truncate">{label}</span>
          </button>
        )}
      >
        {({ close }) => <StatusDetail status={status} close={close} onOpenMerge={() => setMergeOpen(true)} />}
      </Popover>
      {mergeOpen && <ConflictMergeDialog onOpenChange={setMergeOpen} />}
    </>
  );
}
