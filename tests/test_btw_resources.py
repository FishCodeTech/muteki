import asyncio
from pathlib import Path
import sqlite3
import tempfile
import threading
import unittest
from unittest.mock import patch
from muteki.core.blocking import run_blocking
from muteki.solver.btw_resources import snapshot_context, cleanup_turn


class BtwResourcesTest(unittest.IsolatedAsyncioTestCase):
    async def test_snapshot_keeps_event_loop_responsive_and_source_database_unchanged(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            events, db = root / 'source.jsonl', root / 'source.db'
            events.write_text('test\n')
            with sqlite3.connect(db) as connection:
                connection.execute('CREATE TABLE data (value TEXT)')
                connection.execute("INSERT INTO data VALUES ('original')")
            entered, release = threading.Event(), threading.Event()
            def slow_snapshot():
                entered.set()
                if not release.wait(2):
                    raise AssertionError('event loop did not progress')
                return snapshot_context(root / 'scratch', events, db)
            task = asyncio.create_task(run_blocking(slow_snapshot))
            while not entered.is_set():
                await asyncio.sleep(0.001)
            release.set()
            copied_events, copied_db = await task
            self.assertEqual(copied_events.read_text(), 'test\n')
            with sqlite3.connect(copied_db) as connection:
                self.assertEqual(connection.execute('SELECT value FROM data').fetchone()[0], 'original')
            with sqlite3.connect(db) as connection:
                self.assertEqual(connection.execute('SELECT value FROM data').fetchone()[0], 'original')

    async def test_cleanup_only_removes_this_turn(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            token = 'a' * 32
            work = root / 'workers' / '_btw' / ('codex-' + token)
            home = root / '.muteki-agent-state' / ('btw-codex-' + token)
            container_home = root / 'homes' / home.name
            main_home = root / 'homes' / 'main'
            main = root / 'workers' / 'main'
            sibling = work.with_name('codex-' + 'b' * 32)
            for path in (work, home, container_home, main_home, main, sibling):
                path.mkdir(parents=True)
                (path / 'keep').write_text('fake')
            await run_blocking(cleanup_turn, root, work, home.name)
            self.assertFalse(work.exists())
            self.assertFalse(home.exists())
            self.assertFalse(container_home.exists())
            self.assertTrue((main_home / 'keep').exists())
            self.assertTrue((main / 'keep').exists())
            self.assertTrue((sibling / 'keep').exists())
            with self.assertRaises(ValueError):
                cleanup_turn(root, main, home.name)

    async def test_cleanup_rejects_symlink(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            work = root / 'workers' / '_btw' / ('codex-' + 'a' * 32)
            work.parent.mkdir(parents=True)
            target = root / 'main'
            target.mkdir()
            work.symlink_to(target, target_is_directory=True)
            with self.assertRaises(ValueError):
                cleanup_turn(root, work, 'btw-' + work.name)
            self.assertTrue(target.exists())

    async def test_cleanup_rejects_container_home_symlink_without_touching_main(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            label = 'btw-codex-' + 'a' * 32
            work = root / 'workers' / '_btw' / label.removeprefix('btw-')
            work.mkdir(parents=True)
            main = root / 'homes' / 'main'
            main.mkdir(parents=True)
            (main / 'keep').write_text('main credentials')
            (root / 'homes' / label).symlink_to(main, target_is_directory=True)
            with self.assertRaises(ValueError):
                cleanup_turn(root, work, label)
            self.assertEqual((main / 'keep').read_text(), 'main credentials')
