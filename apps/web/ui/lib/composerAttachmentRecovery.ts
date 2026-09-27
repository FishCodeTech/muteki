/** Recovery action for a composer attachment chip / retry control. */
export function composerAttachmentRecoveryAction(att: {
  needsReselect?: boolean;
  file?: File | null;
  uploadStatus?: "pending" | "uploading" | "done" | "error";
}): "reselect" | "retry" | null {
  if (att.needsReselect || !att.file) return "reselect";
  if (att.uploadStatus === "error") return "retry";
  return null;
}
