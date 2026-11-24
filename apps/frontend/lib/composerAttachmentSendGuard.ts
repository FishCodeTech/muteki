export function attachmentUploadAbortMessage(
  uploadErrors: readonly string[],
): string | null {
  const errors = uploadErrors
    .map((row) => String(row || "").trim())
    .filter(Boolean);
  if (errors.length === 0) return null;
  if (errors.length === 1) return errors[0]!;
  return `部分附件上传失败（${errors.length} 个）：${errors[0]}。请处理失败附件后再发送`;
}
