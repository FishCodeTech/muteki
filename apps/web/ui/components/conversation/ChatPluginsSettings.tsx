"use client";

import { useCallback, useEffect, useState } from "react";
import { useSearchParams } from "next/navigation";
import { Icon } from "@/components/Icon";
import { Button, Callout, Dialog, Input, Label, SegmentedControl, Spinner, Switch, TextArea } from "@/components/chat/ui";
import { apiFetch } from "@/lib/useRun";

const LABELS: Record<string, string> = { claude: "Claude", codex: "Codex", cursor: "Cursor", pi: "Pi", omp: "OMP", kimi: "Kimi", grok: "Grok", opencode: "OpenCode" };
type Package = {
  id: string; name: string; description: string; version: string; enabled: boolean;
  engines: string[]; skills: { name: string; description: string }[]; mcp_servers: string[];
  native_components: string[]; diagnostics: string[]; can_rollback: boolean;
  digest: string; native_hooks_enabled?: boolean; hook_commands?: string[]; hook_definitions?: unknown[];
  compatibility?: Record<string, { status: string; components: string[]; unavailable: string[]; reasons: string[]; required_tools: string[] }>;
};
type Catalog = {
  engine: string; packages: Package[]; control_enabled: boolean;
  native_skills: { id: string; name: string; description: string; source: string }[];
  native_mcp: { name: string; enabled: boolean; source: string; transport: string }[];
};

async function request(path: string, method = "GET", body?: unknown) {
  const response = await apiFetch(`/api/chat-plugins${path}`, {
    method, headers: { "Content-Type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const data = await response.json();
  if (!response.ok) throw new Error(typeof data.detail === "string" ? data.detail : "操作失败，请稍后重试");
  return data;
}

export function ChatPluginsSettings() {
  const params = useSearchParams();
  const [sourceTab, setSourceTab] = useState(params.get("source") === "native" ? "native" : "managed");
  const [nativeEngine, setNativeEngine] = useState(LABELS[params.get("engine") || ""] ? params.get("engine")! : "codex");
  const [catalog, setCatalog] = useState<Catalog | null>(null);
  const [query, setQuery] = useState("");
  const [filter, setFilter] = useState(["skill", "mcp"].includes(params.get("type") || "") ? params.get("type")! : "all");
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [busy, setBusy] = useState(false);
  const [installOpen, setInstallOpen] = useState(false);
  const [installType, setInstallType] = useState("package");
  const [kind, setKind] = useState("local-dir");
  const [source, setSource] = useState("");
  const [ref, setRef] = useState("");
  const [name, setName] = useState("");
  const [config, setConfig] = useState('{\n  "mcpServers": {\n    "my-server": { "url": "https://example.com/mcp" }\n  }\n}');
  const [detailId, setDetailId] = useState("");
  const [remove, setRemove] = useState<Package | null>(null);
  const detail = catalog?.packages.find((p) => p.id === detailId);
  const reload = useCallback(async () => setCatalog(await request(`?engine=${nativeEngine}`)), [nativeEngine]);

  useEffect(() => {
    let active = true;
    request(`?engine=${nativeEngine}`).then((data) => { if (active) setCatalog(data); })
      .catch((e) => { if (active) setError(e.message); });
    return () => { active = false; };
  }, [nativeEngine]);

  const mutate = async (action: () => Promise<unknown>, message: string) => {
    setBusy(true); setError(""); setNotice("");
    try {
      await action(); await reload(); setNotice(message);
      window.dispatchEvent(new Event("muteki:chat-plugins-changed"));
    } catch (e) { setError(e instanceof Error ? e.message : String(e)); }
    finally { setBusy(false); }
  };
  const install = () => mutate(async () => {
    if (installType === "mcp") {
      let parsed;
      try { parsed = JSON.parse(config); } catch { throw new Error("配置不是有效的 JSON，请检查括号、引号和逗号。"); }
      await request("/mcp", "POST", { name, servers: parsed.mcpServers || parsed });
    } else {
      await request("/install", "POST", { source: { kind, ...(kind === "git" ? { url: source, ref } : { path: source }) } });
    }
    setInstallOpen(false); setSource(""); setName(""); setRef("");
  }, "已添加，所有兼容的聊天引擎均可使用。现有聊天将在下一轮更新。");
  const matches = (text: string) => text.toLowerCase().includes(query.toLowerCase());
  const packages = (catalog?.packages || []).filter((p) => matches(`${p.name} ${p.description}`)
    && (filter === "all" || (filter === "skill" ? p.skills.length > 0 : p.mcp_servers.length > 0)));
  const showBuiltin = filter === "all" && matches("Muteki 平台工具 muteki-control 内置 任务 运行");
  const nativeReady = catalog?.engine === nativeEngine;
  const nativeSkills = (nativeReady ? catalog.native_skills : []).filter((p) => filter !== "mcp" && matches(`${p.name} ${p.description}`));
  const nativeMcp = (nativeReady ? catalog.native_mcp : []).filter((p) => filter !== "skill" && matches(p.name));

  return <div className="cx-root mx-auto w-full max-w-4xl" data-testid="chat-plugins-settings">
    <div className="mb-6 flex flex-wrap items-center justify-between gap-3">
      <SegmentedControl ariaLabel="扩展来源" value={sourceTab} onChange={(tab) => { setSourceTab(tab); setQuery(""); setNotice(""); setError(""); }} options={[
        { value: "managed", label: "Muteki 扩展" }, { value: "native", label: "本机 Agent" },
      ]} />
      <Button variant="primary" icon="plus" onClick={() => { setError(""); setInstallOpen(true); }}>添加扩展</Button>
    </div>
    <div className="mb-4 flex flex-wrap items-center gap-3">
      <Input aria-label="搜索扩展" icon="search" placeholder={sourceTab === "managed" ? "搜索已安装的扩展" : "搜索本机能力"} value={query} onChange={(e) => setQuery(e.target.value)} className="min-w-0 flex-1 basis-48" />
      {sourceTab === "native" && <select aria-label="查看本机 Agent" disabled={busy} value={nativeEngine} onChange={(e) => setNativeEngine(e.target.value)} className="rounded-lg border border-cx-border bg-cx-elevated px-3 py-2 text-sm text-cx-fg">
        {Object.entries(LABELS).map(([id, label]) => <option key={id} value={id}>{label}</option>)}
      </select>}
      <SegmentedControl ariaLabel="扩展类型" value={filter} onChange={setFilter} options={[
        { value: "all", label: "全部" }, { value: "skill", label: "Skills" }, { value: "mcp", label: "MCP" },
      ]} />
    </div>
    <p className="mb-5 text-xs leading-5 text-cx-fg-3">{sourceTab === "managed"
      ? "安装一次，统一管理。已启用的扩展会自动用于所有兼容的聊天引擎。"
      : `只读查看 ${LABELS[nativeEngine]} 已有的能力，仅在使用它聊天时提供。这里不会修改本机配置。`}</p>
    {error && !installOpen && <div className="mb-4" role="alert"><Callout tone="danger">{error}</Callout></div>}
    {notice && <div className="mb-4" role="status"><Callout>{notice}</Callout></div>}
    {!catalog && !error && <div className="flex justify-center py-12"><Spinner /></div>}
    {sourceTab === "managed" && catalog && <div className="divide-y divide-cx-border border-y border-cx-border">
      {showBuiltin && <div className="flex items-center gap-4 py-5">
        <span className="grid size-10 shrink-0 place-items-center rounded-xl bg-cx-hover text-cx-fg-2"><Icon name="network" size={19}/></span>
        <div className="min-w-0 flex-1"><div className="flex flex-wrap items-center gap-2"><span className="text-sm font-medium">Muteki 平台工具</span><span className="rounded bg-cx-hover px-1.5 py-0.5 text-[10px] text-cx-fg-3">内置</span></div>
          <p className="mt-1 text-xs leading-5 text-cx-fg-3">在聊天中访问 Muteki 任务、运行状态与平台功能。</p>
        </div>
        <Switch label={<span className="sr-only">启用 Muteki 平台工具</span>} checked={catalog.control_enabled} disabled={busy} onCheckedChange={(enabled) => void mutate(() => request("/control", "PUT", { enabled }), enabled ? "已启用平台工具" : "已停用聊天中的平台工具")} />
      </div>}
      {packages.map((p) => <div key={p.id} className="flex items-start gap-4 py-5" data-plugin-id={p.id}>
        <span className="grid size-10 shrink-0 place-items-center rounded-xl bg-cx-hover text-cx-fg-2"><Icon name={p.mcp_servers.length ? "plug" : "sparkles"} size={19}/></span>
        <div className="min-w-0 flex-1">
          <button type="button" onClick={() => { setError(""); setNotice(""); setDetailId(p.id); }} className="break-all text-left text-sm font-medium hover:underline">{p.name}</button>
          <p className="mt-1 break-words text-xs leading-5 text-cx-fg-3">{p.description}</p>
          <div className="mt-2 flex flex-wrap items-center gap-x-3 gap-y-1 text-[11px] text-cx-fg-4">
            {p.skills.length > 0 && <span>{p.skills.length} 个 Skill</span>}{p.mcp_servers.length > 0 && <span>{p.mcp_servers.length} 个 MCP</span>}
            <span>{p.enabled ? "已启用" : "已停用"}</span><span>{p.version}</span>
            {p.compatibility && <span>{Object.values(p.compatibility).filter((c) => c.status !== "blocked").length} 个引擎可用{Object.values(p.compatibility).some((c) => c.status !== "supported") ? " · 查看兼容详情" : ""}</span>}
            <button type="button" className="text-cx-fg-3 hover:text-cx-fg" onClick={() => { setError(""); setNotice(""); setDetailId(p.id); }}>详情</button>
          </div>
          {p.diagnostics.map((d) => <p key={d} className="mt-2 text-xs text-cx-danger">{d}</p>)}
        </div>
        <Switch label={<span className="sr-only">启用 {p.name}</span>} checked={p.enabled} disabled={busy} onCheckedChange={(enabled) => void mutate(() => request(`/packages/${encodeURIComponent(p.id)}`, "PATCH", { enabled }), enabled ? `已启用 ${p.name}` : `已停用 ${p.name}`)} />
      </div>)}
      {!packages.length && !showBuiltin && <p className="py-12 text-center text-sm text-cx-fg-3">没有找到匹配的扩展</p>}
      {!catalog.packages.length && !query && filter === "all" && <div className="py-10 text-center"><p className="text-sm text-cx-fg-2">添加你需要的 Skills 和工具</p><p className="mt-2 text-xs text-cx-fg-3">导入现有插件，或连接一个 MCP 服务。</p><Button className="mt-4" variant="outline" icon="plus" onClick={() => setInstallOpen(true)}>添加扩展</Button></div>}
    </div>}
    {sourceTab === "native" && catalog && <div className="divide-y divide-cx-border border-y border-cx-border">
      {nativeSkills.map((s) => <div key={s.id} className="flex gap-3 py-4"><Icon name="sparkles" size={16} className="mt-1 shrink-0 text-cx-fg-3"/><div className="min-w-0"><p className="break-words text-sm font-medium">{s.name}</p><p className="mt-1 break-words text-xs leading-5 text-cx-fg-3">{s.description}</p><p className="mt-1 text-[11px] text-cx-fg-4">{s.source} · Skill</p></div></div>)}
      {nativeMcp.map((m) => <div key={m.name} className="flex gap-3 py-4"><Icon name="plug" size={16} className="mt-1 shrink-0 text-cx-fg-3"/><div><p className="text-sm font-medium">{m.name}</p><p className="mt-1 text-xs text-cx-fg-3">MCP · {m.enabled ? "已配置" : "已停用"}</p></div></div>)}
      {!nativeReady ? <div className="flex justify-center py-12"><Spinner /></div> : !nativeSkills.length && !nativeMcp.length && <p className="py-12 text-center text-sm text-cx-fg-3">{query ? "没有匹配的能力" : "没有发现本机能力"}</p>}
    </div>}
    <div className="mt-5 flex items-start gap-2 text-[11px] leading-5 text-cx-fg-4"><Icon name="lock" size={13} className="mt-1 shrink-0"/><span>{sourceTab === "managed" ? "扩展保存在 Muteki 独立目录，仅作用于聊天；不会安装到本机 Agent，也不会自动带入做题任务。" : "本机配置与实际连接状态可能不同。原生命令和工具的可用性，以聊天会话公布的目录为准。"}</span></div>

    <Dialog open={installOpen} onOpenChange={(open) => { if (!busy) { setInstallOpen(open); if (!open) setError(""); } }} title="添加聊天扩展" description="安装后自动用于所有兼容的聊天引擎。" footer={<>
      <Button disabled={busy} onClick={() => { setInstallOpen(false); setError(""); }}>取消</Button>
      <Button variant="primary" loading={busy} disabled={installType === "mcp" ? !name.trim() : !source.trim() || (kind === "git" && !ref.trim())} onClick={() => void install()}>{installType === "mcp" ? "添加 MCP" : "导入"}</Button>
    </>}>
      <div className="space-y-5">
        <div><SegmentedControl ariaLabel="添加类型" value={installType} onChange={(value) => { setInstallType(value); setError(""); }} options={[{ value: "package", label: "Skill / 插件" }, { value: "mcp", label: "MCP 服务" }]} /></div>
        {installType === "mcp" ? <>
          <div><Label htmlFor="mcp-name">名称</Label><Input id="mcp-name" value={name} onChange={(e) => setName(e.target.value)} placeholder="my-tools" /></div>
          <TextArea label="连接配置（JSON）" value={config} onChange={(e) => setConfig(e.target.value)} rows={9} className="font-cx-mono" />
          <p className="text-xs leading-5 text-cx-fg-3">支持 HTTP 和 stdio。stdio 使用独立的数据目录，默认关闭外网；需要联网时可配置 network 数组。</p>
        </> : <>
          <div><Label>导入来源</Label><SegmentedControl ariaLabel="导入来源" value={kind} onChange={setKind} options={[{ value: "local-dir", label: "本地目录" }, { value: "git", label: "Git 仓库" }, { value: "archive", label: "压缩包" }]} /></div>
          <div><Label htmlFor="plugin-source">{kind === "git" ? "仓库地址" : "来源路径"}</Label><Input id="plugin-source" value={source} onChange={(e) => setSource(e.target.value)} placeholder={kind === "git" ? "https://github.com/owner/plugin.git" : kind === "archive" ? "/path/to/plugin.zip" : "/path/to/skill-or-plugin"} /><p className="mt-2 text-xs leading-5 text-cx-fg-3">{kind === "git" ? "从指定版本导入，更新时可再次导入同名插件。" : "填写运行 Muteki 的机器上的路径。原始文件保持不变。"}</p></div>
          {kind === "git" && <div><Label htmlFor="plugin-ref">版本（Tag 或 Commit）</Label><Input id="plugin-ref" value={ref} onChange={(e) => setRef(e.target.value)} placeholder="v1.0.0" /></div>}
        </>}
        {error && <p role="alert" className="text-sm text-cx-danger">{error}</p>}
      </div>
    </Dialog>
    <Dialog open={!!detail} onOpenChange={(open) => { if (!open) setDetailId(""); }} title={detail?.name || "扩展详情"} description={detail?.description} footer={<>
      <Button variant="ghost" onClick={() => { if (detail) setRemove(detail); setDetailId(""); }}>卸载</Button>
      <Button onClick={() => setDetailId("")}>完成</Button>
    </>}>
      {detail && <div className="space-y-5 text-sm">
        <div className="flex justify-between text-xs text-cx-fg-3"><span>版本 {detail.version}</span><span>{detail.enabled ? "已启用" : "已停用"} · 仅 Muteki 聊天</span></div>
        {detail.skills.length > 0 && <div><p className="mb-2 font-medium">Skills</p>{detail.skills.map((s) => <p key={s.name} className="py-1 text-cx-fg-3">{s.name}</p>)}</div>}
        {detail.mcp_servers.length > 0 && <div><p className="mb-2 font-medium">MCP 服务</p>{detail.mcp_servers.map((s) => <p key={s} className="py-1 text-cx-fg-3">{s}</p>)}<Button className="mt-3" variant="outline" size="sm" loading={busy} onClick={() => void mutate(async () => { const r = await request(`/check/${encodeURIComponent(detail.id)}`, "POST"); if (r.diagnostics.length) throw new Error(r.diagnostics.join("；")); }, "MCP 连接正常")}>检查连接</Button></div>}
        {!!detail.hook_commands?.length && <div className="space-y-3"><p className="font-medium">自动执行的 Hooks</p><p className="text-xs text-cx-fg-3">在聊天的独立目录中执行以下命令。启用仅适用于当前版本；更新后需要重新查看。</p>
          <pre className="max-h-52 overflow-auto whitespace-pre-wrap break-all rounded bg-cx-bg-2 p-3 text-xs">{JSON.stringify(detail.hook_definitions || detail.hook_commands, null, 2)}</pre>
          <Switch label="启用当前版本的 Hooks" checked={Boolean(detail.native_hooks_enabled)} disabled={busy} onCheckedChange={(value) => void mutate(() => request(`/packages/${encodeURIComponent(detail.id)}`, "PATCH", { native_hooks: value, digest: detail.digest }), value ? "此版本的 Hooks 已启用" : "Hooks 已停用")} />
        </div>}
        <div><p className="mb-2 font-medium">引擎兼容性</p><div className="divide-y divide-cx-border">
          {Object.entries(detail.compatibility || {}).map(([engine, value]) => <div key={engine} className="flex gap-3 py-2 text-xs">
            <span className="w-20 shrink-0">{LABELS[engine]}</span><div className="min-w-0 flex-1 text-cx-fg-3">
              <p>{value.status === "supported" ? "可用" : value.status === "partial" ? "部分可用" : "不可用"}{value.components.length ? ` · ${value.components.join("、")}` : ""}</p>
              {value.unavailable.length > 0 && <p>当前未提供：{value.unavailable.join("、")}</p>}
              {value.reasons.map((r) => <p key={r}>{r}</p>)}
              {value.required_tools.length > 0 && <p>所需工具：{value.required_tools.join("、")}</p>}
            </div></div>)}
        </div></div>
        {detail.can_rollback && <Button variant="outline" size="sm" disabled={busy} onClick={() => void mutate(() => request(`/packages/${encodeURIComponent(detail.id)}`, "PATCH", { rollback: true }), "已恢复上一版本")}>恢复上一版本</Button>}
        {notice && <p role="status" className="text-xs text-cx-fg-2">{notice}</p>}
        {error && <p role="alert" className="text-xs text-cx-danger">{error}</p>}
      </div>}
    </Dialog>
    <Dialog open={!!remove} onOpenChange={(open) => { if (!busy && !open) setRemove(null); }} title={`卸载 ${remove?.name || "扩展"}？`} description="从 Muteki 聊天中移除。原始文件和本机 Agent 的安装不受影响。" tone="danger" footer={<>
      <Button disabled={busy} onClick={() => setRemove(null)}>取消</Button><Button variant="danger" loading={busy} onClick={() => void mutate(async () => { await request(`/packages/${encodeURIComponent(remove?.id || "")}`, "DELETE"); setRemove(null); }, "扩展已卸载")}>卸载</Button>
    </>} />
  </div>;
}
