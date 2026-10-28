// Discover model capabilities through Pi's public RPC protocol, without a turn.
import { spawn } from "node:child_process";
import { mkdtemp, rm } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import readline from "node:readline";

const thinkingLevels = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];
const cwd = await mkdtemp(path.join(os.tmpdir(), "muteki-pi-catalog-"));
const child = spawn(process.argv[2] || "pi", [
  "--mode", "rpc", "--no-session", "--offline", "--no-tools", "--no-extensions",
  "--no-mcp", "--no-skills", "--no-prompt-templates", "--no-themes", "--no-context-files",
], { cwd, env: { ...process.env, PI_OFFLINE: "1", PI_TELEMETRY: "0" }, stdio: ["pipe", "pipe", "pipe"] });
const pending = new Map();
let sequence = 0;
let failure;
function fail(error) {
  failure = error;
  for (const { reject } of pending.values()) reject(error);
  pending.clear();
}
const lines = readline.createInterface({ input: child.stdout });
lines.on("line", line => {
  let response;
  try { response = JSON.parse(line); } catch { return; }
  if (response.type !== "response" || !pending.has(response.id)) return;
  const { resolve, reject } = pending.get(response.id);
  pending.delete(response.id);
  if (response.success) resolve(response.data);
  else reject(new Error(`Pi RPC ${response.command} failed: ${response.error || "unknown error"}`));
});
// Drain startup diagnostics so a verbose CLI cannot block on stderr.
child.stderr.resume();
child.stdin.on("error", fail);
child.on("error", fail);
child.on("exit", code => fail(new Error(`Pi RPC model discovery exited (${code})`)));
const timer = setTimeout(() => fail(new Error("Pi RPC model discovery timed out")), 25000);
function request(type) {
  if (failure) return Promise.reject(failure);
  return new Promise((resolve, reject) => {
    const id = `catalog-${++sequence}`;
    pending.set(id, { resolve, reject });
    child.stdin.write(JSON.stringify({ id, type }) + "\n", error => { if (error) fail(error); });
  });
}
try {
  const state = await request("get_state");
  const inventory = await request("get_available_models");
  if (!Array.isArray(inventory?.models)) throw new Error("Pi RPC returned an invalid model inventory");
  const models = inventory.models.map(model => {
    const mapping = model.thinkingLevelMap || {};
    const levels = model.reasoning === true ? thinkingLevels.filter(level =>
      mapping[level] !== null && (!["xhigh", "max"].includes(level) || mapping[level] !== undefined)
    ) : [];
    const active = state?.model?.id === model.id && state?.model?.provider === model.provider;
    return {
      id: model.id,
      label: `${model.name || model.id} (${model.provider})`,
      provider: model.provider,
      input: model.input,
      contextWindow: model.contextWindow,
      maxTokens: model.maxTokens,
      reasoning: model.reasoning === true,
      ...(model.thinkingLevelMap ? { thinkingLevelMap: model.thinkingLevelMap } : {}),
      levels,
      ...(active && levels.includes(state.thinkingLevel) ? { default: state.thinkingLevel } : {}),
    };
  });
  console.log(JSON.stringify({ models }));
} catch (error) {
  console.error(error.message);
  process.exitCode = 1;
} finally {
  clearTimeout(timer);
  lines.close();
  child.stdin.destroy();
  child.kill();
  await new Promise(resolve => {
    if (!child.pid || child.exitCode !== null || child.signalCode !== null) return resolve();
    child.once("exit", resolve);
    const force = setTimeout(() => child.kill("SIGKILL"), 1000);
    force.unref();
  });
  await rm(cwd, { recursive: true, force: true });
}
