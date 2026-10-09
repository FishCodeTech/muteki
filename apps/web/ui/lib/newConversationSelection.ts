import type { ChatDefaultModel, ChatLastSelection, ProjectConvDefaults } from "./conversationDefaults";
import type { ComposerLaunchSelection } from "./composerRestoreBinding";
import type { ConversationCredential, RuntimeInstance } from "./useConversation";
import { allModelIds, credentialAvailable, pickRuntimeForEngine } from "./conversationReadiness";
import { readChatPreferences } from "./chatPreferences";
import { credentialForRuntime, rememberedModelEffort, validModelEffort } from "./modelReasoning";
import { descriptorForEngine, runtimeScopesModelCatalog, type ProviderDescriptorCatalog } from "./providerDescriptors";

/** Resolve fresh-chat settings once catalogs are ready; saved drafts bypass this. */
export function resolveNewConversationSelection(input: {
  credentials: ConversationCredential[];
  runtimes: RuntimeInstance[];
  recent: ChatLastSelection | null;
  configured: ChatDefaultModel | null;
  project?: Partial<ProjectConvDefaults>;
  descriptors: ProviderDescriptorCatalog | null;
  /** Pick among the engine's default (structured) adapter instances first. */
  preferDefaultAdapter?: boolean;
  /** Defaults to the stored chat preference. */
  globalAccessMode?: string;
}): ComposerLaunchSelection | null {
  const available = input.credentials.filter(credentialAvailable);
  const preferences = [input.project, input.configured, input.recent];
  const preferred = preferences.find((item) => item?.credentialId
    && available.some((row) => row.id === item.credentialId));
  const credential = available.find((row) => row.id === preferred?.credentialId) || available[0];
  if (!credential) return null;
  const recent = input.recent;
  const defaultAdapter = input.preferDefaultAdapter
    ? descriptorForEngine(input.descriptors, credential.engine)?.identity.default_adapter_id : "";
  const native = defaultAdapter
    ? input.runtimes.filter((row) => row.adapter_id === defaultAdapter && row.enabled !== false) : [];
  const runtime = input.runtimes.find((row) => row.key === recent?.runtimeKey
    && row.engine === credential.engine && row.enabled !== false)
    || input.runtimes.find((row) => row.key === pickRuntimeForEngine(credential.engine, native.length ? native : input.runtimes)?.key);
  const scoped = credentialForRuntime(
    credential, runtime?.key || "", runtimeScopesModelCatalog(input.descriptors, runtime?.key));
  const ids = allModelIds(scoped);
  const model = preferences.find((item) => item?.modelId
    && (!item.credentialId || item.credentialId === credential.id)
    && ids.includes(item.modelId))?.modelId
    || (scoped.default_model && ids.includes(scoped.default_model) ? scoped.default_model : "")
    || ids[0] || "";
  const selectedModel = [...scoped.models, ...scoped.candidate_models].find((row) => row.id === model);
  const sameModel = recent?.credentialId === credential.id && recent.modelId === model;
  const effort = input.project?.effort
    ?? (sameModel ? recent.effort : rememberedModelEffort(`${credential.id}:${runtime?.key || ""}`, selectedModel));
  const projectCredentialMatches = !input.project?.credentialId || input.project.credentialId === credential.id;
  // Permission defaults apply only with their chosen endpoint. Ordinary new chats
  // never inherit unrestricted permissions merely from a recent conversation.
  // The global default is an explicit user setting, applied only where the runtime advertises that mode.
  const globalAccess = input.globalAccessMode ?? readChatPreferences().defaultAccessMode;
  const modes = runtime?.access_modes || (runtime?.health?.capabilities as { access_modes?: unknown } | undefined)?.access_modes;
  const globalSupported = Boolean(globalAccess && Array.isArray(modes) && modes.includes(globalAccess));
  const fallbackAccess = globalSupported ? globalAccess : "supervised";
  const requestedAccess = projectCredentialMatches && input.project?.accessMode ? input.project.accessMode : fallbackAccess;
  // Keep the chosen permission explicit. Unsupported supervised mode must be
  // reported by readiness/the service, never upgraded to the first supported mode.
  const accessMode = requestedAccess;
  return {
    credentialId: credential.id,
    runtimeKey: runtime?.key || "",
    model,
    effort: validModelEffort(selectedModel, effort) && effort !== "default" ? effort : "",
    accessMode,
  };
}
