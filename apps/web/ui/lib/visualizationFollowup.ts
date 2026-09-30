export class VisualizationSelectionError extends Error {
  constructor(public code: string, message: string) { super(message); this.name = "VisualizationSelectionError"; }
}
/** Widget selections are JSON data. Reject unsupported/oversized values explicitly. */
export function visualizationSelectionText(value: unknown): string {
  if (value == null) return "";
  let encoded: string;
  try { encoded = JSON.stringify(value); }
  catch (error) { throw new VisualizationSelectionError("visualization.selection.serialization_failed", `交互图选择无法序列化；原图和选择仍保留。${error instanceof Error ? error.message : String(error)}`); }
  if (typeof encoded !== "string") throw new VisualizationSelectionError("visualization.selection.invalid", "交互图选择不是可序列化的 JSON，未插入输入框");
  if (new TextEncoder().encode(encoded).byteLength > 16384) throw new VisualizationSelectionError("visualization.selection.too_large", "交互图选择超过 16 KiB，未截断或插入输入框；请缩小选择后重试");
  return "\n\n[交互图当前选择]\n" + encoded;
}
