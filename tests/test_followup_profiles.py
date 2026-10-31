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

    def test_explicit_request_wins_and_preserves_account_model(self):
        self.policy['btw_profile'] = 'claude-a'
        self.assertEqual(select_btw_profile(self.cfg, {}, 'codex-b'), self.other)

    def test_dedicated_btw_overrides_review(self):
        self.policy['btw_profile'] = 'codex-a'
        self.assertEqual(select_btw_profile(self.cfg, {}), self.codex)

    def test_missing_explicit_profile_never_falls_back(self):
        with self.assertRaises(ValueError):
            select_btw_profile(self.cfg, {}, 'missing')

    def test_disabled_explicit_profile_never_falls_back(self):
        self.codex['enabled'] = False
        with self.assertRaises(ValueError):
            select_btw_profile(self.cfg, {}, 'codex-a')

    def test_ambiguous_engine_requires_profile(self):
        with self.assertRaises(ValueError):
            select_btw_profile(self.cfg, {}, 'codex')

    def test_legacy_unique_engine_resolves_a_profile(self):
        self.assertEqual(select_btw_profile(self.cfg, {}, 'claude'), self.claude)

    def test_missing_configured_btw_fails_closed(self):
        self.policy['btw_profile'] = 'deleted'
        with self.assertRaises(ValueError):
            select_btw_profile(self.cfg, {})

    def test_enabled_review_with_deleted_profile_fails_closed(self):
        self.policy['review'] = {'enabled': True, 'engine': 'deleted'}
        with self.assertRaises(ValueError):
            select_btw_profile(self.cfg, {})

    def test_empty_roster_never_returns_claude(self):
        with self.assertRaises(ValueError):
            select_btw_profile({}, {})

    def test_winner_uses_exact_profile_not_first_matching_engine(self):
        self.policy['review'] = {}
        self.assertEqual(select_btw_profile(self.cfg, {'engine': 'codex', 'profile_id': 'codex-b'}), self.other)

    def test_unbound_legacy_default_does_not_launch_claude(self):
        self.cfg['worker_profiles'] = [self.codex]
        self.policy['review'] = {'engine': 'claude'}
        self.assertEqual(select_btw_profile(self.cfg, {}), self.codex)

    def test_standby_override_does_not_reuse_other_worker_state(self):
        winner = dict(engine='claude', profile_id='claude-a', session='old',
                      worker_id='old-worker', workdir_rel='old-dir', agent_state_rel='old-state')
        before = copy.deepcopy(winner)
        self.policy['standby_profile'] = 'codex-b'
        for action in ('ask', 'writeup'):
            result = configured_standby_identity(winner, self.cfg, action)
            self.assertEqual(result['engine'], 'codex')
            self.assertEqual(result['profile_id'], 'codex-b')
            for key in ('session', 'worker_id', 'workdir_rel', 'agent_state_rel'):
                self.assertEqual(result[key], '')
        self.assertEqual(winner, before)
        self.assertIsNone(configured_standby_identity(winner, self.cfg, 'mark_false'))

    def test_automatic_standby_keeps_existing_recovery(self):
        self.assertIsNone(configured_standby_identity({}, self.cfg, 'ask'))

    def test_missing_standby_override_fails_closed(self):
        self.policy['standby_profile'] = 'missing'
        with self.assertRaises(ValueError):
            configured_standby_identity({}, self.cfg, 'ask')

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

    def test_btw_and_standby_selection_do_not_mutate_main_profiles(self):
        self.policy['standby_profile'] = 'codex-a'
        self.policy['btw_profile'] = 'codex-b'
        original = copy.deepcopy(self.cfg)
        btw = select_btw_profile(self.cfg, {})
        standby = configured_standby_identity({}, self.cfg, 'ask')
        self.assertEqual(standby['profile_id'], 'codex-a')
        self.assertEqual(btw['id'], 'codex-b')
        btw['credential_account'] = 'changed-only-in-runtime'
        btw['roles'].append('temporary')
        self.assertEqual(self.cfg, original)
