/**
 * Shared display rules for the conversation workspace branch badge.
 *
 * Live Git `current_branch` always wins over create-time `settings.branch` /
 * `configured_branch`. Those configured values may appear only as an explicit
 * "创建时分支" hint, never as the current-branch label.
 */

export type LiveGitBranchStatus = {
  is_repo?: boolean;
  current_branch?: string | null;
  detached_head?: boolean;
  detached_sha?: string | null;
  branches?: string[];
  configured_branch?: string | null;
  root_path?: string;
};

export type WorkspaceBranchChipInput = {
  git?: LiveGitBranchStatus | null;
  /** Present when the git status request failed. */
  gitError?: string | null;
  /** True while the first shared status load is in flight. */
  gitLoading?: boolean;
  /** Creation-time workspace.settings.branch — never the live current branch. */
  settingsBranch?: string | null;
  mode?: string;
  rootPath?: string;
  kind?: string;
};

export type WorkspaceBranchChip = {
  icon: "gitBranch" | "shield";
  label: string;
  title: string;
  /** Create-time branch when it differs from the live current branch. */
  configuredHint?: string;
  source: "live" | "detached" | "missing" | "deleted" | "error" | "worktree" | "sandbox";
};

function trimBranch(value: unknown): string {
  return typeof value === "string" ? value.trim() : "";
}

function basename(path: string): string {
  const trimmed = path.replace(/\/$/, "");
  return trimmed.split("/").filter(Boolean).at(-1) || trimmed;
}

function configuredBranchOf(input: WorkspaceBranchChipInput): string {
  const fromGit = trimBranch(input.git?.configured_branch);
  if (fromGit) return fromGit;
  return trimBranch(input.settingsBranch);
}

function withConfiguredHint(
  title: string,
  configured: string,
  currentLabel: string,
): { title: string; configuredHint?: string } {
  if (!configured || configured === currentLabel) return { title };
  const hint = `创建时分支: ${configured}`;
  return {
    title: title ? `${title} · ${hint}` : hint,
    configuredHint: hint,
  };
}

/**
 * Resolve the header workspace chip.
 * Prefers live Git current_branch; never treats settings.branch as current.
 */
export function resolveWorkspaceBranchChip(
  input: WorkspaceBranchChipInput,
): WorkspaceBranchChip | null {
  const root = (input.rootPath || input.git?.root_path || "").trim();
  const configured = configuredBranchOf(input);
  const mode = String(input.mode || "");
  const git = input.git;

  if (input.gitError && !git) {
    const label = "分支读取失败";
    const { title, configuredHint } = withConfiguredHint(
      input.gitError,
      configured,
      label,
    );
    return {
      icon: "gitBranch",
      label,
      title: root ? `${title} · ${root}` : title,
      configuredHint,
      source: "error",
    };
  }

  if (git?.is_repo) {
    if (git.detached_head) {
      const sha = trimBranch(git.detached_sha);
      const label = sha ? `detached ${sha.slice(0, 7)}` : "detached HEAD";
      const baseTitle = root ? `${label} · ${root}` : label;
      const { title, configuredHint } = withConfiguredHint(baseTitle, configured, label);
      return { icon: "gitBranch", label, title, configuredHint, source: "detached" };
    }

    const current = trimBranch(git.current_branch);
    if (current) {
      const branches = Array.isArray(git.branches) ? git.branches : [];
      const deleted =
        branches.length > 0 && !branches.some((branch) => branch === current);
      const label = deleted ? `${current}（已删除）` : current;
      const baseTitle = root ? `${label} · ${root}` : label;
      const { title, configuredHint } = withConfiguredHint(baseTitle, configured, current);
      return {
        icon: "gitBranch",
        label,
        title,
        configuredHint,
        source: deleted ? "deleted" : "live",
      };
    }

    const label = "未知分支";
    const baseTitle = root ? `${label} · ${root}` : label;
    const { title, configuredHint } = withConfiguredHint(baseTitle, configured, label);
    return { icon: "gitBranch", label, title, configuredHint, source: "missing" };
  }

  // While loading (or before first fetch), do not fall back to settings.branch
  // as if it were the live current branch.
  if (mode === "new_worktree" || mode === "existing_worktree") {
    const label = basename(root) || "Git worktree";
    const baseTitle = root || "Git worktree";
    const { title, configuredHint } = withConfiguredHint(baseTitle, configured, label);
    return { icon: "gitBranch", label, title, configuredHint, source: "worktree" };
  }

  if (input.kind === "isolated") {
    return {
      icon: "shield",
      label: "沙箱工作区",
      title: root || "沙箱工作区",
      source: "sandbox",
    };
  }

  return null;
}

/** Pure helper used by self-checks: live current always beats settings.branch. */
export function preferLiveBranchLabel(input: {
  currentBranch?: string | null;
  settingsBranch?: string | null;
  configuredBranch?: string | null;
}): string {
  const chip = resolveWorkspaceBranchChip({
    git: {
      is_repo: true,
      current_branch: input.currentBranch ?? null,
      configured_branch: input.configuredBranch ?? null,
      branches: input.currentBranch ? [String(input.currentBranch)] : [],
    },
    settingsBranch: input.settingsBranch ?? null,
  });
  return chip?.label || "";
}
