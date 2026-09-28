const { contextBridge, ipcRenderer } = require('electron');
async function invoke(channel, ...args) {
  const result = await ipcRenderer.invoke(channel, ...args);
  if (!result.ok) throw new Error(result.error);
  return result.value;
}
contextBridge.exposeInMainWorld('mutekiDesktop', {
  getState: () => invoke('desktop:state'),
  connect: origin => invoke('desktop:connect', origin),
  configure: () => invoke('desktop:configure'),
  resume: () => invoke('desktop:resume'),
  navigate: href => invoke('desktop:navigate', href),
  action: name => invoke('desktop:action', name),
  windowAction: name => invoke('desktop:window', name),
  onState: callback => { const listener = (_event, value) => callback(value); ipcRenderer.on('desktop:state-changed', listener); return () => ipcRenderer.removeListener('desktop:state-changed', listener); },
});
