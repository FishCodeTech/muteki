const INTERACTIVE = 'a[href],button,input:not([type=hidden]),textarea,select,summary,[role=button],[role=link],[role=checkbox],[role=radio],[role=tab],[role=menuitem],[role=option],[role=switch],[role=textbox],[role=combobox],[role=searchbox],[contenteditable=""],[contenteditable=true],[onclick],[tabindex]:not([tabindex="-1"])';
function pageRead({ mode, selector, interactive }) {
  const root = selector ? document.querySelector(selector) : (document.body || document.documentElement);
  if (!root) return { missing: true };
  const base = { url: location.href, title: document.title };
  if (mode === "html") return { ...base, content: root.outerHTML };
  if (mode === "text") return { ...base, content: root.innerText ?? root.textContent ?? "" };
  const visible = (el) => {
    const rect = el.getBoundingClientRect();
    if (rect.width <= 0 || rect.height <= 0) return false;
    const style = getComputedStyle(el);
    return style.visibility !== "hidden" && style.display !== "none" && Number(style.opacity) !== 0;
  };
  let seq = Number(document.documentElement.dataset.mutekiRefSeq || 0);
  const elements = [];
  for (const el of root.querySelectorAll(interactive)) {
    if (!visible(el)) continue;
    if (!el.dataset.mutekiRef) el.dataset.mutekiRef = String(++seq);
    const rect = el.getBoundingClientRect();
    const type = el.getAttribute("type") || undefined;
    const secret = type === "password";
    const hasValue = "value" in el && typeof el.value === "string";
    elements.push({
      ref: Number(el.dataset.mutekiRef), tag: el.tagName.toLowerCase(), role: el.getAttribute("role") || undefined, type,
      name: el.getAttribute("aria-label") || el.getAttribute("placeholder") || el.getAttribute("title") || el.getAttribute("alt") || el.getAttribute("name") || undefined,
      text: (el.innerText || "").replace(/\s+/g, " ").trim() || undefined,
      value: hasValue && !secret ? el.value : undefined, value_length: hasValue && secret ? el.value.length : undefined,
      href: typeof el.href === "string" && el.href ? el.href : undefined,
      checked: type === "checkbox" || type === "radio" ? Boolean(el.checked) : undefined,
      disabled: el.disabled === true || el.getAttribute("aria-disabled") === "true" || undefined,
      in_viewport: rect.bottom > 0 && rect.right > 0 && rect.top < innerHeight && rect.left < innerWidth,
    });
  }
  document.documentElement.dataset.mutekiRefSeq = String(seq);
  return { ...base, elements };
}
function pageTarget({ ref, selector, text, interactive }) {
  const visible = (el) => {
    const rect = el.getBoundingClientRect();
    if (rect.width <= 0 || rect.height <= 0) return false;
    const style = getComputedStyle(el);
    return style.visibility !== "hidden" && style.display !== "none";
  };
  const label = (el) => (el.innerText || el.value || el.getAttribute("aria-label") || "").replace(/\s+/g, " ").trim();
  let el = null;
  if (ref) el = document.querySelector(`[data-muteki-ref="${Number(ref)}"]`);
  else if (selector) el = document.querySelector(selector);
  else if (text) {
    const want = text.replace(/\s+/g, " ").trim().toLowerCase();
    const pool = Array.from(document.querySelectorAll(interactive)).filter(visible);
    el = pool.find((item) => label(item).toLowerCase() === want) || pool.find((item) => label(item).toLowerCase().includes(want)) || null;
    if (!el) el = Array.from(document.body.querySelectorAll("*")).filter((item) => label(item).toLowerCase() === want && visible(item)).at(-1) || null;
  }
  if (!el) return { missing: true };
  el.scrollIntoView({ block: "center", inline: "center", behavior: "instant" });
  const rect = el.getBoundingClientRect();
  const x = rect.left + rect.width / 2, y = rect.top + rect.height / 2;
  const hit = document.elementFromPoint(x, y);
  return { x, y, tag: el.tagName.toLowerCase(), text: label(el) || undefined, covered_by: hit && hit !== el && !el.contains(hit) ? hit.tagName.toLowerCase() : undefined };
}
function pageFocused({ caretToEnd, expectedRef }) {
  const el = document.activeElement;
  if (!el || el === document.body || el === document.documentElement) return null;
  if (expectedRef && el.dataset.mutekiRef !== expectedRef) return null;
  const tag = el.tagName.toLowerCase();
  const editable = el.isContentEditable || tag === "textarea" || tag === "select"
    || (tag === "input" && !["button", "submit", "reset", "checkbox", "radio", "file", "image", "range", "color"].includes(el.type));
  if (caretToEnd && typeof el.value === "string" && typeof el.setSelectionRange === "function") {
    try { el.setSelectionRange(el.value.length, el.value.length); } catch {   }
  }
  const text = typeof el.value === "string" ? el.value : (el.isContentEditable ? (el.innerText || "") : undefined);
  const secret = tag === "input" && el.type === "password";
  return { tag, type: el.getAttribute("type") || undefined, name: el.getAttribute("name") || el.getAttribute("aria-label") || el.getAttribute("placeholder") || undefined,
    editable, value: secret ? undefined : text, value_length: text === undefined ? undefined : text.length };
}
function pageInsert({ clear, text, submit }) {
  const el = document.activeElement;
  if (clear) {
    if (el && typeof el.select === "function") el.select(); else document.execCommand("selectAll");
    if (!text) document.execCommand("delete");
  }
  if (text && !document.execCommand("insertText", false, text)) return false;
  if (!submit) return true;
  if (!el.dataset.mutekiRef) {
    const seq = Number(document.documentElement.dataset.mutekiRefSeq || 0) + 1;
    document.documentElement.dataset.mutekiRefSeq = String(seq);
    el.dataset.mutekiRef = String(seq);
  }
  return el.dataset.mutekiRef;
}
function pagePickElement({ htmlLimit }) {
  const KEY = "__mutekiElementPicker";
  if (window[KEY]) window[KEY].cancel();
  return new Promise((resolve) => {
    const box = document.createElement("div");
    const tag = document.createElement("div");
    const style = document.createElement("style");
    const z = "2147483647";
    box.setAttribute("data-muteki-picker", "");
    tag.setAttribute("data-muteki-picker", "");
    box.style.cssText = `position:fixed;pointer-events:none;z-index:${z};border:2px solid #3b82f6;background:rgba(59,130,246,.14);border-radius:3px;display:none;box-sizing:border-box`;
    tag.style.cssText = `position:fixed;pointer-events:none;z-index:${z};font:600 11px/1.4 ui-monospace,SFMono-Regular,Menlo,monospace;color:#fff;background:#1d4ed8;padding:2px 6px;border-radius:4px;white-space:nowrap;max-width:60vw;overflow:hidden;text-overflow:ellipsis;display:none`;
    style.textContent = "*{cursor:crosshair!important}";
    document.documentElement.append(style, box, tag);
    let target = null;
    const short = (el) => {
      let out = el.tagName.toLowerCase();
      if (el.id) out += `#${el.id}`;
      const classes = [...el.classList].slice(0, 2);
      if (classes.length) out += `.${classes.join(".")}`;
      return out;
    };
    const esc = (value) => (window.CSS && CSS.escape ? CSS.escape(value) : value.replace(/[^\w-]/g, "\\$&"));
    const selectorFor = (el) => {
      if (el.id && document.querySelectorAll(`#${esc(el.id)}`).length === 1) return `#${esc(el.id)}`;
      const parts = [];
      let node = el;
      while (node && node.nodeType === 1 && node !== document.documentElement) {
        let part = node.tagName.toLowerCase();
        if (node.id && document.querySelectorAll(`#${esc(node.id)}`).length === 1) { parts.unshift(`#${esc(node.id)}`); break; }
        const testId = node.getAttribute("data-testid");
        if (testId) part += `[data-testid="${testId.replace(/"/g, '\\"')}"]`;
        const parent = node.parentElement;
        if (parent) {
          const same = [...parent.children].filter((child) => child.tagName === node.tagName);
          if (same.length > 1) part += `:nth-of-type(${same.indexOf(node) + 1})`;
        }
        parts.unshift(part);
        const candidate = parts.join(" > ");
        try { if (document.querySelectorAll(candidate).length === 1) return candidate; } catch {   }
        node = parent;
      }
      return parts.join(" > ");
    };
    const describe = (el) => {
      const rect = el.getBoundingClientRect();
      const secret = el.tagName.toLowerCase() === "input" && el.type === "password";
      const attrs = {};
      for (const name of ["role", "aria-label", "name", "type", "href", "src", "alt", "placeholder", "title", "data-testid", "value"]) {
        if (name === "value" && secret) continue;
        const value = name === "value" && "value" in el ? el.value : el.getAttribute(name);
        if (value != null && value !== "") attrs[name] = String(value);
      }
      const computed = getComputedStyle(el);
      const styles = {};
      for (const name of ["display", "position", "color", "background-color", "font-size", "font-weight", "font-family", "padding", "margin", "border-radius"]) styles[name] = computed.getPropertyValue(name);
      const copy = el.cloneNode(true);
      const passwords = [...copy.querySelectorAll('input[type="password"]')];
      if (secret) passwords.push(copy);
      for (const password of passwords) password.removeAttribute("value");
      const html = copy.outerHTML || "";
      const text = (el.innerText || el.textContent || "").replace(/\s+/g, " ").trim();
      return {
        url: location.href, title: document.title, tag: el.tagName.toLowerCase(), selector: selectorFor(el),
        id: el.id || null, classes: [...el.classList], text, attributes: attrs, styles,
        html: html.length > htmlLimit ? html.slice(0, htmlLimit) : html, html_length: html.length, html_truncated: html.length > htmlLimit,
        rect: { x: rect.x, y: rect.y, width: rect.width, height: rect.height },
        viewport: { width: innerWidth, height: innerHeight }, device_pixel_ratio: devicePixelRatio,
      };
    };
    const show = (el) => {
      const rect = el.getBoundingClientRect();
      Object.assign(box.style, { display: "block", left: `${rect.left}px`, top: `${rect.top}px`, width: `${rect.width}px`, height: `${rect.height}px` });
      tag.textContent = `${short(el)}  ${Math.round(rect.width)}×${Math.round(rect.height)}`;
      const above = rect.top > 22;
      Object.assign(tag.style, { display: "block", left: `${Math.max(0, rect.left)}px`, top: `${above ? rect.top - 21 : Math.min(innerHeight - 20, rect.bottom + 3)}px` });
    };
    const pick = (event) => {
      const el = document.elementFromPoint(event.clientX, event.clientY);
      if (!el || el.hasAttribute("data-muteki-picker")) return;
      target = el;
      show(el);
    };
    const swallow = (event) => { event.preventDefault(); event.stopPropagation(); event.stopImmediatePropagation(); };
    const finish = (value) => {
      window.removeEventListener("mousemove", pick, true);
      for (const type of ["mousedown", "mouseup", "pointerdown", "pointerup", "dblclick", "contextmenu"]) window.removeEventListener(type, swallow, true);
      window.removeEventListener("click", choose, true);
      window.removeEventListener("keydown", key, true);
      box.remove(); tag.remove(); style.remove();
      delete window[KEY];
      let settled = false;
      const settle = () => { if (!settled) { settled = true; resolve(value); } };
      requestAnimationFrame(() => requestAnimationFrame(settle));
      setTimeout(settle, 150);
    };
    const choose = (event) => {
      swallow(event);
      const el = target || document.elementFromPoint(event.clientX, event.clientY);
      finish(el ? describe(el) : null);
    };
    const key = (event) => {
      if (event.key === "Escape") { swallow(event); finish(null); }
      else if (event.key === "ArrowUp" && target && target.parentElement && target.parentElement !== document.documentElement) { swallow(event); target = target.parentElement; show(target); }
      else if (event.key === "Enter" && target) { swallow(event); finish(describe(target)); }
    };
    window.addEventListener("mousemove", pick, true);
    for (const type of ["mousedown", "mouseup", "pointerdown", "pointerup", "dblclick", "contextmenu"]) window.addEventListener(type, swallow, true);
    window.addEventListener("click", choose, true);
    window.addEventListener("keydown", key, true);
    window[KEY] = { cancel: () => finish(null) };
  });
}
function pageCancelPick() {
  if (window.__mutekiElementPicker) window.__mutekiElementPicker.cancel();
  return true;
}
function pageScroll({ direction, amount, to }) {
  const root = document.scrollingElement || document.documentElement;
  if (to === "top") window.scrollTo({ top: 0, behavior: "instant" });
  else if (to === "bottom") window.scrollTo({ top: root.scrollHeight, behavior: "instant" });
  else if (direction) {
    const step = amount || (direction === "left" || direction === "right" ? innerWidth : innerHeight) * 0.85;
    window.scrollBy({ left: direction === "left" ? -step : direction === "right" ? step : 0, top: direction === "up" ? -step : direction === "down" ? step : 0, behavior: "instant" });
  }
  return { scroll_x: Math.round(scrollX), scroll_y: Math.round(scrollY), page_width: root.scrollWidth, page_height: root.scrollHeight, viewport: { width: innerWidth, height: innerHeight } };
}
function pagePresent({ selector, text }) {
  if (selector) {
    const el = document.querySelector(selector);
    if (!el) return false;
    const rect = el.getBoundingClientRect();
    return rect.width > 0 && rect.height > 0;
  }
  const want = text.replace(/\s+/g, " ").trim().toLowerCase();
  return ((document.body && document.body.innerText) || "").replace(/\s+/g, " ").toLowerCase().includes(want);
}
function pageConsole() {
  const log = window.__mutekiConsole;
  if (!log) return null;
  if (log.error) return { rows: [], dropped: log.dropped || 0, error: log.error };
  const rows = log.rows.map((row) => {
    const level = row.level === "debug" ? "verbose" : row.level === "warn" ? "warning" : row.level || "info";
    const text = String(row.message ?? row.text ?? "");
    const timestamp = String(row.timestamp || row.at || "");
    return {
      level,
      message: text,
      text,
      url: String(row.url || location.href),
      timestamp,
      at: timestamp,
      ...(typeof row.line === "number" ? { line: row.line } : {}),
    };
  });
  return { rows, dropped: log.dropped || 0 };
}
function pageNetwork() {
  const log = window.__mutekiNetwork;
  if (!log) return null;
  if (log.error) return { rows: [], dropped: log.dropped || 0, error: log.error };
  const rows = log.rows.map((row) => {
    const status = Number(row.status) || 0;
    const timestamp = String(row.timestamp || row.at || "");
    return {
      id: row.id,
      method: String(row.method || "GET").toUpperCase(),
      url: String(row.url || location.href),
      status,
      statusText: String(row.statusText || ""),
      ok: typeof row.ok === "boolean" ? row.ok : status >= 200 && status < 300,
      duration: Math.max(0, Math.round(Number(row.duration) || 0)),
      initiatorType: String(row.initiatorType || "other"),
      timestamp,
      at: timestamp,
      ...(row.error ? { error: String(row.error) } : {}),
    };
  });
  return { rows, dropped: log.dropped || 0 };
}
