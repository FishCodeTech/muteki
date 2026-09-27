"""#138: long-stream InterfaceError from dual locks on shared sqlite conn.

ConversationStore used to own a private RLock while PlatformStore held a
different one for the same connection. Concurrent MESSAGE_DELTA persistence
(projection) + SSE ``public_events_for`` + queue pause then raced into
``sqlite3.InterfaceError: bad parameter or other API misuse``, which the
executor mapped to ``conversation.turn.executor_error`` and truncated the
reply.
"""

from __future__ import annotations

import tempfile
import threading
import unittest
from pathlib import Path

from muteki.conversation.events import EV_MESSAGE_DELTA, thread_event
from muteki.conversation.models import ConversationMessage, ThreadState
from muteki.conversation.store import ConversationStore
from muteki.platform.contracts.base import new_id
from muteki.platform.store import PlatformStore


class StreamSqliteLockTests(unittest.TestCase):
    def test_conversation_store_shares_platform_lock(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            platform = PlatformStore(db_path=Path(tmp) / "platform.db")
            conv = ConversationStore(platform)
            self.assertIs(conv._lock, platform.lock)

    def test_concurrent_stream_writes_and_sse_reads_no_interface_error(self) -> None:
        """Reproduce the #138 race: deltas + SSE poll + queue pause."""
        with tempfile.TemporaryDirectory() as tmp:
            platform = PlatformStore(db_path=Path(tmp) / "platform.db")
            conv = ConversationStore(platform)
            thread_id = new_id("thr")
            turn_id = new_id("turn")
            conv.save_state(ThreadState(thread_id=thread_id))

            errors: list[BaseException] = []
            stop = threading.Event()

            def capture(label: str, fn) -> None:  # noqa: ANN001
                try:
                    fn()
                except BaseException as exc:  # noqa: BLE001 — test harness
                    errors.append(exc)
                    stop.set()

            def writer_deltas() -> None:
                for i in range(400):
                    if stop.is_set():
                        return
                    platform.append_events(
                        thread_event(
                            thread_id,
                            EV_MESSAGE_DELTA,
                            {
                                "turn_id": turn_id,
                                "role": "assistant",
                                "text": f"line-{i}\n",
                            },
                        )
                    )
                    # Projection-style growing assistant message, same path as
                    # long streaming turns.
                    existing = conv.get_message(f"asst-{turn_id}")
                    text = (existing.text if existing else "") + f"line-{i}\n"
                    conv.save_message(
                        ConversationMessage(
                            message_id=f"asst-{turn_id}",
                            thread_id=thread_id,
                            turn_id=turn_id,
                            role="assistant",
                            text=text,
                            stream_seq=i + 1,
                        )
                    )

            def sse_reader() -> None:
                cursor = 0
                for _ in range(400):
                    if stop.is_set():
                        return
                    page = platform.public_events_for(
                        "thread", thread_id, after_seq=cursor, limit=50,
                    )
                    if page:
                        cursor = max(cursor, page[-1][0])

            def queue_pause_spam() -> None:
                for _ in range(200):
                    if stop.is_set():
                        return
                    conv.pause_queue(thread_id, "user_pause_auto_send")
                    conv.get_state(thread_id)

            threads = [
                threading.Thread(target=lambda: capture("deltas", writer_deltas)),
                threading.Thread(target=lambda: capture("sse", sse_reader)),
                threading.Thread(target=lambda: capture("pause", queue_pause_spam)),
            ]
            for t in threads:
                t.start()
            for t in threads:
                t.join(timeout=60)

            interface = [
                e for e in errors
                if type(e).__name__ == "InterfaceError"
                or "bad parameter" in str(e).lower()
                or "api misuse" in str(e).lower()
            ]
            self.assertEqual(
                interface,
                [],
                f"sqlite InterfaceError under concurrent stream I/O: {errors!r}",
            )
            self.assertEqual(errors, [], f"unexpected concurrent errors: {errors!r}")
            msg = conv.get_message(f"asst-{turn_id}")
            self.assertIsNotNone(msg)
            assert msg is not None
            self.assertIn("line-0", msg.text)
            self.assertIn("line-399", msg.text)


if __name__ == "__main__":
    unittest.main()
