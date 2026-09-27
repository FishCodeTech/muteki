import { splitUnifiedDiffFiles, type DiffFileMeta, type DiffStaging } from "@/lib/conversationDiff";
import { parsePatchCached, type DiffListFile } from "./model";

export function diffFileKey(file: Pick<DiffFileMeta, "path" | "staging">): string {
  return `${file.staging || "artifact"}:${file.path}`;
}

/** Split a multi-file patch into list files tagged with one staging section. */
export function filesFromPatch(patch: string, staging: DiffStaging): DiffListFile[] {
  return splitUnifiedDiffFiles(patch).map((file) => {
    const meta: DiffFileMeta = {
      path: file.path,
      old_path: file.oldPath,
      status: file.status,
      staging,
      binary: file.binary,
      additions: file.additions,
      deletions: file.deletions,
      patch: file.raw,
    };
    return { key: diffFileKey(meta), meta, parsed: parsePatchCached(file.raw, file.binary) };
  });
}

/**
 * Build list files from metadata rows, taking each file's patch from the row
 * itself or, failing that, from a combined patch keyed by path.
 */
export function filesFromMetas(metas: readonly DiffFileMeta[], combinedPatch = ""): DiffListFile[] {
  const byPath = new Map<string, string>();
  if (combinedPatch) {
    for (const chunk of splitUnifiedDiffFiles(combinedPatch)) {
      if (!byPath.has(chunk.path)) byPath.set(chunk.path, chunk.raw);
    }
  }
  return metas.map((meta) => {
    const raw = meta.patch || byPath.get(meta.path) || "";
    return { key: diffFileKey(meta), meta, parsed: parsePatchCached(raw, Boolean(meta.binary)) };
  });
}

export function totalStats(files: readonly DiffListFile[]): { additions: number; deletions: number } {
  let additions = 0;
  let deletions = 0;
  for (const file of files) {
    additions += file.meta.additions ?? file.parsed.additions;
    deletions += file.meta.deletions ?? file.parsed.deletions;
  }
  return { additions, deletions };
}
