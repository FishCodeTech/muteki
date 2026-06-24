"""Launchable TUI entrypoint:  `uv run python -m apps.tui [options]`.

`apps/tui/app.py` only defines the `MutekiTUI` widget — it has no runner, so
`python -m apps.tui.app` just loads classes and exits. THIS module is the real
launcher: it wires an EventBus + a background driver (mock or real swarm) to the
TUI and runs it.

Modes:
  (no args)            mock driver — scripted event stream, NO API key, UI demo.
  --swarm KEY          solve a real NYU-bench challenge by key (needs a key).
  --swarm --desc "…" --target URL --category web
                       solve an ad-hoc challenge described inline.

The TUI is a dumb subscriber (§3): it renders the bus and routes HITL commands
back into the run. Same contract the web deck uses.
"""

from __future__ import annotations

import argparse
import asyncio
from pathlib import Path

from muteki.core.cost import CostController
from muteki.core.dotenv_boot import load_env
from muteki.core.event_bus import EventBus
from muteki.core.events import Event, EventType, hitl_response_payload

from apps.tui.app import MutekiTUI

load_env()  # pick up repo-root .env so --swarm finds the key (shell env wins)


def _parse(argv: list[str] | None = None) -> argparse.Namespace:
    p = argparse.ArgumentParser(prog="python -m apps.tui",
                                description="Project Muteki TUI command deck")
    p.add_argument("--swarm", action="store_true",
                   help="run the real solver swarm (needs MUTEKI_DEEPSEEK_API_KEY)")
    p.add_argument("--key", default="",
                   help="NYU-bench challenge key to solve (with --swarm)")
    p.add_argument("--desc", default="", help="ad-hoc challenge description")
    p.add_argument("--target", default="", help="ad-hoc challenge target URL/host")
    p.add_argument("--category", default="web",
                   help="track: web/crypto/reverse/forensics/misc/pwn")
    p.add_argument("--n-solvers", type=int, default=2, help="swarm size")
    return p.parse_args(argv)


async def _mock_driver(bus: EventBus, cost: CostController, run_id: str) -> None:
    from examples.mock_solver import run_mock_solve

    await run_mock_solve(bus, cost, run_id=run_id)


async def _swarm_driver(bus: EventBus, cost: CostController, run_id: str,
                        args: argparse.Namespace) -> None:
    import os
    import tempfile

    from muteki.core.llm import LLMClient
    from muteki.learning.distill import TemplateStore
    from muteki.models.solve_graph import Challenge
    from muteki.sandbox.manager import SandboxManager
    from muteki.solver.result import ArtifactStore
    from muteki.solver.types import SolverConfig
    from muteki.swarm.models import default_lineup
    from muteki.swarm.swarm import Swarm

    challenge = Challenge(
        id=run_id, name=args.key or args.desc[:32] or run_id,
        category=args.category, points=0, description=args.desc,
        target=args.target or None,
        flag_format=r"[A-Za-z0-9_]{0,15}\{[^}]{1,200}\}",
    )
    root = Path(tempfile.mkdtemp(prefix="muteki-tui-"))
    sandbox = SandboxManager(bus=bus, root=root / "sbx")
    arts = ArtifactStore(root=root / "arts")
    knowledge = TemplateStore(root=os.environ.get("MUTEKI_KNOWLEDGE_DIR", "knowledge"))
    async with LLMClient(cost=cost, bus=bus) as llm:
        swarm = Swarm(
            challenge, default_lineup(args.n_solvers), llm=llm, sandbox=sandbox,
            bus=bus, cost=cost, artifacts=arts, config=SolverConfig(),
            run_id=run_id, knowledge=knowledge,
            executor="cli", cli_race=True,
        )
        try:
            await swarm.run()
        finally:
            await sandbox.shutdown_all()


async def _amain(args: argparse.Namespace) -> None:
    run_id = "tui-run"
    bus = EventBus()
    cost = CostController(bus=bus)

    if args.swarm:
        lineup = f"swarm×{args.n_solvers} ({args.category})"
        driver = _swarm_driver(bus, cost, run_id, args)
    else:
        lineup = "mock (UI demo — pass --swarm to solve for real)"
        driver = _mock_driver(bus, cost, run_id)

    async def _run_driver() -> None:
        try:
            await driver
        finally:
            await bus.close()

    driver_task = asyncio.create_task(_run_driver())

    async def hitl(target: str, action: str, text: str) -> None:
        await bus.emit(Event(
            event_type=EventType.HITL_RESPONSE, run_id=run_id,
            payload=hitl_response_payload(target, action, text=text),
        ))

    app = MutekiTUI(bus, hitl=hitl, lineup=lineup, stop_on_finish=False)
    try:
        await app.run_async()
    finally:
        driver_task.cancel()
        await asyncio.gather(driver_task, return_exceptions=True)


def main(argv: list[str] | None = None) -> int:
    args = _parse(argv)
    asyncio.run(_amain(args))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
