/** Host ABI used by installed Visualize skills. No access to the parent DOM. */
export const VISUALIZATION_BRIDGE = String.raw`
(() => {
  const documentNonce = __MUTEKI_DOCUMENT_NONCE__;
  const standalone = __MUTEKI_STANDALONE__;
  let serial = 0;
  const pending = new Map();
  const tweaks = new Map();
  const notify = (type, value) => parent.postMessage({type:'muteki:viz:'+type, documentNonce, ...value}, '*');
  const request = (type, value) => new Promise((resolve, reject) => {
    if(standalone) {
      if(type==='state-write') {try {localStorage.setItem('muteki:visualization:'+documentNonce,JSON.stringify(value.value));resolve();}catch(error){reject(error);}return;}
      reject(new Error('请回到 Muteki 聊天继续分析'));return;
    }
    const id = ++serial;
    const timer = setTimeout(() => {pending.delete(id); reject(new Error('操作未完成'));}, 120000);
    pending.set(id, {resolve, reject, timer}); notify(type, {...value, id});
  });
  const changed = globals => {
    Object.assign(api, globals);
    dispatchEvent(new CustomEvent('muteki:visualization-context',{detail:{globals}}));
    dispatchEvent(new CustomEvent('openai:set_globals',{detail:{globals}}));
  };
  const api = {
    widgetState:null, theme:getComputedStyle(document.documentElement).colorScheme === 'light' ? 'light' : 'dark',
    statePersistence:'local', stateModelContext:'follow-up',
    visualizationTheme:getComputedStyle(document.documentElement).colorScheme === 'light' ? 'light' : 'dark', visualizationStyleVariables:{},
    setWidgetState:async value => {
      const next = typeof value === 'function' ? value(api.widgetState) : value;
      if (!next || typeof next !== 'object' || Array.isArray(next)) throw new Error('状态必须是 JSON 对象');
      const state = JSON.parse(JSON.stringify({modelContent:null,privateContent:null,...next}));
      if(new TextEncoder().encode(JSON.stringify(state)).length>16384) throw new Error('状态超过 16 KiB');
      changed({widgetState:state}); return request('state-write',{value:state});
    },
    requestFollowUp: value => {
      if(!navigator.userActivation.isActive) return Promise.reject(new Error('请通过点击发起后续消息'));
      return request('followup',{prompt:String(value.prompt||''),title:String(value.title||''),modelContent:api.widgetState?.modelContent??null});
    },
    openExternal: value => {
      if(navigator.userActivation.isActive) {
        if(standalone) {const url=new URL(String(value.href||''));if(['http:','https:'].includes(url.protocol)&&!url.username&&!url.password)window.open(url.href,'_blank','noopener,noreferrer');}
        else notify('external',{href:String(value.href||'')});
      }
    }
  };
  window.muteki = {...window.muteki, visualize:api};
  window.openai = api;
  api.sendFollowUpMessage = api.requestFollowUp;
  addEventListener('message', event => {
    if(event.source!==parent) return;
    const data=event.data;
    if(data?.documentNonce!==documentNonce) return;
    if(data?.type==='muteki:viz:theme' && ['light','dark'].includes(data.value)) {
      const style=document.getElementById('muteki-visualization-theme');
      const variables=data.variables||{};
      if(style)style.textContent=':root{color-scheme:'+data.value+';'+Object.entries(variables).filter(([key])=>/^--[a-z0-9-]+$/.test(key)).map(([key,value])=>key+':'+String(value).replace(/[;{}<>]/g,'')+';').join('')+'}';
      changed({theme:data.value,visualizationTheme:data.value,visualizationStyleVariables:variables});
    }
    if(data?.type==='muteki:viz:state') changed({widgetState:data.value});
    if(data?.type==='muteki:viz:result') {
      const value=pending.get(data.id); if(!value) return;
      pending.delete(data.id); clearTimeout(value.timer);
      data.ok ? value.resolve() : value.reject(new Error(data.error||'操作未完成'));
    }
    if(data?.type==='muteki:viz:tweak-set') {
      const item=tweaks.get(data.id); if(!item) return;
      const type=item.type;
      let value=data.reset ? item.initial : data.value;
      if(type==='slider') {value=Number(value); if(!Number.isFinite(value)) return; value=Math.max(item.options.min??0,Math.min(item.options.max??100,value));}
      if(type==='toggle' && typeof value!=='boolean') return;
      if(type==='color' && !/^#[0-9a-f]{6}$/i.test(value)) return;
      if(type==='select' && !item.options.options.some(o=>(typeof o==='string'?o:o.value)===value)) return;
      item.object[item.property]=value; item.group.onChange?.();
    }
  });
  window.Tweak=class {
    supported=true;
    constructor(options={}) {this.container=options.container;this.onChange=options.onChange;this.ids=[];}
    add(type,object,property,options={}) {
      if(this.ids.length>=12) return this;
      const id=++serial;this.ids.push(id);
      const safeOptions={label:String(options.label||property),min:options.min,max:options.max,step:options.step,unit:options.unit,
        options:(options.options||[])};
      tweaks.set(id,{group:this,type,object,property,options:safeOptions,initial:object[property]});
      notify('tweak-add',{control:{id,type,group:this.container?.getAttribute('aria-label')||'设计控件',value:object[property],...safeOptions}});
      return this;
    }
    addSlider(o,p,c) {return this.add('slider',o,p,c);}
    addColorPicker(o,p,c) {return this.add('color',o,p,c);}
    addToggle(o,p,c) {return this.add('toggle',o,p,c);}
    addSelect(o,p,c) {return this.add('select',o,p,c);}
    dispose() {for(const id of this.ids) tweaks.delete(id);notify('tweak-remove',{ids:this.ids});this.ids=[];}
  };
  addEventListener('DOMContentLoaded',()=>{
    if(standalone){try {changed({widgetState:JSON.parse(localStorage.getItem('muteki:visualization:'+documentNonce)||'null')});}catch{/* Local files can deny storage. */}}
    else notify('ready',{});
    let scheduled=0,lastHeight=0;
    const measure=()=>{
      cancelAnimationFrame(scheduled);scheduled=requestAnimationFrame(()=>{
        const body=document.body;if(!body)return;
        const range=document.createRange();range.selectNodeContents(body);
        const box=range.getBoundingClientRect();
        const height=Math.ceil(Math.max(box.bottom,body.getBoundingClientRect().bottom)+scrollY);
        if(height!==lastHeight){lastHeight=height;notify('resize',{height});}
      });
    };
    new ResizeObserver(measure).observe(document.body);
    new MutationObserver(measure).observe(document.body,{subtree:true,childList:true,attributes:true,characterData:true});
    addEventListener('resize',measure);addEventListener('load',measure,true);document.fonts?.ready.then(measure);
    measure();
    window.lucide?.createIcons();
  });
  document.addEventListener('click',event=>{
    const link=event.isTrusted?event.composedPath().find(node=>node?.matches?.('a[href]')):null;
    if(!link||link.getAttribute('href').startsWith('#'))return;
    event.preventDefault();window.openai.openExternal({href:link.href});
  },true);
})();`;

export function visualizationBridge(documentNonce: string, standalone = false): string {
  return VISUALIZATION_BRIDGE.replace("__MUTEKI_DOCUMENT_NONCE__", JSON.stringify(documentNonce))
    .replace("__MUTEKI_STANDALONE__", String(standalone));
}
