"use client";

import { useCallback, useEffect, useMemo, useRef, useState, useSyncExternalStore } from "react";
import type { SettingsNavigation } from "./settings/SettingsHost";
import { NativePathPicker } from "./NativePathPicker";
import { conversationStorageScope, subscribeConversationStorageScope } from "@/lib/conversationStorageScope";
import { Icon } from "@muteki/ui";
import { Button, Checkbox, Dialog, IconButton, Input, Label, Menu, MenuItem, MenuSeparator, SearchInput, Select, Switch, TextArea, useContextMenu } from "@muteki/ui";
import { apiFetch, useRunList } from "@/lib/useRun";

type Mode = "chat" | "ctf" | "pentest";
type SourceTab = "managed" | "native";
type Filter = "all" | "skill" | "mcp" | "plugin";
type Compatibility = { status: string; components: string[]; unavailable?: string[]; reasons?: string[] };
type Package = {
  id: string; name: string; description: string; version: string; enabled: boolean;
  modes: Mode[]; allowed_modes?: Mode[]; skills: { name: string; description: string }[];
  mcp_servers: string[]; native_components: string[]; snapshot_id?: string; digest?: string;
  diagnostics: string[]; can_rollback: boolean; native_hooks_enabled?: boolean;
  hook_commands?: string[]; hook_definitions?: unknown[];
  compatibility?: Record<string, Compatibility>;
  worker_compatibility?: Record<string, Compatibility>;
};
type Catalog = {
  engine: string; packages: Package[]; control_enabled: boolean;
  bundled_plugins?: { id: string; name: string; modes: Mode[] }[];
  builtin_skills: { id: string; modes: Mode[]; enabled: boolean; scope: string }[];
  native_skills: { id: string; name: string; description: string; source: string }[];
  native_mcp: { name: string; enabled: boolean; source: string; transport: string }[];
  host_discovery_enabled?: boolean;
  runtime_scope: string;
  runtime_mcp: { package_id: string; server: string; status: "connected" | "failed" | "disconnected" | "not_checked"; observed_at?: string }[];
};

const MODES: { id: Mode; label: string; description: string }[] = [
  { id: "chat", label: "聊天", description: "用于对话 Agent；当前对话在下一轮更新。" },
  { id: "ctf", label: "CTF", description: "用于新启动的 CTF Worker，按引擎投放到任务工作区。" },
  { id: "pentest", label: "渗透测试", description: "用于新启动的渗透 Worker；MCP 状态按 Run 隔离。" },
];
const ENGINES: Record<string, string> = {
  claude: "Claude", codex: "Codex", cursor: "Cursor", pi: "Pi",
  omp: "OMP", kimi: "Kimi", grok: "Grok", opencode: "OpenCode", devin: "Devin", droid: "Droid", agy: "Antigravity",
};
const SOURCE_KIND_OPTIONS = [
  { value: "local-dir", label: "服务宿主目录" },
  { value: "archive", label: "服务宿主压缩包" },
  { value: "git", label: "固定 Git 版本" },
];
const ENGINE_OPTIONS = Object.entries(ENGINES).map(([value, label]) => ({ value, label }));

class ExtensionRequestError extends Error {
  constructor(readonly code: string, message: string, readonly raw: string, readonly status: number) {
    super(`${message}\n${code} · HTTP ${status}\n${raw}`); this.name = "ExtensionRequestError";
  }
}
async function request(path: string, method = "GET", body?: unknown, base = "/api/agent-extensions", signal?: AbortSignal) {
  const response = await apiFetch(`${base}${path}`, {method, signal, headers: {"Content-Type": "application/json"}, body: body === undefined ? undefined : JSON.stringify(body)});
  const raw = await response.text();
  let data;
  try { data = JSON.parse(raw); } catch { throw new ExtensionRequestError("extensions.response_invalid", "响应不是有效 JSON", raw, response.status); }
  if (!response.ok) {
    const detail = data?.error || data?.detail;
    throw new ExtensionRequestError(detail?.code || "extensions.http_error", typeof detail === "string" ? detail : detail?.message || "扩展操作失败", raw, response.status);
  }
  return data;
}
function validatedCatalog(value: unknown, engine: string): Catalog {
  const object = (v: unknown): v is Record<string, unknown> => Boolean(v && typeof v === "object" && !Array.isArray(v));
  const strings = (v: unknown): v is string[] => Array.isArray(v) && v.every(item => typeof item === "string");
  if (!object(value) || value.engine !== engine || typeof value.control_enabled !== "boolean"
    || !Array.isArray(value.packages) || !Array.isArray(value.native_skills) || !Array.isArray(value.native_mcp)
    || !Array.isArray(value.builtin_skills) || !Array.isArray(value.runtime_mcp) || typeof value.runtime_scope !== "string"
    || value.packages.some(item => !object(item) || !["id", "name", "description", "version"].every(key => typeof item[key] === "string")
      || typeof (item.snapshot_id || item.digest) !== "string"
      || !item.id || typeof item.enabled !== "boolean" || typeof item.can_rollback !== "boolean" || !strings(item.modes)
      || !strings(item.mcp_servers) || !strings(item.native_components) || !strings(item.diagnostics) || !Array.isArray(item.skills)
      || item.skills.some(skill => !object(skill) || typeof skill.name !== "string" || typeof skill.description !== "string"))
    || value.native_skills.some(item => !object(item) || !["id", "name", "description", "source"].every(key => typeof item[key] === "string"))
    || value.native_mcp.some(item => !object(item) || !["name", "source", "transport"].every(key => typeof item[key] === "string") || typeof item.enabled !== "boolean")) {
    throw new ExtensionRequestError("extensions.catalog_invalid", "扩展目录响应不符合契约", JSON.stringify(value), 200);
  }
  const catalog = value as unknown as Catalog;
  if (new Set(catalog.packages.map(item => item.id)).size !== catalog.packages.length) throw new ExtensionRequestError("extensions.catalog_invalid", "扩展目录有重复 ID", JSON.stringify(value), 200);
  return catalog;
}

function ModeChoices({ value, onChange, allowed }: { value: Mode[]; onChange: (next: Mode[]) => void; allowed?: Mode[] }) {
  return <div className="ae-mode-choices">
    {MODES.map((mode) => <div key={mode.id} className="ae-mode-choice">
      <Checkbox label={mode.label} description={mode.description} checked={value.includes(mode.id)}
        disabled={Boolean(allowed && !allowed.includes(mode.id))}
        onCheckedChange={(checked) => onChange(checked ? [...value, mode.id] : value.filter((item) => item !== mode.id))} />
    </div>)}
  </div>;
}

export function AgentExtensionsSettings({navigation}: {navigation: SettingsNavigation}) {
  const params = navigation.searchParams;
  const scope = useSyncExternalStore(subscribeConversationStorageScope, conversationStorageScope, () => "");
  const mode: Mode = params.get("mode") === "ctf" ? "ctf" : params.get("mode") === "pentest" ? "pentest" : "chat";
  const runId = params.get("run") || "";
  const runs = useRunList(15000);
  const sourceTab: SourceTab = params.get("source") === "native" ? "native" : "managed";
  const engine = ENGINES[params.get("engine") || ""] ? params.get("engine")! : "codex";
  const updateQuery = (patch: Record<string, string>) => {
    const next = new URLSearchParams(params);
    Object.entries(patch).forEach(([key, value]) => { if (value) next.set(key, value); else next.delete(key); });
    navigation.router.replace(navigation.pathname + (next.size ? `?${next}` : ""));
  };
  const setSourceTab = (value: string) => updateQuery({source: value});
  const setEngine = (value: string) => updateQuery({engine: value});
  const [snapshot, setSnapshot] = useState<{ scope: string; owner: string; data: Catalog } | null>(null);
  const [query, setQuery] = useState("");
  const filter: Filter = ["skill", "mcp", "plugin"].includes(params.get("type") || "") ? params.get("type") as Filter : "all";
  const setFilter = (value: Filter) => updateQuery({type: value});
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [busy, setBusy] = useState(false);
  const [picking, setPicking] = useState(false);
  const [installOpen, setInstallOpen] = useState(false);
  const [installType, setInstallType] = useState<"package" | "mcp">("package");
  const [installModes, setInstallModes] = useState<Mode[]>([mode]);
  const [kind, setKind] = useState("local-dir");
  const [source, setSource] = useState("");
  const [ref, setRef] = useState("");
  const [name, setName] = useState("");
  const [config, setConfig] = useState('{\n  "mcpServers": {\n    "my-server": { "url": "https://example.com/mcp" }\n  }\n}');
  const [detailId, setDetailId] = useState("");
  const [remove, setRemove] = useState<Package | null>(null);
  const contextMenu = useContextMenu();
  const [contextPackageId, setContextPackageId] = useState("");
  const selectedMode = MODES.find((item) => item.id === mode)!;
  const selectedRun = runs.find((item) => item.run_id === runId);
  const catalogPath = `?engine=${encodeURIComponent(engine)}&mode=${mode}${runId ? `&run_id=${encodeURIComponent(runId)}` : ""}`;
  const owner = `${scope}:${catalogPath}`;
  const catalog = snapshot?.owner === owner ? snapshot.data : null;
  const countCatalog = snapshot?.scope === scope ? snapshot.data : null;
  const detail = catalog?.packages.find((item) => item.id === detailId);
  const currentOwner = useRef(owner); currentOwner.current = owner;
  const controller = useRef<AbortController | null>(null);
  const operation = useRef<symbol | null>(null);
  const mounted = useRef(true);
  const reload = useCallback(async () => {
    if (!scope || currentOwner.current !== owner || !mounted.current) return null;
    controller.current?.abort(); const abort = new AbortController(); controller.current = abort;
    const data = validatedCatalog(await request(catalogPath, "GET", undefined, "/api/agent-extensions", abort.signal), engine);
    if (abort.signal.aborted || currentOwner.current !== owner || !mounted.current) return null;
    setSnapshot({ scope, owner, data }); setError(""); return data;
  }, [scope, owner, catalogPath, engine]);
  useEffect(() => {
    mounted.current = true; setError(""); setNotice(""); setBusy(false); setPicking(false);
    setInstallOpen(false); setDetailId(""); setRemove(null); setContextPackageId(""); operation.current = null;
    if (scope) void reload().catch(cause => { if (mounted.current && currentOwner.current === owner && !controller.current?.signal.aborted) setError(cause instanceof Error ? cause.message : String(cause)); });
    return () => { mounted.current = false; controller.current?.abort(); };
  }, [owner, scope, reload]);
  const mutate = async (action: () => Promise<unknown>, message: string, completed?: () => void, confirm?: (data: Catalog) => boolean) => {
    if (!scope || currentOwner.current !== owner || operation.current || picking) return;
    const ticket = Symbol(); operation.current = ticket; setBusy(true); setError(""); setNotice(""); let applied = false;
    const current = () => mounted.current && currentOwner.current === owner && operation.current === ticket && conversationStorageScope() === scope;
    try {
      await action(); applied = true;
      if (!current()) return;
      const data = await reload();
      if (!current() || !data) return;
      if (confirm && !confirm(data)) throw new Error("目录尚未确认本次变更，请重载核对。");
      completed?.(); setNotice(message); window.dispatchEvent(new Event("muteki:chat-plugins-changed"));
    } catch (cause) {
      if (current()) setError(`${applied ? "变更已返回，但目录确认失败；请重载核对结果。\n" : ""}${cause instanceof Error ? cause.message : String(cause)}`);
    } finally { if (operation.current === ticket) { operation.current = null; if (currentOwner.current === owner && mounted.current) setBusy(false); } }
  };
  const setActiveMode = (next: Mode) => updateQuery({mode: next === "chat" ? "" : next, run: next === "chat" || selectedRun?.mode !== next ? "" : runId});
  const selectRun = (next: string) => updateQuery({run: next});
  const togglePackageMode = (item: Package, enabled: boolean) => {
    const modes = enabled ? [...item.modes, mode] : item.modes.filter((entry) => entry !== mode);
    if (!modes.length) { setError("至少保留一个使用场景；若要全部停用，请在详情中关闭全局启用。"); return; }
    void mutate(() => request(`/packages/${encodeURIComponent(item.id)}`, "PATCH", { modes }),
      `${item.name} 在${selectedMode.label}中已${enabled ? "启用" : "停用"}。新 Worker 将采用此配置。`);
  };
  const togglePackage = (item: Package, enabled: boolean) => void mutate(
    () => request(`/packages/${encodeURIComponent(item.id)}`, "PATCH", { enabled }),
    enabled ? "扩展已启用" : "扩展已停用",
  );
  const rollbackPackage = (item: Package) => void mutate(
    () => request(`/packages/${encodeURIComponent(item.id)}`, "PATCH", { rollback: true }), "已恢复上一版本",
  );
  const checkPackageMcp = (item: Package) => void mutate(async () => {
    const result = await request(`/check/${encodeURIComponent(item.id)}`, "POST");
    if (!Number.isSafeInteger(result?.tool_count) || !Array.isArray(result?.diagnostics)) throw new Error("MCP 检查响应无效");
    if (result.diagnostics.length) throw new Error(result.diagnostics.join("\n"));
  }, "MCP 检查已返回，目录已刷新。");
  const requestRemoval = (item: Package) => { setRemove(item); setDetailId(""); };
  const toggleBuiltinBrowser = (enabled: boolean) => void mutate(
    () => request("/resources/skills/agent-browser", "PUT", { enabled }, "/api/capability-management"),
    `agent-browser 已${enabled ? "启用" : "关闭"}；新启动的渗透 Worker 将采用此设置。`,
  );
  const openInstall = () => { setInstallModes([mode]); setError(""); setInstallOpen(true); };
  const install = () => void mutate(async () => {
    if (!installModes.length) throw new Error("请至少选择一个使用场景");
    if (installType === "mcp") {
      let parsed: Record<string, unknown>;
      try { parsed = JSON.parse(config); } catch { throw new Error("MCP 配置不是有效的 JSON"); }
      await request("/mcp", "POST", { name: name.trim(), servers: parsed.mcpServers || parsed, modes: installModes });
    } else {
      await request("/install", "POST", {
        source: { kind, ...(kind === "git" ? { url: source, ref } : { path: source }) }, modes: installModes,
      });
    }
  }, "扩展已安装，所选场景将在下一次 Agent 启动或对话轮次中读取兼容组件。", () => { setInstallOpen(false); setSource(""); setRef(""); setName(""); });

  const packages = useMemo(() => (catalog?.packages || []).filter((item) => {
    const matches = `${item.name} ${item.description} ${item.skills.map((skill) => skill.name).join(" ")} ${item.mcp_servers.join(" ")}`
      .toLowerCase().includes(query.trim().toLowerCase());
    const hasNative = Boolean(item.native_components.length || Object.values(item.worker_compatibility || {}).some((row) => row.components.includes("原生插件")));
    const typeMatches = filter === "all" || (filter === "skill" && item.skills.length > 0)
      || (filter === "mcp" && item.mcp_servers.length > 0) || (filter === "plugin" && hasNative);
    return matches && typeMatches;
  }), [catalog, filter, query]);
  const browserEnabled = catalog?.builtin_skills?.find((item) => item.id === "agent-browser")?.enabled ?? false;
  const modeCount = (selected: Mode) => countCatalog
    ? countCatalog.packages.filter((item) => item.enabled && item.modes.includes(selected)).length
      + (selected === "pentest" && countCatalog.builtin_skills.some((item) => item.id === "agent-browser" && item.enabled) ? 1 : 0)
    : "–";
  const activeCount = modeCount(mode);
  const browserVisible = mode === "pentest" && (filter === "all" || filter === "skill")
    && "agent-browser 浏览器 截图".toLowerCase().includes(query.trim().toLowerCase());
  const nativeSkills = Array.from((catalog?.native_skills || [])
    .filter((item) => filter !== "mcp" && filter !== "plugin" && item.name.toLowerCase().includes(query.toLowerCase()))
    .reduce((groups, item) => {
      const existing = groups.get(item.name);
      if (!existing) groups.set(item.name, { ...item, sources: [item.source] });
      else if (!existing.sources.includes(item.source)) existing.sources.push(item.source);
      return groups;
    }, new Map<string, Catalog["native_skills"][number] & { sources: string[] }>()).values());
  const nativeMcp = (catalog?.native_mcp || []).filter((item) => filter !== "skill" && filter !== "plugin" && item.name.toLowerCase().includes(query.toLowerCase()));
  const packageActions = (item: Package) => {
    const compatible = mode === "chat" ? item.compatibility?.[engine] : item.worker_compatibility?.[engine];
    const available = Boolean((!item.allowed_modes || item.allowed_modes.includes(mode))
      && compatible && compatible.status !== "blocked" && compatible.status !== "unavailable");
    const selected = item.modes.includes(mode);
    return <>
      <MenuItem icon="info" onSelect={() => setDetailId(item.id)}>查看详情</MenuItem>
      <MenuItem icon={selected ? "pause" : "play"} disabled={busy || !available || (selected && item.modes.length === 1)}
        description={selected && item.modes.length === 1 ? "至少保留一个场景；可全局停用" : undefined}
        onSelect={() => togglePackageMode(item, !selected)}>在{selectedMode.label}中{selected ? "停用" : "启用"}</MenuItem>
      <MenuItem icon={item.enabled ? "pause" : "play"} disabled={busy} onSelect={() => togglePackage(item, !item.enabled)}>{item.enabled ? "全局停用" : "全局启用"}</MenuItem>
      {item.mcp_servers.length ? <MenuItem icon="plug" disabled={busy} onSelect={() => checkPackageMcp(item)}>检查 MCP</MenuItem> : null}
      {item.can_rollback ? <MenuItem icon="undo" disabled={busy} onSelect={() => rollbackPackage(item)}>恢复上一版本</MenuItem> : null}
      <MenuSeparator />
      <MenuItem icon="trash" danger disabled={busy} onSelect={() => requestRemoval(item)}>卸载…</MenuItem>
    </>;
  };
  const contextPackage = catalog?.packages.find(item => item.id === contextPackageId);
  const modeRuns = useMemo(() => runs.filter((item) => item.mode === mode), [runs, mode]);
  const runOptions = useMemo(() => [
    { value: "", label: "未选择 Run（仅显示配置与兼容性）" },
    ...modeRuns.map((item) => ({
      value: item.run_id,
      label: `${item.name || item.run_id} · ${item.run_id}${item.finished ? " · 已结束" : ""}`,
    })),
  ], [modeRuns]);

  return <div className="agent-extensions cx-root" data-testid="agent-extensions-settings">
    <div className="ae-mode-bar" role="tablist" aria-label="扩展使用场景">
      {MODES.map((item) => <button key={item.id} type="button" role="tab" aria-selected={mode === item.id}
        className={mode === item.id ? "active" : ""} onClick={() => setActiveMode(item.id)}>
        {item.label}<span>{modeCount(item.id)}</span>
      </button>)}
    </div>

    <div className="ae-intro">
      <div>
        <h2>{/[A-Za-z]$/.test(selectedMode.label) ? `${selectedMode.label} 能力` : `${selectedMode.label}能力`}</h2><p>{mode === "chat"
          ? "已启用的插件包、Skill 与外部 MCP 在下一轮聊天读取；内置浏览器、电脑操控与安全平台桥接请在「设置 → 内置能力」管理。"
          : `新启动的${selectedMode.label} Worker 读取已分配的 Skill 和兼容插件；MCP 工具按 Run 连接。${runId ? ` 当前查看 ${runId} 的连接观测。` : "打开具体 Run 时可查看连接观测。"}`}</p></div>
      <div className="ae-intro-count"><strong>{activeCount}</strong><span>项已启用</span></div>
    </div>

    <div className="ae-toolbar">
      <div className="ae-source" role="group" aria-label="扩展来源">
        <button type="button" className={sourceTab === "managed" ? "active" : ""} onClick={() => setSourceTab("managed")}>Muteki 安装</button>
        <button type="button" className={sourceTab === "native" ? "active" : ""} onClick={() => setSourceTab("native")}>服务宿主 Agent · 只读</button>
      </div>
      <div className="ml-auto flex flex-wrap items-center gap-2">
        {mode === "chat" && catalog?.bundled_plugins?.some((item) => item.id === "muteki-visualize")
          && !catalog.packages.some((item) => item.id === "muteki-visualize") && <Button variant="outline" disabled={busy}
            onClick={() => void mutate(() => request("/bundled/muteki-visualize", "POST"), "可视化插件已独立安装，下一轮聊天可用。")}>安装可视化插件</Button>}
        <Button variant="primary" icon="plus" onClick={openInstall}>添加扩展</Button>
      </div>
    </div>
    {mode !== "chat" && <div className="ae-run-picker">
      <span className="ae-run-picker-label">查看 Run 的 MCP 连接</span>
      <Select<string>
        value={runId}
        onChange={selectRun}
        ariaLabel="查看 Run 的 MCP 连接"
        placeholder={runId || "未选择 Run（仅显示配置与兼容性）"}
        searchable={modeRuns.length > 8}
        className="ae-run-select h-8"
        options={runOptions}
      />
      {runId && <Button variant="outline" onClick={() => void reload().catch((cause) => {
        if (currentOwner.current === owner) {
          setError(cause instanceof Error ? cause.message : String(cause));
        }
      })}>刷新连接状态</Button>}
      {selectedRun?.finished && <small>该 Run 已结束，MCP 连接已释放。</small>}
    </div>}
    <div className="ae-filters">
      <SearchInput size="md" className="ae-search" aria-label="搜索扩展" value={query} onValueChange={setQuery} placeholder="搜索名称、Skill 或 MCP" />
      <Select
        value={engine}
        onChange={setEngine}
        ariaLabel="查看引擎兼容性"
        className="ae-engine-select h-8"
        options={ENGINE_OPTIONS}
      />
      <div className="ae-filters-types" role="group" aria-label="扩展类型">
        {([ ["all", "全部"], ["skill", "Skill"], ["mcp", "MCP"], ["plugin", "插件"] ] as const).map(([id, label]) =>
          <button type="button" key={id} aria-pressed={filter === id} onClick={() => setFilter(id)}>{label}</button>)}
      </div>
    </div>

    {error && <p className="ae-message ae-error" role="alert">{error}</p>}
    {notice && <p className="ae-message" role="status">{notice}</p>}
    {sourceTab === "managed" ? <section className="ae-list" aria-label="已安装扩展">
      <div className="ae-list-head"><span>已安装扩展</span><span>组件</span><span>{ENGINES[engine]} 兼容与连接</span><span>{selectedMode.label}适用</span></div>
      {browserVisible && <div className="ae-row ae-row-builtin" data-testid="agent-browser-builtin">
        <div className="ae-row-name"><span className="ae-row-icon"><Icon name="globe" size={17} /></span>
          <div><strong>agent-browser</strong><p>共享 Run 会话与 Cookie；Worker 独立标签页，关键页面可保存截图。</p>
            <small>运行时内置 · 容器 Worker</small></div></div>
        <div className="ae-row-components"><span>Skill</span><span>浏览器 CLI</span></div>
        <div className="ae-row-engine"><span className={browserEnabled ? "available" : "unavailable"}>{browserEnabled ? "已启用" : "已关闭"}</span><small>新启动的渗透测试 Worker；此处不检测当前浏览器进程</small></div>
        <div className="ae-row-action"><Switch ariaLabel="agent-browser 在渗透测试中可用"
          checked={browserEnabled} disabled={busy || !catalog} onCheckedChange={toggleBuiltinBrowser} /></div>
      </div>}
      {packages.map((item) => {
        const compatible = mode === "chat" ? item.compatibility?.[engine] : item.worker_compatibility?.[engine];
        const available = Boolean((!item.allowed_modes || item.allowed_modes.includes(mode))
          && compatible && compatible.status !== "blocked" && compatible.status !== "unavailable");
        const selected = item.modes.includes(mode);
        const observed = (catalog?.runtime_mcp || []).filter((row) => row.package_id === item.id);
        const mcpState = item.mcp_servers.length === 0 ? "" : mode === "chat" ? "聊天时连接"
          : !selected ? "未分配至此场景"
          : !runId ? "请选择 Run 查看连接"
          : selectedRun?.finished ? "Run 已结束，连接已释放"
          : observed.some((row) => row.status === "failed") ? "当前 Run 连接失败，可重试"
          : observed.some((row) => row.status === "disconnected") ? "当前 Run 连接已断开"
          : observed.length > 0 && observed.every((row) => row.status === "connected") ? "当前 Run 最近连接成功"
          : "当前 Run 尚未检查连接";
        return <div className="ae-row" key={item.id} data-plugin-id={item.id}
          onContextMenu={event => { contextMenu.onContextMenu(event); if (event.defaultPrevented) setContextPackageId(item.id); }}
          onKeyDown={event => { contextMenu.onKeyDown(event); if (event.defaultPrevented) setContextPackageId(item.id); }}>
          <div className="ae-row-name"><span className="ae-row-icon"><Icon name={item.mcp_servers.length ? "plug" : "sparkles"} size={17} /></span>
            <div><button type="button" onClick={() => setDetailId(item.id)}>{item.name}</button><p>{item.description}</p>
              <small>{item.version} · {item.enabled ? "全局启用" : "全局停用"}</small></div></div>
          <div className="ae-row-components">{item.skills.length > 0 && <span>{item.skills.length} Skill</span>}{item.mcp_servers.length > 0 && <span>{item.mcp_servers.length} MCP</span>}
            {Object.values(item.worker_compatibility || {}).some((row) => row.components.includes("原生插件")) && <span>原生插件</span>}</div>
          <div className="ae-row-engine"><span className={available ? "available" : "unavailable"}>{available ? "兼容" : "不兼容"}</span><small>{compatible?.components.join(" · ") || "无可投放组件"}</small>{mcpState && <small>{mcpState}</small>}</div>
          <div className="ae-row-action"><Switch ariaLabel={`${item.name} 在${selectedMode.label}中可用`}
            checked={selected} disabled={busy || !available} onCheckedChange={(value) => togglePackageMode(item, value)} />
            <Menu trigger={<IconButton icon="more" label={`${item.name} 更多操作`} size="sm" />} placement="bottom-end" ariaLabel={`${item.name} 操作`}>
              {packageActions(item)}
            </Menu></div>
          {item.diagnostics.map((diagnostic) => <p className="ae-row-diagnostic" key={diagnostic}>{diagnostic}</p>)}
        </div>;
      })}
      {!catalog && !error && <div className="ae-empty">正在加载…</div>}
      {catalog && packages.length === 0 && !browserVisible && <div className="ae-empty"><Icon name="layers" size={23} /><strong>{query || filter !== "all" ? "没有匹配的扩展" : "尚未安装扩展"}</strong>
        <p>{query || filter !== "all" ? "试试其他名称或类型。" : `添加 Skill、插件或 MCP，并选择在${selectedMode.label}中使用。`}</p>
        {!query && filter === "all" && <Button variant="outline" className="mt-2" onClick={openInstall}>添加扩展</Button>}</div>}
    </section> : <section className="ae-native" aria-label="服务宿主 Agent 能力">
      {catalog?.host_discovery_enabled === false ? <p role="status">当前服务已禁用宿主能力发现；这不表示 Agent 未登录。</p> : null}
      <p>这里仅展示 {ENGINES[engine]} 的服务宿主配置。Worker 使用 Muteki 安装并明确启用的包；本机 Agent 的私有目录不会直接挂载进任务容器。</p>
      {nativeSkills.length > 0 && <h3 className="ae-native-group">Skill · {nativeSkills.length}</h3>}
      {nativeSkills.map((item) => <div className="ae-native-row" key={`skill:${item.name}`}><strong>{item.name}</strong>
        <span>{item.sources.join(" · ")}</span>{item.description && <p>{item.description}</p>}</div>)}
      {nativeMcp.length > 0 && <h3 className="ae-native-group">MCP · {nativeMcp.length}</h3>}
      {nativeMcp.length > 0 && <p>来自 {ENGINES[engine]} 配置文件；对话中的实际连接状态见聊天输入框的 @ 菜单。</p>}
      {nativeMcp.map((item) => <div className="ae-native-row" key={`mcp:${item.name}`}><strong>{item.name}</strong>
        <span>{item.source} · {item.transport} · {item.enabled ? "已配置" : "已停用"}</span></div>)}
      {!nativeSkills.length && !nativeMcp.length && <div className="ae-empty">没有发现匹配的服务宿主能力</div>}
    </section>}

    {mode === "ctf" && engine === "pi" ? <section className="ae-native mt-4" aria-label="CTF 本地 Pi 单会话">
      <p>CTF 本地 Pi 单会话以 <code>--no-skills --no-extensions --no-mcp</code> 启动，不读取宿主全局 Skills。本页为 Pi / CTF 启用的 Skill 会以只读快照投影到 Run 工作区的 <code>.pi/skills/</code>，和 5 个内置 CTF Skills 一起通过 <code>--skill</code> 传入。</p>
      <div className="px-5 py-3">
        <Button size="sm" variant="secondary" icon="terminal" onClick={() => navigation.router.push("/ctf/workers?section=local_pi")}>打开本地 Pi 单会话设置</Button>
      </div>
    </section> : null}

    <p className="ae-boundary"><Icon name="lock" size={14} /> {mode === "pentest"
      ? "设置只影响新启动的 Worker。agent-browser 会话协调依赖受管命令入口，不能替代容器网络与文件系统隔离；已投放扩展的内容会在下次 Worker 启动时重新校验。"
      : `包保存在 Muteki 私有目录。${mode === "chat" ? "对话在下一轮读取变更。" : "新启动的 Worker 读取变更；运行中的 Worker 保留已投放的 Skill 快照。"}`}</p>

    {contextPackage ? contextMenu.render(packageActions(contextPackage), `${contextPackage.name} 操作`) : null}

    <Dialog open={installOpen} onOpenChange={(open) => { if (!busy && !picking) { setInstallOpen(open); if (!open) setError(""); } }}
      title="添加 Agent 扩展" description="导入一个包或连接 MCP，并明确选择它作用于哪些场景。" footer={<>
        <Button variant="ghost" onClick={() => setInstallOpen(false)} disabled={busy || picking}>取消</Button>
        <Button variant="primary" loading={busy} disabled={busy || picking || !installModes.length || (installType === "mcp" ? !name.trim() : !source.trim() || (kind === "git" && !ref.trim()))} onClick={install}>{installType === "mcp" ? "添加 MCP" : "导入扩展"}</Button>
      </>}>
      <div className="ae-dialog-content">
        <div className="ae-dialog-tabs"><button type="button" aria-pressed={installType === "package"} onClick={() => setInstallType("package")}>Skill / 插件</button><button type="button" aria-pressed={installType === "mcp"} onClick={() => setInstallType("mcp")}>MCP 服务</button></div>
        {installType === "mcp" ? <><div><Label htmlFor="ae-mcp-name">名称</Label><Input id="ae-mcp-name" value={name} onChange={(event) => setName(event.target.value)} placeholder="my-tools" /></div>
          <TextArea label="连接配置（JSON）" value={config} onChange={(event) => setConfig(event.target.value)} rows={8} className="font-cx-mono" />
          <p>支持 HTTP 与 stdio。Worker 调用由宿主转发，MCP 进程状态按 Run 隔离。</p></>
          : <><div><Label>来源</Label><Select ariaLabel="来源" className="w-full" value={kind} options={SOURCE_KIND_OPTIONS} onChange={setKind} /></div>
            {kind === "git" ? <div><Label htmlFor="ae-source-path">仓库地址</Label><Input id="ae-source-path" value={source} onChange={event => setSource(event.target.value)} /></div> : <NativePathPicker key={kind} id="ae-source-path" label="扩展来源路径" kind={kind === "archive" ? "file" : "directory"} value={source} onChange={setSource} onBusyChange={setPicking} disabled={busy} placeholder="当前服务可访问的路径" />}
            {kind === "git" && <div><Label htmlFor="ae-source-ref">Tag 或 Commit</Label><Input id="ae-source-ref" value={ref} onChange={(event) => setRef(event.target.value)} placeholder="v1.0.0" /></div>}</>}
        <div><strong className="ae-dialog-label">使用场景</strong><ModeChoices value={installModes} onChange={setInstallModes} /></div>
        {error && <p className="ae-dialog-error" role="alert">{error}</p>}
      </div>
    </Dialog>

    <Dialog open={!!detail} onOpenChange={(open) => { if (!open) setDetailId(""); }} title={detail?.name || "扩展详情"} description={detail?.description} footer={<>
      <Button variant="ghost" disabled={busy} onClick={() => { if (detail) requestRemoval(detail); }}>卸载</Button>
      <Button variant="primary" onClick={() => setDetailId("")}>完成</Button>
    </>}>
      {detail && <div className="ae-dialog-content"><p>版本 {detail.version} · {(detail.snapshot_id || detail.digest || "").slice(0, 12)}</p>
        <Switch label="全局启用" checked={detail.enabled} disabled={busy} onCheckedChange={(value) => togglePackage(detail, value)} />
        <div><strong className="ae-dialog-label">适用场景</strong><ModeChoices value={detail.modes} allowed={detail.allowed_modes} onChange={(modes) => {
          if (!modes.length) { setError("至少保留一个使用场景；可关闭全局启用。"); return; }
          void mutate(() => request(`/packages/${encodeURIComponent(detail.id)}`, "PATCH", { modes }), "适用场景已更新，新 Worker 启动时生效。");
        }} /></div>
        <div><strong className="ae-dialog-label">包含的组件</strong><p>{detail.skills.map((item) => item.name).join("、") || "无 Skill"} · {detail.mcp_servers.join("、") || "无 MCP"}</p></div>
        {!!detail.hook_commands?.length && <div><strong className="ae-dialog-label">原生 Hooks</strong><p>仅在支持该 ABI 的聊天 Agent 使用；启用需绑定当前包版本。</p><pre>{JSON.stringify(detail.hook_definitions || detail.hook_commands, null, 2)}</pre>
          <Switch label="启用此版本的 Hooks" checked={Boolean(detail.native_hooks_enabled)} disabled={busy} onCheckedChange={(value) => void mutate(() => request(`/packages/${encodeURIComponent(detail.id)}`, "PATCH", { native_hooks: value, snapshot_id: detail.snapshot_id || detail.digest }), value ? "Hooks 已启用" : "Hooks 已停用")} /></div>}
        <div><strong className="ae-dialog-label">{mode === "chat" ? "聊天引擎兼容性" : "Worker 引擎兼容性"}</strong><div className="ae-compatibility">{Object.entries(ENGINES).map(([id, label]) => {
          const result = mode === "chat" ? detail.compatibility?.[id] : detail.worker_compatibility?.[id];
          const supported = result && ["available", "supported", "partial"].includes(result.status);
          return <div key={id}><strong>{label}</strong><span>{supported ? result.components.join("、") : "无兼容组件"}</span></div>;
        })}</div></div>
        {(detail.mcp_servers.length > 0 || detail.can_rollback) ? (
          <div className="flex flex-wrap items-center gap-2">
            {detail.mcp_servers.length > 0 ? <Button variant="outline" size="sm" disabled={busy} onClick={() => checkPackageMcp(detail)}>检查 MCP</Button> : null}
            {detail.can_rollback ? <Button variant="outline" size="sm" disabled={busy} onClick={() => rollbackPackage(detail)}>恢复上一版本</Button> : null}
          </div>
        ) : null}
        {notice && <p role="status">{notice}</p>}{error && <p className="ae-dialog-error" role="alert">{error}</p>}
      </div>}
    </Dialog>
    <Dialog open={!!remove} onOpenChange={(open) => { if (!busy && !open) setRemove(null); }} title={`卸载 ${remove?.name || "扩展"}？`}
      description="从 Muteki 安装目录移除。已运行的 Worker 保留本次快照。" tone="danger" footer={<>
        <Button variant="ghost" disabled={busy} onClick={() => setRemove(null)}>取消</Button><Button variant="danger" loading={busy} onClick={() => void mutate(async () => { await request(`/packages/${encodeURIComponent(remove?.id || "")}`, "DELETE"); }, "扩展已卸载", () => setRemove(null))}>卸载</Button>
      </>} />
  </div>;
}
