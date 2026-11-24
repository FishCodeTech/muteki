# shellcheck shell=sh
# Login shells in Tcode terminal panes also need the agents and global npm commands.
export PATH="/data/agents/bin:/data/bin:$NPM_CONFIG_PREFIX/bin:$PATH"
