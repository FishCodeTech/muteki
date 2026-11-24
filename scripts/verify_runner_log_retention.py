#!/usr/bin/env python3
"""Real Runner + Docker Workers (no model): node-wide event-log budget, purge of finished logs, retention."""
import json, os, signal, subprocess, sys, tempfile, time, uuid
from pathlib import Path
from verify_runner_ipc import read

BUDGET = 300000


def start_runner(binary, directory, log, retention):
    env = dict(os.environ, MUTEKI_RUNNER_NODE_LOG_BYTES=str(BUDGET), MUTEKI_RUNNER_LOG_RETENTION_SECS=str(retention), MUTEKI_RUNNER_LOG_GC_SECS='2')
    process = subprocess.Popen([binary, 'runner', '--state-dir', directory], stdout=log, stderr=log, env=env)
    socket = Path(directory) / 'runner/control.sock'
    deadline = time.monotonic() + 30
    while not socket.exists():
        assert process.poll() is None and time.monotonic() < deadline
        time.sleep(.05)
    return process, socket


def main():
    binary = str(Path('.cache/target/debug/muteki-server').resolve())
    with tempfile.TemporaryDirectory(prefix='runner-logs-', dir='/tmp') as directory:
        root = Path(directory)
        with (root / 'service.log').open('w+') as log:
            process, socket = start_runner(binary, directory, log, 3600)
            handle = None
            try:
                run = 'logs379-' + uuid.uuid4().hex[:10]
                code, handle = read(socket, '/v1/containers/ensure', dict(run_id=run, owner_run_id=run,
                    settings=dict(worker_container_scope='run', worker_network='bridge', worker_privilege='default')))
                assert code == 200, handle

                def start(kb, tag):
                    source = f"import sys\nfor i in range({kb}): sys.stdout.write('L'*1000+'\\n')\nsys.stdout.flush()"
                    code, started = read(socket, '/v1/workers/start', dict(command_id=uuid.uuid4().hex, run_id=run, generation=handle['generation'],
                        spec=dict(argv=['python3', '-c', source], cwd=handle['container_workspace'], env={}, tag=tag, timeout_sec=60,
                            output_limit_bytes=64 * 1024 * 1024, disk_limit_bytes=64 * 1024 * 1024)))
                    assert code == 200, started
                    return started

                def drain(worker):
                    cursor = 0; frames = 0; deadline = time.monotonic() + 60
                    while time.monotonic() < deadline:
                        code, page = read(socket, f"/v1/workers/{worker['worker_id']}/events?after={cursor}")
                        assert code == 200, page
                        cursor = page['after']
                        for frame in page['frames']:
                            frames += 1
                            if frame['t'] in ('exit', 'transport_error'):
                                return frame, frames
                        time.sleep(.05)
                    raise RuntimeError('no terminal frame')

                def log_files():
                    return {p.name: p.stat().st_size for p in (root / 'runner/events').glob('*.jsonl')}

                first = start(100, 'log-a'); terminal_a, _ = drain(first); assert terminal_a['t'] == 'exit' and terminal_a['rc'] == 0, terminal_a
                second = start(100, 'log-b'); terminal_b, _ = drain(second); assert terminal_b['t'] == 'exit', terminal_b
                before = log_files(); assert sum(before.values()) > BUDGET * .6, before
                third = start(100, 'log-c'); terminal_c, _ = drain(third)
                assert terminal_c['t'] == 'exit' and terminal_c['rc'] == 0, terminal_c
                after = log_files()
                oldest = f"{first['worker_id']}.jsonl"
                assert oldest not in after, after
                code, receipt = read(socket, f"/v1/commands/{first['command_id']}")
                assert code == 200 and receipt.get('events_purged_reason') == 'node_log_budget', receipt
                code, gone = read(socket, f"/v1/workers/{first['worker_id']}/events?after=0")
                assert code != 200, gone

                big = start(400, 'log-d'); terminal_d, _ = drain(big)
                assert terminal_d['t'] == 'transport_error' and 'budget' in terminal_d['error'], terminal_d
                deadline = time.monotonic() + 30
                while True:
                    code, receipt_d = read(socket, f"/v1/commands/{big['command_id']}")
                    if code == 200 and receipt_d.get('execution_stopped') is True: break
                    assert time.monotonic() < deadline, receipt_d
                    time.sleep(.1)
                node_total = sum(log_files().values()); assert node_total <= BUDGET, node_total

                process.send_signal(signal.SIGINT); process.wait(timeout=15)
                process, socket = start_runner(binary, directory, log, 1)
                deadline = time.monotonic() + 30
                while log_files():
                    assert time.monotonic() < deadline, log_files()
                    time.sleep(.5)
                purged = {}
                for name, command in (('a', first), ('b', second), ('c', third), ('d', big)):
                    code, row = read(socket, f"/v1/commands/{command['command_id']}")
                    purged[name] = row.get('events_purged_reason') if code == 200 else code
                assert purged['a'] == purged['b'] == purged['c'] == 'node_log_budget' and purged['d'] == 'retention', purged
                print(json.dumps(dict(node_budget_bytes=BUDGET, logs_before_third_worker=before, logs_after_third_worker=after,
                    oldest_purge_reason=receipt['events_purged_reason'], oversized_worker=terminal_d, node_total_after_refusal=node_total,
                    purge_reasons_after_restart_with_retention=purged), indent=1))
            except BaseException:
                log.flush(); log.seek(0); print(log.read()[-8000:])
                raise
            finally:
                if handle:
                    try: read(socket, f"/v1/containers/{handle['run_id']}/teardown", {'generation': handle['generation']})
                    except Exception: pass
                    subprocess.run(['docker', 'rm', '-f', handle['name']], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                if process.poll() is None:
                    process.send_signal(signal.SIGINT)
                    try: process.wait(timeout=10)
                    except subprocess.TimeoutExpired: process.kill(); process.wait()


if __name__ == '__main__':
    main()
