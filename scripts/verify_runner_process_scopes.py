#!/usr/bin/env python3
"""Actual Linux Run subreapers inside one Docker container; no model API calls."""
import concurrent.futures
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import time
import uuid
from verify_runner_ipc import read

DAEMON = r'''
import ctypes,json,os,time
if os.fork(): os._exit(0)
os.setsid()
if os.fork(): os._exit(0)
null=os.open('/dev/null',os.O_RDWR)
for fd in (0,1,2): os.dup2(null,fd)
# A nested subreaper must not hide descendants from the enclosing Run owner.
assert ctypes.CDLL(None).prctl(36,1,0,0,0)==0
child=os.fork()
if child==0:
    os.setsid()
    with open('nested.pid','w') as file: json.dump({'pid':os.getpid()},file)
    while True:
        with open('nested.counter','a') as file: file.write('tick\n')
        time.sleep(.05)
time.sleep(.1)
with open('daemon.pid','w') as file: json.dump({'pid':os.getpid(),'scope_pid':os.getppid()},file)
while True:
    with open('daemon.counter','a') as file: file.write('tick\n')
    time.sleep(.05)
'''


def main():
    binary = str(Path('.cache/target/debug/muteki-server').resolve())
    with tempfile.TemporaryDirectory(prefix='runner-subscope-', dir='/tmp') as directory:
        root = Path(directory).resolve(); socket = root / 'runner/control.sock'
        prefix = 'subscope-' + uuid.uuid4().hex[:8]
        env = dict(os.environ); env.pop('MUTEKI_RUNNER_SOCKET', None)
        handles = []
        def call(path, body=None):
            status, value = read(socket, path, body)
            assert status == 200, value
            return value
        def ensure(suffix):
            run = prefix + '-' + suffix
            handle = call('/v1/containers/ensure', {'run_id': run, 'owner_run_id': run,
                'settings': {'worker_container_scope': 'shared', 'worker_network': 'bridge'}})
            handles.append(handle)
            return handle
        def start(handle, argv, stream=False):
            command = uuid.uuid4().hex
            result = call('/v1/workers/start', {'run_id': handle['run_id'], 'generation': 1, 'command_id': command,
                'spec': {'argv': argv, 'cwd': handle['container_workspace'], 'env': {}, 'timeout_sec': 120,
                         'stdin_mode': 'stream' if stream else None}})
            return command, result['worker_id']
        def exit_code(worker):
            deadline = time.monotonic() + 15
            while True:
                page = call('/v1/workers/' + worker + '/events?after=0')
                exits = [frame for frame in page['frames'] if frame['t'] == 'exit']
                if exits:
                    return exits[0].get('rc', 0)
                assert time.monotonic() < deadline, page
                time.sleep(.05)
        def stop(handle):
            return call('/v1/containers/' + handle['run_id'] + '/teardown', {'generation': 1})
        def daemon(handle):
            _, worker = start(handle, ['python3', '-c', DAEMON])
            assert exit_code(worker) == 0
            workspace = Path(handle['host_workspace']); deadline = time.monotonic() + 10
            while not (workspace / 'daemon.pid').exists() or not (workspace / 'nested.counter').exists():
                assert time.monotonic() < deadline
                time.sleep(.05)
            return json.loads((workspace / 'daemon.pid').read_text())
        def counter(handle, name):
            return (Path(handle['host_workspace']) / name).read_bytes()
        def health(handle):
            return call('/v1/containers/' + handle['run_id'] + '/control', {'generation': 1, 'op': 'Health'})
        with tempfile.TemporaryFile() as log:
            runner = subprocess.Popen([binary, 'runner', '--state-dir', str(root)], env=env, stdout=log, stderr=log)
            try:
                deadline = time.monotonic() + 20
                while not socket.exists():
                    if runner.poll() is not None or time.monotonic() > deadline:
                        log.seek(0); raise RuntimeError(log.read().decode())
                    time.sleep(.05)
                a, b = ensure('a'), ensure('b')
                assert a['container_id'] == b['container_id']
                assert health(a)['scoped_processes'] is True
                owner_a, owner_b = daemon(a), daemon(b)
                assert owner_a['scope_pid'] != owner_b['scope_pid']
                assert owner_a['scope_pid'] > 1 and owner_b['scope_pid'] > 1
                started = time.monotonic(); stop(a); stop_seconds = time.monotonic() - started
                a_before = [counter(a, name) for name in ('daemon.counter', 'nested.counter')]
                b_before = [counter(b, name) for name in ('daemon.counter', 'nested.counter')]
                time.sleep(.4)
                assert [counter(a, name) for name in ('daemon.counter', 'nested.counter')] == a_before
                assert all(len(counter(b, name)) > len(before) for name, before in zip(('daemon.counter', 'nested.counter'), b_before))
                assert health(b)['ok']
                c = ensure('blocked-stdin')
                _, waiting = start(c, ['/bin/sleep', '120'], stream=True)
                with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
                    blocked = pool.submit(read, socket, '/v1/containers/' + c['run_id'] + '/control',
                        {'generation': 1, 'op': 'WriteStdin', 'worker_id': waiting, 'data': 'x' * (512 * 1024)})
                    time.sleep(.2)
                    started = time.monotonic(); assert health(b)['ok']; unrelated_control = time.monotonic() - started
                    assert unrelated_control < 2
                    _, quick = start(b, ['/bin/true']); assert exit_code(quick) == 0
                    started = time.monotonic(); stop(c); blocked_stop = time.monotonic() - started
                    assert blocked_stop < 3
                    blocked.result(timeout=10)
                d = ensure('scope-loss'); lost = daemon(d)
                pending_command, pending_worker = start(b, ['/bin/sleep', '120'])
                subprocess.run(['docker', 'exec', d['container_id'], '/bin/kill', '-KILL', str(lost['scope_pid'])], check=True, capture_output=True)
                deadline = time.monotonic() + 15
                while True:
                    inspect = subprocess.run(['docker', 'inspect', d['container_id']], capture_output=True, text=True)
                    assert inspect.returncode == 0
                    state = json.loads(inspect.stdout)[0]['State']
                    if not state['Running']:
                        assert state['ExitCode'] == 76, state
                        break
                    assert time.monotonic() < deadline, state
                    time.sleep(.1)
                for handle in (b, d):
                    deadline = time.monotonic() + 10
                    while call('/v1/runs/' + handle['run_id'])['run']['state'] != 'stopped':
                        assert time.monotonic() < deadline
                        time.sleep(.1)
                    recovery = call('/v1/runs/' + handle['run_id'])['run']['recovery']
                    assert recovery['cause'] == 'scope_supervisor_failure'
                    assert {entry['run_id'] for entry in recovery['affected_executions']} == {b['run_id'], d['run_id']}
                receipt = call('/v1/commands/' + pending_command)
                assert receipt.get('execution_stopped') is True and 'exit' not in receipt, receipt
                before = [counter(handle, 'daemon.counter') for handle in (b, d)]
                time.sleep(.3)
                assert [counter(handle, 'daemon.counter') for handle in (b, d)] == before
                print(json.dumps({'double_fork_and_setsid_owned_by_distinct_run_subreapers': True,
                    'nested_subreaper_descendants_stopped': True, 'shared_neighbor_daemons_survive_normal_stop': True,
                    'scope_stop_seconds': round(stop_seconds, 3), 'unrelated_control_during_blocked_stdin_seconds': round(unrelated_control, 3),
                    'blocked_stdin_scope_stop_seconds': round(blocked_stop, 3), 'unexpected_scope_loss_exits_pool_76': True,
                    'all_pool_runs_reconciled_stopped': True, 'no_fabricated_worker_exit_after_pool_loss': True}, indent=2))
            finally:
                if runner.poll() is None:
                    runner.send_signal(signal.SIGINT)
                    try: runner.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        runner.kill(); runner.wait()
                ids = subprocess.check_output(['docker', 'ps', '-a', '--filter', f'label=muteki.state_dir={root}', '--format', '{{.ID}}'], text=True).splitlines()
                if ids:
                    subprocess.run(['docker', 'rm', '-f', *ids], check=True, capture_output=True)


if __name__ == '__main__':
    main()
