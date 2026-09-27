/** Browser preference for the default chat credential + model. */

export type ChatDefaultModel = {
  credentialId: string;
  modelId: string;
};

const CHAT_DEFAULT_MODEL_KEY = "muteki.conversation.default-model.v1";

export function readChatDefaultModel(): ChatDefaultModel | null {
  if (typeof window === "undefined") return null;
  try {
    const raw = window.localStorage.getItem(CHAT_DEFAULT_MODEL_KEY);
    if (!raw) return null;
    const parsed = JSON.parse(raw) as Partial<ChatDefaultModel>;
    const credentialId = String(parsed?.credentialId ?? "").trim();
    const modelId = String(parsed?.modelId ?? "").trim();
    if (!credentialId || !modelId) return null;
    return { credentialId, modelId };
  } catch {
    return null;
  }
}

export function writeChatDefaultModel(value: ChatDefaultModel | null): void {
  if (typeof window === "undefined") return;
  try {
    if (!value?.credentialId || !value?.modelId) {
      window.localStorage.removeItem(CHAT_DEFAULT_MODEL_KEY);
      return;
    }
    window.localStorage.setItem(
      CHAT_DEFAULT_MODEL_KEY,
      JSON.stringify({
        credentialId: value.credentialId.trim(),
        modelId: value.modelId.trim(),
      }),
    );
  } catch {
    // Non-blocking: private mode / quota.
  }
}

/**
 * Project-level default model/effort/accessMode stored in project.settings.
 * Keys: conv_default_credential_id, conv_default_model, conv_default_effort,
 *       conv_default_access_mode.
 */
export type ProjectConvDefaults = {
  credentialId: string;
  modelId: string;
  effort: string;
  accessMode: string;
};

export function readProjectConvDefaults(
  settings: Record<string, unknown> | undefined | null,
): Partial<ProjectConvDefaults> {
  if (!settings) return {};
  return {
    credentialId: String(settings.conv_default_credential_id ?? "").trim() || undefined,
    modelId: String(settings.conv_default_model ?? "").trim() || undefined,
    effort: String(settings.conv_default_effort ?? "").trim() || undefined,
    accessMode: String(settings.conv_default_access_mode ?? "").trim() || undefined,
  } as Partial<ProjectConvDefaults>;
}

export function projectConvDefaultsPayload(
  defaults: Partial<ProjectConvDefaults> | null,
): Record<string, string> {
  if (!defaults) {
    return {
      conv_default_credential_id: "",
      conv_default_model: "",
      conv_default_effort: "",
      conv_default_access_mode: "",
    };
  }
  return {
    conv_default_credential_id: defaults.credentialId ?? "",
    conv_default_model: defaults.modelId ?? "",
    conv_default_effort: defaults.effort ?? "",
    conv_default_access_mode: defaults.accessMode ?? "",
  };
}
