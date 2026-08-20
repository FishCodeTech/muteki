---
name: muteki-blackboard
description: >
  Shared team blackboard for a CTF/pentest solver swarm. ALWAYS use this skill
  whenever you are solving a challenge as part of a team — before starting any new
  direction (check what teammates already ruled out), when you confirm a fact (write
  it so teammates benefit), and when you hit a dead end (mark it so nobody retries
  it). Use it whenever a task mentions a "blackboard", "shared notes", "teammates",
  "the board", "what others found", "intents", or coordinating with other agents.
  Reading the board first saves you from repeating work others already proved
  impossible.
---

# Team Blackboard

You are ONE worker in a swarm. Your teammates are other AI agents working the same
challenge. You do **not** talk to them directly — you coordinate through a shared
**blackboard** (a fact/intent graph). The `blackboard.py` script in this skill is
your interface to it.

## When to use it (this is the important part)

Run these at the RIGHT moments — not constantly, not never:

1. **Before you start a direction** — check what's already been ruled out:
   ```
   python3 blackboard.py read-deadends
   python3 blackboard.py read-review
   ```
   If your idea is already on the dead-end list, suppressed by Review-Arbiter, or
   depends on a challenged fact, pick a different angle or prove/disprove the
   challenged fact first. This is the single highest-value call — it stops you
   wasting time on a proven dead end or a loop the review worker already diagnosed.

2. **When you're stuck or switching angles** — see what teammates confirmed:
   ```
   python3 blackboard.py read-facts
   python3 blackboard.py read-routes
   python3 blackboard.py read-branches
   ```
   A fact someone else verified (a leaked cred, a service version, a decoded
   intermediate) may be exactly the stepping stone you need.
   A suppressed route means the swarm has seen enough evidence to stop repeating
   that approach until new evidence appears. A branch is a forked hypothesis: work
   one branch cleanly instead of mixing incompatible assumptions.

   On a **multi-flag** challenge, also check which flags are already recovered so
   you go after the missing ones instead of re-finding a teammate's:
   ```
   python3 blackboard.py read-flags
   ```

3. **The moment you CONFIRM something in real output** — write it back:
   ```
   python3 blackboard.py write-fact "admin:admin logs in at /login (302 -> /dashboard)" --verified
   ```
   Use `--verified` only for things you saw in REAL command output. Drop it for a
   strong hypothesis you haven't proven. Keep facts short and objective.

4. **When you rule a direction out** — mark it dead so nobody retries:
   ```
   python3 blackboard.py mark-deadend "no SQLi on /search — all params parameterized"
   ```

5. **If you were assigned to pick up open work** — claim an intent first:
   ```
   python3 blackboard.py list-intents
   python3 blackboard.py claim I3
   ```
   `claim` prints `WON` (it's yours) or `LOST` (a teammate beat you — pick another).
   `list-intents` only shows ACTIVE intents — paused/retired directions are hidden,
   so anything you see is genuinely claimable.

6. **Before destructive / exclusive work** (remote RCE, a reverse-shell listener, a
   relay, an exclusive shell, a rate-limited account) — claim the RESOURCE so two
   workers don't collide on the same target/port/account:
   ```
   python3 blackboard.py read-resource-locks
   python3 blackboard.py claim-resource "destructive:tcp:445@172.22.11.45" --risk-class destructive
   ...do the work...
   python3 blackboard.py release-resource "destructive:tcp:445@172.22.11.45"
   ```
   `claim-resource` prints `WON` (exclusive access) or `LOST` (a teammate holds it —
   do not run conflicting work). Resource keys are resource-only:
   `risk_class:transport:port@host`.

7. **Check operator directives** — the operator can steer the swarm. Their guidance
   is the highest-priority instruction (still guidance, not proven evidence):
   ```
   python3 blackboard.py read-directives
   ```

8. **Submit a recovered Flag through the Blackboard API** — this is the only path
   that can complete a CTF task:
   ```
   python3 blackboard.py submit-flag '<exact flag>'
   ```
   Submit only after the Flag appeared in real command output or a real artifact.
   The API records a candidate; the owning Worker validates it against captured
   execution output before the Coordinator accepts it. Writing `FOUND_FLAG=` or
   placing a Flag in ordinary assistant text does not submit or accept it.

## Rules

- Query the board with intent, then get back to running real commands. Do **not**
  dump the whole board into your reasoning or browse it aimlessly.
- Only `--verified` facts that came from real output. The swarm's planner trusts
  verified facts; a hallucinated "fact" poisons everyone's plan.
- Writing a dead-end is as valuable as writing a fact — it's how the swarm avoids
  going in circles.
- Never use ordinary reply text or `FOUND_FLAG=` as a submission mechanism. Call
  `submit-flag` once for every distinct candidate recovered from real output.
- Treat Review-Arbiter output as control guidance, not ground truth. A challenged
  fact is temporarily unsafe to rely on; a suppressed route should be avoided unless
  you have fresh evidence that reopens it.
- The board persists across workers: a worker that starts after you will read what
  you wrote. That's the whole point — you're building a shared map.

## Teammate mode (f11 agent-teams)

When your prompt hat says you are a named teammate on a team (e.g. `exploit-1` on
`team-<challenge>`), you are in **teammate mode**. Always pass `--mode=teammate`;
in this mode the full-board read subcommands (`read-facts`, `read-routes`,
`read-branches`, …) are **not registered at all** — your world is your task, your
mailbox, and evidence-backed assertions, not the whole board.

Your identity comes from `$MUTEKI_TEAM_MEMBER` (set it to your teammate name).
Loop: `heartbeat` (~every 30s while working) → `msg-check` → `task-list` →
`task-claim` → real tools → `task-done`/`assert-write`/`msg-send`.

```
# liveness (protocol frame, free — do it often)
python3 blackboard.py --mode=teammate heartbeat

# inbox + team channel digest (pull-style; never dumps the raw channel)
python3 blackboard.py --mode=teammate msg-check
python3 blackboard.py --mode=teammate msg-check --digest

# shared task list: claim atomically (WON/LOST), finish with evidence
python3 blackboard.py --mode=teammate task-list --status pending
python3 blackboard.py --mode=teammate task-claim task-abc123
python3 blackboard.py --mode=teammate task-done task-abc123 \
    --evidence artifact:ws/poc/resp.txt:sha256:deadbeef...

# direct messages (typed; evidence-kind messages REQUIRE --evidence; hop≤2)
python3 blackboard.py --mode=teammate msg-send --kind=evidence --to=verify-1 \
    --body="login param looks injectable" \
    --evidence artifact:ws/poc/login_resp.txt:sha256:...

# team channel: ONLY evidence|dead_end|surprise|request_help (no coordination,
# no chatter); ≤12 posts per member per challenge
python3 blackboard.py --mode=teammate msg-send --kind=channel \
    --channel-kind=surprise --body="unexpected Set-Cookie on 403"

# register a file as citable evidence first
python3 blackboard.py --mode=teammate artifact-put ws/poc/login_resp.txt

# order-sensitive chain steps: block on the turn token, attach the printed
# fence to token-gated claims
python3 blackboard.py --mode=teammate token-wait --protocol=chain-handoff-1 --timeout=60
python3 blackboard.py --mode=teammate task-claim task-xyz --token-id=tt-... --token-fence=7
```

Hard rules enforced by the server (not by your good intentions): messages past
`msg_cap` or an open circuit breaker are rejected unless they are evidence-class;
`hop>2`, evidence-less evidence/dead_end/contradiction messages, and off-whitelist
channel kinds are rejected; `task-done` and `assert-write` without `--evidence`
are rejected.
