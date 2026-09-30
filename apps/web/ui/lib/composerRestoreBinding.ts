import type { ConversationCredential, RuntimeInstance } from "./useConversation";
import { credentialForRuntime, validModelEffort } from "./modelReasoning";
import { allModelIds, credentialAvailable, pickRuntimeForEngine } from "./conversationReadiness";

export type ComposerLaunchSelection = {
  credentialId: string;
  runtimeKey: string;
  model: string;
  effort: string;
  accessMode: string;
};

export type ComposerBindingRestore = {
  restored: boolean;
  selection: ComposerLaunchSelection;
  reason: string;
  legacyRuntime: boolean;
};

/** Restore launch settings as one validated group; never mix old permissions with a retained engine. */
export function resolveComposerBindingRestore(input: {
  snapshot: Partial<ComposerLaunchSelection>;
  current: ComposerLaunchSelection;
  credentials: ConversationCredential[];
  runtimes: RuntimeInstance[];
}): ComposerBindingRestore {
  const retain = (reason: string): ComposerBindingRestore => ({
    restored: false, selection: input.current, reason, legacyRuntime: false,
  });
  const saved = input.snapshot;
  const credential = input.credentials.find((row) => row.id === saved.credentialId);
  if (!credentialAvailable(credential)) return retain("原接入点已不可用");
  if (!credential || !saved.model) return retain("原模型已不可用");
  const legacyRuntime = !saved.runtimeKey;
  const key = saved.runtimeKey || pickRuntimeForEngine(credential.engine, input.runtimes)?.key;
  const runtime = input.runtimes.find((row) => row.key === key);
  if (!runtime || runtime.enabled === false || runtime.engine !== credential.engine) {
    return retain("原 Runtime 实例已不可用或与接入点不匹配");
  }
  const scopedCredential = credentialForRuntime(credential, runtime.key);
  if (!allModelIds(scopedCredential).includes(saved.model)) return retain("原模型不再属于该 Runtime 的目录");
  const effort = saved.effort ?? "";
  const model = [...scopedCredential.models, ...scopedCredential.candidate_models].find((row) => row.id === saved.model);
  if (!validModelEffort(model, effort)) {
    return retain("原推理强度不再受该模型支持");
  }
  const accessMode = saved.accessMode || "supervised";
  const modes = runtime.access_modes || runtime.health?.capabilities?.access_modes;
  if (Array.isArray(modes) && modes.length && !modes.includes(accessMode)) {
    return retain("原权限模式不再受该 Runtime 支持");
  }
  return {
    restored: true,
    selection: { credentialId: credential.id, runtimeKey: runtime.key, model: saved.model, effort, accessMode },
    reason: "",
    legacyRuntime,
  };
}
