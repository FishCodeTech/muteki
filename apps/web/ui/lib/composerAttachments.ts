const IMAGE_EXTENSIONS = /\.(avif|bmp|gif|heic|heif|jpe?g|png|svg|webp)$/i;

function extensionForMime(type: string): string {
  if (type === "image/png") return "png";
  if (type === "image/jpeg") return "jpg";
  if (type === "image/gif") return "gif";
  if (type === "image/webp") return "webp";
  if (type === "image/svg+xml") return "svg";
  if (type === "application/pdf") return "pdf";
  if (type === "text/plain") return "txt";
  return "bin";
}

/** True when the attachment should render as an image thumbnail preview. */
export function isComposerImageAttachment(att: {
  name: string;
  file?: File;
  type?: string;
}): boolean {
  const mime = (att.file?.type || att.type || "").trim().toLowerCase();
  if (mime.startsWith("image/")) return true;
  return IMAGE_EXTENSIONS.test(att.name);
}

function needsGeneratedName(file: File): boolean {
  const name = file.name.trim();
  return !name || name === "blob" || name === "image.png";
}

/** Give clipboard / screenshot blobs a stable, unique filename. */
function normalizeComposerFile(file: File, index = 0): File {
  if (!needsGeneratedName(file)) return file;
  const ext = extensionForMime(file.type);
  const stamp = new Date().toISOString().replace(/[:.]/g, "-");
  const suffix = index > 0 ? `-${index + 1}` : "";
  return new File([file], `pasted-${stamp}${suffix}.${ext}`, {
    type: file.type,
    lastModified: file.lastModified,
  });
}

export function filesFromDataTransfer(data: DataTransfer | null | undefined): File[] {
  if (!data) return [];
  if (data.files?.length) {
    return Array.from(data.files);
  }
  return Array.from(data.items || [])
    .filter((item) => item.kind === "file")
    .map((item) => item.getAsFile())
    .filter((file): file is File => file != null);
}

export function filesFromClipboard(data: DataTransfer | null | undefined): File[] {
  return filesFromDataTransfer(data).map((file, index) => normalizeComposerFile(file, index));
}

export function dataTransferHasFiles(data: DataTransfer | null | undefined): boolean {
  if (!data) return false;
  if (data.files?.length) return true;
  return Array.from(data.items || []).some((item) => item.kind === "file");
}

/**
 * Mixed clipboard rule (image + text):
 * - Files / images become formal attachments.
 * - Non-empty plain text is kept and inserted into the composer.
 * - Structured MIME / ⟦ref:…⟧ paste still runs alongside when present.
 * Failures (file-like payload that cannot be read as File) surface a readable hint.
 */
export type ComposerClipboardPastePlan = {
  files: File[];
  text: string;
  /** True when clipboard advertised file/image payload (even if File extraction failed). */
  hadFilePayload: boolean;
  /** Human-readable reason when file payload could not become attachments. */
  hint: string | null;
};

function clipboardTypeList(data: DataTransfer): string[] {
  try {
    return Array.from(data.types || []);
  } catch {
    return [];
  }
}

function clipboardLooksLikeImage(data: DataTransfer): boolean {
  const types = clipboardTypeList(data);
  if (types.some((t) => /^image\//i.test(t))) return true;
  return Array.from(data.items || []).some(
    (item) => item.kind === "file" && /^image\//i.test(item.type || ""),
  );
}

/**
 * Shared paste inspector for structured editor + legacy textarea.
 * Callers: attach `files`, insert `text` when non-empty, show `hint` when set.
 */
export function inspectComposerClipboardPaste(
  data: DataTransfer | null | undefined,
): ComposerClipboardPastePlan {
  if (!data) {
    return { files: [], text: "", hadFilePayload: false, hint: null };
  }
  const hadFilePayload = dataTransferHasFiles(data) || clipboardLooksLikeImage(data);
  const files = filesFromClipboard(data);
  const text = (data.getData("text/plain") || data.getData("text") || "").replace(/\u0000/g, "");
  let hint: string | null = null;
  if (hadFilePayload && files.length === 0) {
    hint = clipboardLooksLikeImage(data)
      ? "剪贴板图片无法读取为附件，请改用拖放或文件选择"
      : "剪贴板文件无法读取为附件，请改用拖放或文件选择";
  }
  return { files, text, hadFilePayload, hint };
}
