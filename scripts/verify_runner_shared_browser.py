#!/usr/bin/env python3
"""Real Chromium/agent-browser ownership inside a shared Runner container."""
import json
import argparse
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import uuid
from verify_runner_ipc import read


def main():
    parser=argparse.ArgumentParser();parser.add_argument("--automatic-limits",action="store_true");args=parser.parse_args()
    binary = str(Path('.cache/target/debug/muteki-server').resolve())
    with tempfile.TemporaryDirectory(prefix='runner-browser-scope-', dir='/tmp') as directory, tempfile.TemporaryFile() as log:
        root = Path(directory).resolve(); sock = root / 'runner/control.sock'
        env = dict(os.environ); env.pop('MUTEKI_RUNNER_SOCKET', None)
        runner = subprocess.Popen([binary, 'runner', '--state-dir', str(root)], env=env, stdout=log, stderr=log)
        def call(route, body=None):
            code, value = read(sock, route, body)
            assert code == 200, value
            return value
        def execute(handle, argv, worker='one', expected=0):
            result = call('/v1/workers/start', {'run_id': handle['run_id'], 'generation': 1,
                'command_id': uuid.uuid4().hex, 'spec': {'argv': argv, 'cwd': handle['container_workspace'],
                    'env': {'MUTEKI_BROWSER_RUN_ROOT': handle['container_workspace'], 'MUTEKI_AGENT_BROWSER_ENABLED': '1',
                            'MUTEKI_WORKER_ID': worker}, 'timeout_sec': 45}})
            after = 0; output = ''; deadline = time.monotonic() + 55
            while True:
                page = call('/v1/workers/' + result['worker_id'] + '/events?after=' + str(after)); after = page['after']
                for frame in page['frames']:
                    if frame['t'] in ('out', 'err'): output += frame.get('line', '') + '\n'
                    if frame['t'] == 'exit':
                        assert frame.get('rc', 0) == expected, (argv, frame, output)
                        return output
                assert time.monotonic() < deadline, (argv, output)
                time.sleep(.03)
        def browser(handle, *args, worker='one', expected=0):
            return execute(handle, ['python3', '/opt/muteki/browser_coord.py', *args], worker, expected)
        def ensure():
            run = 'browser-' + uuid.uuid4().hex[:10]
            settings={'worker_container_scope':'shared','worker_network':'bridge','_runner_browser_enabled':True}
            if not args.automatic_limits: settings.update({'worker_pids_limit':1024,'worker_memory':'2g','worker_cpus':2})
            handle = call('/v1/containers/ensure', {'run_id': run, 'owner_run_id': run, 'settings':settings})
            contract=call('/v1/runs/'+run)['run']
            assert contract['browser_reserved'] is True
            if args.automatic_limits:
                assert contract['resource_limit_sources']['worker_pids_limit']=='node_shared_default'
                assert handle['limits']['pids_limit']>=512
            again=call('/v1/containers/ensure', {'run_id':run,'owner_run_id':run,'settings':settings})
            assert again['container_id']==handle['container_id'] and again['limits']==handle['limits']
            handle['namespace'] = uuid.uuid4().hex[:12]
            config = json.dumps({'run_id': run, 'browser_namespace': handle['namespace']})
            execute(handle, ['python3', '-c', 'from pathlib import Path; p=Path(".muteki-browser");p.mkdir(exist_ok=True);(p/"run.json").write_text(' + repr(config) + ')'])
            return handle
        def processes(handle):
            raw = subprocess.check_output(['docker', 'exec', handle['container_id'], 'python3', '-c', '''
import json, pathlib
rows=[]
for path in pathlib.Path('/proc').glob('[0-9]*/cmdline'):
 try:
  cmd=path.read_bytes().replace(b'\\0',b' ').decode(errors='replace')
  if 'chromium' in cmd or 'agent-browser' in cmd: rows.append({'pid':int(path.parent.name),'cmd':cmd})
 except (FileNotFoundError,ProcessLookupError): pass
print(json.dumps(rows))
'''], text=True)
            return json.loads(raw)
        def browser_roots(handle):
            profile = handle['container_workspace'] + '/.muteki-browser-profile'
            return {p['pid'] for p in processes(handle) if '--user-data-dir=' + profile in p['cmd'] and '--type=' not in p['cmd']}
        try:
            deadline = time.monotonic() + 20
            while not sock.exists():
                if runner.poll() is not None or time.monotonic() > deadline:
                    log.seek(0); raise RuntimeError(log.read().decode())
                time.sleep(.05)
            a, b = ensure(), ensure()
            assert a['container_id'] == b['container_id']
            browser(a, 'open', 'data:text/html,<title>Run A</title><h1>A</h1>', worker='a-one')
            browser(a, 'open', 'data:text/html,<title>Run A second</title><h1>A2</h1>', worker='a-two')
            browser(b, 'open', 'data:text/html,<title>Run B</title><h1>B</h1>', worker='b-one')
            a_temp, b_temp = [execute(handle, ['python3', '-c', 'import os;print(os.environ["MUTEKI_RUN_TEMP_DIR"])']).strip() for handle in (a,b)]
            assert a_temp != b_temp and all(value.startswith('/tmp/muteki-run-') for value in (a_temp,b_temp))
            execute(b, ['python3', '-c', 'from pathlib import Path;Path(' + repr(b_temp + '/sentinel') + ').write_text("neighbor")'])
            execute(a, ['python3', '-c', 'from pathlib import Path;Path(' + repr(a_temp + '/foreign-link') + ').symlink_to(' + repr(b_temp) + ',target_is_directory=True)'])
            a_roots, b_roots = browser_roots(a), browser_roots(b)
            assert len(a_roots) == len(b_roots) == 1 and a_roots.isdisjoint(b_roots), (a_roots, b_roots)
            assert 'Run A' in browser(a, 'get', 'title', worker='a-one')
            assert 'Run A second' in browser(a, 'get', 'title', worker='a-two')
            assert 'Run B' in browser(b, 'get', 'title', worker='b-one')
            browser(a, 'close', worker='a-one', expected=2)
            assert browser_roots(a) == a_roots
            assert 'Run A second' in browser(a, 'get', 'title', worker='a-two')
            started = time.monotonic()
            call('/v1/containers/' + a['run_id'] + '/teardown', {'generation': 1})
            stop_seconds = time.monotonic() - started
            assert not browser_roots(a)
            assert subprocess.run(['docker', 'exec', b['container_id'], 'test', '-e', a_temp], capture_output=True).returncode == 1
            socket_check = subprocess.run(['docker', 'exec', b['container_id'], 'python3', '-c', 'import pathlib,stat,sys;paths=list(pathlib.Path(' + repr(b_temp) + ').rglob("*"));print([(str(p),p.lstat().st_mode) for p in paths]);sys.exit(not any(stat.S_ISSOCK(p.lstat().st_mode) for p in paths))'], capture_output=True, text=True)
            assert socket_check.returncode == 0, (socket_check.stdout, socket_check.stderr)
            assert browser_roots(b) == b_roots
            assert execute(b, ['python3', '-c', 'from pathlib import Path;print(Path(' + repr(b_temp + '/sentinel') + ').read_text())']).strip() == 'neighbor'
            assert 'Run B' in browser(b, 'get', 'title', worker='b-one')
            a_processes = [p for p in processes(a) if a['namespace'] in p['cmd'] or a['container_workspace'] in p['cmd']]
            assert not a_processes, a_processes
            call('/v1/containers/' + b['run_id'] + '/teardown', {'generation': 1})
            assert subprocess.run(['docker', 'inspect', b['container_id']], capture_output=True).returncode != 0
            print(json.dumps({'pool_limits': a['limits'], 'automatic_pool_limits': args.automatic_limits, 'real_chromium': True, 'same_run_workers_share_one_browser': True,
                'worker_tabs_remain_distinct': True, 'different_runs_use_distinct_browser_processes': True,
                'worker_cannot_close_run_browser': True, 'stop_a_preserves_b_browser_pid_and_tab': True,
                'a_browser_processes_removed': True, 'run_temp_and_socket_cleanup': True, 'cleanup_does_not_follow_neighbor_symlink': True, 'last_run_reclaims_container': True,
                'scope_stop_seconds': round(stop_seconds, 3)}, indent=2))
        except BaseException:
            ids = subprocess.check_output(['docker', 'ps', '-a', '--filter', f'label=muteki.state_dir={root}', '--format', '{{.ID}}'], text=True).splitlines()
            for identity in ids:
                result = subprocess.run(['docker', 'logs', '--tail', '40', identity], capture_output=True, text=True)
                print(result.stdout + result.stderr, file=sys.stderr)
            raise
        finally:
            if runner.poll() is None:
                runner.send_signal(signal.SIGINT)
                try: runner.wait(timeout=10)
                except subprocess.TimeoutExpired: runner.kill(); runner.wait()
            ids = subprocess.check_output(['docker', 'ps', '-a', '--filter', f'label=muteki.state_dir={root}', '--format', '{{.ID}}'], text=True).splitlines()
            if ids: subprocess.run(['docker', 'rm', '-f', *ids], check=True, capture_output=True)


if __name__ == '__main__':
    main()
