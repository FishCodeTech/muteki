"""Asynchronous initial titles for Conversation Threads.

The event hook only schedules work. Runtime completion and SSE delivery never
wait for the Thread's selected model. A persisted revision/turn fence ensures
that a late response cannot overwrite a manual rename or metadata whose
basis turn has been replaced.
"""

from __future__ import annotations

import asyncio
import json
import logging
from typing import Awaitable, Callable, Optional

from apps.web.titler import (
    ConversationTitle,
    generate_conversation_title_with_runtime,
)
from muteki.external_agents.factory import engine_for_adapter
from muteki.external_agents.registry import AdapterRegistry
from muteki.conversation import events as ev
from muteki.conversation.manager import ConversationManager
from muteki.platform.contracts.events import EventEnvelope
from muteki.platform.store import PlatformStore
from muteki.platform.contracts.objects import Artifact


LOG = logging.getLogger(__name__)

TitleGenerator = Callable[..., Awaitable[Optional[ConversationTitle]]]

class ConversationMetadataService:
    """Title the first user request without blocking the conversation Runtime."""

    def __init__(
        self,
        store: PlatformStore,
        manager: ConversationManager,
        *,
        sessions_root: str,
        registry: Optional[AdapterRegistry] = None,
        generator: TitleGenerator = generate_conversation_title_with_runtime,
    ) -> None:
        self._store = store
        self._manager = manager
        self._sessions_root = sessions_root
        self._registry = registry
        self._generator = generator
        self._tasks: dict[str, asyncio.Task[None]] = {}
        self._event_hook = self.consume

        hooks = getattr(store, "_conversation_projection_hooks", None)
        if hooks is None:
            raise RuntimeError(
                "ConversationMetadataService requires ConversationService first"
            )
        hooks.append(self._event_hook)

    def consume(self, event: EventEnvelope) -> None:
        """Schedule metadata work from normalized public Conversation events."""
        if event.aggregate_type != ev.AGGREGATE_THREAD:
            return
        turn_id = str(event.payload.get("turn_id") or "").strip()
        if not turn_id:
            return
        thread = self._manager.get_thread(event.aggregate_id)
        if thread is None or thread.mode != "conversation":
            return
        if event.event_type in {
            ev.EV_TURN_RETRIED,
            ev.EV_TURN_EDIT_RESENT,
            ev.EV_TURN_FAILED,
            ev.EV_TURN_INTERRUPTED,
        }:
            replaced = event.payload.get("superseded_turn_ids") or []
            if (thread.metadata_turn_id == turn_id
                    or thread.metadata_turn_id in replaced
                    or thread.metadata_turn_id == event.payload.get("retry_of_turn_id")):
                self._cancel(event.aggregate_id)
            return
        if event.event_type != ev.EV_TURN_REQUESTED:
            return
        if thread.title_source != "fallback":
            return
        # Initial titles describe the opening request. A later turn neither
        # regenerates metadata nor cancels an initial title still in flight.
        first = next((turn for turn in self._manager.conv.list_current_turns(
            event.aggregate_id
        ) if self._is_user_request(turn)), None)
        if first is None or first.turn_id != turn_id:
            return
        if thread.metadata_turn_id == turn_id:
            return
        try:
            self._manager.reserve_thread_metadata(event.aggregate_id, turn_id)
        except Exception:  # noqa: BLE001 - event delivery stays isolated
            LOG.exception("failed to reserve conversation title: %s", event.aggregate_id)
        else:
            self._schedule_update(event.aggregate_id, turn_id)

    @staticmethod
    def _is_user_request(turn: object) -> bool:
        invocation = getattr(turn, "runtime_invocation", {}) or {}
        return invocation.get("kind") != "command" and bool(
            str(getattr(turn, "text", "") or "").strip()
            or getattr(turn, "attachments", [])
            or getattr(turn, "capability_refs", [])
        )

    def _schedule_update(self, thread_id: str, turn_id: str) -> None:
        try:
            loop = asyncio.get_running_loop()
        except RuntimeError:
            LOG.warning(
                "skip conversation metadata outside an async loop: %s",
                thread_id,
            )
            return
        self._cancel(thread_id)
        task = loop.create_task(
            self._update(thread_id, turn_id),
            name=f"conversation-metadata:{thread_id}:{turn_id}",
        )
        self._tasks[thread_id] = task
        task.add_done_callback(
            lambda done, thread_id=thread_id: self._finish(
                thread_id, done
            )
        )

    def _cancel(self, thread_id: str) -> None:
        task = self._tasks.pop(thread_id, None)
        if task is not None and not task.done():
            task.cancel()

    def _finish(self, thread_id: str, task: asyncio.Task[None]) -> None:
        if self._tasks.get(thread_id) is task:
            self._tasks.pop(thread_id, None)
        try:
            task.result()
        except asyncio.CancelledError:
            pass
        except Exception:  # noqa: BLE001 - background work must stay isolated
            LOG.exception("conversation metadata task failed: %s", thread_id)

    def _opening_request(self, turn: object) -> str:
        rows = [str(getattr(turn, "text", "") or "")]
        for digest in getattr(turn, "attachments", []):
            artifact = self._store.get(Artifact, digest)
            details = (
                {"sha256": artifact.sha256, "name": artifact.name,
                 "media_type": artifact.media_type, "size": artifact.size,
                 "kind": artifact.kind}
                if artifact is not None else {"sha256": digest, "metadata": "unavailable"}
            )
            rows.append("Attachment metadata: " + json.dumps(details, ensure_ascii=False))
        refs = getattr(turn, "capability_refs", [])
        if refs:
            rows.append("Selected context: " + json.dumps(refs, ensure_ascii=False))
        return "\n".join(rows)

    async def _update(self, thread_id: str, turn_id: str) -> None:
        thread = self._manager.get_thread(thread_id)
        turn = self._manager.conv.get_turn(turn_id)
        if thread is None or turn is None or turn.thread_id != thread_id:
            return
        if thread.title_source != "fallback" or not self._is_user_request(turn):
            return
        runtime = self._manager.runtime_selection(thread_id)
        if not runtime.adapter_id:
            LOG.warning(
                "conversation metadata skipped without a selected runtime: %s",
                thread_id,
            )
            return

        # The event hook reserved this revision before scheduling. Do not
        # reserve again: a manual rename in between must invalidate this job.
        reserved = thread
        if reserved.metadata_turn_id != turn_id:
            return
        transcript = await asyncio.to_thread(self._opening_request, turn)
        from muteki.core.usage import usage_context
        metadata = None
        for attempt, delay in enumerate((0, 2, 4)):
            if delay:
                await asyncio.sleep(delay)
            current = self._manager.get_thread(thread_id)
            if (current is None or current.title_source != "fallback"
                    or current.metadata_revision != reserved.metadata_revision
                    or current.metadata_turn_id != turn_id):
                return
            try:
                with usage_context(self._sessions_root, thread_id=thread_id, turn_id=turn_id,
                                   workspace_kind="conversation", actor_kind="auxiliary", role="titler",
                                   credential_id=runtime.credential_id,
                                   speed={"priority": "fast", "fast": "fast", "ultrafast": "ultrafast"}.get(
                                       runtime.service_tier, "standard")):
                    metadata = await self._generate_title(
                        transcript, runtime=runtime, thread_id=thread_id, turn_id=turn_id,
                    )
            except asyncio.CancelledError:
                raise
            except Exception:  # noqa: BLE001 - title failures stay isolated from turns
                LOG.exception(
                    "conversation title generation failed: thread_id=%s turn_id=%s "
                    "engine=%s model=%s attempt=%s",
                    thread_id, turn_id, engine_for_adapter(runtime.adapter_id),
                    runtime.model, attempt + 1,
                )
            if metadata is not None and metadata.title:
                break
            LOG.warning(
                "conversation title returned no result: thread_id=%s turn_id=%s "
                "engine=%s model=%s attempt=%s",
                thread_id, turn_id, engine_for_adapter(runtime.adapter_id),
                runtime.model, attempt + 1,
            )
        if metadata is None or not metadata.title:
            return

        updated = self._manager.apply_generated_thread_metadata(
            thread_id,
            expected_revision=reserved.metadata_revision,
            turn_id=turn_id,
            title=metadata.title,
        )
        if updated is None:
            return
        self._store.append_events(ev.thread_event(
            thread_id,
            ev.EV_THREAD_METADATA_UPDATED,
            {
                "thread_id": thread_id,
                "turn_id": turn_id,
                "title": updated.title,
                "title_source": updated.title_source,
                "summary": updated.summary,
                "metadata_revision": updated.metadata_revision,
                "runtime": {
                    "adapter_id": runtime.adapter_id,
                    "instance_id": runtime.instance_id,
                    "credential_id": runtime.credential_id,
                    "model": runtime.model,
                    "effort": runtime.effort,
                },
            },
            actor_id="system",
        ))

    async def _generate_title(
        self,
        transcript: str,
        *,
        runtime: object,
        thread_id: str,
        turn_id: str,
    ) -> Optional[ConversationTitle]:
        """Generate a title with the model selected for this conversation."""
        kwargs = {
            "runtime": runtime,
            "sessions_root": self._sessions_root,
            "thread_id": thread_id,
            "turn_id": turn_id,
        }
        if self._generator is generate_conversation_title_with_runtime:
            kwargs["chat_runtime"] = True
        if (
            self._registry is not None
            and self._generator is generate_conversation_title_with_runtime
        ):
            kwargs["adapter"] = self._registry.get(
                str(getattr(runtime, "adapter_id", "") or ""),
                str(getattr(runtime, "instance_id", "") or "default"),
            )
        return await self._generator(transcript, **kwargs)

    async def shutdown(self) -> None:
        hooks = getattr(self._store, "_conversation_projection_hooks", [])
        if self._event_hook in hooks:
            hooks.remove(self._event_hook)
        tasks = list(self._tasks.values())
        self._tasks.clear()
        for task in tasks:
            if not task.done():
                task.cancel()
        if tasks:
            await asyncio.gather(*tasks, return_exceptions=True)


__all__ = ["ConversationMetadataService"]
