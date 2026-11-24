# Usage fixtures

`usage_recorded.jsonl` and `compaction_recorded.jsonl` were captured on
2026-09-08 from new Claude Code 2.1.263 sessions in `/tmp/tcode-usage-provider-evidence`,
with `TCODE_DATA_DIR` pointing to a throwaway profile. No existing session was
opened or resumed. The probe used `--no-session-persistence`, `--verbose`,
`--output-format stream-json`, and `--include-partial-messages`.

The first prompt asked for two separate Bash `printf` calls and a DONE response.
Only message IDs (replaced with stable fixture IDs), model names, usage and result
accounting fields were retained. Prompts, content, initialization payloads,
environment, stderr, account identifiers and credentials were not saved.
The independently calculated latest input/cache contexts are 20,250, 20,635 and
20,764. Turn traffic is 6 + 10,840 + 50,803 + 435 = 62,084. Repeated assistant
blocks share usage; their output values are placeholders, while stream deltas
are cumulative within each request.

The second probe used `--input-format stream-json`, sent a new harmless printf
turn, then `/compact` in that same newly created process. The recorded lifecycle
is compacting → compact boundary, with manual trigger, 19,555 pre-tokens, 2,948
post-tokens, 16,607 cumulative dropped tokens and 24,455 ms duration. Transcript
UUIDs in preserved-segment metadata were omitted. Post-tokens describe the
compaction result; they are not a fresh request context observation.

`usage_scope.jsonl` and `usage_resume.jsonl` are constructed protocol fixtures,
not live captures. They protect >capacity turn traffic, output-only deltas,
repeated message/result IDs, main/subagent routing, a fresh process on an existing
timeline, compaction and interrupted completion. Their expected counts are
literal assertions in the production adapter → timeline → composer replay.

Contract references:
- https://code.claude.com/docs/en/agent-sdk/cost-tracking
- https://platform.claude.com/docs/en/build-with-claude/streaming
- https://code.claude.com/docs/en/statusline#context-window-fields

The exact reporter's 2.5M/4.1M run and automatic compaction were not captured.
The fixtures deliberately distinguish observed evidence from constructed cases.

A separate read-only native account probe ran the existing ignored
`provider_usage::tests::live_provider_usage` test explicitly. Both Codex and
Claude returned non-empty windows with no error; only normalized usage output
was captured outside the repository. The unsupported endpoint and temporary
failure cases remain deterministic local fixtures, not claims about a live
custom account.
