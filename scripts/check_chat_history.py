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
from muteki.conversation.models import TurnRecord, ConversationMessage, ThreadRuntimeSelection, ThreadPlanSnapshot
from muteki.platform.contracts.objects import AgentSession, Workspace
from muteki.platform.contracts.base import utcnow
from muteki.platform.contracts.external_agents import AgentSessionRef, AgentEvent, AgentEventType
from muteki.conversation.session_handoff import build_session_handoff
from muteki.conversation import events as ev
from muteki.conversation.commands import ConversationCommandHandler, COMMAND_TYPES
from muteki.conversation.executor import ExternalAgentSessionExecutor
from muteki.platform.command_handlers.base import HandlerContext

async def check_session_lifecycle():
 # Explicitly offline scripted-adapter fixtures, no provider or model requests.
 with tempfile.TemporaryDirectory() as temp:
  store=PlatformStore(Path(temp)/'db.sqlite3');conv=ConversationStore(store)
  manager=ConversationManager(store,conv,CapabilityBindingService(store),ConversationProjection(store,conv))
  thread=manager.create_thread(title='Offline native session lifecycle')
  selection=ThreadRuntimeSelection(thread_id=thread.thread_id,adapter_id='fixture.sdk',model='one',effort='xhigh',access_mode='full-access')
  conv.save_runtime_selection(selection)
  record=AgentSession(thread_id=thread.thread_id,adapter_id=selection.adapter_id,runtime_instance_id='default',resume_handle='native-history',external_session_id='native-history',execution_generation=1)
  store.save(record)
  prior=TurnRecord(thread_id=thread.thread_id,seq=1,status='completed',text='remember this',agent_session_id=record.agent_session_id,native_turn_id='native-turn',runtime_snapshot={key:value for key,value in selection.model_dump().items() if isinstance(value,str)})
  conv.save_turn(prior)
  conv.save_message(ConversationMessage(thread_id=thread.thread_id,turn_id=prior.turn_id,role='assistant',text='remembered'))
  conv.save_state(conv.get_state(thread.thread_id).model_copy(update={'agent_session_id':record.agent_session_id,'session_runtime_key':selection.session_key,'session_launch_cwd':''}))
  caps=SimpleNamespace(resume=True,resume_continues_turn=False,plan_mode_per_turn=True,model_options_per_turn=True)
  adapter=SimpleNamespace(id=selection.adapter_id,identity=SimpleNamespace(instance_id='default'),capabilities=AsyncMock(return_value=caps))
  launches=[];sent=[]
  async def start(request):
   launches.append(request)
   row=AgentSession(agent_session_id=request.agent_session_id,adapter_id=selection.adapter_id,runtime_instance_id='default',thread_id=thread.thread_id,resume_handle=request.resume_handle or 'new-native-history',external_session_id=request.resume_handle or 'new-native-history',execution_generation=request.execution_generation)
   store.save(row)
   return AgentSessionRef(**{key:getattr(row,key) for key in ('agent_session_id','adapter_id','runtime_instance_id','resume_handle','external_session_id')})
  async def close(ref):
   row=store.get(AgentSession,ref.agent_session_id)
   store.save(row.model_copy(update={'closed_at':utcnow()}))
  adapter.start=AsyncMock(side_effect=start);adapter.close=AsyncMock(side_effect=close)
  adapter.send=lambda ref,message:sent.append(message)
  registry=SimpleNamespace(get=lambda *_:adapter)
  executor=ExternalAgentSessionExecutor(store,conv,manager,registry,sessions_root=temp)
  executor.refresh_runtime_capabilities=lambda *_:False
  executor._live.add(record.agent_session_id)
  executor._capability_cache[thread.thread_id]=SimpleNamespace(stale=False)
  turn=TurnRecord(thread_id=thread.thread_id,seq=2,text='continue',kind='resume')
  conv.save_turn(turn)
  with patch.object(executor,'_adapter_for',return_value=adapter):
   # Ambient catalog/native revisions cannot close a live conversation.
   with patch.object(executor,'_chat_revision_matches',side_effect=AssertionError('ambient revision consulted')):
    _,ref=await executor.ensure_session(thread,turn)
   assert ref.agent_session_id==record.agent_session_id and not launches
   selection=selection.model_copy(update={'model':'two','effort':'','service_tier':''});conv.save_runtime_selection(selection)
   with patch.object(executor,'_capture_turn_checkpoint',new=AsyncMock()),patch.object(executor,'_consume',new=AsyncMock()):
    await executor._run_turn(turn)
   assert not launches and sent[-1].payload.model=='two' and sent[-1].payload.effort==''
   assert not any(part.kind=='history_handoff' for part in sent[-1].payload.context_parts),'native continuation duplicated history'
   executor._live.discard(record.agent_session_id)
   with patch.object(executor,'_capture_turn_checkpoint',new=AsyncMock()),patch.object(executor,'_consume',new=AsyncMock()):
    await executor._run_turn(turn)
   assert launches[-1].resume_handle=='native-history'
   assert conv.get_turn(turn.turn_id).session_resolution=='native_resume'
   assert not any(part.kind=='history_handoff' for part in sent[-1].payload.context_parts),'native load duplicated history'
   launches.clear()
   # Discovery with pending permission/model changes must be read-only.
   selection=selection.model_copy(update={'access_mode':'supervised'});conv.save_runtime_selection(selection)
   adapter.close.reset_mock()
   _,ref=await executor.ensure_capability_session(thread.thread_id)
   assert ref.agent_session_id==record.agent_session_id and not launches
   adapter.close.assert_not_awaited()
   # Explicit launch settings reopen the same native identity, with a new fence.
   _,ref=await executor.ensure_session(thread,turn)
   assert len(launches)==1 and launches[-1].resume_handle=='native-history'
   assert ref.agent_session_id==record.agent_session_id and launches[-1].execution_generation==3
   adapter.close.assert_awaited_once()
   assert conv.get_turn(turn.turn_id).session_resolution=='native_resume'
   caps.model_options_per_turn=False
   selection=selection.model_copy(update={'model':'three'});conv.save_runtime_selection(selection)
   _,ref=await executor.ensure_session(thread,turn)
   assert len(launches)==2 and launches[-1].model=='three' and launches[-1].resume_handle=='native-history'
   # Providers may support only some selection fields on a live session.
   adapter.chat_model_fields=('model',)
   selection=selection.model_copy(update={'model':'live-model'});conv.save_runtime_selection(selection)
   await executor.ensure_session(thread,turn)
   assert len(launches)==2
   selection=selection.model_copy(update={'effort':'high'});conv.save_runtime_selection(selection)
   await executor.ensure_session(thread,turn)
   assert len(launches)==3 and launches[-1].resume_handle=='native-history'
   # Clearing a selection whose native default is launch-scoped resumes history.
   adapter.chat_model_reset_requires_restart=('model',)
   selection=selection.model_copy(update={'model':''});conv.save_runtime_selection(selection)
   await executor.ensure_session(thread,turn)
   assert len(launches)==4 and launches[-1].resume_handle=='native-history'
   del adapter.chat_model_fields,adapter.chat_model_reset_requires_restart
   # Strict close failure stops replacement without changing persisted identity.
   selection=selection.model_copy(update={'model':'four'});conv.save_runtime_selection(selection)
   adapter.close.side_effect=RuntimeError('offline close failure')
   try:await executor.ensure_session(thread,turn)
   except RuntimeError as exc:assert str(exc)=='offline close failure'
   else:raise AssertionError('close failure swallowed')
   assert len(launches)==4 and conv.get_state(thread.thread_id).agent_session_id==record.agent_session_id
   adapter.close.side_effect=close
   # Provider resume errors remain original errors; there is no cold-start fallback.
   adapter.start.side_effect=RuntimeError('offline native load failure')
   before=conv.get_state(thread.thread_id)
   try:await executor.ensure_session(thread,turn)
   except RuntimeError as exc:assert str(exc)=='offline native load failure'
   else:raise AssertionError('resume failure swallowed')
   assert conv.get_state(thread.thread_id).session_runtime_key==before.session_runtime_key
   assert store.get(AgentSession,record.agent_session_id).resume_handle=='native-history'
   adapter.start.side_effect=start
   # Real history without a usable handle must fail rather than rebuild.
   row=store.get(AgentSession,record.agent_session_id)
   store.save(row.model_copy(update={'resume_handle':None}))
   try:await executor.ensure_session(thread,turn)
   except ConversationError as exc:assert exc.code=='conversation.session.resume_unavailable'
   else:raise AssertionError('lost native history cold-started')
   store.save(row)
   # Workspace and cwd changes apply by native resume, including same-workspace root changes.
   workspace=Workspace(root_path=temp)
   store.save(workspace)
   thread=thread.model_copy(update={'workspace_id':workspace.workspace_id});store.save(thread)
   await executor.ensure_session(thread,turn)
   assert launches[-1].workspace_id==workspace.workspace_id and launches[-1].options.cwd==str(Path(temp).resolve())
   assert launches[-1].resume_handle=='native-history'
   workspace=workspace.model_copy(update={'root_path':str(Path(temp)/'changed-root')});store.save(workspace)
   count=len(launches)
   await executor.ensure_session(thread,turn)
   assert len(launches)==count+1 and launches[-1].options.cwd==str(Path(workspace.root_path).resolve())
   assert conv.get_state(thread.thread_id).session_launch_cwd==str(Path(workspace.root_path).resolve())
   # A persisted prepared handoff survives process-local cache loss and sends once.
   handoff=build_session_handoff(thread_id=thread.thread_id,messages=conv.list_current_messages(thread.thread_id),turns=conv.list_turns(thread.thread_id))
   handoff.target_agent_session_id=record.agent_session_id
   conv.save_state(conv.get_state(thread.thread_id).model_copy(update={'pending_handoff':handoff.to_dict()}))
   executor._pending_handoff.clear()
   with patch.object(executor,'_capture_turn_checkpoint',new=AsyncMock()),patch.object(executor,'_consume',new=AsyncMock()):
    await executor._run_turn(turn)
   assert sum(part.kind=='history_handoff' for part in sent[-1].payload.context_parts)==1
   journal=conv.get_state(thread.thread_id).pending_handoff
   assert journal['handoff_delivery_state']=='prepared' and journal['handoff_delivery_turn_id']==turn.turn_id
   # Pending delivery does not add a separate gate to session reuse.
   await executor.ensure_session(thread,turn)
   # Only runtime acceptance commits delivery; discovery never does.
   started=AgentEvent(event_type=AgentEventType.TURN_STARTED,agent_session_id=record.agent_session_id,turn_id='accepted-native-turn',payload={})
   executor._translate(thread.thread_id,prior,started)
   assert conv.get_state(thread.thread_id).pending_handoff is not None,'background turn acknowledged another delivery'
   emit=executor._emit
   def fail_ack(*args,**kwargs):
    if args[1]==ev.EV_RUNTIME_SWITCHED:raise sqlite3.OperationalError('offline append failure')
    return emit(*args,**kwargs)
   with patch.object(executor,'_emit',side_effect=fail_ack):
    try:executor._translate(thread.thread_id,turn,started)
    except sqlite3.OperationalError:pass
    else:raise AssertionError('delivery event persistence failure swallowed')
   assert conv.get_state(thread.thread_id).pending_handoff is not None,'failed event append lost delivery journal'
   executor._translate(thread.thread_id,turn,AgentEvent(event_type=AgentEventType.TURN_STARTED,agent_session_id=record.agent_session_id,turn_id='accepted-native-turn',payload={}))
   assert conv.get_state(thread.thread_id).pending_handoff is None
   events=store.read_events('thread',thread.thread_id,limit=1000)
   assert any(event.payload.get('handoff_id')==handoff.handoff_id and event.payload.get('handoff_delivery_state')=='delivered' for event in events)
   # Apply real close projection, then recover through a new executor instance.
   conv.save_turn(conv.get_turn(turn.turn_id).model_copy(update={'status':'completed'}))
   await executor.close_thread(thread.thread_id,reason='capability_binding_changed')
   projection=ConversationProjection(store,conv)
   projection.apply(store.read_events('thread',thread.thread_id,limit=1000)[-1])
   closed_state=conv.get_state(thread.thread_id)
   assert closed_state.agent_session_id==record.agent_session_id and closed_state.session_runtime_key
   restarted=ExternalAgentSessionExecutor(store,conv,manager,registry,sessions_root=temp)
   with patch.object(restarted,'_adapter_for',return_value=adapter):
    _,resumed=await restarted.ensure_session(thread,turn)
   assert resumed.agent_session_id==record.agent_session_id and launches[-1].resume_handle=='native-history'
   # A preserved history pointer without a handle fails explicitly after projection.
   restored=store.get(AgentSession,record.agent_session_id)
   store.save(restored.model_copy(update={'resume_handle':None}))
   await executor.close_thread(thread.thread_id,reason='server_shutdown')
   projection.apply(store.read_events('thread',thread.thread_id,limit=1000)[-1])
   assert conv.get_state(thread.thread_id).agent_session_id==record.agent_session_id
   with patch.object(restarted,'_adapter_for',return_value=adapter):
    try:await restarted.ensure_session(thread,turn)
    except ConversationError as exc:assert exc.code=='conversation.session.resume_unavailable'
    else:raise AssertionError('closed history without handle silently forgotten')
   store.save(restored)
   plan=ThreadPlanSnapshot(phase='executing',last_change_summary='Original plan')
   conv.save_state(conv.get_state(thread.thread_id).model_copy(update={'plan':plan}))
   projection.apply(ev.thread_event(thread.thread_id,ev.EV_RUNTIME_SWITCHED,{'agent_session_id':record.agent_session_id,'runtime_key':selection.session_key,'recovery_kind':'native_resume'}))
   assert conv.get_state(thread.thread_id).plan.last_change_summary=='Original plan'
   # Missing launch identity must be recovered from the accepted turn, including accounts.
   selection=selection.model_copy(update={'credential_id':'offline-account'});conv.save_runtime_selection(selection)
   prior=prior.model_copy(update={'runtime_snapshot':{key:value for key,value in selection.model_dump().items() if isinstance(value,str)}});conv.save_turn(prior)
   conv.save_state(conv.get_state(thread.thread_id).model_copy(update={'session_runtime_key':'','session_launch_cwd':None}))
   with patch.object(executor,'_start',new=AsyncMock(return_value=(adapter,ref))) as reopen:
    await executor.ensure_session(thread,turn)
   assert reopen.await_args.kwargs['resume_handle']=='native-history' and not reopen.await_args.kwargs.get('switched')
   # Uncertain rewind recovery must not repair its pointer back to discarded native history.
   selection=selection.model_copy(update={'credential_id':''});conv.save_runtime_selection(selection)
   conv.save_turn(prior.model_copy(update={'runtime_snapshot':{key:value for key,value in selection.model_dump().items() if isinstance(value,str)}}))
   conv.save_state(conv.get_state(thread.thread_id).model_copy(update={'history_recovery_required':True,'session_runtime_key':selection.session_key}))
   with patch.object(executor,'_start',new=AsyncMock(return_value=(adapter,ref))) as rebuild:
    await executor.ensure_session(thread,turn)
   assert not rebuild.await_args.kwargs.get('resume_handle'),'uncertain rewind resumed discarded native history'
   assert not rebuild.await_args.kwargs.get('agent_session_id')
   assert conv.get_state(thread.thread_id).history_rebuild_pending
   # Catalog discovery also respects rebuild intent and creates a clean target.
   _,fresh=await executor.ensure_capability_session(thread.thread_id)
   assert fresh.agent_session_id!=record.agent_session_id and not launches[-1].resume_handle
   count=len(launches)
   _,continued=await executor.ensure_session(thread,turn)
   assert continued.agent_session_id==fresh.agent_session_id and len(launches)==count,'rewind target replaced with prior branch history'
   # A committed rebuild flag invalidates even an explicitly retained old pointer.
   conv.save_state(conv.get_state(thread.thread_id).model_copy(update={'agent_session_id':record.agent_session_id,'history_rebuild_pending':True}))
   with patch.object(executor,'_start',new=AsyncMock(return_value=(adapter,ref))) as rebuild:
    await executor.ensure_session(thread,turn)
   assert not rebuild.await_args.kwargs.get('resume_handle'),'committed rebuild reused old native branch'
 print('PASS: offline lifecycle reuse, per-turn models, read-only discovery, same-native restart, strict close, resume failure, no cold start, handoff acceptance journal')

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
if __name__=='__main__':
 asyncio.run(check())
 asyncio.run(check_session_lifecycle())
