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

## Rules

- Query the board with intent, then get back to running real commands. Do **not**
  dump the whole board into your reasoning or browse it aimlessly.
- Only `--verified` facts that came from real output. The swarm's planner trusts
  verified facts; a hallucinated "fact" poisons everyone's plan.
- Writing a dead-end is as valuable as writing a fact — it's how the swarm avoids
  going in circles.
- Treat Review-Arbiter output as control guidance, not ground truth. A challenged
  fact is temporarily unsafe to rely on; a suppressed route should be avoided unless
  you have fresh evidence that reopens it.
- The board persists across workers: a worker that starts after you will read what
  you wrote. That's the whole point — you're building a shared map.
