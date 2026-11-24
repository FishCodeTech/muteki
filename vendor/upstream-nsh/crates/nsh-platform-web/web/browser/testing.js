// A test session's app realm: its time and its requests are the engine's.
// On a manual clock (`open(..., { clock: {} })`) the browser's timers,
// frames, `Date` and `performance.now()` would run on the page's time,
// which the test cannot move, so the engine keeps them (`timers.rs`) and
// runs them as `app.clock` moves it. In a test session `fetch` goes through
// the engine's network (`fetch.rs`), where the test's routes answer it and
// `app.requests()` records it. Installed before the entry is imported.

const REDIRECTS = new Set([301, 302, 303, 307, 308]);
const NULL_BODY = new Set([101, 103, 204, 205, 304]);

const define = (target, name, value) =>
  Object.defineProperty(target, name, { value, writable: true, configurable: true, enumerable: false });

function installClock(target, call, active, wake) {
  const NativeDate = target.Date;
  const now = () => call(1001, []);
  // The intrinsic given the clock's time when given none; its prototype
  // stays, so dates, `instanceof Date` and subclasses behave as before.
  function ClockDate(...args) {
    if (new.target === undefined) return new NativeDate(now()).toString();
    return Reflect.construct(NativeDate, args.length ? args : [now()], new.target);
  }
  Object.defineProperty(ClockDate, "name", { value: "Date", configurable: true });
  Object.defineProperty(ClockDate, "length", { value: 7, configurable: true });
  Object.defineProperty(ClockDate, "prototype", { value: NativeDate.prototype, writable: false });
  define(NativeDate.prototype, "constructor", ClockDate);
  define(ClockDate, "UTC", NativeDate.UTC);
  define(ClockDate, "parse", NativeDate.parse);
  define(ClockDate, "now", function now() { return call(1001, []); });
  define(target, "Date", ClockDate);

  const performance = target.performance;
  if (performance) {
    define(performance, "now", function now() { return call(1002, []); });
    Object.defineProperty(performance, "timeOrigin", { value: call(1001, []) - call(1002, []), configurable: true });
  }

  // Each callback ends with a pump: what its promises do afterwards is
  // committed too.
  const guarded = (callback, args) => (...more) => {
    if (!active()) return;
    try { callback.apply(undefined, args ?? more); } finally { wake(); }
  };
  const handler = (name, callback) => {
    if (typeof callback !== "function") throw new TypeError(`${name}: the handler is not a function`);
    return callback;
  };
  define(target, "setTimeout", function setTimeout(callback, delay, ...args) {
    return call(1003, [guarded(handler("setTimeout", callback), args), Number(delay), false]);
  });
  define(target, "setInterval", function setInterval(callback, delay, ...args) {
    return call(1003, [guarded(handler("setInterval", callback), args), Number(delay), true]);
  });
  const clear = id => { if (typeof id === "number") call(1004, [id]); };
  define(target, "clearTimeout", function clearTimeout(id) { clear(id); });
  define(target, "clearInterval", function clearInterval(id) { clear(id); });
  define(target, "requestAnimationFrame", function requestAnimationFrame(callback) {
    return call(1005, [guarded(handler("requestAnimationFrame", callback))]);
  });
  define(target, "cancelAnimationFrame", function cancelAnimationFrame(id) {
    if (typeof id === "number") call(1006, [id]);
  });
}

function installFetch(target, call, active, wake) {
  const { Request, Response, ReadableStream, TypeError: RealmTypeError } = target;
  const nativeFetch = target.fetch;
  const failure = message => new RealmTypeError(`Network request failed: ${message}`);
  const inflight = new Map();
  const finish = id => {
    const entry = inflight.get(id);
    if (!entry) return undefined;
    inflight.delete(id);
    entry.signal.removeEventListener("abort", entry.abort);
    return entry;
  };
  const head = (id, entry, [status, statusText, url, redirected, headers]) => {
    if (entry.redirectError && REDIRECTS.has(status)) {
      finish(id);
      call(1009, [id]);
      entry.reject(failure('the response was a redirect and redirect is "error"'));
      return;
    }
    let body = null;
    if (!entry.nullBody && !NULL_BODY.has(status)) {
      body = new ReadableStream({
        start(controller) { entry.controller = controller; },
        cancel() { if (finish(id)) call(1009, [id]); },
      });
    }
    let response;
    try {
      response = new Response(body, { status, statusText, headers });
    } catch (error) {
      finish(id);
      call(1009, [id]);
      entry.reject(error);
      return;
    }
    // What a network response reports, which a constructed one cannot.
    Object.defineProperties(response, {
      url: { value: url, configurable: true },
      redirected: { value: redirected, configurable: true },
      type: { value: "basic", configurable: true },
    });
    entry.resolve(response);
    entry.resolve = entry.reject = null;
  };
  call(1007, [(id, kind, ...data) => {
    const entry = inflight.get(id);
    if (!entry || !active()) return;
    try {
      switch (kind) {
        case 0: if (entry.resolve) head(id, entry, data); break;
        case 1: entry.controller?.enqueue(data[0]); break;
        case 2:
          finish(id);
          if (entry.reject) entry.reject(failure("the response ended before its head"));
          else entry.controller?.close();
          break;
        case 3: {
          finish(id);
          const error = failure(data[0]);
          if (entry.reject) entry.reject(error); else entry.controller?.error(error);
          break;
        }
        case 4: {
          // Refused by the capabilities: a TypeError as fetch's, with the
          // code, the capability and the URL refused.
          finish(id);
          const error = Object.assign(failure(data[0]), { code: "ERR_ACCESS_DENIED", permission: "network", resource: data[1] });
          if (entry.reject) entry.reject(error); else entry.controller?.error(error);
          break;
        }
      }
    } finally { wake(); }
  }]);

  define(target, "fetch", async function fetch(input, init) {
    const request = new Request(input, init);
    const { protocol } = new URL(request.url);
    // Only HTTP crosses the network; `data:` and `blob:` URLs stay the page's.
    if (protocol !== "http:" && protocol !== "https:") return nativeFetch.call(target, input, init);
    const { signal } = request;
    if (signal.aborted) throw signal.reason;
    const body = request.body ? new Uint8Array(await request.arrayBuffer()) : undefined;
    if (signal.aborted) throw signal.reason;
    const headers = [...request.headers];
    if (!request.headers.has("accept")) headers.push(["accept", "*/*"]);
    const url = request.url.split("#")[0];
    return new Promise((resolve, reject) => {
      const id = call(1008, [request.method, url, headers, body, request.redirect === "follow"]);
      const entry = {
        resolve, reject, signal, controller: null,
        nullBody: request.method === "HEAD", redirectError: request.redirect === "error",
        abort() {
          if (!finish(id)) return;
          call(1009, [id]);
          const reason = signal.reason;
          if (entry.reject) entry.reject(reason); else entry.controller?.error(reason);
          wake();
        },
      };
      inflight.set(id, entry);
      signal.addEventListener("abort", entry.abort);
      wake();
    });
  });
}

/**
 * Puts the engine's clock and network in `target`'s place (the app realm's
 * global object) where the session needs them: the clock when the
 * engine's is manual, `fetch` when `network` (a test session). `call` is
 * the host's operation call; callbacks end with `wake` (a pump).
 */
export function installTesting(target, call, { active = () => true, wake = () => {}, network = false } = {}) {
  const clock = call(1000, []) === true;
  if (clock) installClock(target, call, active, wake);
  if (network) installFetch(target, call, active, wake);
  return { clock, network };
}
