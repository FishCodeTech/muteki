import os
from pathlib import Path
import tempfile
import unittest
from concurrent.futures import ThreadPoolExecutor

from muteki.solver.btw_history import BtwHistory


class BtwHistoryTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.history = BtwHistory(self.root / 'run-one')

    def test_completed_turn_survives_new_store(self):
        turn = self.history.begin('question', engine='codex', model='fake-model')
        self.history.update(turn, 'answer', 'completed')
        rows = BtwHistory(self.root / 'run-one').list()
        self.assertEqual(len(rows), 1)
        self.assertEqual(rows[0]['question'], 'question')
        self.assertEqual(rows[0]['answer'], 'answer')
        self.assertEqual(rows[0]['model'], 'fake-model')
        self.assertEqual(os.stat(self.history.path).st_mode & 0o777, 0o600)

    def test_delete_turn_keeps_other_turns(self):
        first = self.history.begin('first', engine='codex', model='fake')
        second = self.history.begin('second', engine='codex', model='fake')
        self.assertEqual(self.history.delete(first), 1)
        self.assertEqual([r['id'] for r in self.history.list()], [second])
        self.assertEqual(self.history.delete(first), 0)

    def test_clear_and_late_update_do_not_resurrect_records(self):
        turn = self.history.begin('first', engine='codex', model='fake')
        self.history.update(turn, 'partial', 'incomplete')
        self.assertEqual(self.history.delete(), 1)
        self.history.update(turn, 'late final', 'completed')
        self.assertEqual(self.history.list(), [])

    def test_runs_are_isolated(self):
        self.history.begin('one', engine='codex', model='fake')
        other = BtwHistory(self.root / 'run-two')
        self.assertEqual(other.list(), [])
        other.delete()
        self.assertEqual(len(self.history.list()), 1)

    def test_only_retained_completed_turns_enter_context(self):
        first = self.history.begin('first', engine='codex', model='fake')
        self.history.update(first, 'answer', 'completed')
        second = self.history.begin('second', engine='codex', model='fake')
        self.history.update(second, 'partial', 'failed')
        self.assertEqual(self.history.transcript(), [
            {'role': 'user', 'content': 'first'}, {'role': 'assistant', 'content': 'answer'}])
        self.history.delete(first)
        self.assertEqual(self.history.transcript(), [])
        self.assertEqual(self.history.list()[0]['answer'], 'partial')

    def test_context_is_bounded_without_deleting_history(self):
        for i in range(3):
            turn = self.history.begin(str(i), engine='codex', model='fake')
            self.history.update(turn, 'abc', 'completed')
        self.assertEqual(self.history.transcript(max_chars=4), [
            {'role': 'user', 'content': '2'}, {'role': 'assistant', 'content': 'abc'}])
        self.assertEqual(len(self.history.list()), 3)

    def test_cancelled_or_crashed_turn_is_not_shown_as_completed(self):
        turn = self.history.begin('first', engine='codex', model='fake')
        self.assertEqual(BtwHistory(self.root / 'run-one').list()[0]['status'], 'incomplete')
        self.history.update(turn, 'partial', 'cancelled')
        self.assertEqual(self.history.list()[0]['status'], 'cancelled')

    def test_concurrent_writers_keep_all_turns(self):
        self.history.begin('initial', engine='codex', model='fake')
        with ThreadPoolExecutor(max_workers=4) as pool:
            list(pool.map(lambda i: self.history.begin(str(i), engine='codex', model='fake'), range(12)))
        self.assertEqual(len(self.history.list()), 13)

    def test_missing_history_read_does_not_create_database(self):
        self.assertEqual(self.history.list(), [])
        self.assertEqual(self.history.delete(), 0)
        self.assertFalse(self.history.path.exists())
