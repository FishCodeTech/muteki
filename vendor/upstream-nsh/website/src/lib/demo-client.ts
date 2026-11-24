/** The page's theme, as the player takes it (`?theme=`). */
export const siteTheme = () => (document.documentElement.classList.contains('light') ? 'light' : 'dark');

/** Calls `change` with the page's theme whenever it changes. */
export function onThemeChange(change: (theme: 'light' | 'dark') => void) {
  let last = siteTheme();
  new MutationObserver(() => {
    const now = siteTheme();
    if (now !== last) change((last = now));
  }).observe(document.documentElement, { attributes: true, attributeFilter: ['class'] });
}

/** Demos follow the page's theme: the player's `?theme=` before they load
 * (and in "Open in new tab" links), a message when the theme changes, so a
 * running demo switches without reloading. */
function themeDemos(root: ParentNode) {
  const themed = (url: string) => url.replace(/([?&]theme=)(dark|light)/, `$1${siteTheme()}`);
  // The frames carry their player URL in `data-player-src`; it gets the
  // page's theme before the first load, so a demo never loads twice.
  const frames = [...root.querySelectorAll<HTMLIFrameElement>('.phone__screen iframe[data-player-src]')];
  const links = [...root.querySelectorAll<HTMLAnchorElement>('a[href*="play/index.html?theme="]')];
  const apply = () => {
    const want = siteTheme();
    for (const link of links) link.href = themed(link.href);
    for (const frame of frames) {
      if (frame.dataset.theme === want) continue;
      // Running: tell the player; not loaded yet (lazy): change the URL.
      if (frame.dataset.loaded) frame.contentWindow?.postMessage({ theme: want }, '*');
      else frame.setAttribute('src', themed(frame.dataset.playerSrc!));
      frame.dataset.theme = want;
    }
  };
  for (const frame of frames) frame.dataset.theme = '';
  for (const frame of frames) frame.addEventListener('load', () => (frame.dataset.loaded = '1'));
  apply();
  onThemeChange(apply);
}

/** The frame sizes and idioms of the layouts (`LAYOUTS` in `demo.js`). */
const LAYOUTS = {
  mobile: { width: 390, height: 844, idiom: 'phone' },
  desktop: { width: 1024, height: 640, idiom: 'desktop' },
} as const;
type Layout = keyof typeof LAYOUTS;

/** The layout the visitor chose last, for every demo (a convenience). */
const LAYOUT_KEY = 'nsh-demo-layout';
function savedLayout(): Layout {
  try {
    return localStorage.getItem(LAYOUT_KEY) === 'desktop' ? 'desktop' : 'mobile';
  } catch {
    return 'mobile';
  }
}

/** A player URL for `layout`: its idiom in `?idiom=`. */
const withIdiom = (url: string, layout: Layout) =>
  /[?&]idiom=/.test(url)
    ? url.replace(/([?&]idiom=)[a-z]+/, `$1${LAYOUTS[layout].idiom}`)
    : url.replace('?', `?idiom=${LAYOUTS[layout].idiom}&`);

/** The Mobile / Desktop switch of a demo panel: the frame's size, the
 * idiom the app runs with (it reloads), and a desktop frame scaled to the
 * stage's width. */
function layoutDemo(demo: HTMLElement, all: HTMLElement[]) {
  const phone = demo.querySelector<HTMLElement>('.phone');
  const frame = demo.querySelector<HTMLIFrameElement>('.phone__screen iframe');
  const stage = demo.querySelector<HTMLElement>('.demo__stage');
  const buttons = [...demo.querySelectorAll<HTMLButtonElement>('[data-demo-layout]')];
  if (!phone || !stage || buttons.length === 0) return;
  const fit = () => {
    if (demo.dataset.layout !== 'desktop') {
      phone.style.removeProperty('--phone-scale');
      return;
    }
    const room = stage.clientWidth - 40;
    const scale = Math.max(0.3, Math.min(1, room / (LAYOUTS.desktop.width + 16)));
    phone.style.setProperty('--phone-scale', scale.toFixed(3));
  };
  new ResizeObserver(fit).observe(stage);
  const apply = (layout: Layout, save: boolean) => {
    if (demo.dataset.layout === layout && phone.dataset.layout === layout) return fit();
    demo.dataset.layout = layout;
    phone.dataset.layout = layout;
    const size = LAYOUTS[layout];
    phone.style.setProperty('--phone-w', String(size.width));
    phone.style.setProperty('--phone-h', String(size.height));
    for (const b of buttons) {
      const on = b.dataset.demoLayout === layout;
      b.setAttribute('aria-checked', String(on));
      b.tabIndex = on ? 0 : -1;
    }
    for (const link of demo.querySelectorAll<HTMLAnchorElement>('a[href*="play/index.html"]')) {
      link.href = withIdiom(link.href, layout);
    }
    if (frame?.dataset.playerSrc) {
      frame.width = String(size.width);
      frame.height = String(size.height);
      frame.dataset.playerSrc = withIdiom(frame.dataset.playerSrc, layout);
      // Loaded (or about to be): run it again with the new idiom, and the
      // page's theme (its `src` keeps the one it first loaded with).
      const src = frame.getAttribute('src');
      if (src) frame.setAttribute('src', withIdiom(src, layout).replace(/([?&]theme=)(dark|light)/, `$1${siteTheme()}`));
    }
    fit();
    if (save) {
      try {
        localStorage.setItem(LAYOUT_KEY, layout);
      } catch {}
      // The other demos of the page follow.
      for (const other of all) if (other !== demo) other.dispatchEvent(new CustomEvent('demo-layout', { detail: layout }));
    }
  };
  for (const b of buttons) b.addEventListener('click', () => apply(b.dataset.demoLayout as Layout, true));
  demo.addEventListener('demo-layout', (e) => apply((e as CustomEvent<Layout>).detail, false));
  apply(savedLayout(), false);
}

/** The Preview / Source and Mobile / Desktop switches of demo panels
 * (markup from `demo.js`). */
export function initDemos(root: ParentNode = document) {
  // Before the theme: a frame gets its layout's URL before it first loads.
  const panels = [...root.querySelectorAll<HTMLElement>('.demo[data-layout]')];
  for (const demo of panels) layoutDemo(demo, panels);
  themeDemos(root);
  for (const demo of root.querySelectorAll<HTMLElement>('.demo[data-view]')) {
    const tabs = [...demo.querySelectorAll<HTMLButtonElement>('[data-demo-view]')];
    const show = (view: string, focus = false) => {
      demo.dataset.view = view;
      for (const tab of tabs) {
        const selected = tab.dataset.demoView === view;
        tab.setAttribute('aria-selected', String(selected));
        tab.tabIndex = selected ? 0 : -1;
        const panel = document.getElementById(tab.getAttribute('aria-controls') ?? '');
        // Not `hidden`: on wide panels CSS shows both, which `hidden` (an
        // !important preflight rule) would forbid.
        if (panel) panel.toggleAttribute('data-active', selected);
        if (selected && focus) tab.focus();
      }
    };
    for (const tab of tabs) {
      tab.addEventListener('click', () => show(tab.dataset.demoView!));
      tab.addEventListener('keydown', (event) => {
        if (event.key !== 'ArrowLeft' && event.key !== 'ArrowRight') return;
        event.preventDefault();
        const at = tabs.indexOf(tab);
        const next = tabs[(at + (event.key === 'ArrowRight' ? 1 : tabs.length - 1)) % tabs.length];
        show(next.dataset.demoView!, true);
      });
    }
  }
}
