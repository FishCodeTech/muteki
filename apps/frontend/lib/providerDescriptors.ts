"use client";

import { useEffect, useSyncExternalStore } from "react";
import { apiFetch, onAuthRequired } from "./serviceAuth";
import { useConversationLocalPreview } from "./conversationPreviewMode";


export const PROVIDER_DESCRIPTOR_SCHEMA_VERSION = 1;

export type ProviderTransportKind = "rpc" | "sdk" | "acp" | "http" | "http+ws" | "cli";
export type ProviderAdapterRole = "default" | "variant" | "cli";

export interface ProviderCapabilities {
  streaming: boolean;
  resume: boolean;
  steer: boolean;
  interrupt: boolean;
  approval: boolean;
  user_input: boolean;
  fork: boolean;
  structured_output: boolean;
  subagents: boolean;
  skills: boolean;
  mcp: boolean;
  native_tool_binding: boolean;
  acp_mcp_config: boolean;
  agent_plugin: boolean;
  runtime_plugin_binding: boolean;
  structured_http_rpc: boolean;
  tool_events: boolean;
  usage_events: boolean;
  session_persistence: boolean;
  plan: boolean;
  plan_mode: boolean;
  image_input: boolean;
  compaction: boolean;
  context_usage: boolean;
  supported_models: string[];
  supported_efforts: string[];
  access_modes: string[];
  permission_modes: string[];
  sandbox_modes: string[];
  transport_kind: string;
  protocol_version: string;
  runtime_version: string;
  capability_source: string;
}

export interface ProviderTransportSetting {
  name: string;
  type: "boolean" | "string" | "number";
  title: string;
  description: string;
  default: string | number | boolean | null;
  minimum: number | null;
  maximum: number | null;
}

export interface ProviderAdapterDescriptor {
  adapter_id: string;
  role: ProviderAdapterRole;
  transport_kind: ProviderTransportKind;
  legacy_aliases: string[];
  capabilities: ProviderCapabilities;
  native_rewind: boolean;
  capability_gateway: boolean;
  access_mode_notes: Record<string, string>;
  binary_resolution: "engine_bin" | "adapter_owned";
  scoped_model_catalog_probe: boolean;
  accepts_adapter_endpoint: boolean;
  accepts_launch_args: boolean;
  transport_settings: ProviderTransportSetting[];
  notes: string;
}

export interface ProviderIdentity {
  engine: string;
  display_name: string;
  support_status: string;
  default_adapter_id: string;
  cli_adapter_id: string;
  transport_label: string;
  default_enabled?: boolean;
}

export interface ProviderLoginSpec {
  guidance_command: string;
  guidance_note: string;
  login_argv: string[];
  status_probe: string;
  status_argv: string[];
  host_login_import: "none" | "claude_settings_env" | "kimi_home_copy" | "grok_home_copy" | "codex_auth_json";
  system_login_only: boolean;
}

export interface ProviderCredentialSpec {
  env_resolver: string;
  env_keys: string[];
  secret_file: "" | "CLAUDE_CODE_OAUTH_TOKEN" | "CURSOR_API_KEY" | "API_KEY" | "CODEX_AUTH_HOME";
  agent_state_dir: boolean;
}

export interface ProviderModelDiscoverySpec {
  method: "cli" | "acp" | "reference_catalog";
  argv: string[];
  stdin?: string;
  fallback_argv: string[];
  fallback_source: string;
  parser: string | null;
  metadata_probe: string;
  provider_scoped: boolean;
  endpoint_protocol: "openai" | "anthropic";
  endpoint_test_isolated_config: boolean;
  system_credential_live_catalog: boolean;
}

export interface ProviderEnvironmentSpec {
  home_env_var: string;
  home_relative: string;
  home_env_is_parent: boolean;
  managed_home: boolean;
  mcp_files: string[];
  extra_mcp_home_files: string[];
  extra_configuration_names: string[];
  native_extension_assets: boolean;
  extra_imports: Array<{ host_relative: string; target: string }>;
  private_env: Record<string, "home_target" | "private_data">;
  memory_credential_store_when: string[];
  memory_credential_store_env: string;
  user_skill_roots: string[];
  plugin_manifest: string;
  worker_component_home_env: string;
  worker_component_home_subdir: string;
}

export interface ProviderComponentSpec {
  native_agents: "none" | "plugin_package" | "home_agents_dir";
  native_hooks: "none" | "all" | "command_only";
  native_plugin_packages: "none" | "claude_local_plugins" | "codex_marketplace";
  native_skill_semantics: boolean;
  extension_abi: boolean;
  extension_dir: string;
  extension_export: "" | "default" | "star";
}

export interface ProviderCliSpec {
  reasoning_efforts: string[];
  effort_style: string;
  options_before_prompt_flag: boolean;
  runtime_argv: string[];
  model_env_var: string;
  endpoint_driver_sets_model: boolean;
  provider_env_prefix: string;
  turn_settled_event: string;
}

export interface ProviderWorkerProfileSpec {
  transport: string;
  endpoint_wire_api: "" | "responses" | "chat_completions";
  protocol_label: string;
  official_credential_mode: "subscription" | "api_key";
}

export interface ProviderCommandCatalogSpec {
  client_aliases: Record<string, string>;
  terminal_only: string[] | null;
}

export interface ProviderDescriptor {
  descriptor_version: number;
  identity: ProviderIdentity;
  adapters: ProviderAdapterDescriptor[];
  login: ProviderLoginSpec;
  credentials: ProviderCredentialSpec;
  models: ProviderModelDiscoverySpec;
  environment: ProviderEnvironmentSpec;
  components: ProviderComponentSpec;
  attachments: { native_image_wire: "none" | "codex_user_input" | "claude_content_blocks" | "pi_prompt_images" | "acp_content" };
  cli: ProviderCliSpec;
  worker?: ProviderWorkerProfileSpec;
  commands: ProviderCommandCatalogSpec;
  session_import: "none" | "claude_projects" | "codex_sessions";
}

export interface ProviderInstanceCapabilities {
  key: string;
  adapter_id: string;
  engine: string;
  role: ProviderAdapterRole;
  configured: boolean;
  probed_at: string;
  capabilities: ProviderCapabilities;
  capability_sources: Record<string, string>;
  native_rewind: boolean;
  capability_gateway: boolean;
  access_mode_notes: Record<string, string>;
  capability_snapshot_error?: { code: string; message: string };
}

export interface ProviderDescriptorCatalog {
  schemaVersion: number;
  descriptors: ProviderDescriptor[];
  instances: ProviderInstanceCapabilities[];
}

export class ProviderDescriptorError extends Error {
  readonly httpStatus: number;
  readonly code: string;

  constructor(message: string, httpStatus: number, code = "") {
    super(message);
    this.name = "ProviderDescriptorError";
    this.httpStatus = httpStatus;
    this.code = code;
  }
}

async function responseError(response: Response): Promise<ProviderDescriptorError> {
  let detail = "";
  let code = "";
  try {
    const body = await response.json() as { error?: { code?: string; message?: string } };
    detail = String(body.error?.message || "");
    code = String(body.error?.code || "");
  } catch {  }
  const reason = detail ? `：${detail}` : "";
  return new ProviderDescriptorError(`加载引擎描述失败${reason}（HTTP ${response.status}）`, response.status, code);
}

export async function fetchProviderDescriptors(engine = ""): Promise<ProviderDescriptorCatalog> {
  const query = engine ? `?engine=${encodeURIComponent(engine)}` : "";
  let response: Response;
  try {
    response = await apiFetch(`/api/agent-runtimes/descriptors${query}`);
  } catch (cause) {
    const reason = cause instanceof Error && cause.message ? `：${cause.message}` : "";
    throw new ProviderDescriptorError(`加载引擎描述失败${reason}（HTTP network）`, 0);
  }
  if (!response.ok) throw await responseError(response);
  const body = await response.json() as {
    schema_version?: unknown; descriptors?: unknown; instances?: unknown;
  };
  if (body.schema_version !== PROVIDER_DESCRIPTOR_SCHEMA_VERSION) {
    throw new ProviderDescriptorError(
      `引擎描述版本不兼容（服务端 ${String(body.schema_version)}，界面 ${PROVIDER_DESCRIPTOR_SCHEMA_VERSION}）`,
      response.status, "provider.descriptor_schema_mismatch",
    );
  }
  if (!Array.isArray(body.descriptors) || !Array.isArray(body.instances)) {
    throw new ProviderDescriptorError("引擎描述返回了无效响应", response.status, "provider.descriptor_invalid");
  }
  return {
    schemaVersion: body.schema_version,
    descriptors: body.descriptors as ProviderDescriptor[],
    instances: body.instances as ProviderInstanceCapabilities[],
  };
}

export type ProviderDescriptorState =
  | { status: "loading" }
  | { status: "ready"; catalog: ProviderDescriptorCatalog }
  | { status: "error"; message: string; httpStatus: number; code: string };

const LOADING: ProviderDescriptorState = { status: "loading" };
let state: ProviderDescriptorState = LOADING;
let inflight: Promise<ProviderDescriptorCatalog> | null = null;
let epoch = 0;
const listeners = new Set<() => void>();

function publish(next: ProviderDescriptorState) {
  state = next;
  listeners.forEach((listener) => listener());
}

if (typeof window !== "undefined") {
  onAuthRequired(() => {
    epoch += 1;
    inflight = null;
    publish(LOADING);
  });
}

export function loadProviderDescriptors(options?: { fresh?: boolean }): Promise<ProviderDescriptorCatalog> {
  if (!options?.fresh && state.status === "ready") return Promise.resolve(state.catalog);
  if (inflight && !options?.fresh) return inflight;
  const owner = epoch;
  const request = fetchProviderDescriptors().then(
    (catalog) => {
      if (owner === epoch) publish({ status: "ready", catalog });
      return catalog;
    },
    (cause: unknown) => {
      if (owner === epoch) {
        const error = cause instanceof ProviderDescriptorError ? cause : null;
        publish({
          status: "error",
          message: cause instanceof Error ? cause.message : String(cause),
          httpStatus: error?.httpStatus ?? 0,
          code: error?.code ?? "",
        });
      }
      throw cause;
    },
  ).finally(() => {
    if (inflight === request) inflight = null;
  });
  inflight = request;
  return request;
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => { listeners.delete(listener); };
}

export function useProviderDescriptors(options?: { enabled?: boolean }): ProviderDescriptorState {
  const localPreview = useConversationLocalPreview();
  const enabled = options?.enabled !== false && !localPreview;
  const current = useSyncExternalStore(subscribe, () => state, () => LOADING);
  useEffect(() => {
    if (enabled && state.status === "loading") void loadProviderDescriptors().catch(() => undefined);
  }, [enabled]);
  return enabled ? current : LOCAL_PREVIEW_STATE;
}

const LOCAL_PREVIEW_STATE: ProviderDescriptorState = {
  status: "ready",
  catalog: { schemaVersion: PROVIDER_DESCRIPTOR_SCHEMA_VERSION, descriptors: [], instances: [] },
};

export function readyCatalog(current: ProviderDescriptorState): ProviderDescriptorCatalog | null {
  return current.status === "ready" ? current.catalog : null;
}


type Catalog = ProviderDescriptorCatalog | null | undefined;

export function providerEngines(catalog: Catalog): string[] {
  return (catalog?.descriptors ?? []).map((item) => item.identity.engine);
}

export function providerWorkerEngines(catalog: Catalog): string[] {
  return (catalog?.descriptors ?? []).filter((item) => Boolean(item.worker?.transport)).map((item) => item.identity.engine);
}

export function isKnownEngine(catalog: Catalog, engine: string | undefined): boolean {
  return Boolean(descriptorForEngine(catalog, engine));
}

export function descriptorForEngine(catalog: Catalog, engine: string | undefined): ProviderDescriptor | undefined {
  const wanted = String(engine || "").trim().toLowerCase();
  if (!wanted) return undefined;
  return catalog?.descriptors.find((item) => item.identity.engine === wanted);
}

export function adapterIdOfInstanceKey(instanceKey: string): string {
  const key = String(instanceKey || "");
  const split = key.indexOf(":");
  return split < 0 ? key : key.slice(0, split);
}

function adapterEntry(catalog: Catalog, adapterId: string | undefined):
  { descriptor: ProviderDescriptor; adapter: ProviderAdapterDescriptor } | undefined {
  const wanted = String(adapterId || "").trim();
  if (!wanted || !catalog) return undefined;
  for (const descriptor of catalog.descriptors) {
    const adapter = descriptor.adapters.find((item) => (
      item.adapter_id === wanted || item.legacy_aliases.includes(wanted)
    ));
    if (adapter) return { descriptor, adapter };
  }
  const descriptor = descriptorForEngine(catalog, wanted);
  const adapter = descriptor?.adapters.find((item) => item.adapter_id === descriptor.identity.default_adapter_id);
  return descriptor && adapter ? { descriptor, adapter } : undefined;
}

export function descriptorForAdapter(catalog: Catalog, adapterId: string | undefined): ProviderDescriptor | undefined {
  return adapterEntry(catalog, adapterId)?.descriptor;
}

export function adapterDescriptor(catalog: Catalog, adapterId: string | undefined): ProviderAdapterDescriptor | undefined {
  return adapterEntry(catalog, adapterId)?.adapter;
}

export function engineForAdapter(catalog: Catalog, adapterId: string | undefined): string {
  return descriptorForAdapter(catalog, adapterId)?.identity.engine ?? "";
}

export function isKnownAdapter(catalog: Catalog, adapterId: string | undefined): boolean {
  return Boolean(adapterEntry(catalog, adapterId));
}

export function engineDisplayName(catalog: Catalog, engine: string): string {
  return descriptorForEngine(catalog, engine)?.identity.display_name || engine;
}

export function loginGuidance(catalog: Catalog, engine: string): { command: string; note: string } {
  const login = descriptorForEngine(catalog, engine)?.login;
  return { command: login?.guidance_command ?? "", note: login?.guidance_note ?? "" };
}

export function capabilitiesFor(catalog: Catalog, instanceKey: string | undefined): ProviderCapabilities | undefined {
  const key = String(instanceKey || "");
  if (!key || !catalog) return undefined;
  const row = catalog.instances.find((item) => item.key === key);
  if (row) return row.capabilities;
  return adapterDescriptor(catalog, adapterIdOfInstanceKey(key))?.capabilities;
}

export function runtimeScopesModelCatalog(catalog: Catalog, runtimeKey: string | undefined): boolean {
  return adapterDescriptor(catalog, adapterIdOfInstanceKey(runtimeKey || ""))?.scoped_model_catalog_probe === true;
}
