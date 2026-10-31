import tempfile
import unittest
from muteki.core.cost import CostController, Budget
from muteki.core.usage import UsageStore
from muteki.solver.followup_budget import standby_cost_controller


class FollowupBudgetTest(unittest.IsolatedAsyncioTestCase):
    async def test_auxiliary_usage_is_saved_without_charging_main_in_both_modes(self):
        for mode in ('ctf', 'pentest'):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as root:
                usage = UsageStore(root)
                def sink(raw, **scope):
                    return usage.record(raw, run_id='run-test', **scope)
                main = CostController(budget=Budget(global_usd=1), usage_sink=sink)
                await main.add_external_usd(0.25, run_id='run-test', solver_id='main', usage_id='main')
                before = main.snapshot()
                for action in ('ask', 'writeup'):
                    followup = standby_cost_controller(main, action)
                    await followup.add_external_usd(10, run_id='run-test', solver_id='cli-codex-standby', usage_id=action)
                    self.assertIsNone(followup.bus)
                    self.assertEqual(main.snapshot(), before)
                usage.record({'reported_cost': 3}, identity='btw', run_id='run-test', role='btw', actor_kind='auxiliary')
                # Reopen durable storage, as after a service restart.
                recovered = UsageStore(root).run_totals('run-test', exclude_followups=True)
                self.assertAlmostEqual(recovered['reported_cost'], 0.25)
                self.assertAlmostEqual(usage.run_totals('run-test')['reported_cost'], 23.25)
                restored = CostController()
                restored.restore_run_totals(recovered)
                self.assertAlmostEqual(restored.snapshot()['global_usd'], 0.25)
                self.assertIs(standby_cost_controller(main, 'mark_false'), main)
