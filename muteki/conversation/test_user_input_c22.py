"""C22 structured user-input normalization and resolve fixtures."""

from __future__ import annotations

import os
import unittest

from muteki.external_agents.user_input_schema import (
    answers_for_codex_tool,
    c22_fixture_questions,
    content_for_elicitation,
    expand_legacy_text_answers,
    normalize_pending_user_input,
    questions_from_codex_params,
    questions_from_schema,
    validate_user_input_answers,
)


class UserInputSchemaTests(unittest.TestCase):
    def test_codex_multi_question_normalize(self) -> None:
        questions = questions_from_codex_params({
            "message": "部署",
            "questions": [
                {"id": "q_header", "header": "部署确认"},
                {
                    "id": "q_env",
                    "question": "环境",
                    "options": [
                        {"value": "staging", "label": "staging"},
                        {"value": "prod", "label": "prod", "recommended": True},
                    ],
                },
                {
                    "id": "q_flags",
                    "question": "开关",
                    "multi": True,
                    "options": [{"value": "a"}, {"value": "b"}, {"value": "c"}],
                    "required": False,
                },
                {"id": "q_note", "question": "备注"},
            ],
        })
        by_id = {q["question_id"]: q for q in questions}
        self.assertEqual(by_id["q_header"]["kind"], "header")
        self.assertEqual(by_id["q_env"]["kind"], "single_select")
        self.assertTrue(
            any(opt["recommended"] for opt in by_id["q_env"]["options"])
        )
        self.assertEqual(by_id["q_flags"]["kind"], "multi_select")
        self.assertFalse(by_id["q_flags"]["required"])
        self.assertEqual(by_id["q_note"]["kind"], "free_text")

    def test_schema_multi_property(self) -> None:
        questions = questions_from_schema({
            "type": "object",
            "required": ["env", "note"],
            "properties": {
                "env": {
                    "type": "string",
                    "title": "环境",
                    "enum": ["staging", "prod"],
                },
                "flags": {
                    "type": "array",
                    "title": "开关",
                    "enum": ["a", "b"],
                },
                "note": {"type": "string", "title": "备注"},
            },
        }, title="确认")
        kinds = {q["question_id"]: q["kind"] for q in questions}
        self.assertEqual(kinds["__title__"], "header")
        self.assertEqual(kinds["env"], "single_select")
        self.assertEqual(kinds["flags"], "multi_select")
        self.assertEqual(kinds["note"], "free_text")

    def test_validate_required_and_keyed_answers(self) -> None:
        pending = normalize_pending_user_input({
            "request_id": "req-1",
            "questions": c22_fixture_questions(),
        })
        with self.assertRaises(ValueError) as ctx:
            validate_user_input_answers(pending, {}, decision="submit")
        self.assertIn("conversation.user_input.required", str(ctx.exception))

        validated = validate_user_input_answers(pending, {
            "q_env": {"values": ["staging"]},
            "q_flags": {"values": ["a", "c"]},
            "q_note": {"text": "ok-c22"},
        })
        self.assertEqual(set(validated), {"q_env", "q_flags", "q_note"})
        self.assertEqual(validated["q_flags"]["values"], ["a", "c"])
        tool = answers_for_codex_tool(validated)
        self.assertEqual(tool["answers"]["q_env"]["answers"], ["staging"])
        self.assertEqual(tool["answers"]["q_note"]["answers"], ["ok-c22"])

    def test_recommended_not_auto_filled(self) -> None:
        pending = normalize_pending_user_input({
            "request_id": "req-1",
            "questions": c22_fixture_questions(),
        })
        # Opening the form must not invent answers from recommended options.
        self.assertEqual(expand_legacy_text_answers(pending, ""), {})
        with self.assertRaises(ValueError):
            validate_user_input_answers(pending, {})

    def test_cancel_skips_validation(self) -> None:
        pending = normalize_pending_user_input({
            "request_id": "req-1",
            "questions": c22_fixture_questions(),
        })
        self.assertEqual(
            validate_user_input_answers(pending, {}, decision="cancel"),
            {},
        )

    def test_elicitation_content(self) -> None:
        content = content_for_elicitation(
            {
                "env": {"values": ["prod"]},
                "flags": {"values": ["a", "c"]},
                "note": {"text": "ship it"},
            },
            schema={
                "properties": {
                    "env": {"type": "string"},
                    "flags": {"type": "array"},
                    "note": {"type": "string"},
                }
            },
        )
        self.assertEqual(content["env"], "prod")
        self.assertEqual(content["flags"], ["a", "c"])
        self.assertEqual(content["note"], "ship it")


class UserInputFixtureCommandTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self) -> None:
        os.environ["MUTEKI_ALLOW_USER_INPUT_FIXTURES"] = "1"
        from pathlib import Path
        import tempfile

        from muteki.conversation.module import ConversationService
        from muteki.platform.command_api import MutekiCommandApiImpl
        from muteki.platform.contracts.commands import ActorRef, CommandEnvelope
        from muteki.platform.store import PlatformStore

        self._tmpdir = tempfile.TemporaryDirectory()
        root = Path(self._tmpdir.name)
        self.store = PlatformStore(root / "platform.db")
        self.service = ConversationService(self.store, sessions_root=root)
        self.api = MutekiCommandApiImpl(self.store)
        self.service.register(self.api)
        self.actor = ActorRef(kind="operator", id="test")

        create = await self.api.dispatch(CommandEnvelope(
            command_type="conversation.thread.create",
            actor=self.actor,
            payload={"title": "c22"},
        ))
        self.thread_id = create.aggregate.id

    async def asyncTearDown(self) -> None:
        os.environ.pop("MUTEKI_ALLOW_USER_INPUT_FIXTURES", None)
        self._tmpdir.cleanup()

    async def test_inject_resolve_round_trip_and_mismatch(self) -> None:
        from muteki.platform.contracts.commands import CommandEnvelope
        from muteki.platform.contracts.receipts import ReceiptState

        inject = await self.api.dispatch(CommandEnvelope(
            command_type="conversation.user_input.inject",
            actor=self.actor,
            aggregate_type="thread",
            aggregate_id=self.thread_id,
            payload={"thread_id": self.thread_id, "request_id": "req-1"},
        ))
        self.assertEqual(inject.state, ReceiptState.COMPLETED)
        state = self.service.manager.conv.get_state(self.thread_id)
        self.assertIsNotNone(state.pending_user_input)
        self.assertEqual(state.pending_user_input["request_id"], "req-1")
        qids = {
            q["question_id"]
            for q in state.pending_user_input.get("questions") or []
            if q.get("kind") != "header"
        }
        self.assertEqual(qids, {"q_env", "q_flags", "q_note"})

        resolve = await self.api.dispatch(CommandEnvelope(
            command_type="conversation.user_input.resolve",
            actor=self.actor,
            aggregate_type="thread",
            aggregate_id=self.thread_id,
            payload={
                "thread_id": self.thread_id,
                "request_id": "req-1",
                "decision": "submit",
                "answers": {
                    "q_env": {"values": ["staging"]},
                    "q_flags": {"values": ["a", "c"]},
                    "q_note": {"values": ["ok-c22"], "text": "ok-c22"},
                },
            },
        ))
        self.assertEqual(resolve.state, ReceiptState.COMPLETED)
        capture = self.service.manager.get_user_input_fixture_capture(self.thread_id)
        self.assertIsNotNone(capture)
        assert capture is not None
        self.assertEqual(capture["answers"]["q_env"]["values"], ["staging"])
        self.assertEqual(capture["answers"]["q_flags"]["values"], ["a", "c"])
        note_vals = capture["answers"]["q_note"]["values"]
        if not note_vals:
            note_vals = [capture["answers"]["q_note"].get("text")]
        self.assertEqual(note_vals, ["ok-c22"])

        await self.api.dispatch(CommandEnvelope(
            command_type="conversation.user_input.inject",
            actor=self.actor,
            aggregate_type="thread",
            aggregate_id=self.thread_id,
            payload={"thread_id": self.thread_id, "request_id": "req-2"},
        ))
        stale = await self.api.dispatch(CommandEnvelope(
            command_type="conversation.user_input.resolve",
            actor=self.actor,
            aggregate_type="thread",
            aggregate_id=self.thread_id,
            payload={
                "thread_id": self.thread_id,
                "request_id": "req-1",
                "decision": "submit",
                "answers": {
                    "q_env": {"values": ["prod"]},
                    "q_note": {"text": "stale"},
                },
            },
        ))
        self.assertEqual(stale.state, ReceiptState.FAILED)
        self.assertEqual(
            (stale.error.code if stale.error else ""),
            "conversation.user_input.mismatch",
        )


    async def test_failed_delivery_keeps_persisted_question_and_late_ack_keeps_new_one(self) -> None:
        from muteki.conversation import events as ev
        from muteki.conversation.store import ConversationStore
        from muteki.platform.contracts.commands import CommandEnvelope
        from muteki.platform.contracts.receipts import ReceiptState
        from muteki.platform.store import PlatformStore

        await self.api.dispatch(CommandEnvelope(
            command_type="conversation.user_input.inject", actor=self.actor,
            aggregate_type="thread", aggregate_id=self.thread_id,
            payload={"request_id": "reply-a"},
        ))
        async def rejected(*args, **kwargs):
            raise RuntimeError("synthetic native turn/start rejection")
        self.service.executor.resolve_user_input = rejected
        with self.assertLogs("muteki.platform.command_api", level="ERROR"):
            receipt = await self.api.dispatch(CommandEnvelope(
                command_type="conversation.user_input.resolve", actor=self.actor,
                aggregate_type="thread", aggregate_id=self.thread_id,
                payload={"request_id": "reply-a", "decision": "cancel"},
            ))
        self.assertEqual(receipt.state, ReceiptState.FAILED)
        reopened = PlatformStore(self.store.db_path)
        try:
            self.assertEqual(ConversationStore(reopened).get_state(self.thread_id)
                             .pending_user_input["request_id"], "reply-a")
        finally:
            reopened.close()
        pending = self.service.conv.get_state(self.thread_id).pending_user_input
        self.store.append_events([
            ev.thread_event(self.thread_id, ev.EV_USER_INPUT_REQUESTED,
                            {**pending, "request_id": "reply-b"}),
            ev.thread_event(self.thread_id, ev.EV_USER_INPUT_RESOLVED,
                            {"request_id": "reply-a"}),
        ])
        self.assertEqual(self.service.conv.get_state(self.thread_id)
                         .pending_user_input["request_id"], "reply-b")


class CodexAsyncAnswerDeliveryTests(unittest.IsolatedAsyncioTestCase):
    def adapter(self, request, current="native-turn"):
        import asyncio
        from types import SimpleNamespace
        from muteki.external_agents.codex import CodexAppServerAdapter
        from muteki.platform.contracts.external_agents import AgentSessionRef
        adapter = CodexAppServerAdapter(schema_probe=False)
        session = AgentSessionRef(agent_session_id="synthetic-session")
        future = asyncio.get_running_loop().create_future()
        ctx = {"thread_id": "synthetic-native", "current_turn_id": current,
               "conn": SimpleNamespace(request=request), "user_inputs": {"q": future},
               "user_input_params": {"q": {"method": "agentMessage/async",
                   "pending": {"request_id": "q", "questions": []}}}}
        adapter._runs[session.agent_session_id] = ctx
        return adapter, session, ctx, future

    async def test_concurrent_identical_answers_send_one_rpc(self):
        import asyncio
        entered, release = asyncio.Event(), asyncio.Event()
        calls = []
        async def request(method, params, **kwargs):
            calls.append(method)
            entered.set()
            await release.wait()
            return {}
        adapter, session, _ctx, _future = self.adapter(request)
        tasks = [asyncio.create_task(adapter.respond_user_input(
            session, "q", {"__text__": "ordinary answer"})) for _ in range(2)]
        await entered.wait()
        await asyncio.sleep(0)
        self.assertEqual(calls, ["turn/steer"])
        release.set()
        receipts = await asyncio.gather(*tasks)
        self.assertEqual([r.state.value for r in receipts], ["completed", "completed"])
        self.assertEqual(sum(r.deduplicated for r in receipts), 1)

    async def test_continuation_must_start_before_answer_is_consumed(self):
        from muteki.external_agents.codex import JsonRpcError
        fail = True
        async def request(method, params, **kwargs):
            self.assertEqual(method, "turn/start")
            if fail:
                raise JsonRpcError(-32000, "synthetic rejection")
            return {"turn": {"id": "new-native-turn"}}
        adapter, session, ctx, future = self.adapter(request, current=None)
        with self.assertRaises(JsonRpcError):
            await adapter.respond_user_input(session, "q", {"__text__": "answer"})
        self.assertFalse(future.done())
        self.assertNotIn("async_started_turn", ctx)
        fail = False
        receipt = await adapter.respond_user_input(session, "q", {"__text__": "answer"})
        self.assertEqual(receipt.state.value, "completed")
        self.assertTrue(future.done())
        self.assertEqual(ctx["async_started_turn"]["result"]["turn"]["id"], "new-native-turn")

    async def test_cancel_during_start_stops_unclaimed_continuation(self):
        import asyncio
        entered, release = asyncio.Event(), asyncio.Event()
        calls = []
        async def request(method, params, **kwargs):
            calls.append(method)
            entered.set()
            await release.wait()
            return {"turn": {"id": "new-native-turn"}}
        adapter, session, ctx, future = self.adapter(request, current=None)
        task = asyncio.create_task(adapter.respond_user_input(session, "q", {"__text__": "answer"}))
        await entered.wait()
        future.cancel()
        release.set()
        with self.assertRaises(RuntimeError):
            await task
        self.assertEqual(calls, ["turn/start", "turn/interrupt"])
        self.assertNotIn("async_started_turn", ctx)


if __name__ == "__main__":
    unittest.main()
