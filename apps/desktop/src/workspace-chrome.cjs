// Desktop-only presentation changes. The actual business pages, their state,
// routing, API calls, composers, and dialogs are still served by the Web app.
const WORKSPACE_CSS = `
.workspace-frame { --workspace-nav-height: 0px !important; --workspace-rail-width: 0px !important; grid-template-columns: minmax(0, 1fr) !important; }
.workspace-activity-rail, .workspace-nav, .workspace-curtain-zone { display: none !important; }
.workspace-frame:not([data-conversation-layout="true"]) .workspace-shared-brand-dock { display: none !important; }
.workspace-frame[data-conversation-layout="true"] .workspace-shared-brand-dock {
  height: var(--workspace-collapsed-brand-height, 52px) !important;
  background: var(--rail); border-bottom: 1px solid var(--line);
  opacity: 1 !important; visibility: visible !important;
}
.workspace-frame[data-conversation-layout="true"][data-conversation-sidebar-collapsed="true"] .workspace-shared-brand-dock { display: none !important; }
.workspace-frame[data-conversation-layout="true"] .cx-sidebar-nav::before {
  content: ""; display: block; flex: none;
  height: var(--workspace-collapsed-brand-height, 52px) !important;
}
`;

// Read only navigation metadata and appearance. Never read authentication
// storage or conversation contents into the desktop shell.
const READ_CHROME = `(() => {
  const frame = document.querySelector('.workspace-frame');
  const links = Array.from(document.querySelectorAll('#workspace-navigation > a'));
  const items = links.map(link => ({
    href: new URL(link.href).pathname,
    label: (link.querySelector('strong')?.textContent || link.textContent || '').trim().slice(0, 60),
    badge: (link.querySelector('[data-workspace-badge], .workspace-nav-badge')?.textContent || '').trim().slice(0, 5),
  }));
  const themeButton = document.querySelector('[data-workspace-action="theme"], .workspace-nav-theme-toggle');
  const sidebarButton = document.querySelector('[data-workspace-action="sidebar"], .workspace-brand-sidebar-toggle');
  let solveOnly = frame?.dataset.solveOnly === 'true';
  if (!frame) { try { solveOnly = localStorage.getItem('muteki.workspace.solveOnly') !== '0'; } catch {} }
  return {
    items, solveOnly, hasFrame: !!frame, pathname: location.pathname,
    theme: document.documentElement.dataset.theme === 'light' ? 'light' : 'dark',
    settingsHref: document.querySelector('[data-workspace-action="settings"], .workspace-nav-settings-gear')?.getAttribute('href') || (solveOnly ? '/settings/appearance' : '/settings/agents'),
    usage: !!document.querySelector('[data-workspace-action="usage"], .workspace-nav-usage'),
    search: !!document.querySelector('[data-workspace-action="search"], .workspace-nav-command'),
    themeToggle: !!themeButton,
    sidebar: !!sidebarButton,
    sidebarCollapsed: frame?.dataset.conversationSidebarCollapsed === 'true',
  };
})()`;

const ACTION_SCRIPTS = Object.freeze({
  search: `(() => { const node = document.querySelector('[data-workspace-action="search"], .workspace-nav-command'); if (!node) return false; node.click(); return true; })()`,
  theme: `(() => { const node = document.querySelector('[data-workspace-action="theme"], .workspace-nav-theme-toggle'); if (!node) return false; node.click(); return true; })()`,
  sidebar: `(() => { const node = document.querySelector('[data-workspace-action="sidebar"], .workspace-brand-sidebar-toggle'); if (!node) return false; node.click(); return true; })()`,
});

function navigationScript(href) {
  return `(() => {
    const target = new URL(${JSON.stringify(href)}, location.origin).href;
    const link = Array.from(document.querySelectorAll('a[href]')).find(node => node.href === target);
    if (!link) return false;
    link.click(); return true;
  })()`;
}

function normalizeRoute(value, origin) {
  if (typeof value !== 'string' || value.length > 2048 || !value.startsWith('/') || value.startsWith('//') || /[\\\u0000-\u0020]/.test(value)) throw new Error('无效的工作区地址。');
  const url = new URL(value, origin);
  if (url.origin !== origin || url.username || url.password || url.pathname.startsWith('/api/') || url.pathname.startsWith('/_next/')) throw new Error('不允许打开此地址。');
  return `${url.pathname}${url.search}${url.hash}`;
}

function iconForRoute(href) {
  if (href.startsWith('/chat')) return 'chat';
  if (href.startsWith('/pentest')) return 'pentest';
  if (href.startsWith('/ctf') || href.startsWith('/task') || href.startsWith('/solve') || href.startsWith('/run/')) return 'task';
  if (href.startsWith('/competition')) return 'competition';
  return 'extension';
}

function activeRoute(pathname, items) {
  if (pathname === '/') return '/';
  if (pathname.startsWith('/settings')) return 'settings';
  if (pathname === '/usage') return '/usage';
  if (pathname.startsWith('/run/') || pathname.startsWith('/solve')) return items.find(item => item.href === '/ctf')?.href || items.find(item => item.href === '/task')?.href || '/ctf';
  return [...items].sort((a, b) => b.href.length - a.href.length).find(item => pathname === item.href || pathname.startsWith(`${item.href}/`))?.href || '';
}
module.exports = { WORKSPACE_CSS, READ_CHROME, ACTION_SCRIPTS, navigationScript, normalizeRoute, iconForRoute, activeRoute };
