import type { ConversationCredential, ConversationCredentialModel, ConversationServiceTier } from "./useConversation";
import { variantParameterKeys } from "./modelVariants";

const MEMORY_KEY = "muteki.chat.model-efforts.v1";
const KNOWN_ORDER = ["off", "none", "no-thinking", "on", "thinking", "default", "minimal", "low", "medium", "high", "xhigh", "extra-high", "max", "ultra"];
const effortOrder = (value: string) => {
  const index = KNOWN_ORDER.indexOf(value.toLowerCase().replaceAll(" ", "-"));
  return index < 0 ? KNOWN_ORDER.length : index;
};

export function credentialForRuntime(
  credential: ConversationCredential,
  runtimeKey: string,
  scopedCatalog: boolean,
): ConversationCredential {
  const catalog = credential.model_catalogs?.[runtimeKey];
  const models = new Map((catalog || []).map(model => [model.id, model]));
  const scope = (rows: ConversationCredentialModel[]) => rows.map(model => {
    const metadata = models.get(model.id);
    return {
      ...model, reasoning: metadata?.reasoning, variant: metadata?.variant,
      service_tiers: metadata?.service_tiers, default_service_tier: metadata?.default_service_tier,
    };
  });
  const endpointCatalog = credential.connection === "custom_endpoint" && Boolean(catalog?.length);
  if (scopedCatalog || endpointCatalog) {
    const verified = new Set(credential.verified_models_by_runtime?.[runtimeKey] || []);
    const rows = new Map((catalog || []).map(model => [model.id, model]));
    if (!endpointCatalog) for (const id of verified) if (!rows.has(id)) rows.set(id, { id, label: id });
    return {
      ...credential, runtime_instance: runtimeKey,
      default_model: endpointCatalog && !rows.has(credential.default_model || "") ? "" : credential.default_model,
      models: scope([...rows.values()].filter(model => verified.has(model.id))),
      candidate_models: scope([...rows.values()].filter(model => !verified.has(model.id))),
    };
  }
  if (credential.verified_models_by_runtime) {
    const verified = new Set(credential.verified_models_by_runtime[runtimeKey] || []);
    const rows = new Map([...credential.models, ...credential.candidate_models, ...(catalog || [])]
      .map(model => [model.id, model]));
    for (const id of verified) {
      if (!rows.has(id)) rows.set(id, { id, label: id });
    }
    return {
      ...credential, runtime_instance: runtimeKey,
      models: scope([...rows.values()].filter(model => verified.has(model.id))),
      candidate_models: scope([...rows.values()].filter(model => !verified.has(model.id))),
    };
  }
  return { ...credential, runtime_instance: runtimeKey, models: scope(credential.models), candidate_models: scope(credential.candidate_models) };
}

export function modelEffortLevels(model: ConversationCredentialModel | null | undefined): string[] {
  const reasoning = model?.reasoning;
  if (!reasoning || reasoning.supported === false) return [];
  const levels = [...new Set((reasoning.levels || []).map(value => value.trim()).filter(Boolean))]
    .filter(value => value !== "default");
  if (reasoning.kind === "variant") return levels;
  return levels.sort((a, b) => {
    return effortOrder(a) - effortOrder(b);
  });
}

export const MODEL_THINKING_PARAMETERS = ["effort", "thinking"] as const;

export function modelThinkingControls(
  model: ConversationCredentialModel | undefined,
  models: ConversationCredentialModel[],
  effort: string,
) {
  const nativeLevels = modelEffortLevels(model);
  const nativeDefault = model?.reasoning?.default || "";
  const native = {
    choices: nativeLevels.map(value => ({ value, label: value, model: undefined as ConversationCredentialModel | undefined })),
    value: effort === "default" ? "" : effort,
    defaultValue: nativeLevels.includes(nativeDefault) ? nativeDefault : "",
    variant: false,
    canReset: true,
  };
  if (nativeLevels.length || !model?.variant
    || !MODEL_THINKING_PARAMETERS.some(key => model.variant?.parameters[key] !== undefined)) return native;

  const parameters = model.variant.parameters;
  const keyOf = (parameters: Record<string, string>) => JSON.stringify([
    parameters.thinking || "",
    parameters.thinking === "No Thinking" ? "" : parameters.effort || "",
  ]);
  const family = models.filter(candidate => candidate.variant?.family_id === model.variant?.family_id);
  const choices = new Map<string, { value: string; label: string; model: ConversationCredentialModel }>();
  for (const candidate of family) {
    const next = candidate.variant!.parameters;
    if (!variantParameterKeys([model, candidate]).every(key => MODEL_THINKING_PARAMETERS.some(parameter => parameter === key)
      || next[key] === parameters[key])) continue;
    const value = keyOf(next);
    const kept = choices.get(value);
    if (!kept || candidate.id === model.id || (kept.model.id !== model.id
      && next.effort === parameters.effort && kept.model.variant?.parameters.effort !== parameters.effort)) {
      choices.set(value, { value, label: next.thinking === "No Thinking" ? next.thinking : next.effort || next.thinking, model: candidate });
    }
  }
  const declaredDefault = family.find(candidate => candidate.variant?.default);
  const defaultValue = declaredDefault ? keyOf(declaredDefault.variant!.parameters) : "";
  const value = keyOf(parameters);
  return {
    choices: [...choices.values()].sort((a, b) => effortOrder(a.label) - effortOrder(b.label)),
    value,
    defaultValue: choices.has(defaultValue) ? defaultValue : "",
    variant: true,
    canReset: choices.has(defaultValue) && value !== defaultValue,
  };
}

export function validModelEffort(model: ConversationCredentialModel | null | undefined, effort: string): boolean {
  return !effort || effort === "default" || modelEffortLevels(model).includes(effort);
}

export function modelServiceTiers(model: ConversationCredentialModel | null | undefined): ConversationServiceTier[] {
  return model?.service_tiers || [];
}

export function effectiveServiceTier(model: ConversationCredentialModel | null | undefined, tier: string): string {
  return tier && modelServiceTiers(model).some(row => row.id === tier) ? tier : "";
}

const SERVICE_TIER_KEY = "muteki.chat.service-tier.v1";

export function readPreferredServiceTier(): string {
  try { return localStorage.getItem(SERVICE_TIER_KEY) || ""; } catch { return ""; }
}

export function rememberServiceTier(tier: string): void {
  try {
    if (tier) localStorage.setItem(SERVICE_TIER_KEY, tier);
    else localStorage.removeItem(SERVICE_TIER_KEY);
  } catch {  }
}

function readMemory(): Record<string, string> {
  try {
    const value: unknown = JSON.parse(localStorage.getItem(MEMORY_KEY) || "{}");
    return value && typeof value === "object" && !Array.isArray(value)
      ? Object.fromEntries(Object.entries(value).filter((entry): entry is [string, string] => typeof entry[1] === "string"))
      : {};
  } catch { return {}; }
}

export function rememberModelEffort(credentialId: string, model: ConversationCredentialModel, effort: string): void {
  if (!credentialId || !validModelEffort(model, effort)) return;
  try {
    const memory = readMemory();
    const key = JSON.stringify([credentialId, model.id]);
    delete memory[key];
    memory[key] = effort === "default" ? "" : effort;
    localStorage.setItem(MEMORY_KEY, JSON.stringify(Object.fromEntries(Object.entries(memory).slice(-128))));
  } catch {  }
}

export function rememberedModelEffort(credentialId: string, model: ConversationCredentialModel | undefined): string {
  if (!model) return "";
  const effort = readMemory()[JSON.stringify([credentialId, model.id])] || "";
  return validModelEffort(model, effort) ? effort : "";
}
