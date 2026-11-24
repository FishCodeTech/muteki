#!/usr/bin/env python3
"""Verify real Docker measurements and single-counted shared-runtime attribution."""
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


def main():
    binary = str(Path('.cache/target/debug/muteki-server').resolve())
    with tempfile.TemporaryDirectory(prefix='runner-usage-', dir='/tmp') as directory, tempfile.TemporaryFile() as log:
        root = Path(directory).resolve(); sock = root / 'runner/control.sock'
        env = dict(os.environ); env.pop('MUTEKI_RUNNER_SOCKET', None)
        runner = subprocess.Popen([binary, 'runner', '--state-dir', str(root)], env=env, stdout=log, stderr=log)
        def call(route, body=None):
            code, value = read(sock, route, body)
            assert code == 200, value
            return value
        handles = []
        try:
            deadline = time.monotonic() + 20
            while not sock.exists():
                if runner.poll() is not None or time.monotonic() > deadline:
                    log.seek(0); raise RuntimeError(log.read().decode())
                time.sleep(.05)
            for scope in ('shared', 'shared', 'run'):
                run = 'usage-' + uuid.uuid4().hex[:10]
                handle = call('/v1/containers/ensure', {'run_id': run, 'owner_run_id': run,
                    'settings': {'worker_container_scope': scope, 'worker_network': 'bridge',
                                 'worker_memory': '512m', 'worker_cpus': 1, 'worker_pids_limit': 128}})
                handles.append(handle)
                call('/v1/workers/start', {'run_id': run, 'generation': 1, 'command_id': uuid.uuid4().hex,
                    'spec': {'argv': ['python3', '-c', 'import time;from pathlib import Path;data=bytearray(32*1024*1024);Path("allocated").write_text("ready");time.sleep(90)'],
                             'cwd': handle['container_workspace'], 'env': {}, 'timeout_sec': 100}})
            deadline = time.monotonic() + 10
            while not all((Path(h['host_workspace'])/'allocated').exists() for h in handles):
                assert time.monotonic() < deadline
                time.sleep(.05)
            with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
                sampled = pool.submit(call, '/v1/resources')
                time.sleep(.05)
                started = time.monotonic()
                health = call('/v1/containers/' + handles[0]['run_id'] + '/control', {'generation': 1, 'op': 'Health'})
                health_seconds = time.monotonic() - started
                assert health['ok'] and health_seconds < 1, health
                value = sampled.result(timeout=20)
            assert value['usage']['available'], value
            pressure = value['memory_pressure']
            assert pressure['available'] and 0 <= pressure['host_available_bytes'] <= pressure['host_total_bytes'], pressure
            assert value['recommendation']['source'] == 'docker_host_mem_available_with_worker_estimate', value
            assert health['memory_sampled'] and health['host_memory_total_bytes'] == pressure['host_total_bytes'], health
            assert 'containers' not in value['usage']
            physical = value['physical_runtimes']; assert len(physical) == 2, physical
            shared = next(row for row in physical if row['attribution'] == 'shared_container')
            assert {r['run_id'] for r in shared['runs']} == {h['run_id'] for h in handles[:2]}
            for row in physical:
                assert row['measurement_available'], row
                sample = row['measurement']
                assert sample['memory_working_set_bytes'] > 20*1024*1024, row
                assert sample['memory_limit_bytes'] == 512*1024*1024, row
                assert sample['processes_and_threads'] > 0, row
                assert row['configured_limits']['pids_limit'] == 128, row
                assert row['configured_limits']['memory_bytes'] == sample['memory_limit_bytes'], row
            measured = sum(row['measurement']['memory_working_set_bytes'] for row in physical)
            assert value['usage']['measured_container_memory_bytes'] >= measured
            call('/v1/containers/' + handles[0]['run_id'] + '/teardown', {'generation': 1})
            after = call('/v1/resources')
            shared_after = next(row for row in after['physical_runtimes'] if row['attribution'] == 'shared_container')
            assert [r['run_id'] for r in shared_after['runs']] == [handles[1]['run_id']]
            for handle in handles[1:]: call('/v1/containers/' + handle['run_id'] + '/teardown', {'generation': 1})
            assert call('/v1/resources')['physical_runtimes'] == []
            print(json.dumps({'real_docker_usage': True, 'actual_vm_available_memory_used_for_recommendation': True, 'three_runs_counted_as_two_physical_containers': True,
                'shared_totals_not_divided_or_duplicated': True, 'configured_limits_match_docker': True,
                'measured_owned_container_bytes': measured, 'unrelated_container_identities_not_exposed': True,
                'health_during_measurement_seconds': round(health_seconds, 3),
                'attribution_updates_without_waiting_for_cached_measurement': True}, indent=2))
        finally:
            if runner.poll() is None:
                runner.send_signal(signal.SIGINT)
                try: runner.wait(timeout=10)
                except subprocess.TimeoutExpired: runner.kill(); runner.wait()
            ids = subprocess.check_output(['docker', 'ps', '-a', '--filter', f'label=muteki.state_dir={root}', '--format', '{{.ID}}'], text=True).splitlines()
            if ids: subprocess.run(['docker', 'rm', '-f', *ids], check=True, capture_output=True)


if __name__ == '__main__':
    main()
