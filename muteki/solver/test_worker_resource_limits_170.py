"""#170 / MNT-09.03: configurable worker cgroup + stream/workdir budgets."""

from __future__ import annotations

import os
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from muteki.solver.result_codes import (
    RESULT_DISK_LIMIT,
    RESULT_OUTPUT_LIMIT,
    TRANSIENT_CODES,
    is_transient,
)
from muteki.solver.worker_resource_limits import (
    FULL_DEFAULTS,
    SLIM_DEFAULTS,
    defaults_for_image,
    is_slim_worker_image,
    parse_byte_size,
    resolve_worker_resource_limits,
    validate_cpus,
    validate_memory,
    validate_pids_limit,
)


class ParseAndValidateTests(unittest.TestCase):
    def test_parse_byte_size(self) -> None:
        self.assertEqual(parse_byte_size("2g"), 2 * 1024**3)
        self.assertEqual(parse_byte_size("64m"), 64 * 1024**2)
        self.assertEqual(parse_byte_size(1024), 1024)
        with self.assertRaises(ValueError):
            parse_byte_size("0m")
        with self.assertRaises(ValueError):
            parse_byte_size("")

    def test_reject_nonfinite_fractional_and_overflow_values(self) -> None:
        for value in [float("nan"), float("inf"), "-inf"]:
            with self.assertRaises(ValueError): validate_cpus(value)
        for value in ["9223372036854775808", "99e", "0.1"]:
            with self.assertRaises(ValueError): parse_byte_size(value)
        with self.assertRaises(ValueError): validate_pids_limit(1.5)

    def test_validate_memory_cpus_pids(self) -> None:
        self.assertEqual(validate_memory("2G"), "2g")
        self.assertEqual(validate_cpus(2), "2")
        self.assertEqual(validate_cpus("1.5"), "1.5")
        self.assertEqual(validate_pids_limit("512"), 512)
        with self.assertRaises(ValueError):
            validate_cpus(0)
        with self.assertRaises(ValueError):
            validate_pids_limit(-1)
        with self.assertRaises(ValueError):
            validate_memory("not-a-size")


class DefaultsAndEnvTests(unittest.TestCase):
    def test_full_vs_slim_defaults(self) -> None:
        self.assertFalse(is_slim_worker_image("ghcr.io/fishcodetech/muteki-worker:latest"))
        self.assertTrue(is_slim_worker_image("muteki-worker-slim:latest"))
        self.assertEqual(defaults_for_image("muteki-worker:latest").memory, "2g")
        self.assertEqual(defaults_for_image("muteki-worker-slim:1").memory, "1g")
        self.assertEqual(FULL_DEFAULTS.pids_limit, 512)
        self.assertEqual(SLIM_DEFAULTS.pids_limit, 256)
        # Must not copy TSec 64MiB/256MiB/4GiB/70% defaults as the product memory cap.
        self.assertNotEqual(FULL_DEFAULTS.memory.lower(), "64m")
        self.assertNotEqual(FULL_DEFAULTS.memory.lower(), "256m")

    def test_env_overrides(self) -> None:
        with mock.patch.dict(os.environ, {
            "MUTEKI_WORKER_IMAGE": "muteki-worker:latest",
            "MUTEKI_WORKER_MEMORY": "3g",
            "MUTEKI_WORKER_CPUS": "4",
            "MUTEKI_WORKER_PIDS": "128",
            "MUTEKI_WORKER_OUTPUT_LIMIT": "16m",
            "MUTEKI_WORKER_DISK_LIMIT": "1g",
        }, clear=False):
            lim = resolve_worker_resource_limits()
        self.assertEqual(lim.memory, "3g")
        self.assertEqual(lim.cpus, "4")
        self.assertEqual(lim.pids_limit, 128)
        self.assertEqual(lim.output_limit, "16m")
        self.assertEqual(lim.disk_limit, "1g")
        self.assertEqual(lim.output_limit_bytes, 16 * 1024**2)

    def test_explicit_beats_env(self) -> None:
        with mock.patch.dict(os.environ, {"MUTEKI_WORKER_MEMORY": "9g"}, clear=False):
            lim = resolve_worker_resource_limits(memory="1g")
        self.assertEqual(lim.memory, "1g")


class WorkerConfigStoreTests(unittest.TestCase):
    def test_get_includes_limits_and_set_validates(self) -> None:
        from apps.web.worker_config import WorkerConfigStore

        with tempfile.TemporaryDirectory() as tmp:
            store = WorkerConfigStore(tmp)
            cfg = store.get()
            self.assertIn("worker_memory", cfg)
            self.assertIn("worker_cpus", cfg)
            self.assertIn("worker_pids_limit", cfg)
            self.assertIn("worker_output_limit", cfg)
            self.assertIn("worker_disk_limit", cfg)
            self.assertEqual(cfg["worker_memory"], FULL_DEFAULTS.memory)
            store.set(worker_memory="1500m", worker_cpus="1", worker_pids_limit=256)
            cfg2 = store.get()
            self.assertEqual(cfg2["worker_memory"], "1500m")
            self.assertEqual(cfg2["worker_cpus"], "1")
            self.assertEqual(cfg2["worker_pids_limit"], 256)
            resolved = store.resolve("misc")
            self.assertEqual(resolved["worker_memory"], "1500m")
            self.assertEqual(resolved["worker_pids_limit"], 256)
            with self.assertRaises(ValueError):
                store.set(worker_memory="0m")
            with self.assertRaises(ValueError):
                store.set(worker_pids_limit=0)


class ResultMappingTests(unittest.TestCase):
    def test_output_and_disk_are_transient_not_explored(self) -> None:
        self.assertIn(RESULT_OUTPUT_LIMIT, TRANSIENT_CODES)
        self.assertIn(RESULT_DISK_LIMIT, TRANSIENT_CODES)
        self.assertTrue(is_transient(RESULT_OUTPUT_LIMIT))
        self.assertTrue(is_transient(RESULT_DISK_LIMIT))

    def test_controlled_result_stop_maps_limits(self) -> None:
        from muteki.solver.cli_engines.types import CliResult
        from muteki.solver.cli_results import _controlled_result_stop

        out = CliResult(text="", output_limit=True)
        mapped = _controlled_result_stop(out)
        self.assertIsNotNone(mapped)
        assert mapped is not None
        self.assertEqual(mapped[0], RESULT_OUTPUT_LIMIT)

        disk = CliResult(text="", disk_limit=True)
        mapped = _controlled_result_stop(disk)
        self.assertIsNotNone(mapped)
        assert mapped is not None
        self.assertEqual(mapped[0], RESULT_DISK_LIMIT)

        # Over-limit must not look like a normal explored/unsolved outcome.
        self.assertNotEqual(mapped[0], "explored")


class ContainerResultEvidenceTests(unittest.TestCase):
    def test_stream_and_nonstream_keep_budget_and_evidence(self) -> None:
        from types import SimpleNamespace
        from muteki.solver import container_exec as ce
        from muteki.solver.cli_engines.types import CliResult
        limits = resolve_worker_resource_limits(output_limit="2m", disk_limit="8m")
        root = str(Path("/tmp/test").resolve())
        handle = ce.ContainerHandle(run_id="budget-test", run_workspace=root,
                                    host_workspace=root, container="test", resource_limits=limits)
        for streaming in (False, True):
            result = CliResult(text="", output_limit=True,
                               runtime_status={"status":"output_limit", "rc":137, "output_bytes":100})
            call = "run_cli_streaming_rcp" if streaming else "run_cli_rcp"
            with mock.patch.object(ce, "_ensure_alive"), \
                 mock.patch.object(ce, "check_process_launch"), \
                 mock.patch.object(ce, "_containerize_argv", return_value=["/bin/true"]), \
                 mock.patch("muteki.solver.control_client."+call, return_value=result) as run:
                fn = ce.run_cli_streaming_container if streaming else ce.run_cli_container
                kwargs = {"on_step":lambda step: None} if streaming else {}
                actual = fn(SimpleNamespace(name="pi"), ["/bin/true"], handle=handle,
                            cwd=root+"/child", timeout=10, **kwargs)
            self.assertIs(run.call_args.kwargs["resource_limits"], limits)
            self.assertEqual(actual.runtime_status["status"], "output_limit")
            self.assertEqual(actual.runtime_status["output_bytes"], 100)


class SwarmStoresLimitsTests(unittest.TestCase):
    def test_swarm_stores_limit_fields(self) -> None:
        # Importing Swarm pulls heavy deps; only assert the ctor signature accepts
        # the new kwargs by inspecting __init__ defaults via a lightweight mock
        # of required args is too heavy — instead check the source attributes on
        # a minimal constructed path is skipped and we verify annotations exist.
        import inspect
        from muteki.swarm import swarm as swarm_mod

        sig = inspect.signature(swarm_mod.Swarm.__init__)
        for name in (
            "worker_memory", "worker_cpus", "worker_pids_limit",
            "worker_output_limit", "worker_disk_limit",
        ):
            self.assertIn(name, sig.parameters)


if __name__ == "__main__":
    unittest.main()
