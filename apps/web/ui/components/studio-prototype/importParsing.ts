/**
 * Reading local CSV, JSON and TXT files into import records. Everything here
 * is pure: the store decides what each record does to the library, the views
 * only render the results.
 */

import { SOURCE_KINDS, type SourceKind } from "./model";

export type ImportFormat = "csv" | "json" | "txt";
export type ImportField = "title" | "body" | "kind" | "author" | "collectedAt" | "key";
export type ImportOutcome = "created" | "updated" | "skipped" | "failed";
export type ImportValues = Record<ImportField, string>;
/** Column (CSV header, JSON key or TXT part) each field reads; "" leaves the field unmapped. */
export type ImportMapping = Record<ImportField, string>;
export type ImportCounts = Record<ImportOutcome, number>;
export type CsvDelimiter = "auto" | "," | "\t" | ";" | "|";
export type TxtMode = "whole" | "blocks";
export type ConflictPolicy = "update" | "skip";
export type ImportEncoding = "utf-8" | "gb18030" | "gbk" | "big5" | "utf-16le" | "iso-8859-1";
export type ImportCombineMode = "per_row" | "by_key" | "single_source";

export interface ImportParseOptions {
  format: ImportFormat;
  fileName: string;
  delimiter: CsvDelimiter;
  /** CSV only: the first row names the columns. */
  header: boolean;
  /** TXT only: the whole file is one source, or each blank-line separated block is one. */
  txtMode: TxtMode;
}

export interface ImportSettings extends ImportParseOptions {
  mapping: ImportMapping;
  defaultKind: SourceKind;
  defaultAuthor: string;
  /** What happens to a record that matches a source already in the library. */
  conflict: ConflictPolicy;
  /** Character encoding used to decode the file bytes. */
  encoding?: ImportEncoding;
  /** How multiple CSV/JSON records are grouped into sources. */
  combineMode?: ImportCombineMode;
  /** Title used when `combineMode === "single_source"`. */
  combinedTitle?: string;
}

/** Where a record sits in the original file; lines are 1-based and inclusive. */
export interface ImportLocation {
  label: string;
  line: number;
  endLine: number;
  /** JSON only: path of the record, e.g. `$.items[3]`. */
  path?: string;
}

export interface ParsedRecord {
  /** 1-based record number within the file. */
  index: number;
  location: ImportLocation;
  /** The record's original text. */
  raw: string;
  cells: Record<string, string>;
  /** Problem found while reading the record; "" when it was read cleanly. */
  error: string;
}

export interface ImportFileError {
  message: string;
  line?: number;
  column?: number;
}

export interface ParsedImport {
  columns: string[];
  records: ParsedRecord[];
  /** Mapping guessed from the column names and contents. */
  suggested: ImportMapping;
  notes: string[];
  error?: ImportFileError;
}

/** A record with the mapping applied, ready to be matched against the library. */
export interface ImportDraft {
  index: number;
  location: ImportLocation;
  raw: string;
  values: ImportValues;
  /** Carried over from `ParsedRecord.error`; cleared once the user corrects the record. */
  parseError: string;
  /** Number of raw rows synthesized into this draft when `combineMode` is used. */
  mergedCount?: number;
}

export const IMPORT_FIELDS: Array<{ value: ImportField; label: string; required: boolean; hint: string }> = [
  { value: "title", label: "标题", required: true, hint: "资产标题" },
  { value: "body", label: "正文", required: true, hint: "每行成为一个可引用的段落" },
  { value: "kind", label: "类型", required: false, hint: "留空时用默认类型" },
  { value: "author", label: "作者", required: false, hint: "留空时用默认作者" },
  { value: "collectedAt", label: "采集时间", required: false, hint: "留空时用导入当天" },
  { value: "key", label: "唯一标识", required: false, hint: "按它匹配已有资产；不选则按标题匹配" },
];

export const IMPORT_FORMATS: Array<{ value: ImportFormat; label: string }> = [
  { value: "csv", label: "CSV" },
  { value: "json", label: "JSON" },
  { value: "txt", label: "TXT" },
];

export const CSV_DELIMITERS: Array<{ value: CsvDelimiter; label: string }> = [
  { value: "auto", label: "自动识别" },
  { value: ",", label: "逗号" },
  { value: "\t", label: "制表符" },
  { value: ";", label: "分号" },
  { value: "|", label: "竖线" },
];

export const IMPORT_ENCODINGS: Array<{ value: ImportEncoding; label: string }> = [
  { value: "utf-8", label: "UTF-8（默认）" },
  { value: "gb18030", label: "GB18030 / 中文国标" },
  { value: "gbk", label: "GBK / 简体中文" },
  { value: "big5", label: "Big5 / 繁体中文" },
  { value: "utf-16le", label: "UTF-16 LE" },
  { value: "iso-8859-1", label: "ISO-8859-1 / Latin-1" },
];

export const IMPORT_COMBINE_MODES: Array<{ value: ImportCombineMode; label: string; description: string }> = [
  { value: "per_row", label: "每条记录一份资产", description: "每行 CSV 或每个 JSON 对象各自创建或更新一份资产" },
  { value: "by_key", label: "按标识/标题合并多条记录", description: "相同唯一标识或标题的多条记录合成为一份资料，每条记录作为独立段落" },
  { value: "single_source", label: "多条记录合成一份资料", description: "整份文件的全部记录合成为一份综合资料，每条记录转为带标题的段落" },
];

export const IMPORT_ACCEPT = ".csv,.json,.txt,text/csv,application/json,text/plain";
export const MAX_IMPORT_BYTES = 5 * 1024 * 1024;
export const MAX_IMPORT_RECORDS = 5000;

export const EMPTY_IMPORT_VALUES: ImportValues = { title: "", body: "", kind: "", author: "", collectedAt: "", key: "" };
export const EMPTY_IMPORT_MAPPING: ImportMapping = { title: "", body: "", kind: "", author: "", collectedAt: "", key: "" };

const TXT_COLUMNS = { file: "文件名", first: "首行", rest: "其余内容", all: "全部内容" } as const;

export function importFormatLabel(format: ImportFormat): string {
  return IMPORT_FORMATS.find((item) => item.value === format)?.label ?? format.toUpperCase();
}

export function detectImportFormat(name: string, mime: string): ImportFormat | null {
  const ext = name.includes(".") ? name.slice(name.lastIndexOf(".") + 1).toLowerCase() : "";
  if (ext === "csv") return "csv";
  if (ext === "json") return "json";
  if (ext === "txt") return "txt";
  if (mime === "text/csv") return "csv";
  if (mime === "application/json") return "json";
  if (mime === "text/plain") return "txt";
  return null;
}

/** Decodes raw file bytes with the chosen character encoding and normalizes line/BOM text. */
export function decodeImportBytes(buffer: ArrayBuffer, encoding: ImportEncoding = "utf-8"): { text: string; error?: string; warning?: string } {
  try {
    const decoder = new TextDecoder(encoding, { fatal: false });
    return normalizeImportText(decoder.decode(buffer), encoding);
  } catch {
    const fallback = new TextDecoder("utf-8", { fatal: false });
    return normalizeImportText(fallback.decode(buffer), "utf-8");
  }
}

/** Drops a byte-order mark and rejects content that is not usable text. */
export function normalizeImportText(raw: string, encoding: ImportEncoding = "utf-8"): { text: string; error?: string; warning?: string } {
  const text = raw.charCodeAt(0) === 0xfeff ? raw.slice(1) : raw;
  if (text.includes("\u0000")) return { text, error: "文件里有二进制内容，看起来不是文本文件（或编码选择不匹配，如 UTF-16 文件请切换编码）。" };
  if (!text.trim()) return { text, error: "文件是空的。" };
  return {
    text,
    warning: text.includes("\uFFFD")
      ? `当前按 ${encoding.toUpperCase()} 解码出现了无法识别的字符（）。如果预览有乱码，请在「文件编码」里切换为 GB18030 / GBK / Big5。`
      : undefined,
  };
}

/** Same line breaks as `lineStarts`, so line numbers agree everywhere. */
export function splitLines(text: string): string[] {
  return text.split(/\r\n|\r|\n/);
}

/** Passages of a body: one per non-empty line, as typed-in sources get. */
export function splitPassages(text: string): string[] {
  return text.split(/\n\s*\n|\n/).map((part) => part.trim()).filter(Boolean);
}

export function resolveSourceKind(raw: string): SourceKind | null | undefined {
  const value = raw.trim().toLowerCase();
  if (!value) return null;
  return SOURCE_KINDS.find((item) => item.value.toLowerCase() === value || item.label.toLowerCase() === value)?.value;
}

export function countImportOutcomes(items: Array<{ outcome: ImportOutcome }>): ImportCounts {
  const counts: ImportCounts = { created: 0, updated: 0, skipped: 0, failed: 0 };
  items.forEach((item) => { counts[item.outcome] += 1; });
  return counts;
}

export function mapImportRecord(cells: Record<string, string>, mapping: ImportMapping): ImportValues {
  const read = (field: ImportField) => {
    const column = mapping[field];
    return column ? cells[column] ?? "" : "";
  };
  return {
    title: read("title").trim(),
    body: read("body").trim(),
    kind: read("kind").trim(),
    author: read("author").trim(),
    collectedAt: read("collectedAt").trim(),
    key: read("key").trim(),
  };
}

export function toImportDraft(record: ParsedRecord, mapping: ImportMapping): ImportDraft {
  return { index: record.index, location: record.location, raw: record.raw, values: mapImportRecord(record.cells, mapping), parseError: record.error };
}

/**
 * Applies `combineMode` to mapped drafts so multiple CSV/JSON records can be
 * synthesized into one document when requested by the user.
 */
export function combineImportDrafts(
  drafts: ImportDraft[],
  options: { combineMode?: ImportCombineMode; combinedTitle?: string; fileName?: string },
): ImportDraft[] {
  const mode = options.combineMode ?? "per_row";
  if (mode === "per_row" || drafts.length <= 1) return drafts;

  if (mode === "single_source") {
    const valid = drafts.filter((d) => !d.parseError);
    const errors = drafts.filter((d) => d.parseError);
    if (!valid.length) return drafts;
    const baseTitle = (options.combinedTitle ?? "").trim()
      || (options.fileName ? options.fileName.replace(/\.[^.]+$/, "").trim() : "")
      || valid[0].values.title
      || "合并导入资料";
    const bodyLines = valid.map((d, idx) => {
      const head = d.values.title ? `【记录 ${idx + 1} · ${d.values.title}】` : `【记录 ${idx + 1}】`;
      const body = d.values.body || d.raw;
      return `${head} ${body.replace(/\s*\n\s*/g, " ； ")}`;
    });
    const combined: ImportDraft = {
      index: 1,
      location: {
        label: `合并全部 ${valid.length} 条记录（第 ${valid[0].location.line}–${valid[valid.length - 1].location.endLine} 行）`,
        line: valid[0].location.line,
        endLine: valid[valid.length - 1].location.endLine,
      },
      raw: valid.map((d) => d.raw).join("\n"),
      values: {
        title: baseTitle,
        body: bodyLines.join("\n"),
        kind: valid[0].values.kind,
        author: valid[0].values.author,
        collectedAt: valid[0].values.collectedAt,
        key: valid[0].values.key ? `${valid[0].values.key}-combined` : "",
      },
      parseError: "",
      mergedCount: valid.length,
    };
    return [combined, ...errors.map((e, i) => ({ ...e, index: i + 2 }))];
  }

  // mode === "by_key": group records sharing the same key or normalized title
  const groups = new Map<string, ImportDraft[]>();
  const passthroughErrors: ImportDraft[] = [];
  drafts.forEach((draft) => {
    if (draft.parseError) {
      passthroughErrors.push(draft);
      return;
    }
    const groupKey = draft.values.key
      ? `key:${draft.values.key.toLowerCase()}`
      : `title:${draft.values.title.trim().toLowerCase()}`;
    const list = groups.get(groupKey) ?? [];
    list.push(draft);
    groups.set(groupKey, list);
  });

  const out: ImportDraft[] = [];
  let seq = 1;
  groups.forEach((items) => {
    if (items.length === 1) {
      out.push({ ...items[0], index: seq++ });
      return;
    }
    const first = items[0];
    const last = items[items.length - 1];
    out.push({
      index: seq++,
      location: {
        label: `合成 ${items.length} 条同源记录（${first.location.label} 至 ${last.location.label}）`,
        line: first.location.line,
        endLine: last.location.endLine,
      },
      raw: items.map((item) => item.raw).join("\n"),
      values: {
        title: first.values.title,
        body: items.map((item, idx) => item.values.body || `记录 ${idx + 1}：${item.raw}`).join("\n"),
        kind: first.values.kind,
        author: first.values.author,
        collectedAt: first.values.collectedAt,
        key: first.values.key,
      },
      parseError: "",
      mergedCount: items.length,
    });
  });
  passthroughErrors.forEach((err) => out.push({ ...err, index: seq++ }));
  return out;
}

// ---------------------------------------------------------------------------
// Positions
// ---------------------------------------------------------------------------

function lineStarts(text: string): number[] {
  const starts = [0];
  for (let i = 0; i < text.length; i += 1) {
    const code = text.charCodeAt(i);
    if (code === 10) starts.push(i + 1);
    else if (code === 13) {
      if (text.charCodeAt(i + 1) === 10) i += 1;
      starts.push(i + 1);
    }
  }
  return starts;
}

/** 1-based line containing `offset`. */
function lineAt(starts: number[], offset: number): number {
  let lo = 0;
  let hi = starts.length - 1;
  while (lo < hi) {
    const mid = (lo + hi + 1) >> 1;
    if ((starts[mid] ?? 0) <= offset) lo = mid;
    else hi = mid - 1;
  }
  return lo + 1;
}

function linesLabel(line: number, endLine: number): string {
  return line === endLine ? `第 ${line} 行` : `第 ${line}–${endLine} 行`;
}

function spanLines(starts: number[], start: number, end: number): { line: number; endLine: number } {
  const line = lineAt(starts, start);
  return { line, endLine: Math.max(line, lineAt(starts, Math.max(start, end - 1))) };
}

// Null-prototype records, so columns such as "__proto__" or "constructor" read as plain cells.
function emptyCells(): Record<string, string> {
  return Object.create(null) as Record<string, string>;
}

// ---------------------------------------------------------------------------
// Mapping guesses
// ---------------------------------------------------------------------------

const FIELD_ALIASES: Record<ImportField, string[]> = {
  key: ["id", "key", "编号", "标识", "唯一标识", "uid", "uuid", "externalid", "sourceid", "code", "asset_id", "canonical_key"],
  title: ["title", "标题", "name", "名称", "题目", "subject", "heading", "主题", "display_name", "host", "domain"],
  body: ["body", "text", "content", "contents", "正文", "内容", "文本", "passages", "paragraphs", "段落", "摘录", "notes", "note", "笔记", "transcript", "summary", "banner"],
  kind: ["kind", "type", "类型", "category", "分类", "sourcetype", "asset_type", "family"],
  author: ["author", "作者", "记录人", "采访人", "creator", "by", "owner", "撰写人", "source"],
  collectedAt: ["collectedat", "collected", "date", "日期", "时间", "time", "采集时间", "收集时间", "记录时间", "created", "createdat", "observed_at", "last_observed_at"],
};

const GUESS_ORDER: ImportField[] = ["key", "title", "body", "kind", "author", "collectedAt"];

function aliasName(value: string): string {
  return value.toLowerCase().replace(/[\s_\-.]/g, "");
}

function guessMapping(columns: string[], records: ParsedRecord[]): ImportMapping {
  const mapping: ImportMapping = { ...EMPTY_IMPORT_MAPPING };
  const used = new Set<string>();
  const names = columns.map((column) => ({ column, names: [aliasName(column), aliasName(column.split(".").pop() ?? column)] }));
  for (const field of GUESS_ORDER) {
    const aliases = FIELD_ALIASES[field].map(aliasName);
    const hit = names.find((entry) => !used.has(entry.column) && entry.names.some((name) => aliases.includes(name)));
    if (hit) {
      mapping[field] = hit.column;
      used.add(hit.column);
    }
  }
  const sample = records.slice(0, 50);
  const averageLength = (column: string) => sample.reduce((sum, record) => sum + (record.cells[column] ?? "").length, 0) / Math.max(1, sample.length);
  const free = () => columns.filter((column) => !used.has(column));
  if (!mapping.body) {
    const longest = free().map((column) => ({ column, length: averageLength(column) })).sort((a, b) => b.length - a.length)[0];
    if (longest && longest.length >= 10) {
      mapping.body = longest.column;
      used.add(longest.column);
    }
  }
  if (!mapping.title) {
    const short = free().find((column) => {
      const length = averageLength(column);
      return length > 0 && length <= 80;
    });
    if (short) mapping.title = short;
  }
  return mapping;
}

// ---------------------------------------------------------------------------
// CSV
// ---------------------------------------------------------------------------

type Delimiter = Exclude<CsvDelimiter, "auto">;

function delimiterLabel(delimiter: Delimiter): string {
  return CSV_DELIMITERS.find((item) => item.value === delimiter)?.label ?? delimiter;
}

/** Picks the candidate that occurs most often in the first non-empty line, outside quotes. */
function detectDelimiter(text: string): Delimiter {
  const counts: Record<Delimiter, number> = { ",": 0, "\t": 0, ";": 0, "|": 0 };
  let quoted = false;
  for (let i = 0; i < text.length && i < 20000; i += 1) {
    const ch = text.charAt(i);
    if (ch === "\"") quoted = !quoted;
    else if (quoted) continue;
    else if (ch === "\n" || ch === "\r") {
      if (counts[","] || counts["\t"] || counts[";"] || counts["|"]) break;
    } else if (ch === "," || ch === "\t" || ch === ";" || ch === "|") counts[ch] += 1;
  }
  const best = (Object.keys(counts) as Delimiter[]).sort((a, b) => counts[b] - counts[a])[0];
  return best && counts[best] ? best : ",";
}

interface CsvRow {
  cells: string[];
  start: number;
  /** Offset just past the row's content, before its line break. */
  end: number;
  error: string;
}

/** RFC 4180 rows: quoted fields may hold delimiters, line breaks and doubled quotes. */
function readCsvRows(text: string, delimiter: Delimiter): CsvRow[] {
  const rows: CsvRow[] = [];
  let i = 0;
  while (i < text.length) {
    const start = i;
    const cells: string[] = [];
    let field = "";
    let quoted = false;
    let fieldStart = true;
    let end = text.length;
    while (i < text.length) {
      const ch = text.charAt(i);
      if (quoted) {
        if (ch === "\"") {
          if (text.charAt(i + 1) === "\"") { field += "\""; i += 2; continue; }
          quoted = false;
          i += 1;
          continue;
        }
        field += ch;
        i += 1;
        continue;
      }
      if (ch === "\"" && fieldStart) { quoted = true; fieldStart = false; i += 1; continue; }
      if (ch === delimiter) { cells.push(field); field = ""; fieldStart = true; i += 1; continue; }
      if (ch === "\n" || ch === "\r") {
        end = i;
        i += ch === "\r" && text.charAt(i + 1) === "\n" ? 2 : 1;
        break;
      }
      field += ch;
      fieldStart = false;
      i += 1;
    }
    cells.push(field);
    rows.push({ cells, start, end, error: quoted ? "引号没有闭合：从这一行的某个双引号开始，直到文件末尾都没有找到结束的双引号。" : "" });
  }
  return rows;
}

function uniqueColumns(cells: string[]): string[] {
  const seen = new Map<string, number>();
  return cells.map((cell, index) => {
    const base = cell.trim() || `第 ${index + 1} 列`;
    const count = (seen.get(base) ?? 0) + 1;
    seen.set(base, count);
    return count > 1 ? `${base}（${count}）` : base;
  });
}

function parseCsv(text: string, options: ImportParseOptions): ParsedImport {
  const delimiter = options.delimiter === "auto" ? detectDelimiter(text) : options.delimiter;
  const starts = lineStarts(text);
  const rows = readCsvRows(text, delimiter).filter((row) => row.error || row.cells.some((cell) => cell.trim()));
  const fail = (error: ImportFileError): ParsedImport => ({ columns: [], records: [], suggested: EMPTY_IMPORT_MAPPING, notes: [], error });
  const head = rows[0];
  if (!head) return fail({ message: "文件里没有数据行。" });
  if (options.header && head.error) return fail({ message: `表头${head.error}`, line: lineAt(starts, head.start) });
  const dataRows = options.header ? rows.slice(1) : rows;
  if (!dataRows.length) return fail({ message: "只有表头，没有数据行。", line: lineAt(starts, head.start) });
  if (dataRows.length > MAX_IMPORT_RECORDS) return fail({ message: `有 ${dataRows.length} 行数据，超过一次 ${MAX_IMPORT_RECORDS} 条的上限，请拆分后分批导入。` });
  const columns = options.header
    ? uniqueColumns(head.cells)
    : Array.from({ length: Math.max(...dataRows.map((row) => row.cells.length)) }, (_, index) => `第 ${index + 1} 列`);
  const records = dataRows.map((row, index): ParsedRecord => {
    const { line, endLine } = spanLines(starts, row.start, row.end);
    const cells = emptyCells();
    columns.forEach((column, at) => { cells[column] = row.cells[at] ?? ""; });
    let error = row.error;
    if (!error && options.header && row.cells.length !== columns.length) {
      error = `这一行有 ${row.cells.length} 列，表头有 ${columns.length} 列。可能是内容里的${delimiterLabel(delimiter)}没有用双引号括起来，或者少了一格。`;
    }
    return { index: index + 1, location: { label: linesLabel(line, endLine), line, endLine }, raw: text.slice(row.start, row.end), cells, error };
  });
  return {
    columns,
    records,
    suggested: guessMapping(columns, records),
    notes: [`分隔符：${delimiterLabel(delimiter)}${options.delimiter === "auto" ? "（自动识别）" : ""}`, `${columns.length} 列`, `${records.length} 条记录`],
  };
}

// ---------------------------------------------------------------------------
// JSON
// ---------------------------------------------------------------------------

class JsonSyntaxError extends Error {
  readonly offset: number;
  constructor(message: string, offset: number) {
    super(message);
    this.offset = offset;
    Object.setPrototypeOf(this, JsonSyntaxError.prototype);
  }
}

const JSON_ESCAPES: Record<string, string> = { "\"": "\"", "\\": "\\", "/": "/", b: "\b", f: "\f", n: "\n", r: "\r", t: "\t" };

function parseJsonWithSpans(text: string): { value: unknown; spans: WeakMap<unknown[], Array<[number, number]>> } {
  const spans = new WeakMap<unknown[], Array<[number, number]>>();
  let i = 0;
  const syntax = (message: string, at = i) => new JsonSyntaxError(message, at);
  const ws = () => {
    while (i < text.length) {
      const code = text.charCodeAt(i);
      if (code === 32 || code === 9 || code === 10 || code === 13) i += 1;
      else break;
    }
  };
  const string = (): string => {
    const start = i;
    i += 1;
    let out = "";
    let chunk = i;
    for (;;) {
      if (i >= text.length) throw syntax("字符串缺少结束的双引号", start);
      const code = text.charCodeAt(i);
      if (code === 34) {
        out += text.slice(chunk, i);
        i += 1;
        return out;
      }
      if (code === 92) {
        out += text.slice(chunk, i);
        const escape = text.charAt(i + 1);
        if (escape === "u") {
          const hex = text.slice(i + 2, i + 6);
          if (!/^[0-9a-fA-F]{4}$/.test(hex)) throw syntax("「\\u」后面应是 4 位十六进制数字");
          out += String.fromCharCode(parseInt(hex, 16));
          i += 6;
        } else {
          const mapped = JSON_ESCAPES[escape];
          if (mapped === undefined) throw syntax(`无效的转义「\\${escape}」`);
          out += mapped;
          i += 2;
        }
        chunk = i;
        continue;
      }
      if (code < 32) throw syntax(code === 10 || code === 13 ? "字符串里不能直接换行，需要写成 \\n" : "字符串里有不可见的控制字符");
      i += 1;
    }
  };
  const number = (): number => {
    const match = /^-?(?:0|[1-9]\d*)(?:\.\d+)?(?:[eE][+-]?\d+)?/.exec(text.slice(i, i + 1000));
    if (!match) throw syntax("数字格式不正确");
    i += match[0].length;
    return Number(match[0]);
  };
  const array = (): unknown[] => {
    i += 1;
    const out: unknown[] = [];
    const list: Array<[number, number]> = [];
    spans.set(out, list);
    ws();
    if (text.charAt(i) === "]") { i += 1; return out; }
    for (;;) {
      ws();
      if (text.charAt(i) === "]") throw syntax("最后一项后面多了逗号");
      const start = i;
      out.push(value());
      list.push([start, i]);
      ws();
      const ch = text.charAt(i);
      if (ch === ",") { i += 1; continue; }
      if (ch === "]") { i += 1; return out; }
      throw syntax(ch ? "两项之间缺少逗号" : "数组没有闭合，缺少「]」");
    }
  };
  const object = (): Record<string, unknown> => {
    i += 1;
    const out: Record<string, unknown> = {};
    ws();
    if (text.charAt(i) === "}") { i += 1; return out; }
    for (;;) {
      ws();
      const head = text.charAt(i);
      if (head !== "\"") throw syntax(head === "}" ? "最后一项后面多了逗号" : head ? "这里应是用双引号括起来的字段名" : "对象没有闭合，缺少「}」");
      const key = string();
      ws();
      if (text.charAt(i) !== ":") throw syntax("字段名后面缺少冒号");
      i += 1;
      Object.defineProperty(out, key, { value: value(), enumerable: true, writable: true, configurable: true });
      ws();
      const ch = text.charAt(i);
      if (ch === ",") { i += 1; continue; }
      if (ch === "}") { i += 1; return out; }
      throw syntax(ch ? "两项之间缺少逗号" : "对象没有闭合，缺少「}」");
    }
  };
  function value(): unknown {
    ws();
    const ch = text.charAt(i);
    if (ch === "{") return object();
    if (ch === "[") return array();
    if (ch === "\"") return string();
    if (ch === "-" || (ch >= "0" && ch <= "9")) return number();
    if (text.startsWith("true", i)) { i += 4; return true; }
    if (text.startsWith("false", i)) { i += 5; return false; }
    if (text.startsWith("null", i)) { i += 4; return null; }
    if (!ch) throw syntax("内容不完整，文件提前结束了");
    throw syntax(`这里出现了无法识别的字符「${ch}」`);
  }
  const result = value();
  ws();
  if (i < text.length) throw syntax("JSON 已经结束，后面还有多余的内容");
  return { value: result, spans };
}

function isPlainObject(value: unknown): boolean {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function jsonTypeName(value: unknown): string {
  if (value === null) return "空值 null";
  if (Array.isArray(value)) return "数组";
  if (typeof value === "string") return "文本";
  if (typeof value === "number") return "数字";
  if (typeof value === "boolean") return "布尔值";
  return "无法识别的值";
}

function jsonText(value: unknown): string {
  if (value === null || value === undefined) return "";
  if (typeof value === "string") return value;
  if (typeof value === "number" || typeof value === "boolean") return String(value);
  if (Array.isArray(value)) {
    return value.map((item) => {
      if (isPlainObject(item)) {
        const obj = item as Record<string, unknown>;
        if (typeof obj.text === "string") return obj.text;
        if (typeof obj.content === "string") return obj.content;
        return JSON.stringify(obj);
      }
      return jsonText(item);
    }).filter((line) => line.trim()).join("\n");
  }
  return JSON.stringify(value);
}

function flattenJson(source: Record<string, unknown>, prefix: string, out: Record<string, string>, depth: number) {
  for (const [key, value] of Object.entries(source)) {
    const name = prefix ? `${prefix}.${key}` : key;
    if (isPlainObject(value) && depth < 2) flattenJson(value as Record<string, unknown>, name, out, depth + 1);
    else out[name] = jsonText(value);
  }
}

const PREFERRED_ARRAY_KEYS = ["items", "records", "data", "sources", "rows", "entries", "list", "documents", "docs", "assets"];

function jsonKeyPath(key: string): string {
  return /^[A-Za-z_$][\w$]*$/.test(key) ? `$.${key}` : `$[${JSON.stringify(key)}]`;
}

function parseJson(text: string): ParsedImport {
  const fail = (error: ImportFileError): ParsedImport => ({ columns: [], records: [], suggested: EMPTY_IMPORT_MAPPING, notes: [], error });
  const starts = lineStarts(text);
  let parsed: ReturnType<typeof parseJsonWithSpans>;
  try {
    parsed = parseJsonWithSpans(text);
  } catch (error) {
    if (error instanceof JsonSyntaxError) {
      const line = lineAt(starts, error.offset);
      return fail({ message: `JSON 格式错误：${error.message}。`, line, column: error.offset - (starts[line - 1] ?? 0) + 1 });
    }
    return fail({ message: "JSON 嵌套层级太深，无法读取。" });
  }

  const root = parsed.value;
  let list: unknown[] | null = null;
  let base = "$";
  if (Array.isArray(root)) list = root;
  else if (isPlainObject(root)) {
    const rootObj = root as Record<string, unknown>;
    const keys = Object.keys(rootObj).filter((key) => {
      const val = rootObj[key];
      return Array.isArray(val) && val.some(isPlainObject);
    });
    const key = keys.find((item) => PREFERRED_ARRAY_KEYS.includes(item.toLowerCase())) ?? keys[0];
    const val = key === undefined ? undefined : rootObj[key];
    if (key !== undefined && Array.isArray(val)) {
      list = val;
      base = jsonKeyPath(key);
    }
  } else {
    return fail({ message: "JSON 顶层应是对象数组，或包含对象数组的对象。", line: 1, column: 1 });
  }

  const entries: Array<{ value: unknown; start: number; end: number; path: string }> = [];
  if (list) {
    const listSpans = parsed.spans.get(list) ?? [];
    list.forEach((val, at) => {
      const span = listSpans[at] ?? [0, text.length];
      entries.push({ value: val, start: span[0], end: span[1], path: `${base}[${at}]` });
    });
  } else {
    const start = Math.max(0, text.search(/\S/));
    entries.push({ value: root, start, end: text.trimEnd().length, path: "$" });
  }
  if (!entries.length) return fail({ message: "JSON 里没有记录，数组是空的。" });
  if (entries.length > MAX_IMPORT_RECORDS) return fail({ message: `有 ${entries.length} 条记录，超过一次 ${MAX_IMPORT_RECORDS} 条的上限，请拆分后分批导入。` });

  const columns: string[] = [];
  const known = new Set<string>();
  const records = entries.map((entry, index): ParsedRecord => {
    const { line, endLine } = spanLines(starts, entry.start, entry.end);
    const cells = emptyCells();
    let error = "";
    if (isPlainObject(entry.value)) {
      flattenJson(entry.value as Record<string, unknown>, "", cells, 0);
      Object.keys(cells).forEach((column) => {
        if (known.has(column) || columns.length >= 200) return;
        known.add(column);
        columns.push(column);
      });
    } else {
      error = `这一条是${jsonTypeName(entry.value)}，应是一个对象（{ … }）。`;
    }
    return {
      index: index + 1,
      location: { label: `第 ${index + 1} 条 · ${linesLabel(line, endLine)}`, line, endLine, path: entry.path },
      raw: text.slice(entry.start, entry.end),
      cells,
      error,
    };
  });
  return {
    columns,
    records,
    suggested: guessMapping(columns, records),
    notes: [`记录来自 ${list ? base : "顶层对象"}`, `${columns.length} 个字段`, `${records.length} 条记录`],
  };
}

// ---------------------------------------------------------------------------
// TXT
// ---------------------------------------------------------------------------

function parseTxt(text: string, options: ImportParseOptions): ParsedImport {
  const lines = splitLines(text);
  const blocks: Array<{ from: number; to: number }> = [];
  if (options.txtMode === "whole") {
    const from = lines.findIndex((line) => line.trim());
    let to = lines.length - 1;
    while (to > from && !(lines[to] ?? "").trim()) to -= 1;
    if (from >= 0) blocks.push({ from, to });
  } else {
    let from = -1;
    lines.forEach((line, at) => {
      if (line.trim()) {
        if (from < 0) from = at;
      } else if (from >= 0) {
        blocks.push({ from, to: at - 1 });
        from = -1;
      }
    });
    if (from >= 0) blocks.push({ from, to: lines.length - 1 });
  }
  const fail = (error: ImportFileError): ParsedImport => ({ columns: [], records: [], suggested: EMPTY_IMPORT_MAPPING, notes: [], error });
  if (!blocks.length) return fail({ message: "文件是空的。" });
  if (blocks.length > MAX_IMPORT_RECORDS) return fail({ message: `分出了 ${blocks.length} 块，超过一次 ${MAX_IMPORT_RECORDS} 条的上限，请拆分后分批导入。` });
  const fileTitle = options.fileName.replace(/\.[^.]+$/, "").trim() || options.fileName;
  const records = blocks.map((block, index): ParsedRecord => {
    const part = lines.slice(block.from, block.to + 1);
    const cells = emptyCells();
    cells[TXT_COLUMNS.file] = fileTitle;
    cells[TXT_COLUMNS.first] = (part[0] ?? "").trim();
    cells[TXT_COLUMNS.rest] = part.slice(1).join("\n").trim();
    cells[TXT_COLUMNS.all] = part.join("\n").trim();
    const line = block.from + 1;
    const endLine = block.to + 1;
    return { index: index + 1, location: { label: linesLabel(line, endLine), line, endLine }, raw: part.join("\n"), cells, error: "" };
  });
  const whole = options.txtMode === "whole";
  return {
    columns: [TXT_COLUMNS.file, TXT_COLUMNS.first, TXT_COLUMNS.rest, TXT_COLUMNS.all],
    records,
    suggested: {
      ...EMPTY_IMPORT_MAPPING,
      title: whole ? TXT_COLUMNS.file : TXT_COLUMNS.first,
      body: whole ? TXT_COLUMNS.all : TXT_COLUMNS.rest,
    },
    notes: whole ? [`整份文件作为一份资产`, `${lines.length} 行`] : [`按空行分成 ${records.length} 块`, "每块首行作标题"],
  };
}

export function parseImport(text: string, options: ImportParseOptions): ParsedImport {
  if (options.format === "csv") return parseCsv(text, options);
  if (options.format === "json") return parseJson(text);
  return parseTxt(text, options);
}
