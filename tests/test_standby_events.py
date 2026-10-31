"""Follow-up observability must not reopen a terminated solve generation."""
import unittest
from types import SimpleNamespace

from apps.web.run_manager import RunManager
from muteki.core.event_bus import EventBus
from muteki.core.events import Event, EventType
from muteki.solver.cli_events import _emit


class StandbyEventsTest(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.run = SimpleNamespace(execution_generation=2, control_generation=1,
                                   terminal_generations={2}, active_followups={'ask-1'})
        self.bus = EventBus()
        self.bus.add_filter(RunManager._generation_filter_for(None, self.run))
        self.saved = []
        async def sink(event):
            self.saved.append(event)
        self.bus.add_sink(sink, required=True)
        self.worker = SimpleNamespace(bus=self.bus, run_id='test',
                                      challenge=SimpleNamespace(id='test'),
                                      solver_id='cli-codex-standby', mode='respond',
                                      hitl_cmd={'action': 'ask', 'followup_id': 'ask-1'})

    async def test_ask_and_writeup_output_reaches_durable_sink(self):
        for action in ('ask', 'writeup'):
            self.worker.hitl_cmd['action'] = action
            for kind in (EventType.WORKER_PROMPT, EventType.TEXT_MESSAGE_DELTA,
                         EventType.REASONING_DELTA, EventType.TOOL_CALL_START,
                         EventType.TOOL_CALL_RESULT, EventType.COST_UPDATE):
                before = len(self.saved)
                await _emit(self.worker, kind, text='synthetic')
                self.assertEqual(len(self.saved), before + 1)
                self.assertEqual(self.saved[-1].payload['followup_id'], 'ask-1')

    async def test_finished_followup_output_is_rejected(self):
        self.run.active_followups.clear()
        await _emit(self.worker, EventType.TEXT_MESSAGE_DELTA, text='late')
        self.assertEqual(self.saved, [])

    async def test_old_generation_output_is_rejected(self):
        await _emit(self.worker, EventType.TEXT_MESSAGE_DELTA, execution_generation=1)
        self.assertEqual(self.saved, [])

    async def test_stopped_ordinary_worker_output_is_rejected(self):
        self.worker.solver_id = 'cli-codex'
        self.worker.mode = 'bootstrap'
        await _emit(self.worker, EventType.TOOL_CALL_START, followup_id='ask-1')
        self.assertEqual(self.saved, [])

    async def test_followup_cannot_publish_solve_mutations(self):
        for kind in (EventType.BLACKBOARD_DELTA, EventType.RUN_FINISHED,
                     EventType.FLAG_ACCEPTED, EventType.SOLVE_GRAPH_DELTA):
            await _emit(self.worker, kind)
        self.assertEqual(self.saved, [])

    async def test_missing_followup_identity_is_rejected(self):
        await self.bus.emit(Event(event_type=EventType.TEXT_MESSAGE_DELTA,
                                  run_id='test', solver_id='cli-codex-standby'))
        self.assertEqual(self.saved, [])

    async def test_live_followup_cannot_change_ctf_or_pentest_state(self):
        for mode in ('ctf', 'pentest'):
            for action in ('ask', 'writeup'):
                with self.subTest(mode=mode, action=action):
                    self.run.mode = mode
                    self.run.terminal_generations.clear()
                    self.saved.clear()
                    self.worker.hitl_cmd['action'] = action
                    for kind in (EventType.RUN_FINISHED, EventType.RUN_REOPENED,
                                 EventType.FLAG_ACCEPTED, EventType.BLACKBOARD_DELTA,
                                 EventType.SOLVE_GRAPH_DELTA):
                        await _emit(self.worker, kind)
                    self.assertEqual(self.saved, [])
                    self.assertEqual(self.run.terminal_generations, set())
                    await _emit(self.worker, EventType.TEXT_MESSAGE_DELTA, text='answer')
                    self.assertEqual(len(self.saved), 1)

    async def test_main_solve_and_mark_false_still_publish_mutations(self):
        for solver, action in (('cli-codex', 'execute'),
                               ('cli-codex-standby', 'mark_false')):
            self.run.terminal_generations.clear()
            self.saved.clear()
            self.worker.solver_id = solver
            self.worker.hitl_cmd = {'action': action}
            await _emit(self.worker, EventType.RUN_FINISHED)
            self.assertEqual(len(self.saved), 1)
            self.assertEqual(self.run.terminal_generations, {2})
