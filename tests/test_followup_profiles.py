import copy
import json
from pathlib import Path
import tempfile
import unittest

from apps.web.worker_config import WorkerConfigStore
from muteki.solver.followup_profiles import (
    configured_standby_identity, require_followup_profile, select_btw_profile,
)


class FollowupProfilesTest(unittest.TestCase):
    def setUp(self):
        self.codex = dict(id='codex-a', name='codex-a', engine='codex', enabled=True,
                          credential_account='fake-a', model='model-a', roles=['respond'])
        self.other = {**self.codex, 'id': 'codex-b', 'name': 'codex-b',
                      'credential_account': 'fake-b', 'model': 'model-b'}
        self.claude = dict(id='claude-a', name='claude-a', engine='claude', enabled=True,
                           credential_account='fake-c', roles=['review'])
        self.cfg = dict(worker_profiles=[self.codex, self.other, self.claude],
                        stage_policy={'coordinator': {'review': {'engine': 'claude-a'}}})
        self.policy = self.cfg['stage_policy']['coordinator']

    def participants(self):
        return [dict(worker_id=f"worker-{index}", profile_id=p['id'], engine=p['engine'],
                     model=p.get('model', ''), account_id=p.get('credential_account', ''))
                for index, p in enumerate(self.cfg['worker_profiles'])]

    def select(self, requested='', winner=None):
        return select_btw_profile(self.cfg, winner or {}, requested, participants=self.participants())

    def test_three_participants_are_selectable_including_same_engine(self):
        for index, profile in enumerate(self.cfg['worker_profiles']):
            self.assertEqual(self.select(f'worker-{index}'), profile)

    def test_unlaunched_profile_and_bare_engine_are_rejected(self):
        participants = self.participants()[:1]
        for ref in ('codex-b', 'codex', 'missing'):
            with self.assertRaises(ValueError):
                select_btw_profile(self.cfg, {}, ref, participants=participants)

    def test_missing_or_disabled_profile_does_not_change_identity(self):
        self.codex['enabled'] = False
        with self.assertRaises(ValueError):
            self.select('worker-0')
        self.cfg['worker_profiles'].pop(0)
        with self.assertRaises(ValueError):
            select_btw_profile(self.cfg, {}, 'worker-0', participants=[dict(
                worker_id='worker-0', profile_id='codex-a', engine='codex')])

    def test_empty_roster_fails_without_fallback(self):
        with self.assertRaises(ValueError):
            select_btw_profile(self.cfg, {}, participants=[])

    def test_default_prefers_participating_winner_and_ignores_global_overrides(self):
        self.policy.update(btw_profile='deleted', standby_profile='deleted')
        self.assertEqual(self.select(winner={'worker_id': 'worker-1'}), self.other)

    def test_recorded_model_is_preserved_and_changed_account_rejected(self):
        participants = self.participants()
        self.codex['model'] = 'new-model'
        self.assertEqual(select_btw_profile(self.cfg, {}, 'worker-0', participants=participants)['model'], 'model-a')
        self.codex['credential_account'] = 'new-account'
        with self.assertRaises(ValueError):
            select_btw_profile(self.cfg, {}, 'worker-0', participants=participants)

    def test_standby_uses_selected_identity_without_resuming_solve_session(self):
        winner = dict(worker_id='worker-0', session='old', workdir_rel='old-dir', agent_state_rel='old-state')
        original = copy.deepcopy(winner)
        for action in ('ask', 'writeup'):
            result = configured_standby_identity(winner, self.cfg, action,
                participants=self.participants(), requested='worker-1')
            self.assertEqual(result['profile_id'], 'codex-b')
            self.assertEqual(result['worker_id'], 'worker-1')
            for key in ('session', 'workdir_rel', 'agent_state_rel'):
                self.assertEqual(result[key], '')
        self.assertEqual(winner, original)
        self.assertIsNone(configured_standby_identity(winner, self.cfg, 'mark_false', participants=[]))

    def test_only_actual_launch_events_establish_participation(self):
        from types import SimpleNamespace
        from muteki.core.events import EventType
        from muteki.solver.followup_profiles import participant_rows
        def event(worker, **payload):
            return SimpleNamespace(event_type=EventType.WORKER_STATUS, solver_id=worker,
                payload=dict(engine='codex', profile_id='codex-a', **payload))
        rows = participant_rows([event('configured-only', online=False),
            event('real-worker', online=True), event('real-worker', online=False),
            event('cli-codex-standby', online=True), event('respond', online=True, worker_role='respond')])
        self.assertEqual([row['worker_id'] for row in rows], ['real-worker'])

    def test_settings_preserve_new_fields_on_disk_and_resolve(self):
        with tempfile.TemporaryDirectory() as tmp:
            # No accounts or host credentials are needed to persist Profile refs.
            store = WorkerConfigStore(tmp)
            result = store.set(stage_policy={'coordinator': {
                'standby_profile': '', 'btw_profile': '',
                'review': {'enabled': False}, 'verifier': {'enabled': False}}})
            for key in ('standby_profile', 'btw_profile'):
                self.assertEqual(result['stage_policy']['coordinator'][key], '')
            data = json.loads(store.path.read_text())
            data['worker_profiles'] = [self.codex]
            data.pop('seats', None)
            data.pop('credentials', None)
            data['stage_policy']['coordinator'].update(standby_profile='codex-a', btw_profile='codex-a')
            store.path.write_text(json.dumps(data))
            reloaded = WorkerConfigStore(tmp)
            reloaded.set(stage_policy=data['stage_policy'])
            reloaded = WorkerConfigStore(tmp)
            for key in ('standby_profile', 'btw_profile'):
                self.assertEqual(reloaded.resolve('web')['stage_policy']['coordinator'][key], 'codex-a')

    def test_settings_reject_unavailable_and_non_string_references(self):
        for key in ('standby_profile', 'btw_profile'):
            with self.subTest(key=key):
                cfg = copy.deepcopy(self.cfg)
                cfg['stage_policy']['coordinator'][key] = 'missing'
                with self.assertRaises(ValueError):
                    WorkerConfigStore._validate_active_stage_channels(cfg)
                with self.assertRaises(ValueError):
                    WorkerConfigStore._validate_stage_limits({'coordinator': {key: 123}})

    def test_valid_settings_references_are_accepted(self):
        self.policy.update(standby_profile='codex-a', btw_profile='codex-b')
        WorkerConfigStore._validate_active_stage_channels(self.cfg)

    def test_selection_does_not_mutate_main_profiles(self):
        original = copy.deepcopy(self.cfg)
        profile = self.select('worker-1')
        profile['roles'].append('temporary')
        profile['credential_account'] = 'changed'
        self.assertEqual(self.cfg, original)
