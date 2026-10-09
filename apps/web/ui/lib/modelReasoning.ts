import type { ConversationCredential, ConversationCredentialModel, ConversationServiceTier } from "./useConversation";

const MEMORY_KEY = "muteki.chat.model-efforts.v1";
const KNOWN_ORDER = ["off", "none", "on", "minimal", "low", "medium", "high", "xhigh", "extra-high", "max", "ultra"];

/**
 * Project a credential's models onto one Runtime. ``scopedCatalog`` (see
 * providerDescriptors.runtimeScopesModelCatalog) means the runtime reads its
 * own credential-scoped catalog, so credential-level model lists do not apply.
 */
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
      ...model, reasoning: metadata?.reasoning,
      service_tiers: metadata?.service_tiers, default_service_tier: metadata?.default_service_tier,
    };
  });
  if (scopedCatalog) {
    const verified = new Set(credential.verified_models_by_runtime?.[runtimeKey] || []);
    const rows = new Map((catalog || []).map(model => [model.id, model]));
    for (const id of verified) if (!rows.has(id)) rows.set(id, { id, label: id });
    return {
      ...credential, runtime_instance: runtimeKey,
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
  // Named variants belong to the provider; preserve their spelling and order.
  if (reasoning.kind === "variant") return levels;
  return levels.sort((a, b) => {
    const left = KNOWN_ORDER.indexOf(a);
    const right = KNOWN_ORDER.indexOf(b);
    return (left < 0 ? KNOWN_ORDER.length : left) - (right < 0 ? KNOWN_ORDER.length : right);
  });
}

export function validModelEffort(model: ConversationCredentialModel | null | undefined, effort: string): boolean {
  return !effort || effort === "default" || modelEffortLevels(model).includes(effort);
}

export function modelServiceTiers(model: ConversationCredentialModel | null | undefined): ConversationServiceTier[] {
  return model?.service_tiers || [];
}

/** The tier only applies to models whose catalog declares it; others run at standard speed. */
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
  } catch { /* Optional preference storage must not block the composer. */ }
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
  } catch { /* Optional preference storage must not block model selection. */ }
}

export function rememberedModelEffort(credentialId: string, model: ConversationCredentialModel | undefined): string {
  if (!model) return "";
  const effort = readMemory()[JSON.stringify([credentialId, model.id])] || "";
  return validModelEffort(model, effort) ? effort : "";
}
