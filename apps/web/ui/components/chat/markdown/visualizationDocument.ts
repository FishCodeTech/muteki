/**
 * Document bootstrap adapted from T3 Code nightly cfa4f765ec05950a032b6c1cf9cdfff0c2391545.
 * Copyright (c) 2026 T3 Tools Inc. MIT; see T3CODE-LICENSE.txt.
 * Source: packages/shared/src/htmlRender.ts, PR #15968.
 * Adapted for Muteki's message snapshots, theme tokens and restricted sandbox.
 */
import { visualizationBridge } from "./visualizationBridge";
import type { VisualizationTheme } from "./useVisualizationTheme";

const CDN = "https://cdnjs.cloudflare.com https://esm.sh https://cdn.jsdelivr.net https://unpkg.com";
const CSP = `default-src 'none'; script-src 'unsafe-inline' ${CDN}; style-src 'unsafe-inline' ${CDN} https://fonts.googleapis.com https://fonts.bunny.net; font-src https://fonts.gstatic.com https://fonts.bunny.net; img-src data: blob: ${CDN}; connect-src 'none'; frame-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'`;
const STYLE = `
:root { color-scheme: light dark; --background: light-dark(#fff,#18191b); --foreground: light-dark(#25262a,#e7e7e9);
--card: light-dark(#f6f6f7,#24252a); --card-foreground: var(--foreground); --muted: var(--card); --muted-foreground: light-dark(#64656b,#acadb3);
--popover: var(--card); --popover-foreground: var(--foreground); --primary: var(--foreground); --primary-foreground: var(--background);
--secondary: var(--card); --secondary-foreground: var(--foreground); --accent: var(--card); --accent-foreground: var(--foreground);
--border: light-dark(#dddde1,#3e3f45); --input: var(--border); --ring: #709be9; --destructive: #d95c5c;
--viz-series-1:#729bdb; --viz-series-2:#62ae97; --viz-series-3:#d5a354; --viz-series-4:#b18dd7; --viz-series-5:#d5839b; --viz-series-6:#76baca;
--blue:var(--viz-series-1); --green:var(--viz-series-2); --orange:var(--viz-series-3); --purple:var(--viz-series-4); --red:var(--destructive); --yellow:#d6c26e; --font-size-base:14px; }
* { box-sizing:border-box } html { background:var(--background); color:var(--foreground); font:14px/1.5 var(--font-sans,system-ui,sans-serif) } body { display:flow-root; margin:0; background:transparent; overflow-wrap:anywhere } code,pre,kbd,samp { font-family:var(--font-mono,monospace) }
svg,canvas,img { max-width:100% } h1,h2,h3 { font-size:16px; font-weight:500 } .card { padding:14px; background:var(--card); border-radius:10px }
.viz-row,.viz-controls { display:flex; align-items:center; flex-wrap:wrap; gap:12px } .viz-grid { display:grid; grid-template-columns:repeat(auto-fit,minmax(min(180px,100%),1fr)); gap:12px }
.text-muted { color:var(--muted-foreground) } .text-small { font-size:12px } .text-destructive { color:var(--destructive) } .text-end{text-align:right} .text-center{text-align:center} .text-nowrap{white-space:nowrap} .tabular-nums{font-variant-numeric:tabular-nums}
.btn,.form-control,.form-select { font:inherit; color:var(--foreground); background:var(--card); border:1px solid var(--border); border-radius:7px; padding:6px 10px; max-width:100% }
.btn{cursor:pointer} .btn-primary { background:var(--primary);color:var(--primary-foreground) } .btn-ghost { background:transparent;border-color:transparent } .btn-block{width:100%}
.form-label { display:block; font-size:12px } .form-range { max-width:100% } .form-check { display:flex; align-items:center; gap:7px } .nav{display:flex;gap:6px;flex-wrap:wrap}
.nav-link{font:inherit;color:var(--foreground);background:transparent;border:0;padding:7px 12px;border-radius:7px;cursor:pointer} .nav-link.active{background:var(--card)}
.table { width:100%;border-collapse:collapse } .table td,.table th{padding:8px;text-align:start;border-bottom:1px solid var(--border)} .table-responsive{overflow:auto}
.sr-only{position:absolute;width:1px;height:1px;padding:0;overflow:hidden;clip:rect(0,0,0,0)} hr{border:0;border-top:1px solid var(--border)}
`;

const blankNonMarkup = (html: string) => {
  const scan = html.replace(
    /<!--[\s\S]*?(?:-->|$)|<(script|style|textarea|title|xmp|iframe|noembed|noframes|noscript)\b[\s\S]*?(?:<\/\1\s*>|$)|<plaintext\b[\s\S]*$/gi,
    (match) => " ".repeat(match.length),
  );
  const parts: string[] = [];
  let depth = 0;
  let start = 0;
  let at = 0;
  for (const match of scan.matchAll(/<(\/?)template(?:\s[^>]*)?\/?>/gi)) {
    if (!match[1]) {
      if (depth++ === 0) start = match.index;
    } else if (depth > 0 && --depth === 0) {
      const end = match.index + match[0].length;
      parts.push(scan.slice(at, start), " ".repeat(end - start));
      at = end;
    }
  }
  if (depth > 0) {
    parts.push(scan.slice(at, start), " ".repeat(scan.length - start));
    at = scan.length;
  }
  parts.push(scan.slice(at));
  return parts.join("");
};

export function injectVisualizationBootstrap(html: string, markup: string): string {
  const scan = blankNonMarkup(html);
  const headOpen = /<head(?:\s[^>]*)?>/i.exec(scan);
  if (headOpen) {
    const at = headOpen.index + headOpen[0].length;
    return html.slice(0, at) + markup + html.slice(at);
  }
  const htmlOpen = /<html(?:\s[^>]*)?>/i.exec(scan);
  if (htmlOpen) {
    const at = htmlOpen.index + htmlOpen[0].length;
    return `${html.slice(0, at)}<head>${markup}</head>${html.slice(at)}`;
  }
  const doctype = /^\s*<!doctype[^>]*>/i.exec(html);
  if (doctype) {
    const at = doctype[0].length;
    return `${html.slice(0, at)}<head>${markup}</head>${html.slice(at)}`;
  }
  return `<!doctype html><head>${markup}</head>${html}`;
}

export function visualizationThemeCss(theme: VisualizationTheme): string {
  const properties = Object.entries(theme.variables).filter(([name]) => /^--[a-z0-9-]+$/.test(name))
    .map(([name, value]) => `${name}:${value.replace(/[;{}<>]/g, "")};`).join("");
  return `:root{color-scheme:${theme.appearance};${properties}}`;
}

export function buildVisualizationDocument(html: string, assets: Record<string, string>, theme: VisualizationTheme,
  nonce: string, standalone = false): string {
  const script = (source: string) => "<script>" + source.replace(/<\/script/gi, "<\\/script") + "</script>";
  const scan = blankNonMarkup(html);
  const content = /<html\b|<!doctype\b/i.test(scan) ? html
    : assets["visualize.html"]?.replace("<!--__INLINE_VISUALIZATION_FRAGMENT__-->", html) || html;
  const markup = `<meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><meta name="referrer" content="no-referrer"><style>${STYLE}${assets["visualize.css"] || ""}</style><style id="muteki-visualization-theme">${visualizationThemeCss(theme)}</style>`
    + script(visualizationBridge(nonce, standalone))
    + `<script defer src="https://cdn.jsdelivr.net/npm/lucide@0.468.0/dist/umd/lucide.min.js"></script>`
    + (assets["calendar.js"] ? script(assets["calendar.js"]) : "");
  const document = injectVisualizationBootstrap(content, markup);
  // Put the policy before *all* authored content, including malformed content
  // preceding an explicit head. Never parse the page in the host DOM.
  const doctype = /^\s*<!doctype[^>]*>/i.exec(document);
  const at = doctype?.[0].length || 0;
  return document.slice(0, at) + `<meta http-equiv="Content-Security-Policy" content="${CSP}">` + document.slice(at);
}
