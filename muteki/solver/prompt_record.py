"""Read-only prompt telemetry; never changes the invocation sent to a worker."""
from __future__ import annotations

import asyncio
import json
import re
import threading
from typing import Any
from urllib.parse import quote
from uuid import uuid4

from muteki.core.events import EventType


class PromptArgv(list[str]):
    """Keep the exact prompt beside argv without changing runner signatures."""

    def __init__(self, argv: list[str], prompt: str, session: str | None, kind: str):
        super().__init__(argv)
        self.prompt = prompt
        self.session = session or ""
        self.kind = kind


_SECRET_ENV = re.compile(
    r"(?:^|_)(?:API_KEY|ACCESS_KEY|TOKEN|SECRET|PASSWORD|PASSWD|CREDENTIAL|PRIVATE_KEY)(?:$|_)",
    re.IGNORECASE,
)


def redact_prompt(text: str, secrets: list[str]) -> tuple[str, bool]:
    """Mask explicitly materialized secrets before the bus or durable log sees them."""
    variants: set[str] = set()
    for value in secrets:
        if not isinstance(value, str) or not value:
            continue
        variants.update((value, json.dumps(value, ensure_ascii=False)[1:-1], quote(value, safe="")))
    original = text
    for value in sorted(variants, key=len, reverse=True):
        text = text.replace(value, "[REDACTED]")
    return text, text != original


class PromptRecorder:
    """Observe the existing process/stdin handoff callbacks, including failed starts.

    Callbacks can run on the runner thread. Only the event loop publishes, and
    finish() drains its queued receipts before the worker's terminal event.
    """

    def __init__(self, worker: Any, argv: list[str], env: dict, stdin_text: str | None):
        self.worker = worker
        self.loop = asyncio.get_running_loop()
        self.lock = threading.Lock()
        self.status = "prepared"
        self.pending: list[Any] = []
        self.payload: dict[str, Any] | None = None
        self.closed = False
        if not isinstance(argv, PromptArgv) or worker.bus is None:
            return
        secrets = list(getattr(worker, "_control_secret_values", []) or [])
        secrets.extend(str(value) for key, value in env.items() if value and _SECRET_ENV.search(str(key)))
        prompt, redacted = redact_prompt(argv.prompt, secrets)
        identity = getattr(worker, "identity", {}) or {}
        self.payload = {
            "prompt_id": f"prompt-{uuid4().hex}",
            "prompt": prompt,
            "redacted": redacted,
            "kind": argv.kind,
            "transport": "stdin" if stdin_text is not None else "argv",
            "session": argv.session,
            "engine": str(worker.driver.name),
            "model": str(identity.get("model") or ""),
            "intent_id": str(getattr(worker, "intent_id_assigned", "") or getattr(worker, "_intent_id", "") or ""),
            "phase": str(getattr(worker, "mode", "") or ""),
            "status": "prepared",
        }

    async def _emit(self, payload: dict[str, Any]) -> None:
        try:
            await self.worker._emit(EventType.WORKER_PROMPT, **payload)
        except Exception:
            # Observability failures must not alter execution or delivery fences.
            pass

    async def prepare(self) -> None:
        if self.payload is not None:
            await self._emit(self.payload)

    def update(self, status: str) -> None:
        if self.payload is None:
            return
        with self.lock:
            if self.closed or self.status != "prepared":
                return
            self.status = status
            receipt = {"prompt_id": self.payload["prompt_id"], "status": status}
            self.pending.append(asyncio.run_coroutine_threadsafe(self._emit(receipt), self.loop))

    async def finish(self) -> None:
        self.update("not_sent")
        with self.lock:
            self.closed = True
            pending = tuple(self.pending)
        if pending:
            await asyncio.gather(*(asyncio.wrap_future(item) for item in pending), return_exceptions=True)
