import { useCallback, useEffect, useRef, useState, useSyncExternalStore } from 'react';
import { Button, Callout, Dialog, Input, Switch, TextArea } from '@/components/chat/ui';
import { Icon } from '@/components/Icon';
import { NativePathPicker } from '@/components/NativePathPicker';
import { NativeCapabilitiesPanel } from '@/components/NativeCapabilitiesPanel';
import { apiFetch, currentAuthScope } from '@/lib/useRun';
import { conversationStorageScope, subscribeConversationStorageScope } from '@/lib/conversationStorageScope';
import { useLang } from '@/lib/i18n';

type SourceKind = 'local-dir' | 'archive' | 'git';
type ExtensionPackage = {
  id: string; name: string; description: string; version: string; enabled: boolean;
  skills: Array<{ name: string; description: string }>; mcp_servers: string[];
  native_components: string[]; diagnostics: string[]; can_rollback: boolean; digest: string;
  native_hooks_enabled?: boolean; hook_commands?: string[]; hook_definitions?: unknown[];
  compatibility?: Record<string, unknown>;
};
type Catalog = {
  engine: string; packages: ExtensionPackage[]; control_enabled: boolean;
  host_discovery_enabled?: boolean;
  native_skills: Array<{ id: string; name: string; description: string; source: string }>;
  native_mcp: Array<{ name: string; enabled: boolean; source: string; transport: string }>;
};
export interface ChatExtensionsNavigation {
  pathname: string; searchParams: URLSearchParams; router: { replace: (href: string) => void };
}
const ENGINES: Record<string, string> = { claude: 'Claude', codex: 'Codex', cursor: 'Cursor', pi: 'Pi', omp: 'OMP', kimi: 'Kimi', grok: 'Grok', opencode: 'OpenCode' };
const record = (value: unknown): value is Record<string, unknown> => Boolean(value && typeof value === 'object' && !Array.isArray(value));
const strings = (value: unknown): value is string[] => Array.isArray(value) && value.every(item => typeof item === 'string');

export class ChatExtensionsError extends Error {
  constructor(readonly code: string, message: string, readonly rawBody = '', readonly httpStatus?: number) {
    super(message); this.name = 'ChatExtensionsError';
  }
}
export async function requestChatExtensions(path: string, method = 'GET', body?: unknown, signal?: AbortSignal): Promise<unknown> {
  const response = await apiFetch(`/api/chat-plugins${path}`, { method, signal,
    headers: { 'Content-Type': 'application/json' }, body: body === undefined ? undefined : JSON.stringify(body) });
  const raw = await response.text();
  let data: unknown;
  try { data = JSON.parse(raw); } catch {
    throw new ChatExtensionsError('chat_extensions.response_invalid', `HTTP ${response.status}: ${raw}`, raw, response.status);
  }
  if (!response.ok) {
    const error = record(data) && record(data.error) ? data.error : record(data) && record(data.detail) ? data.detail : null;
    const detail = error?.message ?? (record(data) ? data.detail : undefined);
    throw new ChatExtensionsError(typeof error?.code === 'string' ? error.code : 'chat_extensions.http_error',
      typeof detail === 'string' ? detail : `HTTP ${response.status}`, raw, response.status);
  }
  return data;
}
export function chatExtensionsCatalog(value: unknown, engine: string): Catalog {
  if (!record(value) || value.engine !== engine || typeof value.control_enabled !== 'boolean'
    || (value.host_discovery_enabled != null && typeof value.host_discovery_enabled !== 'boolean')
    || !Array.isArray(value.packages) || !Array.isArray(value.native_skills) || !Array.isArray(value.native_mcp)
    || value.packages.some(item => !record(item) || !['id', 'name', 'description', 'version', 'digest'].every(key => typeof item[key] === 'string')
      || !item.id || typeof item.enabled !== 'boolean' || typeof item.can_rollback !== 'boolean'
      || !strings(item.mcp_servers) || !strings(item.native_components) || !strings(item.diagnostics)
      || !Array.isArray(item.skills) || item.skills.some(skill => !record(skill) || typeof skill.name !== 'string' || typeof skill.description !== 'string'))
    || value.native_skills.some(item => !record(item) || !['id', 'name', 'description', 'source'].every(key => typeof item[key] === 'string'))
    || value.native_mcp.some(item => !record(item) || !['name', 'source', 'transport'].every(key => typeof item[key] === 'string') || typeof item.enabled !== 'boolean')) {
    throw new ChatExtensionsError('chat_extensions.catalog_invalid', 'chat_extensions.catalog_invalid', JSON.stringify(value, null, 2));
  }
  const ids = (value.packages as ExtensionPackage[]).map(item => item.id);
  if (new Set(ids).size !== ids.length) throw new ChatExtensionsError('chat_extensions.catalog_invalid', 'Duplicate package IDs', JSON.stringify(value, null, 2));
  return value as unknown as Catalog;
}

/** Chat-only desktop UI for the existing main-branch /api/chat-plugins contract. */
export function ChatExtensionsSettings({ navigation }: { navigation: ChatExtensionsNavigation }) {
  const { lang } = useLang(); const en = lang === 'en'; const t = (zh: string, english: string) => en ? english : zh;
  const scope = useSyncExternalStore(subscribeConversationStorageScope, conversationStorageScope, () => '');
  const [engine, setEngine] = useState(ENGINES[navigation.searchParams.get('engine') || ''] ? navigation.searchParams.get('engine')! : 'codex');
  const [sourceTab, setSourceTab] = useState(navigation.searchParams.get('source') === 'native' ? 'native' : 'managed');
  const [query, setQuery] = useState(''); const [catalogRecord, setCatalogRecord] = useState<{ owner: string; data: Catalog } | null>(null);
  const [error, setError] = useState<ChatExtensionsError | null>(null); const [notice, setNotice] = useState('');
  const [loading, setLoading] = useState(false); const [busy, setBusy] = useState(false); const [picking, setPicking] = useState(false);
  const [installOpen, setInstallOpen] = useState(false); const [installOwner, setInstallOwner] = useState(''); const [installType, setInstallType] = useState<'package' | 'mcp'>('package');
  const [kind, setKind] = useState<SourceKind>('local-dir'); const [source, setSource] = useState(''); const [ref, setRef] = useState(''); const [name, setName] = useState('');
  const [config, setConfig] = useState('{\n  "mcpServers": {}\n}'); const [detailId, setDetailId] = useState(''); const [removeRecord, setRemoveRecord] = useState<{ owner: string; item: ExtensionPackage } | null>(null);
  const owner = JSON.stringify([scope, engine]); const ownerRef = useRef(owner); ownerRef.current = owner;
  const requestId = useRef(0); const operation = useRef<{ owner: string } | null>(null); const abort = useRef<AbortController | null>(null);
  const catalog = catalogRecord?.owner === owner ? catalogRecord.data : null;
  const remove = removeRecord?.owner === owner ? removeRecord.item : null;
  const detail = catalog?.packages.find(item => item.id === detailId);
  const isCurrent = useCallback((snapshot: string) => ownerRef.current === snapshot && Boolean(scope) && currentAuthScope() === scope, [scope]);
  const reportError = (cause: unknown) => cause instanceof ChatExtensionsError ? cause : new ChatExtensionsError('chat_extensions.request_failed', cause instanceof Error ? cause.message : String(cause));
  const load = useCallback(async (snapshot = owner): Promise<Catalog | null> => {
    if (!isCurrent(snapshot)) return null;
    abort.current?.abort(); const controller = new AbortController(); abort.current = controller;
    const id = ++requestId.current; setLoading(true);
    try {
      const data = chatExtensionsCatalog(await requestChatExtensions(`?engine=${encodeURIComponent(engine)}`, 'GET', undefined, controller.signal), engine);
      if (isCurrent(snapshot) && id === requestId.current) { setCatalogRecord({ owner: snapshot, data }); setError(null); return data; }
      return null;
    } catch (cause) {
      if (controller.signal.aborted || id !== requestId.current || !isCurrent(snapshot)) return null;
      throw cause;
    } finally { if (isCurrent(snapshot) && id === requestId.current) setLoading(false); }
  }, [engine, isCurrent, owner]);
  useEffect(() => {
    setCatalogRecord(null); setError(null); setNotice(''); setLoading(false); setBusy(false); setPicking(false);
    setInstallOpen(false); setDetailId(''); setRemoveRecord(null); setSource(''); setName(''); setRef(''); setConfig('{\n  "mcpServers": {}\n}'); setKind('local-dir'); setInstallType('package');
    operation.current = null;
    if (scope) void load().catch(cause => { if (isCurrent(owner)) setError(reportError(cause)); });
    return () => { abort.current?.abort(); ++requestId.current; };
  }, [owner, scope, load, isCurrent]);
  const refresh = () => { setNotice(''); void load().catch(cause => { if (isCurrent(owner)) setError(reportError(cause)); }); };
  const mutate = async (action: () => Promise<unknown>, success: string, completed?: () => void, confirm?: (data: Catalog) => boolean) => {
    if (!isCurrent(owner) || operation.current) return;
    const ticket = { owner }; operation.current = ticket; setBusy(true); setError(null); setNotice(''); let applied = false;
    try {
      await action(); applied = true;
      if (!isCurrent(owner) || operation.current !== ticket) return;
      completed?.(); const refreshed = await load(owner);
      if (refreshed && confirm && !confirm(refreshed)) throw new ChatExtensionsError('chat_extensions.readback_mismatch', t('目录尚未确认本次变更，请重载核对。', 'The catalog did not confirm this change. Reload to verify it.'));
      if (refreshed && isCurrent(owner) && operation.current === ticket) setNotice(success);
    } catch (cause) {
      if (isCurrent(owner) && operation.current === ticket) {
        const failure = reportError(cause);
        setError(applied ? new ChatExtensionsError('chat_extensions.readback_failed', t('变更已返回，但目录刷新失败；请重载确认结果。', 'The change returned, but catalog refresh failed. Reload to verify the outcome.'), failure.rawBody || failure.message, failure.httpStatus) : failure);
      }
    } finally { if (operation.current === ticket) { operation.current = null; if (isCurrent(owner)) setBusy(false); } }
  };
  const install = () => {
    let installedId = '';
    void mutate(async () => {
      let result: unknown;
      if (installType === 'mcp') {
        let parsed: unknown; try { parsed = JSON.parse(config); } catch { throw new ChatExtensionsError('chat_extensions.mcp_invalid', t('MCP 配置不是有效 JSON。', 'MCP configuration is not valid JSON.')); }
        if (!record(parsed)) throw new ChatExtensionsError('chat_extensions.mcp_invalid', t('MCP 配置须为对象。', 'MCP configuration must be an object.'));
        result = await requestChatExtensions('/mcp', 'POST', { name: name.trim(), servers: parsed.mcpServers ?? parsed });
      } else {
        if (!source.trim()) throw new ChatExtensionsError('chat_extensions.source_missing', t('请填写服务可访问的来源。', 'Enter a source accessible to the service.'));
        result = await requestChatExtensions('/install', 'POST', { source: { kind, ...(kind === 'git' ? { url: source.trim(), ref: ref.trim() } : { path: source.trim() }) } });
      }
      if (!record(result) || typeof result.id !== 'string' || !result.id) throw new ChatExtensionsError('chat_extensions.mutation_response_invalid', t('服务未确认已导入的扩展，请重载核对。', 'The service did not confirm the imported package. Reload to verify it.'), JSON.stringify(result, null, 2));
      installedId = result.id;
    }, t('扩展已添加，聊天会在下一轮读取变更。', 'Extension added. Chat sessions read changes on their next turn.'), () => { setInstallOpen(false); setSource(''); setName(''); setRef(''); }, data => data.packages.some(item => item.id === installedId));
  };
  const update = (item: ExtensionPackage, body: Record<string, unknown>, success: string) => void mutate(async () => {
    const result = await requestChatExtensions(`/packages/${encodeURIComponent(item.id)}`, 'PATCH', body);
    if (!record(result) || result.id !== item.id || (typeof body.enabled === 'boolean' && result.enabled !== body.enabled)
      || (typeof body.native_hooks === 'boolean' && result.native_hooks_enabled !== body.native_hooks)) throw new ChatExtensionsError('chat_extensions.mutation_response_invalid', t('服务未确认扩展变更，请重载核对。', 'The service did not confirm the package change. Reload to verify it.'), JSON.stringify(result, null, 2));
  }, success, undefined, data => data.packages.some(row => row.id === item.id && (typeof body.enabled !== 'boolean' || row.enabled === body.enabled)
    && (typeof body.native_hooks !== 'boolean' || Boolean(row.native_hooks_enabled) === body.native_hooks)));
  const check = (item: ExtensionPackage) => void mutate(async () => {
    const result = await requestChatExtensions(`/check/${encodeURIComponent(item.id)}`, 'POST');
    if (!record(result) || !Number.isSafeInteger(result.tool_count) || Number(result.tool_count) < 0 || !strings(result.diagnostics)) throw new ChatExtensionsError('chat_extensions.check_invalid', 'chat_extensions.check_invalid', JSON.stringify(result, null, 2));
    if (result.diagnostics.length) throw new ChatExtensionsError('chat_extensions.check_failed', result.diagnostics.join('\n'), JSON.stringify(result, null, 2));
  }, t('MCP 检查已返回；目录已刷新。', 'MCP check returned; the catalog was refreshed.'));
  const errorView = error ? <Callout tone="danger" role="alert" title={t('操作未完成', 'Operation incomplete')}><p>{error.message}</p><details><summary>{t('完整诊断', 'Full diagnostics')}</summary><pre className="mt-2 whitespace-pre-wrap break-all text-[11.5px]">{error.code}{error.httpStatus != null ? ` · HTTP ${error.httpStatus}` : ''}{error.rawBody ? `\n${error.rawBody}` : ''}</pre></details></Callout> : null;
  const filtered = catalog?.packages.filter(item => `${item.name} ${item.description}`.toLocaleLowerCase().includes(query.toLocaleLowerCase())) || [];
  const locked = busy || picking || !scope;
  return <div className="cx-root mx-auto flex w-full max-w-4xl flex-col gap-4 px-3 py-4" data-testid="desktop-chat-extensions">
    <div className="flex flex-wrap items-center justify-between gap-2"><h2 className="text-[16px] font-medium">{t('聊天扩展', 'Chat extensions')}</h2><div className="flex gap-2"><Button variant="secondary" disabled={locked || loading} icon="refresh" onClick={refresh}>{t('重载目录', 'Reload catalog')}</Button><Button variant="primary" disabled={locked} icon="plus" onClick={() => { setError(null); setSource(''); setName(''); setRef(''); setInstallOwner(owner); setInstallOpen(true); }}>{t('添加扩展', 'Add extension')}</Button></div></div>
    <p className="text-[12.5px] text-cx-fg-3">{t('仅管理聊天 Agent 的扩展。包与本机能力来自当前服务宿主；桌面客户端的文件不会自动同步至服务。', 'Manage chat Agent extensions only. Packages and native capabilities belong to the connected service host; client files do not automatically synchronize with the service.')}</p>
    <NativeCapabilitiesPanel />
    {!scope ? <Callout tone="warning">{t('尚未确认服务身份，请先连接并登录。', 'Connect and sign in to confirm the service identity.')}</Callout> : null}
    <div className="flex flex-wrap gap-2"><div role="group" aria-label={t('扩展来源', 'Extension source')} className="flex gap-1">{(['managed', 'native'] as const).map(tab => <button type="button" key={tab} aria-pressed={sourceTab === tab} disabled={locked} className="rounded-md border border-cx-border px-3 py-1.5 text-[12.5px]" onClick={() => { setSourceTab(tab); setQuery(''); }}>{tab === 'managed' ? t('已安装扩展', 'Installed packages') : t('服务宿主能力', 'Service host capabilities')}</button>)}</div><label className="flex items-center gap-2 text-[12px]">{t('Agent', 'Agent')}<select aria-label={t('聊天 Agent', 'Chat Agent')} disabled={locked} value={engine} className="rounded-md border border-cx-border bg-cx-elevated px-2 py-1.5" onChange={event => setEngine(event.target.value)}>{Object.entries(ENGINES).map(([id, label]) => <option key={id} value={id}>{label}</option>)}</select></label><Input aria-label={t('搜索扩展', 'Search extensions')} value={query} onChange={event => setQuery(event.target.value)} placeholder={t('搜索名称和说明', 'Search name and description')} /></div>
    {!installOpen && !detail && !remove ? errorView : null}{notice ? <Callout role="status">{notice}</Callout> : null}{loading ? <p role="status">{t('读取目录中…', 'Loading catalog…')}</p> : null}
    {sourceTab === 'managed' && catalog ? <div className="flex flex-col divide-y divide-cx-border border-y border-cx-border"><div className="flex items-center justify-between gap-3 py-3"><div><strong className="text-[13px] font-medium">{t('Muteki 平台工具', 'Muteki platform tools')}</strong><p className="text-[12px] text-cx-fg-3">{t('聊天中访问授权的平台能力。', 'Access authorized platform capabilities from chat.')}</p></div><Switch label={t('启用平台工具', 'Enable platform tools')} checked={catalog.control_enabled} disabled={locked} onCheckedChange={enabled => void mutate(() => requestChatExtensions('/control', 'PUT', { enabled }).then(result => { if (!record(result) || result.enabled !== enabled) throw new ChatExtensionsError('chat_extensions.mutation_response_invalid', t('服务未确认平台工具变更。', 'The service did not confirm the platform tool change.'), JSON.stringify(result, null, 2)); return result; }), t('平台工具状态已更新。', 'Platform tool state updated.'), undefined, data => data.control_enabled === enabled)} /></div>{filtered.map(item => <div key={item.id} className="flex items-start gap-3 py-4"><Icon name={item.mcp_servers.length ? 'plug' : 'sparkles'} size={18} /><div className="min-w-0 flex-1"><button type="button" disabled={locked} className="break-all text-left text-[13px] font-medium hover:underline" onClick={() => { setError(null); setDetailId(item.id); }}>{item.name}</button><p className="mt-1 break-words text-[12px] text-cx-fg-3">{item.description}</p><p className="mt-1 text-[11.5px] text-cx-fg-4">{item.version} · {item.skills.length} Skills · {item.mcp_servers.length} MCP</p>{item.diagnostics.map((diagnostic, index) => <p key={index} className="mt-1 whitespace-pre-wrap break-all text-[12px] text-cx-danger">{diagnostic}</p>)}</div><Switch label={`${t('启用', 'Enable')} ${item.name}`} checked={item.enabled} disabled={locked} onCheckedChange={enabled => update(item, { enabled }, t('扩展启用状态已更新。', 'Package enabled state updated.'))} /></div>)}{!filtered.length ? <p className="py-5 text-[12.5px] text-cx-fg-3">{t('没有匹配的扩展。', 'No matching packages.')}</p> : null}</div> : null}
    {sourceTab === 'native' && catalog ? <div className="flex flex-col gap-3">{catalog.host_discovery_enabled === false ? <Callout tone="warning">{t('当前服务已禁用宿主能力发现。请使用已登记凭据和手动导入扩展；这不表示 Agent 未登录。', 'This service disables host discovery. Use registered credentials and manually imported packages; this does not mean the Agent is signed out.')}</Callout> : <p className="text-[12px] text-cx-fg-3">{t('只读查看服务宿主已发现的 Skill 和 MCP 配置；配置存在不等于已连接。', 'Read-only discovered skills and MCP configuration on the service host. Configuration presence does not prove a connection.')}</p>}{catalog.native_skills.filter(item => `${item.name} ${item.description}`.toLocaleLowerCase().includes(query.toLocaleLowerCase())).map(item => <div key={item.id} className="border-b border-cx-border py-2"><strong className="text-[13px] font-medium">{item.name}</strong><p className="whitespace-pre-wrap break-words text-[12px] text-cx-fg-3">{item.description}</p><p className="break-all text-[11.5px] text-cx-fg-4">{item.source}</p></div>)}{catalog.native_mcp.filter(item => item.name.toLocaleLowerCase().includes(query.toLocaleLowerCase())).map((item, index) => <div key={`${item.name}:${index}`} className="border-b border-cx-border py-2"><strong className="text-[13px] font-medium">{item.name}</strong><p className="break-all text-[12px] text-cx-fg-3">{item.enabled ? t('配置已启用', 'Configuration enabled') : t('配置已禁用', 'Configuration disabled')} · {item.transport} · {item.source}</p></div>)}</div> : null}
    <Dialog open={installOpen && installOwner === owner} onOpenChange={next => { if (!busy && !picking) setInstallOpen(next); }} title={t('添加聊天扩展', 'Add chat extension')} size="md" dismissable={!busy && !picking} footer={<><Button variant="ghost" disabled={busy || picking} onClick={() => setInstallOpen(false)}>{t('取消', 'Cancel')}</Button><Button variant="primary" disabled={busy || picking || (installType === 'package' && !source.trim())} loading={busy} onClick={install}>{t('导入', 'Import')}</Button></>}>
      <div className="flex flex-col gap-4"><label className="text-[12px]">{t('扩展类型', 'Extension type')}<select value={installType} disabled={busy || picking} className="mt-1 w-full rounded-md border border-cx-border bg-cx-elevated px-2 py-1.5" onChange={event => { setInstallType(event.target.value as 'package' | 'mcp'); setError(null); }}><option value="package">{t('插件包 / Skill', 'Package / Skill')}</option><option value="mcp">MCP</option></select></label>{installType === 'mcp' ? <><Input value={name} disabled={busy} onChange={event => setName(event.target.value)} aria-label={t('MCP 名称', 'MCP name')} /><TextArea label={t('完整 MCP JSON 配置', 'Full MCP JSON configuration')} value={config} onChange={event => setConfig(event.target.value)} disabled={busy} rows={8} /></> : <><label className="text-[12px]">{t('来源类型', 'Source type')}<select value={kind} disabled={busy || picking} className="mt-1 w-full rounded-md border border-cx-border bg-cx-elevated px-2 py-1.5" onChange={event => { setKind(event.target.value as SourceKind); setSource(''); setRef(''); setError(null); }}><option value="local-dir">{t('服务宿主目录', 'Service directory')}</option><option value="archive">{t('服务宿主归档文件', 'Service archive file')}</option><option value="git">{t('Git 仓库', 'Git repository')}</option></select></label>{kind === 'git' ? <><Input aria-label={t('Git 仓库地址', 'Git repository URL')} value={source} disabled={busy} onChange={event => setSource(event.target.value)} /><Input aria-label={t('Git 引用（可选）', 'Git ref (optional)')} value={ref} disabled={busy} onChange={event => setRef(event.target.value)} /></> : <NativePathPicker key={kind} id="desktop-chat-plugin-source" label={t('扩展来源路径', 'Extension source path')} kind={kind === 'archive' ? 'file' : 'directory'} value={source} onChange={setSource} disabled={busy} onBusyChange={setPicking} placeholder={t('当前服务可访问的路径', 'Path accessible to the connected service')} />}</>}{errorView}<p className="text-[12px] text-cx-fg-3">{t('导入文件属于服务宿主范围。原生选取的桌面路径须由你确认服务可以访问，再提交给服务校验。', 'Imports belong to the service host. Confirm that a client-selected path is accessible to the service before server validation.')}</p></div>
    </Dialog>
    <Dialog open={Boolean(detail)} onOpenChange={next => { if (!next && !busy) setDetailId(''); }} title={detail?.name || ''} size="md" dismissable={!busy} footer={<><Button variant="ghost" disabled={busy} onClick={() => setDetailId('')}>{t('关闭', 'Close')}</Button>{detail?.can_rollback ? <Button variant="secondary" disabled={busy} onClick={() => update(detail, { rollback: true }, t('版本已回滚。', 'Version rolled back.'))}>{t('回滚', 'Roll back')}</Button> : null}{detail?.mcp_servers.length ? <Button variant="secondary" disabled={busy} onClick={() => check(detail)}>{t('检查 MCP', 'Check MCP')}</Button> : null}<Button variant="danger" disabled={busy || !detail} onClick={() => { if (detail) { setRemoveRecord({ owner, item: detail }); setDetailId(''); } }}>{t('移除', 'Remove')}</Button></>}>
      {detail ? <div className="flex flex-col gap-3"><p className="whitespace-pre-wrap break-words text-[12.5px]">{detail.description}</p>{errorView}<p className="break-all text-[12px]">{t('版本与指纹', 'Version and digest')}: {detail.version} · <code>{detail.digest}</code></p>{Boolean(detail.hook_commands?.length) ? <><pre className="whitespace-pre-wrap break-all text-[12px]">{detail.hook_commands?.join('\n')}</pre><Switch checked={Boolean(detail.native_hooks_enabled)} disabled={busy} label={t('允许此版本的原生 hooks', 'Allow native hooks for this version')} onCheckedChange={native_hooks => update(detail, { native_hooks, digest: detail.digest }, t('此版本 hooks 状态已更新。', 'Hooks for this version updated.'))} /></> : null}<details><summary>{t('完整兼容性与组件信息', 'Full compatibility and component information')}</summary><pre className="mt-2 whitespace-pre-wrap break-all text-[11.5px]">{JSON.stringify({ skills: detail.skills, mcp: detail.mcp_servers, components: detail.native_components, compatibility: detail.compatibility, hooks: detail.hook_definitions, diagnostics: detail.diagnostics }, null, 2)}</pre></details></div> : null}
    </Dialog>
    <Dialog open={Boolean(remove)} onOpenChange={next => { if (!next && !busy) setRemoveRecord(null); }} title={t('移除聊天扩展', 'Remove chat extension')} size="sm" dismissable={!busy} footer={<><Button variant="ghost" disabled={busy} onClick={() => setRemoveRecord(null)}>{t('取消', 'Cancel')}</Button><Button variant="danger" disabled={busy || !remove} loading={busy} onClick={() => { if (remove) void mutate(async () => { const result = await requestChatExtensions(`/packages/${encodeURIComponent(remove.id)}`, 'DELETE'); if (!record(result) || result.removed !== true) throw new ChatExtensionsError('chat_extensions.mutation_response_invalid', t('服务未确认移除，请重载核对。', 'The service did not confirm removal. Reload to verify it.'), JSON.stringify(result, null, 2)); }, t('扩展已移除。', 'Package removed.'), () => setRemoveRecord(null), data => !data.packages.some(item => item.id === remove.id)); }}>{t('确认移除', 'Confirm removal')}</Button></>}><p className="break-words text-[13px]">{remove?.name}</p>{errorView}</Dialog>
  </div>;
}
