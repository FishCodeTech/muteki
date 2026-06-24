"""Textual TUI command deck (§15 / Sprint 1.3).

In-process subscriber to an EventBus — no server needed (the kernel is Python,
§15.1). Renders:
  - a scrolling transcript (reasoning / tool calls / terminal / insights / flag)
  - a status bar (solver lineup, cost, last cost/context)
  - a command input (HITL: hint / pause / submit) routed to a callback

Esc requests interrupt; the transcript updates live as events stream. Designed
to be headless-testable via Textual's run_test() (see tests/test_tui.py).
"""

from __future__ import annotations

import asyncio
from typing import Awaitable, Callable, Optional

from textual.app import App, ComposeResult
from textual.containers import Vertical
from textual.widgets import Footer, Header, Input, RichLog, Static

from muteki.core.event_bus import EventBus
from muteki.core.events import Event, EventType

# callback(target, action, text) for HITL commands the user types
HitlSink = Callable[[str, str, str], Awaitable[None]]


def format_event(ev: Event) -> Optional[str]:
    """One transcript line for an event, or None to skip (noisy/empty)."""
    p = ev.payload
    sid = ev.solver_id or "-"
    et = ev.event_type
    if et is EventType.RUN_STARTED:
        ch = p.get("challenge", {})
        return f"[b]▶ RUN[/b] {ch.get('name','?')} [{ch.get('category','?')}]"
    if et is EventType.REASONING_DELTA:
        t = p.get("text", "").strip()
        return f"[dim]{sid} ⋯ {t}[/dim]" if t else None
    if et is EventType.TOOL_CALL_START:
        return f"[cyan]{sid} ⚙ {p.get('tool','?')}[/cyan]"
    if et is EventType.TOOL_CALL_RESULT:
        res = p.get("result") or {}
        cond = res.get("condensed", "") if isinstance(res, dict) else str(res)
        head = cond.splitlines()[0] if cond else "(result)"
        return f"  ↳ {head[:160]}"
    if et is EventType.TERMINAL_OUTPUT:
        return f"[green]{p.get('text','').rstrip()}[/green]"
    if et is EventType.SOLVE_GRAPH_DELTA:
        kind = p.get("kind")
        if kind == "evidence_added":
            return f"[yellow]✓ fact:[/yellow] {p.get('fact','')[:160]}"
        if kind == "flag":
            return f"[b green]⚑ FLAG {p.get('flag')}[/b green]"
        return None
    if et is EventType.SHARED_GRAPH_DELTA:
        # P-A/P-B: a fact entered the shared graph with its provenance verdict.
        fact = p.get("fact", "")[:160]
        if p.get("verified"):
            return f"[b yellow]✓✓ verified[/b yellow] {fact}"
        conf = p.get("confidence", 0.0)
        return f"[dim]? candidate (conf={conf:.1f}, needs verification):[/dim] {fact}"
    if et is EventType.REASON_INTENT:
        # P-C: the planner dispatched intents (+ evidence audit).
        intents = p.get("intents") or []
        if p.get("goal_met"):
            return "[b green]🧠 reason: goal met[/b green]"
        parts = [f"[b]🧠 reason → {len(intents)} intent(s)[/b]"]
        for it in intents[:4]:
            wc = it.get("worker_class", "code")
            parts.append(f"   [cyan]{wc}[/cyan] {it.get('goal','')[:120]}")
        for a in (p.get("audit") or [])[:3]:
            parts.append(f"   [yellow]⚠ audit:[/yellow] {a[:120]}")
        return "\n".join(parts)
    if et is EventType.INSIGHT_BUS_EVENT:
        return f"[magenta]📡 {p.get('kind')}: {p.get('flag') or p.get('text','')}[/magenta]"
    if et is EventType.STALLED:
        return f"[red]⚠ stalled: {p.get('reason','')}[/red]"
    if et is EventType.HITL_RESPONSE:
        return f"[b]» you → {p.get('target')}: {p.get('action')} {p.get('text','')}[/b]"
    if et is EventType.RUN_FINISHED:
        ok = p.get("solved")
        flags = p.get("flags") or ([p["flag"]] if p.get("flag") else [])
        exp = p.get("expected_flags") or 1
        shown = (f"flags={len(flags)}/{exp} {flags}" if exp > 1
                 else f"flag={p.get('flag')}")
        return f"[b]■ FINISHED solved={ok} {shown}[/b]"
    return None


class MutekiTUI(App):
    CSS = """
    #status { dock: top; height: 1; background: $panel; color: $text; }
    #transcript { height: 1fr; border: round $primary; }
    #cmd { dock: bottom; }
    """
    BINDINGS = [("escape", "interrupt", "Interrupt"), ("ctrl+c", "quit", "Quit")]

    def __init__(self, bus: EventBus, *, hitl: Optional[HitlSink] = None,
                 lineup: str = "", stop_on_finish: bool = False) -> None:
        super().__init__()
        self.bus = bus
        self.hitl = hitl
        self.lineup = lineup
        self.stop_on_finish = stop_on_finish
        self._usd = 0.0
        self._cost_by_solver: dict[str, float] = {}
        self._ctx = ""
        self._sub_task: Optional[asyncio.Task] = None
        self.last_line: str = ""
        self.finished = False

    def compose(self) -> ComposeResult:
        yield Header()
        yield Static(self._status_text(), id="status")
        yield Vertical(RichLog(id="transcript", markup=True, wrap=True))
        yield Input(placeholder="command: hint/pause/submit  (Enter to send, Esc to interrupt)", id="cmd")
        yield Footer()

    def _status_text(self) -> str:
        return f" lineup: {self.lineup or '—'}   cost: ${self._usd:.4f}   {self._ctx}"

    def on_mount(self) -> None:
        self._sub_task = asyncio.create_task(self._consume())

    async def _consume(self) -> None:
        log = self.query_one("#transcript", RichLog)
        async for ev in self.bus.subscribe(last_event_id=0):
            if ev.event_type is EventType.COST_UPDATE:
                # COST_UPDATE is almost always per-solver-scoped (each carries that
                # solver's running total). Summing the last-seen total per solver
                # gives the global figure — taking a single payload would track only
                # the agent that fired last (and a $0 subscription worker would zero
                # the bar). Falls back to the raw usd for any non-solver scope.
                p = ev.payload
                if p.get("scope") == "solver" and ev.solver_id:
                    self._cost_by_solver[ev.solver_id] = float(p.get("usd", 0.0))
                    self._usd = sum(self._cost_by_solver.values())
                else:
                    self._usd = max(float(p.get("usd", self._usd)),
                                    sum(self._cost_by_solver.values()))
                self.query_one("#status", Static).update(self._status_text())
            elif ev.event_type is EventType.CONTEXT_STATE:
                self._ctx = f"ctx {ev.payload.get('total',0)}/{ev.payload.get('limit',0)}"
                self.query_one("#status", Static).update(self._status_text())
            line = format_event(ev)
            if line:
                self.last_line = line
                log.write(line)
            if ev.event_type is EventType.RUN_FINISHED:
                self.finished = True
                if self.stop_on_finish:
                    self.exit()
                return

    async def on_input_submitted(self, event: Input.Submitted) -> None:
        text = event.value.strip()
        event.input.value = ""
        if not text or self.hitl is None:
            return
        # syntax: "[target] action rest"  — default target=global, action=hint
        action = "hint"
        target = "global"
        if text.startswith("/"):
            parts = text[1:].split(" ", 1)
            action = parts[0]
            text = parts[1] if len(parts) > 1 else ""
        await self.hitl(target, action, text)

    async def action_interrupt(self) -> None:
        if self.hitl is not None:
            await self.hitl("global", "interrupt", "")
