"use client";

import { useCallback, useEffect, useMemo, useRef, useState, type CSSProperties, type KeyboardEvent } from "react";
import { Background, Controls, ReactFlow, type Edge, type Node as FlowNode } from "@xyflow/react";
import "@xyflow/react/dist/style.css";
import { cn } from "@/lib/cn";
import { scrollBehavior } from "@/lib/usePrefersReducedMotion";
import { Icon, type IconName } from "@muteki/ui";
import {
  Button,
  Callout,
  Checkbox,
  CopyButton,
  Dialog,
  EmptyState,
  IconButton,
  Input,
  Label,
  Menu,
  MenuItem,
  MenuLabel,
  MenuSeparator,
  SearchInput,
  SegmentedControl,
  Select,
  Switch,
  TextArea,
  TextField,
  toast,
  useContextMenu,
} from "@muteki/ui";
import { SectionHeader, SurfaceToolbar, relativeTime } from "@/components/chat/surfaces/shared";
import { SurfaceCard, SurfaceRow } from "@/components/chat/surfaces/OverviewSurface";
import { PathBreadcrumb } from "@/components/chat/surfaces/PathBreadcrumb";
import {
  ASSET_IMPORTANCE_LEVELS,
  SOURCE_KINDS,
  SRC_ASSET_TYPES,
  assetTypeLabel,
  categoryLabel,
  importanceLabel,
  sourceKindLabel,
  statusLabel,
  type AssetImportance,
  type AssetVersionSnapshot,
  type SourceKind,
  type SrcAssetType,
  type StudioPassage,
  type StudioRef,
  type StudioSource,
} from "./model";
import { openRef } from "./nav";
import {
  AttachButton,
  HighlightedText,
  NewMark,
  SheetAskBar,
  StudioBadge,
  askAi,
  dragRef,
  quoteToComposer,
  useRememberedScroll,
  useViewing,
  citeRef,
} from "./parts";
import { Dl, sendAbout } from "./Kit";
import { ImportHistory, ImportOriginLink, ImportSourcesDialog, ImportStatusRow } from "./ImportViews";
import {
  EMPTY_SOURCE_FILTER,
  SOURCE_STATUSES,
  countLabel,
  findSourceDuplicates,
  formatTime,
  mergeOf,
  displayTitle,
  pickSourceFilter,
  RUN_STATE_LABEL,
  sameFilter,
  sourceStatusLabel,
  type FilterTimeRange,
  type FindingsPresence,
  type MergeRecord,
  type SourceFilter,
} from "./records";
import {
  BatchExportDialog,
  MergeInfo,
  VirtualRecordList,
  BatchLinkDialog,
  BatchTagDialog,
  DuplicatesView,
  FilterChips,
  FilterField,
  FilterSelect,
  HistoryView,
  MergeDialog,
  MergeMark,
  RecordHistory,
  batchScope,
  reportChange,
  revertEntry,
  scopedHistory,
  useChecklist,
  type FilterChip,
} from "./RecordTools";
import { postCommand, querySrc } from "../src/reviewPrimitives";
import {
  arrayBufferToBase64,
  putConnectorCredential,
  saveAssetJudgement,
  saveConnectorConfig,
  splitMergedAsset,
  verifyConnectorConnection,
} from "./real/assets";
import { afterDomainMutation, attachedTaskId, hasAttachedDomains, readStudioState, studio, useStudioState, type ImportOrigin, type LibraryUi, type StudioState } from "./store";

function FoldedRoutesPanel({ taskId, onBack }: { taskId: string; onBack: () => void }) {
  const [groups, setGroups] = useState<Array<Record<string, unknown>>>([]);
  const [host, setHost] = useState("");
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    if (!taskId) return;
    setBusy(true);
    try {
      const res = await querySrc<{ groups?: Array<Record<string, unknown>> }>(taskId, "src.asset.fold_routes", {
        host: host.trim() || undefined,
        limit: 200,
      });
      setGroups(res.groups ?? []);
    } catch (err) {
      toast({ title: "加载路由折叠失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
    } finally {
      setBusy(false);
    }
  }, [host, taskId]);

  useEffect(() => {
    void load();
  }, [load]);

  return (
    <div className="cx-scroll min-h-0 flex-1 overflow-y-auto p-4" data-testid="studio-folded-routes-panel">
      <div className="mx-auto flex max-w-[820px] flex-col gap-3">
        <div className="flex flex-wrap items-center gap-2">
          <Button size="md" variant="ghost" icon="arrowLeft" onClick={onBack}>返回资产列表</Button>
          <Input size="sm" value={host} onChange={(e) => setHost(e.target.value)} placeholder="过滤主机（可选）" className="min-w-[180px] flex-1" />
          <Button size="sm" variant="secondary" icon="refresh" disabled={busy} onClick={() => { void load(); }}>刷新</Button>
        </div>
        <SectionHeader title="REST 路由折叠簇" count={groups.length} />
        <SurfaceCard>
          {groups.length ? groups.map((g, idx) => (
            <SurfaceRow
              key={`${String(g.method)}:${String(g.host)}:${String(g.folded_path)}:${idx}`}
              icon="globe"
              title={`${String(g.method)} ${String(g.folded_path)}`}
              subtitle={`${String(g.host)} · 样本：${(g.raw_sample_paths as string[] | undefined)?.slice(0, 3).join(", ") || "—"}`}
              trailing={(
                <span className="flex items-center gap-1.5">
                  {((g.evidence_tiers as string[]) ?? []).map((tier) => (
                    <StudioBadge key={tier} tone={tier === "sourcemap_verified" || tier === "openapi_contract" ? "success" : "neutral"}>{tier}</StudioBadge>
                  ))}
                  <StudioBadge tone="accent">{String(g.count ?? 0)}</StudioBadge>
                </span>
              )}
            />
          )) : <SurfaceRow icon="globe" title="暂无可折叠路由" />}
        </SurfaceCard>
      </div>
    </div>
  );
}

const INTEL_STATUS_META: Record<string, { label: string; tone: "neutral" | "accent" | "success" | "warning" }> = {
  no_observations: { label: "暂无组件版本观测", tone: "neutral" },
  no_advisories: { label: "未导入安全公告", tone: "warning" },
  no_valid_semver: { label: "观测缺少有效 SemVer", tone: "warning" },
  matched_zero: { label: "已比对，无命中", tone: "success" },
  matched: { label: "存在命中预警", tone: "warning" },
};

function VulnIntelPanel({ taskId, onBack }: { taskId: string; onBack: () => void }) {
  const [data, setData] = useState<{
    advisories: Array<Record<string, unknown>>;
    matches: Array<Record<string, unknown>>;
    observations: Array<Record<string, unknown>>;
    statusDoc: {
      observation_count: number;
      valid_semver_count: number;
      advisory_count: number;
      matched_count: number;
      status: string;
      last_run: Record<string, unknown> | null;
    } | null;
  }>({ advisories: [], matches: [], observations: [], statusDoc: null });
  const [loadError, setLoadError] = useState("");
  const [importText, setImportText] = useState("");
  const [importOpen, setImportOpen] = useState(false);
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    if (!taskId) return;
    setLoadError("");
    try {
      const [res, statusRes] = await Promise.all([
        querySrc<{
          advisories?: Array<Record<string, unknown>>;
          matches?: Array<Record<string, unknown>>;
          observations?: Array<Record<string, unknown>>;
        }>(taskId, "src.intel.list", {}),
        querySrc<{
          observation_count?: number;
          valid_semver_count?: number;
          advisory_count?: number;
          matched_count?: number;
          status?: string;
          last_run?: Record<string, unknown> | null;
        }>(taskId, "src.intel.status", {}),
      ]);
      setData({
        advisories: res.advisories ?? [],
        matches: res.matches ?? [],
        observations: res.observations ?? [],
        statusDoc: {
          observation_count: Number(statusRes.observation_count ?? 0),
          valid_semver_count: Number(statusRes.valid_semver_count ?? 0),
          advisory_count: Number(statusRes.advisory_count ?? 0),
          matched_count: Number(statusRes.matched_count ?? 0),
          status: String(statusRes.status || "no_observations"),
          last_run: statusRes.last_run ?? null,
        },
      });
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      setLoadError(msg);
      toast({ title: "加载漏洞情报失败", description: msg, tone: "danger" });
    }
  }, [taskId]);

  useEffect(() => {
    void load();
  }, [load]);

  const runMatch = async () => {
    if (!taskId) return;
    setBusy(true);
    try {
      const res = await postCommand<{ match_count?: number }>(taskId, "src.intel.match_run", { emit_suggestions: true });
      await afterDomainMutation();
      await load();
      toast({ title: `SemVer 匹配完成：命中 ${res.match_count ?? 0} 条`, tone: "success" });
    } catch (err) {
      toast({ title: "SemVer 匹配失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
    } finally {
      setBusy(false);
    }
  };

  const importAdvisories = async () => {
    if (!taskId || !importText.trim()) return;
    setBusy(true);
    try {
      const parsed = JSON.parse(importText);
      const advisories = Array.isArray(parsed) ? parsed : (parsed.advisories ?? [parsed]);
      await postCommand(taskId, "src.intel.advisory.import", { advisories });
      setImportOpen(false);
      setImportText("");
      await load();
      toast({ title: "已导入 SemVer 公告", tone: "success" });
    } catch (err) {
      toast({ title: "导入公告失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
    } finally {
      setBusy(false);
    }
  };

  const statusMeta = data.statusDoc ? (INTEL_STATUS_META[data.statusDoc.status] ?? { label: data.statusDoc.status, tone: "neutral" as const }) : null;

  return (
    <div className="cx-scroll min-h-0 flex-1 overflow-y-auto p-4" data-testid="studio-vuln-intel-panel">
      <div className="mx-auto flex max-w-[820px] flex-col gap-4">
        <div className="flex flex-wrap items-center gap-2">
          <Button size="sm" variant="ghost" icon="arrowLeft" onClick={onBack}>返回资产列表</Button>
          <Button size="sm" variant="secondary" icon="plus" onClick={() => setImportOpen(true)}>导入公告 JSON</Button>
          <Button size="sm" variant="primary" icon="play" disabled={busy} onClick={() => { void runMatch(); }}>执行 SemVer 匹配</Button>
          <Button size="sm" variant="ghost" icon="refresh" disabled={busy} onClick={() => { void load(); }}>刷新</Button>
        </div>

        {loadError ? (
          <Callout
            tone="danger"
            title="读取漏洞情报状态失败"
            role="alert"
            action={<Button size="sm" variant="secondary" icon="retry" onClick={() => { void load(); }}>重试</Button>}
          >
            {loadError}
          </Callout>
        ) : null}

        {data.statusDoc ? (
          <SurfaceCard className="p-3" data-testid="intel-status-summary">
            <div className="flex flex-wrap items-center justify-between gap-2">
              <div className="flex flex-wrap items-center gap-2">
                {statusMeta ? <StudioBadge tone={statusMeta.tone}>{statusMeta.label}</StudioBadge> : null}
                <span className="text-cx-xs text-cx-fg-2">组件观测 {data.statusDoc.observation_count} 条</span>
                <span className="text-cx-xs text-cx-fg-2">有效 SemVer {data.statusDoc.valid_semver_count} 条</span>
                <span className="text-cx-xs text-cx-fg-2">安全公告 {data.statusDoc.advisory_count} 条</span>
                <span className="text-cx-xs font-medium text-cx-fg">命中 {data.statusDoc.matched_count} 条</span>
              </div>
              {data.statusDoc.last_run ? (
                <span className="text-cx-xs text-cx-fg-3">
                  最近比对：{String(data.statusDoc.last_run.ran_at || data.statusDoc.last_run.at || data.statusDoc.last_run.status || "已完成")}
                </span>
              ) : (
                <span className="text-cx-xs text-cx-fg-4">尚未运行匹配</span>
              )}
            </div>
          </SurfaceCard>
        ) : null}

        <div>
          <SectionHeader title="SemVer 命中结果" count={data.matches.length} />
          <SurfaceCard>
            {data.matches.length ? data.matches.map((m) => (
              <SurfaceRow
                key={String(m.match_id)}
                icon="shield"
                title={`${String(m.package_name)}@${String(m.observed_semver)} → ${String(m.advisory_id)}`}
                subtitle={`${String(m.title)} · 区间 ${String(m.matched_range)}${m.fix_version ? ` · 修复 >= ${String(m.fix_version)}` : ""}`}
                trailing={<StudioBadge tone={m.severity === "critical" || m.severity === "high" ? "danger" : "warning"}>{String(m.severity || "medium")}</StudioBadge>}
              />
            )) : <SurfaceRow icon="shield" title="暂无命中" />}
          </SurfaceCard>
        </div>

        <div>
          <SectionHeader title="观测组件版本（来自 SourceMap / package.json）" count={data.observations.length} />
          <SurfaceCard>
            {data.observations.length ? data.observations.map((o) => (
              <SurfaceRow
                key={String(o.observation_id)}
                icon="book"
                title={`${String(o.package_name)} ${String(o.version)}`}
                subtitle={`${String(o.ecosystem)} · ${String(o.source_ref || "")}`}
              />
            )) : <SurfaceRow icon="book" title="暂无组件观测" />}
          </SurfaceCard>
        </div>

        <Dialog
          open={importOpen}
          onOpenChange={setImportOpen}
          title="导入 SemVer 安全公告 (JSON)"
          size="md"
          footer={(
            <>
              <Button variant="ghost" onClick={() => setImportOpen(false)}>取消</Button>
              <Button variant="primary" disabled={busy || !importText.trim()} onClick={() => { void importAdvisories(); }}>导入</Button>
            </>
          )}
        >
          <TextArea
            rows={8}
            value={importText}
            onChange={(e) => setImportText(e.target.value)}
            placeholder={'[{"advisory_id":"GHSA-demo-1","ecosystem":"npm","package_name":"lodash","affected_ranges":["<4.17.21"],"severity":"high","title":"Prototype Pollution","fix_version":"4.17.21"}]'}
            className="font-mono text-cx-xs"
          />
        </Dialog>
      </div>
    </div>
  );
}

function IdFlowGraphPanel({ onBack }: { onBack: () => void }) {
  const state = useStudioState();
  const taskId = attachedTaskId();
  const [bizEdges, setBizEdges] = useState<Array<Record<string, unknown>>>([]);
  const [busy, setBusy] = useState(false);

  const loadBizGraph = useCallback(async () => {
    if (!taskId) return;
    try {
      const [ctxRes, modelRes] = await Promise.all([
        querySrc<{ records?: Array<Record<string, unknown>> }>(taskId, "src.record.list", { kind: "business_context", limit: 10 }),
        querySrc<{ records?: Array<Record<string, unknown>> }>(taskId, "src.record.list", { kind: "business_model", limit: 10 }),
      ]);
      const allRecs = [...(ctxRes.records ?? []), ...(modelRes.records ?? [])];
      const edges: Array<Record<string, unknown>> = [];
      for (const row of allRecs) {
        const g = (row.payload as Record<string, unknown> | undefined)?.idFlowGraph;
        if (Array.isArray(g)) {
          for (const e of g) {
            if (e && typeof e === "object") edges.push(e as Record<string, unknown>);
          }
        }
      }
      setBizEdges(edges);
    } catch {
    }
  }, [taskId]);

  useEffect(() => {
    void loadBizGraph();
  }, [loadBizGraph]);

  const deriveGraph = async () => {
    if (!taskId) return;
    setBusy(true);
    try {
      await postCommand(taskId, "src.research.business.derive", {});
      await loadBizGraph();
      toast({ title: "已推导业务对象关系流", tone: "success" });
    } catch (err) {
      toast({ title: "推导失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
    } finally {
      setBusy(false);
    }
  };

  const { nodes, edges } = useMemo(() => {
    const nodeStyle = {
      background: "var(--cx-elevated)",
      color: "var(--cx-fg)",
      border: "1px solid var(--cx-border)",
      borderRadius: 8,
      fontSize: 12,
      padding: "6px 10px",
    } as const;
    const outNodes: FlowNode[] = [];
    const outEdges: Edge[] = [];
    const nodePos = new Map<string, number>();

    bizEdges.slice(0, 32).forEach((edge, idx) => {
      const from = String(edge.from || edge.source || edge.subject || `node_${idx}_a`);
      const to = String(edge.to || edge.target || edge.object || `node_${idx}_b`);
      const label = String(edge.action || edge.relation || edge.edge || "");
      if (!nodePos.has(from)) {
        const i = nodePos.size;
        nodePos.set(from, i);
        outNodes.push({ id: from, position: { x: 40 + (i % 2) * 260, y: Math.floor(i / 2) * 82 }, data: { label: from }, style: nodeStyle });
      }
      if (!nodePos.has(to)) {
        const i = nodePos.size;
        nodePos.set(to, i);
        outNodes.push({ id: to, position: { x: 40 + (i % 2) * 260, y: Math.floor(i / 2) * 82 }, data: { label: to }, style: nodeStyle });
      }
      outEdges.push({ id: `biz:${idx}:${from}->${to}`, source: from, target: to, label });
    });

    const seenSources = new Map<string, string>();
    const baseY = outNodes.length ? (Math.ceil(outNodes.length / 2) + 1) * 82 : 0;
    const clues = state.clues.slice(0, 24);
    clues.forEach((clue, idx) => {
      const cid = `clue:${clue.id}`;
      outNodes.push({
        id: cid,
        position: { x: 420, y: baseY + idx * 84 },
        data: { label: `${clue.title || clue.id} (${clue.readiness})` },
        style: nodeStyle,
      });
      const sourceIds = Array.from(
        new Set([clue.sourceId, ...(clue.affectedSourceIds ?? [])].filter((id): id is string => Boolean(id))),
      );
      sourceIds.forEach((sid, sIdx) => {
        const nid = `src:${sid}`;
        if (!seenSources.has(nid)) {
          seenSources.set(nid, sid);
          const src = state.sources.find((s) => s.id === sid);
          outNodes.push({
            id: nid,
            position: { x: 40, y: baseY + (seenSources.size - 1) * 78 },
            data: { label: src?.title ?? sid },
            style: nodeStyle,
          });
        }
        outEdges.push({
          id: `e:${nid}->${cid}:${sIdx}`,
          source: nid,
          target: cid,
        });
      });
    });
    if (!outNodes.length) {
      state.sources.slice(0, 12).forEach((s, i) => {
        outNodes.push({
          id: `src:${s.id}`,
          position: { x: (i % 3) * 260, y: Math.floor(i / 3) * 86 },
          data: { label: s.title || s.id },
          style: nodeStyle,
        });
      });
    }
    return { nodes: outNodes, edges: outEdges };
  }, [bizEdges, state.clues, state.sources]);

  return (
    <div className="flex min-h-0 flex-1 flex-col p-4" data-testid="studio-id-flow-graph">
      <div className="mb-3">
        <SectionHeader
          title="对象与证据关联图"
          count={nodes.length}
          action={(
            <div className="flex flex-wrap items-center gap-2">
              <Button size="sm" variant="ghost" icon="arrowLeft" onClick={onBack}>返回资产列表</Button>
              {taskId ? <Button size="sm" variant="secondary" disabled={busy} onClick={() => { void deriveGraph(); }}>推导对象流</Button> : null}
            </div>
          )}
        />
      </div>
      <div className="min-h-[380px] flex-1 overflow-hidden rounded-xl border border-cx-border bg-cx-elevated">
        <ReactFlow nodes={nodes} edges={edges} fitView>
          <Background />
          <Controls />
        </ReactFlow>
      </div>
    </div>
  );
}

function GraphqlGrpcPanel({ taskId, onBack }: { taskId: string; onBack: () => void }) {
  const [matrix, setMatrix] = useState<{
    contexts: string[];
    matrix: Array<Record<string, unknown>>;
    bfla_candidate_count: number;
  }>({ contexts: [], matrix: [], bfla_candidate_count: 0 });
  const [busy, setBusy] = useState(false);
  const [ingestOpen, setIngestOpen] = useState(false);
  const [proto, setProto] = useState<"graphql" | "grpc">("graphql");
  const [endpointUrl, setEndpointUrl] = useState("");
  const [schemaInput, setSchemaInput] = useState("");

  const load = useCallback(async () => {
    if (!taskId) return;
    setBusy(true);
    try {
      const res = await querySrc<{
        contexts?: string[];
        matrix?: Array<Record<string, unknown>>;
        bfla_candidate_count?: number;
      }>(taskId, "src.research.graphql_grpc.auth_matrix", {});
      setMatrix({
        contexts: res.contexts ?? [],
        matrix: res.matrix ?? [],
        bfla_candidate_count: res.bfla_candidate_count ?? 0,
      });
    } catch (err) {
      toast({ title: "加载 GraphQL/gRPC 矩阵失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
    } finally {
      setBusy(false);
    }
  }, [taskId]);

  useEffect(() => {
    void load();
  }, [load]);

  const submitIngest = async () => {
    if (!taskId || !schemaInput.trim()) return;
    setBusy(true);
    try {
      await postCommand(taskId, "src.research.graphql_grpc.ingest", {
        protocol: proto,
        ...(endpointUrl.trim() ? { endpoint_url: endpointUrl.trim() } : {}),
        ...(proto === "grpc" ? { proto_text: schemaInput } : { schema_text: schemaInput }),
      });
      setIngestOpen(false);
      setSchemaInput("");
      await load();
      toast({ title: `已解析 ${proto.toUpperCase()} 契约`, tone: "success" });
    } catch (err) {
      toast({ title: "导入失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="cx-scroll min-h-0 flex-1 overflow-y-auto p-4" data-testid="studio-graphql-grpc-panel">
      <div className="mx-auto flex max-w-[880px] flex-col gap-3">
        <div className="flex flex-wrap items-center gap-2">
          <Button size="sm" variant="ghost" icon="arrowLeft" onClick={onBack}>返回资产列表</Button>
          <Button size="sm" variant="primary" disabled={!taskId || busy} onClick={() => setIngestOpen(true)}>导入 Schema / Proto</Button>
          <Button size="sm" variant="secondary" icon="refresh" disabled={!taskId || busy} onClick={() => { void load(); }}>刷新</Button>
          {matrix.bfla_candidate_count > 0 ? <StudioBadge tone="warning">{`BFLA 候选 ${matrix.bfla_candidate_count}`}</StudioBadge> : null}
        </div>
        <SectionHeader title="GraphQL / gRPC 权限对比矩阵" count={matrix.matrix.length} />
        <SurfaceCard>
          {matrix.matrix.length ? (
            <div className="overflow-x-auto">
              <table className="w-full border-collapse text-left text-cx-xs">
                <thead>
                  <tr className="border-b border-cx-border-subtle text-cx-fg-3">
                    <th className="px-3 py-2 font-medium">协议</th>
                    <th className="px-3 py-2 font-medium">类型</th>
                    <th className="px-3 py-2 font-medium">操作</th>
                    {matrix.contexts.map((ctx) => (
                      <th key={ctx} className="px-3 py-2 font-medium">{ctx}</th>
                    ))}
                    <th className="px-3 py-2 font-medium">状态</th>
                  </tr>
                </thead>
                <tbody>
                  {matrix.matrix.map((row, idx) => {
                    const cells = (row.cells as Record<string, Record<string, unknown>>) ?? {};
                    return (
                      <tr key={String(row.op_id || idx)} className="border-b border-cx-border-subtle last:border-0">
                        <td className="px-3 py-2 font-cx-mono">{String(row.protocol || "")}</td>
                        <td className="px-3 py-2 font-cx-mono">{String(row.kind || "")}</td>
                        <td className="px-3 py-2 font-cx-mono text-cx-fg">{String(row.operation || "")}</td>
                        {matrix.contexts.map((ctx) => {
                          const c = cells[ctx];
                          const access = String(c?.access || "untested");
                          return (
                            <td key={ctx} className="px-3 py-2">
                              <StudioBadge tone={access === "allowed_2xx" ? "accent" : access === "denied_auth" ? "success" : "neutral"}>
                                {c?.status_code ? `${access} (${String(c.status_code)})` : access}
                              </StudioBadge>
                            </td>
                          );
                        })}
                        <td className="px-3 py-2">
                          {row.bfla_candidate ? <StudioBadge tone="danger">BFLA 候选</StudioBadge> : <StudioBadge tone="neutral">正常</StudioBadge>}
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
          ) : <SurfaceRow icon="code" title="暂无 GraphQL / gRPC 操作契约" />}
        </SurfaceCard>
        <Dialog
          open={ingestOpen}
          onOpenChange={setIngestOpen}
          title="导入 GraphQL Introspection / .proto"
          size="lg"
          footer={<><Button variant="ghost" onClick={() => setIngestOpen(false)}>取消</Button><Button variant="primary" disabled={busy || !schemaInput.trim()} onClick={() => { void submitIngest(); }}>解析入库</Button></>}
        >
          <div className="flex flex-col gap-3">
            <div className="flex flex-col">
              <Label>协议</Label>
              <Select<"graphql" | "grpc"> value={proto} onChange={setProto} options={[{ value: "graphql", label: "GraphQL Introspection JSON" }, { value: "grpc", label: "gRPC .proto" }]} ariaLabel="协议类型" />
            </div>
            <TextField label="服务端点 URL (可选)" value={endpointUrl} onChange={(e) => setEndpointUrl(e.target.value)} placeholder="https://api.example.com/graphql" />
            <TextArea rows={8} value={schemaInput} onChange={(e) => setSchemaInput(e.target.value)} placeholder={proto === "grpc" ? "service OrderService { rpc CancelOrder(Req) returns (Resp); }" : '{"__schema": ...}'} className="font-cx-mono text-cx-xs" />
          </div>
        </Dialog>
      </div>
    </div>
  );
}

const KIND_ICON: Record<SourceKind, IconName> = {
  interview: "messageCircle",
  field: "pencilLine",
  archive: "archive",
  minutes: "rows",
};

const FINDINGS_PRESENCE: Array<{ value: FindingsPresence; label: string }> = [
  { value: "all", label: "不限" },
  { value: "with", label: "有发现" },
  { value: "open", label: "有待核实的发现" },
  { value: "without", label: "还没有发现" },
];

const TIME_RANGES: Array<{ value: FilterTimeRange; label: string }> = [
  { value: "all", label: "不限时间" },
  { value: "24h", label: "最近观察（10月27日）" },
  { value: "7d", label: "近 7 天观察" },
  { value: "30d", label: "早期基线" },
  { value: "stale", label: "待刷新" },
];

interface ClueCounts {
  total: number;
  open: number;
}

function useClueCounts() {
  const state = useStudioState();
  return useMemo(() => {
    const counts = new Map<string, ClueCounts>();
    state.clues.forEach((clue) => {
      if (mergeOf(state.merges, "clue", clue.id)) return;
      const assets = clue.affectedSourceIds?.length ? clue.affectedSourceIds : [clue.sourceId];
      assets.forEach((assetId) => {
        const entry = counts.get(assetId) ?? { total: 0, open: 0 };
        entry.total += 1;
        if (clue.status !== "verified") entry.open += 1;
        counts.set(assetId, entry);
      });
    });
    return counts;
  }, [state.clues, state.merges]);
}

function sourceMatches(
  source: StudioSource,
  filter: SourceFilter,
  counts: Map<string, ClueCounts>,
  merges: MergeRecord[],
  origins: Record<string, ImportOrigin>,
  withReportAssets: Set<string>,
  runAssets: Map<string, Set<string>>,
  activeProjectId: string,
): boolean {
  const lifecycle = filter.lifecycle ?? "active";
  const isUnlinked = Boolean(source.unlinkedTaskIds?.includes(activeProjectId));
  if (lifecycle === "active" && (source.archived || isUnlinked)) return false;
  if (lifecycle === "archived" && !source.archived) return false;
  if (lifecycle === "unlinked" && !isUnlinked) return false;

  if (!filter.showMerged && mergeOf(merges, "source", source.id)) return false;
  if (filter.kind !== "all" && source.kind !== filter.kind) return false;
  if (filter.assetType && filter.assetType !== "all" && source.assetType !== filter.assetType) return false;
  if (filter.status !== "all" && source.status !== filter.status) return false;
  if (filter.importance && filter.importance !== "all" && (source.importance ?? "normal") !== filter.importance) return false;
  if (filter.tag && !(source.tags ?? []).includes(filter.tag)) return false;

  const clues = counts.get(source.id);
  if (filter.findings === "with" && !clues?.total) return false;
  if (filter.findings === "open" && !clues?.open) return false;
  if (filter.findings === "without" && clues?.total) return false;

  if (filter.report === "with" && !withReportAssets.has(source.id)) return false;
  if (filter.report === "without" && withReportAssets.has(source.id)) return false;
  if (filter.runId && !runAssets.get(filter.runId)?.has(source.id)) return false;

  if (filter.conflict === "with" && !(source.conflicts?.length)) return false;
  if (filter.conflict === "unresolved" && !(source.conflicts?.some((c) => !c.resolvedBy || c.resolvedBy === "rule"))) return false;

  if (filter.timeRange && filter.timeRange !== "all") {
    const ts = Date.parse(source.lastObservedAt || source.firstSeenAt || "");
    const DAY = 24 * 3600 * 1000;
    if (filter.timeRange === "24h" && (!ts || Date.now() - ts > DAY)) return false;
    if (filter.timeRange === "7d" && (!ts || Date.now() - ts > 7 * DAY)) return false;
    if (filter.timeRange === "30d" && (!ts || Date.now() - ts > 30 * DAY)) return false;
    if (filter.timeRange === "stale" && !source.lastFailedAt) return false;
  }

  if (filter.origin !== "all") {
    const origin = origins[source.id];
    if (filter.origin === "manual" ? origin : filter.origin === "imported" ? !origin : origin?.batchId !== filter.origin) return false;
  }
  const q = filter.query.trim().toLowerCase();
  if (!q) return true;
  return source.title.toLowerCase().includes(q)
    || source.author.toLowerCase().includes(q)
    || (source.canonicalKey ?? "").toLowerCase().includes(q)
    || (source.aliases ?? []).some((a) => a.toLowerCase().includes(q))
    || (source.tags ?? []).some((t) => t.toLowerCase().includes(q))
    || source.passages.some((passage) => passage.text.toLowerCase().includes(q));
}

function originLabel(state: StudioState, origin: string): string {
  if (origin === "manual") return "手动录入";
  if (origin === "imported") return "文件导入";
  const batch = state.imports.find((item) => item.id === origin);
  return batch ? `导入：${batch.fileName}` : "导入批次已不存在";
}

function sourceChips(state: StudioState, filter: SourceFilter): FilterChip[] {
  const set = (patch: Partial<SourceFilter>) => () => studio.setLibraryUi(patch);
  const chips: FilterChip[] = [];
  if (filter.kind !== "all") chips.push({ key: "kind", label: `类型：${sourceKindLabel(filter.kind)}`, onRemove: set({ kind: "all" }) });
  if (filter.assetType && filter.assetType !== "all") chips.push({ key: "assetType", label: `细类：${assetTypeLabel(filter.assetType)}`, onRemove: set({ assetType: "all" }) });
  if (filter.status !== "all") chips.push({ key: "status", label: sourceStatusLabel(filter.status), onRemove: set({ status: "all" }) });
  if (filter.importance && filter.importance !== "all") chips.push({ key: "importance", label: `重要性：${importanceLabel(filter.importance)}`, onRemove: set({ importance: "all" }) });
  if (filter.timeRange && filter.timeRange !== "all") {
    chips.push({ key: "timeRange", label: TIME_RANGES.find((t) => t.value === filter.timeRange)?.label ?? filter.timeRange, onRemove: set({ timeRange: "all" }) });
  }
  if (filter.findings !== "all") {
    chips.push({ key: "findings", label: FINDINGS_PRESENCE.find((item) => item.value === filter.findings)?.label ?? "", onRemove: set({ findings: "all" }) });
  }
  if (filter.report && filter.report !== "all") chips.push({ key: "report", label: filter.report === "with" ? "已关联报告" : "未关联报告", onRemove: set({ report: "all" }) });
  if (filter.runId) chips.push({ key: "runId", label: `工作记录：${filter.runId}`, onRemove: set({ runId: "" }) });
  if (filter.tag) chips.push({ key: "tag", label: `标签：${filter.tag}`, onRemove: set({ tag: "" }) });
  if (filter.conflict && filter.conflict !== "all") chips.push({ key: "conflict", label: filter.conflict === "with" ? "有字段冲突" : "待裁决冲突", onRemove: set({ conflict: "all" }) });
  if (filter.lifecycle && filter.lifecycle !== "active") {
    const lbl = filter.lifecycle === "archived" ? "已归档资产" : filter.lifecycle === "unlinked" ? "已解除任务关联" : "含已归档/解除关联";
    chips.push({ key: "lifecycle", label: lbl, onRemove: set({ lifecycle: "active" }) });
  }
  if (filter.origin !== "all") chips.push({ key: "origin", label: originLabel(state, filter.origin), onRemove: set({ origin: "all" }) });
  if (filter.showMerged) chips.push({ key: "merged", label: "含已归并的", onRemove: set({ showMerged: false }) });
  return chips;
}

function readerTarget(ui: LibraryUi, source: StudioSource): StudioRef {
  if (ui.clueId) return { kind: "clue", id: ui.clueId };
  if (ui.passageId && source.passages.some((passage) => passage.id === ui.passageId)) return { kind: "passage", id: ui.passageId };
  return { kind: "source", id: source.id };
}

function openSource(sourceId: string) {
  studio.setLibraryUi({ sourceId, passageId: "", clueId: "" });
}

function closeReader() {
  studio.setLibraryUi({ sourceId: "", passageId: "", clueId: "" });
}

export function assetTitle(source: StudioSource): string {
  return displayTitle(source.canonicalKey ?? source.title);
}

function CollectSourceDialog({ open, onOpenChange, projectId }: { open: boolean; onOpenChange: (open: boolean) => void; projectId: string }) {
  const state = useStudioState();
  const [title, setTitle] = useState("");
  const [canonicalKey, setCanonicalKey] = useState("");
  const [aliasesText, setAliasesText] = useState("");
  const [importance, setImportance] = useState<AssetImportance>("normal");
  const [assetType, setAssetType] = useState<SrcAssetType>(SRC_ASSET_TYPES[1]?.value ?? "domain");
  const [text, setText] = useState("");
  const [target, setTarget] = useState(projectId);
  useEffect(() => {
    if (!open) return;
    setTitle("");
    setCanonicalKey("");
    setAliasesText("");
    setImportance("normal");
    setAssetType(SRC_ASSET_TYPES[1]?.value ?? "domain");
    setText("");
    setTarget(projectId);
  }, [open, projectId]);
  const derivedKind: SourceKind = SRC_ASSET_TYPES.find((item) => item.value === assetType)?.kind ?? "interview";
  const canSave = Boolean(title.trim() && text.trim() && target);
  const save = () => {
    if (!canSave) return;
    const aliases = aliasesText.split(/[,，\s]+/).map((s) => s.trim()).filter(Boolean);
    const id = studio.addSource({
      projectId: target,
      title: title.trim(),
      kind: derivedKind,
      assetType,
      text,
      author: "我",
      canonicalKey: canonicalKey.trim() || title.trim(),
      aliases,
      importance,
    });
    onOpenChange(false);
    openSource(id);
    toast({
      title: "已录入资产库",
      description: "已记入处理记录（支持一键撤销），并自动保存在当前工作台。",
      tone: "success",
      action: {
        label: "撤销",
        onClick: () => {
          const latest = readStudioState().history[0];
          if (latest) revertEntry(latest.id);
        },
      },
    });
  };
  return (
    <Dialog
      open={open}
      onOpenChange={onOpenChange}
      title="录入资产"
      description="登记资产规范标识、别名、重要性与观测正文，每一行会成为一个可引用的段落。"
      icon="book"
      size="md"
      footer={(
        <>
          <Button variant="ghost" onClick={() => onOpenChange(false)}>取消</Button>
          <Button variant="primary" disabled={!canSave} onClick={save}>加入资产库</Button>
        </>
      )}
    >
      <div className="flex flex-col gap-4">
        <TextField label="资产标题" value={title} onChange={(event) => setTitle(event.target.value)} placeholder="例如：A-1285 · gateway-north.example.invalid" autoFocus />
        <div className="grid grid-cols-2 gap-3 max-sm:grid-cols-1">
          <TextField label="规范标识 (Canonical Key)" value={canonicalKey} onChange={(e) => setCanonicalKey(e.target.value)} placeholder="留空时同标题" />
          <TextField label="别名（逗号分隔）" value={aliasesText} onChange={(e) => setAliasesText(e.target.value)} placeholder="例如：old-gw.example.invalid" />
        </div>
        <div className="grid grid-cols-3 gap-3 max-sm:grid-cols-1">
          <div className="flex flex-col">
            <Label>所属项目</Label>
            <Select value={target} onChange={setTarget} options={state.projects.map((project) => ({ value: project.id, label: displayTitle(project.title) }))} ariaLabel="所属项目" />
          </div>
          <div className="flex flex-col">
            <Label>资产类型</Label>
            <Select<SrcAssetType>
              value={assetType}
              onChange={setAssetType}
              options={SRC_ASSET_TYPES.map((item) => ({ value: item.value, label: item.label }))}
              ariaLabel="资产类型"
              searchable
            />
          </div>
          <div className="flex flex-col">
            <Label>重要性</Label>
            <Select<AssetImportance> value={importance} onChange={setImportance} options={ASSET_IMPORTANCE_LEVELS} ariaLabel="重要性" />
          </div>
        </div>
        <TextArea label="正文与观测说明" value={text} onChange={(event) => setText(event.target.value)} placeholder="每行一段。之后可以在原文里选中文字记为发现。" className="min-h-[140px]" />
      </div>
    </Dialog>
  );
}

function SourceBatchActions({ checked, matchingIds, filterText }: { checked: string[]; matchingIds: string[]; filterText: string }) {
  const state = useStudioState();
  const [mergeIds, setMergeIds] = useState<string[]>([]);
  const [mergeOpen, setMergeOpen] = useState(false);
  const [tagOpen, setTagOpen] = useState(false);
  const [linkOpen, setLinkOpen] = useState(false);
  const [exportOpen, setExportOpen] = useState(false);
  const byId = useMemo(() => new Map(state.sources.map((source) => [source.id, source])), [state.sources]);
  const changing = (differs: (source: StudioSource) => boolean) => checked.filter((id) => {
    const source = byId.get(id);
    return source ? differs(source) : false;
  }).length;
  const scope = batchScope("source", checked.length, filterText);
  const subject = `勾选的 ${countLabel("source", checked.length)}`;

  const onDeleteBatch = () => {
    const entryId = studio.batchDeleteRecords("source", checked, scope);
    reportChange(entryId, "没有可删除的资产。");
  };

  return (
    <>
      <Menu
        ariaLabel="移到"
        trigger={<Button size="md" variant="secondary" iconRight="chevronDown">移到</Button>}
      >
        <MenuLabel>{subject}移到</MenuLabel>
        {SOURCE_STATUSES.map((item) => {
          const count = changing((source) => source.status !== item.value);
          return (
            <MenuItem
              key={item.value}
              disabled={!count}
              hint={count ? `${count} 份会改` : "都已是"}
              onSelect={() => reportChange(studio.setSourceStatus(checked, item.value, scope), `勾选的资产都已经是「${item.label}」。`)}
            >
              {item.label}
            </MenuItem>
          );
        })}
      </Menu>
      <Menu
        ariaLabel="改类型"
        trigger={<Button size="md" variant="secondary" iconRight="chevronDown">改类型</Button>}
      >
        <MenuLabel>{subject}的类型改为</MenuLabel>
        {SOURCE_KINDS.map((item) => {
          const count = changing((source) => source.kind !== item.value);
          return (
            <MenuItem
              key={item.value}
              icon={KIND_ICON[item.value]}
              disabled={!count}
              hint={count ? `${count} 份会改` : "都已是"}
              onSelect={() => reportChange(studio.setSourceKind(checked, item.value, scope), `勾选的资产都已经是「${item.label}」。`)}
            >
              {item.label}
            </MenuItem>
          );
        })}
      </Menu>
      <Button size="md" variant="secondary" icon="hash" onClick={() => setTagOpen(true)}>
        标签…
      </Button>
      <Button size="md" variant="secondary" icon="link" onClick={() => setLinkOpen(true)}>
        关联…
      </Button>
      <Button size="md" variant="secondary" icon="download" onClick={() => setExportOpen(true)}>
        导出…
      </Button>
      <Button
        size="md"
        variant="secondary"
        icon="listCollapse"
        disabled={checked.length < 2}
        tooltip={checked.length < 2 ? "勾选两份或更多重复的资产后归并" : undefined}
        onClick={() => { setMergeIds(checked); setMergeOpen(true); }}
      >
        归并…
      </Button>
      <Button
        size="md"
        variant="ghost"
        icon="archive"
        onClick={() => reportChange(studio.toggleAssetArchive(checked, true, scope), "勾选资产都已归档。")}
      >
        归档
      </Button>
      <Button
        size="md"
        variant="ghost"
        icon="trash"
        className={"text-cx-danger hover:text-cx-danger"}
        onClick={onDeleteBatch}
      >
        删除
      </Button>
      <MergeDialog open={mergeOpen} onOpenChange={setMergeOpen} kind="source" ids={mergeIds} />
      <BatchTagDialog open={tagOpen} onOpenChange={setTagOpen} kind="source" ids={checked} scope={scope} />
      <BatchLinkDialog open={linkOpen} onOpenChange={setLinkOpen} kind="source" ids={checked} scope={scope} />
      <BatchExportDialog open={exportOpen} onOpenChange={setExportOpen} kind="source" checkedIds={checked} matchingIds={matchingIds} />
    </>
  );
}


function passageDomId(passageId: string) {
  return `studio-passage-${passageId}`;
}

interface TextSelection {
  text: string;
  passageId: string;
  top: number;
  left: number;
}

function usePassageSelection(root: HTMLElement | null, enabled: boolean) {
  const [selection, setSelection] = useState<TextSelection | null>(null);
  useEffect(() => {
    if (!enabled || !root) {
      setSelection(null);
      return;
    }
    let frame = 0;
    const read = () => {
      const current = window.getSelection();
      if (!current || current.isCollapsed || !current.rangeCount) { setSelection(null); return; }
      const range = current.getRangeAt(0);
      const host = (node: Node) => (node instanceof Element ? node : node.parentElement)?.closest<HTMLElement>("[data-passage-text]") ?? null;
      const start = host(range.startContainer);
      if (!start || start !== host(range.endContainer) || !root.contains(start)) { setSelection(null); return; }
      const text = current.toString().trim();
      const passageId = start.dataset.passageText ?? "";
      if (!text || !passageId) { setSelection(null); return; }
      const rect = range.getBoundingClientRect();
      const base = root.getBoundingClientRect();
      const above = rect.top - base.top - 52;
      setSelection({
        text,
        passageId,
        top: above >= 0 ? above : rect.bottom - base.top + 8,
        left: Math.min(Math.max(rect.left - base.left + rect.width / 2, 170), Math.max(170, base.width - 170)),
      });
    };
    const onChange = () => {
      window.cancelAnimationFrame(frame);
      frame = window.requestAnimationFrame(read);
    };
    document.addEventListener("selectionchange", onChange);
    return () => {
      window.cancelAnimationFrame(frame);
      document.removeEventListener("selectionchange", onChange);
    };
  }, [enabled, root]);
  return selection;
}

function clearTextSelection() {
  window.getSelection()?.removeAllRanges();
}

function recordFinding(source: StudioSource, passage: StudioPassage, quote: string) {
  const id = studio.addClue({ projectId: source.projectId, sourceId: source.id, passageId: passage.id, quote, category: "detail" });
  studio.setLibraryUi({ passageId: passage.id, clueId: id });
  toast({
    title: "已记为发现",
    description: `归为「${categoryLabel("detail")}」，可以随时改或撤销。`,
    tone: "success",
    action: { label: "查看", onClick: () => openRef({ kind: "clue", id }) },
  });
}

function SelectionToolbar({ selection, source }: { selection: TextSelection; source: StudioSource }) {
  const passage = source.passages.find((item) => item.id === selection.passageId);
  if (!passage) return null;
  const owner: StudioRef = { kind: "passage", id: passage.id };
  const run = (action: () => void) => () => { action(); clearTextSelection(); };
  const style: CSSProperties = { top: selection.top, left: selection.left };
  return (
    <div
      role="toolbar"
      aria-label="选中文字的操作"
      style={style}
      onMouseDown={(event) => event.preventDefault()}
      onKeyDown={(event: KeyboardEvent<HTMLDivElement>) => { if (event.key === "Escape") clearTextSelection(); }}
      className="absolute z-(--z-sticky) flex -translate-x-1/2 items-center gap-1 rounded-2xl ring-1 ring-cx-ring bg-cx-elevated p-1 shadow-cx-md"
      data-testid="studio-selection-toolbar"
    >
      <Button
        size="md"
        variant="primary"
        icon="sparkles"
        onClick={run(() => {
          if (quoteToComposer(selection.text, owner, { silent: true })) {
            askAi(null);
          }
        })}
      >
        问 AI
      </Button>
      <Button size="md" variant="ghost" icon="quote" onClick={run(() => quoteToComposer(selection.text, owner))}>引用</Button>
      <Button size="md" variant="ghost" icon="bookmark" onClick={run(() => recordFinding(source, passage, selection.text))}>记为发现</Button>
    </div>
  );
}

function SourceActions({ source, showOpen = false }: { source: StudioSource; showOpen?: boolean }) {
  const state = useStudioState();
  const projectId = state.workspace.projectId === "all" ? source.projectId : state.workspace.projectId;
  const isUnlinked = Boolean(source.unlinkedTaskIds?.includes(projectId));
  const version = source.versions?.[0]?.version ?? source.pinnedVersion;
  return (
    <>
      {showOpen ? <><MenuItem icon="book" onSelect={() => openSource(source.id)}>打开资产</MenuItem><MenuSeparator /></> : null}
      <MenuItem icon="pin" onSelect={() => citeRef({ kind: "source", id: source.id })}>固定到对话</MenuItem>
      <MenuItem icon="archive" onSelect={() => {
        void (async () => {
          studio.toggleAssetArchive(source.id, !source.archived, source.archived ? "恢复归档资产" : "人工归档");
          const sync = await afterDomainMutation();
          if (!sync.ok) { toast({ title: "资产归档操作失败", description: sync.error, tone: "danger" }); return; }
          toast({ title: source.archived ? "已恢复资产" : "已归档资产", tone: "success" });
        })();
      }}>{source.archived ? "恢复资产" : "归档"}</MenuItem>
      <MenuItem icon="split" onSelect={() => {
        void (async () => {
          studio.toggleTaskUnlinkAsset(source.id, projectId);
          const sync = await afterDomainMutation();
          if (!sync.ok) { toast({ title: "任务关联更新失败", description: sync.error, tone: "danger" }); return; }
          toast({ title: isUnlinked ? "已重新关联到当前任务" : "已解除与本任务的关联", tone: "success" });
        })();
      }}>{isUnlinked ? "恢复本任务关联" : "解除本任务关联"}</MenuItem>
      <MenuItem icon="upload" disabled={version === undefined} description={version === undefined ? "暂无可固定的版本" : undefined} onSelect={() => {
        if (version === undefined) return;
        void (async () => {
          studio.publishOrPinAssetVersion(source.id, version);
          const sync = await afterDomainMutation();
          if (!sync.ok) { toast({ title: "固定版本失败", description: sync.error, tone: "danger" }); return; }
          toast({ title: `已发布固定版本 v${version}`, tone: "success" });
        })();
      }}>发布固定版本</MenuItem>
    </>
  );
}

const JUDGEMENT_LABELS: Record<NonNullable<StudioSource["judgementState"]>, { label: string; tone: "success" | "warning" | "neutral" | "accent" }> = {
  confirmed_in_scope: { label: "已确认范围内", tone: "success" },
  candidate: { label: "候选待核", tone: "warning" },
  out_of_scope: { label: "范围外", tone: "neutral" },
  needs_verify: { label: "待研判", tone: "accent" },
};

const MATERIAL_KIND_LABELS: Record<string, string> = {
  openapi_spec: "OpenAPI 规范",
  javascript_artifact: "JavaScript 产物",
  skill_material: "Skill 资料",
  authorization_snapshot: "授权快照",
  research_note: "研究笔记",
  research_document: "静态资料",
};

function SourceRow({
  source,
  counts,
  reportCount = 0,
  covered = false,
  checked,
  selecting,
  onToggle,
}: {
  source: StudioSource;
  counts?: ClueCounts;
  reportCount?: number;
  covered?: boolean;
  checked: boolean;
  selecting: boolean;
  onToggle: (checked: boolean, range: boolean) => void;
}) {
  const ctx = useContextMenu();
  const shiftRef = useRef(false);
  const actions = <SourceActions source={source} showOpen />;
  const target: StudioRef = { kind: "source", id: source.id };
  const unresolved = (source.conflicts ?? []).filter((conf) => !conf.resolvedBy || conf.resolvedBy === "rule").length;
  const obsCount = source.sourceHistory?.length ?? 0;
  const judgementMeta = source.judgementState ? JUDGEMENT_LABELS[source.judgementState] : null;
  const coverageLabel = source.lastFailedAt
    ? "核验失败"
    : source.lastVerifiedAt || covered
      ? "本期已覆盖"
      : "待覆盖";
  const coverageTone = source.lastFailedAt ? "danger" : source.lastVerifiedAt || covered ? "success" : "neutral";
  const lead = (
    <span className="relative grid size-8 shrink-0 place-items-center" onPointerDownCapture={(e) => { shiftRef.current = e.shiftKey; }}>
      <span className={cn("absolute inset-0 grid place-items-center rounded-lg bg-cx-hover text-cx-fg-3", (selecting || checked) && "hidden", "group-hover/row:hidden")}>
        <Icon name={KIND_ICON[source.kind]} size={15} />
      </span>
      <span className={cn("absolute inset-0 place-items-center", (selecting || checked) ? "grid" : "hidden group-hover/row:grid")}>
        <Checkbox
          checked={checked}
          aria-label={`勾选「${source.title}」`}
          onCheckedChange={(next) => onToggle(Boolean(next), shiftRef.current)}
          className="[&>button]:size-5"
        />
      </span>
    </span>
  );
  const signalLine = source.isMaterial ? (
    <span className="flex flex-wrap items-center gap-1.5 whitespace-normal text-cx-xs text-cx-fg-3">
      <StudioBadge tone="accent">{MATERIAL_KIND_LABELS[source.materialKind || ""] || source.materialKind || "项目资料"}</StudioBadge>
      {source.attributes?.status ? <span>状态：{source.attributes.status}</span> : null}
      <span>{source.lastObservedAt ? `更新 ${relativeTime(source.lastObservedAt) || source.lastObservedAt}` : source.collectedAt}</span>
    </span>
  ) : (
    <span className="flex flex-wrap items-center gap-1.5 whitespace-normal text-cx-xs text-cx-fg-3" data-testid={`asset-signals-${source.id}`}>
      <span>
        {assetTypeLabel(source.assetType, source.kind)} · 观测 {obsCount} 次
      </span>
      {source.activeOperationAuthorized === false ? (
        <StudioBadge tone="warning">待授权候选</StudioBadge>
      ) : null}
      {source.observationRole ? (
        <StudioBadge tone="neutral">{source.observationRole}</StudioBadge>
      ) : null}
      <StudioBadge tone={judgementMeta?.tone ?? "neutral"}>
        {source.systemName ? `${source.systemName} · ` : "未归属系统 · "}
        {judgementMeta?.label ?? "待定性"}
      </StudioBadge>
      <StudioBadge tone={coverageTone}>{coverageLabel}</StudioBadge>
      {unresolved > 0 || source.uncertaintyNote ? (
        <StudioBadge tone="warning">
          {unresolved > 0 ? `冲突 ${unresolved}` : ""}
          {unresolved > 0 && source.uncertaintyNote ? " · " : ""}
          {source.uncertaintyNote ? `待核：${source.uncertaintyNote}` : ""}
        </StudioBadge>
      ) : (
        <span>无冲突</span>
      )}
      <span>
        发现 {counts?.total ?? 0} · 报告 {reportCount}
      </span>
    </span>
  );
  return (
    <span className="group/row block" {...dragRef(target)} onContextMenu={ctx.onContextMenu} onKeyDown={ctx.onKeyDown}>
      <SurfaceRow
        icon={KIND_ICON[source.kind]}
        leading={lead}
        selected={checked}
        title={(
          <>
            {assetTitle(source)}
            <NewMark target={target} />
            <MergeMark kind="source" id={source.id} />
          </>
        )}
        subtitle={(
          <span className="flex flex-col gap-1 whitespace-normal">
            <span>
              {[
                source.canonicalKey && source.canonicalKey !== source.title ? source.canonicalKey : null,
                source.lastObservedAt ? `最近观察 ${relativeTime(source.lastObservedAt) || source.lastObservedAt}` : relativeTime(source.collectedAt) || source.collectedAt,
                source.archived ? "已归档" : null,
              ].filter(Boolean).join(" · ")}
            </span>
            {signalLine}
          </span>
        )}
        trailing={<span className="flex items-center gap-2"><Menu ariaLabel={`资产 ${source.title} 操作`} trigger={<IconButton size="sm" icon="more" label={`资产 ${source.title} 操作`} />}>{actions}</Menu></span>}
        onClick={() => openSource(source.id)}
      />
      {ctx.render(actions, `资产 ${source.title} 操作`)}
    </span>
  );
}

function SourceList({ active, onCollect, onImport }: { active: boolean; onCollect: () => void; onImport: () => void }) {
  const state = useStudioState();
  const counts = useClueCounts();
  const ui = state.workspace.library;
  const projectId = state.workspace.projectId;
  const scroll = useRememberedScroll("library-list", active);
  const [libraryTab, setLibraryTab] = useState<"assets" | "materials">("assets");
  const [exportOpen, setExportOpen] = useState(false);
  const [filterOpen, setFilterOpen] = useState(false);
  const [moreOpen, setMoreOpen] = useState(false);
  const [connectorOpen, setConnectorOpen] = useState(false);

  const filter = useMemo<SourceFilter>(() => pickSourceFilter(ui), [ui]);
  const isEmpty = sameFilter(filter, EMPTY_SOURCE_FILTER);
  const chips = sourceChips(state, filter);
  const describe = [filter.query.trim() ? `搜索「${filter.query.trim()}」` : "", ...chips.map((chip) => chip.label)].filter(Boolean).join("、");

  const { withReportAssets, reportCountsByAsset } = useMemo(() => {
    const ids = new Set<string>();
    const countsMap = new Map<string, number>();
    const clueById = new Map(state.clues.map((c) => [c.id, c]));
    state.reports.forEach((rep) => {
      const c = clueById.get(rep.findingId);
      if (!c) return;
      (c.affectedSourceIds ?? [c.sourceId]).forEach((id) => {
        ids.add(id);
        countsMap.set(id, (countsMap.get(id) ?? 0) + 1);
      });
    });
    return { withReportAssets: ids, reportCountsByAsset: countsMap };
  }, [state.clues, state.reports]);

  const runAssets = useMemo(() => {
    const map = new Map<string, Set<string>>();
    const allCovered = new Set<string>();
    state.workRuns.forEach((r) => {
      const set = new Set<string>();
      r.linkedRefs.forEach((ref) => {
        if (ref.kind === "source") {
          set.add(ref.id);
          allCovered.add(ref.id);
        }
      });
      map.set(r.id, set);
    });
    state.clues.forEach((c) => {
      if (!c.sourceRunId) return;
      const set = map.get(c.sourceRunId) ?? new Set<string>();
      (c.affectedSourceIds ?? [c.sourceId]).forEach((id) => {
        set.add(id);
        allCovered.add(id);
      });
      map.set(c.sourceRunId, set);
    });
    return { byRun: map, allCovered };
  }, [state.clues, state.workRuns]);

  const scopedAll = useMemo(
    () => state.sources.filter((source) => projectId === "all" || source.projectId === projectId),
    [projectId, state.sources],
  );
  const assetTotalCount = useMemo(() => scopedAll.filter((source) => !source.isMaterial).length, [scopedAll]);
  const materialTotalCount = useMemo(() => scopedAll.filter((source) => Boolean(source.isMaterial)).length, [scopedAll]);
  const scoped = useMemo(
    () => scopedAll.filter((source) => (libraryTab === "materials" ? Boolean(source.isMaterial) : !source.isMaterial)),
    [libraryTab, scopedAll],
  );
  const matching = useMemo(
    () => scoped.filter((source) => sourceMatches(source, filter, counts, state.merges, state.importOrigins, withReportAssets, runAssets.byRun, projectId)),
    [counts, filter, projectId, runAssets.byRun, scoped, state.importOrigins, state.merges, withReportAssets],
  );
  const ordered = useMemo(
    () => SOURCE_STATUSES.flatMap((group) => matching.filter((source) => source.status === group.value)),
    [matching],
  );
  const orderedIds = useMemo(() => ordered.map((source) => source.id), [ordered]);

  const setChecked = useCallback((ids: string[]) => studio.setLibraryUi({ checked: ids }), []);
  const checklist = useChecklist(ui.checked, orderedIds, setChecked);

  const duplicates = useMemo(
    () => (ui.view === "duplicates" || moreOpen
      ? findSourceDuplicates(state.sources, state.merges, state.dismissedDuplicates, state.undeterminedDuplicates).filter((group) => projectId === "all" || group.projectId === projectId)
      : []),
    [moreOpen, projectId, state.dismissedDuplicates, state.merges, state.sources, state.undeterminedDuplicates, ui.view],
  );
  const history = useMemo(() => scopedHistory(state.history, "source", state.sources, projectId), [projectId, state.history, state.sources]);

  const batches = state.imports.filter((batch) => projectId === "all" || batch.projectId === projectId);
  const originOptions = [
    { value: "all", label: "不限" },
    { value: "manual", label: "手动录入" },
    { value: "imported", label: "文件导入" },
    ...batches.map((batch) => ({ value: batch.id, label: batch.fileName, description: `导入于 ${formatTime(batch.importedAt)}` })),
  ];
  const tagOptions = useMemo(() => {
    if (!filterOpen) return [{ value: "", label: "不限" }];
    const tags = new Set<string>();
    scoped.forEach((s) => (s.tags ?? []).forEach((t) => tags.add(t)));
    if (filter.tag) tags.add(filter.tag);
    return [{ value: "", label: "不限" }, ...Array.from(tags).sort().map((t) => ({ value: t, label: t }))];
  }, [filter.tag, filterOpen, scoped]);
  const selecting = ui.checked.length > 0;

  return (
    <section aria-label="资产列表" className="flex min-h-0 flex-1 flex-col">
      <SurfaceToolbar>
        <SearchInput
          value={ui.query}
          onValueChange={(query) => studio.setLibraryUi({ query, view: "list" })}
          placeholder="搜索资产"
          aria-label="搜索资产"
          size="sm"
          className="min-w-0 flex-1"
        />
        <Menu
          open={filterOpen}
          onOpenChange={setFilterOpen}
          ariaLabel="筛选资产"
          trigger={(
            <span className="relative inline-flex">
              <IconButton size="sm" icon="filter" label="筛选" />
              {chips.length ? <span className="pointer-events-none absolute right-1 top-1 size-1.5 rounded-full bg-cx-accent" /> : null}
            </span>
          )}
        >
          {filterOpen ? (
            <>
              <MenuLabel>快速筛选</MenuLabel>
              <MenuItem icon="star" checked={filter.importance === "core"} onSelect={() => studio.setLibraryUi({ importance: filter.importance === "core" ? "all" : "core", view: "list" })}>核心</MenuItem>
              <MenuItem icon="gitCompare" checked={filter.conflict === "with" || filter.conflict === "unresolved"} onSelect={() => studio.setLibraryUi({ conflict: filter.conflict === "with" || filter.conflict === "unresolved" ? "all" : "unresolved", view: "list" })}>有冲突</MenuItem>
              <MenuItem icon="refresh" checked={filter.timeRange === "stale"} onSelect={() => studio.setLibraryUi({ timeRange: filter.timeRange === "stale" ? "all" : "stale", view: "list" })}>待刷新</MenuItem>
              <MenuItem icon="archive" checked={filter.lifecycle === "archived"} onSelect={() => studio.setLibraryUi({ lifecycle: filter.lifecycle === "archived" ? "active" : "archived", view: "list" })}>已归档</MenuItem>
              <MenuSeparator />
              <div className="flex flex-col gap-3 p-3">
                <FilterSelect<"all" | SourceKind> label="类型" value={filter.kind} onChange={(kind) => studio.setLibraryUi({ kind })} options={[{ value: "all", label: "不限" }, ...SOURCE_KINDS.map((item) => ({ value: item.value, label: item.label }))]} />
                <FilterSelect<"all" | SrcAssetType> label="具体资产类型" value={filter.assetType ?? "all"} onChange={(assetType) => studio.setLibraryUi({ assetType })} options={[{ value: "all", label: "不限" }, ...SRC_ASSET_TYPES.map((item) => ({ value: item.value, label: item.label }))]} searchable />
                <FilterSelect<"all" | StudioSource["status"]> label="整理状态" value={filter.status} onChange={(status) => studio.setLibraryUi({ status })} options={[{ value: "all", label: "不限" }, ...SOURCE_STATUSES]} />
                <FilterSelect<FilterTimeRange> label="观察时间" value={filter.timeRange ?? "all"} onChange={(timeRange) => studio.setLibraryUi({ timeRange })} options={TIME_RANGES} />
                <FilterSelect<FindingsPresence> label="关联发现" value={filter.findings} onChange={(findings) => studio.setLibraryUi({ findings })} options={FINDINGS_PRESENCE} />
                <FilterSelect<string> label="标签" value={filter.tag ?? ""} onChange={(tag) => studio.setLibraryUi({ tag })} options={tagOptions} searchable={tagOptions.length > 6} />
                <FilterSelect<string> label="来源" value={filter.origin} onChange={(origin) => studio.setLibraryUi({ origin })} options={originOptions} searchable={originOptions.length > 8} />
                <FilterField label="已归并的资产">
                  <Switch checked={filter.showMerged} onCheckedChange={(showMerged) => studio.setLibraryUi({ showMerged })} label="也列出已归并的" />
                </FilterField>
                {!isEmpty ? (
                  <Button size="sm" variant="ghost" className="self-start" onClick={() => studio.setLibraryUi({ ...EMPTY_SOURCE_FILTER })}>清除全部筛选</Button>
                ) : null}
              </div>
            </>
          ) : null}
        </Menu>
        <Menu
          ariaLabel="新建或导入"
          trigger={<IconButton size="sm" icon="plus" label="新建或导入" />}
        >
          <MenuItem icon="upload" onSelect={onImport}>导入文件</MenuItem>
          <MenuItem icon="plug" onSelect={() => setConnectorOpen(true)}>连接外部来源</MenuItem>
          <MenuItem icon="pencilLine" onSelect={onCollect}>新建资产</MenuItem>
        </Menu>
        <Menu
          open={moreOpen}
          onOpenChange={setMoreOpen}
          ariaLabel="更多"
          trigger={<IconButton size="sm" icon="more" label="更多" />}
        >
          <MenuItem icon="layers" onSelect={() => studio.setLibraryUi({ subView: "routes", view: "list" })}>REST 路由折叠</MenuItem>
          <MenuItem icon="shield" onSelect={() => studio.setLibraryUi({ subView: "intel", view: "list" })}>SemVer 漏洞情报</MenuItem>
          <MenuItem icon="code" onSelect={() => studio.setLibraryUi({ subView: "graphql", view: "list" })}>GraphQL / gRPC 矩阵</MenuItem>
          <MenuItem icon="gitBranch" onSelect={() => studio.setLibraryUi({ subView: "flow", view: "list" })}>对象关系图</MenuItem>
          <MenuSeparator />
          <MenuItem icon="copy" disabled={!duplicates.length} onSelect={() => studio.setLibraryUi({ view: "duplicates" })}>可能重复{duplicates.length ? ` ${duplicates.length}` : ""}</MenuItem>
          <MenuItem icon="history" onSelect={() => studio.setLibraryUi({ importsOpen: true })}>导入记录</MenuItem>
          <MenuItem icon="listChecks" onSelect={() => studio.setLibraryUi({ view: "history" })}>处理记录</MenuItem>
          <MenuSeparator />
          <MenuItem icon="download" onSelect={() => setExportOpen(true)}>导出</MenuItem>
        </Menu>
      </SurfaceToolbar>
      {state.domainErrors?.assets ? (
        <div className="mx-3 mt-2">
          <Callout
            tone="danger"
            title="加载资产域失败"
            role="alert"
            action={<Button size="sm" variant="secondary" icon="retry" onClick={() => { void studio.reloadDomain("assets"); }}>重试</Button>}
          >
            {state.domainErrors.assets}
          </Callout>
        </div>
      ) : null}
      {ui.view === "list" && !ui.subView?.match(/routes|intel|flow|graphql/) ? (
        <div className="flex flex-wrap items-center justify-between gap-2 border-b border-cx-border-subtle px-4 py-2">
          <SegmentedControl<"assets" | "materials">
            value={libraryTab}
            onChange={(next) => {
              setLibraryTab(next);
              studio.setLibraryUi({ checked: [] });
            }}
            ariaLabel="台账与资料切换"
            size="sm"
            options={[
              { value: "assets", label: `资产台账 (${assetTotalCount})` },
              { value: "materials", label: `项目资料 (${materialTotalCount})` },
            ]}
          />
          <span className="text-cx-xs text-cx-fg-4">
            {libraryTab === "materials" ? "OpenAPI 规范 / JS 产物 / Skill / 授权快照（不直接生成授权边界）" : "攻击面资产实体台账"}
          </span>
        </div>
      ) : null}
      {state.assetLoad && !state.assetLoad.complete ? (
        <p className="border-b border-cx-border-subtle px-4 py-1.5 text-cx-xs text-cx-fg-3" data-testid="asset-load-notice">
          仅加载了前 {state.assetLoad.loaded} 项 / 共 {state.assetLoad.total} 项（列表分页上限，其余资产仍在服务端）
        </p>
      ) : null}
      {ui.view === "list" && !ui.subView?.match(/routes|intel|flow|graphql/) && chips.length ? <FilterChips chips={chips} onClear={() => studio.setLibraryUi({ ...EMPTY_SOURCE_FILTER, query: ui.query })} /> : null}
      <ImportStatusRow />
      {ui.view === "duplicates" ? (
        <DuplicatesView kind="source" groups={duplicates} active={active} onOpen={openSource} />
      ) : ui.view === "history" ? (
        <HistoryView kind="source" entries={history} active={active} />
      ) : ui.subView === "routes" ? (
        <FoldedRoutesPanel taskId={attachedTaskId()} onBack={() => studio.setLibraryUi({ subView: "list" })} />
      ) : ui.subView === "intel" ? (
        <VulnIntelPanel taskId={attachedTaskId()} onBack={() => studio.setLibraryUi({ subView: "list" })} />
      ) : ui.subView === "graphql" ? (
        <GraphqlGrpcPanel taskId={attachedTaskId()} onBack={() => studio.setLibraryUi({ subView: "list" })} />
      ) : ui.subView === "flow" ? (
        <IdFlowGraphPanel onBack={() => studio.setLibraryUi({ subView: "list" })} />
      ) : (
        <div ref={scroll.ref} className="cx-scroll min-h-0 flex-1 overflow-y-auto p-4">
          {ordered.length ? (
            <>
              <VirtualRecordList
                items={ordered}
                scrollNode={scroll.element ?? scroll.node.current}
                estimateSize={74}
                ariaLabel="资产虚拟滚动列表"
                renderItem={(source) => (
                  <SourceRow
                    key={source.id}
                    source={source}
                    counts={counts.get(source.id)}
                    reportCount={reportCountsByAsset.get(source.id) ?? 0}
                    covered={runAssets.allCovered.has(source.id)}
                    checked={checklist.isChecked(source.id)}
                    selecting={selecting}
                    onToggle={(next, range) => checklist.toggle(source.id, next, range)}
                  />
                )}
              />
              {selecting ? (
                <div className="sticky bottom-3 z-(--z-raised) mx-auto mt-2 flex w-fit max-w-full flex-wrap items-center gap-1 rounded-2xl ring-1 ring-cx-ring bg-cx-elevated px-3 py-1.5 shadow-cx-md" data-testid="studio-source-bulkbar">
                  <span className="mr-1 text-cx-xs font-medium text-cx-fg-2">已选 {ui.checked.length}</span>
                  <SourceBatchActions checked={ui.checked} matchingIds={orderedIds} filterText={describe} />
                  <IconButton size="sm" icon="x" label="清除选择" onClick={() => setChecked([])} />
                </div>
              ) : null}
            </>
          ) : (
            <EmptyState
              compact
              icon="book"
              title={scoped.length ? "没有符合条件的资产" : "还没有资产"}
              description={scoped.length ? "换个关键词或筛选条件试试。" : "录入一份笔记或摘录，或者导入 CSV、JSON、TXT 文件。"}
              action={scoped.length ? (
                isEmpty ? undefined : <Button size="md" variant="secondary" onClick={() => studio.setLibraryUi({ ...EMPTY_SOURCE_FILTER })}>清除筛选和搜索</Button>
              ) : (
                <>
                  <Button size="md" variant="secondary" icon="plus" onClick={onCollect}>录入资产</Button>
                  <Button size="md" variant="secondary" icon="upload" onClick={onImport}>导入文件</Button>
                </>
              )}
            />
          )}
        </div>
      )}
      {exportOpen ? <BatchExportDialog open={exportOpen} onOpenChange={setExportOpen} kind="source" checkedIds={ui.checked} matchingIds={orderedIds} /> : null}
      {connectorOpen ? <ConnectorsSheet open={connectorOpen} onOpenChange={setConnectorOpen} /> : null}
    </section>
  );
}

const CONNECTOR_READINESS_META: Record<NonNullable<StudioSource["kind"]> | string, { label: string; tone: "success" | "warning" | "danger" | "neutral" | "accent" }> = {
  verified: { label: "已验证就绪", tone: "success" },
  configured_unverified: { label: "已配置待验证", tone: "warning" },
  permission_denied: { label: "权限受限", tone: "danger" },
  quota_unknown: { label: "配额未知", tone: "warning" },
  unconnected: { label: "未连接", tone: "neutral" },
  file_import_only: { label: "仅文件导入", tone: "accent" },
};

function ConnectorsSheet({ open, onOpenChange }: { open: boolean; onOpenChange: (open: boolean) => void }) {
  const state = useStudioState();
  const connectors = state.connectors ?? [];
  const [selectedId, setSelectedId] = useState("");
  const activeId = selectedId || connectors[0]?.id || "";
  const activeConn = connectors.find((c) => c.id === activeId);
  const [secretInputs, setSecretInputs] = useState<Record<string, string>>({});
  const [secretScope, setSecretScope] = useState<"task" | "project">("task");
  const [configInputs, setConfigInputs] = useState<Record<string, string>>({});
  const [queryInput, setQueryInput] = useState("");
  const [busy, setBusy] = useState(false);
  const importFileInputRef = useRef<HTMLInputElement | null>(null);

  const taskId = attachedTaskId();
  const activeProjectId = state.workspace.projectId !== "all" ? state.workspace.projectId : "";
  const canUseProjectScope = Boolean(activeProjectId && state.projects.some((p) => p.id === activeProjectId));

  const loadedConnIdRef = useRef("");
  useEffect(() => {
    if (!activeConn) return;
    if (loadedConnIdRef.current === activeConn.id) return;
    loadedConnIdRef.current = activeConn.id;
    setSecretInputs({});
    const nextCfg: Record<string, string> = {};
    for (const f of activeConn.configFields ?? []) {
      const cur = activeConn.currentConfig?.[f.key];
      const def = f.default;
      nextCfg[f.key] = cur != null ? String(cur) : def != null ? String(def) : "";
    }
    setConfigInputs(nextCfg);
    setQueryInput(activeConn.defaultQuery ?? "");
    setSecretScope(canUseProjectScope ? "project" : "task");
  }, [activeConn, canUseProjectScope]);

  const reloadDomains = async () => {
    await studio.reloadDomain("assets");
  };

  const saveConfigAndSecrets = async () => {
    if (!hasAttachedDomains() || !taskId || !activeConn) return;
    setBusy(true);
    try {
      const nextConfig: Record<string, unknown> = { ...(activeConn.currentConfig ?? {}) };
      for (const refField of activeConn.referenceFields ?? []) {
        const rawSecret = (secretInputs[refField] ?? "").trim();
        if (rawSecret) {
          const alias = `conn_${activeId}_${refField}`.replace(/[^a-zA-Z0-9_-]/g, "_");
          const cred = await putConnectorCredential(taskId, {
            alias,
            value: rawSecret,
            scope: canUseProjectScope && secretScope === "project" ? "project" : "task",
            ...(canUseProjectScope && secretScope === "project" ? { projectId: activeProjectId } : {}),
          });
          nextConfig[refField] = cred.reference;
        }
      }
      for (const field of activeConn.configFields ?? []) {
        const raw = (configInputs[field.key] ?? "").trim();
        if (!raw) {
          delete nextConfig[field.key];
          continue;
        }
        if (field.type === "integer" || field.kind === "number") {
          const parsedNum = Number(raw);
          if (!Number.isFinite(parsedNum)) throw new Error(`${field.label} 需要填写数字`);
          nextConfig[field.key] = parsedNum;
        } else if (field.type === "boolean" || field.kind === "boolean") {
          nextConfig[field.key] = raw === "true" || raw === "1" || raw === "yes";
        } else if ((field.type === "array" || field.key === "endpoints") && raw.includes(",")) {
          nextConfig[field.key] = raw.split(/[\r\n,]+/).map((s) => s.trim()).filter(Boolean);
        } else {
          nextConfig[field.key] = raw;
        }
      }
      await saveConnectorConfig(taskId, { connectorId: activeId, config: nextConfig });
      setSecretInputs({});
      toast({ title: `已保存 ${activeConn.name} 配置与凭据引用`, tone: "success" });
    } catch (err) {
      toast({ title: "保存连接器配置失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
    } finally {
      setBusy(false);
    }
  };

  const verifyConn = async () => {
    if (!hasAttachedDomains() || !taskId || !activeId) return;
    setBusy(true);
    try {
      const res = await verifyConnectorConnection(taskId, activeId) as Record<string, unknown>;
      const readiness = String(res.readiness || res.verification_state || "configured_unverified");
      const meta = CONNECTOR_READINESS_META[readiness] ?? { label: readiness, tone: "neutral" as const };
      toast({
        title: `连接校验完成：${meta.label}`,
        description: res.last_error ? String(res.last_error) : undefined,
        tone: meta.tone === "danger" ? "danger" : meta.tone === "success" ? "success" : "warning",
      });
    } catch (err) {
      toast({ title: "连接器校验失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
    } finally {
      setBusy(false);
    }
  };

  const revokeConfig = async () => {
    if (!hasAttachedDomains() || !taskId || !activeId) return;
    setBusy(true);
    try {
      const current = await querySrc<{ version?: number; revision?: number }>(taskId, "src.connector.config", { connector_id: activeId });
      await postCommand(taskId, "src.connector.config.revoke", {
        connector_id: activeId,
        version: current.version ?? 1,
        base_revision: current.revision ?? 0,
      });
      await reloadDomains();
      toast({ title: `已撤销 ${activeId} 配置`, tone: "success" });
    } catch (err) {
      toast({ title: "撤销配置失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
    } finally {
      setBusy(false);
    }
  };

  const runSync = async () => {
    if (!activeId) return;
    setBusy(true);
    try {
      studio.runConnectorSync(activeId, state.workspace.projectId, queryInput.trim() || activeConn?.defaultQuery || "");
      const sync = await afterDomainMutation();
      if (!sync.ok) { toast({ title: "连接器同步失败", description: sync.error, tone: "danger" }); return; }
      const result = sync.outcome?.sourceResult;
      if (result) {
        const name = activeConn?.name ?? activeId;
        const title = result.state === "completed" ? `${name} 查询完成 · ${result.count} 条来源观测`
          : result.state === "partial" ? `${name} 部分返回 · ${result.count} 条来源观测`
          : `${name} 查询 · ${result.state}`;
        toast({ title, description: result.detail, tone: result.state === "completed" ? "success" : result.state === "failed" ? "danger" : "warning" });
      } else {
        toast({ title: `已触发 ${activeConn?.name ?? activeId} 同步`, tone: "success" });
      }
    } finally {
      setBusy(false);
    }
  };

  const importFile = async (file: File) => {
    if (!hasAttachedDomains() || !taskId || !activeId) return;
    setBusy(true);
    try {
      const buf = await file.arrayBuffer();
      const content_b64 = arrayBufferToBase64(buf);
      await postCommand(taskId, "src.connector.import", {
        connector_id: activeId,
        content_b64,
        filename: file.name,
        idempotency_key: `wb-conn-import-${activeId}-${crypto.randomUUID()}`,
      });
      await reloadDomains();
      toast({ title: `已导入 ${file.name}`, tone: "success" });
    } catch (err) {
      toast({ title: "连接器文件导入失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
    } finally {
      setBusy(false);
    }
  };

  const activeReadiness = activeConn?.readiness ?? "unconnected";
  const readinessMeta = CONNECTOR_READINESS_META[activeReadiness] ?? { label: activeReadiness, tone: "neutral" as const };
  const refFields = activeConn?.referenceFields ?? [];
  const cfgFields = activeConn?.configFields ?? [];

  return (
    <Dialog open={open} onOpenChange={onOpenChange} title="连接外部来源" size="lg">
      {connectors.length ? (
        <div className="flex max-h-[75vh] flex-col gap-3 overflow-y-auto pr-1">
          <div className="flex flex-col">
            <Label>选择连接器</Label>
            <Select<string>
              value={activeId}
              onChange={(id) => setSelectedId(id)}
              ariaLabel="选择连接器"
              options={connectors.map((c) => {
                const r = c.readiness ?? "unconnected";
                const m = CONNECTOR_READINESS_META[r] ?? { label: r };
                return { value: c.id, label: `${c.name} · ${m.label}` };
              })}
            />
          </div>
          {activeConn ? (
            <SurfaceCard className="p-3">
              <div className="mb-2 flex flex-wrap items-center justify-between gap-2">
                <StudioBadge tone={readinessMeta.tone}>{readinessMeta.label}</StudioBadge>
                {hasAttachedDomains() ? (
                  <div className="flex items-center gap-2">
                    <Button size="sm" variant="secondary" icon="shield" disabled={busy} onClick={() => { void verifyConn(); }}>验证连接</Button>
                    <Button size="sm" variant="ghost" disabled={busy} onClick={() => { void revokeConfig(); }}>撤销配置</Button>
                  </div>
                ) : null}
              </div>
              <Dl items={[
                { label: "版本", value: activeConn.version || "—", mono: true },
                { label: "能力", value: activeConn.capabilities.join(" / ") || "—" },
                { label: "支持资产", value: activeConn.supportedAssetTypes.join(" / ") || "—" },
                ...(activeConn.verifiedAt ? [{ label: "最近验证", value: activeConn.verifiedAt }] : []),
                ...(activeConn.quotaRemaining > 0 ? [{ label: "剩余配额", value: String(activeConn.quotaRemaining) }] : []),
              ]} />
              {(activeConn.missingFields ?? []).length > 0 ? (
                <Callout tone="warning" className="mt-2" title="缺少必填配置或凭据">
                  {activeConn.missingFields!.join("、")}
                </Callout>
              ) : null}
              {activeConn.lastError ? (
                <Callout tone="danger" className="mt-2" title="最近错误" role="alert">
                  {activeConn.lastError}
                </Callout>
              ) : null}
            </SurfaceCard>
          ) : null}

          {hasAttachedDomains() && activeConn && (refFields.length > 0 || cfgFields.length > 0) ? (
            <SurfaceCard className="flex flex-col gap-3 p-3">
              <div className="flex flex-wrap items-center justify-between gap-2">
                <span className="text-cx-sm font-medium text-cx-fg">凭据引用与运行参数</span>
                {refFields.length > 0 ? (
                  <SegmentedControl<"task" | "project">
                    value={canUseProjectScope ? secretScope : "task"}
                    onChange={(next) => setSecretScope(next)}
                    ariaLabel="凭据保存作用域"
                    size="sm"
                    options={[
                      { value: "task", label: "仅当前任务 (secret://src/task)" },
                      ...(canUseProjectScope ? [{ value: "project" as const, label: "项目共享 (secret://src/project)" }] : []),
                    ]}
                  />
                ) : null}
              </div>
              {refFields.map((refField) => {
                const boundRef = typeof activeConn.currentConfig?.[refField] === "string" ? String(activeConn.currentConfig[refField]) : "";
                return (
                  <div key={refField} className="flex flex-col gap-1">
                    <TextField
                      label={boundRef ? `${refField}（已绑定引用：${boundRef}）` : refField}
                      type="password"
                      value={secretInputs[refField] ?? ""}
                      onChange={(e) => setSecretInputs((prev) => ({ ...prev, [refField]: e.target.value }))}
                      placeholder={boundRef ? "留空保持现有凭据引用，输入新值将覆盖" : `输入 ${refField}，将加密存入凭证库`}
                    />
                  </div>
                );
              })}
              {cfgFields.length > 0 ? (
                <div className="grid grid-cols-2 gap-2.5 max-sm:grid-cols-1">
                  {cfgFields.map((field) => (
                    <TextField
                      key={field.key}
                      label={`${field.label}${field.required ? " *" : ""}`}
                      value={configInputs[field.key] ?? ""}
                      onChange={(e) => setConfigInputs((prev) => ({ ...prev, [field.key]: e.target.value }))}
                      placeholder={field.placeholder || (field.default != null ? String(field.default) : "")}
                    />
                  ))}
                </div>
              ) : null}
              <div className="flex justify-end">
                <Button size="sm" variant="primary" disabled={busy} onClick={() => { void saveConfigAndSecrets(); }}>保存凭据与配置</Button>
              </div>
            </SurfaceCard>
          ) : null}

          <div className="flex items-end gap-2">
            <div className="flex-1">
              <TextField
                label={activeConn?.capabilities.includes("read_repository") ? "仓库标识（可选；留空使用已配置仓库）" : "查询语句"}
                value={queryInput}
                onChange={(e) => setQueryInput(e.target.value)}
                placeholder={activeConn?.capabilities.includes("read_repository") ? "owner/repo（可选）" : "domain=example.com"}
              />
            </div>
            <Button size="md" variant="primary" disabled={busy} onClick={() => { void runSync(); }}>同步查询</Button>
          </div>
          {hasAttachedDomains() ? (
            <div className="flex items-center justify-between gap-2 border-t border-cx-border-subtle pt-3">
              <Label className="mb-0">导入连接器导出文件 (CSV / JSON / JSONL)</Label>
              <input
                ref={importFileInputRef}
                type="file"
                accept=".json,.jsonl,.csv,.txt,.har,.xml"
                disabled={busy}
                tabIndex={-1}
                aria-label="导入连接器导出文件"
                className="sr-only"
                onChange={(e) => {
                  const file = e.target.files?.[0];
                  if (file) void importFile(file);
                  e.currentTarget.value = "";
                }}
              />
              <Button size="sm" variant="secondary" icon="upload" disabled={busy} onClick={() => importFileInputRef.current?.click()}>
                选择文件导入
              </Button>
            </div>
          ) : null}
        </div>
      ) : <p className="p-2 text-cx-sm text-cx-fg-3">后端没有返回连接器清单。</p>}
    </Dialog>
  );
}


function ConflictRow({ source, conf }: { source: StudioSource; conf: NonNullable<StudioSource["conflicts"]>[number] }) {
  const [open, setOpen] = useState(false);
  const pending = !conf.resolvedBy || conf.resolvedBy === "rule";
  return (
    <div className="rounded-xl px-2 py-1.5" data-testid={`studio-conflict-${conf.id}`}>
      <button type="button" onClick={() => setOpen((v) => !v)} className="flex w-full items-center gap-3 text-left">
        <span className="grid size-8 shrink-0 place-items-center rounded-lg bg-cx-hover text-cx-fg-3"><Icon name="gitCompare" size={15} /></span>
        <span className="flex min-w-0 flex-1 flex-col">
          <span className="truncate text-cx-sm font-medium text-cx-fg">{conf.fieldLabel}</span>
          <span className="truncate text-cx-xs text-cx-fg-4">
            {pending ? "待裁决" : conf.resolvedBy === "manual" ? "人工已裁决" : "AI 建议已采纳"} · 当前 {conf.selectedValue}
          </span>
        </span>
        {pending ? <span className="shrink-0 text-cx-xs text-cx-warning">待裁决</span> : null}
        <Icon name={open ? "chevronDown" : "chevronRight"} size={14} className="shrink-0 text-cx-fg-4" />
      </button>
      {open ? (
        <div className="mt-2 flex flex-col gap-1 pl-11">
          {conf.candidates.map((cand, idx) => {
            const active = cand.value === conf.selectedValue;
            return (
              <div key={idx} className="flex items-center justify-between gap-2 rounded-lg px-2 py-1.5 text-cx-sm hover:bg-cx-hover">
                <div className="flex min-w-0 flex-1 flex-col">
                  <span className={cn("truncate", active ? "font-medium text-cx-fg" : "text-cx-fg-2")}>{cand.value}</span>
                  <span className="truncate text-cx-xs text-cx-fg-4">{cand.source} · {cand.observedAt}</span>
                </div>
                <button
                  type="button"
                  disabled={active}
                  className={cn("shrink-0 text-cx-xs font-medium", active ? "cursor-default text-cx-fg-4" : "text-cx-accent hover:underline")}
                  onClick={() => {
                    studio.resolveAssetConflict(source.id, conf.id, cand.value, `人工裁决选用「${cand.source}」（${cand.observedAt}）的值`);
                    toast({ title: `已选用「${cand.source}」的字段值`, tone: "success" });
                  }}
                >
                  {active ? "当前值" : "选用"}
                </button>
              </div>
            );
          })}
        </div>
      ) : null}
    </div>
  );
}

function VersionDiffDialog({ source, version, onClose }: { source: StudioSource; version: AssetVersionSnapshot; onClose: () => void }) {
  const versions = source.versions ?? [];
  const idx = versions.findIndex((v) => v.version === version.version);
  const prev = versions[idx + 1];
  if (!prev) return null;
  const keys = Array.from(new Set([...Object.keys(prev.attributes ?? {}), ...Object.keys(version.attributes ?? {})]));
  const lines = [
    `正文预览`,
    ...keys.map((key) => `${key}: ${prev.attributes[key] ?? "—"} → ${version.attributes[key] ?? "—"}`),
    ``,
    `正文 A: ${prev.bodyPreview}`,
    `正文 B: ${version.bodyPreview}`,
  ].join("\n");
  return (
    <Dialog open onOpenChange={(o) => { if (!o) onClose(); }} title={`${prev.label} → ${version.label}`} description={`${version.createdAt} · ${version.sourceLabel}`} size="lg">
      <pre className="cx-scroll max-h-[50vh] overflow-auto whitespace-pre-wrap rounded-xl bg-cx-bg-subtle p-3 font-cx-mono text-cx-xs leading-5 text-cx-fg-2">{lines}</pre>
      <div className="flex items-center justify-between gap-2 pt-3">
        <span className="text-cx-xs text-cx-fg-4">{version.summary}</span>
        <CopyButton text={lines} label="复制差异" />
      </div>
    </Dialog>
  );
}

function SourceReader({ source, active }: { source: StudioSource; active: boolean }) {
  const ctx = useContextMenu();
  const state = useStudioState();
  const ui = state.workspace.library;
  const scroll = useRememberedScroll(`library-reader:${source.id}`, active);
  const [content, setContent] = useState<HTMLElement | null>(null);
  const selection = usePassageSelection(content, active);
  const [diffVer, setDiffVer] = useState<AssetVersionSnapshot | null>(null);
  const [judgementState, setJudgementState] = useState<StudioSource["judgementState"]>(source.judgementState ?? "needs_verify");
  const [systemName, setSystemName] = useState(source.systemName ?? "");
  const [uncertaintyNote, setUncertaintyNote] = useState(source.uncertaintyNote ?? "");
  const [judgementBusy, setJudgementBusy] = useState(false);
  const [splitIds, setSplitIds] = useState<string[]>([]);
  const [splitBusy, setSplitBusy] = useState(false);

  useEffect(() => {
    setJudgementState(source.judgementState ?? "needs_verify");
    setSystemName(source.systemName ?? "");
    setUncertaintyNote(source.uncertaintyNote ?? "");
    setSplitIds([]);
  }, [source.id, source.judgementState, source.systemName, source.uncertaintyNote]);

  const submitJudgement = async () => {
    const taskId = attachedTaskId();
    if (!hasAttachedDomains() || !taskId) {
      toast({ title: "仅在真实 SRC 任务中支持保存定性研判", tone: "warning" });
      return;
    }
    setJudgementBusy(true);
    try {
      await saveAssetJudgement(taskId, {
        assetId: source.id,
        judgementState,
        systemName,
        uncertaintyNote,
      });
      toast({ title: "已保存资产归属与定性研判", tone: "success" });
    } catch (err) {
      toast({ title: "保存定性研判失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
    } finally {
      setJudgementBusy(false);
    }
  };

  const runSplit = async () => {
    const taskId = attachedTaskId();
    if (!hasAttachedDomains() || !taskId || !splitIds.length) return;
    setSplitBusy(true);
    try {
      await splitMergedAsset(taskId, { assetId: source.id, observationIds: splitIds });
      setSplitIds([]);
      toast({ title: `已从当前资产拆出 ${splitIds.length} 条观测为独立资产`, tone: "success" });
    } catch (err) {
      toast({ title: "拆分错误合并失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
    } finally {
      setSplitBusy(false);
    }
  };

  const clues = useMemo(
    () => state.clues.filter((clue) => clue.sourceId === source.id || clue.affectedSourceIds?.includes(source.id)),
    [source.id, state.clues],
  );
  const listedClues = useMemo(() => clues.filter((clue) => !mergeOf(state.merges, "clue", clue.id)), [clues, state.merges]);
  const activeClue = ui.clueId ? clues.find((clue) => clue.id === ui.clueId) : undefined;
  const focusPassageId = ui.passageId || activeClue?.passageId || "";
  const relations = source.relations ?? [];
  const relatedRuns = state.workRuns.filter((r) => r.linkedRefs.some((ref) => ref.kind === "source" && ref.id === source.id));
  const versions = source.versions ?? [];
  const conflicts = source.conflicts ?? [];
  const unresolvedConflicts = conflicts.filter((conf) => !conf.resolvedBy || conf.resolvedBy === "rule").length;
  const workspaceProject = state.workspace.projectId;

  useViewing(active, readerTarget(ui, source));
  useEffect(() => { studio.markSeen({ kind: "source", id: source.id }); }, [source.id]);
  useEffect(() => { if (activeClue) studio.markSeen({ kind: "clue", id: activeClue.id }); }, [activeClue]);
  useEffect(() => {
    if (workspaceProject !== "all" && workspaceProject !== source.projectId) studio.setWorkspaceProject(source.projectId);
  }, [source.projectId, workspaceProject]);
  useEffect(() => {
    if (!focusPassageId || !active) return;
    const frame = window.requestAnimationFrame(() => {
      document.getElementById(passageDomId(focusPassageId))?.scrollIntoView({ block: "center", behavior: scrollBehavior() });
    });
    return () => window.cancelAnimationFrame(frame);
  }, [active, focusPassageId, ui.clueId]);

  const selectQuote = (id: string) => {
    const clue = clues.find((item) => item.id === id);
    if (clue) studio.setLibraryUi({ passageId: clue.passageId, clueId: clue.id });
  };
  const sourceRef: StudioRef = { kind: "source", id: source.id };
  const actions = <SourceActions source={source} />;
  const backPath = ui.importsOpen ? "导入记录" : ui.view === "duplicates" ? "可能重复" : ui.view === "history" ? "处理记录" : "";

  useEffect(() => {
    if (!active) return;
    const onKey = (event: globalThis.KeyboardEvent) => { if (event.key === "Escape") closeReader(); };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [active]);

  return (
    <div className="flex min-h-0 flex-1 flex-col" data-testid="studio-source-reader" onContextMenu={ctx.onContextMenu} onKeyDown={ctx.onKeyDown}>
      <SurfaceToolbar>
        <IconButton size="sm" icon="arrowLeft" label="返回" onClick={closeReader} />
        <PathBreadcrumb path={`${backPath ? `${backPath}/` : ""}${source.id}`} rootLabel="资产" lastIsFile onSelect={closeReader} />
        <Menu ariaLabel="资产操作" trigger={<IconButton size="sm" icon="more" label="更多" />}>
          {actions}
        </Menu>
      </SurfaceToolbar>
      {ctx.render(actions, `资产 ${source.title} 操作`)}
      <div ref={scroll.ref} className="cx-scroll min-h-0 flex-1 overflow-y-auto">
        <article ref={setContent} className="relative mx-auto flex w-full max-w-[760px] flex-col gap-4 p-4 pb-10">
          <SurfaceCard className="p-3">
            <div className="flex items-start gap-3">
              <div className="min-w-0 flex-1">
                <p className="truncate text-cx-md font-semibold text-cx-fg">{assetTitle(source)}</p>
              </div>
              <StudioBadge tone={source.status === "filed" ? "accent" : "neutral"}>{sourceStatusLabel(source.status)}</StudioBadge>
            </div>
            <Dl className="mt-3" items={[
              {
                label: "类型",
                value: (
                  <Select<SourceKind> value={source.kind} onChange={(kind) => studio.setSourceKind(source.id, kind)} size="sm" ariaLabel="资产类型" className="min-w-[130px]" options={SOURCE_KINDS.map((item) => ({ value: item.value, label: item.label }))} />
                ),
              },
              { label: "资产细类", value: assetTypeLabel(source.assetType, source.kind) },
              {
                label: "重要性",
                value: (
                  <Select<AssetImportance> value={source.importance ?? "normal"} onChange={(imp) => studio.setSourceImportance(source.id, imp)} size="sm" ariaLabel="资产重要性" className="min-w-[130px]" options={ASSET_IMPORTANCE_LEVELS} />
                ),
              },
              {
                label: "整理状态",
                value: (
                  <Select<StudioSource["status"]> value={source.status} onChange={(status) => studio.setSourceStatus(source.id, status)} size="sm" ariaLabel="整理状态" className="min-w-[130px]" options={SOURCE_STATUSES.map((item) => ({ value: item.value, label: item.label }))} />
                ),
              },
              { label: "规范标识", value: displayTitle(source.canonicalKey ?? source.title), mono: true, copy: source.canonicalKey ?? source.title },
              { label: "别名", value: (source.aliases ?? []).join("、") || "无" },
              { label: "首次观察", value: source.firstSeenAt ?? source.collectedAt },
              { label: "最近观察", value: source.lastObservedAt ?? source.collectedAt },
              ...(state.importOrigins[source.id] ? [{ label: "来源批次", value: <ImportOriginLink sourceId={source.id} /> }] : []),
            ]} />
          </SurfaceCard>

          <MergeInfo kind="source" id={source.id} />

          {!source.isMaterial ? (
            <SurfaceCard className="p-3" data-testid="asset-judgement-card">
              <div className="mb-2 flex flex-wrap items-center justify-between gap-2">
                <span className="text-cx-sm font-medium text-cx-fg">研究者归属与定性研判</span>
                {source.activeOperationAuthorized === false ? (
                  <StudioBadge tone="warning">待授权候选</StudioBadge>
                ) : source.observationRole ? (
                  <StudioBadge tone="neutral">{source.observationRole}</StudioBadge>
                ) : null}
              </div>
              <div className="grid grid-cols-2 gap-3 max-sm:grid-cols-1">
                <div className="flex flex-col">
                  <Label>研判结论</Label>
                  <Select<NonNullable<StudioSource["judgementState"]>>
                    value={judgementState ?? "needs_verify"}
                    onChange={setJudgementState}
                    ariaLabel="研判结论"
                    options={[
                      { value: "confirmed_in_scope", label: "已确认范围内" },
                      { value: "candidate", label: "候选待核" },
                      { value: "out_of_scope", label: "范围外" },
                      { value: "needs_verify", label: "待研判" },
                    ]}
                  />
                </div>
                <TextField
                  label="所属系统 / 业务线"
                  value={systemName}
                  onChange={(e) => setSystemName(e.target.value)}
                  placeholder="例如：统一认证网关 / 开放平台"
                />
              </div>
              <div className="mt-2.5 flex flex-col gap-2">
                <TextArea
                  label="不确定性 / 研判备注"
                  rows={2}
                  value={uncertaintyNote}
                  onChange={(e) => setUncertaintyNote(e.target.value)}
                  placeholder="记录来源冲突疑点、归属判断依据或需后续确认事项"
                  className="text-cx-sm"
                />
                <div className="flex justify-end">
                  <Button size="sm" variant="primary" disabled={judgementBusy} onClick={() => { void submitJudgement(); }}>保存定性研判</Button>
                </div>
              </div>
            </SurfaceCard>
          ) : (
            <SurfaceCard className="p-3">
              <div className="flex flex-wrap items-center gap-2">
                <StudioBadge tone="accent">{MATERIAL_KIND_LABELS[source.materialKind || ""] || source.materialKind || "项目资料"}</StudioBadge>
                <span className="text-cx-xs text-cx-fg-3">静态项目资料仅用于查阅与对照分析，不直接授予主动操作授权范围。</span>
              </div>
            </SurfaceCard>
          )}

          {(source.mergedItems ?? []).length ? (
            <div>
              <SectionHeader title="已归并资产条目" count={source.mergedItems!.length} />
              <SurfaceCard>
                {source.mergedItems!.map((item) => (
                  <SurfaceRow
                    key={item.id}
                    icon="layers"
                    title={item.title || item.id}
                    subtitle={[item.id, item.assetType, item.reason].filter(Boolean).join(" · ")}
                  />
                ))}
              </SurfaceCard>
            </div>
          ) : null}

          {(source.sourceHistory ?? []).length ? (
            <div>
              <SectionHeader
                title="来源观测"
                count={source.sourceHistory!.length}
                action={hasAttachedDomains() && (source.sourceHistory?.length ?? 0) > 1 ? (
                  <Button
                    size="sm"
                    variant="secondary"
                    disabled={splitBusy || splitIds.length === 0 || splitIds.length >= (source.sourceHistory?.length ?? 0)}
                    onClick={() => { void runSplit(); }}
                  >
                    拆分错误合并 ({splitIds.length})
                  </Button>
                ) : undefined}
              />
              <SurfaceCard>
                {source.sourceHistory!.map((obs) => {
                  const checked = splitIds.includes(obs.id);
                  return (
                    <SurfaceRow
                      key={obs.id}
                      icon="history"
                      leading={hasAttachedDomains() && (source.sourceHistory?.length ?? 0) > 1 ? (
                        <Checkbox
                          checked={checked}
                          aria-label={`选择观测 ${obs.id} 准备拆分`}
                          onCheckedChange={(next) => {
                            setSplitIds((prev) => (next ? [...new Set([...prev, obs.id])] : prev.filter((x) => x !== obs.id)));
                          }}
                          className="[&>button]:size-5"
                        />
                      ) : undefined}
                      title={obs.sourceLabel || obs.id}
                      subtitle={[obs.observedAt || obs.ingestedAt, obs.summary].filter(Boolean).join(" · ")}
                      selected={checked}
                    />
                  );
                })}
              </SurfaceCard>
            </div>
          ) : null}

          {conflicts.length ? (
            <div>
              <SectionHeader title="字段冲突" count={unresolvedConflicts || conflicts.length} action={<button type="button" className="text-cx-xs font-medium text-cx-accent hover:underline" onClick={() => sendAbout(sourceRef, `请分析资产 ${source.id} 的字段冲突并给出裁决建议。`)}>让 Agent 裁决</button>} />
              <SurfaceCard>
                {conflicts.map((conf) => <ConflictRow key={conf.id} source={source} conf={conf} />)}
              </SurfaceCard>
            </div>
          ) : null}

          {listedClues.length ? (
            <div>
              <SectionHeader title="发现" count={listedClues.length} />
              <SurfaceCard>
                {listedClues.map((clue) => (
                  <SurfaceRow key={clue.id} icon="bookmark" title={displayTitle(clue.title)} subtitle={[categoryLabel(clue.category), statusLabel(clue.status)].join(" · ")} onClick={() => openRef({ kind: "clue", id: clue.id })} />
                ))}
              </SurfaceCard>
            </div>
          ) : null}

          {relations.length || relatedRuns.length ? (
            <div>
              <SectionHeader title="关系" count={relations.length + relatedRuns.length} />
              <SurfaceCard>
                {relations.map((rel) => (
                  <SurfaceRow key={rel.id} icon="link" title={rel.targetLabel} subtitle={`${rel.relationType} · ${{ observed: "观察", derived: "派生", inferred: "推断" }[rel.origin]} · ${rel.observedAt}`} onClick={rel.targetKind === "source" ? () => openSource(rel.targetId) : undefined} />
                ))}
                {relatedRuns.map((r) => (
                  <SurfaceRow key={r.id} icon="terminal" title={displayTitle(r.title)} subtitle={`工作记录 · ${RUN_STATE_LABEL[r.state]}`} onClick={() => openRef({ kind: "run", id: r.id })} />
                ))}
              </SurfaceCard>
            </div>
          ) : null}

          {versions.length ? (
            <div>
              <SectionHeader title="版本" count={versions.length} action={versions.length > 1 ? undefined : undefined} />
              <SurfaceCard>
                {versions.map((v, idx) => (
                  <SurfaceRow
                    key={v.version}
                    icon="history"
                    title={`${v.label}${v.version === source.pinnedVersion ? " · 当前固定" : ""}`}
                    subtitle={`${v.createdAt} · ${v.sourceLabel}`}
                    onClick={idx < versions.length - 1 ? () => setDiffVer(v) : undefined}
                  />
                ))}
              </SurfaceCard>
            </div>
          ) : null}

          <div>
            <SectionHeader title="原文" count={source.passages.length} action={source.passages.length ? <span className="text-cx-xs text-cx-fg-4">选中文字可问 AI 或记为发现</span> : undefined} />
            {source.passages.length ? (
              <ol className="flex flex-col gap-1">
                {source.passages.map((passage) => {
                  const passageClues = clues.filter((clue) => clue.passageId === passage.id);
                  const focused = passage.id === focusPassageId;
                  return (
                    <li
                      key={passage.id}
                      id={passageDomId(passage.id)}
                      className={cn("group relative flex scroll-mt-32 gap-3 rounded-xl py-2.5 pl-1 pr-2 transition-colors", focused ? "bg-cx-accent-soft" : "hover:bg-cx-hover")}
                      data-focused={focused || undefined}
                    >
                      <button
                        type="button"
                        className="cx-tabular h-[32px] w-[32px] shrink-0 rounded-lg text-center text-cx-xs text-cx-fg-4 outline-none hover:bg-cx-active hover:text-cx-fg focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]"
                        aria-label={`定位到第 ${passage.index} 段`}
                        aria-pressed={focused}
                        onClick={() => studio.setLibraryUi({ passageId: focused && !ui.clueId ? "" : passage.id, clueId: "" })}
                        {...dragRef({ kind: "passage", id: passage.id })}
                      >
                        {passage.index}
                      </button>
                      <p data-passage-text={passage.id} className="min-w-0 flex-1 select-text pt-0.5 text-cx-md leading-7 text-cx-fg">
                        <HighlightedText text={passage.text} quotes={passageClues} activeQuote={activeClue?.id} onSelectQuote={selectQuote} />
                      </p>
                      <AttachButton target={{ kind: "passage", id: passage.id }} small reveal className={cn("shrink-0", focused && "opacity-100")} />
                    </li>
                  );
                })}
              </ol>
            ) : <EmptyState compact icon="book" title="这份资产没有正文" />}
          </div>

          <RecordHistory target={sourceRef} />
          {selection ? <SelectionToolbar selection={selection} source={source} /> : null}
        </article>
      </div>
      {diffVer ? <VersionDiffDialog source={source} version={diffVer} onClose={() => setDiffVer(null)} /> : null}
    </div>
  );
}

export function LibraryWorkspace({ active, sheet = false }: { active: boolean; sheet?: boolean }) {
  const state = useStudioState();
  const ui = state.workspace.library;
  const [collectOpen, setCollectOpen] = useState(false);
  const [importOpen, setImportOpen] = useState(false);
  const source = ui.sourceId ? state.sources.find((item) => item.id === ui.sourceId) : undefined;
  const showImports = ui.importsOpen && !ui.sourceId;
  const projectId = state.workspace.projectId;
  const fallbackProject = projectId === "all"
    ? state.threads[state.activeThreadId]?.projectId ?? state.projects[0]?.id ?? ""
    : projectId;

  useViewing(active && !ui.sourceId && !ui.importsOpen, null);
  const openCollect = useCallback(() => setCollectOpen(true), []);
  const openImport = useCallback(() => setImportOpen(true), []);

  return (
    <div className="flex h-full min-h-0 flex-col" data-testid="studio-library-workspace">
      <div className={cn("flex min-h-0 flex-1 flex-col", (ui.sourceId || ui.importsOpen) && "hidden")}>
        <SourceList active={active && !ui.sourceId && !ui.importsOpen} onCollect={openCollect} onImport={openImport} />
      </div>
      {showImports ? <ImportHistory active={active} onImport={openImport} /> : null}
      {ui.sourceId ? (
        source ? <SourceReader key={source.id} source={source} active={active} /> : (
          <div className="grid min-h-0 flex-1 place-items-center p-6">
            <EmptyState
              icon="book"
              title="资产不存在"
              description="链接指向的资产不在这个浏览器保存的内容里。"
              action={<Button size="md" variant="secondary" onClick={closeReader}>返回资产列表</Button>}
            />
          </div>
        )
      ) : null}
      <SheetAskBar
        sheet={sheet}
        target={source ? readerTarget(ui, source) : null}
        prefill={source ? `关于《${source.title}》：` : undefined}
      />
      {collectOpen ? <CollectSourceDialog open={collectOpen} onOpenChange={setCollectOpen} projectId={fallbackProject} /> : null}
      {importOpen ? <ImportSourcesDialog open={importOpen} onOpenChange={setImportOpen} projectId={fallbackProject} /> : null}
    </div>
  );
}
