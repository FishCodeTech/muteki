#!/usr/bin/env bash

if [ -n "${MUTEKI_LOCAL_WORKER_ROOT:-}" ] && [ -f "$MUTEKI_LOCAL_WORKER_ROOT/env.sh" ]; then
  _muteki_ctf_tools_root="$(cd "$MUTEKI_LOCAL_WORKER_ROOT" && pwd)"
elif [ -f "$PWD/ctf-tools/env.sh" ]; then
  _muteki_ctf_tools_root="$PWD/ctf-tools"
else
  _muteki_ctf_tools_root="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
fi
export MUTEKI_LOCAL_WORKER_ROOT="$_muteki_ctf_tools_root"
# Keep the older macOS variable for existing local installations.
export MUTEKI_MAC_WORKER_ROOT="$_muteki_ctf_tools_root"
if [ -d "$_muteki_ctf_tools_root/wordlists/SecLists" ]; then export MUTEKI_SECLISTS_DIR="$_muteki_ctf_tools_root/wordlists/SecLists"; fi
if [ -d "$_muteki_ctf_tools_root/wordlists/kali" ]; then export MUTEKI_WORDLIST_DIR="$_muteki_ctf_tools_root/wordlists/kali"; fi
if [ -d "$_muteki_ctf_tools_root/knowledges" ]; then export MUTEKI_OFFLINE_KNOWLEDGE_DIR="$_muteki_ctf_tools_root/knowledges"; fi
if [ -d "$_muteki_ctf_tools_root/pocs" ]; then export MUTEKI_POC_DIR="$_muteki_ctf_tools_root/pocs"; fi
if [ -d "$_muteki_ctf_tools_root/nuclei-templates" ]; then export NUCLEI_TEMPLATES="$_muteki_ctf_tools_root/nuclei-templates"; fi
export GEM_HOME="$_muteki_ctf_tools_root/ruby"
export GEM_PATH="$GEM_HOME"

for _muteki_tool_bin in \
  "$_muteki_ctf_tools_root/bin" \
  "$_muteki_ctf_tools_root/python/bin" \
  "$_muteki_ctf_tools_root/ruby/bin" \
  "/opt/homebrew/opt/ruby/bin" \
  "/usr/local/opt/ruby/bin"; do
  if [ -d "$_muteki_tool_bin" ]; then
    PATH="$_muteki_tool_bin:$PATH"
  fi
done
export PATH

unset _muteki_tool_bin _muteki_ctf_tools_root
