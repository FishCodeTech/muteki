// Muteki <-> @cursor/sdk bridge.
//
// Protocol: one JSON object per LF-terminated line on stdin/stdout. The
// reader frames on 0x0A bytes only (never readline, which also splits on
// U+2028/U+2029 that are legal inside JSON strings).
//
//   request : {"id": "...", "op": "<op>", ...params}
//   response: {"type": "response", "id": "...", "ok": true, "data": {...}}
//             {"type": "response", "id": "...", "ok": false, "error": {...}}
//   event   : {"type": "event", "kind": "<kind>", "turn": "<turnKey>", ...}
//
// Credentials arrive only through the CURSOR_API_KEY environment variable and
// are never echoed. Everything the SDK prints is routed to stderr; stdout
// carries protocol frames only.
import { readFileSync } from "node:fs";
import process from "node:process";

const PROTOCOL_VERSION = 1;
const MIN_NODE = [22, 13];

// Only send() reaches the real stdout; anything else the SDK or its
// dependencies write there goes to stderr so it can never corrupt a frame.
const writeProtocol = process.stdout.write.bind(process.stdout);
process.stdout.write = (chunk, ...rest) => process.stderr.write(chunk, ...rest);
function send(message) {
  writeProtocol(`${JSON.stringify(message)}\n`);
}
for (const name of ["log", "info", "debug", "warn"]) {
  console[name] = (...args) => console.error(...args);
}

const SHELL_SPAWN_MARKERS = ["dump_zsh_state", "dump_bash_state", "__CURSOR_SANDBOX_ENV_RESTORE"];

// The SDK spawns a sandbox wrapper shell for tool calls; a bad working
// directory makes it reject an internal promise nobody awaits, which would
// otherwise kill this process. Only that exact rejection is tolerated.
function isShellSpawnFailure(reason) {
  if (typeof reason !== "object" || reason === null) return false;
  const { syscall, code, spawnargs } = reason;
  if (typeof syscall !== "string" || !syscall.startsWith("spawn") || typeof code !== "string") return false;
  if (!Array.isArray(spawnargs)) return false;
  return spawnargs.some((arg) => typeof arg === "string" && SHELL_SPAWN_MARKERS.some((m) => arg.includes(m)));
}
process.on("unhandledRejection", (reason) => {
  if (isShellSpawnFailure(reason)) {
    console.error("Cursor shell spawn failed; the bridge keeps running.", reason);
    return;
  }
  console.error("unhandledRejection", reason);
  process.exit(1);
});

function nodeSupported() {
  const [major, minor] = process.versions.node.split(".").map(Number);
  return major > MIN_NODE[0] || (major === MIN_NODE[0] && minor >= MIN_NODE[1]);
}

let sdk;
async function loadSdk() {
  if (!nodeSupported()) {
    throw bridgeError("cursor_sdk.node_unsupported", `Node ${process.versions.node} is older than ${MIN_NODE.join(".")}`, {
      category: "validation",
    });
  }
  sdk ??= await import("@cursor/sdk");
  return sdk;
}

function bridgeError(code, message, extra = {}) {
  const error = new Error(message);
  error.bridge = { code, ...extra };
  return error;
}

function isAbortChain(cause) {
  const seen = new Set();
  let current = cause;
  while (typeof current === "object" && current !== null && !seen.has(current)) {
    if (current.name === "AbortError") return true;
    seen.add(current);
    current = current.cause;
  }
  return false;
}

// Typed mapping from SDK error classes; message text is never inspected.
function describeError(err) {
  if (err?.bridge) {
    return { message: String(err.message), detail: err.stack ? String(err.stack) : "", ...err.bridge };
  }
  const base = {
    message: String(err?.message ?? err),
    detail: err?.stack ? String(err.stack) : "",
    sdkClass: err?.constructor?.name ?? typeof err,
  };
  if (err && typeof err === "object") {
    for (const key of ["status", "requestId", "operation", "endpoint", "helpUrl", "provider"]) {
      if (err[key] !== undefined) base[key] = err[key];
    }
    if (err.code !== undefined) base.nativeCode = String(err.code);
    if (err.isRetryable !== undefined) base.retryable = Boolean(err.isRetryable);
    const causes = [];
    const seen = new Set([err]);
    const pending = [err.cause, ...(err instanceof AggregateError ? err.errors : [])];
    while (pending.length) {
      const cause = pending.shift();
      if (cause === undefined || cause === null) continue;
      if (typeof cause !== "object") {
        causes.push({ message: String(cause) });
        continue;
      }
      if (seen.has(cause)) continue;
      seen.add(cause);
      const item = {};
      for (const key of ["name", "message", "stack", "code", "syscall", "address", "port"]) {
        if (typeof cause[key] === "string" || typeof cause[key] === "number") item[key] = cause[key];
      }
      causes.push(item);
      pending.push(cause.cause, ...(cause instanceof AggregateError ? cause.errors : []));
    }
    if (causes.length) base.causes = causes;
  }
  if (isAbortChain(err)) return { ...base, code: "cursor_sdk.aborted", category: "cancelled" };
  const classes = sdk ?? {};
  const is = (name) => typeof classes[name] === "function" && err instanceof classes[name];
  if (is("AuthenticationError")) return { ...base, code: "cursor_sdk.auth_required", category: "auth" };
  if (is("RateLimitError")) return { ...base, code: "cursor_sdk.rate_limited", category: "usage_limit", retryable: true };
  if (is("IntegrationNotConnectedError")) return { ...base, code: "cursor_sdk.integration_not_connected", category: "auth" };
  if (is("ConfigurationError")) {
    if (err.constructor?.name === "UnsupportedRunOperationError") {
      return { ...base, code: "cursor_sdk.unsupported_operation", category: "unsupported" };
    }
    return { ...base, code: "cursor_sdk.invalid_configuration", category: "validation" };
  }
  if (is("AgentBusyError")) return { ...base, code: "cursor_sdk.agent_busy", category: "provider" };
  if (is("AgentNotFoundError")) return { ...base, code: "cursor_sdk.agent_not_found", category: "provider" };
  if (is("NetworkError")) return { ...base, code: "cursor_sdk.network", category: "transport", retryable: true };
  if (is("CursorSdkError")) return { ...base, code: "cursor_sdk.sdk_error", category: "provider" };
  return { ...base, code: "cursor_sdk.unexpected", category: "unknown" };
}

function apiKey() {
  const key = process.env.CURSOR_API_KEY;
  if (!key) {
    throw bridgeError("cursor_sdk.auth_required", "CURSOR_API_KEY is not set for the Cursor SDK bridge", {
      category: "auth",
    });
  }
  return key;
}

// -- tool normalisation -----------------------------------------------------

function mcpName(call) {
  return `mcp__${call.args?.providerIdentifier ?? "mcp"}__${call.args?.toolName ?? "tool"}`;
}

const TOOL_KINDS = {
  shell: "command",
  edit: "file_change",
  write: "file_change",
  delete: "file_change",
  task: "agent",
};

function describeTool(call) {
  const type = String(call?.type ?? "tool");
  if (type === "mcp") {
    return { name: mcpName(call), toolKind: "mcp", input: call.args?.args ?? call.args ?? null };
  }
  const name = type === "readLints" ? "read_lints" : type === "semSearch" ? "semantic_search" : type;
  return { name, toolKind: TOOL_KINDS[type] ?? "other", input: call?.args ?? null };
}

function toolOutcome(call) {
  const result = call?.result;
  if (!result) return { status: "completed", output: null };
  const failed = result.status !== undefined && result.status !== "success";
  if (call.type === "mcp" && result.value) {
    const isError = Boolean(result.value.isError);
    return { status: failed || isError ? "failed" : "completed", output: result.value, error: failed ? errorText(result) : isError ? errorText(result.value) : undefined };
  }
  const value = result.value ?? result;
  const out = { status: failed ? "failed" : "completed", output: value };
  if (failed) out.error = errorText(result);
  if (call.type === "shell" && value && typeof value === "object") {
    if (typeof value.exitCode === "number") out.exitCode = value.exitCode;
    if (typeof value.executionTime === "number") out.durationMs = value.executionTime;
    if (typeof value.exitCode === "number" && value.exitCode !== 0 && !failed) out.status = "failed";
  }
  return out;
}

function errorText(result) {
  const err = result?.error ?? result?.value ?? result;
  return typeof err === "string" ? err : JSON.stringify(err);
}

function subagentResult(call) {
  const value = call?.result?.value;
  if (!value) return {};
  const texts = [];
  for (const step of value.conversationSteps ?? []) {
    const text = step?.assistantMessage?.text;
    if (typeof text === "string" && text.length > 0) texts.push(text);
  }
  const suffix = typeof value.resultSuffix === "string" ? value.resultSuffix : "";
  return {
    text: texts.join("\n\n") + suffix,
    agentId: value.agentId,
    durationMs: value.durationMs,
    isBackground: value.isBackground,
  };
}

// -- session ------------------------------------------------------------------

let session = null;

function requireSession() {
  if (!session) throw bridgeError("cursor_sdk.no_session", "no agent is open in this bridge", { category: "validation" });
  return session;
}

function localOptions(params, store) {
  return {
    cwd: params.cwd,
    autoReview: Boolean(params.autoReview),
    // T3 nightly 5e222567's CursorAdapterV2 explicitly lists settings layers.
    settingSources: params.settingSources ?? ["project", "user", "team", "mdm", "plugins"],
    sandboxOptions: { enabled: Boolean(params.sandbox) },
    enableAgentRetries: true,
    store,
  };
}

const EFFORT_PARAMETERS = ["reasoning_effort", "effort", "reasoning"];

const FAST_PARAMETER = "fast";

// The catalog names the effort parameter per model; an effort the model does
// not list is a configuration error, never silently dropped. Muteki's "fast"
// service tier maps to Cursor's boolean `fast` parameter. Without a tier the
// run is pinned to fast=false, because some default variants are Fast and the
// picker labels an empty tier as standard speed.
async function resolveModel(model, effort, serviceTier, key) {
  if (!effort && !serviceTier && model.id === "default") return model;
  const catalog = await sdk.Cursor.models.list({ apiKey: key });
  const entry = catalog.find((item) => item.id === model.id || (item.aliases ?? []).includes(model.id));
  const params = [...(model.params ?? [])];
  if (effort) {
    const parameter = entry?.parameters?.find((item) => EFFORT_PARAMETERS.includes(item.id));
    if (!parameter || !parameter.values.some((item) => item.value === effort)) {
      throw bridgeError("cursor_sdk.invalid_configuration", `model ${model.id} does not accept effort ${effort}`, {
        category: "validation",
      });
    }
    params.push({ id: parameter.id, value: effort });
  }
  const fast = entry?.parameters?.find((item) => item.id === FAST_PARAMETER);
  if (serviceTier) {
    if (serviceTier !== FAST_PARAMETER || !fast?.values.some((item) => item.value === "true")) {
      throw bridgeError("cursor_sdk.invalid_configuration", `model ${model.id} does not accept service tier ${serviceTier}`, {
        category: "validation",
      });
    }
    params.push({ id: FAST_PARAMETER, value: "true" });
  } else if (fast?.values.some((item) => item.value === "false")) {
    params.push({ id: FAST_PARAMETER, value: "false" });
  }
  return params.length ? { ...model, params } : model;
}

async function opOpen(params) {
  await loadSdk();
  const key = apiKey();
  if (session) throw bridgeError("cursor_sdk.session_open", "an agent is already open in this bridge", { category: "validation" });
  const store = new sdk.JsonlLocalAgentStore(params.storeDir);
  const model = await resolveModel(params.model, params.effort, params.serviceTier, key);
  const options = {
    apiKey: key,
    model,
    name: params.name ?? "muteki",
    mode: params.mode ?? "agent",
    local: localOptions(params, store),
    ...(params.mcpServers && Object.keys(params.mcpServers).length ? { mcpServers: params.mcpServers } : {}),
  };
  // Keep the executor lease alive with the exact same settings and access
  // policy used by send(). Plugin installation belongs to open's startup
  // budget rather than the first send's acknowledgement budget.
  const platform = await sdk.createAgentPlatform({ workspaceRef: params.cwd, localStore: store });
  const releasePrewarm = await platform.prewarmLocalWorkspace(options);
  let agent;
  try {
    agent = params.agentId ? await sdk.Agent.resume(params.agentId, options) : await sdk.Agent.create(options);
  } catch (error) {
    try {
      await releasePrewarm();
    } catch (cleanupError) {
      throw new AggregateError([error, cleanupError], "Agent open and executor cleanup failed", { cause: error });
    }
    throw error;
  }
  session = {
    agent,
    releasePrewarm,
    key,
    store,
    model,
    cwd: params.cwd,
    runOptions: { runtime: "local", cwd: params.cwd, store },
    run: null,
    cancelRequested: false,
    recovered: false,
    resumed: Boolean(params.agentId),
  };
  return { agentId: agent.agentId, resumed: session.resumed };
}

function emit(turn, kind, fields = {}) {
  send({ type: "event", kind, turn, ...fields });
}

function handleUpdate(turn, update, nestedOf) {
  const type = update?.type;
  const wrap = (kind, fields) => {
    if (nestedOf) emit(turn, "nested", { parentCallId: nestedOf, update: { kind, ...fields } });
    else emit(turn, kind, fields);
  };
  switch (type) {
    case "text-delta":
      if (update.text) wrap("text", { text: update.text });
      break;
    case "thinking-delta":
      if (update.text) wrap("thinking", { text: update.text });
      break;
    case "thinking-completed":
      wrap("thinking_end", { durationMs: update.thinkingDurationMs ?? null });
      break;
    case "step-completed":
      wrap("step_end", {});
      break;
    case "tool-call-started": {
      const call = update.toolCall;
      if (call?.type === "task" && !nestedOf) {
        const a = call.args ?? {};
        emit(turn, "tool_start", {
          callId: update.callId,
          ...describeTool(call),
          subagent: {
            description: a.description ?? null,
            prompt: a.prompt ?? null,
            role: a.subagentType && typeof a.subagentType === "object" ? a.subagentType.kind ?? null : null,
            model: typeof a.model === "string" ? a.model : null,
            agentId: a.agentId ?? null,
            mode: a.mode ?? null,
          },
        });
        break;
      }
      wrap("tool_start", { callId: update.callId, ...describeTool(call) });
      break;
    }
    case "tool-call-completed": {
      const call = update.toolCall;
      const outcome = toolOutcome(call);
      const fields = { callId: update.callId, tool: call?.type, ...outcome };
      if (call?.type === "task" && !nestedOf) fields.subagent = subagentResult(call);
      if (call?.type === "updateTodos" && call.args?.todos) fields.todos = call.args.todos;
      if (call?.type === "createPlan" && call.args?.plan !== undefined) fields.plan = call.args.plan;
      wrap("tool_end", fields);
      break;
    }
    case "tool-call-delta":
      if (!nestedOf && update.taskUpdate) handleUpdate(turn, update.taskUpdate, update.callId);
      break;
    case "shell-output-delta":
      wrap("tool_output", { event: update.event ?? null });
      break;
    case "turn-ended":
      if (!nestedOf && update.usage) session.turnUsage = update.usage;
      break;
    default:
      break;
  }
}

async function opSend(params) {
  const s = requireSession();
  if (s.run) throw bridgeError("cursor_sdk.turn_active", "a turn is already running in this bridge", { category: "provider" });
  const turn = String(params.turn);
  // Match T3's CursorAgentSdk callback chain: the SDK may finish successfully
  // even when onDelta failed. Preserve that first failure through run.wait().
  const callbacks = { chain: Promise.resolve(), failure: null };
  const sendOptions = {
    mode: params.mode ?? "agent",
    model: s.model,
    ...(params.mcpServers && Object.keys(params.mcpServers).length ? { mcpServers: params.mcpServers } : {}),
    onDelta: ({ update }) => {
      callbacks.chain = callbacks.chain.then(() => {
        if (callbacks.failure) return;
        handleUpdate(turn, update, null);
      }).catch((error) => {
        callbacks.failure ??= bridgeError("cursor_sdk.delta_mapping_failed",
          String(error?.message ?? error), { category: "transport",
            detail: error?.stack ? String(error.stack) : "", cause: describeError(error) });
        console.error("delta mapping failed", error);
        emit(turn, "bridge_warning", { code: "cursor_sdk.delta_mapping_failed",
          message: String(error?.message ?? error), detail: error?.stack ? String(error.stack) : "" });
      });
      return callbacks.chain;
    },
  };
  const message = params.images?.length ? { text: params.text, images: params.images } : params.text;
  s.cancelRequested = false;
  s.turnUsage = null;
  const start = () => s.agent.send(message, sendOptions);
  // A previous bridge process may have died mid-run; the SDK keeps that run
  // marked active and refuses new ones with a plain error that carries no
  // class or code to branch on. Decide from the run store instead: a resumed
  // agent's still-running runs are abandoned, so cancel them once up front.
  if (s.resumed && !s.recovered) await cancelActiveRuns(s);
  let run;
  try {
    run = await start();
  } catch (error) {
    if (!s.recovered && sdk.AgentBusyError && error instanceof sdk.AgentBusyError) {
      if ((await cancelActiveRuns(s)).length === 0) throw error;
      run = await start();
    } else {
      throw error;
    }
  }
  s.run = run;
  s.turnKey = turn;
  settle(s, run, turn, callbacks);
  return { runId: run.id, steer: typeof run.steer === "function" };
}

function settle(s, run, turn, callbacks) {
  (async () => {
    let result = null;
    let failure = null;
    try {
      result = await run.wait();
    } catch (error) {
      failure = error;
    }
    await callbacks.chain;
    failure ??= callbacks.failure;
    s.run = null;
    if (failure) {
      if (s.cancelRequested && isAbortChain(failure)) {
        emit(turn, "turn_end", { status: "cancelled", usage: s.turnUsage ?? null });
      } else {
        emit(turn, "turn_end", { status: "error", error: describeError(failure), usage: s.turnUsage ?? null });
      }
      return;
    }
    const fields = {
      status: result.status,
      result: result.result ?? null,
      durationMs: result.durationMs ?? null,
      model: result.model ?? null,
      usage: result.usage ?? s.turnUsage ?? null,
    };
    if (result.status === "error") {
      // Preserve the native error's cause, endpoint and request ID as on the
      // thrown-error path; RunResult.error must not flatten them to a message.
      const error = describeError(result.error ?? { message: "run finished with status error" });
      fields.error = {
        ...error,
        ...(error.code === "cursor_sdk.unexpected"
          ? { code: "cursor_sdk.run_failed", category: "provider" }
          : {}),
      };
    }
    emit(turn, "turn_end", fields);
  })();
}

async function opCancel() {
  const s = requireSession();
  const run = s.run;
  if (!run) return { cancelled: false };
  s.cancelRequested = true;
  try {
    await run.cancel();
  } catch (error) {
    if (!isAbortChain(error)) throw error;
  }
  return { cancelled: true, runId: run.id };
}

async function opSteer(params) {
  const s = requireSession();
  const run = s.run;
  if (!run) throw bridgeError("cursor_sdk.no_active_turn", "no turn is running", { category: "validation" });
  if (params.turn && String(params.turn) !== s.turnKey) {
    throw bridgeError("cursor_sdk.turn_mismatch", `turn ${params.turn} is not the running turn`, { category: "validation" });
  }
  if (typeof run.steer !== "function") {
    throw bridgeError("cursor_sdk.steer_unsupported", "this run does not support steer", { category: "unsupported" });
  }
  return { outcome: await run.steer(String(params.text)) };
}

async function opAuthStatus() {
  await loadSdk();
  const user = await sdk.Cursor.me({ apiKey: apiKey() });
  return { authenticated: true, userId: user?.id ?? null };
}

// Runs once per bridge process: a second "recovery" would cancel a run another
// client legitimately owns.
async function cancelActiveRuns(s) {
  s.recovered = true;
  const latest = await sdk.Agent.listRuns(s.agent.agentId, { ...s.runOptions, limit: 10 });
  const active = (latest.items ?? []).filter((item) => item.status === "running");
  for (const run of active) {
    await sdk.Agent.cancelRun(run.id, s.runOptions);
  }
  return active.map((run) => run.id);
}

async function opRecover() {
  const s = requireSession();
  const cancelledRunIds = await cancelActiveRuns(s);
  if (s.run && cancelledRunIds.includes(s.run.id)) s.run = null;
  return { cancelledRunIds };
}

async function opHistory() {
  const s = requireSession();
  const messages = await sdk.Agent.messages.list(s.agent.agentId, { runtime: "local", cwd: s.cwd, store: s.store });
  return { messages };
}

async function opClose() {
  if (session) {
    const closing = session;
    try {
      closing.agent.close();
    } finally {
      session = null;
      await closing.releasePrewarm();
    }
  }
  return {};
}

async function opHello() {
  let sdkVersion = null;
  if (nodeSupported()) {
    await loadSdk();
    const manifest = new URL("./node_modules/@cursor/sdk/package.json", import.meta.url);
    sdkVersion = JSON.parse(readFileSync(manifest, "utf8")).version;
  }
  return { protocol: PROTOCOL_VERSION, node: process.versions.node, sdk: sdkVersion, nodeSupported: nodeSupported() };
}

async function opModels() {
  await loadSdk();
  return { models: await sdk.Cursor.models.list({ apiKey: apiKey() }) };
}

// This uses the SDK 1.0.36's actual auth/dashboard endpoints and excludes variables.
async function opPublicPluginMetadata() {
  const base = "https://api2.cursor.sh";
  async function checkedJson(response, endpoint) {
    const body = await response.text();
    if (!response.ok) throw bridgeError("cursor_sdk.plugin_metadata", `Cursor plugin metadata request returned HTTP ${response.status}`, {
      category: response.status === 401 || response.status === 403 ? "auth" : "transport",
      endpoint, status: response.status, requestId: response.headers.get("x-request-id") ?? undefined, detail: body,
    });
    try { return JSON.parse(body); }
    catch (cause) { const error = bridgeError("cursor_sdk.plugin_metadata", "Cursor plugin metadata returned invalid JSON", { category: "transport", endpoint, detail: body }); error.cause = cause; throw error; }
  }
  const exchange = await fetch(`${base}/auth/exchange_user_api_key`, {
    method: "POST", headers: { Authorization: `Bearer ${apiKey()}`, "Content-Type": "application/json" },
    body: "{}", signal: AbortSignal.timeout(30000),
  });
  const token = (await checkedJson(exchange, `${base}/auth/exchange_user_api_key`)).accessToken;
  if (typeof token !== "string" || !token) throw bridgeError("cursor_sdk.plugin_metadata", "Cursor key exchange did not return an access token", { category: "auth" });
  const response = await fetch(`${base}/aiserver.v1.DashboardService/GetEffectiveUserPlugins`, {
    method: "POST", headers: { Authorization: `Bearer ${token}`, "Content-Type": "application/json", "Connect-Protocol-Version": "1" },
    body: JSON.stringify({ excludeConfiguredVariables: true }), signal: AbortSignal.timeout(30000),
  });
  const metadata = await checkedJson(response, `${base}/aiserver.v1.DashboardService/GetEffectiveUserPlugins`);
  const plugins = (metadata.plugins ?? []).filter((item) => item.isEnabled
    && (item.plugin?.distributionGitUrl?.trim() || item.plugin?.gitUrl)?.replace(/\.git$/, "") === "https://github.com/cursor/plugins"
    && (!item.plugin?.marketplace?.distributionGitUrl || item.plugin.marketplace.distributionGitUrl.trim().replace(/\.git$/, "") === "https://github.com/cursor/plugins")
    && item.plugin?.marketplace?.name === "cursor-public").map((item) => ({
      name: item.plugin.name,
      repository: "https://github.com/cursor/plugins.git",
      commit: item.pinnedGitRef || item.plugin.gitRef,
      gitPath: item.plugin.gitPath,
    }));
  return { plugins };
}

const OPS = {
  publicPluginMetadata: opPublicPluginMetadata,
  hello: opHello,
  authStatus: opAuthStatus,
  models: opModels,
  open: opOpen,
  send: opSend,
  cancel: opCancel,
  steer: opSteer,
  recover: opRecover,
  history: opHistory,
  close: opClose,
};

async function dispatch(request) {
  const { id, op, ...params } = request;
  const handler = OPS[op];
  if (!handler) {
    send({ type: "response", id, ok: false, error: { code: "cursor_sdk.unknown_op", category: "validation", message: `unknown op ${String(op)}` } });
    return;
  }
  try {
    send({ type: "response", id, ok: true, data: await handler(params) });
  } catch (error) {
    send({ type: "response", id, ok: false, error: describeError(error) });
  }
}

let buffer = Buffer.alloc(0);
process.stdin.on("data", (chunk) => {
  buffer = Buffer.concat([buffer, chunk]);
  let index;
  while ((index = buffer.indexOf(0x0a)) >= 0) {
    const line = buffer.subarray(0, index).toString("utf8").trim();
    buffer = buffer.subarray(index + 1);
    if (!line) continue;
    let request;
    try {
      request = JSON.parse(line);
    } catch (error) {
      console.error("invalid request frame", error);
      continue;
    }
    dispatch(request);
  }
});
process.stdin.on("end", async () => {
  await opClose().catch((error) => console.error("close failed", error));
  process.exit(0);
});
