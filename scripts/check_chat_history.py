"""Atomic history rewind, failure preservation and command lifecycle regressions."""
from __future__ import annotations
import asyncio
from pathlib import Path
import sqlite3
import sys
import tempfile
from types import SimpleNamespace
from unittest.mock import AsyncMock, patch
sys.path.insert(0,str(Path(__file__).resolve().parents[1]))
from muteki.platform.store import PlatformStore
from muteki.platform.capability_bindings import CapabilityBindingService
from muteki.platform.contracts.commands import CommandEnvelope
from muteki.conversation.store import ConversationStore
from muteki.conversation.manager import ConversationManager, ConversationError
from muteki.conversation.projections import ConversationProjection
from muteki.conversation.models import TurnRecord, ConversationMessage
from muteki.conversation.commands import ConversationCommandHandler, COMMAND_TYPES
from muteki.conversation.executor import ExternalAgentSessionExecutor
from muteki.platform.command_handlers.base import HandlerContext

async def check():
 with tempfile.TemporaryDirectory() as temp:
  store=PlatformStore(Path(temp)/'db.sqlite3');conv=ConversationStore(store)
  manager=ConversationManager(store,conv,CapabilityBindingService(store),ConversationProjection(store,conv))
  thread=manager.create_thread(title='Rewind fixture')
  turns=[TurnRecord(thread_id=thread.thread_id,seq=i,status='completed',text=f'message {i}') for i in range(1,4)]
  for t in turns:
   conv.save_turn(t);conv.save_message(ConversationMessage(thread_id=thread.thread_id,turn_id=t.turn_id,text=t.text))
  opts=dict(capability_override={'invocable':True})
  ids,applied=manager.native_rewind_turn(thread.thread_id,turns[1].turn_id,dry_run=True,**opts)
  assert ids==[t.turn_id for t in turns[1:]] and applied and len(conv.list_current_turns(thread.thread_id))==3
  try:manager.native_rewind_turn(thread.thread_id,turns[1].turn_id,provider_rewind=lambda **_:False,**opts)
  except ConversationError:pass
  else:raise AssertionError('provider failure accepted')
  assert len(conv.list_current_turns(thread.thread_id))==3
  original=conv._execute;writes=0
  def fail_second(sql,params=()):
   nonlocal writes
   if sql.startswith('UPDATE conv_turns'):
    writes+=1
    if writes==2:raise sqlite3.OperationalError('fixture write failure')
   return original(sql,params)
  with patch.object(conv,'_execute',side_effect=fail_second):
   try:manager.native_rewind_turn(thread.thread_id,turns[1].turn_id,provider_rewind=lambda **_:True,**opts)
   except sqlite3.OperationalError:pass
   else:raise AssertionError('store failure accepted')
  assert len(conv.list_current_turns(thread.thread_id))==3,'partial branch commit'
  executor=SimpleNamespace(_history_mutations=set(),rewind_turn=AsyncMock(return_value={'applied':True,'superseded_turn_ids':ids,'strategy':'rebuild'}))
  handler=ConversationCommandHandler(manager,executor)
  command=CommandEnvelope(command_type='conversation.turn.native_rewind',aggregate_type='thread',aggregate_id=thread.thread_id,payload={'turn_id':turns[1].turn_id})
  plan=await handler.plan(command,HandlerContext(store=store))
  assert plan.events and len(conv.list_current_turns(thread.thread_id))==3
  executor.rewind_turn.assert_not_awaited()
  result=await plan.side_effect();assert result.events and result.output['impact']['strategy']=='rebuild'
  executor.rewind_turn.assert_awaited_once()
  assert command.command_type in COMMAND_TYPES
  conv.save_state(conv.get_state(thread.thread_id).model_copy(update={'history_recovery_required':True}))
  recover=ExternalAgentSessionExecutor(store,conv,manager,SimpleNamespace(),sessions_root=temp)
  with patch.object(recover,'_session_record',return_value=None):
   await recover._recover_history_mutation(thread.thread_id)
  recovered=conv.get_state(thread.thread_id)
  assert recovered.history_rebuild_pending and not recovered.history_recovery_required
  assert len(conv.list_current_turns(thread.thread_id))==3,'recovery changed committed branch'
  manager.native_rewind_turn(thread.thread_id,turns[1].turn_id,provider_rewind=lambda **_:True,rebuild_history=True,**opts)
  assert conv.get_state(thread.thread_id).history_rebuild_pending
  assert [m.text for m in conv.list_current_messages(thread.thread_id)]==['message 1']
  assert len(conv.list_turns(thread.thread_id))==3
 print('PASS: rewind registration, deferred side effect, failure preservation, atomic projection, retained audit history')
if __name__=='__main__':asyncio.run(check())
