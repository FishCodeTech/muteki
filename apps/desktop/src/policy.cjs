const { createHash } = require('node:crypto');

const DEFAULT_ORIGIN = 'http://127.0.0.1:3001';
const SHELL_URL = 'muteki-desktop://app/index.html';

function parseWebUrl(value) {
  try {
    const url = new URL(value);
    if (!['http:', 'https:'].includes(url.protocol) || url.username || url.password) return null;
    return url;
  } catch {
    return null;
  }
}

function normalizeOrigin(value) {
  if (typeof value !== 'string' || value.length > 2048) throw new Error('请输入有效的服务地址。');
  const url = parseWebUrl(value.trim());
  if (!url) throw new Error('地址需要以 http:// 或 https:// 开头，且不能包含账号或密码。');
  if (url.pathname !== '/' || url.search || url.hash) {
    throw new Error('请填写工作台的根地址，不要附带路径、参数或登录令牌。');
  }
  const loopback = ['127.0.0.1', 'localhost', '[::1]'].includes(url.hostname);
  if (url.protocol === 'http:' && !loopback) {
    throw new Error('远程服务请使用 HTTPS；HTTP 仅用于本机 localhost、127.0.0.1 或 [::1]。');
  }
  return url.origin;
}

function sameOrigin(value, origin) {
  const url = parseWebUrl(value);
  return Boolean(url && url.origin === origin);
}

function allowedNavigation(value, origin, { preview = false } = {}) {
  if (sameOrigin(value, origin)) return true;
  if (!preview) return false;
  if (value === 'about:blank') return true;
  try {
    const url = new URL(value);
    return url.protocol === 'blob:' && url.origin === origin;
  } catch {
    return false;
  }
}

function partitionFor(origin) {
  return `persist:muteki-${createHash('sha256').update(normalizeOrigin(origin)).digest('hex')}`;
}

function webPreferences(partition) {
  return {
    partition,
    nodeIntegration: false,
    nodeIntegrationInWorker: false,
    nodeIntegrationInSubFrames: false,
    contextIsolation: true,
    sandbox: true,
    webSecurity: true,
    allowRunningInsecureContent: false,
    webviewTag: false,
    navigateOnDragDrop: false,
  };
}

function normalizeDesktopRoute(value) {
  const invalid = () => Object.assign(new Error('此导航地址不可用。'), { code: 'desktop.route_invalid', source: 'desktop', retryable: false });
  if (typeof value !== 'string') throw invalid();
  let url;
  try { url = new URL(value, SHELL_URL); } catch { throw invalid(); }
  if (url.protocol !== 'muteki-desktop:' || url.host !== 'app' || url.username || url.password || url.hash
      || !/^\/(chat(?:\/[^/]+)?|settings(?:\/[a-z-]+)?|ctf(?:\/.*)?|pentest(?:\/.*)?|competitions(?:\/.*)?|task(?:\/.*)?|usage)?$/.test(url.pathname)) throw invalid();
  if (url.pathname.startsWith('/chat/')) {
    try { decodeURIComponent(url.pathname.split('/')[2]); } catch { throw invalid(); }
  }
  return url.pathname + url.search;
}

module.exports = { DEFAULT_ORIGIN, SHELL_URL, parseWebUrl, normalizeOrigin, normalizeDesktopRoute, sameOrigin, allowedNavigation, partitionFor, webPreferences };
