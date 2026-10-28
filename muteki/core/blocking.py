"""Run blocking work without releasing its owner's resources on cancellation."""

from __future__ import annotations

import asyncio
from typing import Callable, ParamSpec, TypeVar

P = ParamSpec("P")
T = TypeVar("T")


async def run_blocking(function: Callable[P, T], *args: P.args, **kwargs: P.kwargs) -> T:
    """Offload a synchronous operation and join it before propagating cancellation.

    Cancelling ``to_thread`` cannot stop the worker. Owners of locks, temporary
    directories and file handles must wait for that worker before releasing them.
    Exceptions from the operation remain visible, including during cancellation.
    """
    worker = asyncio.create_task(asyncio.to_thread(function, *args, **kwargs))
    try:
        return await asyncio.shield(worker)
    except asyncio.CancelledError:
        while not worker.done():
            try:
                await asyncio.shield(worker)
            except asyncio.CancelledError:
                continue
        worker.result()
        raise
