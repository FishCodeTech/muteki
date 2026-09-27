"""#216: PlatformStore must serialize shared-connection execute→fetchall.

ConversationStore already routes through PlatformStore.lock (#138). Several
PlatformStore read paths still called ``_conn.execute(...).fetchall()`` without
holding that lock; concurrent readers + writers corrupt the sqlite/python heap
and surface as SIGTRAP in unicode_decode during fetchall or SIGSEGV in GC.
This regression exercises concurrent list/get/save/public_events under the
shared lock — it is not a native-crash repro.
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


class PlatformStoreThreadSafetyTests(unittest.TestCase):
    def test_fetch_helpers_hold_platform_lock(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = PlatformStore(db_path=Path(tmp) / "platform.db")
            held: list[str] = []
            original = store._lock

            class SpyLock:
                def __enter__(self_inner):
                    held.append("enter")
                    return original.__enter__()

                def __exit__(self_inner, *exc):
                    held.append("exit")
                    return original.__exit__(*exc)

                def acquire(self_inner, *a, **k):
                    return original.acquire(*a, **k)

                def release(self_inner):
                    return original.release()

            store._lock = SpyLock()  # type: ignore[assignment]
            try:
                store._fetchone("SELECT 1")
                store._fetchall("SELECT 1")
            finally:
                store._lock = original
            self.assertGreaterEqual(held.count("enter"), 2)
            self.assertGreaterEqual(held.count("exit"), 2)
            store.close()

    def test_concurrent_unlocked_read_paths_and_writes(self) -> None:
        """Hammer former unlocked reads alongside streaming writes."""
        with tempfile.TemporaryDirectory() as tmp:
            platform = PlatformStore(db_path=Path(tmp) / "platform.db")
            conv = ConversationStore(platform)
            thread_id = new_id("thr")
            turn_id = new_id("turn")
            conv.save_state(ThreadState(thread_id=thread_id))

            errors: list[BaseException] = []
            stop = threading.Event()

            def capture(fn) -> None:  # noqa: ANN001
                try:
                    fn()
                except BaseException as exc:  # noqa: BLE001
                    errors.append(exc)
                    stop.set()

            def writer() -> None:
                for i in range(250):
                    if stop.is_set():
                        return
                    platform.append_events(
                        thread_event(
                            thread_id,
                            EV_MESSAGE_DELTA,
                            {
                                "turn_id": turn_id,
                                "role": "assistant",
                                "text": f"chunk-{i}\n",
                            },
                        )
                    )
                    existing = conv.get_message(f"asst-{turn_id}")
                    text = (existing.text if existing else "") + f"chunk-{i}\n"
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

            def unlocked_style_readers() -> None:
                for _ in range(250):
                    if stop.is_set():
                        return
                    platform.pending_receipts()
                    platform.snapshot()
                    platform.stream_head("thread", thread_id)
                    platform.public_events_for(
                        "thread", thread_id, after_seq=0, limit=40,
                    )
                    platform.get_receipt("missing-cmd")
                    platform.effects_for_command("missing-cmd")
                    platform.list_effect_receipts(limit=10)
                    platform.get_extension("missing-ext")
                    platform.command_domain("missing-cmd")

            def sse_reader() -> None:
                cursor = 0
                for _ in range(250):
                    if stop.is_set():
                        return
                    page = platform.public_events_for(
                        "thread", thread_id, after_seq=cursor, limit=40,
                    )
                    if page:
                        cursor = max(cursor, page[-1][0])

            threads = [
                threading.Thread(target=lambda: capture(writer)),
                threading.Thread(target=lambda: capture(unlocked_style_readers)),
                threading.Thread(target=lambda: capture(sse_reader)),
            ]
            for t in threads:
                t.start()
            for t in threads:
                t.join(timeout=90)
                self.assertFalse(t.is_alive(), "worker thread hung")

            self.assertEqual(errors, [], f"concurrent store errors: {errors!r}")
            msg = conv.get_message(f"asst-{turn_id}")
            self.assertIsNotNone(msg)
            assert msg is not None
            self.assertIn("chunk-0", msg.text)
            self.assertIn("chunk-249", msg.text)
            platform.close()


if __name__ == "__main__":
    unittest.main()
