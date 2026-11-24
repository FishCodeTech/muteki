#!/usr/bin/env python3
"""Real Web + local Runner + Docker + Pi/Ollama coordinator Run across a Web SIGKILL.

Starts a coordinator-mode CTF Run (Pi planner + Pi Worker on Ollama), kills Web while the
Worker container is running, restarts Web on the same state and records what happens to
the Worker, the Run and the coordinator. Use --no-kill for the baseline.
--kill-early kills Web while the coordinator is still planning; --kill-worker also kills the
Worker process while Web is down. Both expect the coordinator to be resumed and to solve.
"""
import argparse,base64,json,os,signal,sqlite3,subprocess,tempfile,time,uuid
from pathlib import Path
from verify_runner_remote_identity import request,raw_request,port
from verify_runner_ipc import read

MODEL=os.environ.get('MUTEKI_TEST_MODEL','deepseek-v4.1-flash:cloud')

def wait(check,seconds=45,what='state'):
    end=time.monotonic()+seconds
    while time.monotonic()<end:
        value=check()
        if value:return value
        time.sleep(.3)
    raise RuntimeError('Timed out waiting for '+what)

def main():
    parser=argparse.ArgumentParser();parser.add_argument('--no-kill',action='store_true');parser.add_argument('--keep',action='store_true');parser.add_argument('--direct',action='store_true');parser.add_argument('--at-worker',action='store_true');parser.add_argument('--kill-early',action='store_true');parser.add_argument('--kill-worker',action='store_true');args=parser.parse_args()
    if args.kill_worker:args.at_worker=True
    binary=str(Path('.cache/target/debug/muteki-server').resolve())
    keep_root=os.environ.get('MUTEKI_KEEP_ROOT')=='1'
    with (tempfile.TemporaryDirectory(prefix='coord-restart-',dir='/tmp') if not keep_root else open(os.devnull)) as tmp:
        if keep_root:tmp=tempfile.mkdtemp(prefix='coord-restart-',dir='/tmp');print('ROOT',tmp,flush=True)
        webroot=Path(tmp).resolve()/'web';webroot.mkdir()
        env=dict(os.environ,MUTEKI_MANAGED_DESKTOP='1',CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC='1')
        env.pop('MUTEKI_RUNNER_SOCKET',None);env.pop('MUTEKI_DESKTOP_MAINTENANCE',None)
        wp=port();url=f'http://127.0.0.1:{wp}'
        log=open(Path(tmp)/'services.log','w+')
        procs=[]
        def start_web():
            p=subprocess.Popen([binary,'--state-dir',str(webroot),'--port',str(wp)],env=env,stdout=log,stderr=log);procs.append(p)
            def ready():
                if p.poll() is not None:raise RuntimeError('Web exited')
                try:return request(url+'/api/runner/nodes')[0]==200
                except OSError:return False
            wait(ready,60,'Web');return p
        def rows(collection):
            with sqlite3.connect(webroot/'muteki.sqlite',timeout=10) as db:return [json.loads(r[0]) for r in db.execute('SELECT value FROM documents WHERE collection=?',(collection,))]
        def run_row(run):return next((r for r in rows('runs') if r.get('run_id')==run),{})
        def workers():
            return subprocess.check_output(['docker','ps','--filter',f'label=muteki.state_dir={webroot}','--format','{{.Names}}'],text=True).split()
        local_pid=None
        try:
            web=start_web()
            local_pid=read(webroot/'runner/control.sock','/v1/health')[1]['pid']
            with sqlite3.connect(webroot/'muteki.sqlite') as db:
                account=dict(id='coord-fixture',engine='api',worker_engine='pi',secret='ollama',base_url='http://host.docker.internal:11434/v1',default_model=MODEL,details={'target_engine':'pi'})
                db.execute('INSERT INTO documents(collection,id,value) VALUES(?,?,?)',('credential_accounts','coord-fixture',json.dumps(account)))
            profile=dict(id='fixture',name='fixture',engine='pi',account_id='coord-fixture',credential_account='coord-fixture',base_url='http://host.docker.internal:11434/v1',model=MODEL,enabled=True,execute_timeout=240,conclude_timeout=15,roles=['solve','respond','review'])
            status,_,saved=raw_request(url+'/api/settings/workers','PUT',json.dumps(dict(worker_profiles=[profile])).encode(),{'Content-Type':'application/json'});assert status==200,saved
            run='coord-'+uuid.uuid4().hex[:10]
            assert request(url+'/api/runs',dict(run_id=run))[0]==200
            flag='flag{'+uuid.uuid4().hex+'}'
            workspace=webroot/'sessions'/run;workspace.mkdir(parents=True,exist_ok=True)
            (workspace/'challenge.txt').write_text('Recover the base64 encoded flag:\n'+base64.b64encode(flag.encode()).decode()+'\n')
            body=dict(command_id='start-'+run,coordinator=not args.direct,cold_start=False,worker_backend='container',worker_node_id='local',worker_container_scope='shared',worker_network='bridge',engines=['fixture'],worker_profiles=[profile],start_workers=1,wall_clock_budget=600,
                **({} if args.direct else dict(llm_profiles=dict(planner=dict(model=MODEL,credential_id='account:coord-fixture')))),
                challenge=dict(name='Coordinator restart',mode='ctf',category='misc',description='Read challenge.txt, decode its base64 payload to recover the flag. Write that exact flag to solved.txt, then submit it with the muteki-blackboard submit-flag command. Finish after submission.',flag_format='flag{...}'))
            status,result=request(url+f'/api/runs/{run}/start',body,timeout=90);assert status==200,result
            timeline=[]
            def worker_processes():
                found=[]
                for name in workers():
                    listed=subprocess.run(['docker','exec',name,'ps','-eo','pid=,args='],capture_output=True,text=True).stdout.splitlines()
                    found+=[(name,line.split(None,1)[0],line.split(None,1)[1]) for line in listed if len(line.split(None,1))==2 and ' pi ' in ' '+line.split(None,1)[1]+' ' and 'runtime-agent' not in line]
                return found
            def snap(label):
                r=run_row(run);timeline.append(dict(t=label,running=r.get('running'),solved=r.get('solved'),workers=workers(),dispatch=[(d.get('worker_id'),d.get('status')) for d in rows('runner_dispatches') if d.get('run_id')==run][-4:],recovery=[(d.get('state'),d.get('reason')) for d in rows('runner_queue_recoveries') if d.get('run_id')==run]))
            if args.kill_early:
                time.sleep(2)
                snap('start_returned')
                web.kill();web.wait(timeout=10)
                snap('after_kill')
                time.sleep(2)
                web=start_web()
                snap('after_restart')
                args.no_kill=True
            try:wait(lambda:(any(d.get('run_id')==run and d.get('status')=='running' and d.get('worker_id','').startswith('cli-') for d in rows('runner_dispatches')) if args.at_worker else [w for w in workers() if 'btw' not in w]),300,'Worker container')
            except RuntimeError:
                status,hist=request(url+f'/api/runs/{run}/events/history?limit=200',timeout=30)
                print('RUN',json.dumps(run_row(run),default=str)[:1500]);print('EVENTS',json.dumps(hist,default=str)[:3000]);print('DISPATCH',json.dumps(rows('runner_dispatches'),default=str)[:1500]);raise
            snap('worker_running')
            if not args.no_kill:
                time.sleep(3)
                web.kill();web.wait(timeout=10)
                snap('after_kill')
                if args.kill_worker:
                    killed=wait(worker_processes,30,'Worker process')
                    for name,pid,_ in killed:subprocess.run(['docker','exec',name,'kill','-9',pid],capture_output=True)
                    timeline.append(dict(t='worker_killed',processes=[(n,p,a[:120]) for n,p,a in killed]))
                    time.sleep(3)
                time.sleep(2)
                web=start_web()
                snap('after_restart')
            deadline=time.monotonic()+420
            while time.monotonic()<deadline:
                r=run_row(run)
                if r.get('running') is False:break
                time.sleep(5);snap('poll')
            snap('final')
            events=[];decisive=[];listed=[]
            try:
                status,hist=request(url+f'/api/runs/{run}/events/history?limit=5000',timeout=60)
                listed=hist.get('events') if isinstance(hist,dict) else hist or []
                errors=sorted({json.dumps(e.get('payload'),sort_keys=True,default=str)[:400] for e in listed if e.get('event_type')=='worker.error'})
                print('WORKER_ERRORS',len(errors),json.dumps(errors,indent=1)[:4000])
                events=[e.get('event_type') for e in listed][-60:]
                decisive=[dict(event=e.get('event_type'),payload=e.get('payload')) for e in listed if e.get('event_type') in ('run.finished','run.restart_recovery','worker.error')
                    or (e.get('event_type')=='blackboard.delta' and (e.get('payload') or {}).get('kind') in ('reason_done','planner_unavailable') and (e.get('payload') or {}).get('status')!='ok')]
            except Exception as error:events=[str(error)]
            solved_file=(workspace/'solved.txt').read_text().strip() if (workspace/'solved.txt').exists() else None
            resumed=any(e.get('event_type')=='run.restart_recovery' and (e.get('payload') or {}).get('coordinator_resumed') is True for e in listed) or any(d['event']=='run.restart_recovery' and (d.get('payload') or {}).get('coordinator_resumed') is True for d in decisive)
            summary=dict(no_kill=args.no_kill,direct=args.direct,kill_early=args.kill_early,kill_worker=args.kill_worker,coordinator_resumed=resumed,
                worker_dispatches=len([d for d in rows('runner_dispatches') if d.get('run_id')==run and str(d.get('worker_id','')).startswith('cli-')]),solved=run_row(run).get('solved'),running=run_row(run).get('running'),flag_file_matches=solved_file==flag,timeline=timeline[-8:],decisive_events=decisive,recent_events=events)
            print(json.dumps(summary,indent=1,default=str)[:6000])
            if args.keep:input('keep running; press enter')
            if args.kill_early or args.kill_worker:
                assert resumed,'coordinator was not resumed after the Web restart'
                assert run_row(run).get('solved') is True and solved_file==flag,'resumed coordinator did not solve the Run'
                print('COORDINATOR_RESUME_OK')
        except Exception:
            log.flush();log.seek(0);print(log.read()[-8000:]);raise
        finally:
            keep=Path('.cache/issue379/coord-last');keep.mkdir(parents=True,exist_ok=True)
            for name in ('muteki.sqlite',):
                if (webroot/name).exists():subprocess.run(['cp',str(webroot/name),str(keep/name)])
            for events_file in (webroot/'sessions').glob('*/events.jsonl'):subprocess.run(['cp',str(events_file),str(keep/'events.jsonl')])
            log.flush();(keep/'services.log').write_text(Path(log.name).read_text())
            for p in procs:
                if p.poll() is None:p.kill();p.wait(timeout=10)
            if local_pid:
                try:os.kill(local_pid,signal.SIGINT)
                except ProcessLookupError:pass
            ids=subprocess.check_output(['docker','ps','-a','--filter',f'label=muteki.state_dir={webroot}','--format','{{.ID}}'],text=True).splitlines()
            if ids:subprocess.run(['docker','rm','-f',*ids],capture_output=True)
            log.close()
if __name__=='__main__':main()
