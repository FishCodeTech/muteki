"""Stopped-run follow-ups must retain the run's original Worker identity."""
import asyncio
import copy
import unittest
from types import SimpleNamespace
from unittest.mock import AsyncMock, patch
from pathlib import Path

from apps.web.drivers import _standby_identity_from_workers, build_standby_driver
from muteki.core.events import EventType


class StandbyIdentityTest(unittest.TestCase):
    def setUp(self):
        self.workers = {
            'cli-codex': {'engine': 'codex', 'profile_id': 'codex-original', 'session': 'old-session'},
            'cli-codex-2': {'engine': 'codex', 'profile_id': 'codex-other'},
        }

    def test_stopped_run_uses_first_original_profile_with_fresh_session(self):
        before = copy.deepcopy(self.workers)
        result = _standby_identity_from_workers({}, self.workers, '')
        self.assertEqual(result['engine'], 'codex')
        self.assertEqual(result['profile_id'], 'codex-original')
        self.assertEqual(result['session'], '')
        self.assertEqual(self.workers, before)

    def test_previous_standby_cannot_contaminate_selection(self):
        workers = {'cli-claude-standby': {'engine': 'claude', 'profile_id': 'wrong'}, **self.workers}
        self.assertEqual(_standby_identity_from_workers({}, workers, '')['engine'], 'codex')

    def test_winner_event_takes_precedence(self):
        result = _standby_identity_from_workers({}, self.workers, 'cli-codex-2')
        self.assertEqual(result['profile_id'], 'codex-other')

    def test_private_winner_identity_is_preserved(self):
        winner = {'engine': 'claude', 'profile_id': 'winning-claude', 'session': 'winner-session'}
        self.assertEqual(_standby_identity_from_workers(winner, self.workers, ''), winner)

    def test_empty_persisted_fields_are_recovered_for_same_worker(self):
        result = _standby_identity_from_workers(
            {'worker_id': 'cli-codex', 'engine': '', 'profile_id': ''}, self.workers, '')
        self.assertEqual(result['profile_id'], 'codex-original')
        self.assertEqual(result['session'], 'old-session')

    def test_no_identity_fails_instead_of_defaulting_to_claude(self):
        with self.assertRaisesRegex(RuntimeError, 'identity is unavailable'):
            _standby_identity_from_workers({}, {}, '')

    def test_claude_original_profile_is_supported(self):
        workers = {'cli-claude': {'engine': 'claude', 'profile_id': 'claude-original'}}
        self.assertEqual(_standby_identity_from_workers({}, workers, '')['profile_id'], 'claude-original')

    def test_ask_and_writeup_replay_stopped_run_identity(self):
        # Exercise the real driver entry point through event replay and recovery.
        # Stop at Challenge construction, before workspace writes or CLI startup.
        class StopBeforeRuntime(Exception):
            pass
        async def replay(run_id):
            yield SimpleNamespace(event_type=EventType.RUN_STARTED, solver_id=None,
                                  payload={'challenge': {'name': 'synthetic'}})
            for worker_id, fields in self.workers.items():
                yield SimpleNamespace(event_type=EventType.WORKER_STATUS,
                                      solver_id=worker_id, payload={**fields, 'online': True})
        for action in ('ask', 'writeup'):
            with self.subTest(action=action):
                manager = SimpleNamespace(
                    _ensure_control_async=AsyncMock(return_value=(None, None, None)),
                    storage=SimpleNamespace(workspace=lambda run_id: Path('/unused')),
                    load_winner_continuation=lambda run_id: {},
                    worker_config=SimpleNamespace(resolve=lambda category: {'worker_profiles': [
                        {'id': row['profile_id'], 'engine': row['engine']} for row in self.workers.values()]}),
                )
                run = SimpleNamespace(run_id='stopped-run', name='synthetic', category='web',
                                      store=SimpleNamespace(replay=replay))
                with patch('muteki.models.solve_graph.Challenge', side_effect=StopBeforeRuntime), \
                     patch('muteki.solver.followup_profiles.configured_standby_identity', wraps=__import__(
                         'muteki.solver.followup_profiles', fromlist=['configured_standby_identity']).configured_standby_identity) as select:
                    with self.assertRaises(StopBeforeRuntime):
                        asyncio.run(build_standby_driver({'action': action, 'worker_id': 'cli-codex-2'}, mgr=manager)(run))
                self.assertEqual(select.call_args.kwargs['requested'], 'cli-codex-2')
                self.assertEqual(len(select.call_args.kwargs['participants']), 2)

    def test_driver_rejects_global_profile_that_never_participated(self):
        class StopBeforeRuntime(Exception):
            pass
        async def replay(run_id):
            if False:
                yield None
        cfg = {'worker_profiles': [{'id': 'dedicated', 'engine': 'codex', 'enabled': True}],
               'stage_policy': {'coordinator': {'standby_profile': 'dedicated'}}}
        manager = SimpleNamespace(
            _ensure_control_async=AsyncMock(return_value=(None, None, None)),
            storage=SimpleNamespace(workspace=lambda run_id: Path('/unused')),
            load_winner_continuation=lambda run_id: {},
            worker_config=SimpleNamespace(resolve=lambda category: cfg))
        run = SimpleNamespace(run_id='stopped-run', name='synthetic', category='web',
                              store=SimpleNamespace(replay=replay))
        for action in ('ask', 'writeup'):
            with self.subTest(action=action), \
                 patch('apps.web.drivers._standby_identity_from_workers') as recover, \
                 patch('muteki.models.solve_graph.Challenge', side_effect=StopBeforeRuntime):
                from muteki.solver.followup_profiles import FollowupSelectionError
                with self.assertRaises(FollowupSelectionError):
                    asyncio.run(build_standby_driver({'action': action}, mgr=manager)(run))
                recover.assert_not_called()


if __name__ == '__main__':
    unittest.main()
