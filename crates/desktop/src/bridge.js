(() => {
  "use strict";
  if (location.protocol !== "http:" || window.top !== window || window.mutekiDesktop) return;
  const handler = window.webkit && window.webkit.messageHandlers && window.webkit.messageHandlers.nsh;
  if (!handler) return;
  const KEY = "__MUTEKI_BRIDGE_KEY__";
  const RAW_WINDOW_ID = "__MUTEKI_WINDOW_ID__";
  const WINDOW_ID = RAW_WINDOW_ID.startsWith("__MUTEKI_") ? "main" : RAW_WINDOW_ID;
  const createDocumentId = () =>
    typeof crypto !== "undefined" && typeof crypto.randomUUID === "function"
      ? crypto.randomUUID()
      : `${Date.now()}-${Math.random().toString(36).slice(2)}`;
  let documentId = createDocumentId();
  let scopeVersion = 1;
  let connectionVersion = 1;
  const post = (message) =>
    handler.postMessage(
      JSON.stringify({
        ...message,
        k: KEY,
        windowId: WINDOW_ID,
        documentId,
        scopeVersion,
        connectionVersion,
      })
    );
  const pending = new Map();
  const listeners = new Map();
  const flushHandlers = new Set();
  let draftState = { ready: false, dirtyCount: 0 };
  let seq = 0;
  let documentReady = null;
  let historyConnection = null;
  const desktopError = (code, message, retryable = false) => ({ code, message, retryable, source: "desktop", __mutekiDesktopError: true });
  const sendCall = (method, params) => new Promise((resolve, reject) => {
    const id = ++seq;
    pending.set(id, { resolve, reject, method, documentId, scopeVersion });
    try { post({ t: "call", id, m: method, p: params === undefined ? null : params }); }
    catch (error) { pending.delete(id); reject(desktopError("desktop.bridge_unavailable", String(error && error.message || error))); }
  });
  const call = (method, params) => documentReady && method !== "ui.ready"
    ? documentReady.then(() => sendCall(method, params))
    : sendCall(method, params);
  const emit = (name, value) => {
    for (const callback of Array.from(listeners.get(name) || [])) {
      try { callback(value); } catch (error) { console.error("muteki.desktop.listener_failed", name, error); }
    }
  };
  const subscribe = (name) => (callback) => {
    if (typeof callback !== "function") throw new TypeError("callback must be a function");
    if (!listeners.has(name)) listeners.set(name, new Set());
    listeners.get(name).add(callback);
    return () => { listeners.get(name).delete(callback); };
  };

  let persistenceLock = null;
  const handleBeforeCloseEvent = async (payload) => {
    const token = payload && payload.token;
    const reason = (payload && payload.reason) || "close";
    if (reason === "persistence-barrier" && document.documentElement) { persistenceLock = { token, inert: document.documentElement.inert }; document.documentElement.inert = true; }
    const closeListeners = listeners.get("before-close");
    if (closeListeners && closeListeners.size > 0) {
      emit("before-close", payload);
      return;
    }
    if (flushHandlers.size > 0) {
      try {
        for (const fn of Array.from(flushHandlers)) {
          await fn(payload);
        }
        await call("close.ack", { token, persisted: true, status: "flushed", reason });
      } catch (error) {
        await call("close.ack", {
          token,
          persisted: false,
          status: "failed",
          error: String((error && error.message) || error || "草稿保存失败。"),
          reason,
        }).catch(() => {});
      }
      return;
    }
    if (draftState.ready && draftState.dirtyCount === 0) {
      await call("close.ack", { token, persisted: true, status: "no-dirty-drafts", reason }).catch(() => {});
      return;
    }
    for (let i = 0; i < 12; i++) {
      await new Promise((r) => setTimeout(r, 50));
      const nowListeners = listeners.get("before-close");
      if (nowListeners && nowListeners.size > 0) {
        emit("before-close", payload);
        return;
      }
      if (draftState.ready && draftState.dirtyCount === 0) {
        await call("close.ack", { token, persisted: true, status: "no-dirty-drafts", reason }).catch(() => {});
        return;
      }
    }
    const status = draftState.ready ? "dirty-unhandled" : "page-not-ready";
    const error = draftState.ready
      ? "存在未保存的草稿，且页面未完成草稿刷新。"
      : "页面尚未准备就绪，无法确认草稿是否已保存。";
    await call("close.ack", { token, persisted: false, status, error, reason }).catch(() => {});
  };

  const receive = (message) => {
    if (!message || typeof message !== "object") return;
    if (message.windowId && message.windowId !== WINDOW_ID) return;
    if (message.documentId && message.documentId !== documentId) return;
    if (typeof message.scopeVersion === "number" && message.scopeVersion > scopeVersion) {
      scopeVersion = message.scopeVersion;
    }
    if (typeof message.connectionVersion === "number" && message.connectionVersion > connectionVersion) {
      connectionVersion = message.connectionVersion;
    }
    if (message.t === "reply") {
      const entry = pending.get(message.id);
      if (!entry) return;
      pending.delete(message.id);
      if (entry.documentId !== documentId) {
        entry.reject(desktopError("desktop.document_stale", "页面已切换或刷新，操作结果已过期。", true));
        return;
      }
      if (message.ok) entry.resolve(message.v === undefined ? null : message.v);
      else entry.reject({ ...(message.e || {}), __mutekiDesktopError: true, message: (message.e && message.e.message) || "桌面操作失败。" });
    } else if (message.t === "event") {
      if (message.n === "state" && message.v && typeof message.v === "object") {
        if (typeof message.v.scopeVersion === "number" && message.v.scopeVersion > 0) {
          scopeVersion = message.v.scopeVersion;
        }
        if (typeof message.v.connectionVersion === "number" && message.v.connectionVersion > 0) {
          connectionVersion = message.v.connectionVersion;
        }
      }
      if (message.n === "state" && message.v?.transportOrigin && transport?.origin !== message.v.transportOrigin) {
        refreshTransport().then(async () => { token = await call("auth.token"); emit("state", message.v); }).catch(error => console.error("desktop.connection_refresh_failed", error));
        return;
      }
      if (message.n === "persistence-released") {
        if (persistenceLock?.token === message.v?.token && document.documentElement) { document.documentElement.inert = persistenceLock.inert; persistenceLock = null; }
        return;
      }
      if (message.n === "before-close") {
        void handleBeforeCloseEvent(message.v);
        return;
      }
      if (message.n === "navigate" && message.v && typeof message.v.route === "string") {
        const payload = { ...message.v, handled: false };
        let handled = false;
        for (const callback of Array.from(listeners.get("navigate") || [])) {
          try {
            if (callback(payload) === true || payload.handled) handled = true;
          } catch (error) {
            console.error("muteki.desktop.listener_failed", "navigate", error);
          }
        }
        const targetRoute = message.v.route;
        const currentRoute = location.pathname + location.search + location.hash;
        if (!handled && targetRoute && targetRoute !== currentRoute) {
          try {
            location.assign(targetRoute);
          } catch {
            console.error("muteki.desktop.navigation_failed", "location.assign");
          }
        }
        return;
      }
      emit(message.n, message.v);
    }
  };
  Object.defineProperty(window, "__mutekiDesktopReceive", { value: receive, writable: false, configurable: false, enumerable: false });
  const registerDocument = () => sendCall("ui.ready");
  documentReady = registerDocument();
  void documentReady.catch(() => {});

  addEventListener("pagehide", (event) => {
    for (const [id, entry] of pending.entries()) {
      pending.delete(id);
      entry.reject(desktopError("desktop.document_unloaded", "页面已卸载或刷新。", true));
    }
    if (event.isTrusted && event.persisted) {
      let resume;
      const ready = new Promise(resolve => { resume = resolve; });
      historyConnection = { ready, resume };
    }
  });

  const nativeFetch = window.fetch.bind(window);
  let token = null;
  let transport = null;
  let transportReady = call("connection.transport").then(value => { transport = value; return value; });
  const refreshTransport = () => { transportReady = call("connection.transport").then(value => { transport = value; return value; }); return transportReady; };
  const connectedTransport = () => historyConnection ? historyConnection.ready.then(() => transportReady) : transportReady;
  const requestToken = () => call("auth.token").then(value => { token = typeof value === "string" && value ? value : null; });
  let tokenReady = requestToken();
  const transportUrl = (value, socket = false) => {
    if (!transport) throw desktopError("desktop.connection_unavailable", "工作台传输尚未就绪。", true);
    const original = new URL(String(value), location.href);
    const requestOrigin = new URL(original.href);
    if (requestOrigin.protocol === "ws:") requestOrigin.protocol = "http:";
    if (requestOrigin.protocol === "wss:") requestOrigin.protocol = "https:";
    if (!original.pathname.startsWith("/api/") || (requestOrigin.origin !== location.origin && requestOrigin.origin !== transport.origin)) return original.href;
    const target = new URL(original.pathname + original.search, transport.origin);
    if (socket) { target.protocol = target.protocol === "https:" ? "wss:" : "ws:"; }
    target.searchParams.set("__muteki_desktop_proxy", transport.key);
    return target.href;
  };
  window.fetch = async function mutekiFetch(input, init) {
    let url;
    try { url = new URL(input instanceof Request ? input.url : String(input), location.href); } catch { return nativeFetch(input, init); }
    if (!url.pathname.startsWith("/api/") || (url.origin !== location.origin && url.origin !== transport?.origin)) return nativeFetch(input, init);
    let request;
    try { request = new Request(input, init); } catch { return nativeFetch(input, init); }
    if (historyConnection) await historyConnection.ready;
    await Promise.all([tokenReady, transportReady]);
    const selected = transport, requestVersion = connectionVersion;
    const headers = new Headers(request.headers);
    headers.set("x-muteki-desktop-proxy", selected.key);
    headers.delete("Cookie");
    if (token) headers.set("Authorization", `Bearer ${token}`);
    const method = request.method.toUpperCase();
    request = new Request(new URL(url.pathname + url.search, selected.origin), {
      method, headers, credentials: "omit", signal: request.signal,
      body: method === "GET" || method === "HEAD" || request.body === null ? undefined : await request.blob(),
      mode: request.mode, cache: request.cache, redirect: request.redirect,
      referrer: request.referrer, referrerPolicy: request.referrerPolicy,
      integrity: request.integrity, keepalive: request.keepalive,
    });
    const response = await nativeFetch(request);
    if (historyConnection) await historyConnection.ready;
    if (requestVersion !== connectionVersion || selected.origin !== transport?.origin || selected.key !== transport?.key) throw desktopError("desktop.connection_changed", "工作台已切换，请在当前工作台重试。", true);
    if (url.pathname === "/api/auth/login" && method === "POST" && response.ok) {
      const data = await response.clone().json().catch(() => null);
      if (data && typeof data.token === "string") {
        const next = data.token || null;
        await call("auth.set", { token: next, serviceId: data.service_id, identityId: data.identity_id }); token = next;
        const { token: _secret, ...visible } = data;
        const headers = new Headers(response.headers); headers.delete("content-length");
        return new Response(JSON.stringify(visible), { status: response.status, statusText: response.statusText, headers });
      }
    }
    if (url.pathname === "/api/auth/logout" && method === "POST" && (response.ok || response.status === 401)) { token = null; await call("auth.set", { token: null }); }
    return response;
  };
  const NativeEventSource = window.EventSource, NativeWebSocket = window.WebSocket;
  const deferredConnection = (Original, socket) => {
    class Connection extends EventTarget {
      constructor(url, options) {
        super(); this._url = String(url); this._native = null; this._closed = false; this._binaryType = "blob"; this._withCredentials = Boolean(options?.withCredentials);
        this.onopen = this.onmessage = this.onerror = this.onclose = null; this._relayed = new Set(["open", "message", "error", ...(socket ? ["close"] : [])]);
        connectedTransport().then(() => {
          if (this._closed) return;
          const destination = transportUrl(url, socket);
          const proxied = new URL(destination, location.href).searchParams.has("__muteki_desktop_proxy");
          if (!socket && proxied) this._withCredentials = false;
          const native = this._native = new Original(destination, socket || !proxied ? options : { ...options, withCredentials: false });
          if (socket) native.binaryType = this._binaryType;
          for (const name of ["open", "message", "error", ...(socket ? ["close"] : [])]) native.addEventListener(name, event => {
            const copied = name === "message" ? new MessageEvent(name, { data: event.data, origin: event.origin, lastEventId: event.lastEventId })
              : name === "close" ? new CloseEvent(name, { code: event.code, reason: event.reason, wasClean: event.wasClean }) : new Event(name);
            this.dispatchEvent(copied); if (typeof this[`on${name}`] === "function") this[`on${name}`].call(this, copied);
          });
        }).catch(error => { this._closed = true; const event = new Event("error"); this.dispatchEvent(event); this.onerror?.call(this, event); console.error("desktop.transport_failed", error); });
      }
      get url() { return this._native?.url || this._url; }
      get readyState() { return this._native?.readyState ?? (this._closed ? (socket ? 3 : 2) : 0); }
      get bufferedAmount() { return this._native?.bufferedAmount || 0; }
      get protocol() { return this._native?.protocol || ""; }
      get extensions() { return this._native?.extensions || ""; }
      get withCredentials() { return this._native?.withCredentials ?? this._withCredentials; }
      get binaryType() { return this._native?.binaryType || this._binaryType; }
      set binaryType(value) { this._binaryType = value; if (this._native) this._native.binaryType = value; }
      send(data) { if (!this._native) throw new DOMException("WebSocket is still connecting", "InvalidStateError"); return this._native.send(data); }
      close(...args) { this._closed = true; return this._native?.close(...args); }
      addEventListener(name, listener, options) {
        super.addEventListener(name, listener, options);
        if (!socket && !this._relayed.has(name)) { this._relayed.add(name); connectedTransport().then(() => {
          this._native?.addEventListener(name, event => this.dispatchEvent(new MessageEvent(name, { data: event.data, origin: event.origin, lastEventId: event.lastEventId })));
        }); }
      }
    }
    for (const name of socket ? ["CONNECTING", "OPEN", "CLOSING", "CLOSED"] : ["CONNECTING", "OPEN", "CLOSED"]) { Object.defineProperty(Connection, name, { value: Original[name] }); Object.defineProperty(Connection.prototype, name, { value: Original[name] }); }
    return Connection;
  };
  window.EventSource = deferredConnection(NativeEventSource, false);
  window.WebSocket = deferredConnection(NativeWebSocket, true);

  const nativeOpen = window.open.bind(window);
  window.open = function mutekiOpen(url, target, features) {
    let parsed;
    try { parsed = new URL(url === undefined || url === null || url === "" ? "about:blank" : String(url), location.href); } catch { return null; }
    if (parsed.protocol === "http:" || parsed.protocol === "https:") {
      void call("openExternal", { url: parsed.href }).catch((error) => console.error("muteki.desktop.open_external_failed", error));
      return null;
    }
    return nativeOpen(url, target, features);
  };

  let lastView = "";
  let explicitVisibleThreads = [];
  const routeThreadFromLocation = () => {
    if (location.pathname.startsWith("/chat/")) {
      try {
        return decodeURIComponent(location.pathname.split("/")[2] || "");
      } catch {
        return "";
      }
    }
    if (location.pathname === "/src" || location.pathname.startsWith("/src/")) {
      try {
        const params = new URLSearchParams(location.search);
        return params.get("thread") || params.get("threadId") || params.get("id") || "";
      } catch {
        return "";
      }
    }
    return "";
  };
  const collectVisibleThreads = () => {
    const out = [];
    const routeThread = routeThreadFromLocation();
    if (routeThread) out.push(routeThread);
    const pageList = Array.isArray(window.__mutekiVisibleThreadIds)
      ? window.__mutekiVisibleThreadIds
      : [];
    for (const item of [...explicitVisibleThreads, ...pageList]) {
      if (typeof item === "string" && item.trim() && !out.includes(item.trim())) {
        out.push(item.trim());
      }
    }
    return out;
  };
  const reportView = () => {
    const visibleThreadIds = collectVisibleThreads();
    const view = {
      route: location.pathname + location.search + location.hash,
      visible: document.visibilityState === "visible",
      focused: document.hasFocus(),
      visibleThreadIds,
      windowId: WINDOW_ID,
      documentId,
    };
    const key = JSON.stringify(view);
    if (key === lastView) return;
    lastView = key;
    void call("ui.view", view).catch(() => {});
  };
  const scheduleView = () => queueMicrotask(reportView);
  for (const name of ["pushState", "replaceState"]) {
    const original = history[name];
    history[name] = function (...args) { const result = original.apply(this, args); scheduleView(); return result; };
  }
  addEventListener("popstate", scheduleView);
  addEventListener("hashchange", scheduleView);
  addEventListener("focus", scheduleView);
  addEventListener("blur", scheduleView);
  document.addEventListener("visibilitychange", scheduleView);
  addEventListener("pageshow", (event) => {
    if (event.isTrusted && event.persisted) {
      const suspended = historyConnection;
      documentReady = registerDocument().catch(error => {
        if (error.code !== "desktop.document_stale") throw error;
        documentId = createDocumentId();
        return registerDocument();
      });
      tokenReady = requestToken();
      void Promise.all([refreshTransport(), tokenReady]).finally(() => {
        if (historyConnection === suspended) historyConnection = null;
        suspended?.resume();
      }).then(async () => {
        const state = await call("state");
        receive({ t: "event", n: "state", v: state });
        lastView = "";
        reportView();
      }).catch(error => console.error("desktop.document_restore_failed", error));
      return;
    }
    scheduleView();
  });

  const activeThread = () => {
    return routeThreadFromLocation() || explicitVisibleThreads[0] || "";
  };

  const CDN = "https://cdnjs.cloudflare.com https://esm.sh https://cdn.jsdelivr.net https://unpkg.com";
  const VISUALIZATION_CSP = `default-src 'none'; script-src 'unsafe-inline' ${CDN}; style-src 'unsafe-inline' ${CDN} https://fonts.googleapis.com https://fonts.bunny.net; font-src https://fonts.gstatic.com https://fonts.bunny.net; img-src data: blob: ${CDN}; connect-src 'none'; frame-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'`;
  const visualizations = new Map();
  const withPolicy = (html) => {
    const meta = `<meta http-equiv="Content-Security-Policy" content="${VISUALIZATION_CSP}">`;
    const doctype = /^\s*<!doctype[^>]*>/i.exec(html);
    return doctype ? doctype[0] + meta + html.slice(doctype[0].length) : meta + html;
  };

  const terminals = new Map();
  const terminalOpen = async (input) => {
    const threadId = input && input.threadId, path = input && input.path;
    await call("terminal.authorize", { threadId, path });
    const id = crypto.randomUUID();
    const socket = new WebSocket(`ws://${location.host}${path}`);
    socket.binaryType = "arraybuffer";
    const decoder = new TextDecoder();
    terminals.set(id, socket);
    socket.onopen = () => emit("terminal", { id, type: "open" });
    socket.onmessage = (event) => emit("terminal", { id, type: "message", data: typeof event.data === "string" ? event.data : decoder.decode(event.data, { stream: true }) });
    socket.onerror = () => emit("terminal", { id, type: "error", message: "desktop.terminal.transport_error" });
    socket.onclose = (event) => { terminals.delete(id); emit("terminal", { id, type: "close", code: event.code, message: event.reason || "" }); };
    return { id };
  };
  const terminalSocket = (id) => {
    const socket = terminals.get(id);
    if (!socket) throw desktopError("desktop.terminal_closed", "终端连接已关闭。");
    return socket;
  };

  const toBase64 = (buffer) => {
    const bytes = new Uint8Array(buffer);
    let binary = "";
    for (let i = 0; i < bytes.length; i += 0x8000) binary += String.fromCharCode.apply(null, bytes.subarray(i, i + 0x8000));
    return btoa(binary);
  };
  const fromBase64 = (text) => {
    const binary = atob(text);
    const bytes = new Uint8Array(binary.length);
    for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
    return bytes.buffer;
  };

  /*__MUTEKI_BROWSER_PAGE__*/
  const page = (fn, arg) => `(${fn.toString()})(${JSON.stringify(arg)})`;
  const evaluateScript = (expression) => `(async () => {
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
  const EVALUATE_MAX_CHARS = 200000;
  const PICK_HTML_LIMIT = 6000;
  const PICK_PADDING = 6;
  const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  const run = async (id, fn, arg) => {
    try { return await call("preview.exec", { id, script: page(fn, arg) }); }
    catch (error) {
      if (error && typeof error.code === "string" && error.code !== "desktop.preview_script_failed") throw error;
      throw desktopError("desktop.browser_script_failed", `页面脚本执行失败：${error && error.message || error}`);
    }
  };
  const previewState = (input) => call("preview.state", input);
  const settle = async (id, timeoutMs) => {
    const end = Date.now() + timeoutMs;
    for (;;) {
      const state = await previewState({ id });
      if (!state.loading || Date.now() >= end) return state;
      await sleep(100);
    }
  };
  const visibleState = (state) => ({ url: state.url, title: state.title, loading: state.loading, can_go_back: state.can_go_back, can_go_forward: state.can_go_forward });
  const webHref = (value) => { try { const url = new URL(value); return (url.protocol === "http:" || url.protocol === "https:") && !url.username && !url.password ? url.href : null; } catch { return null; } };
  async function locate(threadId, timeoutMs, url) {
    const requested = url ? webHref(url) : undefined;
    const end = Date.now() + timeoutMs;
    for (;;) {
      const state = await previewState({ threadId }).catch((error) => { if (error && error.code === "desktop.browser_thread_inactive") throw error; return null; });
      if (state && (!state.hidden || state.error) && (!requested || state.requestedUrl === requested)) return state;
      if (Date.now() >= end) throw desktopError("desktop.browser_preview_missing", "右侧浏览器当前不可见：面板可能已关闭、被对话框遮挡，或窗口尺寸刚改变。", true);
      await sleep(100);
    }
  }
  async function click(preview, target, timeoutMs) {
    let point = target.point, found = {};
    if (!point) {
      found = await run(preview.id, pageTarget, { ref: target.ref, selector: target.selector, text: target.text, interactive: INTERACTIVE });
      if (!found || found.missing) throw desktopError("desktop.browser_target_missing", "页面上找不到该元素；请重新读取 elements 获取最新 ref。");
      point = { x: found.x, y: found.y };
    }
    const x = Math.round(point.x), y = Math.round(point.y);
    if (x < 0 || y < 0 || x >= preview.width || y >= preview.height) throw desktopError("desktop.browser_point_outside", `点击位置 (${x}, ${y}) 不在 ${preview.width}x${preview.height} 视口内。`);
    await call("preview.input", { id: preview.id, kind: "click", x, y });
    await sleep(400); await settle(preview.id, timeoutMs);
    return { x, y, tag: found.tag, text: found.text, covered_by: found.covered_by };
  }
  async function press(preview, key, modifiers, repeat) {
    await call("preview.input", { id: preview.id, kind: "key", key, modifiers, repeat });
  }
  async function readPreviewLogs(id, input) {
    const kind = (input && input.kind) || "console";
    if (kind !== "console" && kind !== "network") {
      throw desktopError("desktop.browser_logs_invalid", "日志类型须为 console 或 network。");
    }
    const logs = await run(id, kind === "network" ? pageNetwork : pageConsole, null);
    if (!logs) throw desktopError("desktop.browser_logs_unavailable", "此预览没有记录日志，请重新打开网页。");
    if (logs.error) throw desktopError("desktop.browser_logs_capture_limit", logs.error.message);
    let rows = logs.rows;
    if (kind === "console" && input.level) {
      rows = rows.filter((row) => row.level === input.level);
    }
    if (kind === "network") {
      if (input.failed_only) rows = rows.filter((row) => !row.ok);
      if (input.method) {
        const wantMethod = String(input.method).toUpperCase();
        rows = rows.filter((row) => row.method === wantMethod);
      }
      if (input.status !== undefined && input.status !== null) {
        const wantStatus = Number(input.status);
        rows = rows.filter((row) => row.status === wantStatus);
      }
      if (input.initiatorType) {
        rows = rows.filter((row) => row.initiatorType === input.initiatorType);
      }
    }
    const offset = Math.max(0, Number(input.offset) || 0);
    const limit = Math.max(1, Number(input.limit) || 100);
    const end = Math.min(rows.length, offset + limit);
    return {
      ...visibleState(await previewState({ id })),
      kind,
      total: rows.length,
      dropped: logs.dropped || 0,
      offset,
      next_offset: end < rows.length ? end : null,
      entries: rows.slice(offset, end),
    };
  }
  async function browserControl(input) {
    if (!input || typeof input !== "object" || typeof input.threadId !== "string") throw desktopError("desktop.browser_invalid", "浏览器操作请求无效。");
    const timeoutMs = Math.min(Math.max(Number(input.timeoutMs) || 0, input.action === "type" ? 0 : 500), 30000), started = Date.now();
    const left = () => Math.max(200, timeoutMs - (Date.now() - started));
    const preview = await locate(input.threadId, input.action === "wait" ? timeoutMs : Math.min(timeoutMs, 5000), input.action === "wait" ? input.url : undefined);
    const id = preview.id;
    switch (input.action) {
      case "wait": {
        const settled = await settle(id, left());
        if (!input.selector && !input.text) return visibleState(settled);
        const want = { selector: input.selector || null, text: input.text || null };
        let met = false;
        for (;;) {
          const present = await run(id, pagePresent, want).catch(() => false);
          met = input.gone ? !present : present;
          if (met || left() <= 200) break;
          await sleep(200);
        }
        if (!met) throw desktopError("desktop.browser_wait_timeout", `等待${input.gone ? "消失" : "出现"}超时：${input.selector || input.text}`, true);
        return { ...visibleState(await previewState({ id })), waited_for: input.selector || input.text, gone: Boolean(input.gone) };
      }
      case "press": {
        const target = input.ref || input.selector ? await click(preview, { ref: input.ref, selector: input.selector }, Math.min(left(), 3000)) : undefined;
        const modifiers = Array.isArray(input.modifiers) ? input.modifiers.map(String) : [];
        await press(preview, String(input.key || ""), modifiers, Math.max(1, Math.min(50, Number(input.repeat) || 1)));
        await sleep(250);
        const state = await settle(id, left());
        return { ...visibleState(state), pressed: input.key, modifiers, repeat: input.repeat || 1, focused: await run(id, pageFocused, { caretToEnd: false }),
          target: target ? { tag: target.tag, text: target.text } : undefined };
      }
      case "scroll": {
        let target;
        if (input.ref || input.selector) {
          target = await run(id, pageTarget, { ref: input.ref, selector: input.selector, interactive: INTERACTIVE });
          if (!target || target.missing) throw desktopError("desktop.browser_target_missing", "页面上找不到该元素；请重新读取 elements 获取最新 ref。");
        }
        await sleep(150);
        const position = await run(id, pageScroll, { direction: input.direction, amount: input.amount, to: input.to });
        return { ...visibleState(await previewState({ id })), ...position, target: target ? { tag: target.tag, text: target.text } : undefined };
      }
      case "evaluate": {
        let result;
        try { result = await call("preview.exec", { id, script: evaluateScript(String(input.expression || "")) }); }
        catch (error) { throw desktopError("desktop.browser_evaluate_failed", `表达式执行失败：${error && error.message || error}`); }
        if (!result || typeof result.json !== "string") throw desktopError("desktop.browser_evaluate_failed", "表达式没有返回可读取的结果。");
        if (result.json.length > EVALUATE_MAX_CHARS) throw desktopError("desktop.browser_evaluate_too_large", `结果有 ${result.json.length} 个字符，超过 ${EVALUATE_MAX_CHARS}；请缩小表达式返回的数据。`);
        return { ...visibleState(await previewState({ id })), result_type: result.type, result: JSON.parse(result.json) };
      }
      case "logs": {
        return readPreviewLogs(id, input);
      }
      case "navigate": {
        const url = webHref(input.url);
        if (!url) throw desktopError("desktop.browser_url_invalid", "只能打开 http:// 或 https:// 地址。");
        await call("preview.navigate", { id, url });
        await sleep(200);
        const state = await settle(id, left());
        if (state.error) throw desktopError("desktop.browser_load_failed", `网页加载失败：${state.error}`, true);
        return visibleState(state);
      }
      case "history": {
        if (input.direction === "back" && !preview.can_go_back) throw desktopError("desktop.browser_no_history", "没有可后退的页面。");
        if (input.direction === "forward" && !preview.can_go_forward) throw desktopError("desktop.browser_no_history", "没有可前进的页面。");
        if (!["back", "forward", "reload"].includes(input.direction)) throw desktopError("desktop.browser_no_history", "没有可前进的页面。");
        await call("preview.action", { id, action: input.direction });
        await sleep(300);
        const state = await settle(id, left());
        if (state.error) throw desktopError("desktop.browser_load_failed", `网页加载失败：${state.error}`, true);
        return visibleState(state);
      }
      case "read": {
        await settle(id, left());
        const result = await run(id, pageRead, { mode: input.mode, selector: input.selector || null, interactive: INTERACTIVE });
        if (!result || result.missing) throw desktopError("desktop.browser_target_missing", `找不到选择器 ${input.selector} 对应的元素。`);
        const offset = Math.max(0, Number(input.offset) || 0), limit = Math.max(1, Number(input.limit) || 1);
        const rows = input.mode === "elements" ? result.elements : result.content;
        const total = rows.length, end = Math.min(total, offset + limit);
        return { ...visibleState(await previewState({ id })), mode: input.mode, offset, [input.mode === "elements" ? "total_elements" : "total_chars"]: total,
          next_offset: end < total ? end : null, [input.mode === "elements" ? "elements" : "content"]: rows.slice(offset, end) };
      }
      case "click": {
        const clicked = await click(preview, input, left());
        return { ...visibleState(await previewState({ id })), clicked };
      }
      case "type": {
        const target = input.ref || input.selector ? await click(preview, { ref: input.ref, selector: input.selector }, Math.min(left(), 3000)) : undefined;
        const focused = await run(id, pageFocused, { caretToEnd: !input.clear });
        if (!focused || !focused.editable) throw desktopError("desktop.browser_not_editable", "当前焦点不是可输入的元素；请提供 ref 或 selector。");
        const text = String(input.text || "");
        if (Date.now() >= started + timeoutMs) throw desktopError("desktop.browser_input_timeout", "输入期限已到，未输入文字。");
        const inserted = await run(id, pageInsert, { clear: Boolean(input.clear), text, submit: input.submit });
        if (!inserted) throw desktopError("desktop.browser_type_failed", "页面拒绝了文字输入。");
        if (input.submit && input.submitWhen) {
          const deadline = started + timeoutMs;
          while (Date.now() < deadline && !await run(id, pagePresent, { selector: input.submitWhen })) {
            await sleep(Math.min(50, Math.max(0, deadline - Date.now())));
          }
        }
        const typed = await run(id, pageFocused, { caretToEnd: false, expectedRef: input.submit ? inserted : null });
        if (input.submit) {
          if (!typed) throw desktopError("desktop.browser_submit_focus_changed", "输入框焦点已改变，未发送 Enter。");
          if (Date.now() >= started + timeoutMs) throw desktopError("desktop.browser_submit_timeout", "文字已输入，提交期限已到，未发送 Enter。");
          await press(preview, "Enter", [], 1);
        }
        const state = input.submit ? await settle(id, left()) : await previewState({ id });
        return { ...visibleState(state), typed_chars: text.length, cleared: Boolean(input.clear), submitted: Boolean(input.submit),
          target: target ? { tag: target.tag, text: target.text } : undefined, focused: typed || focused };
      }
      case "screenshot": {
        const state = await settle(id, left());
        const shot = await call("preview.action", { id, action: "snapshot" });
        return { ...visibleState(state), width: shot.width, height: shot.height, data: shot.data };
      }
      case "resize": {
        const wantWidth = Math.round(Number(input.width) || 0);
        const wantHeight = Math.round(Number(input.height) || 0);
        const deadline = Date.now() + Math.min(left(), 4000);
        let state = preview;
        for (;;) {
          const vw = Math.round(state.viewportWidth || state.width || 0);
          const vh = Math.round(state.viewportHeight || state.height || 0);
          if (vw === wantWidth && vh === wantHeight) break;
          if (Date.now() >= deadline) throw desktopError("desktop.browser_resize_timeout", "右侧浏览器视口调整超时。", true);
          await sleep(50);
          state = await previewState({ id });
        }
        await run(id, () => new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(() => resolve({ width: innerWidth, height: innerHeight })))), null);
        state = await previewState({ id });
        return {
          ...visibleState(state),
          width: Math.round(state.width),
          height: Math.round(state.height),
          viewport: {
            width: Math.round(state.viewportWidth || state.width),
            height: Math.round(state.viewportHeight || state.height),
          },
        };
      }
      default:
        throw desktopError("desktop.browser_action_invalid", "此浏览器操作不可用。");
    }
  }
  async function previewAction(input) {
    const id = input && input.id;
    if (input && input.action === "pick") {
      const picked = await run(id, pagePickElement, { htmlLimit: PICK_HTML_LIMIT });
      if (!picked) return null;
      const state = await previewState({ id });
      const x = Math.max(0, Math.floor(picked.rect.x - PICK_PADDING)), y = Math.max(0, Math.floor(picked.rect.y - PICK_PADDING));
      const right = Math.min(state.width, Math.ceil(picked.rect.x + picked.rect.width + PICK_PADDING));
      const bottom = Math.min(state.height, Math.ceil(picked.rect.y + picked.rect.height + PICK_PADDING));
      let screenshot = null, screenshotError = null;
      if (right - x >= 2 && bottom - y >= 2) {
        try {
          const shot = await call("preview.action", { id, action: "snapshot", rect: { x, y, width: right - x, height: bottom - y } });
          screenshot = { mime: "image/png", data: shot.data, width: shot.width, height: shot.height };
        } catch (error) { screenshotError = `截图失败：${error && error.message || error}`; }
      }
      return { ...picked, screenshot, screenshot_error: screenshotError,
        screenshot_clipped: picked.rect.x < 0 || picked.rect.y < 0 || picked.rect.x + picked.rect.width > state.width || picked.rect.y + picked.rect.height > state.height };
    }
    if (input && input.action === "pick-cancel") {
      try { await run(id, pageCancelPick, null); } catch {   }
      return null;
    }
    if (input && input.action === "screenshot") {
      const shot = await call("preview.action", { id, action: "snapshot" });
      const state = await previewState({ id });
      return { mime: "image/png", data: shot.data, width: shot.width, height: shot.height, url: state.url, title: state.title };
    }
    if (input && input.action === "logs") {
      return readPreviewLogs(id, input);
    }
    return call("preview.action", input);
  }

  const previewCapabilities = Object.freeze({
    navigation: true,
    screenshot: true,
    consoleLogs: true,
    networkLogs: true,
    devtools: true,
    clearWebsiteData: true,
    clearHttpAuth: true,
  });

  const bridge = {
    windowId: WINDOW_ID,
    get documentId() { return documentId; },
    getScopeVersion: () => scopeVersion,
    getConnectionVersion: () => connectionVersion,
    previewCapabilities,
    getState: () => call("state"),
    setWindowChrome: (input) => call("window.chrome", input),
    windowAction: (action) => call("window.action", { action }),
    connect: async (origin) => { const state = await call("connection.connect", { origin }); await refreshTransport(); token = await call("auth.token"); return state; },
    connectLocal: async () => { const state = await call("connection.local"); await refreshTransport(); token = await call("auth.token"); return state; },
    configure: () => call("connection.configure"),
    resume: () => call("connection.resume"),
    onState: subscribe("state"),
    onCommand: subscribe("command"),
    onBeforeClose: subscribe("before-close"),
    onQuitRequested: subscribe("quit-requested"),
    getQuitRequest: () => call("quit.pending"),
    replyQuit: (input) => call("quit.reply", input),
    onFlush: (callback) => {
      if (typeof callback !== "function") throw new TypeError("callback must be a function");
      flushHandlers.add(callback);
      return () => { flushHandlers.delete(callback); };
    },
    setDraftState: (input) => {
      if (input && typeof input === "object") {
        draftState = {
          ready: Boolean(input.ready),
          dirtyCount: Math.max(0, Number(input.dirtyCount) || 0),
        };
      }
      return { ...draftState };
    },
    flushAll: async (input) => {
      for (const fn of Array.from(flushHandlers)) {
        await fn(input || {});
      }
      return { persisted: true, status: flushHandlers.size ? "flushed" : (draftState.ready && draftState.dirtyCount === 0 ? "no-dirty-drafts" : "noop") };
    },
    setVisibleThreads: (ids) => {
      explicitVisibleThreads = Array.isArray(ids)
        ? ids.filter((id) => typeof id === "string" && id.trim()).map((id) => id.trim())
        : [];
      scheduleView();
      return explicitVisibleThreads.slice();
    },
    onTerminal: subscribe("terminal"),
    onPreview: subscribe("preview"),
    onNotification: subscribe("notification"),
    onSpeech: subscribe("speech"),
    onDeepLink: subscribe("deep-link"),
    onNavigate: subscribe("navigate"),
    updates: Object.freeze({
      getStatus: () => call("updates.status"),
      check: () => call("updates.check"),
      install: () => call("updates.install"),
      onStatus: subscribe("update-status"),
    }),
    openExternal: (url) => call("openExternal", { url: String(url) }),
    openWindow: (input) => call("window.open", input || {}),
    newWindow: (input) => call("window.open", input || {}),
    openDeepLink: (input) => call("deepLink.open", typeof input === "string" ? { url: input } : (input || {})),
    createVisualization: async (input) => {
      const html = input && input.html, threadId = input && input.threadId;
      if (typeof html !== "string" || !html || typeof threadId !== "string" || threadId !== activeThread()) throw desktopError("desktop.visualization_invalid", "交互图请求无效或所属会话已改变。");
      if (new Blob([html]).size > 16 * 1024 * 1024) throw desktopError("desktop.visualization_too_large", "交互图超出桌面展示范围，请打开完整原文件。");
      const id = crypto.randomUUID();
      const url = URL.createObjectURL(new Blob([withPolicy(html)], { type: "text/html;charset=utf-8" }));
      visualizations.set(id, url);
      return { id, url };
    },
    releaseVisualization: async (id) => {
      const url = visualizations.get(id);
      if (url) { visualizations.delete(id); URL.revokeObjectURL(url); }
      return null;
    },
    selectPath: (input) => call("selectPath", input),
    openPath: (input) => call("openPath", input),
    selectWorkspaceRoot: (input) => call("selectWorkspaceRoot", input),
    openWorkspaceFile: (input) => call("openWorkspaceFile", input),
    openWorkspaceInEditor: (input) => call("openWorkspaceInEditor", input),
    setMenuAccelerators: (input) => call("setMenuAccelerators", input),
    openPreview: (input) => call("preview.open", input),
    presentReportPreview: (input) => {
      reportView();
      return call("reportPreview.present", input);
    },
    hideReportPreview: (surfaceId, resetScroll) => call("reportPreview.hide", { surfaceId, resetScroll }),
    closeReportPreview: (surfaceId) => call("reportPreview.close", { surfaceId }),
    scrollReportPreview: (surfaceId, ratio) => call("reportPreview.scroll", { surfaceId, ratio }),
    wheelReportPreview: (surfaceId, delta) => call("reportPreview.scrollBy", { surfaceId, delta }),
    focusReportPreview: (surfaceId) => call("reportPreview.focus", { surfaceId }),
    pointerReportPreview: (surfaceId, pointer) => call("reportPreview.pointer", { surfaceId, pointer }),
    onReportPreview: subscribe("report-preview"),
    closePreview: (input) => call("preview.close", input || {}),
    previewAction,
    browserControl,
    cacheAttachment: (input) => {
      if (!input || !(input.data instanceof ArrayBuffer)) return Promise.reject(desktopError("desktop.attachment_cache_limit", "附件超过桌面缓存范围。"));
      if (input.data.byteLength > 256 * 1024 * 1024) return Promise.reject(desktopError("desktop.attachment_cache_limit", "附件超过桌面草稿缓存范围，请保留原文件后重选。"));
      return call("attachment.cache", { data: toBase64(input.data), name: String(input.name || ""), type: String(input.type || ""), lastModified: Number(input.lastModified) || 0, draftId: String(input.draftId || "") });
    },
    restoreAttachment: async (input) => {
      const restored = await call("attachment.restore", input);
      return { data: fromBase64(restored.data), name: restored.name, type: restored.type, lastModified: restored.lastModified };
    },
    removeAttachment: (input) => call("attachment.remove", input),
    attachmentCacheUsage: () => call("attachment.usage"),
    terminalOpen,
    terminalSend: async (id, data) => { terminalSocket(id).send(String(data)); return null; },
    terminalClose: async (id) => { const socket = terminals.get(id); if (socket) { terminals.delete(id); socket.close(); } return null; },
    acknowledgeClose: (input) => call("close.ack", input),
    requestMicrophone: (input) => call("speech.requestMicrophone", input || {}),
    startSpeech: (input) => call("speech.start", input || {}),
    finishSpeech: (input) => call("speech.finish", input || {}),
    cancelSpeech: (input) => call("speech.cancel", input || {}),
    openPermissionSettings: (kind) => call("openPermissionSettings", { kind }),
    notificationStatus: (input) => call("notification.status", input),
    requestNotifications: (input) => call("notification.request", input),
    sendNotification: (input) => call("notification.send", input),
    testNotification: (input) => call("notification.test", input),
    playNotificationSound: (input) => call("notification.sound", input),
    setDockBadge: (input) => call("notification.badge", input),
    getDockBadge: () => call("notification.dockBadge"),
  };
  Object.defineProperty(window, "mutekiDesktop", { value: Object.freeze(bridge), writable: false, configurable: false, enumerable: false });
  scheduleView();
})();
