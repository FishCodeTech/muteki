#!/usr/bin/env python3
"""Real shared Docker Workers: oversized output, burst logs and independent controls."""
import json, os, signal, subprocess, tempfile, time, uuid
from pathlib import Path
from verify_runner_ipc import read


def main():
    binary = str(Path('.cache/target/debug/muteki-server').resolve())
    with tempfile.TemporaryDirectory(prefix='runner-output-', dir='/tmp') as directory:
        root = Path(directory)
        with (root/'service.log').open('w+') as log:
            process = subprocess.Popen([binary, 'runner', '--state-dir', directory], stdout=log, stderr=log)
            socket = root/'runner/control.sock'
            handles = []
            latencies = []
            peak_rss = 0
            try:
                deadline = time.monotonic()+20
                while not socket.exists():
                    assert process.poll() is None and time.monotonic()<deadline
                    time.sleep(.05)
                prefix = 'output379-'+uuid.uuid4().hex[:10]
                for suffix in ('a','b'):
                    run = prefix+'-'+suffix
                    code, handle = read(socket, '/v1/containers/ensure', dict(run_id=run, owner_run_id=run,
                        settings=dict(worker_container_scope='shared', worker_network='bridge', worker_privilege='default')))
                    assert code == 200, handle
                    handles.append(handle)
                a,b = handles
                assert a['name'] == b['name']
                observation = json.loads(subprocess.check_output(['docker','inspect',a['name']]))[0]
                identity = observation['Id']
                def start(handle, source):
                    command = dict(command_id=uuid.uuid4().hex, run_id=handle['run_id'], generation=handle['generation'],
                        spec=dict(argv=['python3','-c',source], cwd=handle['container_workspace'], env={},
                            timeout_sec=90, output_limit_bytes=64*1024*1024, disk_limit_bytes=64*1024*1024))
                    code, started = read(socket, '/v1/workers/start', command)
                    assert code == 200, started
                    return started
                survivor = start(b, "import os,time\nprint('survivor-ready',flush=True)\nwhile not os.path.exists('release-b'): time.sleep(.1)\nprint('survivor-finished',flush=True)")
                def drain(worker, timeout=45):
                    nonlocal peak_rss
                    cursor = 0; count = 0; errors = []; tail = []; deadline = time.monotonic()+timeout
                    while time.monotonic()<deadline:
                        began=time.monotonic()
                        code, health=read(socket, f"/v1/containers/{b['run_id']}/control", {'op':'Health','generation':1})
                        latencies.append(time.monotonic()-began)
                        assert code == 200, health
                        peak_rss=max(peak_rss,int(subprocess.check_output(['ps','-o','rss=','-p',str(process.pid)]).strip())*1024)
                        code, page = read(socket, f"/v1/workers/{worker['worker_id']}/events?after={cursor}")
                        assert code == 200, page
                        cursor=page['after']
                        for frame in page['frames']:
                            count+=1
                            if frame['t'] == 'err': errors.append(frame.get('line',''))
                            if frame['t'] == 'out': tail=(tail+[frame.get('line','')])[-2:]
                            if frame['t'] in ('exit','transport_error'):
                                return dict(terminal=frame, frames=count, errors=errors, tail=tail)
                        time.sleep(.01)
                    raise RuntimeError('Worker did not produce a terminal outcome')
                oversized = drain(start(a, "import sys,time\nsys.stdout.write('X'*(600*1024)+'\\n');sys.stdout.flush();time.sleep(30)"))
                assert oversized['terminal']['t']=='exit' and oversized['terminal']['output_limit'] is True, oversized
                assert any('frame budget' in e for e in oversized['errors']), oversized
                burst_worker = start(a, "import sys\nfor i in range(30000): sys.stdout.write('B'*1023+'\\n')\nsys.stdout.flush()")
                burst = drain(burst_worker)
                if burst['terminal']['t']=='exit': assert burst['terminal']['rc']==0,burst['terminal']
                else:
                    assert 'buffer' in burst['terminal'].get('error',''),burst['terminal']
                    deadline=time.monotonic()+20
                    while True:
                        code,receipt=read(socket,f"/v1/commands/{burst_worker['command_id']}")
                        assert code==200,receipt
                        if receipt.get('execution_stopped') is True: break
                        assert time.monotonic()<deadline,receipt
                        time.sleep(.05)
                    assert read(socket,f"/v1/containers/{a['run_id']}/control",{'generation':1,'op':'Health'})[0]==409
                    code,replacement=read(socket,'/v1/containers/ensure',dict(run_id=a['run_id'],owner_run_id=a['run_id'],generation=2,
                        settings=dict(worker_container_scope='shared',worker_network='bridge',worker_privilege='default')))
                    assert code==200,replacement
                    a.update(replacement)
                    renewed=start(a,"import time; time.sleep(1); print('new-generation-survived')")
                    assert read(socket,f"/v1/containers/{a['run_id']}/teardown",{'generation':1})[0]==409
                    assert drain(renewed)['terminal']['rc']==0
                code, stopped = read(socket, f"/v1/containers/{a['run_id']}/teardown", {'generation':a['generation']})
                assert code==200,stopped
                (Path(b['host_workspace'])/'release-b').write_text('go')
                survived=drain(survivor)
                assert survived['terminal']['t']=='exit' and survived['terminal']['rc']==0, survived
                assert 'survivor-finished' in survived['tail'],survived
                assert max(latencies)<5,latencies
                print(json.dumps(dict(image=os.environ.get('MUTEKI_WORKER_IMAGE'), container_id=identity, image_id=observation['Image'],
                    oversized_worker_stopped=True, oversized_exit=oversized['terminal'],
                    burst_terminal=burst['terminal'], burst_frames=burst['frames'],
                    unrelated_shared_worker_completed=True, failed_run_fenced=(burst['terminal']['t']=='transport_error'), control_max_seconds=max(latencies),
                    runner_peak_rss_bytes=peak_rss),indent=2))
            except BaseException:
                log.flush();log.seek(0);print(log.read()[-12000:])
                raise
            finally:
                for handle in reversed(handles):
                    try: read(socket,f"/v1/containers/{handle['run_id']}/teardown",{'generation':handle['generation']})
                    except Exception: pass
                for name in {h['name'] for h in handles}:
                    subprocess.run(['docker','rm','-f',name],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
                if process.poll() is None:
                    process.send_signal(signal.SIGINT)
                    try: process.wait(timeout=10)
                    except subprocess.TimeoutExpired: process.kill();process.wait()

if __name__=='__main__': main()
