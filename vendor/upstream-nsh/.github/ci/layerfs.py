"""Prepare and validate NSH's local Linux LayerFS workspaces; see docs/dev/layerfs.md."""

import argparse
import io
import json
import os
import pathlib
import platform
import shutil
import subprocess
import sys
import tarfile
import uuid

SOURCE = pathlib.Path('.nsh/layerfs/source')
REVISION = '2aaf2552ce21154928b9f9c83582e41a637f5910'
STATE = pathlib.Path('.nsh/layerfs')
ROOT = pathlib.Path(__file__).resolve().parents[2]


def run(*args, capture=False):
    return subprocess.run(args, check=True, text=True, capture_output=capture).stdout


def docker_socket(explicit=None):
    if explicit:
        return pathlib.Path(explicit).resolve()
    endpoint = os.environ.get('DOCKER_HOST') if not os.environ.get('DOCKER_CONTEXT') else None
    if not endpoint:
        endpoint = run('docker', 'context', 'inspect', '--format',
                       '{{.Endpoints.docker.Host}}', capture=True).strip()
    if not endpoint.startswith('unix://'):
        raise RuntimeError('LayerFS requires a local Docker Unix socket; use --docker-socket PATH')
    return pathlib.Path(endpoint.removeprefix('unix://')).resolve()


def target_dir():
    return pathlib.Path(os.environ.get('CARGO_TARGET_DIR', 'target')).resolve()


def check_docker():
    api = run('docker', 'version', '--format', '{{.Server.APIVersion}}', capture=True).strip()
    if tuple(map(int, api.split('.'))) < (1, 54):
        raise RuntimeError(f'LayerFS 1.0.0 requires Docker API 1.54+, found {api}')


def prepare():
    check_docker()
    if platform.system() == 'Darwin':
        prepare_in_docker()
    elif platform.system() == 'Linux':
        build_tools()
        run('docker', 'build', '--tag', 'nsh-layerfs:1.0.0', '.github/ci/layerfs')
    else:
        raise RuntimeError('LayerFS preparation requires Linux or macOS with a local Linux Docker engine')
    image = run('docker', 'image', 'inspect', '--format', '{{.Id}}',
                'nsh-layerfs:1.0.0', capture=True).strip()
    settings = {'revision': REVISION, 'image': image,
                'daemon': str(target_dir() / 'release/layerfs-daemon')}
    (STATE / 'deployment.json').write_text(json.dumps(settings, indent=2))


def build_tools():
    STATE.mkdir(parents=True, exist_ok=True)
    run('rustup', 'toolchain', 'install', '1.85.1', '--profile', 'minimal')
    if not SOURCE.exists():
        run('git', 'clone', '--filter=blob:none', '--no-checkout', '--depth', '1', '--branch', 'v1.0.0',
            'https://github.com/Ephemeral-AI-Lab/layerfs.git', str(SOURCE))
        run('git', '-C', str(SOURCE), 'sparse-checkout', 'set', '--no-cone',
            '/.cargo/', '/core/Cargo.toml', '/core/Cargo.lock',
            '/core/crates/', '/core/vendor/', '/sdk/js/')
        run('git', '-C', str(SOURCE), 'checkout', '--detach', REVISION)
    revision = run('git', '-C', str(SOURCE), 'rev-parse', 'HEAD', capture=True).strip()
    if revision != REVISION:
        raise RuntimeError(f'LayerFS source must be {REVISION}, found {revision}')
    run('git', '-C', str(SOURCE), 'diff', '--exit-code', 'HEAD', '--', '.cargo', 'core', 'sdk/js')
    run('cargo', '+1.85.1', '--config', str(SOURCE / '.cargo/config.toml'),
        'build', '--manifest-path', str(SOURCE / 'core/Cargo.toml'),
        '--target-dir', str(target_dir()), '--locked', '--release',
        '-p', 'layerfs-node', '--lib', '-p', 'layerfs-daemon', '--bins')
    shutil.copyfile(target_dir() / 'release/liblayerfs_node.so', SOURCE / 'sdk/js/native/layerfs.node')


def prepare_in_docker():
    run('docker', 'build', '--tag', 'nsh-layerfs:1.0.0', '.github/ci/layerfs')
    run('docker', 'build', '--file', '.github/ci/layerfs/prepare.Dockerfile',
        '--tag', 'nsh-layerfs-build:1.0.0', '.github/ci/layerfs')
    target = target_dir()
    registry = pathlib.Path(os.environ.get('CARGO_HOME', pathlib.Path.home() / '.cargo')) / 'registry'
    target.mkdir(parents=True, exist_ok=True)
    registry.mkdir(parents=True, exist_ok=True)
    mounts = [
        (ROOT, ROOT), (target, target),
        (registry.resolve(), pathlib.Path('/usr/local/cargo/registry')),
    ]
    command = ['docker', 'run', '--rm', '--workdir', str(ROOT)]
    for source, destination in dict(mounts).items():
        command.extend(['--mount', f'type=bind,source={source},target={destination}'])
    if 'CARGO_TARGET_DIR' in os.environ:
        command.extend(['--env', 'CARGO_TARGET_DIR'])
    run(*command, 'nsh-layerfs-build:1.0.0', 'python3', '-c',
        "import runpy; runpy.run_path('.github/ci/layerfs.py')['build_tools']()")


def deployment():
    path = STATE / 'deployment.json'
    if not path.is_file():
        raise RuntimeError('LayerFS is not prepared; run ./scripts/layerfs prepare')
    settings = json.loads(path.read_text())
    daemon = pathlib.Path(settings.get('daemon', target_dir() / 'release/layerfs-daemon'))
    if not daemon.is_file() or not (SOURCE / 'sdk/js/native/layerfs.node').is_file():
        raise RuntimeError('LayerFS build outputs are missing; run ./scripts/layerfs prepare')
    run('docker', 'image', 'inspect', settings['image'], capture=True)
    return settings, daemon.resolve()


def status():
    settings, daemon = deployment()
    print(f"LayerFS: {settings['revision']}")
    print(f"Image: {settings['image']}")
    print(f'Daemon: {daemon}')
    receipts = list(STATE.glob('nsh-*/result.json'))
    if receipts:
        latest = max(receipts, key=lambda path: path.stat().st_mtime)
        result = json.loads(latest.read_text())
        outcome = 'passed' if result.get('ok') else ('failed' if 'error' in result else 'in progress')
        print(f'Latest validation: {outcome}')
        print(f'Receipt: {latest.resolve()}')
    else:
        print('Validation: run ./scripts/layerfs smoke')


def smoke(socket):
    check_docker()
    settings, daemon = deployment()
    name = 'nsh-' + uuid.uuid4().hex[:12]
    state = STATE / name
    source = state / 'input'
    source.mkdir(parents=True)
    # Use a named, committed input, excluding ignored caches and credentials.
    # Init itself intentionally captures everything in that input, including .git.
    archive = subprocess.check_output(['git', 'archive', 'HEAD'])
    with tarfile.open(fileobj=io.BytesIO(archive)) as tree:
        tree.extractall(source, filter='data')
    run('git', 'init', '--initial-branch=main', str(source))
    run('git', '-C', str(source), 'add', '.')
    run('git', '-C', str(source), '-c', 'user.name=NSH LayerFS trial',
        '-c', 'user.email=layerfs@example.invalid',
        '-c', 'maintenance.auto=false', '-c', 'gc.auto=0', 'commit', '-qm', 'Trial input')
    run('git', '-C', str(source), 'fsck', '--full')
    status = run('git', '-C', str(source), 'status', '--porcelain', capture=True)
    if status:
        raise RuntimeError(f'Trial input must be clean: {status}')
    state = state.resolve()
    volume = 'nsh-layerfs-' + name
    run('docker', 'volume', 'create', '--label', 'nsh.layerfs=trial', volume)
    settings = {**settings, 'project': name, 'volume': volume,
                'gitCommit': run('git', 'rev-parse', 'HEAD', capture=True).strip()}
    (state / 'settings.json').write_text(json.dumps(settings, indent=2))
    # The Linux SDK opens the same plain local volume as the Sandbox. Host
    # networking lets it reach the daemon's loopback-only published port.
    try:
        run('docker', 'run', '--rm', '--network', 'host',
            '--mount', f'type=bind,source={socket},target=/var/run/docker.sock',
            '--mount', f'type=volume,source={volume},target=/layerfs-store',
            '--mount', f'type=bind,source={state},target=/layerfs-state',
            '--mount', f'type=bind,source={source.resolve()},target=/layerfs-input,readonly',
            '--mount', f'type=bind,source={SOURCE.resolve() / "sdk/js"},target=/layerfs-sdk,readonly',
            '--mount', f'type=bind,source={daemon},target=/layerfs-daemon,readonly',
            '--mount', f'type=bind,source={pathlib.Path(".github/ci/layerfs/smoke.mjs").resolve()},target=/smoke.mjs,readonly',
            settings['image'], 'node', '/smoke.mjs')
    finally:
        print(f'Receipts: {state}/result.json', flush=True)
        print(f'Store retained in Docker volume: {volume}', flush=True)


def main():
    parser = argparse.ArgumentParser(prog='./scripts/layerfs', description=__doc__)
    parser.add_argument('command', choices=['prepare', 'smoke', 'status'])
    parser.add_argument('--docker-socket', help='local Docker socket (default: active Docker context)')
    args = parser.parse_args()
    os.chdir(ROOT)
    socket = docker_socket(args.docker_socket)
    if not socket.is_socket():
        raise RuntimeError(f'Docker socket is unavailable: {socket}; start your local Docker engine')
    os.environ['DOCKER_HOST'] = f'unix://{socket}'
    os.environ.pop('DOCKER_CONTEXT', None)
    if args.command == 'status':
        status()
    elif args.command == 'prepare':
        prepare()
    else:
        smoke(socket)


if __name__ == '__main__':
    try:
        main()
    except (RuntimeError, OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f'LayerFS: {error}', file=sys.stderr)
        sys.exit(1)
