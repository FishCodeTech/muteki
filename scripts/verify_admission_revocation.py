#!/usr/bin/env python3
"""Real Web + local Runner + Docker + Pi/Ollama check of node admission and account revocation.

admission: with the node limited to one Worker, a container model probe started while a Pi
Worker runs must wait in the same queue and only be admitted after that Worker is released.
revocation: deleting the credential account of a running Pi Worker must stop that Worker.
"""
import argparse,base64,json,os,signal,sqlite3,subprocess,tempfile,threading,time,urllib.error,urllib.request,uuid
from pathlib import Path
from verify_runner_remote_identity import request,raw_request,port
from verify_runner_ipc import read

MODEL=os.environ.get('MUTEKI_TEST_MODEL','deepseek-v4.1-flash:cloud')
ACCOUNT='adm-fixture'

def call(url,method,body=None,timeout=30):
    data=None if body is None else json.dumps(body).encode()
    req=urllib.request.Request(url,method=method,data=data,headers={'Content-Type':'application/json','X-Muteki-CSRF':'1'})
    try:
        with urllib.request.urlopen(req,timeout=timeout) as result:status,raw=result.status,result.read()
    except urllib.error.HTTPError as error:status,raw=error.code,error.read()
    try:return status,json.loads(raw)
    except ValueError:return status,raw.decode(errors='replace')[:600]

def wait(check,seconds=45,what='state'):
    end=time.monotonic()+seconds
    while time.monotonic()<end:
        value=check()
        if value:return value
        time.sleep(.3)
    raise RuntimeError('Timed out waiting for '+what)

def main():
    parser=argparse.ArgumentParser();parser.add_argument('phase',choices=['admission','revocation'])
    parser.add_argument('--release-after-queue',action='store_true',help='stop the holding Run once the probe is queued so the probe must be admitted after the release')
    args=parser.parse_args()
    binary=str(Path('.cache/target/debug/muteki-server').resolve())
    with tempfile.TemporaryDirectory(prefix='adm-',dir='/tmp') as tmp:
        webroot=Path(tmp).resolve()/'web';webroot.mkdir()
        env=dict(os.environ,MUTEKI_MANAGED_DESKTOP='1',CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC='1')
        env.pop('MUTEKI_RUNNER_SOCKET',None);env.pop('MUTEKI_DESKTOP_MAINTENANCE',None)
        wp=port();url=f'http://127.0.0.1:{wp}'
        log=open(Path(tmp)/'services.log','w+')
        web=subprocess.Popen([binary,'--state-dir',str(webroot),'--port',str(wp)],env=env,stdout=log,stderr=log)
        def rows(collection):
            with sqlite3.connect(webroot/'muteki.sqlite',timeout=10) as db:return [json.loads(r[0]) for r in db.execute('SELECT value FROM documents WHERE collection=?',(collection,))]
        def run_row(run):return next((r for r in rows('runs') if r.get('run_id')==run),{})
        def dispatches():return rows('runner_dispatches')
        def worker_processes():
            found=[]
            for name in subprocess.check_output(['docker','ps','--filter',f'label=muteki.state_dir={webroot}','--format','{{.Names}}'],text=True).split():
                listed=subprocess.run(['docker','exec',name,'ps','-eo','pid=,args='],capture_output=True,text=True).stdout.splitlines()
                found+=[(name,l.strip()[:140]) for l in listed if ' pi ' in ' '+l.split(None,1)[-1]+' ' and 'runtime-agent' not in l]
            return found
        local_pid=None
        try:
            def ready():
                if web.poll() is not None:raise RuntimeError('Web exited')
                try:return request(url+'/api/runner/nodes')[0]==200
                except OSError:return False
            wait(ready,60,'Web')
            local_pid=read(webroot/'runner/control.sock','/v1/health')[1]['pid']
            with sqlite3.connect(webroot/'muteki.sqlite') as db:
                account=dict(id=ACCOUNT,engine='api',worker_engine='pi',secret='ollama',base_url='http://host.docker.internal:11434/v1',default_model=MODEL,details={'target_engine':'pi'})
                db.execute('INSERT INTO documents(collection,id,value) VALUES(?,?,?)',('credential_accounts',ACCOUNT,json.dumps(account)))
            profile=dict(id='fixture',name='fixture',engine='pi',account_id=ACCOUNT,credential_account=ACCOUNT,base_url='http://host.docker.internal:11434/v1',model=MODEL,enabled=True,execute_timeout=400,conclude_timeout=15,roles=['solve','respond','review'])
            status,_,saved=raw_request(url+'/api/settings/workers','PUT',json.dumps(dict(worker_profiles=[profile])).encode(),{'Content-Type':'application/json'});assert status==200,saved
            if args.phase=='admission':
                status,reply=call(url+'/api/runner/resources','PUT',dict(mode='manual',max_workers=1,max_runs=1));assert status==200,reply
            hold=40 if args.phase=='admission' and not args.release_after_queue else 600
            run='adm-'+uuid.uuid4().hex[:10]
            assert request(url+'/api/runs',dict(run_id=run))[0]==200
            flag='flag{'+uuid.uuid4().hex+'}'
            workspace=webroot/'sessions'/run;workspace.mkdir(parents=True,exist_ok=True)
            (workspace/'challenge.txt').write_text('Recover the base64 encoded flag:\n'+base64.b64encode(flag.encode()).decode()+'\n')
            body=dict(command_id='start-'+run,coordinator=False,cold_start=False,worker_backend='container',worker_node_id='local',worker_container_scope='shared',worker_network='bridge',engines=['fixture'],worker_profiles=[profile],start_workers=1,wall_clock_budget=900,
                challenge=dict(name='Admission hold',mode='ctf',category='misc',description=f'First run the shell command `sleep {hold}` and wait for it to finish. Then read challenge.txt, decode its base64 payload to recover the flag, write that exact flag to solved.txt and submit it with the muteki-blackboard submit-flag command.',flag_format='flag{...}'))
            status,result=request(url+f'/api/runs/{run}/start',body,timeout=90);assert status==200,result
            worker=wait(lambda:next((d for d in dispatches() if d.get('run_id')==run and d.get('status')=='running' and str(d.get('worker_id','')).startswith('cli-')),None),300,'Worker dispatch')
            wait(worker_processes,120,'Pi process')
            summary=dict(phase=args.phase,run=run,worker_dispatch=worker['id'])
            if args.phase=='admission':
                probe={}
                def run_probe():
                    started=time.time();status,reply=call(url+f'/api/settings/credentials/{ACCOUNT}/test','POST',dict(model=MODEL,backend='container'),timeout=200)
                    probe.update(status=status,reply=reply,seconds=round(time.time()-started,1))
                thread=threading.Thread(target=run_probe);thread.start()
                queued=wait(lambda:next((d for d in dispatches() if d.get('worker_id')=='probe-1' and d.get('status')=='queued' and d.get('waiting')),None),30,'queued probe')
                worker_state=next(d for d in dispatches() if d['id']==worker['id'])['status']
                if args.release_after_queue:
                    status,stopped=call(url+f'/api/runs/{run}/control','POST',dict(action='stop',command_id='stop-'+run,text='release the node seat'),timeout=120)
                    summary.update(stop_status=status,stop_response=stopped)
                    assert status==200,summary
                    wait(lambda:next((d for d in dispatches() if d['id']==worker['id'] and d.get('status')!='running'),None),120,'Worker release')
                thread.join(timeout=300)
                rows_now={d['id']:d for d in dispatches()}
                probe_row=rows_now[queued['id']];worker_row=rows_now[worker['id']]
                summary.update(probe_http=probe,probe_waiting=queued.get('waiting'),worker_status_while_probe_queued=worker_state,
                    probe_dispatch=dict((k,probe_row.get(k)) for k in ('status','admitted_at','settled_at','admission_sequence')),
                    worker_dispatch_row=dict((k,worker_row.get(k)) for k in ('status','admitted_at','settled_at','admission_sequence')))
                assert worker_state=='running',summary
                assert queued['waiting'].get('reason')=='node_concurrency_limit',summary
                if probe_row.get('admitted_at'):
                    assert worker_row.get('settled_at') and probe_row['admitted_at']>=worker_row['settled_at'],summary
                if args.release_after_queue:
                    summary.update(run_running=run_row(run).get('running'))
                    print(json.dumps(summary,indent=1,default=str))
                    assert probe_row.get('admitted_at'),summary
                    assert isinstance(probe.get('reply'),dict) and probe['reply'].get('ok') is True,summary
                    print('ADMISSION_RELEASE_OK');return
                deadline=time.monotonic()+300
                while time.monotonic()<deadline and run_row(run).get('running') is not False:time.sleep(3)
                summary.update(solved=run_row(run).get('solved'))
                print(json.dumps(summary,indent=1,default=str));print('ADMISSION_QUEUE_OK')
            else:
                status,reply=call(url+f'/api/settings/credential-accounts/{ACCOUNT}?detach_references=true','DELETE')
                deleted_at=time.time()
                summary.update(delete_status=status,delete_reply=reply)
                assert status==200,summary
                stopped=wait(lambda:next((d for d in dispatches() if d['id']==worker['id'] and d.get('status')!='running'),None),120,'Worker stop')
                left=worker_processes()
                status,hist=request(url+f'/api/runs/{run}/events/history?limit=400',timeout=30)
                listed=hist.get('events') if isinstance(hist,dict) else hist or []
                revoked=[e.get('payload') for e in listed if e.get('event_type')=='worker.credential_revoked']
                summary.update(stop_seconds=round(time.time()-deleted_at,1),worker_dispatch_after=stopped.get('status'),pi_processes_after=left,revoked_events=revoked,solved=run_row(run).get('solved'),flag_written=(workspace/'solved.txt').exists())
                print(json.dumps(summary,indent=1,default=str))
                assert isinstance(reply,dict) and reply.get('worker_revocation',{}).get('stop_requested'),summary
                assert revoked and not left and run_row(run).get('solved') is not True,summary
                print('ACCOUNT_REVOCATION_OK')
        except Exception:
            log.flush();log.seek(0);print(log.read()[-6000:]);raise
        finally:
            if web.poll() is None:web.kill();web.wait(timeout=10)
            if local_pid:
                try:os.kill(local_pid,signal.SIGINT)
                except ProcessLookupError:pass
            ids=subprocess.check_output(['docker','ps','-a','--filter',f'label=muteki.state_dir={webroot}','--format','{{.ID}}'],text=True).splitlines()
            if ids:subprocess.run(['docker','rm','-f',*ids],capture_output=True)
            log.close()
if __name__=='__main__':main()
