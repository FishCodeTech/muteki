export function composerAttachmentRecoveryAction(att: {
  needsReselect?: boolean;
  file?: File | null;
  artifactId?: string;
  uploadStatus?: "pending" | "uploading" | "done" | "error";
}): "reselect" | "retry" | null {
  if (att.needsReselect || (!att.file && !att.artifactId)) return "reselect";
  if (att.uploadStatus === "error") return "retry";
  return null;
}
