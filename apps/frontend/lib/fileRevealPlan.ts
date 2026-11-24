
export type FileRevealTarget =
  | { kind: "directory"; path: string }
  | { kind: "file"; path: string; line?: number };

export type FileRevealRequest = FileRevealTarget & { nonce: number };

export type FileRevealPlan =
  | { mode: "directory"; loadPath: string; clearPreview: true }
  | { mode: "file"; loadPath: string; openPath: string; line?: number };

function parentDir(path: string): string {
  return path.split("/").filter(Boolean).slice(0, -1).join("/");
}

export function planFileReveal(target: FileRevealTarget): FileRevealPlan {
  if (target.kind === "directory") {
    return { mode: "directory", loadPath: target.path, clearPreview: true };
  }
  return {
    mode: "file",
    loadPath: parentDir(target.path),
    openPath: target.path,
    line: target.line,
  };
}
