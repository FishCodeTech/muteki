"""Boundary checks for the local LayerFS command (no Docker daemon required)."""

import importlib.util
import json
import os
import pathlib
import socket
import sys
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('layerfs', pathlib.Path(__file__).with_name('layerfs.py'))
layerfs = importlib.util.module_from_spec(spec)
spec.loader.exec_module(layerfs)


class LayerfsTests(unittest.TestCase):
    def test_host_endpoint_is_used_without_a_context_override(self):
        with patch.dict(os.environ, {'DOCKER_HOST': 'unix:///tmp/engine.sock'}, clear=True):
            with patch.object(layerfs, 'run') as run:
                self.assertEqual(layerfs.docker_socket(), pathlib.Path('/tmp/engine.sock').resolve())
                run.assert_not_called()

    def test_context_override_takes_precedence_over_host(self):
        with patch.dict(os.environ, {'DOCKER_CONTEXT': 'local', 'DOCKER_HOST': 'tcp://remote:2376'}, clear=True):
            with patch.object(layerfs, 'run', return_value='unix:///tmp/context.sock\n'):
                self.assertEqual(layerfs.docker_socket(), pathlib.Path('/tmp/context.sock').resolve())

    def test_remote_endpoint_is_refused(self):
        with patch.dict(os.environ, {'DOCKER_HOST': 'tcp://remote:2376'}, clear=True):
            with self.assertRaisesRegex(RuntimeError, 'local Docker Unix socket'):
                layerfs.docker_socket()

    def test_explicit_socket_controls_subsequent_docker_commands(self):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / 'docker.sock'
            with socket.socket(socket.AF_UNIX) as engine:
                engine.bind(str(path))
                with patch.dict(os.environ, {'DOCKER_CONTEXT': 'other'}, clear=True):
                    with patch.object(sys, 'argv', ['layerfs', 'prepare', '--docker-socket', str(path)]):
                        with patch.object(layerfs, 'prepare') as prepare, patch('os.chdir'):
                            layerfs.main()
                            prepare.assert_called_once_with()
                            self.assertEqual(os.environ['DOCKER_HOST'], f'unix://{path.resolve()}')
                            self.assertNotIn('DOCKER_CONTEXT', os.environ)

    def test_saved_daemon_path_survives_a_changed_target_environment(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            daemon = root / 'original-target' / 'release' / 'layerfs-daemon'
            daemon.parent.mkdir(parents=True)
            daemon.touch()
            source = root / 'source'
            addon = source / 'sdk/js/native/layerfs.node'
            addon.parent.mkdir(parents=True)
            addon.touch()
            (root / 'deployment.json').write_text(json.dumps({'image': 'sha256:test', 'daemon': str(daemon)}))
            with patch.object(layerfs, 'STATE', root), patch.object(layerfs, 'SOURCE', source):
                with patch.dict(os.environ, {'CARGO_TARGET_DIR': str(root / 'another-target')}):
                    with patch.object(layerfs, 'run'):
                        self.assertEqual(layerfs.deployment()[1], daemon.resolve())

    def test_missing_build_outputs_fail_before_starting_containers(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            (root / 'deployment.json').write_text(json.dumps({'image': 'sha256:test'}))
            with patch.object(layerfs, 'STATE', root), patch.object(layerfs, 'run') as run:
                with patch.dict(os.environ, {'CARGO_TARGET_DIR': str(root / 'missing')}):
                    with self.assertRaisesRegex(RuntimeError, 'build outputs are missing'):
                        layerfs.deployment()
                    run.assert_not_called()


if __name__ == '__main__':
    unittest.main()
