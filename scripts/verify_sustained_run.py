#!/usr/bin/env python3
"""Sustained multi-task run with memory and file-descriptor sampling (real Web + Runner + Pi/Ollama).

Runs real CTF tasks back to back for a wall-clock budget and samples the Web and Runner
processes' RSS and open file descriptors, the Worker container count and the store size after
every task, so growth over the session can be reported rather than guessed.

Usage: verify_sustained_run.py [--minutes 35] [--parallel 2]
"""
import argparse,base64,json,os,signal,sqlite3,subprocess,tempfile,time,urllib.error,urllib.request,uuid
from pathlib import Path
from verify_runner_remote_identity import request,raw_request,port
from verify_runner_ipc import read

MODEL=os.environ.get('MUTEKI_TEST_MODEL','deepseek-v4.1-flash:cloud')
ACCOUNT='sustained-fixture'

def call(url,method,body=None,timeout=30):
    data=None if body is None else json.dumps(body).encode()
    req=urllib.request.Request(url,method=method,data=data,headers={'Content-Type':'application/json','X-Muteki-CSRF':'1'})
    try:
        with urllib.request.urlopen(req,timeout=timeout) as result:status,raw=result.status,result.read()
    except urllib.error.HTTPError as error:status,raw=error.code,error.read()
    try:return status,json.loads(raw)
    except ValueError:return status,raw.decode(errors='replace')[:600]

def wait(check,seconds=60,what='state'):
    end=time.monotonic()+seconds
    while time.monotonic()<end:
        value=check()
        if value:return value
        time.sleep(.5)
    raise RuntimeError('Timed out waiting for '+what)

def wait_for_idle_resources(pid, list_containers, timeout=60):
    remaining = {}
    def released():
        probe = subprocess.run(
            ['lsof', '-Fn', '-p', str(pid)], check=True, capture_output=True, text=True,
        )
        remaining['locks'] = [line[1:] for line in probe.stdout.splitlines()
                              if line.startswith('n') and line.endswith('/execution-owner.lock')]
        remaining['containers'] = list_containers()
        return not any(remaining.values())
    try:
        wait(released, timeout, 'idle resources')
    except RuntimeError as exc:
        raise RuntimeError(f'{exc}: {remaining}') from exc


def rss_kib(pid):
    out=subprocess.run(['ps','-o','rss=','-p',str(pid)],capture_output=True,text=True)
    try:return int(out.stdout.strip() or 0)
    except ValueError:return 0

def descriptors(pid):
    out=subprocess.run(['lsof','-p',str(pid)],capture_output=True,text=True)
    return max(len(out.stdout.splitlines())-1,0)

def main():
    parser=argparse.ArgumentParser()
    parser.add_argument('--minutes',type=float,default=35.0)
    parser.add_argument('--parallel',type=int,default=2)
    args=parser.parse_args()
    binary=str(Path('.cache/target/debug/muteki-server').resolve())
    with tempfile.TemporaryDirectory(prefix='sustain-',dir='/tmp') as tmp:
        webroot=Path(tmp).resolve()/'web';webroot.mkdir()
        env=dict(os.environ,MUTEKI_MANAGED_DESKTOP='1',CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC='1')
        env.pop('MUTEKI_RUNNER_SOCKET',None);env.pop('MUTEKI_DESKTOP_MAINTENANCE',None)
        wp=port();url=f'http://127.0.0.1:{wp}'
        log=open(Path(tmp)/'services.log','w+')
        web=subprocess.Popen([binary,'--state-dir',str(webroot),'--port',str(wp)],env=env,stdout=log,stderr=log)
        def rows(collection):
            with sqlite3.connect(webroot/'muteki.sqlite',timeout=10) as db:
                return [json.loads(r[0]) for r in db.execute('SELECT value FROM documents WHERE collection=?',(collection,))]
        def run_row(run):return next((r for r in rows('runs') if r.get('run_id')==run),{})
        def containers():
            listed=subprocess.run(['docker','ps','--filter',f'label=muteki.state_dir={webroot}','--format','{{.Names}}'],
                capture_output=True,text=True,check=True).stdout.split()
            return listed
        runner_pid=None
        try:
            def ready():
                if web.poll() is not None:raise RuntimeError('Web exited')
                try:return request(url+'/api/runner/nodes')[0]==200
                except OSError:return False
            wait(ready,60,'Web')
            runner_pid=read(webroot/'runner/control.sock','/v1/health')[1]['pid']
            with sqlite3.connect(webroot/'muteki.sqlite') as db:
                account=dict(id=ACCOUNT,engine='api',worker_engine='pi',secret='ollama',
                    base_url='http://host.docker.internal:11434/v1',default_model=MODEL,details={'target_engine':'pi'})
                db.execute('INSERT INTO documents(collection,id,value) VALUES(?,?,?)',('credential_accounts',ACCOUNT,json.dumps(account)))
            profile=dict(id='fixture',name='fixture',engine='pi',account_id=ACCOUNT,credential_account=ACCOUNT,
                base_url='http://host.docker.internal:11434/v1',model=MODEL,enabled=True,execute_timeout=240,conclude_timeout=15,
                roles=['solve','respond','review'])
            status,_,saved=raw_request(url+'/api/settings/workers','PUT',json.dumps(dict(worker_profiles=[profile])).encode(),
                {'Content-Type':'application/json'});assert status==200,saved
            status,reply=call(url+'/api/runner/resources','PUT',dict(mode='manual',max_workers=args.parallel,max_runs=args.parallel))
            assert status==200,reply

            def sample(label):
                return dict(label=label,at=round(time.time(),1),
                    web_rss_kib=rss_kib(web.pid),web_fds=descriptors(web.pid),
                    runner_rss_kib=rss_kib(runner_pid),runner_fds=descriptors(runner_pid),
                    worker_containers=len(containers()),
                    store_bytes=(webroot/'muteki.sqlite').stat().st_size,
                    dispatch_rows=len(rows('runner_dispatches')))
            def launch(run=None):
                fresh=run is None
                run=run or 'sustain-'+uuid.uuid4().hex[:10]
                if fresh:assert request(url+'/api/runs',dict(run_id=run))[0]==200
                flag='flag{'+uuid.uuid4().hex+'}'
                workspace=webroot/'sessions'/run;workspace.mkdir(parents=True,exist_ok=True)
                (workspace/'challenge.txt').write_text('Recover the base64 encoded flag:\n'+base64.b64encode(flag.encode()).decode()+'\n')
                body=dict(command_id='start-'+uuid.uuid4().hex,coordinator=False,cold_start=False,worker_backend='container',
                    worker_node_id='local',worker_container_scope='shared',worker_network='bridge',engines=['fixture'],
                    worker_profiles=[profile],start_workers=1,wall_clock_budget=420,
                    challenge=dict(name='Sustained cycle',mode='ctf',category='misc',
                        description='Read challenge.txt and decode its base64 payload. Before submitting, you MUST write that exact flag to solved.txt in the current workspace and read solved.txt back using Bash. Only then submit it with muteki-blackboard submit-flag. Finish after submission.',
                        flag_format='flag{...}'))
                status,result=request(url+f'/api/runs/{run}/start',body,timeout=90);assert status==200,result
                return run,flag,workspace

            samples=[sample('start')]
            cycles=[];deadline=time.monotonic()+args.minutes*60
            while time.monotonic()<deadline:
                batch=[launch() for _ in range(args.parallel)]
                for run,flag,workspace in batch:
                    try:
                        wait(lambda run=run:run_row(run).get('running') is False,600,'task '+run)
                        row=run_row(run)
                        cycles.append(dict(run=run,solved=row.get('solved'),
                            artifact=(workspace/'solved.txt').exists() and flag in (workspace/'solved.txt').read_text(errors='replace')))
                    except RuntimeError as error:
                        cycles.append(dict(run=run,solved=run_row(run).get('solved'),error=str(error)))
                samples.append(sample(f'after-{len(cycles)}'))
            wait_for_idle_resources(web.pid, containers)
            samples.append(sample('idle-settled'))
            idle_fds=subprocess.run(['lsof','-Fn','-p',str(web.pid)],capture_output=True,text=True).stdout.splitlines()
            assert not any(name.endswith('execution-owner.lock') for name in idle_fds),'Idle ownership locks were retained'
            original=cycles[0]['run'];generation=run_row(original)['execution_generation']
            resumed,flag,workspace=launch(original)
            wait(lambda:run_row(resumed).get('running') is False,600,'restarted idle task')
            assert run_row(resumed)['execution_generation']==generation+1
            cycles.append(dict(run=resumed,solved=run_row(resumed).get('solved'),
                artifact=(workspace/'solved.txt').exists() and flag in (workspace/'solved.txt').read_text(errors='replace')))
            wait_for_idle_resources(web.pid, containers)
            samples.append(sample('idle-task-restarted-and-settled'))
            leftover=containers()
            solved=[c for c in cycles if c.get('solved') is True]
            first,last=samples[0],samples[-1]
            peak={key:max(s[key] for s in samples) for key in ('web_rss_kib','web_fds','runner_rss_kib','runner_fds','worker_containers')}
            summary=dict(idle_task_restarted=True,minutes=args.minutes,parallel=args.parallel,tasks=len(cycles),solved=len(solved),
                artifacts=len([c for c in cycles if c.get('artifact')]),
                missing_artifacts=[c['run'] for c in cycles if not c.get('artifact')],
                web_fd_names=subprocess.run(['lsof','-Fn','-p',str(web.pid)],capture_output=True,text=True).stdout.splitlines(),failures=[c for c in cycles if c.get('solved') is not True],
                first_sample=first,last_sample=last,peak=peak,
                growth=dict(web_rss_kib=last['web_rss_kib']-first['web_rss_kib'],web_fds=last['web_fds']-first['web_fds'],
                    runner_rss_kib=last['runner_rss_kib']-first['runner_rss_kib'],runner_fds=last['runner_fds']-first['runner_fds'],
                    store_bytes=last['store_bytes']-first['store_bytes']),
                leftover_containers=leftover,samples=samples)
            print(json.dumps(summary,indent=1,default=str))
            assert cycles and len(solved)==len(cycles),'not every task solved'
            assert all(c.get('artifact') is True for c in cycles),'not every task returned its artifact'
            assert not leftover,'Worker containers were left behind'
            assert not any(name.endswith('execution-owner.lock') for name in summary['web_fd_names']),'Finished local Runs retained ownership lock FDs'
            print('SUSTAINED_RUN_OK')
        except Exception:
            log.flush();log.seek(0);print(log.read()[-6000:]);raise
        finally:
            if web.poll() is None:web.kill();web.wait(timeout=10)
            if runner_pid:
                try:os.kill(runner_pid,signal.SIGINT)
                except ProcessLookupError:pass
            ids=subprocess.check_output(['docker','ps','-a','--filter',f'label=muteki.state_dir={webroot}','--format','{{.ID}}'],text=True).splitlines()
            if ids:subprocess.run(['docker','rm','-f',*ids],capture_output=True)
            log.close()
if __name__=='__main__':main()
