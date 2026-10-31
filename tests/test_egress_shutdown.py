import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock
from muteki.src.egress import SrcEgressManager


class EgressShutdownTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.manager = SrcEgressManager(self.root)
        self.manager._docker = Mock()

    def test_empty_instance_never_contacts_docker(self):
        self.manager.stop_all()
        self.manager._docker.assert_not_called()

    def test_only_current_instances_mounts_are_stopped(self):
        owned, foreign = 'a' * 24, 'b' * 24
        for name in (owned, foreign):
            (self.root / name).mkdir()
        def inspect(name):
            source = self.root / owned if name.endswith(owned) else Path('/other/installation') / foreign
            return {'Mounts': [{'Type': 'bind', 'Destination': '/run/src', 'Source': str(source)}]}
        self.manager._inspect = Mock(side_effect=inspect)
        self.manager.stop_all()
        self.manager._docker.assert_called_once_with('stop', 'muteki-src-egress-' + owned)

    def test_missing_containers_and_unrelated_directories_are_ignored(self):
        (self.root / ('c' * 24)).mkdir()
        (self.root / 'unrelated').mkdir()
        (self.root / ('d' * 24)).symlink_to(self.root / 'unrelated', target_is_directory=True)
        self.manager._inspect = Mock(return_value=None)
        self.manager.stop_all()
        self.manager._inspect.assert_called_once_with('muteki-src-egress-' + 'c' * 24)
        self.manager._docker.assert_not_called()
