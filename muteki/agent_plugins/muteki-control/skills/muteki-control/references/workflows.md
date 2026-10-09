# Muteki workflow and tool reference

The Gateway filters tools by the current thread binding. A missing tool means
the current mode or grant does not authorize it; do not replace it with direct
database or filesystem access.

## History and identity

- `muteki_list_projects`: find product projects.
- `muteki_list_threads`: find conversations and their current state.
- `muteki_list_tasks`: search tasks by product metadata.
- `muteki_get_task`: read one task and its full input.
- `muteki_list_runs`: find existing runs without scanning session folders.
- `muteki_get_command_receipt`: verify an accepted command reached a terminal
  state.

List tools return `next_cursor` and `has_more`. Continue only when more records
are needed, passing `next_cursor` as `cursor` with the same filters. Read one
task's complete input with `muteki_get_task` after obtaining its ID.

## Task and run lifecycle

- `muteki_create_task`: create the durable unit of work.
- `muteki_dispatch_challenge`: create and dispatch a challenge-shaped task.
- `muteki_create_run`: allocate a run for a task.
- `muteki_start_swarm`: start the product Coordinator path. A
  `pentest.target` task automatically starts in Pentest mode, derives goal and
  scope from the task input, and disables the race-scout stage unless the call
  explicitly overrides those fields. CTF challenge categories are normalized
  to the product's lowercase category values.
- `muteki_wait_run`: wait for bounded progress or completion.
- `muteki_resolve_run`: continue an ended, incomplete run in its next execution
  generation, preserving its run ID and workspace.
- `muteki_get_run_snapshot`: read projected run state.
- `muteki_read_run_events`: read durable execution evidence.
- `muteki_read_shared_graph`: read facts, routes, branches, intents, flags, and
  review state; use `sections` from `available_sections` to select complete
  relevant blocks.

## Active run control

- `muteki_pause_run`, `muteki_resume_run`, `muteki_stop_run`: change run state.
- `muteki_send_operator_directive`: give the Coordinator a durable operator
  instruction.
- `muteki_add_run_context`: attach relevant context to an active run.
- `muteki_spawn_run_worker`, `muteki_cancel_run_worker`: manage individual
  workers inside the run. Their command receipt includes `control_receipt`.
  Treat the worker mutation as successful only when that nested receipt has
  `status=effect_observed`; `muteki_spawn_run_worker` exposes `worker_id` only
  in that state. Reuse that exact ID when cancelling the worker.

## Competition state

- `muteki_list_competitions`: find configured competitions.
- `muteki_list_connections`: read platform connections.
- `muteki_list_competition_challenges`: read challenge inventory and status.
- `muteki_list_competition_submissions`: read submission history.
- `muteki_list_competition_leases`: read challenge ownership and lease state.
- `muteki_get_competition_snapshot`: read the current competition projection.

## Competition control

- `muteki_create_connection`, `muteki_test_connection`: create and verify a
  platform connection.
- `muteki_sync_competition`: refresh remote competition state.
- `muteki_update_policy`: change scheduler policy.
- `muteki_select_challenge`, `muteki_queue_challenge`,
  `muteki_skip_challenge`: control challenge selection.
- `muteki_start_scheduler`, `muteki_pause_scheduler`,
  `muteki_resume_scheduler`: control scheduling.
- `muteki_ensure_challenge_instance`, `muteki_stop_challenge_instance`: manage
  per-challenge target instances.
- `muteki_submit_competition_answer`: submit a verified answer through the
  competition product path.

## Runtime management

- `muteki_list_runtime_instances`: read registered agent runtime instances and
  health.
- `muteki_list_worker_profiles`: read schedulable worker profiles.

## Decision rules

1. List before get when no exact ID is known.
2. Get or snapshot before mutation when current state affects the action.
3. A command receipt proves acceptance; terminal receipt state or later
   projection proves completion.
4. Use bounded waits instead of repeated rapid polling.
5. Use actual command output, stderr, or artifacts as Flag evidence. Product
   instructions, review text, and model claims are not Flag evidence.
6. Treat every returned ID as branch-local data. If the call that should
   produce an ID fails or omits it, stop the branch and report that response;
   do not call the next tool with an empty, inferred, placeholder, or reused ID.
7. For worker control, inspect `output.control_receipt` before continuing. An
   accepted outer command without an observed control effect does not prove
   that a worker was created or cancelled.
