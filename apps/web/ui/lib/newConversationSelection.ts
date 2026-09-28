import type { ChatDefaultModel, ChatLastSelection, ProjectConvDefaults } from "./conversationDefaults";
import type { ComposerLaunchSelection } from "./composerRestoreBinding";
import type { ConversationCredential, RuntimeInstance } from "./useConversation";
import { allModelIds, credentialAvailable, pickRuntimeForEngine } from "./conversationReadiness";
import { credentialForRuntime, rememberedModelEffort, validModelEffort } from "./modelReasoning";

/** Resolve fresh-chat settings once catalogs are ready; saved drafts bypass this. */
export function resolveNewConversationSelection(input: {
  credentials: ConversationCredential[];
  runtimes: RuntimeInstance[];
  recent: ChatLastSelection | null;
  configured: ChatDefaultModel | null;
  project?: Partial<ProjectConvDefaults>;
}): ComposerLaunchSelection | null {
  const available = input.credentials.filter(credentialAvailable);
  const preferences = [input.project, input.recent, input.configured];
  const preferred = preferences.find((item) => item?.credentialId
    && available.some((row) => row.id === item.credentialId));
  const credential = available.find((row) => row.id === preferred?.credentialId) || available[0];
  if (!credential) return null;
  const ids = allModelIds(credential);
  const model = preferences.find((item) => item?.modelId
    && (!item.credentialId || item.credentialId === credential.id)
    && ids.includes(item.modelId))?.modelId
    || (credential.default_model && ids.includes(credential.default_model) ? credential.default_model : "")
    || ids[0] || "";
  const recent = input.recent;
  const runtime = input.runtimes.find((row) => row.key === recent?.runtimeKey
    && row.engine === credential.engine && row.enabled !== false)
    || input.runtimes.find((row) => row.key === pickRuntimeForEngine(credential.engine, input.runtimes)?.key);
  const scoped = credentialForRuntime(credential, runtime?.key || "");
  const selectedModel = [...scoped.models, ...scoped.candidate_models].find((row) => row.id === model);
  const sameModel = recent?.credentialId === credential.id && recent.modelId === model;
  const effort = input.project?.effort
    ?? (sameModel ? recent.effort : rememberedModelEffort(`${credential.id}:${runtime?.key || ""}`, selectedModel));
  const requestedAccess = input.project?.accessMode || recent?.accessMode || "supervised";
  const modes = runtime?.access_modes || runtime?.health?.capabilities?.access_modes;
  const accessMode = Array.isArray(modes) && modes.length && !modes.includes(requestedAccess)
    ? modes.includes("supervised") ? "supervised" : String(modes[0])
    : requestedAccess;
  return {
    credentialId: credential.id,
    runtimeKey: runtime?.key || "",
    model,
    effort: validModelEffort(selectedModel, effort) && effort !== "default" ? effort : "",
    accessMode,
  };
}
