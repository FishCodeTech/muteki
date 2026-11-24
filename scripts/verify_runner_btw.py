#!/usr/bin/env python3
"""Real Web + local Runner + Docker + Pi/Ollama check of auxiliary (BTW) Worker admission.

Solves one ordinary Run, limits the node to one Run slot, then fires two BTW side
questions at once. The second must wait in the durable node queue instead of
bypassing the limit, and both must finish with a real model answer.
"""
import json,os,signal,sqlite3,subprocess,tempfile,threading,time,urllib.request,uuid,base64
from pathlib import Path
from verify_runner_remote_identity import request,raw_request,port
from verify_runner_ipc import read

MODEL=os.environ.get('MUTEKI_TEST_MODEL','deepseek-v4.1-flash:cloud')

def wait(check,seconds=45):
    end=time.monotonic()+seconds
    while time.monotonic()<end:
        value=check()
        if value:return value
        time.sleep(.2)
    raise RuntimeError('Timed out waiting for real service state')

def stream(url,body,out):
    frames=[]
    req=urllib.request.Request(url,json.dumps(body).encode(),{'Content-Type':'application/json','X-Muteki-CSRF':'1'},method='POST')
    try:
        with urllib.request.urlopen(req,timeout=240) as response:
            for raw in response:
                line=raw.decode().strip()
                if line.startswith('data: '):frames.append(json.loads(line[6:]))
    except Exception as error:frames.append({'error':str(error),'done':True})
    out.append(frames)

def main():
    binary=str(Path('.cache/target/debug/muteki-server').resolve())
    with tempfile.TemporaryDirectory(prefix='runner-btw-',dir='/tmp') as tmp:
        webroot=Path(tmp).resolve()/'web';webroot.mkdir()
        env=dict(os.environ,MUTEKI_MANAGED_DESKTOP='1',CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC='1')
        env.pop('MUTEKI_RUNNER_SOCKET',None);env.pop('MUTEKI_DESKTOP_MAINTENANCE',None)
        wp=port();url=f'http://127.0.0.1:{wp}'
        log=open(Path(tmp)/'services.log','w+')
        web=subprocess.Popen([binary,'--state-dir',str(webroot),'--port',str(wp)],env=env,stdout=log,stderr=log)
        local_pid=None
        def rows(collection):
            with sqlite3.connect(webroot/'muteki.sqlite') as db:return [json.loads(r[0]) for r in db.execute('SELECT value FROM documents WHERE collection=?',(collection,))]
        try:
            def ready():
                if web.poll() is not None:raise RuntimeError('Web exited')
                try:return request(url+'/api/runner/nodes')[0]==200
                except OSError:return False
            wait(ready)
            local_pid=read(webroot/'runner/control.sock','/v1/health')[1]['pid']
            with sqlite3.connect(webroot/'muteki.sqlite') as db:
                account=dict(id='btw-fixture',engine='api',worker_engine='pi',secret='ollama',base_url='http://host.docker.internal:11434/v1',default_model=MODEL,details={'target_engine':'pi'})
                db.execute('INSERT INTO documents(collection,id,value) VALUES(?,?,?)',('credential_accounts','btw-fixture',json.dumps(account)))
            profile=dict(id='fixture',name='fixture',engine='pi',account_id='btw-fixture',credential_account='btw-fixture',base_url='http://host.docker.internal:11434/v1',model=MODEL,enabled=True,execute_timeout=180,conclude_timeout=15,roles=['solve','respond','review'])
            status,_,saved=raw_request(url+'/api/settings/workers','PUT',json.dumps(dict(worker_profiles=[profile])).encode(),{'Content-Type':'application/json'});assert status==200,saved
            def seed():
                run='btw-'+uuid.uuid4().hex[:12]
                assert request(url+'/api/runs',dict(run_id=run))[0]==200
                flag='flag{'+uuid.uuid4().hex+'}'
                workspace=webroot/'sessions'/run;workspace.mkdir(parents=True,exist_ok=True)
                (workspace/'challenge.txt').write_text('Recover the base64 encoded flag:\n'+base64.b64encode(flag.encode()).decode()+'\n')
                body=dict(command_id='start-'+run,coordinator=False,cold_start=False,worker_backend='container',worker_node_id='local',worker_container_scope='shared',worker_network='bridge',engines=['fixture'],worker_profiles=[profile],start_workers=1,wall_clock_budget=240,challenge=dict(name='BTW acceptance',mode='ctf',category='misc',description='Read challenge.txt, decode its base64 payload to recover the flag. Write that exact flag to solved.txt, then submit it with the muteki-blackboard submit-flag command. Finish after submission.',flag_format='flag{...}'))
                status,result=request(url+f'/api/runs/{run}/start',body,timeout=60);assert status==200,result
                wait(lambda:next((r for r in rows('runs') if r.get('run_id')==run and r.get('running') is False),None),240)
                assert next(r for r in rows('runs') if r.get('run_id')==run).get('solved') is True,'seed Run did not solve'
                return run
            runs=[seed(),seed()]
            status,_,policy=raw_request(url+'/api/runner/resources','PUT',json.dumps(dict(mode='manual',max_workers=1,max_runs=1)).encode(),{'Content-Type':'application/json'});assert status==200,policy
            samples=[];stop=threading.Event()
            def sampler():
                while not stop.is_set():
                    samples.append([(r['worker_id'],r['status']) for r in rows('runner_dispatches') if str(r.get('worker_id','')).startswith('btw-')])
                    time.sleep(.2)
            threading.Thread(target=sampler,daemon=True).start()
            outs=[];threads=[]
            for run,question in zip(runs,('Reply with exactly one word: the first word of the challenge description.','Reply with exactly one word: the challenge category.')):
                t=threading.Thread(target=stream,args=(url+f'/api/runs/{run}/btw',dict(question=question,transcript=[]),outs));t.start();threads.append(t);time.sleep(1.5)
            for t in threads:t.join(260)
            stop.set()
            assert len(outs)==2,outs
            for frames in outs:
                assert frames and frames[-1].get('done') is True and not any('error' in f for f in frames),frames
                assert any(f.get('delta') or f.get('answer') for f in frames),frames
            assert any(f.get('progress',{}).get('phase')=='queued' for frames in outs for f in frames),'second BTW never reported waiting for node capacity'
            peak=max(sum(1 for _,s in sample if s in ('running','detached')) for sample in samples)
            assert peak<=1,('auxiliary Workers exceeded the one-slot node limit',peak)
            final=[r for r in rows('runner_dispatches') if str(r.get('worker_id','')).startswith('btw-')]
            assert len(final)==2 and all(r['status'] in ('released','detached') for r in final),final
            def containers():return subprocess.check_output(['docker','ps','-a','--filter',f'label=muteki.state_dir={webroot}','--format','{{.Names}}'],text=True).split()
            wait(lambda:not [n for n in containers() if 'btw' in n],60)
            leftover=containers()
            print(json.dumps(dict(actual_services=True,provider='real Ollama',model=MODEL,btw_reservations=len(final),peak_concurrent_auxiliary=peak,second_request_queued=True,both_answered=True,auxiliary_containers_removed=True,remaining_shared_pool_containers=len(leftover)),indent=2))
        except Exception:
            log.flush();log.seek(0);print(log.read()[-12000:]);raise
        finally:
            if web.poll() is None:web.kill();web.wait(timeout=10)
            if local_pid:
                try:os.kill(local_pid,signal.SIGINT)
                except ProcessLookupError:pass
            ids=subprocess.check_output(['docker','ps','-a','--filter',f'label=muteki.state_dir={webroot}','--format','{{.ID}}'],text=True).splitlines()
            if ids:subprocess.run(['docker','rm','-f',*ids],capture_output=True)
            log.close()
if __name__=='__main__':main()
