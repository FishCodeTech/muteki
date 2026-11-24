#!/usr/bin/env python3
"""Real Web restart -> saved queue -> remote Runner -> installed CLI task flow.
Uses the actual Pi CLI and an actual installed Ollama model. No Rust test harness.
"""
import argparse,base64,json,os,signal,socket,sqlite3,subprocess,tempfile,time,uuid
from pathlib import Path
from verify_runner_remote_identity import request,raw_request,port
from verify_runner_ipc import read

def wait(check,seconds=45):
    end=time.monotonic()+seconds
    while time.monotonic()<end:
        value=check()
        if value:return value
        time.sleep(.15)
    raise RuntimeError('Timed out waiting for real service state')

def main():
    parser=argparse.ArgumentParser();parser.add_argument('--scope',choices=['run','shared'],default='shared');parser.add_argument('--model',default='deepseek-v4.1-flash:cloud');parser.add_argument('--active-restart',action='store_true');parser.add_argument('--disconnect-stop',action='store_true');args=parser.parse_args();args.active_restart=args.active_restart or args.disconnect_stop
    binary=str(Path('.cache/target/debug/muteki-server').resolve())
    with tempfile.TemporaryDirectory(prefix='runner-task-restart-',dir='/tmp') as tmp:
        root=Path(tmp).resolve();webroot=root/'web';node=root/'node';webroot.mkdir()
        def openssl(args):subprocess.run(['openssl',*args],check=True,capture_output=True)
        openssl(['req','-x509','-newkey','rsa:2048','-nodes','-days','1','-subj','/CN=Task Runner CA','-addext','basicConstraints=critical,CA:TRUE','-keyout',str(root/'ca-key.pem'),'-out',str(root/'ca.pem')])
        openssl(['req','-new','-newkey','rsa:2048','-nodes','-subj','/CN=localhost','-keyout',str(root/'key.pem'),'-out',str(root/'server.csr')])
        (root/'ext').write_text('basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:localhost\n')
        openssl(['x509','-req','-in',str(root/'server.csr'),'-CA',str(root/'ca.pem'),'-CAkey',str(root/'ca-key.pem'),'-CAcreateserial','-days','1','-extfile',str(root/'ext'),'-out',str(root/'cert.pem')])
        env=dict(os.environ,MUTEKI_MANAGED_DESKTOP='1',CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC='1')
        env.pop('MUTEKI_RUNNER_SOCKET',None);env.pop('MUTEKI_DESKTOP_MAINTENANCE',None)
        wp,tp=port(),port();url=f'http://127.0.0.1:{wp}';web=None;runner=None;local_pid=None
        log=open(root/'services.log','w+')
        def start_web():
            process=subprocess.Popen([binary,'--state-dir',str(webroot),'--port',str(wp),'--runner-tls-listen',f'127.0.0.1:{tp}','--runner-tls-cert',str(root/'cert.pem'),'--runner-tls-key',str(root/'key.pem')],env=env,stdout=log,stderr=log)
            def ready():
                if process.poll() is not None:raise RuntimeError('Web exited')
                try:return request(url+'/api/runner/nodes')[0]==200
                except OSError:return False
            wait(ready);return process
        def rows(collection):
            with sqlite3.connect(webroot/'muteki.sqlite') as db:return [json.loads(r[0]) for r in db.execute('SELECT value FROM documents WHERE collection=?',(collection,))]
        try:
            web=start_web();local_pid=read(webroot/'runner/control.sock','/v1/health')[1]['pid']
            with sqlite3.connect(webroot/'muteki.sqlite') as db:
                account=dict(id='restart-fixture',engine='api',worker_engine='pi',secret='ollama',base_url='http://host.docker.internal:11434/v1',default_model=args.model,details={'target_engine':'pi'})
                db.execute('INSERT INTO documents(collection,id,value) VALUES(?,?,?)',('credential_accounts','restart-fixture',json.dumps(account)))
            status,enrollment=request(url+'/api/runner/enrollments',dict(name='restart-node',projects=['personal'],ttl_seconds=120));assert status==200,enrollment
            token=root/'enroll';token.write_text(enrollment['token']);token.chmod(0o600)
            subprocess.run([binary,'runner','enroll','--state-dir',str(node),'--server',f'https://localhost:{tp}','--token-file',str(token),'--ca-file',str(root/'ca.pem')],env=env,check=True,capture_output=True)
            node_id=json.loads((node/'runner/remote.json').read_text())['credential']['node_id']
            run='task-'+uuid.uuid4().hex
            assert request(url+'/api/runs',dict(run_id=run))[0]==200
            flag='flag{'+uuid.uuid4().hex+'}'
            workspace=webroot/'sessions'/run;workspace.mkdir(parents=True,exist_ok=True)
            (workspace/'challenge.txt').write_text('Recover the base64 encoded flag:\n'+base64.b64encode(flag.encode()).decode()+'\n')
            body=dict(command_id='start-'+run,coordinator=False,cold_start=False,worker_backend='container',worker_node_id=node_id,worker_container_scope=args.scope,worker_network='bridge',engines=['fixture'],worker_profiles=[dict(id='fixture',engine='pi',account_id='restart-fixture',model=args.model,enabled=True,execute_timeout=180,conclude_timeout=15)],start_workers=1,wall_clock_budget=240,challenge=dict(name='Restart acceptance',mode='ctf',category='misc',description='Read challenge.txt, decode its base64 payload to recover the flag. Write that exact flag to solved.txt, then submit it with the muteki-blackboard submit-flag command. Finish after submission.',flag_format='flag{...}'))
            if args.active_restart:
                body['challenge']['description']='First write started.txt containing RUNNING, then execute sleep 40 using Bash. Only after waiting, '+body['challenge']['description']
                runner=subprocess.Popen([binary,'runner','--remote','--state-dir',str(node)],env=env,stdout=log,stderr=log)
            status,result=request(url+f'/api/runs/{run}/start',body,timeout=45);assert status==200,result
            if args.active_restart:
                wait(lambda:next(node.rglob('started.txt'),None),120)
                before=[r for r in rows('runner_commands') if r.get('run_id')==run]
                assert len(before)==1,before
                web.kill();web.wait(timeout=10);web=start_web()
                status,response=request(url+f'/api/runs/{run}/resolve',dict(command_id='resume-'+run),timeout=15)
                assert status==409 and response.get('error',{}).get('code')=='run.execution.unconfirmed',response
                wait(lambda:any(n.get('id')==node_id and n.get('connected') for n in request(url+'/api/runner/nodes')[1]['nodes']))
                runtime=next(r for r in rows('runner_controller_runs') if r.get('run_id')==run)
                container_id=runtime['container_id'];assert len(container_id)==64
                if args.disconnect_stop:
                    runner.send_signal(signal.SIGSTOP)
                    wait(lambda:all(not n.get('connected') for n in request(url+'/api/runner/nodes')[1]['nodes'] if n.get('id')==node_id),20)
                status,response=request(url+f'/api/runs/{run}/control',dict(action='stop',command_id='stop-'+run),timeout=45)
                if args.disconnect_stop:
                    assert status==200 and response.get('status')=='unknown' and not response.get('ok'),response
                    pending=next(r for r in rows('runs') if r.get('run_id')==run)
                    assert pending.get('status')=='stopping' and pending.get('running'),pending
                    web.kill();web.wait(timeout=10);web=start_web()
                    runner.send_signal(signal.SIGCONT)
                    response=wait(lambda:(lambda r:r if r.get('status')=='applied' else None)(request(url+f'/api/runs/{run}/control/stop-'+run)[1]),50)
                assert status==200 and response.get('ok'),response
                after=[r for r in rows('runner_commands') if r.get('run_id')==run]
                assert len(after)==1 and after[0].get('execution_stopped'),after
                inspected=subprocess.run(['docker','inspect',container_id],capture_output=True,text=True)
                if inspected.returncode==0:
                    physical=json.loads(inspected.stdout)[0]['State'];assert not physical['Running'] and physical['Pid']==0,physical
                else:assert 'No such' in inspected.stderr,inspected.stderr
                wait(lambda:(workspace/'started.txt').exists(),45)
                assert (workspace/'started.txt').read_text().strip()=='RUNNING'
                current=next(r for r in rows('runs') if r.get('run_id')==run)
                assert current['running'] is False,current
                print(json.dumps(dict(actual_web_kill_during_pi_task=True,disconnected_stop_reconciled=args.disconnect_stop,stop_intent_survived_web_restart=args.disconnect_stop,duplicate_resume_rejected=True,original_execution_stop_confirmed=True,docker_exit_independently_confirmed=True,stopped_artifact_returned=True,start_count=1,scope=args.scope,provider='real Ollama',engine='pi',model=args.model),indent=2))
                return
            queued=wait(lambda:next((r for r in rows('runner_dispatches') if r.get('run_id')==run and r.get('status')=='queued'),None))
            assert not [r for r in rows('runner_commands') if r.get('run_id')==run], 'offline node unexpectedly executed work'
            status,_,_=raw_request(url+f"/api/runner/dispatches/{queued['id']}/priority",'PUT',json.dumps(dict(priority=7)).encode(),{'Content-Type':'application/json'});assert status==200
            web.kill();web.wait(timeout=10);web=start_web()
            recovered=wait(lambda:next((r for r in rows('runner_dispatches') if r.get('restored_from')==queued['id'] and r.get('status')=='queued'),None))
            assert recovered['priority']==7 and recovered['queued_at']==queued['queued_at'],recovered
            assert not [r for r in rows('runner_commands') if r.get('run_id')==run]
            runner=subprocess.Popen([binary,'runner','--remote','--state-dir',str(node)],env=env,stdout=log,stderr=log)
            def finished():
                current=next((r for r in rows('runs') if r.get('run_id')==run),{})
                return current if current.get('running') is False else None
            result=wait(finished,210)
            commands=[r for r in rows('runner_commands') if r.get('run_id')==run]
            assert commands and all(r.get('exit') or r.get('execution_stopped') for r in commands),(result,commands)
            assert result.get('solved') is True and flag in result.get('flags',[]),result
            wait(lambda:(workspace/'solved.txt').is_file(),45)
            assert (workspace/'solved.txt').read_text().strip()==flag,'Remote artifact was not returned'
            print(json.dumps(dict(actual_web_kill_restart=True,remote_offline_queue_preserved=True,queue_priority_and_time_preserved=True,actual_cli_model_protocol=True,task_solved=True,start_count=len(commands),scope=args.scope,provider='real Ollama',engine='pi',model=args.model,artifact_returned=True),indent=2))
        except Exception:
            log.flush();log.seek(0);print(log.read()[-18000:]);print('run records:',[{k:r.get(k) for k in ['run_id','status','solved','outcome_reason','outcome_detail']} for r in rows('runs')] if (webroot/'muteki.sqlite').exists() else [])
            for path in (webroot/'sessions').glob('*/events.jsonl'):
                for line in path.read_text().splitlines():
                    event=json.loads(line)
                    if event.get('event_type') in ['terminal.output','worker.error','tool.result','run.finished']:print(json.dumps(event,ensure_ascii=False))
            raise
        finally:
            for proc in [runner,web]:
                if proc and proc.poll() is None:proc.kill();proc.wait(timeout=10)
            if local_pid:
                try:os.kill(local_pid,signal.SIGINT)
                except ProcessLookupError:pass
            ids=subprocess.check_output(['docker','ps','-a','--filter',f'label=muteki.state_dir={node}','--format','{{.ID}}'],text=True).splitlines()
            if ids:subprocess.run(['docker','rm','-f',*ids],check=True,capture_output=True)
            log.close()
if __name__=='__main__':main()
