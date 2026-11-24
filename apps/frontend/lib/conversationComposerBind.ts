
import {
  allModelIds,
  credentialAvailable,
  pickRuntimeForEngine,
  type ReadinessCredential,
  type ReadinessRuntime,
} from "./conversationReadiness";

export type ComposerBindSource = "draft" | "view" | "preferred" | "empty";

export interface ComposerBindSelection {
  credentialId: string;
  modelId: string;
  runtimeKey: string;
  source: ComposerBindSource;
}

export interface ComposerBindSavedDefault {
  credentialId: string;
  modelId: string;
}

function pickPreferredCredential(
  credentials: ReadinessCredential[],
  saved: ComposerBindSavedDefault | null | undefined,
): ReadinessCredential | undefined {
  const available = credentials.filter(credentialAvailable);
  if (!available.length) return undefined;
  if (saved?.credentialId) {
    const matched = available.find((row) => row.id === saved.credentialId);
    if (matched) return matched;
  }
  return available[0];
}

function pickPreferredModelId(
  credential: ReadinessCredential,
  saved: ComposerBindSavedDefault | null | undefined,
): string {
  const modelIds = allModelIds(credential);
  if (
    saved
    && saved.credentialId === credential.id
    && modelIds.includes(saved.modelId)
  ) {
    return saved.modelId;
  }
  const defaultModel = String(credential.default_model || "").trim();
  if (defaultModel && defaultModel !== "default" && modelIds.includes(defaultModel)) {
    return defaultModel;
  }
  return modelIds[0] || "";
}

function runtimeKeyForCredential(
  credential: ReadinessCredential | undefined,
  runtimes: ReadinessRuntime[],
  preferredKey = "",
): string {
  if (preferredKey && credential && runtimes.some((row) => (
    row.key === preferredKey && row.engine === credential.engine && row.enabled !== false
  ))) return preferredKey;
  if (!credential) return "";
  return pickRuntimeForEngine(credential.engine, runtimes)?.key || "";
}

export function resolveComposerRuntimeBind(input: {
  draftCredentialId?: string;
  draftModel?: string;
  draftRuntimeKey?: string;
  viewCredentialId?: string;
  viewModel?: string;
  viewRuntimeKey?: string;
  credentials: ReadinessCredential[];
  runtimes: ReadinessRuntime[];
  savedDefault?: ComposerBindSavedDefault | null;
}): ComposerBindSelection {
  const draftCredentialId = String(input.draftCredentialId || "").trim();
  const draftModel = String(input.draftModel || "").trim();
  const draftRuntimeKey = String(input.draftRuntimeKey || "").trim();
  const viewCredentialId = String(input.viewCredentialId || "").trim();
  const viewModel = String(input.viewModel || "").trim();
  const viewRuntimeKey = String(input.viewRuntimeKey || "").trim();
  const credentials = input.credentials || [];
  const runtimes = input.runtimes || [];

  if (draftCredentialId) {
    const credential = credentials.find((row) => row.id === draftCredentialId);
    return {
      credentialId: draftCredentialId,
      modelId: draftModel
        || (credential ? pickPreferredModelId(credential, input.savedDefault) : ""),
      runtimeKey: draftRuntimeKey || runtimeKeyForCredential(credential, runtimes, viewRuntimeKey),
      source: "draft",
    };
  }

  if (viewCredentialId) {
    const credential = credentials.find((row) => row.id === viewCredentialId);
    return {
      credentialId: viewCredentialId,
      modelId: viewModel
        || (credential ? pickPreferredModelId(credential, input.savedDefault) : ""),
      runtimeKey: runtimeKeyForCredential(credential, runtimes, viewRuntimeKey),
      source: "view",
    };
  }

  const extensionRuntime = runtimes.find((row) => row.key === viewRuntimeKey && row.origin === "extension");
  if (extensionRuntime) {
    const credential = credentials.find((row) => row.engine === extensionRuntime.engine && credentialAvailable(row));
    return { credentialId: credential?.id || "", modelId: viewModel || (credential ? pickPreferredModelId(credential, input.savedDefault) : ""),
      runtimeKey: viewRuntimeKey, source: "view" };
  }

  const preferred = pickPreferredCredential(credentials, input.savedDefault);
  if (!preferred) {
    return {
      credentialId: "",
      modelId: "",
      runtimeKey: "",
      source: "empty",
    };
  }

  return {
    credentialId: preferred.id,
    modelId: pickPreferredModelId(preferred, input.savedDefault),
    runtimeKey: runtimeKeyForCredential(preferred, runtimes),
    source: "preferred",
  };
}
