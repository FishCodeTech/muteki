
export interface DiffLine {
  kind: "same" | "add" | "del";
  text: string;
}

const MAX_PREVIEW_LINES = 2_000;
const MAX_MATRIX_CELLS = 1_048_576;

export function reportDiffPreview(current: string, incoming: string): DiffRow[] | null {
  if (current === incoming) {
    let count = 1;
    for (let newline = current.indexOf("\n"); newline >= 0; newline = current.indexOf("\n", newline + 1)) count++;
    return count === 1 ? [{ kind: "same", text: current }] : [{ kind: "gap", count }];
  }
  const a = current.split("\n", MAX_PREVIEW_LINES + 1);
  const b = incoming.split("\n", MAX_PREVIEW_LINES + 1);
  const n = a.length;
  const m = b.length;
  const cells = (n + 1) * (m + 1);
  if (n > MAX_PREVIEW_LINES || m > MAX_PREVIEW_LINES || cells > MAX_MATRIX_CELLS) return null;
  const dp = new Uint32Array(cells);
  const idx = (i: number, j: number) => i * (m + 1) + j;
  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) {
      dp[idx(i, j)] = a[i] === b[j] ? dp[idx(i + 1, j + 1)] + 1 : Math.max(dp[idx(i + 1, j)], dp[idx(i, j + 1)]);
    }
  }
  const out: DiffLine[] = [];
  let i = 0;
  let j = 0;
  while (i < n && j < m) {
    if (a[i] === b[j]) {
      out.push({ kind: "same", text: a[i] });
      i++;
      j++;
    } else if (dp[idx(i + 1, j)] >= dp[idx(i, j + 1)]) {
      out.push({ kind: "del", text: a[i] });
      i++;
    } else {
      out.push({ kind: "add", text: b[j] });
      j++;
    }
  }
  while (i < n) out.push({ kind: "del", text: a[i++] });
  while (j < m) out.push({ kind: "add", text: b[j++] });
  return collapseDiff(out);
}

export type DiffRow = DiffLine | { kind: "gap"; count: number };

export function collapseDiff(lines: DiffLine[], context = 3): DiffRow[] {
  const rows: DiffRow[] = [];
  let i = 0;
  while (i < lines.length) {
    if (lines[i].kind !== "same") {
      rows.push(lines[i]);
      i++;
      continue;
    }
    let j = i;
    while (j < lines.length && lines[j].kind === "same") j++;
    const run = lines.slice(i, j);
    const leading = i === 0 ? 0 : context;
    const trailing = j === lines.length ? 0 : context;
    if (run.length <= leading + trailing + 1) {
      rows.push(...run);
    } else {
      rows.push(...run.slice(0, leading));
      rows.push({ kind: "gap", count: run.length - leading - trailing });
      rows.push(...run.slice(run.length - trailing));
    }
    i = j;
  }
  return rows;
}
