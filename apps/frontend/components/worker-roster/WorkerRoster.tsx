// ai-design-check-ignore-file sentence-case -- "Review Worker" and "Verifier Worker" are product role names
"use client";

import { useEffect, useMemo, useState, type DragEvent } from "react";
import { Button, Callout, EmptyState, SearchInput, Switch, Tooltip } from "@muteki/ui";
import { SettingsNote, SettingsSection } from "@/components/settings/primitives";
import { copyToClipboard } from "@/lib/clipboard";
import { scrollBehavior } from "@/lib/usePrefersReducedMotion";
import {
  useAgentVersionPolicy, workerVersionIssue,
  type PolicyModeId, type WorkerVersionIssue,
} from "@/lib/agentVersionPolicy";
import {
  loadProviderDescriptors, readyCatalog, useProviderDescriptors,
} from "@/lib/providerDescriptors";
import type { GlobalCredential, WorkerModelOptions, WorkerModelTestResult, WorkerSettings } from "@/lib/useRun";
import { AuxiliaryRow } from "./AuxiliaryRow";
import { AuxiliarySheet, SeatSheet } from "./DetailSheets";
import { WorkerRow, type RowDragHandlers } from "./WorkerRow";
import {
  canServeChannel, credentialKey, credentialsForEngine, engineLocalOnly, globalCredentialUsable, isOrdinarySeat,
  workerEngines,
  type BatchCheckState, type Credential, type ModelDiscoveryOutcome, type ReviewPolicy, type Seat, type VerifierPolicy,
} from "./model";

type SheetState = { kind: "seat"; id: string } | { kind: "review" } | { kind: "verifier" } | null;

export interface WorkerRosterProps {
  seats: Seat[];
  credentials: Credential[];
  availableCredentials: GlobalCredential[];
  models: WorkerModelOptions;
  backend: WorkerSettings["worker_backend"];
  mode?: PolicyModeId;
  nodeId?: string;
  review: ReviewPolicy;
  verifier: VerifierPolicy;
  testingIds: Set<string>;
  testResults: Record<string, WorkerModelTestResult>;
  batchCheck: BatchCheckState;
  discoveringModels: boolean;
  autoCheck: boolean;
  highlightId: string | null;
  onAutoCheck: (value: boolean) => void;
  onAddWorker: () => void;
  onUpdateSeat: (id: string, patch: Partial<Seat>) => void;
  onBindAccount: (id: string, key: string, credential?: GlobalCredential) => void;
  onChangeEngine: (id: string, engine: string) => void;
  onDuplicate: (id: string) => void;
  onDelete: (id: string) => void;
  onReorder: (source: string, target: string | null) => void;
  onToggleEnabled: (id: string) => void;
  onDisableSeats?: (ids: string[]) => void;
  onReview: (patch: Partial<ReviewPolicy>) => void;
  onVerifier: (patch: Partial<VerifierPolicy>) => void;
  onSetReview: (id: string) => void;
  onSetVerifier: (id: string) => void;
  onCreateDedicatedReview: () => void;
  onCreateDedicatedVerifier: () => void;
  onTest: (seat: Seat) => void;
  onTestAll: () => void;
  onDiscoverModels: (credentialId: string, engine: string) => Promise<ModelDiscoveryOutcome>;
  onOpenCredentials: () => void;
  onOpenVersions?: () => void;
  onOpenRuntime?: () => void;
  onNotice: (message: string) => void;
}

export function WorkerRoster(props: WorkerRosterProps) {
  const {
    seats, credentials, availableCredentials, models, backend, mode, nodeId = "local", review, verifier, testingIds, testResults, batchCheck,
    discoveringModels, autoCheck, highlightId, onAutoCheck, onAddWorker, onUpdateSeat, onBindAccount, onChangeEngine,
    onDuplicate, onDelete, onReorder, onToggleEnabled, onDisableSeats, onReview, onVerifier, onSetReview, onSetVerifier,
    onCreateDedicatedReview, onCreateDedicatedVerifier, onTest, onTestAll, onDiscoverModels, onOpenCredentials, onOpenVersions, onOpenRuntime, onNotice,
  } = props;
  const descriptorState = useProviderDescriptors();
  const catalog = readyCatalog(descriptorState);
  const versionPolicy = useAgentVersionPolicy();
  const [query, setQuery] = useState("");
  const [sheet, setSheet] = useState<SheetState>(null);
  const [draggingId, setDraggingId] = useState<string | null>(null);
  const [dragOverId, setDragOverId] = useState<string | null>(null);
  const [copiedCmd, setCopiedCmd] = useState("");

  const versionIssuesBySeat = useMemo(() => {
    const map = new Map<string, WorkerVersionIssue>();
    for (const seat of seats) {
      const issue = workerVersionIssue(versionPolicy.data, seat.engine, backend, mode, nodeId);
      if (issue) map.set(seat.id, issue);
    }
    return map;
  }, [backend, mode, nodeId, seats, versionPolicy.data]);

  const versionBlockedIds = useMemo(
    () => new Set<string>(versionIssuesBySeat.keys()),
    [versionIssuesBySeat],
  );

  const enabledViolatingSeats = useMemo(() => seats.filter((seat) => {
    if (!seat.enabled || !versionIssuesBySeat.has(seat.id)) return false;
    return isOrdinarySeat(seat)
      || (review.enabled && review.engine === seat.id)
      || (verifier.enabled && verifier.engine === seat.id);
  }), [review.enabled, review.engine, seats, verifier.enabled, verifier.engine, versionIssuesBySeat]);

  const ordinary = useMemo(() => seats.filter(isOrdinarySeat), [seats]);
  const needle = query.trim().toLocaleLowerCase("en-US");
  const visible = needle ? ordinary.filter((seat) => `${seat.label} ${seat.engine} ${seat.model}`.toLocaleLowerCase("en-US").includes(needle)) : ordinary;
  const enabledOrdinary = ordinary.filter((seat) => seat.enabled);
  const concurrency = enabledOrdinary.reduce((sum, seat) => sum + Math.max(1, seat.capacity.max_running || 1), 0);
  const checked = enabledOrdinary.filter((seat) => testResults[seat.id]);
  const passed = checked.filter((seat) => testResults[seat.id]?.ok).length;
  const checkSummary = batchCheck.running
    ? `正在检查 ${batchCheck.completed}/${batchCheck.total}`
    : checked.length
      ? `自检 ${passed}/${enabledOrdinary.length} 通过`
      : enabledOrdinary.length ? `待自检 ${enabledOrdinary.length}` : "";
  const summary = [`${enabledOrdinary.length} 个启用`, `并发合计 ${concurrency}`, checkSummary].filter(Boolean).join(" · ");

  useEffect(() => {
    if (!highlightId) return;
    const frame = window.requestAnimationFrame(() => {
      document.querySelector(`[data-seat-id="${CSS.escape(highlightId)}"]`)?.scrollIntoView({ block: "nearest", behavior: scrollBehavior() });
    });
    return () => window.cancelAnimationFrame(frame);
  }, [highlightId]);

  const dragFor = (seat: Seat): RowDragHandlers => ({
    dragging: draggingId === seat.id,
    dropTarget: dragOverId === seat.id && draggingId !== seat.id,
    disabled: Boolean(needle),
    onDragStart: (event: DragEvent<HTMLElement>) => {
      event.dataTransfer.setData("text/plain", seat.id);
      event.dataTransfer.effectAllowed = "move";
      setDraggingId(seat.id);
    },
    onDragEnter: () => { if (draggingId && draggingId !== seat.id) setDragOverId(seat.id); },
    onDragOver: (event) => {
      if (!draggingId) return;
      event.preventDefault();
      event.dataTransfer.dropEffect = "move";
    },
    onDrop: (event) => {
      event.preventDefault();
      event.stopPropagation();
      if (draggingId && draggingId !== seat.id) onReorder(draggingId, seat.id);
      setDraggingId(null);
      setDragOverId(null);
    },
    onDragEnd: () => { setDraggingId(null); setDragOverId(null); },
    onMove: (direction) => {
      const index = ordinary.findIndex((item) => item.id === seat.id);
      const target = ordinary[index + direction];
      if (target) onReorder(seat.id, target.id);
    },
  });

  const addable = useMemo(() => {
    const groups: { engine: string; credentials: GlobalCredential[] }[] = [];
    for (const engine of workerEngines(catalog)) {
      if (engineLocalOnly(catalog, engine) && backend !== "local") continue;
      const usable = credentialsForEngine(availableCredentials, engine, backend).filter(globalCredentialUsable);
      if (usable.length) groups.push({ engine, credentials: usable });
    }
    return groups;
  }, [availableCredentials, backend, catalog]);

  const enableAuxiliary = (kind: "review" | "verifier", enabled: boolean) => {
    const policy = kind === "review" ? review : verifier;
    const apply = kind === "review" ? onReview : onVerifier;
    if (!enabled) { apply({ enabled: false }); return; }
    if (policy.engine && seats.some((seat) => seat.id === policy.engine && seat.enabled && !versionBlockedIds.has(seat.id))) { apply({ enabled: true }); return; }
    const first = seats.find((seat) => seat.enabled && !versionBlockedIds.has(seat.id) && canServeChannel(seat, kind));
    if (!first) { onNotice("请先启用一个满足最低版本要求的 Worker，再开启辅助 Worker"); return; }
    (kind === "review" ? onSetReview : onSetVerifier)(first.id);
  };

  const testAuxiliary = (kind: "review" | "verifier") => {
    const policy = kind === "review" ? review : verifier;
    const seat = seats.find((item) => item.id === policy.engine);
    if (!seat) return;
    const override = policy.reasoning_effort && policy.reasoning_effort !== "inherit" ? policy.reasoning_effort : seat.reasoning_effort || "default";
    onTest({ ...seat, reasoning_effort: override });
  };

  const sheetSeat = sheet?.kind === "seat" ? seats.find((seat) => seat.id === sheet.id) ?? null : null;
  const binding = { credentials, availableCredentials, models, backend, mode, nodeId, discovering: discoveringModels, onDiscoverModels, onOpenCredentials, onOpenVersions };
  const reviewSeat = seats.find((seat) => seat.id === review.engine);
  const verifierSeat = seats.find((seat) => seat.id === verifier.engine);
  const checkBusy = batchCheck.running || testingIds.size > 0;
  const modeLabel = mode === "pentest" ? "Pentest 渗透模式" : "CTF 单题模式";

  const toolbar = (
    <div className="flex flex-wrap items-center gap-2">
      <Tooltip content="修改凭据、模型或推理强度后，自动发起一次真实模型请求" placement="bottom">
        <span><Switch size="sm" checked={autoCheck} onCheckedChange={onAutoCheck} label={<span className="text-cx-xs text-cx-fg-3">修改后自动自检</span>} /></span>
      </Tooltip>
      <Button size="sm" variant="outline" icon="refresh" loading={batchCheck.running} disabled={!enabledOrdinary.length || checkBusy} onClick={onTestAll}>
        {batchCheck.running ? `检查中 ${batchCheck.completed}/${batchCheck.total}` : "一键检查"}
      </Button>
      <Button size="sm" variant="secondary" icon="plus" onClick={onAddWorker}>添加 Worker</Button>
    </div>
  );

  return (
    <div className="@container flex flex-col gap-8">
      {descriptorState.status === "error" ? (
        <Callout
          tone="warning"
          icon="alert"
          title="引擎描述读取失败"
          action={<Button size="sm" variant="outline" icon="refresh" onClick={() => void loadProviderDescriptors({ fresh: true }).catch(() => undefined)}>重试</Button>}
        >
          无法添加 Worker 或读取推理强度:{descriptorState.message}
        </Callout>
      ) : null}

      {enabledViolatingSeats.length > 0 ? (
        <Callout
          tone="danger"
          icon="alert"
          title={`存在 ${enabledViolatingSeats.length} 个已启用出战 Worker 低于最低支持版本（禁止出战）`}
          action={(
            <div className="flex flex-wrap items-center gap-1.5">
              {onDisableSeats ? (
                <Button
                  size="sm"
                  variant="outline"
                  onClick={() => onDisableSeats(enabledViolatingSeats.map((item) => item.id))}
                >
                  一键停用低版本席位
                </Button>
              ) : null}
              {backend === "container" && onOpenRuntime ? (
                <Button size="sm" variant="outline" onClick={onOpenRuntime}>切换本地运行</Button>
              ) : null}
              {onOpenVersions ? (
                <Button size="sm" variant="secondary" onClick={onOpenVersions}>版本要求与升级指引</Button>
              ) : null}
            </div>
          )}
        >
          <div className="flex flex-col gap-1.5 text-cx-xs leading-5">
            <span>
              当前处于 <strong>{modeLabel}</strong>（{backend === "container" ? "容器运行" : "本地运行"}）：以下席位的 Agent 低于最低版本，升级或停用后再保存：
            </span>
            <ul className="flex flex-col gap-1 pl-4 list-disc">
              {enabledViolatingSeats.map((seat) => {
                const issue = versionIssuesBySeat.get(seat.id)!;
                return (
                  <li key={seat.id} className="flex flex-wrap items-center gap-2">
                    <span>
                      <strong>{seat.label}</strong>（{issue.displayName} 当前 <code className="font-cx-mono">{issue.installedVersion}</code> &lt; 最低 <code className="font-cx-mono">{issue.minVersion}</code>）
                    </span>
                    {issue.upgradeCommand ? (
                      <Button
                        size="xs"
                        variant="ghost"
                        icon={copiedCmd === seat.id ? "check" : "copy"}
                        onClick={async () => {
                          if (await copyToClipboard(issue.upgradeCommand)) {
                            setCopiedCmd(seat.id);
                            window.setTimeout(() => setCopiedCmd((cur) => (cur === seat.id ? "" : cur)), 1800);
                          }
                        }}
                      >
                        {copiedCmd === seat.id ? "已复制" : `复制升级命令：${issue.upgradeCommand}`}
                      </Button>
                    ) : null}
                  </li>
                );
              })}
            </ul>
          </div>
        </Callout>
      ) : null}

      <div className="flex flex-col gap-2">
        <SettingsSection title="Worker" description={summary || "还没有 Worker"} actions={toolbar}>
          {ordinary.length > 5 ? (
            <div className="border-b border-cx-border-subtle px-4 py-2.5">
              <SearchInput value={query} onValueChange={setQuery} placeholder="搜索 Worker 或模型…" aria-label="搜索 Worker 或模型" />
            </div>
          ) : null}
          <div
            onDragOver={(event) => {
              if (!draggingId) return;
              event.preventDefault();
              event.dataTransfer.dropEffect = "move";
            }}
            onDrop={(event) => {
              event.preventDefault();
              if (draggingId) onReorder(draggingId, null);
              setDraggingId(null);
              setDragOverId(null);
            }}
          >
            {visible.length ? visible.map((seat) => {
              const account = credentialKey(credentials.find((item) => item.id === seat.credential_id));
              return (
                <WorkerRow
                  key={seat.id}
                  seat={seat}
                  credentials={credentials}
                  availableCredentials={availableCredentials}
                  models={models}
                  backend={backend}
                  accountKey={account}
                  testing={testingIds.has(seat.id)}
                  testResult={testResults[seat.id]}
                  versionIssue={versionIssuesBySeat.get(seat.id) ?? null}
                  highlighted={highlightId === seat.id}
                  isReview={review.enabled === true && review.engine === seat.id}
                  isVerifier={verifier.enabled === true && verifier.engine === seat.id}
                  drag={dragFor(seat)}
                  onUpdate={(patch) => onUpdateSeat(seat.id, patch)}
                  onBind={(key, credential) => onBindAccount(seat.id, key, credential)}
                  onOpenCredentials={onOpenCredentials}
                  onOpenVersions={onOpenVersions}
                  onToggleEnabled={() => onToggleEnabled(seat.id)}
                  onTest={() => onTest(seat)}
                  onOpenDetail={() => setSheet({ kind: "seat", id: seat.id })}
                  onDuplicate={() => onDuplicate(seat.id)}
                  onDelete={() => onDelete(seat.id)}
                  onSetReview={() => onSetReview(seat.id)}
                  onSetVerifier={() => onSetVerifier(seat.id)}
                />
              );
            }) : (
              <EmptyState
                icon="grid"
                title={needle ? "没有匹配的 Worker" : "还没有 Worker"}
                description={needle ? "换个名称或模型再试试。" : addable.length ? "点击右上角「添加 Worker」，选择引擎、账号和模型。" : "先在 Agent 凭据里配置账号，再回来添加 Worker。"}
                action={!needle && !addable.length ? <Button size="sm" variant="outline" icon="lock" onClick={onOpenCredentials}>去配置凭据</Button> : undefined}
                className="py-10"
              />
            )}
          </div>
        </SettingsSection>
        {ordinary.length > 1 ? <SettingsNote>从上到下按优先级启动，拖动行首的 ⠿ 可以调整顺序。</SettingsNote> : null}
      </div>

      <SettingsSection title="辅助 Worker" description="仅 Pentest 生效。可沿用上面的 Worker,也可以单独配置。">
        <AuxiliaryRow
          kind="review" title="Review Worker" description="审查建议 · BTW 自动模式候选" icon="eye"
          enabled={review.enabled === true} engineId={review.engine || ""} seats={seats} models={models}
          effort={review.reasoning_effort || "inherit"}
          testing={testingIds.has(review.engine || "")} testResult={review.engine ? testResults[review.engine] : undefined}
          versionBlockedIds={versionBlockedIds}
          onEnabled={(value) => enableAuxiliary("review", value)}
          onSelectSeat={onSetReview}
          onCreateDedicated={onCreateDedicatedReview}
          onEffort={(value) => onReview({ reasoning_effort: value })}
          onTest={() => testAuxiliary("review")}
          onOpenSettings={() => setSheet({ kind: "review" })}
        />
        <AuxiliaryRow
          kind="verifier" title="Verifier Worker" description="独立复现报告" icon="check"
          enabled={verifier.enabled === true} engineId={verifier.engine || ""} seats={seats} models={models}
          effort={verifier.reasoning_effort || "inherit"} concurrency={verifier.max_concurrent ?? 0}
          testing={testingIds.has(verifier.engine || "")} testResult={verifier.engine ? testResults[verifier.engine] : undefined}
          versionBlockedIds={versionBlockedIds}
          onEnabled={(value) => enableAuxiliary("verifier", value)}
          onSelectSeat={onSetVerifier}
          onCreateDedicated={onCreateDedicatedVerifier}
          onEffort={(value) => onVerifier({ reasoning_effort: value })}
          onConcurrency={(value) => onVerifier({ max_concurrent: value })}
          onTest={() => testAuxiliary("verifier")}
          onOpenSettings={() => setSheet({ kind: "verifier" })}
        />
      </SettingsSection>

      <SeatSheet
        {...binding}
        seat={sheetSeat}
        open={sheet?.kind === "seat"}
        onOpenChange={(open) => { if (!open) setSheet(null); }}
        testing={sheetSeat ? testingIds.has(sheetSeat.id) : false}
        testResult={sheetSeat ? testResults[sheetSeat.id] : undefined}
        onUpdate={(patch) => sheetSeat && onUpdateSeat(sheetSeat.id, patch)}
        onBind={(key, credential) => sheetSeat && onBindAccount(sheetSeat.id, key, credential)}
        onEngine={(engine) => sheetSeat && onChangeEngine(sheetSeat.id, engine)}
        onTest={() => sheetSeat && onTest(sheetSeat)}
        onDuplicate={() => { if (sheetSeat) onDuplicate(sheetSeat.id); setSheet(null); }}
        onDelete={() => { if (sheetSeat) onDelete(sheetSeat.id); setSheet(null); }}
      />
      <AuxiliarySheet
        {...binding}
        kind={sheet?.kind === "verifier" ? "verifier" : "review"}
        open={sheet?.kind === "review" || sheet?.kind === "verifier"}
        onOpenChange={(open) => { if (!open) setSheet(null); }}
        seats={seats}
        review={review}
        verifier={verifier}
        testing={testingIds.has((sheet?.kind === "verifier" ? verifierSeat : reviewSeat)?.id || "")}
        testResult={(sheet?.kind === "verifier" ? verifierSeat : reviewSeat) ? testResults[(sheet?.kind === "verifier" ? verifierSeat : reviewSeat)!.id] : undefined}
        onReview={onReview}
        onVerifier={onVerifier}
        onSeatUpdate={onUpdateSeat}
        onSeatBind={onBindAccount}
        onSeatEngine={onChangeEngine}
        onEditOrdinary={(id) => setSheet({ kind: "seat", id })}
        onTest={() => testAuxiliary(sheet?.kind === "verifier" ? "verifier" : "review")}
      />
    </div>
  );
}
