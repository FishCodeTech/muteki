import type { DiscoveredCredentialModel, ModelCapabilityConfig } from "@/lib/useRun";

export type { DiscoveredCredentialModel, ModelCapabilityConfig };

export interface PiModelRecommendation {
  context_window: number;
  max_output_tokens: number;
  reasoning?: boolean;
  input?: string[];
  family: string;
  source: "endpoint" | "recommended";
}

export interface PiModelCapabilityDraft {
  contextWindow: string;
  maxOutputTokens: string;
  reasoning: boolean;
  imageInput: boolean;
}

export const CONTEXT_WINDOW_PRESETS: ReadonlyArray<{ value: number; label: string }> = [
  { value: 128_000, label: "128K" },
  { value: 200_000, label: "200K" },
  { value: 256_000, label: "256K" },
  { value: 512_000, label: "512K" },
  { value: 1_024_000, label: "1M" },
];

const KNOWN_PLAN_ENDPOINTS = new Set([
  "https://ark.cn-beijing.volces.com/api/plan",
  "https://ark.cn-beijing.volces.com/api/plan/v3",
  "https://qianfan.baidubce.com/v2/tokenplan/personal",
]);

function normalizeModelSlug(modelId: string): string {
  const trimmed = modelId.trim().toLowerCase();
  if (!trimmed) return "";
  const slashIdx = trimmed.lastIndexOf("/");
  const bare = slashIdx >= 0 ? trimmed.slice(slashIdx + 1) : trimmed;
  return bare.replace(/:(?:cloud|free|latest)$/i, "");
}

function recommendFromFamily(slug: string, baseUrl?: string): Omit<PiModelRecommendation, "source"> | null {
  if (!slug) return null;
  const endpoint = (baseUrl || "").trim().replace(/\/+$/, "");
  if (KNOWN_PLAN_ENDPOINTS.has(endpoint)) {
    if (slug === "kimi-k3") {
      return { context_window: 1_024_000, max_output_tokens: 65_536, reasoning: true, input: ["text", "image"], family: "Volces/Qianfan Plan" };
    }
    if (slug === "glm-5.3") {
      return { context_window: 1_024_000, max_output_tokens: 65_536, reasoning: true, input: ["text"], family: "Volces/Qianfan Plan" };
    }
    if (slug === "deepseek-v4.1-flash") {
      return { context_window: 1_024_000, max_output_tokens: 393_216, reasoning: true, input: ["text", "image"], family: "Volces/Qianfan Plan" };
    }
    if (slug === "deepseek-v4-flash" || slug === "deepseek-v4-pro") {
      return { context_window: 1_024_000, max_output_tokens: 393_216, reasoning: true, input: ["text"], family: "Volces/Qianfan Plan" };
    }
  }

  if (slug.startsWith("deepseek-v4.1-flash")) {
    return { context_window: 1_024_000, max_output_tokens: 65_536, reasoning: true, input: ["text", "image"], family: "DeepSeek V4.1" };
  }
  if (slug.startsWith("deepseek-v4") || slug.startsWith("deepseek-flash") || slug.startsWith("deepseek-pro")) {
    return { context_window: 1_024_000, max_output_tokens: 65_536, reasoning: true, input: ["text"], family: "DeepSeek V4" };
  }
  if (slug.startsWith("deepseek-r1") || slug.startsWith("deepseek-reasoner")) {
    return { context_window: 131_072, max_output_tokens: 65_536, reasoning: true, input: ["text"], family: "DeepSeek R1" };
  }
  if (slug.startsWith("deepseek-v3.1") || slug.startsWith("deepseek-v3.2")) {
    return { context_window: 131_072, max_output_tokens: 65_536, reasoning: true, input: ["text"], family: "DeepSeek V3" };
  }
  if (slug.startsWith("deepseek-v3") || slug.startsWith("deepseek-chat")) {
    return { context_window: 131_072, max_output_tokens: 8_192, reasoning: false, input: ["text"], family: "DeepSeek V3" };
  }

  if (/^claude-(?:[a-z-]+-)?(?:4|5)(?:[.-]|$)/.test(slug) || /^claude-(?:sonnet|opus|haiku|fable)-(?:4|5)/.test(slug)) {
    const is1m = /(?:^|[-_[\s])1m(?:$|[-_\]\s])/i.test(slug);
    return {
      context_window: is1m ? 1_000_000 : 200_000,
      max_output_tokens: 64_000,
      reasoning: true,
      input: ["text", "image"],
      family: "Claude 4/5",
    };
  }
  if (slug.startsWith("claude-3-7") || slug.startsWith("claude-3.7")) {
    return { context_window: 200_000, max_output_tokens: 64_000, reasoning: true, input: ["text", "image"], family: "Claude 3.7" };
  }
  if (slug.startsWith("claude-3-5") || slug.startsWith("claude-3.5")) {
    return { context_window: 200_000, max_output_tokens: 8_192, reasoning: false, input: ["text", "image"], family: "Claude 3.5" };
  }

  if (slug.startsWith("gpt-5") || slug.startsWith("gpt-6")) {
    return { context_window: 400_000, max_output_tokens: 128_000, reasoning: true, input: ["text", "image"], family: "GPT-5/6" };
  }
  if (slug.startsWith("gpt-4.1")) {
    return { context_window: 1_047_576, max_output_tokens: 32_768, reasoning: false, input: ["text", "image"], family: "GPT-4.1" };
  }
  if (slug.startsWith("gpt-4o")) {
    return { context_window: 128_000, max_output_tokens: 16_384, reasoning: false, input: ["text", "image"], family: "GPT-4o" };
  }
  if (/^o[134](?:$|-)/.test(slug)) {
    return { context_window: 200_000, max_output_tokens: 100_000, reasoning: true, input: ["text", "image"], family: "OpenAI o-series" };
  }

  if (slug.startsWith("gemini-2.5") || slug.startsWith("gemini-3")) {
    return { context_window: 1_048_576, max_output_tokens: 65_536, reasoning: true, input: ["text", "image"], family: "Gemini 2.5/3" };
  }
  if (slug.startsWith("gemini-2.0") || slug.startsWith("gemini-1.5")) {
    return { context_window: 1_048_576, max_output_tokens: 8_192, reasoning: false, input: ["text", "image"], family: "Gemini" };
  }

  if (slug.startsWith("kimi-k3") || slug.startsWith("kimi-k2.5") || slug.startsWith("kimi-for-coding")) {
    return { context_window: 262_144, max_output_tokens: 65_536, reasoning: true, input: ["text", "image"], family: "Kimi" };
  }
  if (slug.startsWith("kimi-k2")) {
    return { context_window: 262_144, max_output_tokens: 32_768, reasoning: true, input: ["text"], family: "Kimi" };
  }

  if (slug.startsWith("glm-5") || slug.startsWith("glm-4.5") || slug.startsWith("glm-4.6") || slug.startsWith("glm-4.7")) {
    return { context_window: 200_000, max_output_tokens: 65_536, reasoning: true, input: ["text"], family: "GLM" };
  }

  if (slug.startsWith("qwen3")) {
    return { context_window: 262_144, max_output_tokens: 65_536, reasoning: true, input: ["text"], family: "Qwen3" };
  }
  if (slug.startsWith("qwq") || slug.startsWith("qwen2.5")) {
    return { context_window: 131_072, max_output_tokens: 32_768, reasoning: slug.startsWith("qwq"), input: ["text"], family: "Qwen" };
  }

  if (slug.startsWith("minimax-m3") || slug.startsWith("minimax-m2.5")) {
    return { context_window: 1_000_000, max_output_tokens: 65_536, reasoning: true, input: ["text"], family: "MiniMax" };
  }
  if (slug.startsWith("minimax-m2")) {
    return { context_window: 204_800, max_output_tokens: 65_536, reasoning: true, input: ["text"], family: "MiniMax" };
  }

  if (slug.startsWith("grok-4")) {
    return { context_window: 256_000, max_output_tokens: 65_536, reasoning: true, input: ["text", "image"], family: "Grok 4" };
  }
  if (slug.startsWith("grok-3")) {
    return { context_window: 131_072, max_output_tokens: 32_768, reasoning: true, input: ["text"], family: "Grok 3" };
  }

  if (slug.startsWith("doubao-seed") || slug.startsWith("doubao-1-5") || slug.startsWith("doubao-1.5")) {
    return { context_window: 256_000, max_output_tokens: 32_768, reasoning: true, input: ["text", "image"], family: "Doubao" };
  }

  const sizeMatch = slug.match(/(?:^|[-_[\s])(1m|512k|256k|200k|128k)(?:$|[-_\]\s])/i);
  if (sizeMatch) {
    const tag = sizeMatch[1].toLowerCase();
    const map: Record<string, number> = {
      "1m": 1_000_000,
      "512k": 524_288,
      "256k": 262_144,
      "200k": 200_000,
      "128k": 131_072,
    };
    const contextWindow = map[tag];
    return {
      context_window: contextWindow,
      max_output_tokens: tag === "128k" ? 16_384 : 32_768,
      family: `${tag.toUpperCase()} 模型`,
    };
  }

  return null;
}

export function recommendPiModelCapabilities(
  modelId: string,
  baseUrl?: string,
  discovered?: DiscoveredCredentialModel | null,
): PiModelRecommendation | null {
  const slug = normalizeModelSlug(modelId);
  const familyMatch = recommendFromFamily(slug, baseUrl);
  const endpointContext = typeof discovered?.context_window === "number" && discovered.context_window > 0
    ? Math.trunc(discovered.context_window)
    : undefined;
  const endpointOutput = typeof discovered?.max_output_tokens === "number" && discovered.max_output_tokens > 0
    ? Math.trunc(discovered.max_output_tokens)
    : undefined;
  if (endpointContext !== undefined || endpointOutput !== undefined) {
    const contextWindow = endpointContext ?? familyMatch?.context_window ?? 128_000;
    const rawOutput = endpointOutput ?? familyMatch?.max_output_tokens ?? 16_384;
    const maxOutputTokens = Math.min(contextWindow, rawOutput);
    return {
      context_window: contextWindow,
      max_output_tokens: maxOutputTokens,
      reasoning: discovered?.reasoning ?? familyMatch?.reasoning,
      input: discovered?.input?.length ? discovered.input : familyMatch?.input,
      family: "端点返回",
      source: "endpoint",
    };
  }
  if (!familyMatch) return null;
  return {
    ...familyMatch,
    max_output_tokens: Math.min(familyMatch.context_window, familyMatch.max_output_tokens),
    source: "recommended",
  };
}

export function formatTokenCount(tokens?: number | null): string {
  if (tokens === null || tokens === undefined || !Number.isFinite(tokens) || tokens <= 0) return "";
  if (tokens >= 1_000_000) {
    if ([1_000_000, 1_024_000, 1_047_576, 1_048_576].includes(tokens)) return "1M";
    return `${(tokens / 1_000_000).toFixed(1).replace(/\.0$/, "")}M`;
  }
  if (tokens >= 1_000) {
    if (tokens % 1000 === 0) return `${tokens / 1000}K`;
    if (tokens % 1024 === 0) return `${tokens / 1024}K`;
    return `${Math.round(tokens / 1000)}K`;
  }
  return String(tokens);
}

export function draftFromCapability(cap?: ModelCapabilityConfig | null): PiModelCapabilityDraft {
  return {
    contextWindow: cap?.context_window ? String(cap.context_window) : "",
    maxOutputTokens: cap?.max_output_tokens ? String(cap.max_output_tokens) : "",
    reasoning: Boolean(cap?.reasoning),
    imageInput: Array.isArray(cap?.input) && cap.input.includes("image"),
  };
}

export function draftFromRecommendation(rec: PiModelRecommendation): PiModelCapabilityDraft {
  return {
    contextWindow: String(rec.context_window),
    maxOutputTokens: String(rec.max_output_tokens),
    reasoning: Boolean(rec.reasoning),
    imageInput: Array.isArray(rec.input) && rec.input.includes("image"),
  };
}

export function isCapabilityDraftEmpty(draft?: PiModelCapabilityDraft | null): boolean {
  if (!draft) return true;
  return !draft.contextWindow.trim() && !draft.maxOutputTokens.trim() && !draft.reasoning && !draft.imageInput;
}

export function capabilityDraftEqualsRecommendation(
  draft: PiModelCapabilityDraft | undefined,
  rec: PiModelRecommendation | null,
): boolean {
  if (!draft || !rec) return false;
  return (
    draft.contextWindow.trim() === String(rec.context_window)
    && draft.maxOutputTokens.trim() === String(rec.max_output_tokens)
    && draft.reasoning === Boolean(rec.reasoning)
    && draft.imageInput === (Array.isArray(rec.input) && rec.input.includes("image"))
  );
}

function parsePositiveIntInput(raw: string, model: string, fieldLabel: string): number | null | string {
  const trimmed = raw.trim();
  if (!trimmed) return null;
  if (!/^\d+$/.test(trimmed)) return `模型 ${model} 的${fieldLabel}必须为正整数。`;
  const parsed = Number(trimmed);
  if (!Number.isSafeInteger(parsed) || parsed <= 0) return `模型 ${model} 的${fieldLabel}必须大于 0。`;
  return parsed;
}

export function buildModelCapabilitiesPayload(
  drafts: Record<string, PiModelCapabilityDraft>,
  existing?: Record<string, ModelCapabilityConfig>,
): { ok: true; value: Record<string, ModelCapabilityConfig> } | { ok: false; error: string } {
  const result: Record<string, ModelCapabilityConfig> = {};
  const keys = new Set([...Object.keys(drafts), ...Object.keys(existing || {})]);
  for (const rawModel of keys) {
    const model = rawModel.trim();
    if (!model) continue;
    const draft = drafts[rawModel] || drafts[model];
    const prev = existing?.[rawModel] || existing?.[model];
    if (!draft) continue;
    const contextWindow = parsePositiveIntInput(draft.contextWindow, model, "最大上下文");
    if (typeof contextWindow === "string") return { ok: false, error: contextWindow };
    const maxOutputTokens = parsePositiveIntInput(draft.maxOutputTokens, model, "最大输出");
    if (typeof maxOutputTokens === "string") return { ok: false, error: maxOutputTokens };
    const hasThinkingMap = Boolean(prev?.thinkingLevelMap && Object.keys(prev.thinkingLevelMap).length);
    if (contextWindow === null && maxOutputTokens === null && !draft.reasoning && !draft.imageInput && !hasThinkingMap) {
      continue;
    }
    const entry: ModelCapabilityConfig = {
      input: draft.imageInput ? ["text", "image"] : ["text"],
    };
    if (contextWindow !== null) entry.context_window = contextWindow;
    if (maxOutputTokens !== null) entry.max_output_tokens = maxOutputTokens;
    if (draft.reasoning) entry.reasoning = true;
    if (prev?.thinkingLevelMap) entry.thinkingLevelMap = prev.thinkingLevelMap;
    result[model] = entry;
  }
  return { ok: true, value: result };
}
