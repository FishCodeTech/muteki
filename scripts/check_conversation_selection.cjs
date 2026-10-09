// Run with: node scripts/check_conversation_selection.cjs
// Exercise the real Shell effects and stores with inert UI children and fixture APIs.
// No model calls, credentials, running server, or extra test dependencies are needed.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { createRequire } = require('node:module');
const ui = path.resolve(__dirname, '../apps/web/ui');
const req = createRequire(path.join(ui, 'package.json'));
const ts = req('typescript');
const React = req('react');
const { act, create } = req('react-test-renderer');
const noop = () => {};
const storage = new Map();
global.localStorage = { getItem: k => storage.get(k) ?? null, setItem: (k,v) => storage.set(k,v), removeItem: k => storage.delete(k) };
global.window = Object.assign(new EventTarget(), {
  localStorage, setTimeout, clearTimeout, innerWidth: 1440,
  requestAnimationFrame: cb => setTimeout(cb, 0), cancelAnimationFrame: clearTimeout,
  matchMedia: () => Object.assign(new EventTarget(), { matches: false }),
  location: { href: 'http://localhost/chat', pathname: '/chat', search: '' },
});
global.document = Object.assign(new EventTarget(), { visibilityState: 'visible', querySelector: () => null, documentElement: {} });
global.getComputedStyle = () => ({ getPropertyValue: () => '' });
global.IS_REACT_ACT_ENVIRONMENT = true;
// Node keeps the process alive while a BroadcastChannel is open; a page does not.
const NodeBroadcastChannel = global.BroadcastChannel;
global.BroadcastChannel = class extends NodeBroadcastChannel { constructor(name) { super(name); this.unref(); } };
let route = '/chat?draft=one';
const router = { push: value => { route = value; }, replace: noop };
const chrome = { sidebarCollapsed: false, mobileSidebarOpen: false, setMobileSidebarOpen: noop, setSidebarCollapsed: noop };
const stubs = new Map();
function component(name) {
  if (!stubs.has(name)) stubs.set(name, Object.assign(props => React.createElement(name, props, props.children), { displayName: name }));
  return stubs.get(name);
}
const components = new Proxy({}, { get: (_, key) => {
  if (key === '__esModule') return true;
  if (key === 'useConversationChrome') return () => chrome;
  return component(key);
}});
const models = [
  { id: 'grok-4.6', label: 'Grok 4.6' },
  { id: 'grok-4.7', label: 'Grok 4.7', reasoning: { supported: true, levels: ['low','high'], kind: 'effort' } },
];
const credentials = [
  { id:'system:codex', engine:'codex', label:'Codex', status:'ready', present:true, models:[{id:'codex-model'}], candidate_models:[], default_model:'codex-model', model_catalogs: {'cli.codex:default':[{id:'codex-model'}]} },
  { id:'system:grok', engine:'grok', label:'Grok', status:'ready', present:true, models:[models[0]], candidate_models:[models[1]], default_model:'grok-4.6', model_catalogs: {'cli.grok:default':models, 'cli.grok:other':models} },
];
const runtimes = [
  { key:'cli.codex:default', adapter_id:'cli.codex', instance_id:'default', engine:'codex', enabled:true, access_modes:['supervised'], health:{healthy:true}, auth:{} },
  { key:'cli.grok:default', adapter_id:'cli.grok', instance_id:'default', engine:'grok', enabled:true, access_modes:['supervised','full-access'], health:{healthy:true}, auth:{} },
  { key:'cli.grok:other', adapter_id:'cli.grok', instance_id:'other', engine:'grok', enabled:true, access_modes:['supervised','full-access'], health:{healthy:true}, auth:{} },
];
// Provider descriptors for the fixture engines, served at the real descriptor route.
const descriptorFor = (engine, displayName, efforts) => ({
  descriptor_version:1,
  identity:{ engine, display_name:displayName, support_status:'supported', default_adapter_id:`cli.${engine}`, cli_adapter_id:`cli.${engine}`, transport_label:'CLI' },
  adapters:[{ adapter_id:`cli.${engine}`, role:'cli', transport_kind:'cli', legacy_aliases:[], capabilities:{}, native_rewind:false, capability_gateway:true, access_mode_notes:{}, binary_resolution:'engine_bin', scoped_model_catalog_probe:false, accepts_adapter_endpoint:false, accepts_launch_args:false, transport_settings:[], notes:'' }],
  login:{ guidance_command:`${engine} login`, guidance_note:'', login_argv:[], status_probe:'', status_argv:[], host_login_import:'none', system_login_only:false },
  credentials:{ env_resolver:'', env_keys:[], secret_file:'' },
  models:{ method:'cli', argv:[], fallback_argv:[], fallback_source:'', parser:null, metadata_probe:'', provider_scoped:false, endpoint_protocol:'openai', endpoint_test_isolated_config:false, system_credential_live_catalog:false },
  cli:{ reasoning_efforts:efforts },
});
const descriptorCatalog = {
  schema_version:1,
  descriptors:[descriptorFor('codex','Codex',['low','medium','high']), descriptorFor('grok','Grok',['low','medium','high','xhigh'])],
  instances:[],
};
const descriptors = { schemaVersion:1, descriptors:descriptorCatalog.descriptors, instances:descriptorCatalog.instances };
const nodeFetch = global.fetch;
global.fetch = (url, init) => String(url).endsWith('/api/agent-runtimes/descriptors')
  ? Promise.resolve(new Response(JSON.stringify(descriptorCatalog), { status:200, headers:{'content-type':'application/json'} }))
  : nodeFetch(url, init);
const projects = [{ project_id:'project-a', name:'A', settings:{} }, { project_id:'project-b', name:'B', settings:{conv_default_credential_id:'system:codex',conv_default_model:'codex-model',conv_default_effort:'',conv_default_access_mode:'supervised'} }];
let view = null;
const conv = { events:[], view:null, refresh:async()=>{}, ensureMessageVisible:async()=> 'ok', loading:false, messages:[], turns:[] };
let credentialFetches = 0;
const api = {
  allCredentialModels: c => [...c.models,...c.candidate_models],
  fetchConversationCredentials: async()=>{ credentialFetches++; return structuredClone(credentials); },
  fetchRuntimeInstances: async()=>structuredClone(runtimes),
  fetchConversationProjects: async()=>structuredClone(projects),
  fetchConversationMemory: async()=>null,
  useConversation: () => ({...conv, view}),
  useConversationThreads: () => ({ threads:[], refresh:conv.refresh, attentionCount:0 }),
};
const cache = new Map();
function load(filename) {
  if (cache.has(filename)) return cache.get(filename).exports;
  const mod={exports:{}}; cache.set(filename,mod);
  const output=ts.transpileModule(fs.readFileSync(filename,'utf8'), {compilerOptions:{module:ts.ModuleKind.CommonJS,target:ts.ScriptTarget.ES2020,jsx:ts.JsxEmit.ReactJSX,esModuleInterop:true}}).outputText;
  function localRequire(name) {
    if (name === 'next/navigation') return {useRouter:()=>router,usePathname:()=>route.split('?')[0],useSearchParams:()=>new URLSearchParams(route.split('?')[1])};
    if (name.endsWith('/useConversation')) return api;
    const target=name.startsWith('@/') ? path.join(ui,name.slice(2)) : name.startsWith('.') ? path.resolve(path.dirname(filename),name) : null;
    if (!target) return req(name);
    if (target.includes('/components/') && !target.endsWith('/conversationEventViews') && !target.endsWith('/ConversationNavigation')) return components;
    const file = [target+'.ts',target+'.tsx',target+'/index.ts'].find(fs.existsSync);
    if (!file) throw new Error('Missing module '+target);
    return load(file);
  }
  new Function('require','module','exports',output)(localRequire,mod,mod.exports);
  return mod.exports;
}
// Local recovery state is only persisted once a verified service + identity owns it.
const storageScope=load(path.join(ui,'lib/conversationStorageScope.ts'));
storageScope.setConversationStorageScope('check-selection');
const defaults=load(path.join(ui,'lib/conversationDefaults.ts'));
const recent={credentialId:'system:grok',modelId:'grok-4.7',runtimeKey:'cli.grok:other',effort:'high',accessMode:'full-access'};
const { resolveNewConversationSelection: resolve } = load(path.join(ui,'lib/newConversationSelection.ts'));
const reasoning = load(path.join(ui,'lib/modelReasoning.ts'));
const configured = { credentialId:'system:codex', modelId:'codex-model' };
const resolveRecent = (patch = {}) => resolve({ credentials, runtimes, recent, configured:null, globalAccessMode:'', descriptors, ...patch });
defaults.writeChatLastSelection(recent);
assert.deepEqual(defaults.readChatLastSelection(), recent);
// Project > configured default > recent; a recent chat never hands its permission to a new one.
assert.deepEqual(resolveRecent(), { credentialId:'system:grok', model:'grok-4.7', runtimeKey:'cli.grok:other', effort:'high', accessMode:'supervised' });
assert.deepEqual(resolveRecent({ configured }), { credentialId:'system:codex', model:'codex-model', runtimeKey:'cli.codex:default', effort:'', accessMode:'supervised' });
// The global default permission applies only where the runtime advertises it.
assert.equal(resolveRecent({ globalAccessMode:'full-access' }).accessMode, 'full-access');
assert.equal(resolveRecent({ configured, globalAccessMode:'full-access' }).accessMode, 'supervised');
assert.equal(resolveRecent({ runtimes:runtimes.filter(r=>r.key!=='cli.grok:other') }).runtimeKey, 'cli.grok:default');
assert.equal(resolveRecent({ recent:{...recent,effort:'unsupported'} }).effort, '');
assert.equal(resolveRecent({ recent:{...recent,accessMode:'unsupported'} }).accessMode, 'supervised');
assert.equal(resolveRecent({ recent:{...recent,credentialId:'deleted'}, configured }).credentialId, 'system:codex');
assert.equal(resolveRecent({ recent:{...recent,modelId:'deleted'} }).model, 'grok-4.6');
assert.equal(resolveRecent({ project:defaults.readProjectConvDefaults({conv_default_credential_id:'system:grok',conv_default_model:'grok-4.7',conv_default_effort:''}) }).effort, '');
assert.equal(resolveRecent({ project:defaults.readProjectConvDefaults({}) }).effort, 'high');
assert.equal(resolveRecent({ credentials:[] }), null);
reasoning.rememberModelEffort('system:grok:cli.grok:other',models[1],'low');
assert.equal(resolveRecent({recent:{...recent,modelId:'grok-4.6'},project:{credentialId:'system:grok',modelId:'grok-4.7'}}).effort,'low');
// Storage corruption and disabled storage must not block rendering.
const preferenceKey = storageScope.conversationStorageKey('muteki.conversation.last-selection.v1');
for (const value of ['{broken','null','[]','42','{}','{"credentialId":5,"modelId":"model"}']) {
  storage.set(preferenceKey,value);
  assert.equal(defaults.readChatLastSelection(),null);
}
const savedStorage=window.localStorage;
window.localStorage={getItem:()=>{throw new Error('blocked');},setItem:()=>{throw new Error('blocked');}};
assert.equal(defaults.readChatLastSelection(),null);
assert.doesNotThrow(()=>defaults.writeChatLastSelection(recent));
window.localStorage=savedStorage;
defaults.writeChatLastSelection(recent);
console.log('Selection precedence, runtime/model compatibility, and storage regression passed');
const { normalizeConversationCredential } = load(path.join(ui,'lib/useConversation.ts'));
const credentialPayload = normalizeConversationCredential({
  id:'system:grok', engine:'grok', models:['grok-4.6'], candidate_models:['grok-4.7'],
  model_catalogs:{
    'cli.grok:default':{discovered_models:models,verified_models:['grok-4.6']},
    'cli.grok:other':{discovered_models:models,verified_models:['grok-4.7']},
  },
});
assert.deepEqual(reasoning.credentialForRuntime(credentialPayload,'cli.grok:other',false).models.map(m=>m.id),['grok-4.7']);
assert.deepEqual(reasoning.credentialForRuntime(credentialPayload,'cli.grok:default',false).models.map(m=>m.id),['grok-4.6']);
assert.deepEqual(reasoning.credentialForRuntime(credentialPayload,'cli.grok:missing',false).models,[]);
console.log('Runtime-scoped model verification projection passed');
const { ConversationShell }=load(path.join(ui,'components/conversation/ConversationShell.tsx'));
// The chat host (Next layout or desktop renderer) owns routing and passes it in.
const shell=(props={})=>React.createElement(ConversationShell,{
  navigation:{ router, pathname:route.split('?')[0], searchParams:new URLSearchParams(route.split('?')[1]||'') },
  ...props,
});
let tree;
const settle=()=>act(async()=>{await new Promise(r=>setTimeout(r,15));});
const home=()=>tree.root.findByType(component('ConversationHome')).props;
const selection=()=>{const p=home();return [p.selectedCredentialId,p.selectedModel,p.selectedEffort,p.selectedAccessMode];};
(async()=>{
  await act(async()=>{tree=create(shell());});
  await settle();
  // Completion can be followed by another SSE event in the same batch.
  const beforeRefresh = credentialFetches;
  credentials[1].verified_models_by_runtime = {'cli.grok:default':['grok-4.6'],'cli.grok:other':['grok-4.7']};
  conv.events = [
    {event_type:'core.turn.completed',event_id:'completed-test',seq:12,payload:{}},
    {event_type:'core.usage.updated',event_id:'usage-test',seq:13,payload:{}},
  ];
  await act(async()=>tree.update(shell()));
  assert.equal(credentialFetches,beforeRefresh+1);
  assert.ok(home().credentials.find(c=>c.id==='system:grok').models.some(m=>m.id==='grok-4.7'));
  await act(async()=>tree.update(shell()));
  assert.equal(credentialFetches,beforeRefresh+1);
  // An older in-flight catalog response must not undo the newer verification.
  const normalCredentialFetch = api.fetchConversationCredentials;
  let releaseOldCredentials;
  api.fetchConversationCredentials=()=>new Promise(resolve=>{releaseOldCredentials=resolve;});
  await act(async()=>home().onRetryCredentials());
  api.fetchConversationCredentials=normalCredentialFetch;
  conv.events=[{event_type:'core.turn.completed',event_id:'newer-completion',seq:14,payload:{}}];
  await act(async()=>tree.update(shell()));
  const staleCredentials=structuredClone(credentials);
  staleCredentials[1].verified_models_by_runtime['cli.grok:other']=[];
  await act(async()=>releaseOldCredentials(staleCredentials));
  assert.ok(home().credentials.find(c=>c.id==='system:grok').models.some(m=>m.id==='grok-4.7'));
  console.log('Completion refresh and stale response ordering passed');
  conv.events = [];
  assert.equal(home().capabilityContext.adapterId, 'cli.grok');
  // Recent model/effort carry over; the recent chat's full access does not.
  assert.deepEqual(selection(),['system:grok','grok-4.7','high','supervised']);
  await act(async()=>{home().onSelectModelParams({credentialId:'system:grok',model:'grok-4.7',effort:'low',accessMode:'full-access'});});
  route='/chat?draft=two';
  await act(async()=>tree.update(shell()));
  await settle();
  // A new draft starts supervised even though the previous draft chose full access.
  assert.deepEqual(selection(),['system:grok','grok-4.7','low','supervised']);
  await act(async()=>home().onProjectChange('project-a'));
  assert.deepEqual(selection(),['system:grok','grok-4.7','low','supervised']);
  await act(async()=>home().onProjectChange('project-b'));
  assert.deepEqual(selection(),['system:codex','codex-model','','supervised']);
  await act(async()=>home().onSelectModelParams({credentialId:'system:grok',model:'grok-4.7',effort:'high',accessMode:'full-access'}));
  await act(async()=>home().onRetryReadiness());
  assert.deepEqual(selection(),['system:grok','grok-4.7','high','full-access']);
  route='/chat?draft=one';
  await act(async()=>tree.update(shell()));
  await settle();
  assert.deepEqual(selection(),['system:grok','grok-4.7','low','full-access']);
  await act(async()=>tree.unmount());
  await act(async()=>{tree=create(shell());});
  await settle();
  assert.deepEqual(selection(),['system:grok','grok-4.7','low','full-access']);
  // Restoring an existing thread must keep its own settings; New Chat inherits its
  // endpoint and model, but starts supervised rather than taking its full access.
  route='/chat/old';
  view={thread:{thread_id:'old',project_id:'',mode:'conversation'},state:{status:'idle'},turns:[],queue:[],artifacts:[],runtime:{adapter_id:'cli.grok',instance_id:'other',credential_id:'system:grok',model:'grok-4.6',effort:'',access_mode:'full-access'}};
  await act(async()=>tree.update(shell({threadId:'old'})));
  await settle();
  await act(async()=>tree.root.findByType(component('ConversationSidebar')).props.onNewChat());
  view=null;
  await act(async()=>tree.update(shell()));
  await settle();
  assert.deepEqual(selection(),['system:grok','grok-4.6','','supervised']);
  await act(async()=>tree.root.findByType(component('ConversationSidebar')).props.onNewChatForProject('project-b'));
  await act(async()=>tree.update(shell()));
  await settle();
  assert.deepEqual(selection(),['system:codex','codex-model','','supervised']);
  await act(async()=>tree.unmount());
  console.log('Shell route/project/refresh/draft/remount/thread regression passed');
  // Catalogs arrive separately; a late project response must not lose its defaults.
  defaults.writeChatLastSelection(recent);
  let releaseCredentials, releaseRuntimes, releaseProjects;
  api.fetchConversationCredentials=()=>new Promise(resolve=>{releaseCredentials=resolve;});
  api.fetchRuntimeInstances=()=>new Promise(resolve=>{releaseRuntimes=resolve;});
  api.fetchConversationProjects=()=>new Promise(resolve=>{releaseProjects=resolve;});
  route='/chat?draft=delayed';
  await act(async()=>{tree=create(shell());});
  await settle();
  await act(async()=>tree.root.findByType(component('ConversationSidebar')).props.onNewChatForProject('project-b'));
  await act(async()=>tree.update(shell()));
  await settle();
  await act(async()=>releaseCredentials(structuredClone(credentials)));
  await act(async()=>releaseRuntimes(structuredClone(runtimes)));
  await act(async()=>releaseProjects(structuredClone(projects)));
  assert.deepEqual(selection(),['system:codex','codex-model','','supervised']);
  await act(async()=>tree.unmount());
  console.log('Delayed catalog and project defaults regression passed');
})().catch(error=>{console.error(error);process.exitCode=1;});

// Shared UI preference migration and concurrency use independent simulated clients.
{
const assert=require('node:assert/strict'), fs=require('node:fs'),vm=require('node:vm');
const root=require('node:path').resolve(__dirname, '..');
const ts=require(root+'/apps/web/ui/node_modules/typescript');
const code=ts.transpileModule(fs.readFileSync(root+'/apps/web/ui/lib/uiPreferences.ts','utf8'),{compilerOptions:{module:ts.ModuleKind.CommonJS,target:ts.ScriptTarget.ES2022}}).outputText;
function server(){return {docs:new Map(), conflict:null, fail:false, count:0, get(scope){return this.docs.get(scope)||{version:0,values:{}}}, async fetch(scope,url,init={}){
 if(url==='/api/health') return Response.json({features:{'ui.preferences':1}});
 assert.equal(url,'/api/settings/ui');
 if(init.method!=='PUT') return Response.json(this.get(scope));
 this.count++; if(this.fail)return Response.json({error:{code:'fixture.failed',message:'synthetic write failure'}},{status:503});
 if(this.conflict){this.conflict(scope);this.conflict=null;}
 const body=JSON.parse(init.body), current=this.get(scope);
 if(body.version!==current.version)return Response.json(current,{status:409});
 const next={version:body.version+1,values:body.values};this.docs.set(scope,next);return Response.json(next);
}};}
function client(s,seed={},remote=false){let scope='service-a:operator';const observers=new Set(),storage=new Map(Object.entries(seed)), events=new EventTarget();
 const localStorage={getItem:k=>storage.get(k)??null,setItem:(k,v)=>storage.set(k,v)};
 const exports={};const ctx=vm.createContext({exports,require:n=>n==='./serviceAuth'?{apiFetch:(...args)=>s.fetch(scope,...args)}:n==='./desktopEnvironment'?{isDesktopRemote:()=>remote}:{conversationStorageScope:()=>scope,conversationStorageKey:(base,owner=scope)=>base+':scope:'+encodeURIComponent(owner),subscribeConversationStorageScope:fn=>{observers.add(fn);return()=>observers.delete(fn)}},AbortController,localStorage,window:events,console});
 vm.runInContext(code,ctx);const off=exports.startUiPreferencesSync();
 return {api:exports,storage,off,setScope(next){scope=next;for(const fn of observers)fn()}};
}
(async()=>{
 const s=server(), c=client(s,{'muteki.chatPrefs.v1':JSON.stringify({diffWrap:false,diffCollapseUnchanged:false}),'muteki.themePreference':'system'});
 await c.api.refreshUiPreferences();
 assert.deepEqual(s.get('service-a:operator').values,{theme:'system',diffWrap:false,diffCollapseUnchanged:false});
 assert.equal(c.storage.get('muteki.ui.migrated.v1:scope:service-a%3Aoperator'),'1');
 s.conflict=scope=>{const v=s.get(scope);s.docs.set(scope,{version:v.version+1,values:{...v.values,language:'en'}})};
 c.api.writeUiPreferences({sendKey:'mod-enter'});await c.api.saveUiPreferences();
 assert.equal(s.get('service-a:operator').values.language,'en');assert.equal(s.get('service-a:operator').values.sendKey,'mod-enter');
 s.fail=true;c.api.writeUiPreferences({diffWrap:true});await c.api.saveUiPreferences();assert.equal(c.api.uiPreferenceSnapshot().status,'error');assert.match(c.api.uiPreferenceSnapshot().error,/503/);
 s.fail=false;await c.api.saveUiPreferences();assert.equal(s.get('service-a:operator').values.diffWrap,true);assert.equal(c.api.uiPreferenceSnapshot().status,'ready');
 c.setScope('service-b:operator');await c.api.refreshUiPreferences();assert.equal(c.api.readUiPreference('sendKey','enter'),'enter');assert.equal(s.get('service-a:operator').values.sendKey,'mod-enter');c.off();
 const r=server(), remote=client(r,{'muteki.theme':'dark','muteki.chatPrefs.v1':JSON.stringify({sendKey:'mod-enter'})},true);await remote.api.refreshUiPreferences();assert.deepEqual(r.get('service-a:operator').values,{});assert.equal(r.count,0);remote.off();
 const m=server();m.conflict=scope=>m.docs.set(scope,{version:1,values:{diffWrap:true}});const migration=client(m,{'muteki.chatPrefs.v1':JSON.stringify({diffWrap:false})});await migration.api.refreshUiPreferences();assert.equal(m.get('service-a:operator').values.diffWrap,true);migration.off();
 console.log('PASS: actual UI store migration preserves false/system; 409 retains unrelated edits; failed writes retry; service scopes isolate; remote skips all migration; concurrent migration never overwrites newly-set fields');
})().catch(e=>{console.error(e);process.exitCode=1});

}
