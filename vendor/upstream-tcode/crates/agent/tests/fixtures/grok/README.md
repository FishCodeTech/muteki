# Grok session fixtures

Each fixture is one Grok Build session recorded on the wire in both
directions, in wire order, untruncated: one JSON object per line,
`{"from": "client" | "agent", "message": <JSON-RPC message>}`.

## `resumed_turn.jsonl`

### Provenance

- CLI: `grok 1.0.46 (2765805b9442)`, darwin-arm64, in a throwaway `HOME` and
  `GROK_HOME`.
- Model: a local scripted stand-in for the xAI Responses API
  (`GROK_XAI_API_BASE_URL` / `GROK_CLI_CHAT_PROXY_BASE_URL` pointing at it,
  `XAI_API_KEY` a placeholder). No xAI account or live model took part; every
  model turn below was scripted, and each model call reports 1,200 input
  tokens (1,000 cached) and 34 output tokens.
- Client: Tcode's production session path, through the probe:

  ```sh
  cargo run -p agent --example probe -- grok "Count, edit, ask and echo" <cwd> auto_edits \
      --resume '{"session_id":"01a10095-e65c-7983-a817-6884b010f1ae"}' --effort low \
      --mcp tcode_probe http://127.0.0.1:18433/mcp tcode-secret
  ```

  with a pass-through recorder as `grok` on `PATH`. Grok was launched as
  `grok --permission-mode default agent --reasoning-effort low stdio`.

### Scenario

An earlier process created the session with one turn; it is not recorded.
This process resumes it (`session/resume`, then `session/set_mode default`)
and runs one turn whose scripted model calls are:

1. a reasoning summary, then `run_terminal_command` printing three lines one
   second apart. Grok asks permission; the probe approves once.
2. `run_terminal_command` `touch quiet.txt`, which Grok runs unprompted and
   which prints nothing.
3. `search_replace` in `in.txt`. Grok asks permission; Tcode approves it once
   itself (AutoAcceptEdits).
4. `ask_user_question` with two options; the probe answers with the first.
5. `use_tool` calling `echo_upper` on a local HTTP MCP server registered as
   `tcode_probe` with a bearer header. Grok asks permission; the probe
   approves once.
6. A final message.

The probe then shuts the session down (`session/close`).

### Sanitization

The `initialize` result's `hostname` is `fixture-host` and its `agentId` and
`agentInstanceId` are zero UUIDs; the scratch directory is renamed to
`/tmp/grok-fixture` in all its forms (`/private/tmp/…`, URL-encoded). Nothing
else was changed. The `/Users/admin/actions-runner/…` paths are Grok's own
built-in workflow metadata.

## `agent_started_turn.jsonl`

Same CLI, backend and recorder as above, through the probe:

```sh
cargo run -p agent --example probe -- grok "Background then foreground" <cwd> full_access \
    --linger 3 --follow-up 4 "And now?"
```

Grok was launched as `grok --permission-mode bypassPermissions agent stdio`.
A new session runs one sent turn whose scripted model calls start
`sleep 1; echo bgdone` in the background (`block_until_ms: 0`), run
`sleep 3; echo fg` in the foreground, and answer. The background task
finishes during that turn, so as soon as the turn ends Grok runs a prompt of
its own, `task-completed-<task id>` (`_x.ai/queue/changed` reports it running
before the sent turn's `session/prompt` result arrives), and the scripted
model answers it. Four seconds after the first turn completed, the probe sends
a second turn, which the scripted model answers; the probe then closes the
session. Sanitized as above.
