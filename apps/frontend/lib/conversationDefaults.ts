import { readUiPreference, writeUiPreferences } from "./uiPreferences";
import { conversationStorageKey, conversationStorageScope } from "./conversationStorageScope";

export type ChatDefaultModel = {
  credentialId: string;
  modelId: string;
};


export type ChatLastSelection = ChatDefaultModel & {
  runtimeKey: string;
  effort: string;
  accessMode: string;
};

const CHAT_LAST_SELECTION_KEY = "muteki.conversation.last-selection.v1";

export function readChatLastSelection(): ChatLastSelection | null {
  if (typeof window === "undefined" || !conversationStorageScope()) return null;
  try {
    const value = JSON.parse(window.localStorage.getItem(conversationStorageKey(CHAT_LAST_SELECTION_KEY)) || "null");
    if (!value || typeof value !== "object") return null;
    if (typeof value.credentialId !== "string" || !value.credentialId.trim()
      || typeof value.modelId !== "string" || !value.modelId.trim()) return null;
    return {
      credentialId: value.credentialId.trim(),
      modelId: value.modelId.trim(),
      runtimeKey: typeof value.runtimeKey === "string" ? value.runtimeKey : "",
      effort: typeof value.effort === "string" && value.effort !== "default" ? value.effort : "",
      accessMode: typeof value.accessMode === "string" && value.accessMode ? value.accessMode : "supervised",
    };
  } catch {
    return null;
  }
}

export function writeChatLastSelection(value: ChatLastSelection): void {
  if (typeof window === "undefined" || !conversationStorageScope() || !value.credentialId || !value.modelId) return;
  try {
    window.localStorage.setItem(conversationStorageKey(CHAT_LAST_SELECTION_KEY), JSON.stringify(value));
  } catch {
  }
}

export function readChatDefaultModel(): ChatDefaultModel | null {
  return readUiPreference("defaultModel", null);
}
export function writeChatDefaultModel(value: ChatDefaultModel | null): boolean {
  return writeUiPreferences({defaultModel: value});
}

export type ProjectConvDefaults = {
  credentialId: string;
  modelId: string;
  effort: string;
};

export function readProjectConvDefaults(
  settings: Record<string, unknown> | undefined | null,
): Partial<ProjectConvDefaults> {
  if (!settings) return {};
  return {
    credentialId: String(settings.conv_default_credential_id ?? "").trim() || undefined,
    modelId: String(settings.conv_default_model ?? "").trim() || undefined,
    effort: String(settings.conv_default_effort ?? "").trim()
      || (settings.conv_default_effort != null
        && (settings.conv_default_credential_id || settings.conv_default_model)
        ? "" : undefined),
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
    };
  }
  return {
    conv_default_credential_id: defaults.credentialId ?? "",
    conv_default_model: defaults.modelId ?? "",
    conv_default_effort: defaults.effort ?? "",
  };
}
