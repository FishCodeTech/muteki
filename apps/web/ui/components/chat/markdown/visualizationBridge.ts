/** Host ABI used by installed Visualize skills. No access to the parent DOM. */
export const VISUALIZATION_BRIDGE = String.raw`
(() => {
  let serial = 0;
  const pending = new Map();
  const tweaks = new Map();
  const notify = (type, value) => parent.postMessage({type:'muteki:viz:'+type, ...value}, '*');
  const request = (type, value) => new Promise((resolve, reject) => {
    const id = ++serial;
    const timer = setTimeout(() => {pending.delete(id); reject(new Error('操作未完成'));}, 120000);
    pending.set(id, {resolve, reject, timer}); notify(type, {...value, id});
  });
  const changed = globals => {Object.assign(window.openai, globals); dispatchEvent(new CustomEvent('openai:set_globals',{detail:{globals}}));};
  window.openai = {
    widgetState:null, theme:document.documentElement.style.colorScheme || 'dark',
    statePersistence:'local', stateModelContext:'follow-up',
    visualizationTheme:document.documentElement.style.colorScheme || 'dark', visualizationStyleVariables:{},
    setWidgetState:async value => {
      const next = typeof value === 'function' ? value(window.openai.widgetState) : value;
      if (!next || typeof next !== 'object' || Array.isArray(next)) throw new Error('状态必须是 JSON 对象');
      const state = JSON.parse(JSON.stringify({modelContent:null,privateContent:null,...next}));
      if(new TextEncoder().encode(JSON.stringify(state)).length>16384) throw new Error('状态超过 16 KiB');
      changed({widgetState:state}); return request('state-write',{value:state});
    },
    sendFollowUpMessage: value => {
      if(!navigator.userActivation.isActive) return Promise.reject(new Error('请通过点击发起后续消息'));
      return request('followup',{prompt:String(value.prompt||''),title:String(value.title||''),modelContent:window.openai.widgetState?.modelContent??null});
    },
    openExternal: value => {
      if(navigator.userActivation.isActive) notify('external',{href:String(value.href||'')});
    }
  };
  addEventListener('message', event => {
    if(event.source!==parent) return;
    const data=event.data;
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
        options:(options.options||[]).slice(0,12)};
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
    notify('ready',{});
    new ResizeObserver(()=>notify('resize',{height:document.body.scrollHeight+24})).observe(document.body);
    window.lucide?.createIcons();
  });
  document.addEventListener('click',event=>{
    const link=event.target.closest?.('a[href]');if(!link||link.getAttribute('href').startsWith('#'))return;
    event.preventDefault();window.openai.openExternal({href:link.href});
  });
})();`;
