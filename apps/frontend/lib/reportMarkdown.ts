
const ASSET_MARKER = "<!-- muteki:assets -->";
const ASSET_LINE = /^\[(img-[a-z0-9]+)\]:\s*(data:image\/(?:png|jpeg|webp|gif);base64,[A-Za-z0-9+/=]+)\s*$/;
const IMAGE_REF = /!\[([^\]]*)\]\[(img-[a-z0-9]+)\]/g;

export const INLINE_IMAGE_SRC = /^data:image\/(?:png|jpeg|webp|gif);base64,[A-Za-z0-9+/=]+$/;
export const MAX_REPORT_BYTES = 1_800_000;
const MAX_IMAGE_BYTES = 1_200_000;
const MAX_IMAGE_EDGE = 1600;

export type ReportAssets = Record<string, string>;

export function splitReportAssets(markdown: string): { body: string; assets: ReportAssets } {
  const at = markdown.lastIndexOf(`\n${ASSET_MARKER}`);
  const start = at >= 0 ? at : markdown.startsWith(ASSET_MARKER) ? 0 : -1;
  if (start < 0) return { body: markdown, assets: {} };
  const assets: ReportAssets = {};
  const tail = markdown.slice(start).split("\n").slice(start === at ? 2 : 1);
  for (const line of tail) {
    const match = ASSET_LINE.exec(line);
    if (match) assets[match[1]] = match[2];
    else if (line.trim()) return { body: markdown, assets: {} };
  }
  return { body: markdown.slice(0, start).replace(/\s+$/, ""), assets };
}

export function referencedAssetIds(body: string): Set<string> {
  return new Set([...body.matchAll(IMAGE_REF)].map((m) => m[2]));
}

export function joinReportAssets(body: string, assets: ReportAssets): string {
  const entries = Object.entries(assets);
  const text = body.trimEnd();
  if (!entries.length) return text;
  const used = referencedAssetIds(body);
  const lines = entries.filter(([id]) => used.has(id)).map(([id, url]) => `[${id}]: ${url}`);
  return lines.length ? `${text}\n\n${ASSET_MARKER}\n${lines.join("\n")}\n` : text;
}

export function inlineReportAssets(body: string, assets: ReportAssets): string {
  if (!Object.keys(assets).length) return body;
  return body.replace(IMAGE_REF, (whole, alt: string, id: string) => (assets[id] ? `![${alt}](${assets[id]})` : whole));
}

export function searchableMarkdown(markdown: string): string {
  return splitReportAssets(markdown).body;
}

export function newAssetId(): string {
  return `img-${Math.random().toString(36).slice(2, 8)}`;
}

function dataUrlBytes(url: string): number {
  return Math.floor((url.length - url.indexOf(",") - 1) * 0.75);
}

function loadImage(file: Blob): Promise<HTMLImageElement> {
  return new Promise((resolve, reject) => {
    const url = URL.createObjectURL(file);
    const image = new Image();
    image.onload = () => { URL.revokeObjectURL(url); resolve(image); };
    image.onerror = () => { URL.revokeObjectURL(url); reject(new Error("无法解码该图片")); };
    image.src = url;
  });
}

function readDataUrl(blob: Blob): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(String(reader.result));
    reader.onerror = () => reject(reader.error ?? new Error("读取图片失败"));
    reader.readAsDataURL(blob);
  });
}

export async function imageFileToDataUrl(file: File): Promise<string> {
  if (!/^image\/(png|jpeg|webp|gif)$/.test(file.type)) throw new Error(`不支持的图片格式：${file.type || file.name}`);
  if (file.type === "image/gif") {
    if (file.size > MAX_IMAGE_BYTES) throw new Error("GIF 超过 1.2 MB，请先压缩后再插入");
    return readDataUrl(file);
  }
  const image = await loadImage(file);
  const scale = Math.min(1, MAX_IMAGE_EDGE / Math.max(image.naturalWidth, image.naturalHeight));
  const canvas = document.createElement("canvas");
  canvas.width = Math.max(1, Math.round(image.naturalWidth * scale));
  canvas.height = Math.max(1, Math.round(image.naturalHeight * scale));
  const context = canvas.getContext("2d");
  if (!context) throw new Error("当前环境无法处理图片");
  context.drawImage(image, 0, 0, canvas.width, canvas.height);
  const encode = (type: string, quality?: number) =>
    new Promise<Blob | null>((resolve) => canvas.toBlob(resolve, type, quality));
  let blob = await encode("image/webp", 0.86);
  if (!blob || blob.type !== "image/webp") blob = await encode(file.type === "image/png" ? "image/png" : "image/jpeg", 0.86);
  if (!blob) throw new Error("图片编码失败");
  if (blob.size > file.size && scale === 1 && file.size <= MAX_IMAGE_BYTES) return readDataUrl(file);
  const url = await readDataUrl(blob);
  if (dataUrlBytes(url) > MAX_IMAGE_BYTES) throw new Error("图片压缩后仍超过 1.2 MB，请缩小后再插入");
  return url;
}

export function reportByteSize(markdown: string): number {
  return new TextEncoder().encode(markdown).length;
}

export function countWords(text: string): number {
  const cjk = text.match(/[\u3400-\u9fff\uf900-\ufaff\u3040-\u30ff\uac00-\ud7af]/g)?.length ?? 0;
  const latin = text.replace(/[\u3400-\u9fff\uf900-\ufaff\u3040-\u30ff\uac00-\ud7af]/g, " ").match(/[\p{L}\p{N}_'-]+/gu)?.length ?? 0;
  return cjk + latin;
}


export interface Edit { start: number; end: number; text: string; select?: [number, number] }

export interface ReportMarkdownInput {
  readonly value: string;
  readonly selectionStart: number;
  readonly selectionEnd: number;
  applyEdit(edit: Edit): void;
}

function lineBounds(value: string, start: number, end: number): [number, number] {
  const from = value.lastIndexOf("\n", start - 1) + 1;
  const stop = value.indexOf("\n", end > start && value[end - 1] === "\n" ? end - 1 : end);
  return [from, stop < 0 ? value.length : stop];
}

export function wrapInline(el: ReportMarkdownInput, open: string, close = open, placeholder = "文本") {
  const { value, selectionStart: s, selectionEnd: e } = el;
  const selected = value.slice(s, e);
  if (selected.startsWith(open) && selected.endsWith(close) && selected.length >= open.length + close.length) {
    const inner = selected.slice(open.length, selected.length - close.length);
    return el.applyEdit({ start: s, end: e, text: inner, select: [s, s + inner.length] });
  }
  if (value.slice(s - open.length, s) === open && value.slice(e, e + close.length) === close) {
    return el.applyEdit({ start: s - open.length, end: e + close.length, text: selected, select: [s - open.length, s - open.length + selected.length] });
  }
  const body = selected || placeholder;
  el.applyEdit({ start: s, end: e, text: `${open}${body}${close}`, select: [s + open.length, s + open.length + body.length] });
}

export type LineKind = "h1" | "h2" | "h3" | "quote" | "bullet" | "ordered" | "task";
const LINE_PREFIX: Record<LineKind, RegExp> = {
  h1: /^#\s+/, h2: /^##\s+/, h3: /^###\s+/, quote: /^>\s?/, bullet: /^[-*+]\s+(?!\[[ xX]\]\s)/, ordered: /^\d+[.)]\s+/, task: /^[-*+]\s+\[[ xX]\]\s+/,
};
const ANY_BLOCK_PREFIX = /^(?:#{1,6}\s+|>\s?|[-*+]\s+\[[ xX]\]\s+|[-*+]\s+|\d+[.)]\s+)/;

export function toggleLinePrefix(el: ReportMarkdownInput, kind: LineKind) {
  const { value, selectionStart: s, selectionEnd: e } = el;
  const [from, to] = lineBounds(value, s, e);
  const lines = value.slice(from, to).split("\n");
  const nonEmpty = lines.filter((line) => line.trim());
  const all = nonEmpty.length > 0 && nonEmpty.every((line) => LINE_PREFIX[kind].test(line.replace(/^\s+/, "")));
  let ordinal = 0;
  const next = lines.map((line) => {
    if (!line.trim()) return line;
    const indent = /^\s*/.exec(line)![0];
    const stripped = line.slice(indent.length).replace(ANY_BLOCK_PREFIX, "");
    if (all) return indent + stripped;
    ordinal += 1;
    const prefix = kind === "h1" ? "# " : kind === "h2" ? "## " : kind === "h3" ? "### " : kind === "quote" ? "> " : kind === "bullet" ? "- " : kind === "task" ? "- [ ] " : `${ordinal}. `;
    return indent + prefix + stripped;
  });
  const text = next.join("\n");
  el.applyEdit({ start: from, end: to, text, select: [from, from + text.length] });
}

export function insertCodeBlock(el: ReportMarkdownInput) {
  const { value, selectionStart: s, selectionEnd: e } = el;
  const selected = value.slice(s, e);
  const lead = s > 0 && value[s - 1] !== "\n" ? "\n" : "";
  const trail = e < value.length && value[e] !== "\n" ? "\n" : "";
  const body = selected || "";
  const text = `${lead}\`\`\`\n${body}\n\`\`\`${trail}`;
  const caret = s + lead.length + 4;
  el.applyEdit({ start: s, end: e, text, select: [caret, caret + body.length] });
}

export function insertLink(el: ReportMarkdownInput) {
  const { value, selectionStart: s, selectionEnd: e } = el;
  const selected = value.slice(s, e);
  const isUrl = /^https?:\/\/\S+$/.test(selected);
  const label = isUrl ? "链接文本" : selected || "链接文本";
  const url = isUrl ? selected : "https://";
  const text = `[${label}](${url})`;
  const urlStart = s + label.length + 3;
  el.applyEdit({ start: s, end: e, text, select: isUrl ? [s + 1, s + 1 + label.length] : [urlStart, urlStart + url.length] });
}

export function insertBlock(el: ReportMarkdownInput, block: string, select?: [number, number]) {
  const { value, selectionStart: s, selectionEnd: e } = el;
  const lead = s === 0 ? "" : value[s - 1] === "\n" ? (s > 1 && value[s - 2] === "\n" ? "" : "\n") : "\n\n";
  const trail = e >= value.length ? "\n" : value[e] === "\n" ? "" : "\n";
  const text = `${lead}${block}${trail}`;
  el.applyEdit({ start: s, end: e, text, select: select ? [s + lead.length + select[0], s + lead.length + select[1]] : undefined });
}

export const TABLE_TEMPLATE = "| 列 1 | 列 2 | 列 3 |\n| --- | --- | --- |\n|  |  |  |";
export function insertTable(el: ReportMarkdownInput) {
  insertBlock(el, TABLE_TEMPLATE, [2, 5]);
}

export function insertRule(el: ReportMarkdownInput) {
  insertBlock(el, "---");
}

const LIST_ITEM = /^(\s*)((?:[-*+]|\d+[.)])\s+(?:\[[ xX]\]\s+)?|>\s?)(.*)$/;

export function continueList(el: ReportMarkdownInput): boolean {
  const { value, selectionStart: s, selectionEnd: e } = el;
  if (s !== e) return false;
  const lineStart = value.lastIndexOf("\n", s - 1) + 1;
  const lineEnd = value.indexOf("\n", s);
  const line = value.slice(lineStart, lineEnd < 0 ? value.length : lineEnd);
  const match = LIST_ITEM.exec(line);
  if (!match || s < lineStart + match[1].length + match[2].length) return false;
  const [, indent, marker, content] = match;
  if (!content.trim()) {
    if (indent) {
      const text = line.replace(/^( {1,2}|\t)/, "");
      el.applyEdit({ start: lineStart, end: lineStart + line.length, text, select: [lineStart + text.length, lineStart + text.length] });
    } else {
      el.applyEdit({ start: lineStart, end: lineStart + line.length, text: "\n" });
    }
    return true;
  }
  let nextMarker = marker;
  const ordered = /^(\d+)([.)])(\s+)/.exec(marker);
  if (ordered) nextMarker = marker.replace(/^\d+/, String(Number(ordered[1]) + 1));
  nextMarker = nextMarker.replace(/\[[xX]\]/, "[ ]");
  el.applyEdit({ start: s, end: e, text: `\n${indent}${nextMarker}` });
  return true;
}

export function shiftIndent(el: ReportMarkdownInput, outdent: boolean): boolean {
  const { value, selectionStart: s, selectionEnd: e } = el;
  const [from, to] = lineBounds(value, s, e);
  const lines = value.slice(from, to).split("\n");
  if (lines.length === 1 && !outdent && !LIST_ITEM.test(lines[0])) return false;
  if (lines.length === 1 && outdent && !/^\s/.test(lines[0])) return false;
  const next = lines.map((line) => (outdent ? line.replace(/^( {1,2}|\t)/, "") : `  ${line}`));
  const text = next.join("\n");
  const delta = next[0].length - lines[0].length;
  el.applyEdit({ start: from, end: to, text, select: lines.length === 1 ? [Math.max(from, s + delta), Math.max(from, e + delta)] : [from, from + text.length] });
  return true;
}
