"""Asynchronous model-authored metadata for Conversation Threads.

The event hook only schedules work. Runtime completion and SSE delivery never
wait for the Thread's selected model. A persisted revision/turn fence ensures
that a late response cannot overwrite a manual rename or metadata based on a
newer turn.
"""

from __future__ import annotations

import asyncio
import logging
import re
from typing import Awaitable, Callable, Optional

from apps.web.titler import (
    ConversationMetadata,
    generate_conversation_metadata,
    generate_conversation_metadata_with_runtime,
)
from apps.web.llm_credentials import resolve_llm_profile_credential
from muteki.conversation import events as ev
from muteki.conversation.manager import ConversationManager
from muteki.platform.contracts.events import EventEnvelope
from muteki.platform.store import PlatformStore


LOG = logging.getLogger(__name__)

MetadataGenerator = Callable[..., Awaitable[Optional[ConversationMetadata]]]

_GREETING_ONLY = re.compile(
    r"^(?:你好|您好|嗨|哈喽|在吗|早上好|下午好|晚上好|"
    r"hi|hello|hey|yo|good\s+(?:morning|afternoon|evening))"
    r"[\s!！,.，。?？~～]*$",
    re.IGNORECASE,
)


def is_informative_message(text: str) -> bool:
    """Return whether an opening message contains more than a greeting."""
    value = re.sub(r"\s+", " ", str(text or "").strip())
    return bool(value and not _GREETING_ONLY.fullmatch(value))


class ConversationMetadataService:
    """Generate Thread metadata when informative turns start and complete."""

    def __init__(
        self,
        store: PlatformStore,
        manager: ConversationManager,
        *,
        sessions_root: str,
        worker_config: object | None = None,
        generator: MetadataGenerator = generate_conversation_metadata_with_runtime,
        titler_generator: MetadataGenerator = generate_conversation_metadata,
    ) -> None:
        self._store = store
        self._manager = manager
        self._sessions_root = sessions_root
        self._worker_config = worker_config
        self._generator = generator
        self._titler_generator = titler_generator
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
        if event.event_type == ev.EV_TURN_REQUESTED:
            turn_id = str(event.payload.get("turn_id") or "").strip()
            self._cancel(event.aggregate_id)
            if turn_id:
                try:
                    self._manager.reserve_thread_metadata(
                        event.aggregate_id, turn_id
                    )
                except Exception:  # noqa: BLE001 - event delivery stays isolated
                    LOG.exception(
                        "failed to invalidate stale conversation metadata: %s",
                        event.aggregate_id,
                    )
                else:
                    self._schedule_update(event.aggregate_id, turn_id)
            return
        if event.event_type == ev.EV_TURN_RETRIED:
            self._cancel(event.aggregate_id)
            return
        if event.event_type != ev.EV_TURN_COMPLETED:
            return
        turn_id = str(event.payload.get("turn_id") or "").strip()
        if not turn_id:
            return
        self._schedule_update(event.aggregate_id, turn_id)

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

    def _transcript(self, thread_id: str) -> str:
        thread = self._manager.get_thread(thread_id)
        messages = self._manager.conv.list_current_messages(thread_id)
        rows: list[str] = []
        if thread is not None and thread.summary:
            rows.append(f"已有摘要：{thread.summary}")
        labels = {"user": "用户", "assistant": "助手", "system": "系统"}
        for message in messages:
            text = re.sub(r"\s+", " ", str(message.text or "").strip())
            if not text:
                continue
            rows.append(f"{labels.get(message.role, message.role)}：{text}")
        return "\n".join(rows)

    async def _update(self, thread_id: str, turn_id: str) -> None:
        thread = self._manager.get_thread(thread_id)
        turn = self._manager.conv.get_turn(turn_id)
        if thread is None or turn is None or turn.thread_id != thread_id:
            return
        if not thread.summary and not is_informative_message(turn.text):
            return
        runtime = self._manager.runtime_selection(thread_id)
        if not runtime.adapter_id:
            LOG.warning(
                "conversation metadata skipped without a selected runtime: %s",
                thread_id,
            )
            return

        reserved = self._manager.reserve_thread_metadata(thread_id, turn_id)
        generate_title = reserved.title_source == "fallback"
        transcript = self._transcript(thread_id)
        from muteki.core.usage import usage_context
        with usage_context(self._sessions_root, thread_id=thread_id, turn_id=turn_id,
                           workspace_kind="conversation", actor_kind="auxiliary", role="titler"):
            metadata = await self._generate_metadata(
                transcript,
                runtime=runtime,
                current_title=reserved.title,
                generate_title=generate_title,
            )
        if metadata is None:
            LOG.warning("conversation metadata model returned no usable result")
            return

        updated = self._manager.apply_generated_thread_metadata(
            thread_id,
            expected_revision=reserved.metadata_revision,
            turn_id=turn_id,
            title=metadata.title if generate_title else "",
            summary=metadata.summary,
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

    async def _generate_metadata(
        self,
        transcript: str,
        *,
        runtime: object,
        current_title: str,
        generate_title: bool,
    ) -> Optional[ConversationMetadata]:
        """Use the single-task titler first, then the Thread runtime."""
        profile: dict[str, object] = {}
        if self._worker_config is not None:
            try:
                config = self._worker_config.get()  # type: ignore[attr-defined]
                profile = dict((config.get("llm_profiles") or {}).get("titler") or {})
                credential = resolve_llm_profile_credential(
                    "titler",
                    profile,
                    sessions_root=self._sessions_root,
                )
                metadata = await self._titler_generator(
                    transcript,
                    current_title=current_title,
                    generate_title=generate_title,
                    model=profile.get("model"),
                    base_url=credential.base_url or None,
                    api_key=credential.api_key or None,
                    temperature_mode=profile.get("temperature_mode"),
                    temperature=profile.get("temperature"),
                )
                if metadata is not None:
                    return metadata
                LOG.warning(
                    "conversation metadata titler returned no usable result; "
                    "falling back to the selected runtime"
                )
            except asyncio.CancelledError:
                raise
            except Exception:  # noqa: BLE001 - fallback is the intended behavior
                LOG.warning(
                    "conversation metadata titler unavailable; falling back to "
                    "the selected runtime",
                    exc_info=True,
                )

        return await self._generator(
            transcript,
            runtime=runtime,
            sessions_root=self._sessions_root,
            current_title=current_title,
            generate_title=generate_title,
        )

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


__all__ = ["ConversationMetadataService", "is_informative_message"]
