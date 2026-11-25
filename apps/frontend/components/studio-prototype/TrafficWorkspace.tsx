"use client";

import { useCallback, useEffect, useMemo, useState } from "react";
import { randomUuid } from "@/lib/browserCrypto";
import {
  Button,
  Callout,
  Checkbox,
  Dialog,
  EmptyState,
  IconButton,
  Input,
  Label,
  SearchInput,
  SegmentedControl,
  Select,
  TextArea,
  TextField,
  toast,
} from "@muteki/ui";
import { CodeBlock } from "@muteki/ui/agent";
import { SectionHeader, SurfaceToolbar, relativeTime } from "@/components/chat/surfaces/shared";
import { SurfaceCard, SurfaceRow } from "@/components/chat/surfaces/OverviewSurface";
import { Dl } from "./Kit";
import { SheetAskBar, StudioBadge, askAi, useRememberedScroll, useViewing } from "./parts";
import { VirtualRecordList } from "./RecordTools";
import { postCommand, querySrc } from "../src/reviewPrimitives";
import { attachedTaskId, hasAttachedDomains, studio, useStudioState } from "./store";
import { recordListAll } from "./real/domain";
import type { IdentityContextSummary, RequestMutatorSummary, StudioRef, TrafficIndexEntry } from "./model";

type TrafficTab = "stream" | "repeater" | "identities" | "proxy" | "proxypool" | "mutators" | "oast" | "streams" | "wire";

function decodeBase64Utf8(b64: string): string {
  try {
    return new TextDecoder("utf-8", { fatal: false }).decode(
      Uint8Array.from(atob(b64), (c) => c.charCodeAt(0)),
    );
  } catch {
    return "";
  }
}

export function TrafficWorkspace({ active, sheet = false }: { active: boolean; sheet?: boolean }) {
  const state = useStudioState();
  const ui = state.workspace.traffic ?? {
    tab: "stream" as TrafficTab,
    selectedExchangeId: "",
    compareBaselineId: "",
    query: "",
    method: "all",
    host: "",
    statusCode: "all",
    contextId: "all",
    bookmarkedOnly: false,
  };
  const taskId = attachedTaskId();
  const attached = hasAttachedDomains() && Boolean(taskId);
  const scroll = useRememberedScroll("traffic:stream", active);

  const [items, setItems] = useState<TrafficIndexEntry[]>(state.trafficItems ?? []);
  const [searching, setSearching] = useState(false);
  const [loadingMore, setLoadingMore] = useState(false);
  const [nextCursor, setNextCursor] = useState<string | null>(null);
  const [searchError, setSearchError] = useState("");
  const [poolError, setPoolError] = useState("");
  const [oastError, setOastError] = useState("");
  const [streamError, setStreamError] = useState("");
  const [diagError, setDiagError] = useState("");
  const [rawView, setRawView] = useState<{
    id: string;
    reqText: string;
    respText: string;
    reqSize: number;
    respSize: number;
    reqSpilled: boolean;
    respSpilled: boolean;
    loading: boolean;
    error: string;
  }>({ id: "", reqText: "", respText: "", reqSize: 0, respSize: 0, reqSpilled: false, respSpilled: false, loading: false, error: "" });

  const [draftRaw, setDraftRaw] = useState("GET / HTTP/1.1\r\nHost: example.com\r\n\r\n");
  const [draftCtx, setDraftCtx] = useState("default");
  const [draftMutator, setDraftMutator] = useState("");
  const [altCtx, setAltCtx] = useState("");
  const [sending, setSending] = useState(false);
  const [diffResult, setDiffResult] = useState<Record<string, unknown> | null>(null);
  const [raceConcurrency, setRaceConcurrency] = useState("5");
  const [raceApprovedOnce, setRaceApprovedOnce] = useState(false);
  useEffect(() => { setRaceApprovedOnce(false); }, [draftRaw, draftCtx, draftMutator, raceConcurrency]);
  const [raceResult, setRaceResult] = useState<Record<string, unknown> | null>(null);

  const [importOpen, setImportOpen] = useState(false);
  const [impCtx, setImpCtx] = useState("");
  const [impDomain, setImpDomain] = useState("");
  const [impCookie, setImpCookie] = useState("");
  const [impAuth, setImpAuth] = useState("");
  const [impTotp, setImpTotp] = useState("");
  const [probeUrlMap, setProbeUrlMap] = useState<Record<string, string>>({});

  const [bridgeCtx, setBridgeCtx] = useState("");
  const [bridgeAllowLan, setBridgeAllowLan] = useState(false);
  const [bridgeBusy, setBridgeBusy] = useState(false);

  const [mutOpen, setMutOpen] = useState(false);
  const [mutName, setMutName] = useState("");
  const [mutKind, setMutKind] = useState<"declarative" | "sandbox_js">("declarative");
  const [mutAlgo, setMutAlgo] = useState("hmac_sha256");
  const [mutSecret, setMutSecret] = useState("");
  const [mutHeader, setMutHeader] = useState("X-Signature");
  const [mutJs, setMutJs] = useState("function mutate(req, secrets) {\n  return { headers: { 'X-Custom-Sign': req.timestamp + ':' + req.nonce } };\n}");
  const [mutTestOut, setMutTestOut] = useState<Record<string, unknown> | null>(null);

  const [poolStatus, setPoolStatus] = useState<Record<string, unknown> | null>(null);
  const [poolEnabled, setPoolEnabled] = useState(false);
  const [poolSelection, setPoolSelection] = useState<"failover" | "round_robin">("failover");
  const [poolFallback, setPoolFallback] = useState(false);
  const [poolScheme, setPoolScheme] = useState<"http" | "https" | "socks5" | "socks5h">("http");
  const [poolHost, setPoolHost] = useState("");
  const [poolPort, setPoolPort] = useState("8080");
  const [poolUser, setPoolUser] = useState("");
  const [poolPass, setPoolPass] = useState("");
  const [poolBusy, setPoolBusy] = useState(false);

  const [oastInteractions, setOastInteractions] = useState<Array<Record<string, unknown>>>([]);
  const [oastEvents, setOastEvents] = useState<Array<Record<string, unknown>>>([]);
  const [selectedInteractionId, setSelectedInteractionId] = useState("");
  const [oastNote, setOastNote] = useState("");
  const [oastBusy, setOastBusy] = useState(false);

  const [streams, setStreams] = useState<Array<Record<string, unknown>>>([]);
  const [selectedStreamId, setSelectedStreamId] = useState("");
  const [streamEvents, setStreamEvents] = useState<Array<Record<string, unknown>>>([]);
  const [streamProto, setStreamProto] = useState<"websocket" | "sse">("websocket");
  const [streamUrl, setStreamUrl] = useState("");
  const [streamCtx, setStreamCtx] = useState("default");
  const [streamSendText, setStreamSendText] = useState("");
  const [streamBusy, setStreamBusy] = useState(false);

  const [wireExchangeId, setWireExchangeId] = useState("");
  const [wireDetail, setWireDetail] = useState<Record<string, unknown> | null>(null);
  const [mcpCircuits, setMcpCircuits] = useState<{ mcp_circuits?: Array<Record<string, unknown>>; quota_cooldowns?: Array<Record<string, unknown>> } | null>(null);
  const [diagBusy, setDiagBusy] = useState(false);

  const loadOastData = useCallback(async () => {
    if (!attached) return;
    setOastBusy(true);
    setOastError("");
    try {
      const [intRes, evRes] = await Promise.all([
        querySrc<{ records?: Array<Record<string, unknown>> }>(taskId, "src.record.list", { kind: "oast_interaction", limit: 50 }),
        querySrc<{ records?: Array<Record<string, unknown>> }>(taskId, "src.record.list", { kind: "oast_event", limit: 100 }),
      ]);
      const ints = intRes.records ?? [];
      setOastInteractions(ints);
      setOastEvents(evRes.records ?? []);
      if (!selectedInteractionId && ints[0]) {
        setSelectedInteractionId(String(ints[0].natural_key || ints[0].record_id || ""));
      }
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      setOastError(msg);
      toast({ title: "加载 OAST 数据失败", description: msg, tone: "danger" });
    } finally {
      setOastBusy(false);
    }
  }, [attached, selectedInteractionId, taskId]);

  const loadStreamsData = useCallback(async (sidOverride?: string) => {
    if (!attached) return;
    setStreamBusy(true);
    setStreamError("");
    try {
      const res = await querySrc<{ records?: Array<Record<string, unknown>> }>(taskId, "src.record.list", {
        kind: "protocol_stream",
        limit: 50,
      });
      const list = res.records ?? [];
      setStreams(list);
      const targetSid = sidOverride || selectedStreamId || String(list[0]?.natural_key || list[0]?.record_id || "");
      if (targetSid) {
        setSelectedStreamId(targetSid);
        const exported = await querySrc<{ events?: Array<Record<string, unknown>> }>(taskId, "src.stream.export", {
          root_stream_id: targetSid,
        });
        setStreamEvents(exported.events ?? []);
      } else {
        setStreamEvents([]);
      }
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      setStreamError(msg);
      toast({ title: "加载 WS/SSE 流失败", description: msg, tone: "danger" });
    } finally {
      setStreamBusy(false);
    }
  }, [attached, selectedStreamId, taskId]);

  const loadDiagnostics = useCallback(async (exId?: string) => {
    if (!attached) return;
    setDiagBusy(true);
    setDiagError("");
    try {
      const circ = await querySrc<{ mcp_circuits?: Array<Record<string, unknown>>; quota_cooldowns?: Array<Record<string, unknown>> }>(
        taskId,
        "src.diagnostics.mcp_circuits",
        {},
      );
      setMcpCircuits(circ);
      const targetEx = (exId ?? (wireExchangeId || ui.selectedExchangeId)).trim();
      if (targetEx) {
        setWireExchangeId(targetEx);
        const w = await querySrc<Record<string, unknown>>(taskId, "src.diagnostics.wire", { exchange_id: targetEx });
        setWireDetail(w);
      } else {
        setWireDetail(null);
      }
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      setDiagError(msg);
      toast({ title: "加载 Wire/熔断诊断失败", description: msg, tone: "danger" });
    } finally {
      setDiagBusy(false);
    }
  }, [attached, taskId, ui.selectedExchangeId, wireExchangeId]);

  const loadPoolStatus = useCallback(async () => {
    if (!attached) return;
    setPoolError("");
    try {
      const res = await querySrc<Record<string, unknown>>(taskId, "src.proxypool.status", {});
      setPoolStatus(res);
      setPoolEnabled(Boolean(res.enabled));
      if (res.selection === "round_robin" || res.selection === "failover") {
        setPoolSelection(res.selection);
      }
      setPoolFallback(Boolean(res.fallback_direct));
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      setPoolError(msg);
      toast({ title: "加载代理池状态失败", description: msg, tone: "danger" });
    }
  }, [attached, taskId]);

  useEffect(() => {
    if (ui.tab === "proxypool" && attached) {
      void loadPoolStatus();
    }
  }, [attached, loadPoolStatus, ui.tab]);

  const entries = items;
  const selected = useMemo(
    () => entries.find((e) => e.exchangeId === ui.selectedExchangeId) ?? entries[0],
    [entries, ui.selectedExchangeId],
  );
  const targetRef: StudioRef | null = selected ? { kind: "evidence", id: selected.exchangeId } : null;
  useViewing(active, targetRef);

  const reloadAll = useCallback(async () => {
    await (globalThis as { __mutekiReloadDomains?: () => Promise<void> }).__mutekiReloadDomains?.();
  }, []);

  const mapTrafficRows = (rows: Array<Record<string, unknown>>): TrafficIndexEntry[] => rows.map((row) => ({
    exchangeId: String(row.exchange_id ?? ""),
    method: String(row.method ?? "GET"),
    observedUrl: String(row.observed_url ?? ""),
    host: String(row.host ?? ""),
    path: String(row.path ?? "/"),
    statusCode: row.status_code != null ? Number(row.status_code) : null,
    protocol: String(row.protocol ?? "http/1.1"),
    contextId: String(row.context_id ?? "default"),
    sourceRunId: String(row.source_run_id ?? ""),
    sourceKind: String(row.source_kind ?? "http_tool"),
    mime: String(row.mime ?? ""),
    startedAt: String(row.started_at ?? ""),
    requestSize: Number(row.request_size ?? 0),
    requestSha256: String(row.request_sha256 ?? ""),
    requestSpilled: Boolean(row.request_spilled),
    responseSize: Number(row.response_size ?? 0),
    responseSha256: String(row.response_sha256 ?? ""),
    responseSpilled: Boolean(row.response_spilled),
    bookmarked: Boolean(row.bookmarked),
    bookmarkLabel: String(row.bookmark_label ?? ""),
  }));

  const runSearch = useCallback(async (opts?: { appendCursor?: string | null }) => {
    if (!attached) return;
    const appendCursor = opts?.appendCursor ?? null;
    const appending = Boolean(appendCursor);
    if (appending) setLoadingMore(true);
    else setSearching(true);
    setSearchError("");
    try {
      const res: { items?: Array<Record<string, unknown>>; next_cursor?: string | null } = await querySrc(taskId, "src.traffic.search", {
        ...(appendCursor ? { cursor: appendCursor } : {}),
        query: ui.query.trim() || undefined,
        method: ui.method !== "all" ? ui.method : undefined,
        status_code: ui.statusCode !== "all" ? Number(ui.statusCode) : undefined,
        context_id: ui.contextId !== "all" ? ui.contextId : undefined,
        bookmarked_only: ui.bookmarkedOnly || undefined,
        limit: 100,
      });
      const mapped = mapTrafficRows(res.items ?? []);
      setNextCursor(res.next_cursor ?? null);
      if (appending) {
        setItems((prev) => {
          const seen = new Set(prev.map((row) => row.exchangeId));
          return [...prev, ...mapped.filter((row) => row.exchangeId && !seen.has(row.exchangeId))];
        });
      } else {
        setItems(mapped);
      }
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      setSearchError(msg);
      toast({ title: "流量检索失败", description: msg, tone: "danger" });
    } finally {
      if (appending) setLoadingMore(false);
      else setSearching(false);
    }
  }, [attached, taskId, ui.bookmarkedOnly, ui.contextId, ui.method, ui.query, ui.statusCode]);

  useEffect(() => {
    if (active && attached) {
      void runSearch();
    }
  }, [active, attached, runSearch]);

  useEffect(() => {
    if (!attached || !selected?.exchangeId) return;
    let cancelled = false;
    setRawView({
      id: selected.exchangeId,
      reqText: "",
      respText: "",
      reqSize: selected.requestSize,
      respSize: selected.responseSize,
      reqSpilled: selected.requestSpilled,
      respSpilled: selected.responseSpilled,
      loading: true,
      error: "",
    });
    void querySrc<{
      request?: { text?: string; total_size?: number; spilled?: boolean; base64?: string } | null;
      response?: { text?: string; total_size?: number; spilled?: boolean; base64?: string } | null;
    }>(taskId, "src.traffic.read_bytes", { exchange_id: selected.exchangeId, part: "both" })
      .then((res) => {
        if (cancelled) return;
        const reqText = res.request?.text ?? (res.request?.base64 ? decodeBase64Utf8(res.request.base64) : "");
        const respText = res.response?.text ?? (res.response?.base64 ? decodeBase64Utf8(res.response.base64) : "");
        setRawView({
          id: selected.exchangeId,
          reqText,
          respText,
          reqSize: res.request?.total_size ?? selected.requestSize,
          respSize: res.response?.total_size ?? selected.responseSize,
          reqSpilled: Boolean(res.request?.spilled),
          respSpilled: Boolean(res.response?.spilled),
          loading: false,
          error: "",
        });
      })
      .catch((err) => {
        if (!cancelled) {
          setRawView((prev) => ({
            ...prev,
            loading: false,
            error: err instanceof Error ? err.message : String(err),
          }));
        }
      });
    return () => { cancelled = true; };
  }, [attached, selected?.exchangeId, selected?.requestSize, selected?.responseSize, selected?.requestSpilled, selected?.responseSpilled, taskId]);

  const sendToRepeater = () => {
    if (!selected) return;
    setDraftRaw(rawView.reqText || `GET ${selected.path} HTTP/1.1\r\nHost: ${selected.host}\r\n\r\n`);
    setDraftCtx(selected.contextId || "default");
    studio.setTrafficUi({ tab: "repeater", selectedExchangeId: selected.exchangeId });
  };

  const toggleBookmark = async (entry: TrafficIndexEntry) => {
    if (!attached) return;
    try {
      await postCommand(taskId, "src.proxy_bridge.bookmark", {
        exchange_id: entry.exchangeId,
        bookmarked: !entry.bookmarked,
        label: entry.bookmarkLabel || `${entry.method} ${entry.path}`,
      });
      await Promise.all([runSearch(), reloadAll()]);
    } catch (err) {
      toast({ title: "书签更新失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
    }
  };

  const copyRedactedCurl = async (exchangeId: string) => {
    if (!attached) return;
    try {
      const res = await querySrc<{ curl?: string }>(taskId, "src.exchange.curl_export", { exchange_id: exchangeId });
      if (res.curl) {
        await navigator.clipboard?.writeText(res.curl);
        toast({ title: "已复制脱敏 cURL", tone: "success" });
      }
    } catch (err) {
      toast({ title: "导出 cURL 失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
    }
  };

  const resolveDraftBase = async (exchangeId: string) => {
    const [policy, existingDrafts, exDetail] = await Promise.all([
      querySrc<{ version?: number }>(taskId, "src.policy.current", {}),
      recordListAll(taskId, "request_draft"),
      querySrc<{ record_revision?: number }>(taskId, "src.exchange.read", { exchange_id: exchangeId }),
    ]);
    const baseDraft = existingDrafts
      .filter((d) => d.source_exchange_id === exchangeId)
      .sort((a, b) => Number(b.revision) - Number(a.revision))[0];
    let baseRevision = Number(exDetail.record_revision ?? 1);
    if (baseDraft && typeof baseDraft.draft_id === "string") {
      const draftDoc = await querySrc<{ record_revision?: number; draft?: { revision?: number } }>(
        taskId,
        "src.exchange.draft.read",
        { draft_id: baseDraft.draft_id },
      );
      baseRevision = Number(draftDoc.record_revision ?? draftDoc.draft?.revision ?? baseDraft.revision ?? baseRevision);
    }
    return { policy, baseDraft, baseRevision };
  };

  const executeRepeater = async () => {
    if (!attached || !selected) return;
    setSending(true);
    try {
      const { policy, baseDraft, baseRevision } = await resolveDraftBase(selected.exchangeId);
      const b64 = btoa(unescape(encodeURIComponent(draftRaw)));
      const saved = await postCommand<{ draft?: { draft_id?: string; revision?: number }; draft_id?: string; revision?: number }>(taskId, "src.exchange.draft.save", {
        source_exchange_id: selected.exchangeId,
        base_revision: baseRevision,
        editor: "local-user",
        raw_request_b64: b64,
        ...(draftMutator ? { metadata: { mutator_id: draftMutator } } : {}),
        idempotency_key: `traffic-draft-${Date.now().toString(36)}`,
      });
      const draftId = saved.draft?.draft_id ?? saved.draft_id ?? (typeof baseDraft?.draft_id === "string" ? baseDraft.draft_id : undefined);
      const rev = saved.draft?.revision ?? saved.revision ?? 1;
      if (!draftId) throw new Error("草稿保存失败");
      const execOut = await postCommand<{ exchange_id?: string; execution_state?: string }>(taskId, "src.exchange.draft.execute", {
        draft_id: draftId,
        draft_revision: rev,
        authorization_version: Number(policy.version ?? 1),
        context_id: draftCtx || "default",
        identity_ref: draftCtx || "default",
        ...(draftMutator ? { mutator_id: draftMutator } : {}),
        idempotency_key: `traffic-exec-${Date.now().toString(36)}`,
      });
      if (execOut.exchange_id) {
        const baselineId = ui.compareBaselineId || selected.exchangeId;
        studio.setTrafficUi({ selectedExchangeId: execOut.exchange_id, compareBaselineId: baselineId });
        try {
          const diff = await querySrc<Record<string, unknown>>(taskId, "src.exchange.diff", {
            baseline_id: baselineId,
            experiment_id: execOut.exchange_id,
          });
          setDiffResult(diff);
        } catch (diffErr) {
          toast({ title: "重放成功但自动 Diff 失败", description: diffErr instanceof Error ? diffErr.message : String(diffErr), tone: "warning" });
        }
      }
      await runSearch();
      toast({ title: `重放已执行 (${execOut.execution_state ?? "done"})`, tone: "success" });
    } catch (err) {
      toast({ title: "重放执行失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
    } finally {
      setSending(false);
    }
  };

  const runDifferentialProbe = async () => {
    if (!attached || !selected) return;
    setSending(true);
    try {
      const variants = altCtx
        ? [{ label: altCtx, context_id: altCtx }, { label: "anonymous", drop_auth: true }]
        : [{ label: "anonymous", drop_auth: true }];
      const res = await postCommand<{ baseline_exchange_id?: string; variants?: Array<Record<string, unknown>> }>(
        taskId,
        "src.exchange.differential_probe",
        {
          source_exchange_id: selected.exchangeId,
          baseline_context_id: selected.contextId || "default",
          variants,
          idempotency_key: `traffic-diff-${Date.now().toString(36)}`,
        },
      );
      const firstVarId = String(res.variants?.[0]?.exchange_id ?? "");
      if (res.baseline_exchange_id && firstVarId) {
        try {
          const diff = await querySrc<Record<string, unknown>>(taskId, "src.exchange.diff", {
            baseline_id: res.baseline_exchange_id,
            experiment_id: firstVarId,
          });
          setDiffResult(diff);
        } catch (diffErr) {
          toast({ title: "对照探测成功但自动 Diff 失败", description: diffErr instanceof Error ? diffErr.message : String(diffErr), tone: "warning" });
        }
      }
      await runSearch();
      toast({ title: "双身份对照完成", tone: "success" });
    } catch (err) {
      toast({ title: "对照探测失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
    } finally {
      setSending(false);
    }
  };

  const runDiffCompare = async () => {
    if (!attached || !selected || !ui.compareBaselineId) return;
    try {
      const diff = await querySrc<Record<string, unknown>>(taskId, "src.exchange.diff", {
        baseline_id: ui.compareBaselineId,
        experiment_id: selected.exchangeId,
      });
      setDiffResult(diff);
    } catch (err) {
      toast({ title: "Diff 比对失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
    }
  };

  const importIdentitySession = async () => {
    if (!attached || !impCtx.trim()) return;
    try {
      await postCommand(taskId, "src.identity.session.import", {
        context_id: impCtx.trim(),
        domain: impDomain.trim() || undefined,
        raw_cookie_header: impCookie.trim() || undefined,
        headers: impAuth.trim() ? { Authorization: impAuth.trim() } : undefined,
        totp_secret: impTotp.trim() || undefined,
      });
      setImportOpen(false);
      setImpCookie("");
      setImpAuth("");
      setImpTotp("");
      await reloadAll();
      toast({ title: `已密封导入会话 ${impCtx.trim()}`, tone: "success" });
    } catch (err) {
      toast({ title: "导入会话失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
    }
  };

  const probeIdentity = async (ctxId: string) => {
    const url = (probeUrlMap[ctxId] ?? "").trim();
    if (!attached || !url) {
      toast({ title: "请先输入探活 URL", tone: "warning" });
      return;
    }
    try {
      const res = await postCommand<{ status?: string; last_probe_status_code?: number }>(taskId, "src.identity.session.probe", {
        context_id: ctxId,
        probe_url: url,
      });
      await reloadAll();
      toast({ title: `探活状态：${res.status ?? "ok"} (${res.last_probe_status_code ?? "—"})`, tone: "success" });
    } catch (err) {
      toast({ title: "探活失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
    }
  };

  const toggleProxyBridge = async () => {
    if (!attached) return;
    setBridgeBusy(true);
    try {
      if (state.proxyBridgeStatus?.listening) {
        await postCommand(taskId, "src.proxy_bridge.stop", {});
      } else {
        await postCommand(taskId, "src.proxy_bridge.start", {
          allow_lan: bridgeAllowLan,
          capture_context_id: bridgeCtx || undefined,
        });
      }
      await reloadAll();
    } catch (err) {
      toast({ title: "代理桥切换失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
    } finally {
      setBridgeBusy(false);
    }
  };

  const downloadProxyCa = async () => {
    if (!attached) return;
    try {
      const res = await querySrc<{ pem?: string; filename?: string }>(taskId, "src.proxy_bridge.ca_cert", {});
      if (!res.pem) return;
      const url = URL.createObjectURL(new Blob([res.pem], { type: "application/x-pem-file" }));
      const a = document.createElement("a");
      a.href = url;
      a.download = res.filename || "muteki-src-ca.pem";
      document.body.appendChild(a);
      a.click();
      a.remove();
      window.setTimeout(() => URL.revokeObjectURL(url), 0);
    } catch (err) {
      toast({ title: "导出 CA 证书失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
    }
  };

  const saveMutatorRecipe = async () => {
    if (!attached || !mutName.trim()) return;
    try {
      let secretRef = "";
      if (mutSecret.trim()) {
        const alias = `mut_${mutName.trim().replace(/[^a-zA-Z0-9_-]/g, "_").toLowerCase()}`;
        const cred = await postCommand<{ reference?: string }>(taskId, "src.credential.put", {
          alias,
          value: mutSecret.trim(),
        });
        secretRef = cred.reference ?? "";
      }
      await postCommand(taskId, "src.mutator.save", {
        name: mutName.trim(),
        kind: mutKind,
        algorithm: mutKind === "declarative" ? mutAlgo : undefined,
        secret_ref: secretRef || undefined,
        output_header: mutHeader.trim() || "X-Signature",
        js_source: mutKind === "sandbox_js" ? mutJs : undefined,
      });
      setMutOpen(false);
      setMutSecret("");
      await reloadAll();
      toast({ title: "已保存加签配方", tone: "success" });
    } catch (err) {
      toast({ title: "保存加签配方失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
    }
  };

  const testMutator = async (mutatorId: string) => {
    if (!attached) return;
    try {
      const res = await postCommand<Record<string, unknown>>(taskId, "src.mutator.test", {
        mutator_id: mutatorId,
        sample_method: selected?.method || "GET",
        sample_url: selected?.observedUrl || "https://example.invalid/api/v1/demo",
      });
      setMutTestOut(res);
    } catch (err) {
      toast({ title: "试算校验失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
    }
  };

  const contexts = state.identityContexts ?? [];
  const bridge = state.proxyBridgeStatus;
  const mutators = state.mutators ?? [];
  const checkedIds = useMemo(() => ui.checked ?? [], [ui.checked]);
  const checkedSet = useMemo(() => new Set(checkedIds), [checkedIds]);
  const toggleRowChecked = (exchangeId: string, nextChecked: boolean) => {
    const next = nextChecked
      ? [...new Set([...checkedIds, exchangeId])]
      : checkedIds.filter((id) => id !== exchangeId);
    studio.setTrafficUi({ checked: next });
  };
  const compareTwoSamples = async () => {
    if (!attached || checkedIds.length !== 2) return;
    const [baselineId, experimentId] = checkedIds;
    try {
      const diff = await querySrc<Record<string, unknown>>(taskId, "src.exchange.diff", {
        baseline_id: baselineId,
        experiment_id: experimentId,
      });
      setDiffResult(diff);
      studio.setTrafficUi({
        tab: "repeater",
        compareBaselineId: baselineId,
        selectedExchangeId: experimentId,
      });
    } catch (err) {
      toast({ title: "双样本对比失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
    }
  };
  const sendSamplesToChat = () => {
    if (!checkedIds.length) return;
    const refs: StudioRef[] = checkedIds.map((id) => ({ kind: "evidence", id }));
    if (state.activeThreadId) studio.addContextMany(state.activeThreadId, refs);
    askAi(null, "请结合已选中的流量样本分析请求与响应差异、鉴权与权限控制逻辑，并给出进一步验证建议。");
  };

  useEffect(() => {
    if (!active || !attached) return;
    if (ui.tab === "oast") void loadOastData();
    else if (ui.tab === "streams") void loadStreamsData();
    else if (ui.tab === "wire") void loadDiagnostics(ui.selectedExchangeId || wireExchangeId);
  }, [active, attached, loadDiagnostics, loadOastData, loadStreamsData, ui.selectedExchangeId, ui.tab, wireExchangeId]);

  return (
    <div className="flex h-full min-h-0 flex-col" data-testid="studio-traffic-workspace">
      <SurfaceToolbar>
        <div className="cx-scroll flex min-w-0 flex-1 overflow-x-auto">
          <SegmentedControl<TrafficTab>
            value={ui.tab}
            onChange={(tab) => studio.setTrafficUi({ tab })}
            size="sm"
            ariaLabel="流量工作台视图"
            options={[
              { value: "stream", label: "实时流" },
              { value: "repeater", label: "重放对比" },
              { value: "identities", label: "凭据池" },
              { value: "proxy", label: "微信桥" },
              { value: "proxypool", label: "代理池" },
              { value: "mutators", label: "加签" },
              { value: "oast", label: "OAST" },
              { value: "streams", label: "WS/SSE" },
              { value: "wire", label: "Wire/熔断" },
            ]}
          />
        </div>
        <div className="ml-auto flex items-center gap-1.5">
          {ui.tab === "stream" ? (
            <>
              <IconButton
                size="sm"
                icon="bookmark"
                label={ui.bookmarkedOnly ? "显示全部流量" : "仅看书签"}
                onClick={() => studio.setTrafficUi({ bookmarkedOnly: !ui.bookmarkedOnly })}
              />
              <IconButton size="sm" icon="refresh" label="刷新流量" onClick={() => { void runSearch(); }} />
            </>
          ) : null}
        </div>
      </SurfaceToolbar>

      {state.domainErrors?.traffic ? (
        <div className="mx-4 mt-3">
          <Callout
            tone="danger"
            title="加载流量域失败"
            role="alert"
            action={<Button size="sm" variant="secondary" icon="retry" onClick={() => { void studio.reloadDomain("traffic"); }}>重试</Button>}
          >
            {state.domainErrors.traffic}
          </Callout>
        </div>
      ) : null}

      {ui.tab === "stream" ? (
        <div className="flex min-h-0 flex-1 flex-col">
          <div className="flex flex-wrap items-center gap-2 border-b border-cx-border-subtle px-4 py-2">
            <SearchInput
              value={ui.query}
              onValueChange={(query) => studio.setTrafficUi({ query })}
              placeholder="Trigram 全文检索 URL / 报文体"
              aria-label="全文检索流量"
              size="sm"
              className="min-w-[180px] flex-1"
            />
            <Select<string>
              value={ui.method}
              onChange={(method) => studio.setTrafficUi({ method })}
              size="sm"
              ariaLabel="HTTP 方法"
              className="h-8 min-w-[90px] text-cx-xs"
              options={[
                { value: "all", label: "方法" },
                { value: "GET", label: "GET" },
                { value: "POST", label: "POST" },
                { value: "PUT", label: "PUT" },
                { value: "DELETE", label: "DELETE" },
                { value: "PATCH", label: "PATCH" },
              ]}
            />
            <Select<string>
              value={ui.statusCode}
              onChange={(statusCode) => studio.setTrafficUi({ statusCode })}
              size="sm"
              ariaLabel="状态码"
              className="h-8 min-w-[90px] text-cx-xs"
              options={[
                { value: "all", label: "状态" },
                { value: "200", label: "200" },
                { value: "302", label: "302" },
                { value: "401", label: "401" },
                { value: "403", label: "403" },
                { value: "500", label: "500" },
              ]}
            />
            {contexts.length ? (
              <Select<string>
                value={ui.contextId}
                onChange={(contextId) => studio.setTrafficUi({ contextId })}
                size="sm"
                ariaLabel="身份"
                className="h-8 min-w-[110px] text-cx-xs"
                options={[{ value: "all", label: "全部身份" }, ...contexts.map((c) => ({ value: c.contextId, label: c.contextId }))]}
              />
            ) : null}
          </div>

          {searchError ? (
            <div className="mx-4 mt-2">
              <Callout
                tone="danger"
                title="流量检索失败"
                role="alert"
                action={<Button size="sm" variant="secondary" icon="retry" onClick={() => { void runSearch(); }}>重试</Button>}
              >
                {searchError}
              </Callout>
            </div>
          ) : null}

          {checkedIds.length > 0 ? (
            <div className="flex flex-wrap items-center justify-between gap-2 border-b border-cx-border-subtle bg-cx-bg-subtle px-4 py-2" data-testid="traffic-multi-sample-bar">
              <span className="text-cx-xs font-medium text-cx-fg">已选 {checkedIds.length} 条流量样本</span>
              <div className="flex flex-wrap items-center gap-2">
                <Button size="sm" variant="primary" icon="send" onClick={sendSamplesToChat}>
                  带上主对话分析选中样本
                </Button>
                {checkedIds.length === 2 ? (
                  <Button size="sm" variant="secondary" icon="gitCompare" onClick={() => { void compareTwoSamples(); }}>
                    双样本对比
                  </Button>
                ) : null}
                <Button size="sm" variant="ghost" onClick={() => studio.setTrafficUi({ checked: [] })}>清除勾选</Button>
              </div>
            </div>
          ) : null}

          <div className="grid min-h-0 flex-1 grid-cols-2 divide-x divide-cx-border-subtle max-lg:grid-cols-1 max-lg:divide-x-0 max-lg:divide-y">
            <div ref={scroll.ref} className="cx-scroll flex min-h-0 flex-col overflow-y-auto p-3">
              {entries.length ? (
                <>
                  <div className="min-h-0 flex-1">
                    <VirtualRecordList
                      items={entries}
                      scrollNode={scroll.element ?? scroll.node.current}
                      estimateSize={56}
                      ariaLabel="HTTP 流量列表"
                      renderItem={(entry) => {
                        const rowChecked = checkedSet.has(entry.exchangeId);
                        return (
                          <SurfaceRow
                            key={entry.exchangeId}
                            icon={entry.bookmarked ? "bookmark" : "network"}
                            leading={(
                              <span
                                className="grid size-8 shrink-0 place-items-center"
                                onClick={(event) => event.stopPropagation()}
                              >
                                <Checkbox
                                  checked={rowChecked}
                                  aria-label={`勾选样本 ${entry.method} ${entry.path}`}
                                  onCheckedChange={(next) => toggleRowChecked(entry.exchangeId, Boolean(next))}
                                  className="[&>button]:size-5"
                                />
                              </span>
                            )}
                            selected={rowChecked || selected?.exchangeId === entry.exchangeId}
                            title={`${entry.method} ${entry.path}`}
                            subtitle={[
                              entry.host,
                              entry.statusCode != null ? String(entry.statusCode) : "—",
                              entry.contextId,
                              `${entry.responseSize}B`,
                              relativeTime(entry.startedAt) || entry.startedAt,
                            ].filter(Boolean).join(" · ")}
                            trailing={entry.responseSpilled ? <StudioBadge tone="warning">CAS</StudioBadge> : undefined}
                            onClick={() => studio.setTrafficUi({ selectedExchangeId: entry.exchangeId })}
                          />
                        );
                      }}
                    />
                  </div>
                  <div className="mt-2 flex items-center justify-between gap-2 border-t border-cx-border-subtle pt-2 text-cx-xs text-cx-fg-3">
                    <span>已加载 {entries.length} 条记录</span>
                    {nextCursor ? (
                      <Button
                        size="sm"
                        variant="secondary"
                        disabled={loadingMore}
                        onClick={() => { void runSearch({ appendCursor: nextCursor }); }}
                      >
                        {loadingMore ? "加载中…" : "继续读取下一页"}
                      </Button>
                    ) : null}
                  </div>
                </>
              ) : (
                <EmptyState compact icon="network" title={searching ? "检索中…" : "暂无捕获流量"} />
              )}
            </div>

            <div className="cx-scroll min-h-0 overflow-y-auto p-4">
              {selected ? (
                <div className="flex flex-col gap-4">
                  <SurfaceCard className="p-3">
                    <div className="flex flex-wrap items-center justify-between gap-2">
                      <span className="truncate font-cx-mono text-cx-sm font-semibold text-cx-fg">{selected.observedUrl}</span>
                      <div className="flex items-center gap-1.5">
                        <Button size="sm" variant="primary" onClick={sendToRepeater}>送入重放</Button>
                        <Button size="sm" variant="secondary" onClick={() => studio.setTrafficUi({ compareBaselineId: selected.exchangeId })}>设为基线</Button>
                        <IconButton size="sm" icon="copy" label="复制脱敏 cURL" onClick={() => { void copyRedactedCurl(selected.exchangeId); }} />
                        <IconButton size="sm" icon="bookmark" label="切换书签" onClick={() => { void toggleBookmark(selected); }} />
                      </div>
                    </div>
                    <Dl className="mt-2" items={[
                      { label: "编号", value: selected.exchangeId, mono: true, copy: selected.exchangeId },
                      { label: "方法 / 状态", value: `${selected.method} · ${selected.statusCode ?? "—"} · ${selected.protocol}` },
                      { label: "身份上下文", value: selected.contextId, mono: true },
                      { label: "SHA-256", value: selected.responseSha256 || selected.requestSha256, mono: true },
                    ]} />
                  </SurfaceCard>

                  {rawView.error ? <p className="text-cx-xs text-cx-danger">{rawView.error}</p> : null}

                  <div>
                    <SectionHeader title={`请求报文 (${rawView.reqSize} B${rawView.reqSpilled ? " · CAS" : ""})`} />
                    <CodeBlock code={rawView.loading ? "读取中…" : rawView.reqText || "无"} language="http" className="max-h-[260px]" />
                  </div>
                  <div>
                    <SectionHeader title={`响应报文 (${rawView.respSize} B${rawView.respSpilled ? " · CAS" : ""})`} />
                    <CodeBlock code={rawView.loading ? "读取中…" : rawView.respText || "无"} language="http" className="max-h-[320px]" />
                  </div>
                </div>
              ) : (
                <EmptyState compact icon="network" title="选择一条流量查看原始报文" />
              )}
            </div>
          </div>
        </div>
      ) : null}

      {ui.tab === "repeater" ? (
        <div className="cx-scroll min-h-0 flex-1 overflow-y-auto p-4">
          <div className="mx-auto flex max-w-[880px] flex-col gap-4">
            <SurfaceCard className="p-3">
              <div className="grid grid-cols-3 gap-2 max-sm:grid-cols-1">
                <div className="flex flex-col">
                  <Label>执行身份</Label>
                  <Select<string>
                    value={draftCtx}
                    onChange={setDraftCtx}
                    ariaLabel="执行身份"
                    options={[
                      { value: "default", label: "default" },
                      { value: "anonymous", label: "anonymous" },
                      ...contexts.map((c) => ({ value: c.contextId, label: c.contextId })),
                    ]}
                  />
                </div>
                <div className="flex flex-col">
                  <Label>对照身份（差分）</Label>
                  <Select<string>
                    value={altCtx}
                    onChange={setAltCtx}
                    ariaLabel="对照身份"
                    options={[{ value: "", label: "仅匿名对照" }, ...contexts.map((c) => ({ value: c.contextId, label: c.contextId }))]}
                  />
                </div>
                <div className="flex flex-col">
                  <Label>请求加签器</Label>
                  <Select<string>
                    value={draftMutator}
                    onChange={setDraftMutator}
                    ariaLabel="请求加签器"
                    options={[{ value: "", label: "不使用" }, ...mutators.map((m) => ({ value: m.mutatorId, label: m.name }))]}
                  />
                </div>
              </div>
              <div className="mt-3">
                <TextArea label="原始 HTTP 请求草稿" value={draftRaw} onChange={(e) => setDraftRaw(e.target.value)} className="min-h-[180px] font-cx-mono text-cx-xs" />
              </div>
              <div className="mt-3 flex flex-wrap items-center gap-2">
                <Button size="sm" variant="primary" icon="play" disabled={!selected || sending} onClick={() => { void executeRepeater(); }}>发送重放</Button>
                <Button size="sm" variant="secondary" icon="gitCompare" disabled={!selected || sending} onClick={() => { void runDifferentialProbe(); }}>双身份差分</Button>
                <Button size="sm" variant="ghost" disabled={!selected || !ui.compareBaselineId} onClick={() => { void runDiffCompare(); }}>对比基线（{ui.compareBaselineId || "未设"}）</Button>
                <span className="ml-auto flex flex-wrap items-center gap-1.5">
                  <Input
                    size="sm"
                    value={raceConcurrency}
                    onChange={(e) => setRaceConcurrency(e.target.value)}
                    aria-label="并发数"
                    placeholder="N<=20"
                    className="w-[72px]"
                  />
                  <Button
                    size="md"
                    variant={raceApprovedOnce ? "primary" : "secondary"}
                    onClick={() => setRaceApprovedOnce((v) => !v)}
                  >
                    {raceApprovedOnce ? "已一次性人工批准" : "确认单次并发批准"}
                  </Button>
                  <Button
                    size="md"
                    variant="secondary"
                    disabled={!selected || sending || !raceApprovedOnce}
                    onClick={async () => {
                      if (!attached || !selected) return;
                      setSending(true);
                      setRaceApprovedOnce(false);
                      try {
                        const n = Math.min(20, Math.max(2, Number(raceConcurrency) || 5));
                        const { policy, baseDraft, baseRevision } = await resolveDraftBase(selected.exchangeId);
                        const b64 = btoa(unescape(encodeURIComponent(draftRaw)));
                        const saved = await postCommand<{ draft?: { draft_id?: string; revision?: number }; draft_id?: string; revision?: number }>(taskId, "src.exchange.draft.save", {
                          source_exchange_id: selected.exchangeId,
                          base_revision: baseRevision,
                          editor: "local-user",
                          raw_request_b64: b64,
                          idempotency_key: `race-draft-${Date.now().toString(36)}`,
                        });
                        const draftId = saved.draft?.draft_id ?? saved.draft_id ?? (typeof baseDraft?.draft_id === "string" ? baseDraft.draft_id : undefined);
                        const rev = saved.draft?.revision ?? saved.revision ?? 1;
                        if (!draftId) throw new Error("草稿保存失败");
                        const out = await postCommand<Record<string, unknown>>(taskId, "src.exchange.race_probe", {
                          draft_id: draftId,
                          draft_revision: rev,
                          authorization_version: Number(policy.version ?? 1),
                          concurrency: n,
                          operator_approved_once: true,
                          context_id: draftCtx || "default",
                          ...(draftMutator ? { mutator_id: draftMutator } : {}),
                          idempotency_key: `race-once-${Date.now().toString(36)}`,
                        });
                        setRaceApprovedOnce(false);
                        setRaceResult(out);
                        await reloadAll();
                        toast({ title: `竞态探测完成 (${n} 并发)`, tone: "success" });
                      } catch (err) {
                        toast({ title: "竞态探测失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
                      } finally {
                        setSending(false);
                      }
                    }}
                  >
                    并发探测 (N&lt;=20)
                  </Button>
                </span>
              </div>
            </SurfaceCard>

            {raceResult ? (
              <SurfaceCard className="p-3">
                <SectionHeader title={`竞态探测结果 (${String(raceResult.concurrency ?? "")} 并发 · ${raceResult.responses_diverged ? "状态/响应存在分歧" : "全部一致"})`} />
                <CodeBlock code={JSON.stringify(raceResult.status_histogram ?? raceResult, null, 2)} language="json" className="max-h-[200px]" />
              </SurfaceCard>
            ) : null}

            {diffResult ? (
              <div className="flex flex-col gap-3">
                {diffResult.structural_json && typeof diffResult.structural_json === "object" ? (
                  <SurfaceCard className="p-3">
                    <SectionHeader title="结构化 JSON Diff" />
                    <CodeBlock code={JSON.stringify(diffResult.structural_json, null, 2)} language="json" className="max-h-[220px]" />
                  </SurfaceCard>
                ) : null}
                <div>
                  <SectionHeader title="响应行级 Diff" />
                  <CodeBlock
                    code={String(((diffResult.raw as Record<string, unknown> | undefined)?.response as Record<string, unknown> | undefined)?.unified_diff || "完全一致")}
                    language="diff"
                    className="max-h-[280px]"
                  />
                </div>
              </div>
            ) : null}
          </div>
        </div>
      ) : null}

      {ui.tab === "identities" ? (
        <div className="cx-scroll min-h-0 flex-1 overflow-y-auto p-4">
          <div className="mx-auto flex max-w-[820px] flex-col gap-4">
            <div className="flex items-center justify-between">
              <SectionHeader title="身份上下文与 Session Jar" count={contexts.length} />
              <Button size="sm" variant="primary" icon="plus" onClick={() => setImportOpen(true)}>导入 Cookie / Token</Button>
            </div>
            {contexts.length ? (
              <div className="flex flex-col gap-3">
                {contexts.map((c: IdentityContextSummary) => {
                  const eff = c.effectiveStatus || c.status || "unverified";
                  const effTone = eff === "verified" || eff === "active"
                    ? "success"
                    : eff === "cookies_expired" || eff === "auth_failed" || eff === "expired"
                      ? "danger"
                      : "warning";
                  const effLabel = eff === "verified"
                    ? "已实时验证 (verified)"
                    : eff === "probe_2xx_cached"
                      ? "缓存探活有效 (probe_2xx_cached)"
                      : eff === "cookies_expired"
                        ? "Cookie 已过期 (cookies_expired)"
                        : eff === "auth_failed"
                          ? "鉴权失败 (auth_failed)"
                          : eff === "unverified"
                            ? "未验证 (unverified)"
                            : eff;
                  const probeAt = c.lastProbeAt || c.lastVerifiedAt;
                  return (
                    <SurfaceCard key={c.contextId} className="p-3">
                      <div className="flex flex-wrap items-center justify-between gap-2">
                        <span className="font-cx-mono text-cx-base font-semibold text-cx-fg">{c.contextId}</span>
                        <div className="flex flex-wrap items-center gap-1.5">
                          {c.totpConfigured ? <StudioBadge tone="accent">TOTP</StudioBadge> : null}
                          {(c.expiredCookieCount ?? 0) > 0 ? (
                            <StudioBadge tone="danger">过期 Cookie {c.expiredCookieCount} 个</StudioBadge>
                          ) : null}
                          <StudioBadge tone={effTone}>{effLabel}</StudioBadge>
                        </div>
                      </div>
                      <Dl className="mt-2" items={[
                        { label: "凭据引用", value: c.credentialRef || "—", mono: true },
                        { label: "会话指纹", value: c.sessionFingerprint || "—", mono: true },
                        { label: "Cookies / Headers", value: `${c.cookieCount} cookies${c.expiredCookieCount ? `（${c.expiredCookieCount} 已过期）` : ""} · ${c.headerNames.join(", ") || "none"}` },
                        { label: "最近探活", value: probeAt ? `${probeAt} (${c.lastProbeStatusCode ?? "—"})` : "尚未探活" },
                      ]} />
                      <div className="mt-3 flex items-center gap-2">
                        <Input
                          size="sm"
                          value={probeUrlMap[c.contextId] ?? ""}
                          onChange={(e) => setProbeUrlMap((prev) => ({ ...prev, [c.contextId]: e.target.value }))}
                          placeholder="https://target.example/api/me"
                          aria-label={`探活 URL ${c.contextId}`}
                          className="flex-1"
                        />
                        <Button size="sm" variant="secondary" icon="refresh" onClick={() => { void probeIdentity(c.contextId); }}>探活</Button>
                      </div>
                    </SurfaceCard>
                  );
                })}
              </div>
            ) : (
              <EmptyState compact icon="lock" title="暂无身份上下文" />
            )}
          </div>
        </div>
      ) : null}

      {ui.tab === "proxy" ? (
        <div className="cx-scroll min-h-0 flex-1 overflow-y-auto p-4">
          <div className="mx-auto flex max-w-[820px] flex-col gap-4">
            <SurfaceCard className="p-3">
              <div className="flex flex-wrap items-center justify-between gap-2">
                <div className="flex items-center gap-2">
                  <span className="text-cx-base font-semibold text-cx-fg">客户端实机抓包代理桥</span>
                  <StudioBadge tone={bridge?.listening ? "success" : "neutral"}>{bridge?.listening ? "监听中" : "已停止"}</StudioBadge>
                </div>
                <div className="flex items-center gap-2">
                  <Button size="sm" variant="secondary" icon="download" onClick={() => { void downloadProxyCa(); }}>导出 CA 证书</Button>
                  <Button size="sm" variant={bridge?.listening ? "danger" : "primary"} disabled={bridgeBusy} onClick={() => { void toggleProxyBridge(); }}>
                    {bridge?.listening ? "停止代理" : "启动代理"}
                  </Button>
                </div>
              </div>
              <div className="mt-3 grid grid-cols-2 gap-2 max-sm:grid-cols-1">
                <Select<string>
                  size="sm"
                  value={bridgeCtx}
                  onChange={setBridgeCtx}
                  ariaLabel="自动收割目标身份"
                  className="h-8 text-cx-sm"
                  options={[{ value: "", label: "不绑定自动收割身份" }, ...contexts.map((c) => ({ value: c.contextId, label: `收割至 ${c.contextId}` }))]}
                />
                <Button size="md" variant={bridgeAllowLan ? "secondary" : "ghost"} onClick={() => setBridgeAllowLan((v) => !v)}>
                  {bridgeAllowLan ? "0.0.0.0 (LAN)" : "127.0.0.1 (本机 / Mac 微信)"}
                </Button>
              </div>
              <Dl className="mt-3" items={[
                { label: "监听地址", value: bridge?.listening ? `${bridge.host}:${bridge.port}` : "未启动", mono: true, copy: bridge?.listening ? `${bridge.host}:${bridge.port}` : undefined },
                { label: "代理地址（含认证）", value: bridge?.proxyUrl || "—", mono: true, copy: bridge?.proxyUrl || undefined },
                { label: "CA SHA-256", value: bridge?.caFingerprintSha256 || "—", mono: true, copy: bridge?.caFingerprintSha256 || undefined },
                { label: "范围命中 / 范围外盲转", value: `${bridge?.inScopeCount ?? 0} / ${bridge?.bypassedOutOfScopeCount ?? 0}` },
              ]} />
            </SurfaceCard>

            <div>
              <SectionHeader title="书签流" count={bridge?.bookmarks.length ?? 0} />
              <SurfaceCard>
                {bridge?.bookmarks.length ? bridge.bookmarks.map((b) => (
                  <SurfaceRow
                    key={b.exchangeId}
                    icon="bookmark"
                    title={b.label || b.exchangeId}
                    subtitle={b.updatedAt}
                    onClick={() => studio.setTrafficUi({ tab: "stream", selectedExchangeId: b.exchangeId })}
                  />
                )) : <SurfaceRow icon="bookmark" title="暂无书签" />}
              </SurfaceCard>
            </div>
          </div>
        </div>
      ) : null}

      {ui.tab === "proxypool" ? (
        <div className="cx-scroll min-h-0 flex-1 overflow-y-auto p-4" data-testid="studio-proxypool-panel">
          <div className="mx-auto flex max-w-[820px] flex-col gap-4">
            {poolError ? (
              <Callout
                tone="danger"
                title="读取代理池状态失败"
                role="alert"
                action={<Button size="sm" variant="secondary" icon="retry" onClick={() => { void loadPoolStatus(); }}>重试</Button>}
              >
                {poolError}
              </Callout>
            ) : null}
            <SurfaceCard className="p-4">
              <SectionHeader title="上游代理池配置" />
              <div className="mt-3 flex flex-wrap items-center gap-3">
                <Select<"failover" | "round_robin">
                  value={poolSelection}
                  onChange={setPoolSelection}
                  size="sm"
                  ariaLabel="调度策略"
                  className="h-8 min-w-[140px]"
                  options={[
                    { value: "failover", label: "主备故障转移" },
                    { value: "round_robin", label: "轮询负载" },
                  ]}
                />
                <Button
                  size="sm"
                  variant={poolEnabled ? "primary" : "secondary"}
                  onClick={() => setPoolEnabled((v) => !v)}
                >
                  {poolEnabled ? "已启用代理池" : "代理池未启用"}
                </Button>
                <Button
                  size="sm"
                  variant={poolFallback ? "secondary" : "ghost"}
                  onClick={() => setPoolFallback((v) => !v)}
                >
                  {poolFallback ? "熔断后允许直连" : "严格经由代理"}
                </Button>
                <Button
                  size="sm"
                  variant="secondary"
                  icon="refresh"
                  disabled={!attached || poolBusy}
                  onClick={async () => {
                    setPoolBusy(true);
                    try {
                      await postCommand(taskId, "src.proxypool.reset_circuit", {});
                      await loadPoolStatus();
                      toast({ title: "已重置全部代理熔断器", tone: "success" });
                    } catch (err) {
                      toast({ title: "重置失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
                    } finally {
                      setPoolBusy(false);
                    }
                  }}
                >
                  重置全部熔断
                </Button>
              </div>
              <div className="mt-3 grid grid-cols-5 items-end gap-2 max-sm:grid-cols-1">
                <div className="flex flex-col">
                  <Label>代理协议</Label>
                  <Select<"http" | "https" | "socks5" | "socks5h">
                    value={poolScheme}
                    onChange={setPoolScheme}
                    ariaLabel="代理协议"
                    options={[
                      { value: "http", label: "HTTP" },
                      { value: "https", label: "HTTPS" },
                      { value: "socks5", label: "SOCKS5" },
                      { value: "socks5h", label: "SOCKS5H" },
                    ]}
                  />
                </div>
                <TextField label="代理主机" value={poolHost} onChange={(e) => setPoolHost(e.target.value)} placeholder="127.0.0.1" />
                <TextField label="端口" value={poolPort} onChange={(e) => setPoolPort(e.target.value)} placeholder="8080" />
                <TextField label="用户名" value={poolUser} onChange={(e) => setPoolUser(e.target.value)} placeholder="可选" />
                <TextField label="密码（密文托管）" type="password" value={poolPass} onChange={(e) => setPoolPass(e.target.value)} placeholder="可选" />
              </div>
              <div className="mt-3 flex justify-end gap-2">
                <Button
                  size="sm"
                  variant="primary"
                  disabled={!attached || poolBusy}
                  onClick={async () => {
                    setPoolBusy(true);
                    try {
                      const existing = Array.isArray(poolStatus?.nodes) ? (poolStatus.nodes as Array<Record<string, unknown>>) : [];
                      const nodes = existing.map((n) => ({
                        node_id: n.node_id,
                        label: n.label,
                        scheme: n.scheme,
                        host: n.host,
                        port: n.port,
                        username: n.username || "",
                        enabled: n.enabled !== false,
                        priority: n.priority ?? 1,
                        ...(typeof n.credential_ref === "string" && n.credential_ref ? { credential_ref: n.credential_ref } : {}),
                      }));
                      if (poolHost.trim()) {
                        const portNum = Number(poolPort) || 8080;
                        nodes.push({
                          node_id: undefined,
                          label: `${poolScheme}://${poolHost.trim()}:${portNum}`,
                          scheme: poolScheme,
                          host: poolHost.trim(),
                          port: portNum,
                          username: poolUser.trim(),
                          enabled: true,
                          priority: nodes.length + 1,
                          ...(poolPass.trim() ? { password: poolPass.trim() } : {}),
                        } as unknown as (typeof nodes)[number]);
                      }
                      const res = await postCommand<Record<string, unknown>>(taskId, "src.proxypool.save", {
                        enabled: poolEnabled,
                        selection: poolSelection,
                        fallback_direct: poolFallback,
                        nodes,
                      });
                      setPoolStatus(res);
                      setPoolHost("");
                      setPoolPass("");
                      toast({ title: "代理池配置已保存", tone: "success" });
                    } catch (err) {
                      toast({ title: "保存失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
                    } finally {
                      setPoolBusy(false);
                    }
                  }}
                >
                  保存配置 / 添加节点
                </Button>
              </div>
            </SurfaceCard>

            <div>
              <SectionHeader title="代理节点与熔断状态" count={Array.isArray(poolStatus?.nodes) ? poolStatus.nodes.length : 0} />
              <SurfaceCard>
                {Array.isArray(poolStatus?.nodes) && poolStatus.nodes.length ? (
                  (poolStatus.nodes as Array<Record<string, unknown>>).map((n) => (
                    <SurfaceRow
                      key={String(n.node_id)}
                      icon="globe"
                      title={String(n.label || `${n.scheme}://${n.host}:${n.port}`)}
                      subtitle={`失败 ${String(n.consecutive_failures ?? 0)} / 5 · 成功 ${String(n.success_count ?? 0)}${Number(n.cooldown_remaining_secs ?? 0) > 0 ? ` · 冷却 ${String(n.cooldown_remaining_secs)}s` : ""}`}
                      trailing={(
                        <StudioBadge tone={n.circuit_state === "open" ? "danger" : n.circuit_state === "half_open" ? "warning" : "success"}>
                          {String(n.circuit_state || "closed")}
                        </StudioBadge>
                      )}
                    />
                  ))
                ) : (
                  <SurfaceRow icon="globe" title="暂无上游代理节点" />
                )}
              </SurfaceCard>
            </div>
          </div>
        </div>
      ) : null}

      {ui.tab === "mutators" ? (
        <div className="cx-scroll min-h-0 flex-1 overflow-y-auto p-4">
          <div className="mx-auto flex max-w-[820px] flex-col gap-4">
            <div className="flex items-center justify-between">
              <SectionHeader title="请求加签配方" count={mutators.length} />
              <Button size="sm" variant="primary" icon="plus" onClick={() => setMutOpen(true)}>新建加签器</Button>
            </div>
            <div className="flex flex-col gap-3">
              {mutators.map((m: RequestMutatorSummary) => (
                <SurfaceCard key={m.mutatorId} className="p-3">
                  <div className="flex items-center justify-between gap-2">
                    <span className="text-cx-base font-semibold text-cx-fg">{m.name}</span>
                    <div className="flex items-center gap-2">
                      <StudioBadge tone="accent">{m.kind === "sandbox_js" ? "sandbox_js" : m.algorithm}</StudioBadge>
                      <Button size="sm" variant="secondary" icon="play" onClick={() => { void testMutator(m.mutatorId); }}>试算校验</Button>
                    </div>
                  </div>
                  <Dl className="mt-2" items={[
                    { label: "编号", value: m.mutatorId, mono: true },
                    { label: "签名头", value: m.outputHeader || m.outputQuery || "—", mono: true },
                    { label: "密钥引用", value: m.secretRef || "无", mono: true },
                  ]} />
                </SurfaceCard>
              ))}
            </div>
            {mutTestOut ? (
              <SurfaceCard className="p-3">
                <SectionHeader title="干跑试算结果" />
                <CodeBlock code={JSON.stringify(mutTestOut, null, 2)} language="json" className="max-h-[220px]" />
              </SurfaceCard>
            ) : null}
          </div>
        </div>
      ) : null}

      {ui.tab === "oast" ? (
        <div className="cx-scroll min-h-0 flex-1 overflow-y-auto p-4" data-testid="studio-traffic-oast">
          <div className="mx-auto flex max-w-[820px] flex-col gap-4">
            {oastError ? (
              <Callout
                tone="danger"
                title="读取 OAST 令牌/事件失败"
                role="alert"
                action={<Button size="sm" variant="secondary" icon="retry" onClick={() => { void loadOastData(); }}>重试</Button>}
              >
                {oastError}
              </Callout>
            ) : null}
            <SurfaceCard className="p-3">
              <div className="flex flex-wrap items-center gap-2">
                <Input size="sm" value={oastNote} onChange={(e) => setOastNote(e.target.value)} placeholder="OAST 令牌备注" className="min-w-[180px] flex-1" />
                <Button
                  size="sm"
                  variant="primary"
                  disabled={!attached || oastBusy}
                  onClick={async () => {
                    setOastBusy(true);
                    try {
                      await postCommand(taskId, "src.oast.token.mint", {
                        note: oastNote.trim() || "studio-oast",
                        protocols: ["http", "dns"],
                        ...(selected?.exchangeId ? { exchange_id: selected.exchangeId } : {}),
                      });
                      setOastNote("");
                      await loadOastData();
                      toast({ title: "已签发 HMAC OAST 令牌", tone: "success" });
                    } catch (err) {
                      toast({ title: "签发失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
                    } finally {
                      setOastBusy(false);
                    }
                  }}
                >
                  签发令牌
                </Button>
                <Button
                  size="md"
                  variant="secondary"
                  disabled={!attached || oastBusy}
                  onClick={async () => {
                    setOastBusy(true);
                    try {
                      await postCommand(taskId, "src.oast.local.start", {});
                      toast({ title: "本地 OAST 监听器已就绪", tone: "success" });
                    } catch (err) {
                      toast({ title: "启动失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
                    } finally {
                      setOastBusy(false);
                    }
                  }}
                >
                  本地监听
                </Button>
                <Button
                  size="md"
                  variant="ghost"
                  icon="refresh"
                  disabled={!attached || oastBusy}
                  onClick={async () => {
                    setOastBusy(true);
                    setOastError("");
                    try {
                      if (selectedInteractionId) {
                        await postCommand(taskId, "src.oast.poll", { interaction_id: selectedInteractionId });
                      }
                      await loadOastData();
                    } catch (err) {
                      const msg = err instanceof Error ? err.message : String(err);
                      setOastError(msg);
                      toast({ title: "OAST 轮询失败", description: msg, tone: "danger" });
                    } finally {
                      setOastBusy(false);
                    }
                  }}
                >
                  轮询
                </Button>
              </div>
            </SurfaceCard>
            <div>
              <SectionHeader title="已签发 OAST 令牌" count={oastInteractions.length} />
              <SurfaceCard>
                {oastInteractions.length ? oastInteractions.map((row) => {
                  const id = String(row.natural_key || row.record_id || "");
                  const p = (row.payload as Record<string, unknown>) ?? {};
                  return (
                    <SurfaceRow
                      key={id}
                      icon="globe"
                      selected={selectedInteractionId === id}
                      title={`${String(p.token || id)}`}
                      subtitle={`${String(p.callback_url || "")} · hits=${String(p.next_seq ?? 0)}`}
                      trailing={<StudioBadge tone={p.state === "active" ? "success" : "neutral"}>{String(p.state || "active")}</StudioBadge>}
                      onClick={() => setSelectedInteractionId(id)}
                    />
                  );
                }) : <SurfaceRow icon="globe" title="暂无 OAST 令牌" />}
              </SurfaceCard>
            </div>
            <div>
              <SectionHeader title="带外回连事件" count={oastEvents.length} />
              <SurfaceCard>
                {oastEvents.length ? oastEvents.map((row, idx) => {
                  const p = (row.payload as Record<string, unknown>) ?? {};
                  return (
                    <SurfaceRow
                      key={String(p.event_id || idx)}
                      icon="activity"
                      title={`${String(p.method || p.protocol || "HTTP")} ${String(p.path || p.request_path || "")}`}
                      subtitle={`${String(p.interaction_id || "")} · ${String(p.source_address || "")} · ${String(p.received_at || "")}`}
                    />
                  );
                }) : <SurfaceRow icon="activity" title="暂无带外回连事件" />}
              </SurfaceCard>
            </div>
          </div>
        </div>
      ) : null}

      {ui.tab === "streams" ? (
        <div className="cx-scroll min-h-0 flex-1 overflow-y-auto p-4" data-testid="studio-traffic-streams">
          <div className="mx-auto flex max-w-[820px] flex-col gap-4">
            {streamError ? (
              <Callout
                tone="danger"
                title="读取 WS/SSE 流失败"
                role="alert"
                action={<Button size="sm" variant="secondary" icon="retry" onClick={() => { void loadStreamsData(); }}>重试</Button>}
              >
                {streamError}
              </Callout>
            ) : null}
            <SurfaceCard className="p-3">
              <div className="flex flex-wrap items-center gap-2">
                <Select<"websocket" | "sse">
                  value={streamProto}
                  onChange={setStreamProto}
                  size="sm"
                  ariaLabel="流协议"
                  className="h-8 min-w-[110px]"
                  options={[{ value: "websocket", label: "WebSocket" }, { value: "sse", label: "SSE" }]}
                />
                <Input size="sm" value={streamUrl} onChange={(e) => setStreamUrl(e.target.value)} placeholder="ws://127.0.0.1:51061/ws 或 http://.../events" className="min-w-[200px] flex-1" />
                <Input size="sm" value={streamCtx} onChange={(e) => setStreamCtx(e.target.value)} placeholder="context_id" className="w-28" />
                <Button
                  size="sm"
                  variant="primary"
                  disabled={!attached || streamBusy || !streamUrl.trim()}
                  onClick={async () => {
                    setStreamBusy(true);
                    try {
                      const pol = await querySrc<{ policy_id?: string; version?: number }>(taskId, "src.policy.current", {});
                      const authRef = `${pol.policy_id || "src-policy"}:v${pol.version ?? 1}`;
                      const cmd = streamProto === "websocket" ? "src.stream.websocket.connect" : "src.stream.sse.connect";
                      const out = await postCommand<Record<string, unknown>>(taskId, cmd, {
                        url: streamUrl.trim(),
                        authorization_ref: authRef,
                        identity_ref: streamCtx.trim() || "default",
                        idempotency_key: randomUuid(),
                      });
                      const sid = String(out.stream_id || "");
                      await loadStreamsData(sid);
                      toast({ title: `流已连接 ${sid}`, tone: "success" });
                    } catch (err) {
                      toast({ title: "连接流失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
                    } finally {
                      setStreamBusy(false);
                    }
                  }}
                >
                  连接
                </Button>
                <Button size="md" variant="ghost" icon="refresh" disabled={!attached || streamBusy} onClick={() => { void loadStreamsData(); }}>刷新</Button>
              </div>
              {selectedStreamId ? (
                <div className="mt-2.5 flex flex-wrap items-center gap-2 border-t border-cx-border-subtle pt-2.5">
                  <Input size="sm" value={streamSendText} onChange={(e) => setStreamSendText(e.target.value)} placeholder="发送 WS 帧内容" className="min-w-[180px] flex-1" />
                  <Button
                    size="sm"
                    variant="secondary"
                    disabled={!attached || streamBusy || !streamSendText.trim()}
                    onClick={async () => {
                      setStreamBusy(true);
                      try {
                        await postCommand(taskId, "src.stream.websocket.send", { stream_id: selectedStreamId, payload: streamSendText });
                        setStreamSendText("");
                        await loadStreamsData(selectedStreamId);
                      } catch (err) {
                        toast({ title: "发送失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
                      } finally {
                        setStreamBusy(false);
                      }
                    }}
                  >
                    发送帧
                  </Button>
                  <Button
                    size="sm"
                    variant="secondary"
                    disabled={!attached || streamBusy}
                    onClick={async () => {
                      setStreamBusy(true);
                      try {
                        const activeStream = streams.find((s) => String(s.natural_key || s.record_id) === selectedStreamId);
                        const isSse = (activeStream?.payload as Record<string, unknown> | undefined)?.protocol === "sse";
                        await postCommand(taskId, isSse ? "src.stream.sse.read" : "src.stream.websocket.receive", { stream_id: selectedStreamId });
                        await loadStreamsData(selectedStreamId);
                      } catch (err) {
                        toast({ title: "读取失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
                      } finally {
                        setStreamBusy(false);
                      }
                    }}
                  >
                    读取帧
                  </Button>
                  <Button
                    size="sm"
                    variant="ghost"
                    disabled={!attached || streamBusy}
                    onClick={async () => {
                      setStreamBusy(true);
                      try {
                        const activeStream = streams.find((s) => String(s.natural_key || s.record_id) === selectedStreamId);
                        const isSse = (activeStream?.payload as Record<string, unknown> | undefined)?.protocol === "sse";
                        await postCommand(taskId, isSse ? "src.stream.sse.close" : "src.stream.websocket.close", { stream_id: selectedStreamId });
                        await loadStreamsData(selectedStreamId);
                      } catch (err) {
                        toast({ title: "关闭失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
                      } finally {
                        setStreamBusy(false);
                      }
                    }}
                  >
                    关闭流
                  </Button>
                </div>
              ) : null}
            </SurfaceCard>
            <div>
              <SectionHeader title="会话列表" count={streams.length} />
              <SurfaceCard>
                {streams.length ? streams.map((row) => {
                  const sid = String(row.natural_key || row.record_id || "");
                  const p = (row.payload as Record<string, unknown>) ?? {};
                  return (
                    <SurfaceRow
                      key={sid}
                      icon="network"
                      selected={selectedStreamId === sid}
                      title={`${String(p.protocol || "ws").toUpperCase()} ${String(p.url || sid)}`}
                      subtitle={`${sid} · seq=${String(p.next_seq ?? 0)}`}
                      trailing={<StudioBadge tone={p.state === "open" ? "success" : "neutral"}>{String(p.state || "closed")}</StudioBadge>}
                      onClick={() => { void loadStreamsData(sid); }}
                    />
                  );
                }) : <SurfaceRow icon="network" title="暂无 WS/SSE 流" />}
              </SurfaceCard>
            </div>
            <div>
              <SectionHeader title="帧事件" count={streamEvents.length} />
              <SurfaceCard>
                {streamEvents.length ? streamEvents.map((ev, idx) => (
                  <SurfaceRow
                    key={String(ev.event_id || idx)}
                    icon="rows"
                    title={`#${String(ev.seq ?? idx)} ${String(ev.direction || ev.event_type || "frame")}`}
                    subtitle={String(ev.payload_text || ev.text || JSON.stringify(ev)).slice(0, 120)}
                  />
                )) : <SurfaceRow icon="rows" title="暂无帧事件" />}
              </SurfaceCard>
            </div>
          </div>
        </div>
      ) : null}

      {ui.tab === "wire" ? (
        <div className="cx-scroll min-h-0 flex-1 overflow-y-auto p-4" data-testid="studio-traffic-wire">
          <div className="mx-auto flex max-w-[820px] flex-col gap-4">
            {diagError ? (
              <Callout
                tone="danger"
                title="读取 Wire/熔断诊断失败"
                role="alert"
                action={<Button size="sm" variant="secondary" icon="retry" onClick={() => { void loadDiagnostics(wireExchangeId); }}>重试</Button>}
              >
                {diagError}
              </Callout>
            ) : null}
            <SurfaceCard className="p-3">
              <div className="flex flex-wrap items-center justify-between gap-2">
                <SectionHeader title="MCP 熔断与账号冷却池" count={(mcpCircuits?.mcp_circuits?.length ?? 0) + (mcpCircuits?.quota_cooldowns?.length ?? 0)} />
                <div className="flex items-center gap-1.5">
                  <Button size="sm" variant="secondary" disabled={!attached || diagBusy} onClick={() => { void loadDiagnostics(wireExchangeId); }}>刷新</Button>
                  <Button
                    size="sm"
                    variant="secondary"
                    disabled={!attached || diagBusy}
                    onClick={async () => {
                      setDiagBusy(true);
                      try {
                        await postCommand(taskId, "src.diagnostics.mcp_reset", {});
                        await loadDiagnostics(wireExchangeId);
                        toast({ title: "已重置 MCP 与配额冷却状态", tone: "success" });
                      } catch (err) {
                        toast({ title: "重置失败", description: err instanceof Error ? err.message : String(err), tone: "danger" });
                      } finally {
                        setDiagBusy(false);
                      }
                    }}
                  >
                    重置熔断
                  </Button>
                </div>
              </div>
              <div className="mt-2">
                {(mcpCircuits?.mcp_circuits ?? []).map((c, idx) => (
                  <SurfaceRow
                    key={String(c.circuit_key || idx)}
                    icon="plug"
                    title={`${String(c.server || "")} (${String(c.package_id || "")})`}
                    subtitle={`scope=${String(c.scope || "chat")} · hash=${String(c.config_hash || "")} · err=${String(c.cause_code || "")}`}
                    trailing={<StudioBadge tone={c.state === "open" ? "danger" : "warning"}>{`${String(c.state)} · ${String(c.remaining_secs ?? 0)}s`}</StudioBadge>}
                  />
                ))}
                {(mcpCircuits?.quota_cooldowns ?? []).map((q, idx) => (
                  <SurfaceRow
                    key={String(q.credential_id || idx)}
                    icon="shield"
                    title={String(q.credential_id || "")}
                    subtitle={`resets_at=${String(q.resets_at ?? 0)}`}
                    trailing={<StudioBadge tone="warning">{`冷却 ${String(q.remaining_secs ?? 0)}s`}</StudioBadge>}
                  />
                ))}
                {!mcpCircuits?.mcp_circuits?.length && !mcpCircuits?.quota_cooldowns?.length ? (
                  <SurfaceRow icon="check" title="无触发中的熔断或配额冷却" />
                ) : null}
              </div>
            </SurfaceCard>

            <SurfaceCard className="p-3">
              <div className="flex flex-wrap items-center gap-2">
                <Input size="sm" value={wireExchangeId} onChange={(e) => setWireExchangeId(e.target.value)} placeholder="输入 exchange_id 查看 Wire 十六进制帧" className="min-w-[200px] flex-1 font-cx-mono text-cx-xs" />
                <Button size="sm" variant="primary" disabled={!attached || diagBusy || !wireExchangeId.trim()} onClick={() => { void loadDiagnostics(wireExchangeId); }}>查看 Wire</Button>
              </div>
              {wireDetail ? (
                <div className="mt-3 flex flex-col gap-3">
                  <Dl items={[
                    { label: "方法与地址", value: `${String(wireDetail.method ?? "")} ${String(wireDetail.observed_url ?? "")}` },
                    { label: "归因跳次", value: `ctx=${String(wireDetail.context_id ?? "—")} · run=${String(wireDetail.source_run_id ?? "—")} · node=${String(wireDetail.proxy_node_id ?? "—")}` },
                    { label: "Req SHA256", value: String(wireDetail.request_sha256 || "—"), mono: true },
                    { label: "Resp SHA256", value: String(wireDetail.response_sha256 || "—"), mono: true },
                  ]} />
                  {wireDetail.request_hex_preview ? (
                    <div>
                      <SectionHeader title="Request Wire Frame (Hex)" />
                      <CodeBlock code={String(wireDetail.request_hex_preview)} language="text" className="max-h-[220px]" />
                    </div>
                  ) : null}
                  {wireDetail.response_hex_preview ? (
                    <div>
                      <SectionHeader title="Response Wire Frame (Hex)" />
                      <CodeBlock code={String(wireDetail.response_hex_preview)} language="text" className="max-h-[220px]" />
                    </div>
                  ) : null}
                </div>
              ) : null}
            </SurfaceCard>
          </div>
        </div>
      ) : null}

      <Dialog
        open={importOpen}
        onOpenChange={setImportOpen}
        title="导入密封会话凭据"
        size="md"
        footer={(
          <>
            <Button variant="ghost" onClick={() => setImportOpen(false)}>取消</Button>
            <Button variant="primary" disabled={!impCtx.trim()} onClick={() => { void importIdentitySession(); }}>保存至凭证库</Button>
          </>
        )}
      >
        <div className="flex flex-col gap-3">
          <TextField label="身份 Context ID" value={impCtx} onChange={(e) => setImpCtx(e.target.value)} placeholder="tenant_a_user" />
          <TextField label="作用域域名" value={impDomain} onChange={(e) => setImpDomain(e.target.value)} placeholder="api.example.com" />
          <TextField label="Cookie 请求头" type="password" value={impCookie} onChange={(e) => setImpCookie(e.target.value)} placeholder="SID=...; csrftoken=..." />
          <TextField label="Authorization 请求头" type="password" value={impAuth} onChange={(e) => setImpAuth(e.target.value)} placeholder="Bearer ..." />
          <TextField label="RFC 6238 TOTP Seed (可选 Base32)" type="password" value={impTotp} onChange={(e) => setImpTotp(e.target.value)} placeholder="JBSWY3DPEHPK3PXP" />
        </div>
      </Dialog>

      <Dialog
        open={mutOpen}
        onOpenChange={setMutOpen}
        title="新建请求加签配方"
        size="lg"
        footer={(
          <>
            <Button variant="ghost" onClick={() => setMutOpen(false)}>取消</Button>
            <Button variant="primary" disabled={!mutName.trim()} onClick={() => { void saveMutatorRecipe(); }}>保存</Button>
          </>
        )}
      >
        <div className="flex flex-col gap-3">
          <TextField label="配方名称" value={mutName} onChange={(e) => setMutName(e.target.value)} placeholder="API HMAC-SHA256" />
          <div className="grid grid-cols-2 gap-2 max-sm:grid-cols-1">
            <div className="flex flex-col">
              <Label>模式</Label>
              <Select<"declarative" | "sandbox_js">
                value={mutKind}
                onChange={setMutKind}
                ariaLabel="加签模式"
                options={[{ value: "declarative", label: "declarative" }, { value: "sandbox_js", label: "sandbox_js" }]}
              />
            </div>
            {mutKind === "declarative" ? (
              <div className="flex flex-col">
                <Label>算法</Label>
                <Select<string>
                  value={mutAlgo}
                  onChange={setMutAlgo}
                  ariaLabel="签名算法"
                  options={[
                    { value: "hmac_sha256", label: "hmac_sha256" },
                    { value: "hmac_sha1", label: "hmac_sha1" },
                    { value: "sha256_kv_sorted", label: "sha256_kv_sorted" },
                    { value: "md5_kv_join", label: "md5_kv_join" },
                    { value: "timestamp_nonce", label: "timestamp_nonce" },
                  ]}
                />
              </div>
            ) : (
              <TextField label="输出签名头" value={mutHeader} onChange={(e) => setMutHeader(e.target.value)} />
            )}
          </div>
          {mutKind === "declarative" ? (
            <TextField label="输出签名头" value={mutHeader} onChange={(e) => setMutHeader(e.target.value)} />
          ) : (
            <TextArea label="沙箱 JS (function mutate(req, secrets))" value={mutJs} onChange={(e) => setMutJs(e.target.value)} className="min-h-[120px] font-cx-mono text-cx-xs" />
          )}
          <TextField label="签名密钥（存入凭证库，不入配置）" type="password" value={mutSecret} onChange={(e) => setMutSecret(e.target.value)} />
        </div>
      </Dialog>

      <SheetAskBar
        sheet={sheet}
        target={targetRef}
        prefill={selected ? `关于 HTTP 流量 ${selected.exchangeId} (${selected.method} ${selected.observedUrl})：` : undefined}
      />
    </div>
  );
}
