# Muteki workflow and tool reference

The Gateway filters tools by the current thread binding. A missing tool means
the current mode or grant does not authorize it; do not replace it with direct
database or filesystem access.

## History, outline anchors, and cross-thread inspection

- Always pass `"self"` as `thread_id` when inspecting or updating your own
  thread so forked sessions never target a stale parent ID.
- `muteki_list_projects`: find product projects.
- `muteki_list_threads`: filter conversations by `project_id`, `status`,
  `archived`, `settled`, `pinned`, `follow_up`, `subagent`, or `query`.
- `muteki_outline_thread`: extract per-turn structure, tool counts, touched
  files, errors, and `<thread_id>#<seq>` anchors without loading full messages.
- `muteki_read_anchors`: read exact `<thread_id>#<start>-<end>` ranges with
  secret redaction and `<quoted_thread_data>` prompt-injection fencing.
- `muteki_search_in_item`: search line-by-line inside one large message or tool
  output item (`<thread_id>#<seq>`).
- `muteki_file_history`: find which threads and `<thread_id>#<seq>` items read
  or modified a file path.
- `muteki_reference_thread`: record a durable `@thread` citation with optional
  `anchors` and receive cross-project/cross-engine boundary notices.
- `muteki_get_git`, `muteki_get_pull_request`, `muteki_read_terminal`,
  `muteki_get_settings`, `muteki_review_candidates`: read workspace Git state,
  PR checks/comments, terminal buffer, non-secret control presets/rosters, and
  threads ready for triage.
- `muteki_list_tasks`, `muteki_get_task`, `muteki_list_runs`,
  `muteki_get_command_receipt`: inspect task and run state.

## Conversation lifecycle, Peer Council, and Thread operation

- `muteki_update_threads`: batch lifecycle transitions (`settle`, `unsettle`,
  `archive`, `unarchive`, `pin`, `unpin`, `rename`, `mark_read`, `follow_up`,
  `park`, `keep`) with `expect` preconditions and `dry_run`.
- `muteki_record_review`: attach structured review conclusions + `evidence_anchors`.
- `muteki_settle_self`: arm automatic self-settle at the end of the current turn (`when="end_of_turn"`)
  using a verbatim `consent_quote` from a human message in this thread.
- `muteki_start_discussion`: launch a multi-model Peer Council (`rounds=1|2`,
  `preset_id` or `members`). Round 1 runs blind; Round 2 unlocks read-only
  `Observe` across sibling peers in the same `discussion_group_id`.
- `muteki_start_thread`, `muteki_send_message` (`queue` / `steer` / `schedule`),
  `muteki_control_thread`, `muteki_notify_when`, `muteki_cancel_notify`,
  `muteki_respond_approval`, `muteki_respond_input`, `muteki_report_result`,
  `muteki_rewind_thread`, `muteki_move_thread`, `muteki_delete_threads`,
  `muteki_patch_settings`, `muteki_git`, `muteki_pull_request`, `muteki_terminal`.

List tools return `next_cursor` and `has_more`. Continue only when more records
are needed, passing `next_cursor` as `cursor` with the same filters. Read one
task's complete input with `muteki_get_task` after obtaining its ID.

## Task and run lifecycle

- `muteki_create_task`: create the durable unit of work.
- `muteki_dispatch_challenge`: create and dispatch a challenge-shaped task.
- `muteki_dispatch_pentest`: create an authorized `pentest.target` task without
  starting a Run. Preserve the returned `task_id` for the next step.
- `muteki_create_run`: allocate a run for a task.
- `muteki_start_swarm`: start the product Coordinator path. A
  `pentest.target` task automatically starts in Pentest mode and derives goal
  and scope from the task input. CTF challenge categories are normalized
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
- `muteki_get_worker_settings`: read the non-secret CTF/Pentest Worker,
  container, network, outbound, resource, roster, and stage configuration.
- `muteki_update_worker_settings`: update only explicitly requested settings;
  read them again before reporting success.
- `muteki_update_worker_identity`: bind Worker seats to existing account
  references and choose the Worker backend/network.
- `muteki_list_credential_accounts`: read account metadata without secrets.
- `muteki_import_host_account`: copy a supported host-native login into an
  account without passing secret bytes through the model or MCP arguments.
- `muteki_import_credential_file`: import an API key or Codex auth JSON from an
  existing local file path; never put the file contents in chat.
- `muteki_get_openvpn_status`, `muteki_import_openvpn_file`: inspect or import
  the Worker outbound VPN configuration by local path without returning it.

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
