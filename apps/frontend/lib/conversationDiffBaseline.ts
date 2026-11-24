
export interface WorktreeDiffBaselineSource {
  baseline?: string;
  head_sha?: string | null;
  detached_sha?: string | null;
  patch?: string;
  sections?: {
    staged?: { patch?: string };
    unstaged?: { patch?: string };
    untracked?: { patch?: string };
  };
}

export function fnv1aHex(input: string): string {
  let hash = 0x811c9dc5;
  for (let i = 0; i < input.length; i += 1) {
    hash ^= input.charCodeAt(i);
    hash = Math.imul(hash, 0x01000193);
  }
  return (hash >>> 0).toString(16).padStart(8, "0");
}

export function worktreeContentRevision(
  data: WorktreeDiffBaselineSource | null | undefined,
): string {
  if (!data) return "";
  const apiBaseline = String(data.baseline || "").trim();
  if (apiBaseline && apiBaseline !== "worktree") return apiBaseline;

  const head = String(data.head_sha || data.detached_sha || "").trim();
  let material: string;
  if (data.sections) {
    const staged = data.sections.staged?.patch ?? "";
    const unstaged = data.sections.unstaged?.patch ?? "";
    const untracked = data.sections.untracked?.patch ?? "";
    material = `staged\n${staged}\nunstaged\n${unstaged}\nuntracked\n${untracked}`;
  } else {
    material = String(data.patch || "");
  }
  const digest = fnv1aHex(material);
  return head ? `${head}:${digest}` : digest;
}

export function annotationBaselineId(
  kind: string,
  turnId?: string,
  artifactId?: string,
  worktreeRevision?: string | null,
): string {
  if (kind === "worktree" || kind === "branch") {
    const rev = String(worktreeRevision || "").trim();
    return rev ? `${kind}:${rev}` : `${kind}:pending`;
  }
  const sha = String(artifactId || "").trim();
  if (sha) return `artifact:${sha}`;
  const turn = String(turnId || "").trim();
  if (turn) return `turn:${turn}`;
  return "unknown";
}
