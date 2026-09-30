"use client";

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useSearchParams } from "next/navigation";
import { Icon } from "@/components/Icon";
import { Button, Dialog, Input, Label, Switch, TextArea } from "@/components/chat/ui";
import { apiFetch, useRunList } from "@/lib/useRun";

type Mode = "chat" | "ctf" | "pentest";
type SourceTab = "managed" | "native";
type Filter = "all" | "skill" | "mcp" | "plugin";
type Compatibility = { status: string; components: string[]; unavailable?: string[]; reasons?: string[] };
type Package = {
  id: string; name: string; description: string; version: string; enabled: boolean;
  modes: Mode[]; skills: { name: string; description: string }[];
  mcp_servers: string[]; native_components: string[]; digest: string;
  diagnostics: string[]; can_rollback: boolean; native_hooks_enabled?: boolean;
  hook_commands?: string[]; hook_definitions?: unknown[];
  compatibility?: Record<string, Compatibility>;
  worker_compatibility?: Record<string, Compatibility>;
};
type Catalog = {
  engine: string; packages: Package[]; control_enabled: boolean;
  builtin_skills: { id: string; modes: Mode[]; enabled: boolean; scope: string }[];
  native_skills: { id: string; name: string; description: string; source: string }[];
  native_mcp: { name: string; enabled: boolean; source: string; transport: string }[];
  runtime_scope: string;
  runtime_mcp: { package_id: string; server: string; status: "connected" | "failed" | "disconnected" | "not_checked"; observed_at?: string }[];
};

const MODES: { id: Mode; label: string; description: string }[] = [
  { id: "chat", label: "聊天", description: "用于对话 Agent；当前会话在下一轮更新。" },
  { id: "ctf", label: "CTF", description: "用于新启动的 CTF Worker，按引擎投放到任务工作区。" },
  { id: "pentest", label: "渗透测试", description: "用于新启动的渗透 Worker；MCP 状态按 Run 隔离。" },
];
const ENGINES: Record<string, string> = {
  claude: "Claude", codex: "Codex", cursor: "Cursor", pi: "Pi",
  omp: "OMP", kimi: "Kimi", grok: "Grok", opencode: "OpenCode",
};

async function request(path: string, method = "GET", body?: unknown, base = "/api/agent-extensions") {
  const response = await apiFetch(`${base}${path}`, {
    method,
    headers: { "Content-Type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const data = await response.json();
  if (!response.ok) {
    const detail = data.detail;
    throw new Error(typeof detail === "string" ? detail : detail?.message || "操作失败，请稍后重试");
  }
  return data;
}

function ModeChoices({ value, onChange }: { value: Mode[]; onChange: (next: Mode[]) => void }) {
  return <div className="ae-mode-choices">
    {MODES.map((mode) => <label key={mode.id}>
      <input type="checkbox" checked={value.includes(mode.id)} onChange={(event) => {
        const next = event.target.checked ? [...value, mode.id] : value.filter((item) => item !== mode.id);
        onChange(next);
      }} />
      <span><strong>{mode.label}</strong><small>{mode.description}</small></span>
    </label>)}
  </div>;
}

export function AgentExtensionsSettings() {
  const params = useSearchParams();
  const requestedMode = params.get("mode");
  const [mode, setMode] = useState<Mode>(requestedMode === "ctf" || requestedMode === "pentest" ? requestedMode : "chat");
  const [runId, setRunId] = useState(params.get("run") || "");
  const runs = useRunList(15000);
  const [sourceTab, setSourceTab] = useState<SourceTab>(params.get("source") === "native" ? "native" : "managed");
  const [engine, setEngine] = useState(ENGINES[params.get("engine") || ""] ? params.get("engine")! : "codex");
  const [catalog, setCatalog] = useState<Catalog | null>(null);
  const [query, setQuery] = useState("");
  const [filter, setFilter] = useState<Filter>(
    ["skill", "mcp", "plugin"].includes(params.get("type") || "") ? params.get("type") as Filter : "all");
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [busy, setBusy] = useState(false);
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
  const detail = catalog?.packages.find((item) => item.id === detailId);
  const selectedMode = MODES.find((item) => item.id === mode)!;
  const selectedRun = runs.find((item) => item.run_id === runId);
  const catalogPath = `?engine=${encodeURIComponent(engine)}&mode=${mode}${runId ? `&run_id=${encodeURIComponent(runId)}` : ""}`;
  const currentCatalogPath = useRef(catalogPath);
  const catalogRequestId = useRef(0);
  currentCatalogPath.current = catalogPath;

  const reload = useCallback(async () => {
    // A mutation may finish after the operator switches Run or engine. Its
    // closure still has the old path, so never let that response replace the
    // catalog for the current selection.
    if (currentCatalogPath.current !== catalogPath) return;
    const requestId = ++catalogRequestId.current;
    const data = await request(catalogPath);
    if (currentCatalogPath.current === catalogPath && catalogRequestId.current === requestId) {
      setCatalog(data);
      setError("");
    }
  }, [catalogPath]);
  useEffect(() => {
    setCatalog(null);
    let active = true;
    void reload().catch((cause) => {
      if (active && currentCatalogPath.current === catalogPath) {
        setError(cause instanceof Error ? cause.message : String(cause));
      }
    });
    return () => { active = false; };
  }, [catalogPath, reload]);

  const mutate = async (action: () => Promise<unknown>, message: string) => {
    setBusy(true); setError(""); setNotice("");
    try {
      await action();
      await reload();
      setNotice(message);
      window.dispatchEvent(new Event("muteki:chat-plugins-changed"));
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally { setBusy(false); }
  };

  const setActiveMode = (next: Mode) => {
    setMode(next); setNotice(""); setError("");
    const url = new URL(window.location.href);
    if (next === "chat") url.searchParams.delete("mode"); else url.searchParams.set("mode", next);
    if (next === "chat" || (runId && selectedRun?.mode !== next)) {
      setRunId(""); url.searchParams.delete("run");
    }
    window.history.replaceState(window.history.state ?? {}, "", `${url.pathname}${url.search}`);
  };
  const selectRun = (next: string) => {
    setRunId(next);
    const url = new URL(window.location.href);
    if (next) url.searchParams.set("run", next); else url.searchParams.delete("run");
    window.history.replaceState(window.history.state ?? {}, "", `${url.pathname}${url.search}`);
  };
  const togglePackageMode = (item: Package, enabled: boolean) => {
    const modes = enabled ? [...item.modes, mode] : item.modes.filter((entry) => entry !== mode);
    if (!modes.length) { setError("至少保留一个使用场景；若要全部停用，请在详情中关闭全局启用。"); return; }
    void mutate(() => request(`/packages/${encodeURIComponent(item.id)}`, "PATCH", { modes }),
      `${item.name} 在${selectedMode.label}中已${enabled ? "启用" : "停用"}。新 Worker 将采用此配置。`);
  };
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
    setInstallOpen(false); setSource(""); setRef(""); setName("");
  }, "扩展已安装，所选场景将在下一次 Agent 启动或会话轮次中读取兼容组件。");

  const packages = useMemo(() => (catalog?.packages || []).filter((item) => {
    const matches = `${item.name} ${item.description} ${item.skills.map((skill) => skill.name).join(" ")} ${item.mcp_servers.join(" ")}`
      .toLowerCase().includes(query.trim().toLowerCase());
    const hasNative = Boolean(item.native_components.length || Object.values(item.worker_compatibility || {}).some((row) => row.components.includes("原生插件")));
    const typeMatches = filter === "all" || (filter === "skill" && item.skills.length > 0)
      || (filter === "mcp" && item.mcp_servers.length > 0) || (filter === "plugin" && hasNative);
    return matches && typeMatches;
  }), [catalog, filter, query]);
  const browserEnabled = catalog?.builtin_skills?.find((item) => item.id === "agent-browser")?.enabled ?? false;
  const modeCount = (selected: Mode) =>
    (catalog?.packages.filter((item) => item.enabled && item.modes.includes(selected)).length || 0)
    + (selected === "pentest" && browserEnabled ? 1 : 0);
  const activeCount = modeCount(mode);
  const browserVisible = mode === "pentest" && (filter === "all" || filter === "skill")
    && "agent-browser 浏览器 截图".toLowerCase().includes(query.trim().toLowerCase());
  const nativeSkills = (catalog?.native_skills || []).filter((item) => filter !== "mcp" && filter !== "plugin" && item.name.toLowerCase().includes(query.toLowerCase()));
  const nativeMcp = (catalog?.native_mcp || []).filter((item) => filter !== "skill" && filter !== "plugin" && item.name.toLowerCase().includes(query.toLowerCase()));

  return <div className="agent-extensions cx-root" data-testid="agent-extensions-settings">
    <div className="ae-mode-bar" role="tablist" aria-label="扩展使用场景">
      {MODES.map((item) => <button key={item.id} type="button" role="tab" aria-selected={mode === item.id}
        className={mode === item.id ? "active" : ""} onClick={() => setActiveMode(item.id)}>
        {item.label}<span>{modeCount(item.id)}</span>
      </button>)}
    </div>

    <div className="ae-intro">
      <div><span className="ae-eyebrow">AGENT EXTENSIONS / {mode.toUpperCase()}</span>
        <h2>{selectedMode.label} 能力</h2><p>{mode === "chat"
          ? "已启用的 Skill、插件与 MCP 在下一轮聊天读取。"
          : `新启动的${selectedMode.label} Worker 读取已分配的 Skill 和兼容插件；MCP 工具按 Run 连接。${runId ? ` 当前查看 ${runId} 的连接观测。` : "打开具体 Run 时可查看连接观测。"}`}</p></div>
      <div className="ae-intro-count"><strong>{activeCount}</strong><span>此场景已启用</span></div>
    </div>

    <div className="ae-toolbar">
      <div className="ae-source" role="group" aria-label="扩展来源">
        <button type="button" className={sourceTab === "managed" ? "active" : ""} onClick={() => setSourceTab("managed")}>Muteki 安装</button>
        <button type="button" className={sourceTab === "native" ? "active" : ""} onClick={() => setSourceTab("native")}>本机 Agent · 只读</button>
      </div>
      <button type="button" className="ae-add" onClick={openInstall}><Icon name="plus" size={16} /> 添加扩展</button>
    </div>
    {mode !== "chat" && <div className="ae-run-picker">
      <label htmlFor="ae-run-health">查看 Run 的 MCP 连接</label>
      <select id="ae-run-health" value={runId} onChange={(event) => selectRun(event.target.value)}>
        <option value="">未选择 Run（仅显示配置与兼容性）</option>
        {runs.filter((item) => item.mode === mode).map((item) =>
          <option key={item.run_id} value={item.run_id}>{item.name || item.run_id} · {item.run_id}{item.finished ? " · 已结束" : ""}</option>)}
      </select>
      {runId && <button type="button" onClick={() => void reload().catch((cause) => {
        if (currentCatalogPath.current === catalogPath) {
          setError(cause instanceof Error ? cause.message : String(cause));
        }
      })}>刷新连接状态</button>}
      {selectedRun?.finished && <small>该 Run 已结束，MCP 连接已释放。</small>}
    </div>}
    <div className="ae-filters">
      <label className="ae-search"><Icon name="search" size={16} /><input aria-label="搜索扩展" value={query} onChange={(event) => setQuery(event.target.value)} placeholder="搜索名称、Skill 或 MCP" /></label>
      <select aria-label="查看引擎兼容性" value={engine} onChange={(event) => setEngine(event.target.value)}>
        {Object.entries(ENGINES).map(([id, label]) => <option key={id} value={id}>{label}</option>)}
      </select>
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
        <div className="ae-row-action"><Switch label={<span className="sr-only">agent-browser 在渗透测试中可用</span>}
          checked={browserEnabled} disabled={busy || !catalog} onCheckedChange={toggleBuiltinBrowser} /></div>
      </div>}
      {packages.map((item) => {
        const compatible = mode === "chat" ? item.compatibility?.[engine] : item.worker_compatibility?.[engine];
        const available = Boolean(compatible && compatible.status !== "blocked" && compatible.status !== "unavailable");
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
        return <div className="ae-row" key={item.id} data-plugin-id={item.id}>
          <div className="ae-row-name"><span className="ae-row-icon"><Icon name={item.mcp_servers.length ? "plug" : "sparkles"} size={17} /></span>
            <div><button type="button" onClick={() => setDetailId(item.id)}>{item.name}</button><p>{item.description}</p>
              <small>{item.version} · {item.enabled ? "全局启用" : "全局停用"}</small></div></div>
          <div className="ae-row-components">{item.skills.length > 0 && <span>{item.skills.length} Skill</span>}{item.mcp_servers.length > 0 && <span>{item.mcp_servers.length} MCP</span>}
            {Object.values(item.worker_compatibility || {}).some((row) => row.components.includes("原生插件")) && <span>原生插件</span>}</div>
          <div className="ae-row-engine"><span className={available ? "available" : "unavailable"}>{available ? "兼容" : "不兼容"}</span><small>{compatible?.components.join(" · ") || "无可投放组件"}</small>{mcpState && <small>{mcpState}</small>}</div>
          <div className="ae-row-action"><Switch label={<span className="sr-only">{item.name} 在{selectedMode.label}中可用</span>}
            checked={selected} disabled={busy || !available} onCheckedChange={(value) => togglePackageMode(item, value)} />
            <button type="button" onClick={() => setDetailId(item.id)}>详情</button></div>
          {item.diagnostics.map((diagnostic) => <p className="ae-row-diagnostic" key={diagnostic}>{diagnostic}</p>)}
        </div>;
      })}
      {!catalog && !error && <div className="ae-empty">正在读取已安装扩展…</div>}
      {catalog && packages.length === 0 && !browserVisible && <div className="ae-empty"><Icon name="layers" size={23} /><strong>{query || filter !== "all" ? "没有匹配的扩展" : "尚未安装扩展"}</strong>
        <p>{query || filter !== "all" ? "试试其他名称或类型。" : `添加 Skill、插件或 MCP，并选择在${selectedMode.label}中使用。`}</p>
        {!query && filter === "all" && <button type="button" onClick={openInstall}>添加扩展</button>}</div>}
    </section> : <section className="ae-native" aria-label="本机 Agent 能力">
      <p>这里仅展示 {ENGINES[engine]} 的本机配置。Worker 使用 Muteki 安装并明确启用的包；本机 Agent 的私有目录不会直接挂载进任务容器。</p>
      {[...nativeSkills.map((item) => ({ key: item.id, name: item.name, info: `${item.source} · Skill`, description: item.description })),
        ...nativeMcp.map((item) => ({ key: item.name, name: item.name, info: `${item.source} · MCP · ${item.enabled ? "已配置" : "已停用"}`, description: "" }))]
        .map((item) => <div className="ae-native-row" key={item.key}><strong>{item.name}</strong><span>{item.info}</span>{item.description && <p>{item.description}</p>}</div>)}
      {!nativeSkills.length && !nativeMcp.length && <div className="ae-empty">没有发现匹配的本机能力</div>}
    </section>}

    <p className="ae-boundary"><Icon name="lock" size={14} /> {mode === "pentest"
      ? "设置只影响新启动的 Worker。agent-browser 会话协调依赖受管命令入口，不能替代容器网络与文件系统隔离；已投放扩展的内容会在下次 Worker 启动时重新校验。"
      : `包保存在 Muteki 私有目录。${mode === "chat" ? "聊天会话在下一轮读取变更。" : "新启动的 Worker 读取变更；运行中的 Worker 保留已投放的 Skill 快照。"}`}</p>

    <Dialog open={installOpen} onOpenChange={(open) => { if (!busy) { setInstallOpen(open); if (!open) setError(""); } }}
      title="添加 Agent 扩展" description="导入一个包或连接 MCP，并明确选择它作用于哪些场景。" footer={<>
        <Button onClick={() => setInstallOpen(false)} disabled={busy}>取消</Button>
        <Button variant="primary" loading={busy} disabled={!installModes.length || (installType === "mcp" ? !name.trim() : !source.trim() || (kind === "git" && !ref.trim()))} onClick={install}>{installType === "mcp" ? "添加 MCP" : "导入扩展"}</Button>
      </>}>
      <div className="ae-dialog-content">
        <div className="ae-dialog-tabs"><button type="button" aria-pressed={installType === "package"} onClick={() => setInstallType("package")}>Skill / 插件</button><button type="button" aria-pressed={installType === "mcp"} onClick={() => setInstallType("mcp")}>MCP 服务</button></div>
        {installType === "mcp" ? <><div><Label htmlFor="ae-mcp-name">名称</Label><Input id="ae-mcp-name" value={name} onChange={(event) => setName(event.target.value)} placeholder="my-tools" /></div>
          <TextArea label="连接配置（JSON）" value={config} onChange={(event) => setConfig(event.target.value)} rows={8} className="font-cx-mono" />
          <p>支持 HTTP 与 stdio。Worker 调用由宿主转发，MCP 进程状态按 Run 隔离。</p></>
          : <><div><Label htmlFor="ae-source-kind">来源</Label><select id="ae-source-kind" value={kind} onChange={(event) => setKind(event.target.value)}><option value="local-dir">本地目录</option><option value="archive">压缩包</option><option value="git">固定 Git 版本</option></select></div>
            <div><Label htmlFor="ae-source-path">{kind === "git" ? "仓库地址" : "来源路径"}</Label><Input id="ae-source-path" value={source} onChange={(event) => setSource(event.target.value)} placeholder={kind === "git" ? "https://github.com/owner/plugin.git" : "/path/to/plugin"} /></div>
            {kind === "git" && <div><Label htmlFor="ae-source-ref">Tag 或 Commit</Label><Input id="ae-source-ref" value={ref} onChange={(event) => setRef(event.target.value)} placeholder="v1.0.0" /></div>}</>}
        <div><strong className="ae-dialog-label">使用场景</strong><ModeChoices value={installModes} onChange={setInstallModes} /></div>
        {error && <p className="ae-dialog-error" role="alert">{error}</p>}
      </div>
    </Dialog>

    <Dialog open={!!detail} onOpenChange={(open) => { if (!open) setDetailId(""); }} title={detail?.name || "扩展详情"} description={detail?.description} footer={<>
      <Button variant="ghost" onClick={() => { if (detail) setRemove(detail); setDetailId(""); }}>卸载</Button>
      <Button onClick={() => setDetailId("")}>完成</Button>
    </>}>
      {detail && <div className="ae-dialog-content"><p>版本 {detail.version} · {detail.digest.slice(0, 12)}</p>
        <Switch label="全局启用" checked={detail.enabled} disabled={busy} onCheckedChange={(value) => void mutate(() => request(`/packages/${encodeURIComponent(detail.id)}`, "PATCH", { enabled: value }), value ? "扩展已启用" : "扩展已停用")} />
        <div><strong className="ae-dialog-label">适用场景</strong><ModeChoices value={detail.modes} onChange={(modes) => {
          if (!modes.length) { setError("至少保留一个使用场景；可关闭全局启用。"); return; }
          void mutate(() => request(`/packages/${encodeURIComponent(detail.id)}`, "PATCH", { modes }), "适用场景已更新，新 Worker 启动时生效。");
        }} /></div>
        <div><strong className="ae-dialog-label">包含的组件</strong><p>{detail.skills.map((item) => item.name).join("、") || "无 Skill"} · {detail.mcp_servers.join("、") || "无 MCP"}</p></div>
        {!!detail.hook_commands?.length && <div><strong className="ae-dialog-label">原生 Hooks</strong><p>仅在支持该 ABI 的聊天 Agent 使用；启用需绑定当前包版本。</p><pre>{JSON.stringify(detail.hook_definitions || detail.hook_commands, null, 2)}</pre>
          <Switch label="启用此版本的 Hooks" checked={Boolean(detail.native_hooks_enabled)} disabled={busy} onCheckedChange={(value) => void mutate(() => request(`/packages/${encodeURIComponent(detail.id)}`, "PATCH", { native_hooks: value, digest: detail.digest }), value ? "Hooks 已启用" : "Hooks 已停用")} /></div>}
        <div><strong className="ae-dialog-label">Worker 引擎兼容性</strong><div className="ae-compatibility">{Object.entries(ENGINES).map(([id, label]) => {
          const result = detail.worker_compatibility?.[id];
          return <div key={id}><strong>{label}</strong><span>{result?.status === "available" ? result.components.join("、") : "无兼容组件"}</span></div>;
        })}</div></div>
        {detail.can_rollback && <Button variant="outline" size="sm" disabled={busy} onClick={() => void mutate(() => request(`/packages/${encodeURIComponent(detail.id)}`, "PATCH", { rollback: true }), "已恢复上一版本")}>恢复上一版本</Button>}
        {notice && <p role="status">{notice}</p>}{error && <p className="ae-dialog-error" role="alert">{error}</p>}
      </div>}
    </Dialog>
    <Dialog open={!!remove} onOpenChange={(open) => { if (!busy && !open) setRemove(null); }} title={`卸载 ${remove?.name || "扩展"}？`}
      description="从 Muteki 安装目录移除。已运行的 Worker 保留本次快照。" tone="danger" footer={<>
        <Button disabled={busy} onClick={() => setRemove(null)}>取消</Button><Button variant="danger" loading={busy} onClick={() => void mutate(async () => { await request(`/packages/${encodeURIComponent(remove?.id || "")}`, "DELETE"); setRemove(null); }, "扩展已卸载")}>卸载</Button>
      </>} />
  </div>;
}
