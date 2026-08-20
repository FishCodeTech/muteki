---
name: muteki-offline
description: Muteki Worker profile with network and integration lookup tools disabled
prompt_mode: full
model: inherit
permission_mode: default
agents_md: true
tools:
  - run_terminal_cmd
  - read_file
  - search_replace
  - grep
  - list_dir
  - write
  - todo_write
disallowedTools:
  - web_search
  - web_fetch
  - search_tool
  - use_tool
  - Agent
---

Complete the assigned task directly using local files, local commands, and the
tools supplied by the current workspace. Network lookup and external integration
tools are unavailable in this profile.
