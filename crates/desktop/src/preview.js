(() => {
  "use strict";
  const MAX_LOG_ENTRIES = 1000;
  const MAX_LOG_CHARS = 256000;
  const entryChars = (entry) => Object.values(entry).reduce((size, value) => size + (typeof value === "string" ? value.length : 32), 0);
  function logBuffer() {
    const rows = [];
    let dropped = 0, chars = 0, error = null;
    return {
      rows,
      get dropped() { return dropped; },
      get error() { return error; },
      push(entry) {
        const size = entryChars(entry);
        if (size > MAX_LOG_CHARS) {
          dropped += 1;
          error = { message: `日志条目有 ${size} 个字符，超过采集上限 ${MAX_LOG_CHARS}；未保存部分正文。请缩小页面日志后重新加载。` };
          return;
        }
        while (rows.length && (rows.length >= MAX_LOG_ENTRIES || chars + size > MAX_LOG_CHARS)) {
          chars -= entryChars(rows.shift());
          dropped += 1;
        }
        chars += size;
        rows.push(entry);
      },
      clear() { rows.length = 0; dropped = 0; chars = 0; error = null; },
    };
  }

  if (!window.__mutekiConsole) {
    const buffer = logBuffer();
    const normalizeLevel = (level) => {
      if (level === "debug" || level === "verbose") return "verbose";
      if (level === "warn" || level === "warning") return "warning";
      if (level === "error") return "error";
      return "info";
    };
    const recordConsole = (rawLevel, args, meta) => {
      try {
        const level = normalizeLevel(rawLevel);
        const rawText = args
          .map((item) => {
            if (typeof item === "string") return item;
            try {
              return JSON.stringify(item);
            } catch {
              return String(item);
            }
          })
          .join(" ");
        const now = new Date().toISOString();
        const url = (meta && meta.url) || location.href;
        const entry = {
          level,
          message: rawText,
          text: rawText,
          url: String(url),
          timestamp: now,
          at: now,
        };
        if (meta && typeof meta.line === "number") entry.line = meta.line;
        buffer.push(entry);
      } catch {
      }
    };
    Object.defineProperty(window, "__mutekiConsole", {
      value: buffer,
      configurable: false,
    });
    for (const [method, level] of [
      ["debug", "verbose"],
      ["log", "info"],
      ["info", "info"],
      ["warn", "warning"],
      ["error", "error"],
    ]) {
      const original = console[method];
      if (typeof original === "function") {
        console[method] = function (...args) {
          recordConsole(level, args);
          return original.apply(this, args);
        };
      }
    }
    addEventListener("error", (event) =>
      recordConsole("error", [event.message || "error"], {
        url: event.filename || location.href,
        line: typeof event.lineno === "number" ? event.lineno : undefined,
      })
    );
    addEventListener("unhandledrejection", (event) =>
      recordConsole("error", [
        String((event.reason && event.reason.message) || event.reason || "unhandledrejection"),
      ])
    );
  }

  if (!window.__mutekiNetwork) {
    const buffer = logBuffer();
    let seq = 0;
    const seenResources = new Set();
    let seenChars = 0;
    const resolveUrl = (input) => {
      try {
        if (!input) return location.href;
        if (typeof input === "string") return new URL(input, location.href).href;
        if (typeof URL !== "undefined" && input instanceof URL) return input.href;
        if (typeof input === "object" && typeof input.url === "string") {
          return new URL(input.url, location.href).href;
        }
        return String(input);
      } catch {
        return String(input || location.href);
      }
    };
    const recordNetwork = (entry) => {
      try {
        const status = Number(entry.status) || 0;
        const now = entry.timestamp || new Date().toISOString();
        const item = {
          id: ++seq,
          method: String(entry.method || "GET").toUpperCase(),
          url: String(entry.url || location.href),
          status,
          statusText: String(entry.statusText || ""),
          ok: typeof entry.ok === "boolean" ? entry.ok : status >= 200 && status < 300,
          duration: Math.max(0, Math.round(Number(entry.duration) || 0)),
          initiatorType: String(entry.initiatorType || "other"),
          timestamp: now,
          at: now,
        };
        if (entry.error) item.error = String(entry.error);
        buffer.push(item);
      } catch {
      }
    };

    Object.defineProperty(window, "__mutekiNetwork", {
      value: {
        rows: buffer.rows,
        get dropped() { return buffer.dropped; },
        get error() { return buffer.error; },
        record: recordNetwork,
        clear() {
          buffer.clear();
          seenResources.clear();
          seenChars = 0;
        },
      },
      configurable: false,
    });

    if (typeof window.fetch === "function") {
      const originalFetch = window.fetch.bind(window);
      window.fetch = async function (...args) {
        const [input, init] = args;
        const method = String(
          (init && init.method) || (input && typeof input === "object" && input.method) || "GET"
        ).toUpperCase();
        const url = resolveUrl(input);
        const started = typeof performance !== "undefined" ? performance.now() : Date.now();
        const timestamp = new Date().toISOString();
        try {
          const response = await originalFetch(...args);
          const ended = typeof performance !== "undefined" ? performance.now() : Date.now();
          recordNetwork({
            method,
            url: response && response.url ? response.url : url,
            status: response ? response.status : 0,
            statusText: (response && response.statusText) || "",
            ok: Boolean(response && response.ok),
            duration: ended - started,
            initiatorType: "fetch",
            timestamp,
          });
          return response;
        } catch (error) {
          const ended = typeof performance !== "undefined" ? performance.now() : Date.now();
          recordNetwork({
            method,
            url,
            status: 0,
            statusText: "",
            ok: false,
            duration: ended - started,
            initiatorType: "fetch",
            timestamp,
            error: (error && error.message) || String(error || "fetch failed"),
          });
          throw error;
        }
      };
    }

    if (typeof window.XMLHttpRequest === "function") {
      const xhrMeta = new WeakMap();
      const origOpen = XMLHttpRequest.prototype.open;
      const origSend = XMLHttpRequest.prototype.send;
      XMLHttpRequest.prototype.open = function (method, url, ...rest) {
        try {
          xhrMeta.set(this, {
            method: String(method || "GET").toUpperCase(),
            url: resolveUrl(url),
          });
        } catch {
        }
        return origOpen.call(this, method, url, ...rest);
      };
      XMLHttpRequest.prototype.send = function (...args) {
        let finishRequest;
        try {
          const meta = xhrMeta.get(this) || { method: "GET", url: location.href };
          const started = typeof performance !== "undefined" ? performance.now() : Date.now();
          const timestamp = new Date().toISOString();
          let recorded = false;
          const finish = (errText) => {
            if (recorded) return;
            recorded = true;
            for (const [name, listener] of listeners) this.removeEventListener(name, listener, true);
            const ended = typeof performance !== "undefined" ? performance.now() : Date.now();
            const sameRequest = xhrMeta.get(this) === meta;
            const status = sameRequest ? Number(this.status) || 0 : 0;
            recordNetwork({
              method: meta.method,
              url: sameRequest ? this.responseURL || meta.url : meta.url,
              status,
              statusText: sameRequest ? this.statusText || "" : "",
              ok: status >= 200 && status < 300,
              duration: ended - started,
              initiatorType: "xmlhttprequest",
              timestamp,
              error: errText || (!sameRequest ? "xhr reused before completion was recorded" : status === 0 ? "xhr failed" : undefined),
            });
          };
          finishRequest = finish;
          const listeners = [["loadend", () => finish(undefined)], ["error", () => finish("network error")], ["abort", () => finish("aborted")], ["timeout", () => finish("timeout")]];
          for (const [name, listener] of listeners) this.addEventListener(name, listener, { once: true, capture: true });
        } catch {
        }
        try { return origSend.apply(this, args); }
        catch (error) {
          finishRequest?.((error && error.message) || "xhr send failed");
          throw error;
        }
      };
    }

    const recordResourceEntry = (entry) => {
      try {
        if (!entry || !entry.name) return;
        const initiator = String(entry.initiatorType || "resource").toLowerCase();
        if (initiator === "fetch" || initiator === "xmlhttprequest") return;
        const key = `${initiator}:${entry.name}:${Math.round(entry.startTime || 0)}`;
        if (seenResources.has(key)) return;
        if (key.length <= MAX_LOG_CHARS) {
          while (seenResources.size && (seenResources.size >= MAX_LOG_ENTRIES || seenChars + key.length > MAX_LOG_CHARS)) {
            const oldest = seenResources.values().next().value;
            seenResources.delete(oldest);
            seenChars -= oldest.length;
          }
          seenResources.add(key);
          seenChars += key.length;
        }
        const originTime =
          typeof performance !== "undefined" && typeof performance.timeOrigin === "number"
            ? performance.timeOrigin
            : Date.now();
        const timestamp = new Date(originTime + (Number(entry.startTime) || 0)).toISOString();
        const status =
          typeof entry.responseStatus === "number" && entry.responseStatus > 0
            ? entry.responseStatus
            : 0;
        recordNetwork({
          method: "GET",
          url: entry.name,
          status,
          statusText: "",
          ok: status >= 200 && status < 400,
          duration: entry.duration || 0,
          initiatorType: initiator,
          timestamp,
        });
      } catch {
      }
    };

    try {
      if (typeof performance !== "undefined" && typeof performance.getEntriesByType === "function") {
        for (const entry of performance.getEntriesByType("resource")) {
          recordResourceEntry(entry);
        }
      }
      if (typeof PerformanceObserver === "function") {
        const observer = new PerformanceObserver((list) => {
          for (const entry of list.getEntries()) {
            recordResourceEntry(entry);
          }
        });
        observer.observe({ entryTypes: ["resource"] });
      }
    } catch {
    }
  }
})();
