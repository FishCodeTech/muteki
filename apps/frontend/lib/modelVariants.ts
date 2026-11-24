import type { ConversationCredentialModel } from "./useConversation";

const PARAMETER_LABELS: Record<string, [string, string]> = {
  lead: ["主模型", "Lead"],
  effort: ["思考强度", "Thinking effort"],
  sidekick: ["协作模型", "Sidekick"],
  speed: ["速度", "Speed"],
  context: ["上下文", "Context"],
  thinking: ["思考", "Thinking"],
  configuration: ["配置", "Configuration"],
};

export function variantParameterLabel(key: string, en: boolean): string {
  return PARAMETER_LABELS[key]?.[en ? 1 : 0] || key;
}

export function variantParameterKeys(models: ConversationCredentialModel[]): string[] {
  const keys = new Set(models.flatMap(model => Object.keys(model.variant?.parameters || {})));
  return [...Object.keys(PARAMETER_LABELS).filter(key => keys.has(key)), ...[...keys].filter(key => !(key in PARAMETER_LABELS))];
}

export function modelVariantSummary(model: ConversationCredentialModel): string {
  return variantParameterKeys([model]).map(key => model.variant?.parameters[key]).filter(Boolean).join(" · ");
}

export function modelVariantForParameter(
  models: ConversationCredentialModel[], current: ConversationCredentialModel, key: string, value: string,
): ConversationCredentialModel | undefined {
  if (!current.variant) return undefined;
  const parameters = current.variant.parameters;
  const candidates = models.filter(model => model.variant?.family_id === current.variant?.family_id
    && model.variant?.parameters[key] === value);
  const keys = variantParameterKeys([current, ...candidates]);
  const position = keys.indexOf(key);
  const fixed = keys.slice(0, position);
  const kept = keys.slice(position + 1);
  let best: ConversationCredentialModel | undefined;
  let bestScore: boolean[] = [];
  for (const model of candidates) {
    if (!fixed.every(parameter => model.variant?.parameters[parameter] === parameters[parameter])) continue;
    const score = kept.map(parameter => model.variant?.parameters[parameter] === parameters[parameter]);
    const first = score.findIndex((same, index) => same !== bestScore[index]);
    if (!best || (first >= 0 && score[first])) { best = model; bestScore = score; }
  }
  return best;
}

export function hasUnreachableModelVariants(
  models: ConversationCredentialModel[], current: ConversationCredentialModel,
): boolean {
  if (!current.variant) return false;
  const family = models.filter(model => model.variant?.family_id === current.variant?.family_id);
  const keys = variantParameterKeys(family);
  const reached = new Set([current.id]);
  const pending = [current];
  for (let index = 0; index < pending.length; index++) {
    const source = pending[index];
    for (const target of family) {
      if (reached.has(target.id)) continue;
      const changes = keys.filter(key => source.variant?.parameters[key] !== target.variant?.parameters[key]).length;
      if (changes > 1) continue;
      reached.add(target.id);
      pending.push(target);
    }
  }
  return family.some(model => !reached.has(model.id));
}

export function groupModelVariantRows<T extends { credential: { id: string }; model: ConversationCredentialModel }>(
  rows: T[], selectedCredentialId: string, selectedModel: string,
): T[] {
  const groups = new Map<string, T>();
  for (const row of rows) {
    const key = JSON.stringify([row.credential.id, row.model.variant ? "family" : "model", row.model.variant?.family_id || row.model.id]);
    const kept = groups.get(key);
    const selected = (candidate: T) => candidate.credential.id === selectedCredentialId && candidate.model.id === selectedModel;
    if (!kept || selected(row) || (!selected(kept) && row.model.variant?.default && !kept.model.variant?.default)) groups.set(key, row);
  }
  return [...groups.values()];
}
