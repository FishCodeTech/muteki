const { contextBridge, ipcRenderer } = require('electron');
async function invoke(channel, ...args) {
  const result = await ipcRenderer.invoke(channel, ...args);
  if (!result.ok) { const error = new Error(result.error?.message || '桌面操作失败。'); Object.assign(error, result.error); throw error; }
  return result.value;
}
function subscribe(channel, callback) {
  if (typeof callback !== 'function') throw new TypeError('callback must be a function');
  const listener = (_event, value) => callback(value);
  ipcRenderer.on(channel, listener);
  return () => ipcRenderer.removeListener(channel, listener);
}
contextBridge.exposeInMainWorld('mutekiDesktop', {
  getState: () => invoke('desktop:state'), connect: origin => invoke('desktop:connect', origin),
  updates: { getStatus: () => invoke('desktop:update-status'), check: () => invoke('desktop:update-check'), install: () => invoke('desktop:update-install'), onStatus: callback => subscribe('desktop:update-status', callback) },
  connectLocal: () => invoke('desktop:connect-local'),
  configure: () => invoke('desktop:configure'), resume: () => invoke('desktop:resume'),
  consumeAnchor: id => invoke('desktop:anchor-consumed', id),
  navigate: (href, mode = 'push') => invoke('desktop:navigate', href, mode), action: name => invoke('desktop:action', name),
  setLocale: lang => invoke('desktop:locale', lang), syncAppearance: input => invoke('desktop:appearance', input), windowAction: name => invoke('desktop:window', name),
  newWindow: route => invoke('desktop:new-window', route), openExternal: url => invoke('desktop:external', url),
  openPreview: input => invoke('desktop:preview', input), closePreview: input => invoke('desktop:preview-close', input),
  previewAction: input => invoke('desktop:preview-action', input), browserControl: input => invoke('desktop:browser-control', input),
  createVisualization: input => invoke('desktop:visualization-create', input), releaseVisualization: id => invoke('desktop:visualization-release', id),
  cacheAttachment: input => invoke('desktop:attachment-cache', input), restoreAttachment: input => invoke('desktop:attachment-restore', input),
  removeAttachment: input => invoke('desktop:attachment-remove', input),
  attachmentCacheUsage: () => invoke('desktop:attachment-cache-usage'),
  selectPath: input => invoke('desktop:select-path', input), openPath: input => invoke('desktop:open-path', input),
  selectWorkspaceRoot: input => invoke('desktop:workspace-root', input), openWorkspaceFile: input => invoke('desktop:workspace-file', input),
  openWorkspaceInEditor: input => invoke('desktop:workspace-editor', input),
  setMenuAccelerators: input => invoke('desktop:menu-accelerators', input),
  requestMicrophone: () => invoke('desktop:microphone'), openPermissionSettings: name => invoke('desktop:permission-settings', name),
  startSpeech: input => invoke('desktop:speech-start', input), finishSpeech: input => invoke('desktop:speech-finish', input), cancelSpeech: input => invoke('desktop:speech-cancel', input),
  onSpeech: callback => subscribe('desktop:speech', callback),
  notificationStatus: input => invoke('desktop:notification-status', input), requestNotifications: input => invoke('desktop:notification-request', input),
  sendNotification: input => invoke('desktop:notification-send', input), playNotificationSound: input => invoke('desktop:notification-sound', input), onNotification: callback => subscribe('desktop:notification-state', callback),
  acknowledgeClose: input => invoke('desktop:close-ack', input),
  terminalOpen: input => invoke('desktop:terminal-open', input), terminalSend: (id, data) => invoke('desktop:terminal-send', id, data), terminalClose: id => invoke('desktop:terminal-close', id),
  onState: callback => subscribe('desktop:state-changed', callback), onCommand: callback => subscribe('desktop:command', callback),
  onFocus: callback => subscribe('desktop:focus', callback),
  onBeforeClose: callback => subscribe('desktop:before-close', callback), onTerminal: callback => subscribe('desktop:terminal', callback),
  onPreview: callback => subscribe('desktop:preview-state', callback),
});
