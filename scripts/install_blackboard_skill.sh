#!/usr/bin/env bash
# Compatibility command retained for existing setup documentation. Current
# Workers receive a project-local projection in their private cwd. This command
# deliberately leaves every user-level Skill directory unchanged.
set -euo pipefail

echo "No user-level files changed. Muteki injects muteki-blackboard into each Worker workspace."
