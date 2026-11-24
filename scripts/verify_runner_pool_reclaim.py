#!/usr/bin/env python3
"""Real Docker acceptance: final shared owner reclaim and independent teardown progress."""
import concurrent.futures
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import tempfile
import time
import uuid
from verify_runner_ipc import read


def main():
    binary = str(Path('.cache/target/debug/muteki-server').resolve())
    docker = shutil.which('docker')
    assert docker
    with tempfile.TemporaryDirectory(prefix='runner-reclaim-', dir='/tmp') as directory:
        root = Path(directory).resolve()
        shim = root / 'bin'; shim.mkdir()
        delayed = root / 'delayed-id'; marker = root / 'delay-started'
        wrapper = shim / 'docker'
        wrapper.write_text('#!/usr/bin/env python3\nimport os,sys,time\nfrom pathlib import Path\n'
            + f'target=Path({str(delayed)!r})\n'
            + f'if len(sys.argv)>3 and sys.argv[1]=="rm" and target.exists() and sys.argv[-1]==target.read_text():\n    Path({str(marker)!r}).touch()\n    time.sleep(6)\n'
            + f'os.execv({docker!r},[{docker!r},*sys.argv[1:]])\n')
        wrapper.chmod(0o755)
        env = dict(os.environ, PATH=str(shim) + os.pathsep + os.environ['PATH'])
        env.pop('MUTEKI_RUNNER_SOCKET', None)
        socket = root / 'runner/control.sock'
        prefix = 'reclaim-' + uuid.uuid4().hex[:8]
        handles = []
        def ensure(run, scope):
            status, handle = read(socket, '/v1/containers/ensure', {'run_id': run, 'owner_run_id': run,
                'settings': {'worker_container_scope': scope, 'worker_network': 'bridge', 'worker_privilege': 'default'}})
            assert status == 200, handle
            handles.append(handle)
            return handle
        def stop(handle):
            status, result = read(socket, '/v1/containers/' + handle['run_id'] + '/teardown', {'generation': 1})
            assert status == 200, result
        def exists(handle):
            return subprocess.run([docker, 'inspect', handle['container_id']], capture_output=True).returncode == 0
        with tempfile.TemporaryFile() as log:
            runner = subprocess.Popen([binary, 'runner', '--state-dir', str(root)], env=env, stdout=log, stderr=log)
            try:
                deadline = time.monotonic() + 20
                while not socket.exists():
                    if runner.poll() is not None or time.monotonic() > deadline:
                        log.seek(0); raise RuntimeError(log.read().decode())
                    time.sleep(0.05)
                a = ensure(prefix + '-a', 'shared'); b = ensure(prefix + '-b', 'shared')
                assert a['container_id'] == b['container_id']
                stop(a); assert exists(b), 'Non-final owner stop removed shared container'
                stop(b); assert not exists(b), 'Final owner left its physical container behind'
                races = []
                with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
                    for index in range(3):
                        before = ensure(prefix + f'-race-{index}-old', 'shared')
                        stopping = pool.submit(stop, before)
                        creating = pool.submit(ensure, prefix + f'-race-{index}-new', 'shared')
                        stopping.result(timeout=25); after = creating.result(timeout=25)
                        assert exists(after), 'New owner was removed by concurrent final-owner cleanup'
                        status, health = read(socket, '/v1/containers/' + after['run_id'] + '/control', {'generation': 1, 'op': 'Health'})
                        assert status == 200 and health['ok'], health
                        races.append('reused' if before['container_id'] == after['container_id'] else 'recreated')
                        stop(after); assert not exists(after)
                    slow = ensure(prefix + '-slow', 'run'); fast = ensure(prefix + '-fast', 'run')
                    delayed.write_text(slow['container_id'])
                    stopping = pool.submit(stop, slow)
                    deadline = time.monotonic() + 10
                    while not marker.exists():
                        assert time.monotonic() < deadline
                        time.sleep(0.02)
                    started = time.monotonic(); stop(fast); fast_seconds = time.monotonic() - started
                    assert fast_seconds < 4, f'Unrelated teardown waited {fast_seconds:.2f}s behind delayed Docker removal'
                    assert not stopping.done(), 'Slow-removal injection did not overlap independent teardown'
                    stopping.result(timeout=20)
                    delayed.unlink()
                    assert not exists(slow) and not exists(fast)
                victim = ensure(prefix + '-identity', 'run')
                subprocess.run([docker, 'rename', victim['container_id'], victim['name'] + '-old'], check=True, capture_output=True)
                replacement = subprocess.check_output([docker, 'create', '--name', victim['name'], '--label',
                    f'muteki.state_dir={root}', '--entrypoint', '/bin/true', victim['image']], text=True).strip()
                stop(victim)
                assert not exists(victim)
                assert subprocess.run([docker, 'inspect', replacement], capture_output=True).returncode == 0
                subprocess.run([docker, 'rm', replacement], check=True, capture_output=True)
                remaining = subprocess.check_output([docker, 'ps', '-a', '--filter', f'label=muteki.state_dir={root}', '--format', '{{.ID}}'], text=True).strip()
                assert not remaining, 'Acceptance left an unreclaimed runtime'
                print(json.dumps({'shared_nonfinal_owner_preserved': True, 'last_owner_container_removed': True,
                    'concurrent_register_and_release_rounds': races, 'injected_remove_delay_seconds': 6,
                    'unrelated_teardown_seconds': round(fast_seconds, 3), 'no_remaining_test_containers': True, 'reused_name_replacement_preserved': True}, indent=2))
            finally:
                delayed.unlink(missing_ok=True)
                if runner.poll() is None:
                    runner.send_signal(signal.SIGINT)
                    runner.wait(timeout=10)
                names = subprocess.check_output([docker, 'ps', '-a', '--filter', f'label=muteki.state_dir={root}', '--format', '{{.ID}}'], text=True).splitlines()
                if names:
                    subprocess.run([docker, 'rm', '-f', *names], check=True, capture_output=True)


if __name__ == '__main__':
    main()
