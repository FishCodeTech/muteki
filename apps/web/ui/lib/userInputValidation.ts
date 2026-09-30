import type { UserInputAnswerEntry } from "./userInputDraftStore";

export interface UserInputQuestionValidation {
  kind: string;
  required: boolean;
  schema?: Record<string, unknown>;
}

/** Validate the primitive constraints preserved by the request contract.
 * The service remains authoritative for the full provider JSON Schema.
 */
export function userInputAnswerError(question: UserInputQuestionValidation, entry?: UserInputAnswerEntry): string | null {
  const originalText = entry?.text || "";
  const text = originalText.trim();
  const values = entry?.values || [];
  if (!text && !values.length) return question.required ? "请填写此项" : null;
  const schema = question.schema || {};
  const kind = schema.type || (question.kind === "number" ? "number" : "string");
  const raw = values.length ? values[0] : originalText;
  if (kind === "number" || kind === "integer") {
    const value = Number(raw);
    if (!raw.trim() || !Number.isFinite(value)) return "请输入有效数字";
    if (kind === "integer" && !Number.isInteger(value)) return "请输入整数";
    if (typeof schema.minimum === "number" && value < schema.minimum) return `不得小于 ${schema.minimum}`;
    if (typeof schema.maximum === "number" && value > schema.maximum) return `不得大于 ${schema.maximum}`;
    if (typeof schema.exclusiveMinimum === "number" && value <= schema.exclusiveMinimum) return `必须大于 ${schema.exclusiveMinimum}`;
    if (typeof schema.exclusiveMaximum === "number" && value >= schema.exclusiveMaximum) return `必须小于 ${schema.exclusiveMaximum}`;
  } else if (kind === "boolean") {
    if (raw !== "true" && raw !== "false") return "请选择 True 或 False";
  } else if (kind === "string") {
    const length = Array.from(raw).length;
    if (typeof schema.minLength === "number" && length < schema.minLength) return `至少需要 ${schema.minLength} 个字符`;
    if (typeof schema.maxLength === "number" && length > schema.maxLength) return `最多允许 ${schema.maxLength} 个字符`;
  } else if (kind === "array") {
    if (typeof schema.minItems === "number" && values.length < schema.minItems) return `至少选择 ${schema.minItems} 项`;
    if (typeof schema.maxItems === "number" && values.length > schema.maxItems) return `最多选择 ${schema.maxItems} 项`;
    if (schema.uniqueItems === true && new Set(values).size !== values.length) return "不能重复选择相同项目";
  }
  return null;
}
