#!/usr/bin/env bash
# Install the muteki-blackboard skill into the user-scope skill dirs used by the
# worker CLIs. Claude Code and Cursor read ~/.claude/skills; Codex reads
# ~/.agents/skills. Idempotent: re-running overwrites with the latest source.
#
# NOTE: a SOURCE run no longer depends on this — cli_solver._blackboard_script_path()
# now hands workers the in-repo skill directly, and Swarm reconciles any stale
# deployed copy at launch (sync_deployed_blackboard_skills). This script remains the
# way to seed/refresh the user-scope copies for the worker CLIs' own skill
# auto-discovery and for installed (non-source) deployments.
set -euo pipefail

cd "$(dirname "$0")/.."
SRC="skills/muteki-blackboard"

if [ ! -f "$SRC/SKILL.md" ]; then
  echo "ERROR: $SRC/SKILL.md not found (run from repo root)" >&2
  exit 1
fi

install_to() {
  local dest="$1"
  mkdir -p "$dest/muteki-blackboard"
  cp "$SRC/SKILL.md" "$dest/muteki-blackboard/SKILL.md"
  cp "$SRC/blackboard.py" "$dest/muteki-blackboard/blackboard.py"
  chmod +x "$dest/muteki-blackboard/blackboard.py"
  echo "  installed -> $dest/muteki-blackboard/"
}

echo "Installing muteki-blackboard skill:"
install_to "$HOME/.claude/skills"   # Claude Code + Cursor user scope
install_to "$HOME/.agents/skills"   # Codex user scope
echo "Done. Claude, Cursor, and Codex workers will discover it at user scope."
