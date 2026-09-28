---
name: muteki-blackboard
description: Shared Fact-Goal-Step state and result submission for Muteki Workers.
---

# Muteki Blackboard

Use `$MUTEKI_BLACKBOARD_SCRIPT` to share conclusions and results. The task prompt
already contains the complete shared state. Refresh it when useful:

```bash
python3 "$MUTEKI_BLACKBOARD_SCRIPT" context
```

The graph may cite an original tool output by artifact ID. Read the complete
persisted text when the exact output matters:

```bash
python3 "$MUTEKI_BLACKBOARD_SCRIPT" read-artifact '<artifact ID>'
```

Draft a confirmed, reusable conclusion for the next Decide pass. A working
entry, payload, credential, or channel may be a Fact; an incomplete attempt or
an untested inference is not. Publish confirmed new information promptly.

```bash
python3 "$MUTEKI_BLACKBOARD_SCRIPT" submit-fact '<title>' '<content>'
```

The draft is not published to the shared graph. Calling `submit-fact` again
overwrites the previous draft. If you have a confirmed Fact, publish it and
conclude the current Step atomically:

```bash
python3 "$MUTEKI_BLACKBOARD_SCRIPT" commit-step
```

`commit-step` ends the Worker immediately. Do not call another tool afterwards.
When no Fact was confirmed, finish with an accurate handoff. The host records
the outcome and concludes the Step without inventing a Fact.

Record a bounded direction ruled out by a real test:

```bash
python3 "$MUTEKI_BLACKBOARD_SCRIPT" mark-deadend '<reason>' \
  --tested '<tested scope>' --observed '<actual result>'
```

Request operator input only for an external resource or environment problem:

```bash
python3 "$MUTEKI_BLACKBOARD_SCRIPT" request-input '<specific required input>'
```

Save a reusable script or file:

```bash
python3 "$MUTEKI_BLACKBOARD_SCRIPT" save-poc ./path \
  --entry-command '<command>' --status available --note '<purpose>'
```

CTF Workers submit a value obtained from real output immediately:

```bash
python3 "$MUTEKI_BLACKBOARD_SCRIPT" submit-flag '<exact flag>'
```

Put ordinary reusable files under `./shared/`. For a later Step that depends on
an exact file version, use `save-poc` so the shared graph can name that resource.
Describe live sessions, listeners, tunnels and proxy endpoints with their actual
health and reuse instructions only when confirmed.

Pentest Workers use the same Fact–Goal–Step loop. Before submitting a Fact,
list this Step's tool artifacts and inspect the selected original output when
needed:

```bash
python3 "$MUTEKI_BLACKBOARD_SCRIPT" recent-evidence
python3 "$MUTEKI_BLACKBOARD_SCRIPT" read-artifact '<artifact ID>'
python3 "$MUTEKI_BLACKBOARD_SCRIPT" submit-fact '<title>' '<content>' --evidence '<artifact ID>'
python3 "$MUTEKI_BLACKBOARD_SCRIPT" commit-step
```

The title describes the observed fact in natural language. The host validates
the selected artifact, Worker identity, current Step, authorized target, and
atomic commit; Decide judges whether the Fact satisfies the user's goal.
Workers may consult public documentation and vulnerability references online;
those sources guide hypotheses but do not count as evidence from the authorized target.

Review Workers use their review commands only when the assigned role explicitly
requests a review.
