
export type PreviewKind =
  | "image"
  | "pdf"
  | "markdown"
  | "html"
  | "code"
  | "text"
  | "binary"
  | "too_large"
  | "missing";

export type ToolFile =
  | { kind: "workspace"; name: string; path: string; line?: number | null }
  | { kind: "artifact"; name: string; artifact_id: string; media_type: string }
  | { kind: "image_path"; name: string; path: string }
  | { kind: "recorded"; name: string; media_type: string; turn_id: string; stream_seq: number; index: number };

export function toolFileUrl(threadId: string, file: Extract<ToolFile, { kind: "image_path" | "recorded" }>): string {
  const thread = `/api/threads/${encodeURIComponent(threadId)}`;
  if (file.kind === "image_path") {
    return `${thread}/images/raw?path=${encodeURIComponent(file.path)}`;
  }
  return `${thread}/turns/${encodeURIComponent(file.turn_id)}/tool-files/${file.stream_seq}/${file.index}`;
}

export type ResourceLinkTarget =
  | { kind: "external"; href: string }
  | { kind: "workspace"; path: string; line?: number }
  | { kind: "artifact"; artifactId: string }
  | { kind: "legacy_artifact"; sha256: string }
  | { kind: "message"; messageId: string }
  | { kind: "unknown"; href: string };

const IMAGE_EXT = new Set(["png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "svg"]);
const PDF_EXT = new Set(["pdf"]);
const HTML_EXT = new Set(["html", "htm"]);
const MD_EXT = new Set(["md", "markdown", "mdx"]);
const CODE_EXT = new Set([
  "py", "ts", "tsx", "js", "jsx", "mjs", "cjs", "json", "css", "rs", "go",
  "java", "kt", "c", "h", "cpp", "hpp", "cs", "rb", "php", "swift", "sh",
  "bash", "zsh", "yml", "yaml", "toml", "xml", "sql", "r", "lua", "vue",
  "svelte", "diff", "patch", "ini", "cfg",
]);
const TEXT_EXT = new Set(["txt", "log"]);

export function extensionOf(name: string): string {
  const base = name.split(/[?#]/)[0] || "";
  const idx = base.lastIndexOf(".");
  if (idx < 0) return "";
  return base.slice(idx + 1).toLowerCase();
}

export function previewKindFromName(
  name: string,
  mediaType?: string | null,
): PreviewKind {
  const mime = String(mediaType || "").toLowerCase();
  if (mime.startsWith("image/")) return "image";
  if (mime === "application/pdf") return "pdf";
  if (mime === "text/html" || mime === "application/xhtml+xml") return "html";
  if (mime === "text/markdown" || mime.endsWith("+markdown")) return "markdown";
  if (mime.startsWith("text/") || mime === "application/json" || mime === "application/javascript") {
    const ext = extensionOf(name);
    if (MD_EXT.has(ext)) return "markdown";
    if (HTML_EXT.has(ext)) return "html";
    if (CODE_EXT.has(ext)) return "code";
    return "text";
  }
  const ext = extensionOf(name);
  if (IMAGE_EXT.has(ext)) return "image";
  if (PDF_EXT.has(ext)) return "pdf";
  if (HTML_EXT.has(ext)) return "html";
  if (MD_EXT.has(ext)) return "markdown";
  if (CODE_EXT.has(ext)) return "code";
  if (TEXT_EXT.has(ext)) return "text";
  if (mime && mime !== "application/octet-stream") return "binary";
  if (!ext) return "text";
  return "binary";
}

export function workspaceRawUrl(
  threadId: string,
  path: string,
  options?: { download?: boolean },
): string {
  const base = `/api/threads/${encodeURIComponent(threadId)}/workspace/file/raw?path=${encodeURIComponent(path)}`;
  return options?.download ? `${base}&download=true` : base;
}

export function artifactUrl(
  threadId: string,
  reference: string,
  options?: { download?: boolean },
): string {
  const base = `/api/threads/${encodeURIComponent(threadId)}/artifacts/${encodeURIComponent(reference)}`;
  return options?.download ? `${base}?download=true` : base;
}

export function threadImageRawUrl(threadId: string, path: string): string {
  return `/api/threads/${encodeURIComponent(threadId)}/images/raw?path=${encodeURIComponent(path)}`;
}

export function localMarkdownImagePath(src: string): string {
  let path = src.trim();
  if (/^file:/i.test(path)) {
    path = path.replace(/^file:\/\//i, "");
    path = path.replace(/^localhost(?=\/)/i, "");
    if (!path.startsWith("/")) path = `/${path}`;
  }
  try {
    path = decodeURIComponent(path);
  } catch {
  }
  return path;
}

export function markdownImageApiPath(threadId: string, src: string): string {
  const artifact = /^(?:artifact:|muteki-artifact:)([a-f0-9]{64}|[a-f0-9]{128}|[a-f0-9-]{36})$/i.exec(src.trim());
  if (artifact) return artifactUrl(threadId, artifact[1].toLowerCase());
  if (src.startsWith("/api/")) return src;
  return threadImageRawUrl(threadId, localMarkdownImagePath(src));
}

const LINE_SUFFIX = /^(.*?)(?::(\d+)|#L(\d+)(?:-L?\d+)?)$/i;

const SAFE_MARKDOWN_PROTOCOL =
  /^(https?|ircs?|mailto|xmpp|tel|workspace|artifact|file|muteki-file|muteki-artifact)$/i;

export function safeMarkdownUrlTransform(value: string): string {
  const url = String(value || "").trim();
  if (!url) return "";

  const colon = url.indexOf(":");
  const questionMark = url.indexOf("?");
  const numberSign = url.indexOf("#");
  const slash = url.indexOf("/");

  if (
    colon === -1 ||
    (slash !== -1 && colon > slash) ||
    (questionMark !== -1 && colon > questionMark) ||
    (numberSign !== -1 && colon > numberSign)
  ) {
    return url;
  }

  const scheme = url.slice(0, colon);
  return SAFE_MARKDOWN_PROTOCOL.test(scheme) ? url : "";
}

export function parseResourceLink(href: string | undefined | null): ResourceLinkTarget {
  const raw = String(href || "").trim();
  if (!raw) return { kind: "unknown", href: "" };

  if (/^https?:\/\//i.test(raw) || raw.startsWith("//")) {
    return { kind: "external", href: raw.startsWith("//") ? `https:${raw}` : raw };
  }

  if (raw.startsWith("mailto:") || raw.startsWith("tel:")) {
    return { kind: "external", href: raw };
  }

  if (raw.startsWith("#msg-") || raw.startsWith("#message-")) {
    return { kind: "message", messageId: raw.replace(/^#message-/, "#msg-").slice(1) };
  }

  const artifactMatch = /^(?:artifact:|muteki-artifact:)([a-f0-9]{64}|[a-f0-9]{128}|[a-f0-9-]{36})$/i.exec(raw)
    || /^#artifact-([a-f0-9]{64}|[a-f0-9]{128}|[a-f0-9-]{36})$/i.exec(raw);
  if (artifactMatch) {
    const value = artifactMatch[1].toLowerCase();
    return value.length === 64 ? { kind: "legacy_artifact", sha256: value } : { kind: "artifact", artifactId: value };
  }

  let candidate = raw;
  const workspaceScheme = /^(?:workspace:|muteki-file:)/i.test(candidate);
  if (workspaceScheme) {
    candidate = candidate.replace(/^(?:workspace:|muteki-file:)/i, "");
  } else if (/^file:\/\//i.test(candidate)) {
    candidate = candidate
      .replace(/^file:\/\/\//i, "/")
      .replace(/^file:\/\//i, "")
      .replace(/^localhost\//i, "/");
  }

  const lineMatch = LINE_SUFFIX.exec(candidate);
  let path = candidate;
  let line: number | undefined;
  if (lineMatch) {
    path = lineMatch[1];
    line = Number(lineMatch[2] || lineMatch[3] || 0) || undefined;
  }
  try {
    path = decodeURIComponent(path);
  } catch {
  }

  if (workspaceScheme) path = path.replace(/^\/+/, "");
  const ext = extensionOf(path);
  const looksLikeFile = Boolean(ext) && !path.includes("://");
  if (looksLikeFile || /^(?:workspace:|muteki-file:|file:)/i.test(raw)) {
    return { kind: "workspace", path, line };
  }

  if (raw.startsWith("#")) {
    return { kind: "unknown", href: raw };
  }

  return { kind: "unknown", href: raw };
}

export interface WorkspaceFilePreview {
  path: string;
  in_workspace?: boolean;
  size: number;
  media_type?: string;
  preview_kind: PreviewKind | string;
  max_preview_bytes?: number;
  truncated?: boolean;
  downloadable?: boolean;
  line?: number | null;
  content?: string | null;
  message?: string | null;
}
