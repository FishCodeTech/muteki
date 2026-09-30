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
  preferNativeCodex?: boolean;
}): ComposerLaunchSelection | null {
  const available = input.credentials.filter(credentialAvailable);
  const preferences = [input.project, input.configured, input.recent];
  const preferred = preferences.find((item) => item?.credentialId
    && available.some((row) => row.id === item.credentialId));
  const credential = available.find((row) => row.id === preferred?.credentialId) || available[0];
  if (!credential) return null;
  const recent = input.recent;
  const native = input.preferNativeCodex && credential.engine === "codex"
    ? input.runtimes.filter((row) => row.adapter_id === "codex.app_server" && row.enabled !== false) : [];
  const runtime = input.runtimes.find((row) => row.key === recent?.runtimeKey
    && row.engine === credential.engine && row.enabled !== false)
    || input.runtimes.find((row) => row.key === pickRuntimeForEngine(credential.engine, native.length ? native : input.runtimes)?.key);
  const scoped = credentialForRuntime(credential, runtime?.key || "");
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
  const requestedAccess = projectCredentialMatches ? input.project?.accessMode || "supervised" : "supervised";
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
