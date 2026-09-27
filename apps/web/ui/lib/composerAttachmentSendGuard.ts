/**
 * #184 — Partial attachment upload failure must block send.
 *
 * After per-file uploads finish, any uploadErrors means we must NOT call
 * conversation.turn.send and must NOT clear composer attachments/draft.
 * Successful rows already carry sha256 so retry skips re-upload; failed
 * chips stay visible for reselect/retry.
 */
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
