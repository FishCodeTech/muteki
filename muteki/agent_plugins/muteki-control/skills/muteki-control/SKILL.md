---
name: muteki-control
description: Operate Muteki projects, conversations, tasks, runs, workers, competitions, connections, submissions, runtimes, and profiles. Use whenever an answer depends on Muteki product state or when starting, monitoring, steering, pausing, resuming, stopping, or resolving Muteki work.
license: AGPL-3.0-or-later
compatibility: Requires the bundled muteki-control MCP server or Python 3 with a Muteki capability endpoint and session-scoped token.
metadata:
  author: FishCodeTech
  version: "1.0.0"
---

# Muteki Control

Use the `muteki_*` MCP tools as the authoritative interface to Muteki. Do not
inspect Muteki's SQLite databases, session directories, output directories, or
process files to answer a question that a Muteki tool can answer.

## Required operating order

1. Read product state with the narrowest relevant `muteki_list_*`,
   `muteki_get_*`, snapshot, event, or wait tool.
2. Reuse returned IDs. Do not infer IDs from directory names or database rows.
3. Perform the requested state change with the matching `muteki_*` tool.
4. For commands, read the returned receipt or call
   `muteki_get_command_receipt` before reporting success.
5. For long-running work, call `muteki_wait_run` with a bounded timeout. Use
   `muteki_read_run_events` only when event-level evidence is needed.
6. After any tool returns an error, stop that workflow branch and report the
   exact error. Never continue with an empty, guessed, or placeholder
   `task_id`, `run_id`, `thread_id`, `worker_id`, `binding_key`, or other ID.

## Workflow routing

- Existing work or history: start with `muteki_list_tasks`, `muteki_get_task`,
  or `muteki_list_runs`.
- New single task: `muteki_create_task` → `muteki_create_run` →
  `muteki_start_swarm` → `muteki_wait_run` → `muteki_resolve_run`.
- Running task control: use run snapshot, events, shared graph, directives,
  pause/resume/stop, context, and worker lifecycle tools.
- Competition work: read competition, connection, challenge, submission, and
  lease state before selecting, queuing, skipping, scheduling, instancing, or
  submitting an answer.
- Runtime configuration: use `muteki_list_runtime_instances` and
  `muteki_list_worker_profiles`.

Read [the workflow and tool reference](references/workflows.md) when a task
crosses more than one of these areas or when choosing between similar tools.

## Skills-only fallback

When the MCP tools are not exposed by the client, run the bundled script from
this skill directory. The session must provide `MUTEKI_CAPABILITY_ENDPOINT` and
`MUTEKI_CAPABILITY_TOKEN` without putting either value in the prompt.

```bash
python3 scripts/muteki_client.py describe
python3 scripts/muteki_client.py call muteki_list_tasks --args '{}'
```

Use the returned JSON-RPC result directly. An `error` result means the action
did not complete; report that error instead of searching internal files.
