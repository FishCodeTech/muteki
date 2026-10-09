// Provider-driven control of a thread's native preview (chat_browser_* tools).
// Page functions below run inside the isolated preview page through
// executeJavaScript, so each must be self-contained.
const { DesktopError } = require('./transport.cjs');
const { parseWebUrl } = require('./policy.cjs');

const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
async function until(predicate, timeoutMs, step = 100) {
  const end = Date.now() + timeoutMs;
  for (;;) {
    const value = predicate();
    if (value || Date.now() >= end) return value;
    await sleep(step);
  }
}

function pageRead({ mode, selector }) {
  const root = selector ? document.querySelector(selector) : (document.body || document.documentElement);
  if (!root) return { missing: true };
  const base = { url: location.href, title: document.title };
  if (mode === 'html') return { ...base, content: root.outerHTML };
  if (mode === 'text') return { ...base, content: root.innerText ?? root.textContent ?? '' };
  const interactive = 'a[href],button,input:not([type=hidden]),textarea,select,summary,[role=button],[role=link],[role=checkbox],[role=radio],[role=tab],[role=menuitem],[role=option],[role=switch],[role=textbox],[role=combobox],[role=searchbox],[contenteditable=""],[contenteditable=true],[onclick],[tabindex]:not([tabindex="-1"])';
  const visible = el => {
    const rect = el.getBoundingClientRect();
    if (rect.width <= 0 || rect.height <= 0) return false;
    const style = getComputedStyle(el);
    return style.visibility !== 'hidden' && style.display !== 'none' && Number(style.opacity) !== 0;
  };
  let seq = Number(document.documentElement.dataset.mutekiRefSeq || 0);
  const elements = [];
  for (const el of root.querySelectorAll(interactive)) {
    if (!visible(el)) continue;
    if (!el.dataset.mutekiRef) el.dataset.mutekiRef = String(++seq);
    const rect = el.getBoundingClientRect();
    const type = el.getAttribute('type') || undefined;
    const secret = type === 'password';
    const hasValue = 'value' in el && typeof el.value === 'string';
    elements.push({
      ref: Number(el.dataset.mutekiRef), tag: el.tagName.toLowerCase(), role: el.getAttribute('role') || undefined, type,
      name: el.getAttribute('aria-label') || el.getAttribute('placeholder') || el.getAttribute('title') || el.getAttribute('alt') || el.getAttribute('name') || undefined,
      text: (el.innerText || '').replace(/\s+/g, ' ').trim() || undefined,
      value: hasValue && !secret ? el.value : undefined, value_length: hasValue && secret ? el.value.length : undefined,
      href: typeof el.href === 'string' && el.href ? el.href : undefined,
      checked: type === 'checkbox' || type === 'radio' ? Boolean(el.checked) : undefined,
      disabled: el.disabled === true || el.getAttribute('aria-disabled') === 'true' || undefined,
      in_viewport: rect.bottom > 0 && rect.right > 0 && rect.top < innerHeight && rect.left < innerWidth,
    });
  }
  document.documentElement.dataset.mutekiRefSeq = String(seq);
  return { ...base, elements };
}

function pageTarget({ ref, selector, text }) {
  const interactive = 'a[href],button,input:not([type=hidden]),textarea,select,summary,[role=button],[role=link],[role=checkbox],[role=radio],[role=tab],[role=menuitem],[role=option],[role=switch],[role=textbox],[role=combobox],[role=searchbox],[contenteditable=""],[contenteditable=true],[onclick],[tabindex]:not([tabindex="-1"])';
  const visible = el => {
    const rect = el.getBoundingClientRect();
    if (rect.width <= 0 || rect.height <= 0) return false;
    const style = getComputedStyle(el);
    return style.visibility !== 'hidden' && style.display !== 'none';
  };
  const label = el => (el.innerText || el.value || el.getAttribute('aria-label') || '').replace(/\s+/g, ' ').trim();
  let el = null;
  if (ref) el = document.querySelector(`[data-muteki-ref="${Number(ref)}"]`);
  else if (selector) el = document.querySelector(selector);
  else if (text) {
    const want = text.replace(/\s+/g, ' ').trim().toLowerCase();
    const pool = Array.from(document.querySelectorAll(interactive)).filter(visible);
    el = pool.find(item => label(item).toLowerCase() === want) || pool.find(item => label(item).toLowerCase().includes(want)) || null;
    if (!el) {
      const exact = Array.from(document.body.querySelectorAll('*')).filter(item => label(item).toLowerCase() === want && visible(item));
      el = exact.at(-1) || null;
    }
  }
  if (!el) return { missing: true };
  el.scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant' });
  const rect = el.getBoundingClientRect();
  const x = rect.left + rect.width / 2, y = rect.top + rect.height / 2;
  const hit = document.elementFromPoint(x, y);
  return { x, y, tag: el.tagName.toLowerCase(), text: label(el) || undefined,
    covered_by: hit && hit !== el && !el.contains(hit) ? hit.tagName.toLowerCase() : undefined };
}

function pageFocused({ caretToEnd }) {
  const el = document.activeElement;
  if (!el || el === document.body || el === document.documentElement) return null;
  const tag = el.tagName.toLowerCase();
  const editable = el.isContentEditable || tag === 'textarea' || tag === 'select'
    || (tag === 'input' && !['button', 'submit', 'reset', 'checkbox', 'radio', 'file', 'image', 'range', 'color'].includes(el.type));
  if (caretToEnd && typeof el.value === 'string' && typeof el.setSelectionRange === 'function') {
    try { el.setSelectionRange(el.value.length, el.value.length); } catch { /* input types without selection */ }
  }
  return { tag, type: el.getAttribute('type') || undefined, name: el.getAttribute('name') || el.getAttribute('aria-label') || el.getAttribute('placeholder') || undefined,
    editable, value_length: typeof el.value === 'string' ? el.value.length : (el.isContentEditable ? (el.innerText || '').length : undefined) };
}

// User-driven element picker. Resolves with the clicked element's description,
// or null when the user presses Escape or the picker is cancelled.
function pagePickElement({ htmlLimit }) {
  const KEY = '__mutekiElementPicker';
  if (window[KEY]) window[KEY].cancel();
  return new Promise(resolve => {
    const box = document.createElement('div');
    const tag = document.createElement('div');
    const style = document.createElement('style');
    const z = '2147483647';
    box.setAttribute('data-muteki-picker', '');
    tag.setAttribute('data-muteki-picker', '');
    box.style.cssText = `position:fixed;pointer-events:none;z-index:${z};border:2px solid #3b82f6;background:rgba(59,130,246,.14);border-radius:3px;transition:all 60ms ease-out;display:none;box-sizing:border-box`;
    tag.style.cssText = `position:fixed;pointer-events:none;z-index:${z};font:600 11px/1.4 ui-monospace,SFMono-Regular,Menlo,monospace;color:#fff;background:#1d4ed8;padding:2px 6px;border-radius:4px;white-space:nowrap;max-width:60vw;overflow:hidden;text-overflow:ellipsis;display:none`;
    style.textContent = '*{cursor:crosshair!important}';
    document.documentElement.append(style, box, tag);
    let target = null;
    const short = el => {
      let out = el.tagName.toLowerCase();
      if (el.id) out += `#${el.id}`;
      const classes = [...el.classList].slice(0, 2);
      if (classes.length) out += `.${classes.join('.')}`;
      return out;
    };
    const esc = value => (window.CSS && CSS.escape ? CSS.escape(value) : value.replace(/[^\w-]/g, '\\$&'));
    const selectorFor = el => {
      if (el.id && document.querySelectorAll(`#${esc(el.id)}`).length === 1) return `#${esc(el.id)}`;
      const parts = [];
      let node = el;
      while (node && node.nodeType === 1 && node !== document.documentElement) {
        let part = node.tagName.toLowerCase();
        if (node.id && document.querySelectorAll(`#${esc(node.id)}`).length === 1) { parts.unshift(`#${esc(node.id)}`); break; }
        const testId = node.getAttribute('data-testid');
        if (testId) part += `[data-testid="${testId.replace(/"/g, '\\"')}"]`;
        const parent = node.parentElement;
        if (parent) {
          const same = [...parent.children].filter(child => child.tagName === node.tagName);
          if (same.length > 1) part += `:nth-of-type(${same.indexOf(node) + 1})`;
        }
        parts.unshift(part);
        const candidate = parts.join(' > ');
        try { if (document.querySelectorAll(candidate).length === 1) return candidate; } catch { /* keep climbing */ }
        node = parent;
      }
      return parts.join(' > ');
    };
    const describe = el => {
      const rect = el.getBoundingClientRect();
      const attrs = {};
      for (const name of ['role', 'aria-label', 'name', 'type', 'href', 'src', 'alt', 'placeholder', 'title', 'data-testid', 'value']) {
        const value = name === 'value' && 'value' in el ? el.value : el.getAttribute(name);
        if (value != null && value !== '') attrs[name] = String(value);
      }
      const computed = getComputedStyle(el);
      const styles = {};
      for (const name of ['display', 'position', 'color', 'background-color', 'font-size', 'font-weight', 'font-family', 'padding', 'margin', 'border-radius']) styles[name] = computed.getPropertyValue(name);
      const html = el.outerHTML || '';
      const text = (el.innerText || el.textContent || '').replace(/\s+/g, ' ').trim();
      return {
        url: location.href, title: document.title, tag: el.tagName.toLowerCase(), selector: selectorFor(el),
        id: el.id || null, classes: [...el.classList], text, attributes: attrs, styles,
        html: html.length > htmlLimit ? html.slice(0, htmlLimit) : html, html_length: html.length, html_truncated: html.length > htmlLimit,
        rect: { x: rect.x, y: rect.y, width: rect.width, height: rect.height },
        viewport: { width: innerWidth, height: innerHeight }, device_pixel_ratio: devicePixelRatio,
      };
    };
    const show = el => {
      const rect = el.getBoundingClientRect();
      Object.assign(box.style, { display: 'block', left: `${rect.left}px`, top: `${rect.top}px`, width: `${rect.width}px`, height: `${rect.height}px` });
      tag.textContent = `${short(el)}  ${Math.round(rect.width)}×${Math.round(rect.height)}`;
      const above = rect.top > 22;
      Object.assign(tag.style, { display: 'block', left: `${Math.max(0, rect.left)}px`, top: `${above ? rect.top - 21 : Math.min(innerHeight - 20, rect.bottom + 3)}px` });
    };
    const pick = event => {
      const el = document.elementFromPoint(event.clientX, event.clientY);
      if (!el || el.hasAttribute('data-muteki-picker')) return;
      target = el;
      show(el);
    };
    const swallow = event => { event.preventDefault(); event.stopPropagation(); event.stopImmediatePropagation(); };
    const finish = value => {
      window.removeEventListener('mousemove', pick, true);
      for (const type of ['mousedown', 'mouseup', 'pointerdown', 'pointerup', 'dblclick', 'contextmenu']) window.removeEventListener(type, swallow, true);
      window.removeEventListener('click', choose, true);
      window.removeEventListener('keydown', key, true);
      box.remove(); tag.remove(); style.remove();
      delete window[KEY];
      // Two frames so the highlight is gone from the next captured frame; rAF
      // never fires while the view is hidden/occluded, so a timer backs it up.
      let settled = false;
      const settle = () => { if (!settled) { settled = true; resolve(value); } };
      requestAnimationFrame(() => requestAnimationFrame(settle));
      setTimeout(settle, 150);
    };
    const choose = event => {
      swallow(event);
      const el = target || document.elementFromPoint(event.clientX, event.clientY);
      finish(el ? describe(el) : null);
    };
    const key = event => {
      if (event.key === 'Escape') { swallow(event); finish(null); }
      else if (event.key === 'ArrowUp' && target?.parentElement && target.parentElement !== document.documentElement) { swallow(event); target = target.parentElement; show(target); }
      else if (event.key === 'Enter' && target) { swallow(event); finish(describe(target)); }
    };
    window.addEventListener('mousemove', pick, true);
    for (const type of ['mousedown', 'mouseup', 'pointerdown', 'pointerup', 'dblclick', 'contextmenu']) window.addEventListener(type, swallow, true);
    window.addEventListener('click', choose, true);
    window.addEventListener('keydown', key, true);
    window[KEY] = { cancel: () => finish(null) };
  });
}

function pageCancelPick() {
  window.__mutekiElementPicker?.cancel();
  return true;
}

function pageScroll({ direction, amount, to }) {
  const root = document.scrollingElement || document.documentElement;
  if (to === 'top') window.scrollTo({ top: 0, behavior: 'instant' });
  else if (to === 'bottom') window.scrollTo({ top: root.scrollHeight, behavior: 'instant' });
  else if (direction) {
    const step = amount || (direction === 'left' || direction === 'right' ? innerWidth : innerHeight) * 0.85;
    window.scrollBy({ left: direction === 'left' ? -step : direction === 'right' ? step : 0, top: direction === 'up' ? -step : direction === 'down' ? step : 0, behavior: 'instant' });
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
  const want = text.replace(/\s+/g, ' ').trim().toLowerCase();
  return (document.body?.innerText || '').replace(/\s+/g, ' ').toLowerCase().includes(want);
}

function page(fn, arg) { return `(${fn.toString()})(${JSON.stringify(arg)})`; }

// The expression is evaluated as written by the agent; serialisation keeps DOM
// nodes and cycles readable instead of failing JSON.stringify.
function evaluateScript(expression) {
  return `(async () => {
    const value = await (${expression}
    );
    const seen = new WeakSet();
    const json = JSON.stringify(value === undefined ? null : value, (key, item) => {
      if (typeof item === 'bigint') return item.toString();
      if (typeof item === 'function') return '[function ' + (item.name || 'anonymous') + ']';
      if (item instanceof Element) return '<' + item.tagName.toLowerCase() + (item.id ? '#' + item.id : '') + '>';
      if (item && typeof item === 'object') { if (seen.has(item)) return '[circular]'; seen.add(item); }
      return item;
    });
    return { type: value === null ? 'null' : Array.isArray(value) ? 'array' : typeof value, json: json ?? 'null' };
  })()`;
}

const KEY_CODES = { ArrowUp: 'Up', ArrowDown: 'Down', ArrowLeft: 'Left', ArrowRight: 'Right', ' ': 'Space', Esc: 'Escape' };
const EVALUATE_MAX_CHARS = 200000;

const PICK_HTML_LIMIT = 6000;
const PICK_PADDING = 6;

/** Runs the element picker on a user-opened preview and crops a screenshot of the pick. */
async function pickElement(preview) {
  const contents = preview.webContents;
  contents.focus();
  let picked;
  try { picked = await contents.executeJavaScript(page(pagePickElement, { htmlLimit: PICK_HTML_LIMIT }), true); }
  catch (error) { throw new DesktopError('desktop.preview_pick_failed', `元素选择器无法在此页面运行：${error?.message || error}`); }
  if (!picked) return null;
  const bounds = preview.getBounds();
  const x = Math.max(0, Math.floor(picked.rect.x - PICK_PADDING)), y = Math.max(0, Math.floor(picked.rect.y - PICK_PADDING));
  const right = Math.min(bounds.width, Math.ceil(picked.rect.x + picked.rect.width + PICK_PADDING));
  const bottom = Math.min(bounds.height, Math.ceil(picked.rect.y + picked.rect.height + PICK_PADDING));
  let screenshot = null;
  let screenshotError = null;
  if (right - x >= 2 && bottom - y >= 2) {
    try {
      const image = await contents.capturePage({ x, y, width: right - x, height: bottom - y });
      if (image.isEmpty()) screenshotError = '截图为空（预览可能被遮挡或未显示）';
      else {
        const size = image.getSize();
        screenshot = { mime: 'image/png', data: image.toPNG().toString('base64'), width: size.width, height: size.height };
      }
    } catch (error) {
      // 截图只是附加信息；元素描述已拿到，带上失败原因交给界面展示。
      screenshotError = `截图失败：${error?.message || error}`;
    }
  }
  return { ...picked, screenshot, screenshot_error: screenshotError, screenshot_clipped: picked.rect.x < 0 || picked.rect.y < 0 || picked.rect.x + picked.rect.width > bounds.width || picked.rect.y + picked.rect.height > bounds.height };
}

async function cancelPick(preview) {
  try { await preview.webContents.executeJavaScript(page(pageCancelPick, null), true); }
  catch { /* page navigated away; nothing left to cancel */ }
}

function createPreviewControl({ activeThread }) {
  const current = (record, threadId) => {
    const preview = record.preview;
    return preview?.webContents && !preview.webContents.isDestroyed() && preview.scope === record.scope && !preview.scope.closed && preview.threadId === threadId ? preview : undefined;
  };
  const liveContents = preview => {
    const contents = preview.webContents;
    if (!contents || contents.isDestroyed()) throw new DesktopError('desktop.browser_preview_closed', '浏览器页面已关闭或替换，请重新打开页面后再操作。', { retryable: true });
    return contents;
  };
  const state = preview => {
    const contents = liveContents(preview);
    return { url: contents.getURL() || preview.url, title: contents.getTitle(), loading: contents.isLoading(),
      can_go_back: contents.navigationHistory.canGoBack(), can_go_forward: contents.navigationHistory.canGoForward() };
  };
  const settle = (preview, timeoutMs) => until(() => !liveContents(preview).isLoading(), timeoutMs);
  const run = async (preview, fn, arg) => {
    try { return await preview.webContents.executeJavaScript(page(fn, arg), true); }
    catch (error) { throw new DesktopError('desktop.browser_script_failed', `页面脚本执行失败：${error?.message || error}`); }
  };
  async function locate(record, threadId, timeoutMs, url) {
    const requested = url ? parseWebUrl(url)?.href : undefined;
    const preview = await until(() => {
      const found = current(record, threadId);
      return found && !found.hidden && (!requested || found.requestedUrl === requested) ? found : undefined;
    }, timeoutMs);
    if (!preview) throw new DesktopError('desktop.browser_preview_missing', '右侧浏览器当前不可见：面板可能已关闭、被对话框遮挡，或窗口尺寸刚改变。', { retryable: true });
    return preview;
  }
  async function click(preview, target, timeoutMs) {
    let point = target.point, found = {};
    if (!point) {
      found = await run(preview, pageTarget, { ref: target.ref, selector: target.selector, text: target.text });
      if (!found || found.missing) throw new DesktopError('desktop.browser_target_missing', '页面上找不到该元素；请重新读取 elements 获取最新 ref。');
      point = { x: found.x, y: found.y };
    }
    const bounds = preview.getBounds(), x = Math.round(point.x), y = Math.round(point.y);
    if (x < 0 || y < 0 || x >= bounds.width || y >= bounds.height) throw new DesktopError('desktop.browser_point_outside', `点击位置 (${x}, ${y}) 不在 ${bounds.width}x${bounds.height} 视口内。`);
    const contents = preview.webContents;
    contents.focus();
    contents.sendInputEvent({ type: 'mouseMove', x, y });
    contents.sendInputEvent({ type: 'mouseDown', x, y, button: 'left', clickCount: 1 });
    contents.sendInputEvent({ type: 'mouseUp', x, y, button: 'left', clickCount: 1 });
    await sleep(400); await settle(preview, timeoutMs);
    return { x, y, tag: found.tag, text: found.text, covered_by: found.covered_by };
  }

  return async function control(record, input) {
    if (!input || typeof input !== 'object' || typeof input.threadId !== 'string') throw new DesktopError('desktop.browser_invalid', '浏览器操作请求无效。');
    if (!record.scope || record.scope.closed || !activeThread(record, input.threadId)) throw new DesktopError('desktop.browser_thread_inactive', '此窗口当前没有显示该对话。', { retryable: true });
    const timeoutMs = Math.min(Math.max(Number(input.timeoutMs) || 0, 500), 30000), started = Date.now();
    const left = () => Math.max(200, timeoutMs - (Date.now() - started));
    const preview = await locate(record, input.threadId, input.action === 'wait' ? timeoutMs : Math.min(timeoutMs, 5000), input.action === 'wait' ? input.url : undefined);
    const contents = preview.webContents;
    switch (input.action) {
      case 'wait': {
        await settle(preview, left());
        if (!input.selector && !input.text) return state(preview);
        const want = { selector: input.selector || null, text: input.text || null };
        let met = false;
        while (!met && !contents.isDestroyed()) {
          const present = await run(preview, pagePresent, want).catch(() => false);
          met = input.gone ? !present : present;
          if (met || left() <= 200) break;
          await sleep(200);
        }
        if (!met) throw new DesktopError('desktop.browser_wait_timeout', `等待${input.gone ? '消失' : '出现'}超时：${input.selector || input.text}`, { retryable: true });
        return { ...state(preview), waited_for: input.selector || input.text, gone: Boolean(input.gone) };
      }
      case 'press': {
        const target = input.ref || input.selector ? await click(preview, { ref: input.ref, selector: input.selector }, Math.min(left(), 3000)) : undefined;
        const keyCode = KEY_CODES[input.key] || input.key;
        const modifiers = Array.isArray(input.modifiers) ? input.modifiers : [];
        const printable = input.key.length === 1 && !modifiers.some(item => item === 'control' || item === 'meta');
        contents.focus();
        for (let i = 0; i < (input.repeat || 1); i += 1) {
          contents.sendInputEvent({ type: 'keyDown', keyCode, modifiers });
          if (printable || keyCode === 'Enter') contents.sendInputEvent({ type: 'char', keyCode: keyCode === 'Enter' ? '\r' : input.key, modifiers });
          contents.sendInputEvent({ type: 'keyUp', keyCode, modifiers });
        }
        await sleep(250); await settle(preview, left());
        return { ...state(preview), pressed: input.key, modifiers, repeat: input.repeat || 1, focused: await run(preview, pageFocused, { caretToEnd: false }),
          target: target ? { tag: target.tag, text: target.text } : undefined };
      }
      case 'scroll': {
        let target;
        if (input.ref || input.selector) {
          target = await run(preview, pageTarget, { ref: input.ref, selector: input.selector });
          if (!target || target.missing) throw new DesktopError('desktop.browser_target_missing', '页面上找不到该元素；请重新读取 elements 获取最新 ref。');
        }
        await sleep(150);
        const position = await run(preview, pageScroll, { direction: input.direction, amount: input.amount, to: input.to });
        return { ...state(preview), ...position, target: target ? { tag: target.tag, text: target.text } : undefined };
      }
      case 'evaluate': {
        let result;
        try { result = await contents.executeJavaScript(evaluateScript(input.expression), true); }
        catch (error) { throw new DesktopError('desktop.browser_evaluate_failed', `表达式执行失败：${error?.message || error}`); }
        if (result.json.length > EVALUATE_MAX_CHARS) throw new DesktopError('desktop.browser_evaluate_too_large', `结果有 ${result.json.length} 个字符，超过 ${EVALUATE_MAX_CHARS}；请缩小表达式返回的数据。`);
        return { ...state(preview), result_type: result.type, result: JSON.parse(result.json) };
      }
      case 'logs': {
        const logs = preview.logs;
        if (!logs) throw new DesktopError('desktop.browser_logs_unavailable', '此预览没有记录日志，请重新打开网页。');
        let rows = logs[input.kind] || [];
        if (input.kind === 'console' && input.level) rows = rows.filter(row => row.level === input.level);
        if (input.kind === 'network' && input.failed_only) rows = rows.filter(row => row.failed);
        const offset = Math.max(0, Number(input.offset) || 0), limit = Math.max(1, Number(input.limit) || 100);
        const end = Math.min(rows.length, offset + limit);
        return { ...state(preview), kind: input.kind, total: rows.length, dropped: logs.dropped[input.kind] || 0, offset,
          next_offset: end < rows.length ? end : null, entries: rows.slice(offset, end) };
      }
      case 'navigate': {
        const url = parseWebUrl(input.url);
        if (!url) throw new DesktopError('desktop.browser_url_invalid', '只能打开 http:// 或 https:// 地址。');
        const load = contents.loadURL(url.href).then(() => null, error => error);
        const failure = await Promise.race([load, sleep(left()).then(() => null)]);
        // ERR_ABORTED: superseded by a redirect or client-side navigation, not a failed load.
        if (failure && failure.code !== 'ERR_ABORTED') throw new DesktopError('desktop.browser_load_failed', `网页加载失败：${failure.code || failure.message}`, { retryable: true });
        return state(preview);
      }
      case 'history': {
        const history = contents.navigationHistory;
        if (input.direction === 'back' && history.canGoBack()) history.goBack();
        else if (input.direction === 'forward' && history.canGoForward()) history.goForward();
        else if (input.direction === 'reload') contents.reload();
        else throw new DesktopError('desktop.browser_no_history', input.direction === 'back' ? '没有可后退的页面。' : '没有可前进的页面。');
        await sleep(300); await settle(preview, left());
        return state(preview);
      }
      case 'read': {
        await settle(preview, left());
        const result = await run(preview, pageRead, { mode: input.mode, selector: input.selector || null });
        if (!result || result.missing) throw new DesktopError('desktop.browser_target_missing', `找不到选择器 ${input.selector} 对应的元素。`);
        const offset = Math.max(0, Number(input.offset) || 0), limit = Math.max(1, Number(input.limit) || 1);
        const rows = input.mode === 'elements' ? result.elements : result.content;
        const total = rows.length, end = Math.min(total, offset + limit);
        return { ...state(preview), mode: input.mode, offset, [input.mode === 'elements' ? 'total_elements' : 'total_chars']: total,
          next_offset: end < total ? end : null, [input.mode === 'elements' ? 'elements' : 'content']: rows.slice(offset, end) };
      }
      case 'click': {
        const clicked = await click(preview, input, left());
        return { ...state(preview), clicked };
      }
      case 'type': {
        const target = input.ref || input.selector ? await click(preview, { ref: input.ref, selector: input.selector }, Math.min(left(), 3000)) : undefined;
        const focused = await run(preview, pageFocused, { caretToEnd: !input.clear });
        if (!focused?.editable) throw new DesktopError('desktop.browser_not_editable', '当前焦点不是可输入的元素；请提供 ref 或 selector。');
        contents.focus();
        if (input.clear) { contents.selectAll(); if (!input.text) contents.delete(); }
        if (input.text) await contents.insertText(input.text);
        if (input.submit) {
          contents.sendInputEvent({ type: 'keyDown', keyCode: 'Enter' });
          contents.sendInputEvent({ type: 'char', keyCode: '\r' });
          contents.sendInputEvent({ type: 'keyUp', keyCode: 'Enter' });
        }
        await sleep(300); await settle(preview, left());
        const after = input.submit ? null : await run(preview, pageFocused, { caretToEnd: false });
        return { ...state(preview), typed_chars: input.text.length, cleared: Boolean(input.clear), submitted: Boolean(input.submit),
          target: target ? { tag: target.tag, text: target.text } : undefined, focused: after || focused };
      }
      case 'screenshot': {
        await settle(preview, left());
        const image = await contents.capturePage();
        if (image.isEmpty()) throw new DesktopError('desktop.browser_capture_empty', '截图为空：右侧浏览器可能尚未绘制。', { retryable: true });
        const bounds = preview.getBounds();
        // CSS-pixel size keeps screenshot coordinates identical to click x/y.
        const scaled = image.getSize().width === bounds.width ? image : image.resize({ width: bounds.width, height: bounds.height, quality: 'best' });
        return { ...state(preview), width: bounds.width, height: bounds.height, data: scaled.toPNG().toString('base64') };
      }
      default:
        throw new DesktopError('desktop.browser_action_invalid', '此浏览器操作不可用。');
    }
  };
}

module.exports = { createPreviewControl, pickElement, cancelPick, pageRead, pageTarget };
