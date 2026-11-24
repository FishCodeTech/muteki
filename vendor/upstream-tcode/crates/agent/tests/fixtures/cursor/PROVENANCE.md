# Cursor fixtures

All of these concern `cursor-agent` 2026.10.01-e373342 (macOS arm64). Two
kinds live here, and only the first is a recording.

## Recorded

Captured on 2026-10-03 from the real binary while signed out, through Tcode's
production path (`cargo run -p agent --example probe -- --list-models cursor`
and `probe cursor …`, with a wrapper that copied the stdio), in a sandbox
(`HOME` redirected, `AGENT_CLI_CREDENTIAL_STORE=file`, `NO_OPEN_BROWSER=1`).
Nothing was edited.

- `recorded_signed_out.jsonl` — what the binary wrote: the `initialize` result
  (line 1), then `-32000 Authentication required` for
  `cursor/list_available_models` (line 2) and for `session/new` (line 3).
- `status_signed_out.json` — `cursor-agent status --format json`.

No signed-in session has been recorded.

## Source-derived

Hand-built from the shipped JavaScript of the same build, not recorded:
they protect Tcode's side of the shapes as read, not that Cursor sends them.
Line numbers refer to the build's `3351.index.js` (the ACP server) after
`prettier --parser babel` 3.9.9, unless named otherwise. Model names, model
parameters, paths and texts are illustrative: Cursor's catalog comes from its
backend.

- `status_signed_in.json` — the `status` command's signed-in report
  (`8657.index.js`, `status:"authenticated"`, `userInfo`).
- `cursor-agent` — the stand-in the tests launch: `--version` and `status`
  answer from these fixtures, anything else relays stdio to the test.

`source_derived.json`, by key:

- `session_new` — the `session/new` result (1257-1262): modes (1910-1932),
  models (1845-1856) and, with `parameterizedModelPicker`, `configOptions`
  (1821-1844): the `mode` and `model` selects (1682-1704) and one select per
  model parameter (1649-1671), booleans as `"false"`/`"true"` (1518-1540),
  categorised `thought_level` or `model_config` (1504-1517, 1656-1658, 2011).
- `set_model_gpt5`, `set_reasoning_high` — `session/set_config_option`
  answers with the full refreshed `configOptions` (1412-1446); a new model
  brings its own parameters (1705-1731), a parameter keeps the model
  (1733-1756).
- `list_available_models` — `cursor/list_available_models` (1457-1458,
  1575-1592).
- `session_load` — the `session/load` result (1354-1358), sent after the
  replay (1359-1366).
- `load_replay` — the replay `session/load` sends first (276-377): the user
  message, thoughts, each tool call as `tool_call` plus a completed
  `tool_call_update` with id `replay-<turn>-<step>` (348-356, 3667-3684), a
  question as its tool card (4443-4446, 4578-4585, `think` 4940), and a
  subagent's lifecycle (361-374, 3012-3027).
- `available_commands` — `available_commands_update` (257-275), sent as soon
  as `session/new` or `session/load` has answered (1264-1268, 1361-1365).
- `turn_tools` — a live turn: `tool_call` (3630-3646, 3784-3801), then
  `in_progress` (3746-3749) and `completed` with `content`, `rawOutput`,
  `locations` (3753-3773). Shell: title, kind, input and
  `rawOutput {exitCode, stdout, stderr}` (4309-4311, 4922, 4502-4503,
  4757-4765). Edit: diff content (4345-4348, 4929, 4511-4515, 4638-4641,
  4679-4731). MCP: `{rejected: true}` (4403-4406, 4932, 4524-4530,
  4836-4846). Grep: `{totalMatches, truncated}` (4313-4340, 4925,
  4786-4805). Text and thoughts (3624-3629, 3686-3689, 3777-3781).
- `run_error` — a thrown agent run becomes the assistant chunk
  `"\n\nError: " + String(e)` (714-734), and the prompt still answers
  `end_turn` (409-426).
- `ask_question` — `cursor/ask_question` params (2039-2049); the reply shapes
  are read at 2053-2075.
- `create_plan_entries`, `create_plan` — the `plan` update sent first
  (2265-2288) and `cursor/create_plan` params (2331-2353); replies read at
  2353-2377.
- `update_todos`, `update_todos_merge` — `cursor/update_todos`, sent once the
  todo tool completes (3840-3856), statuses from 3902-3915.
- `permission_read`, `permission_edit`, `permission_shell` —
  `session/request_permission` with `allow-once`, `allow-always`,
  `reject-once` (2634-2663); title and kind per operation (2684-2753).
- `subagent_turn` — a task tool (4433-4438, 4936, 4552-4559, 4868-4877), its
  `subagent_spawned` and `subagent_state_update` sent under the parent session
  (3158-3189), and the subagent's own updates under its session id
  (2956-2981, 3114-3131).
- `task` — `cursor/task` (3858-3873).
