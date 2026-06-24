# Muteki Command Deck (web UI)

The web command deck — a **dumb subscriber** to the agent event stream. It never
calls the solver core; it consumes the typed SSE event stream and POSTs HITL
commands. The event schema (`lib/events.ts`, mirroring `muteki/core/events.py`)
is the only contract.

## The model: conversation-first, canvas only for spatial views

The deck is a **conversation-first** operator console. The home screen carries
the run snapshot, main timeline, and scalable worker dock. Heavy spatial views
open as a focused canvas beside it:

1. **③ Conversation (the spine).** A ChatGPT/Claude-style thread. Task dispatch
   is *conversational*: describe a challenge in the composer — the swarm reads it
   and **infers** category, target, and how many solvers to race (no form). The
   swarm's reasoning, tool calls, and insight broadcasts stream in as bubbles;
   your input **commands the live swarm** (hint / redirect / focus / pause /
   resume / submit) through the HITL backend.
2. **① Fact graph (artifact canvas).** Opened from the pinned run card. The live,
   evolving DAG — `challenge → solver → candidate/verified fact → intent →
   dead-end → flag` — rendered with Cytoscape + dagre. Click a node for its
   provenance verdict.
3. **② Blackboard (artifact canvas).** A React Flow sticky-note workspace for
   shared intents, verified facts, dead ends, and the found flag.

The fact graph and blackboard are **one artifact canvas, one at a time** — it
slides in beside the thread and closes again. Summary metrics stay on the home
screen instead of living behind a separate statistics tab.

```
ThreadRail (run list) │ Conversation (spine) │ ArtifactPanel (graph / blackboard)
```

## Run

The deck is the **Next.js app** (Cytoscape fact graph, React Flow blackboard,
hot-reload, xterm.js terminal). The FastAPI backend is API-only — it serves the
SSE/`/api` contract the deck consumes.

```bash
./run.sh web          # starts the backend (:8000) AND the Next UI (:3001)
# open http://localhost:3001
```

Or run them separately:

```bash
uv run uvicorn apps.web.server:create_app --factory --port 8000   # backend (API only)
cd apps/web/ui && npm install
# point the browser straight at the backend (the Next dev proxy buffers SSE):
NEXT_PUBLIC_MUTEKI_API=http://127.0.0.1:8000 npm run dev          # http://localhost:3001
# production: npm run build
```

## Backend contract used by the deck

- `GET  /api/runs` — rich per-run summaries (name/category/solved/flag) for the
  thread rail.
- `POST /api/runs` — mint a fresh run id for "+ New solve".
- `POST /api/runs/{id}/start` — dispatch. Accepts a conversational `prompt`; the
  backend infers `challenge.{category,target,name}` when structured fields are
  absent (caller-provided fields always win).
- `GET  /api/runs/{id}/events` — the typed SSE event stream (Last-Event-ID resume).
- `POST /api/runs/{id}/hitl` — a human command into the live run.

## Files

- `lib/events.ts` — typed event model + reducer (`reduce`) folding events → deck state.
- `lib/useRun.ts` — `EventSource` subscription hook + `useRunList` (rail) + `newRun`.
- `app/page.tsx` — the conversation-first shell orchestration.
- `app/globals.css` — the shell layout + all component tokens/styles.
- `components/ThreadRail.tsx` — the left run list.
- `components/Conversation.tsx` — the spine: welcome/dispatch, transcript, pinned
  `RunCard`, dual-mode `Composer`, `HitlCard`.
- `components/ArtifactPanel.tsx` — the canvas host (graph or blackboard).
- `components/GraphView.tsx` — the Cytoscape + dagre fact graph.
- `components/Blackboard.tsx` — the sticky-note collaboration board.
- `components/ContextGauge.tsx` plus `NodeInspector` — shared detail widgets.
- `components/Chat.tsx`, `components/LaunchForm.tsx` — legacy (pre-redesign),
  kept for reference; no longer imported by the shell.
