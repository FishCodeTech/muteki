#!/usr/bin/env python3
"""Run-attributed emergency stop against a controlled HTTP target (real Runner + Docker).

Two shared-scope Runs live in one Worker container. Each starts two beacons that hit its own
unguessable paths on a local HTTP target every second: one foreground command and one
double-forked, setsid detached daemon. Stopping Run A must revoke A's egress (both beacons,
including the detached descendant), mark A's still-running command as stopped without
fabricating an exit, and leave Run B's beacons, container and controls untouched - so the stop
is attributable to one Run and is not a network cut of the shared container. B is stopped after.

No model calls; the Pi/Ollama path is covered by the other acceptance scripts.
"""
import http.server,json,os,signal,subprocess,tempfile,threading,time,uuid
from pathlib import Path
from verify_runner_ipc import read

HITS={}
class Target(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        HITS.setdefault(self.path.strip('/'),[]).append(time.time())
        self.send_response(200);self.end_headers();self.wfile.write(b'ok')
    def log_message(self,*args):pass

DAEMON='''
import os,time,urllib.request
if os.fork(): os._exit(0)
os.setsid()
if os.fork(): os._exit(0)
null=os.open('/dev/null',os.O_RDWR)
for fd in (0,1,2): os.dup2(null,fd)
url='http://host.docker.internal:__PORT__/__PATH__'
while True:
    try: urllib.request.urlopen(url,timeout=2).read()
    except Exception: pass
    time.sleep(1)
'''

def wait(check,seconds=45,what='state'):
    end=time.monotonic()+seconds
    while time.monotonic()<end:
        value=check()
        if value:return value
        time.sleep(.2)
    raise RuntimeError('Timed out waiting for '+what)

def main():
    server=http.server.ThreadingHTTPServer(('0.0.0.0',0),Target)
    target_port=server.server_address[1]
    threading.Thread(target=server.serve_forever,daemon=True).start()
    binary=str(Path('.cache/target/debug/muteki-server').resolve())
    with tempfile.TemporaryDirectory(prefix='estop-',dir='/tmp') as directory:
        root=Path(directory).resolve();socket=root/'runner/control.sock'
        prefix='estop-'+uuid.uuid4().hex[:8]
        env=dict(os.environ);env.pop('MUTEKI_RUNNER_SOCKET',None)
        def call(path,body=None):
            status,value=read(socket,path,body)
            assert status==200,(path,status,value)
            return value
        with tempfile.TemporaryFile() as log:
            runner=subprocess.Popen([binary,'runner','--state-dir',str(root)],env=env,stdout=log,stderr=log)
            try:
                deadline=time.monotonic()+20
                while not socket.exists():
                    if runner.poll() is not None or time.monotonic()>deadline:
                        log.seek(0);raise RuntimeError(log.read().decode())
                    time.sleep(.05)
                def ensure(suffix):
                    run=prefix+'-'+suffix
                    return call('/v1/containers/ensure',{'run_id':run,'owner_run_id':run,
                        'settings':{'worker_container_scope':'shared','worker_network':'bridge'}})
                def start(handle,argv,timeout=900):
                    command=uuid.uuid4().hex
                    result=call('/v1/workers/start',{'run_id':handle['run_id'],'generation':1,'command_id':command,
                        'spec':{'argv':argv,'cwd':handle['container_workspace'],'env':{},'timeout_sec':timeout}})
                    return command,result['worker_id']
                def health(handle):
                    return call('/v1/containers/'+handle['run_id']+'/control',{'generation':1,'op':'Health'})
                def running(container):
                    out=subprocess.run(['docker','inspect',container,'--format','{{.State.Running}}'],capture_output=True,text=True)
                    return out.returncode==0 and out.stdout.strip()=='true'
                def hits(path,after=None):
                    return [t for t in HITS.get(path,[]) if after is None or t>after]

                a,b=ensure('a'),ensure('b')
                assert a['container_id']==b['container_id'],'the two Runs must share one container'
                container=a['container_id']
                beacons={}
                for label,handle in (('a',a),('b',b)):
                    token=uuid.uuid4().hex
                    fg=f'beacon-{label}-fg-{token}';bg=f'beacon-{label}-bg-{token}'
                    beacons[label]=(fg,bg)
                    start(handle,['sh','-c',f'while true; do curl -s -m 2 http://host.docker.internal:{target_port}/{fg} >/dev/null; sleep 1; done'])
                    code,_=start(handle,['python3','-c',DAEMON.replace('__PORT__',str(target_port)).replace('__PATH__',bg)])
                for label in ('a','b'):
                    for path in beacons[label]:
                        wait(lambda path=path:len(hits(path))>=3,120,f'beacon {path}')
                pending_command,pending_worker=start(a,['/bin/sleep','300'])
                time.sleep(2)
                assert 'exit' not in call('/v1/commands/'+pending_command),'the owned command already exited before the stop'

                started=time.monotonic()
                teardown=call('/v1/containers/'+a['run_id']+'/teardown',{'generation':1})
                confirmed=time.time();stop_seconds=round(time.monotonic()-started,3)
                time.sleep(6)
                a_after={path:len(hits(path,confirmed+1.5)) for path in beacons['a']}
                b_after={path:len(hits(path,confirmed+1.5)) for path in beacons['b']}
                receipt=call('/v1/commands/'+pending_command)
                summary=dict(target_port=target_port,shared_container=container,runs={'a':a['run_id'],'b':b['run_id']},
                    beacons=beacons,teardown=teardown,stop_seconds=stop_seconds,
                    a_hits_total={path:len(hits(path)) for path in beacons['a']},
                    a_hits_after_stop=a_after,b_hits_after_a_stop=b_after,
                    pending_receipt=dict(state=receipt.get('state'),execution_stopped=receipt.get('execution_stopped'),
                        exit={k:receipt.get('exit',{}).get(k) for k in ('rc','signalled','ok','timed_out','oom')}),
                    container_running_after_a=running(container),b_health_after_a=health(b).get('ok'),
                    other_paths_seen={k:len(v) for k,v in HITS.items() if k not in sum(beacons.values(),())})
                assert all(count==0 for count in a_after.values()),summary
                assert all(count>=3 for count in b_after.values()),summary
                killed=receipt.get('exit') or {}
                assert killed.get('rc')==137 and killed.get('signalled')==9 and killed.get('ok') is False and killed.get('timed_out') is False,summary
                assert summary['container_running_after_a'] and summary['b_health_after_a'],summary

                b_started=time.monotonic()
                call('/v1/containers/'+b['run_id']+'/teardown',{'generation':1})
                b_confirmed=time.time();time.sleep(6)
                summary.update(b_stop_seconds=round(time.monotonic()-b_started,3),
                    b_hits_after_own_stop={path:len(hits(path,b_confirmed+1.5)) for path in beacons['b']},
                    container_running_after_b=running(container))
                print(json.dumps(summary,indent=1,default=str))
                assert all(count==0 for count in summary['b_hits_after_own_stop'].values()),summary
                assert summary['container_running_after_b'] is False,summary
                print('EMERGENCY_STOP_OK')
            except Exception:
                log.flush();log.seek(0);print(log.read().decode()[-5000:]);print('HITS',{k:len(v) for k,v in HITS.items()});raise
            finally:
                if runner.poll() is None:
                    runner.send_signal(signal.SIGINT)
                    try:runner.wait(timeout=10)
                    except subprocess.TimeoutExpired:runner.kill();runner.wait()
                ids=subprocess.check_output(['docker','ps','-a','--filter',f'label=muteki.state_dir={root}','--format','{{.ID}}'],text=True).splitlines()
                if ids:subprocess.run(['docker','rm','-f',*ids],capture_output=True)
                server.shutdown()
if __name__=='__main__':main()
