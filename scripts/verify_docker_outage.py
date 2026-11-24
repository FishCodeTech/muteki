#!/usr/bin/env python3
"""Docker control-plane outage window against a real Runner, without touching the real daemon.

The Runner talks to Docker through a Unix-socket relay this script owns, so killing the relay
makes the daemon unreachable for the Runner while every container on the host keeps running
(restarting the real daemon would stop unrelated containers). During the window the Runner must
refuse new container work with an explicit error, must not fabricate an exit for the Worker it
already supervises - that supervision runs over the state directory, not Docker - and must
recover once the relay is back.

No model calls.
"""
import json,os,signal,subprocess,tempfile,time,uuid
from pathlib import Path
from verify_runner_ipc import read

RELAY='''
import asyncio,sys
listen,upstream=sys.argv[1],sys.argv[2]
async def pipe(reader,writer):
    try:
        while True:
            chunk=await reader.read(65536)
            if not chunk:break
            writer.write(chunk);await writer.drain()
    except Exception:pass
    finally:
        try:writer.close()
        except Exception:pass
async def handle(client_reader,client_writer):
    try:up_reader,up_writer=await asyncio.open_unix_connection(upstream)
    except Exception:
        client_writer.close();return
    await asyncio.gather(pipe(client_reader,up_writer),pipe(up_reader,client_writer))
async def main():
    server=await asyncio.start_unix_server(handle,path=listen)
    async with server:await server.serve_forever()
asyncio.run(main())
'''

def wait(check,seconds=30,what='state'):
    end=time.monotonic()+seconds
    while time.monotonic()<end:
        value=check()
        if value:return value
        time.sleep(.2)
    raise RuntimeError('Timed out waiting for '+what)

def main():
    binary=str(Path('.cache/target/debug/muteki-server').resolve())
    with tempfile.TemporaryDirectory(prefix='dockout-',dir='/tmp') as directory:
        root=Path(directory).resolve();socket=root/'runner/control.sock'
        relay_path=root/'docker.sock'
        prefix='dockout-'+uuid.uuid4().hex[:8]
        relay=None
        def start_relay():
            if relay_path.exists():relay_path.unlink()
            child=subprocess.Popen(['python3','-c',RELAY,str(relay_path),'/var/run/docker.sock'],
                stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
            wait(lambda:relay_path.exists(),15,'the Docker relay socket')
            return child
        def stop_relay(child):
            child.send_signal(signal.SIGKILL);child.wait()
            if relay_path.exists():relay_path.unlink()
        relay=start_relay()
        env=dict(os.environ,DOCKER_HOST=f'unix://{relay_path}')
        env.pop('MUTEKI_RUNNER_SOCKET',None);env.pop('DOCKER_CONTEXT',None)
        def call(path,body=None):
            return read(socket,path,body)
        def ok(path,body=None):
            status,value=call(path,body)
            assert status==200,(path,status,value)
            return value
        with tempfile.TemporaryFile() as log:
            runner=subprocess.Popen([binary,'runner','--state-dir',str(root)],env=env,stdout=log,stderr=log)
            container=None
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
                status,a=ensure('a');assert status==200,a
                container=a['container_id']
                command=uuid.uuid4().hex
                started=ok('/v1/workers/start',{'run_id':a['run_id'],'generation':1,'command_id':command,
                    'spec':{'argv':['sh','-c','while true; do echo tick >> ticks.txt; sleep 1; done'],
                            'cwd':a['container_workspace'],'env':{},'timeout_sec':900}})
                ticks=Path(a['host_workspace'])/'ticks.txt'
                wait(lambda:ticks.exists() and len(ticks.read_bytes())>0,60,'the Worker to produce output')
                before=len(ticks.read_bytes())

                stop_relay(relay);outage_started=time.monotonic()
                blocked_status,blocked=ensure('during-outage')
                health_status,health=call('/v1/containers/'+a['run_id']+'/control',{'generation':1,'op':'Health'})
                time.sleep(12)
                during=len(ticks.read_bytes())
                receipt_during=ok('/v1/commands/'+command)
                resources_status,resources=call('/v1/resources')
                outage_seconds=round(time.monotonic()-outage_started,2)
                relay=start_relay()

                def recovered_handle():
                    status, handle = ensure('after-outage')
                    return handle if status == 200 else None
                recovered=wait(recovered_handle,120,'the Runner to recover after the outage')
                after=len(ticks.read_bytes())
                receipt_after=ok('/v1/commands/'+command)
                teardown=ok('/v1/containers/'+a['run_id']+'/teardown',{'generation':1})
                summary=dict(shared_container=container,outage_seconds=outage_seconds,
                    ensure_during_outage=dict(status=blocked_status,error=str(blocked)[:300]),
                    supervised_health_during_outage=dict(status=health_status,ok=(health or {}).get('ok') if isinstance(health,dict) else None),
                    resources_during_outage=dict(status=resources_status,error=(resources or {}).get('error') if isinstance(resources,dict) else None),
                    worker_output_bytes=dict(before=before,during=during,after=after),
                    no_fabricated_exit_during_outage='exit' not in receipt_during,
                    no_fabricated_exit_after_outage='exit' not in receipt_after,
                    ensure_after_outage=recovered['container_id']==container,
                    teardown_after_outage=teardown)
                print(json.dumps(summary,indent=1,default=str))
                assert blocked_status!=200,summary
                assert during>before and after>=during,summary
                assert summary['no_fabricated_exit_during_outage'] and summary['no_fabricated_exit_after_outage'],summary
                assert summary['ensure_after_outage'] and teardown.get('exit_confirmed') is True,summary
                print('DOCKER_OUTAGE_OK')
            except Exception:
                log.flush();log.seek(0);print(log.read().decode()[-6000:]);raise
            finally:
                if runner.poll() is None:
                    runner.send_signal(signal.SIGINT)
                    try:runner.wait(timeout=15)
                    except subprocess.TimeoutExpired:runner.kill();runner.wait()
                if relay and relay.poll() is None:stop_relay(relay)
                ids=subprocess.check_output(['docker','ps','-a','--filter',f'label=muteki.state_dir={root}','--format','{{.ID}}'],text=True).splitlines()
                if ids:subprocess.run(['docker','rm','-f',*ids],capture_output=True)
if __name__=='__main__':main()
