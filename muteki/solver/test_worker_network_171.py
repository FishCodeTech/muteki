"""#171 / MNT-09.04: honest worker network resolve — no silent none→bridge."""
from __future__ import annotations

import os
import unittest
from unittest import mock

from muteki.solver.container_exec import (
    WorkerNetworkConfigError,
    project_worker_network,
    resolve_worker_run_network,
)


class ProjectWorkerNetworkTests(unittest.TestCase):
    def test_bridge_default(self) -> None:
        with mock.patch.dict(os.environ, {}, clear=False):
            os.environ.pop("MUTEKI_WORKER_NETWORK", None)
            proj = project_worker_network("bridge")
        self.assertEqual(proj["requested"], "bridge")
        self.assertEqual(proj["effective"], "bridge")

    def test_none_stays_none(self) -> None:
        with mock.patch.dict(os.environ, {}, clear=False):
            os.environ.pop("MUTEKI_WORKER_NETWORK", None)
            proj = project_worker_network("none")
        self.assertEqual(proj["requested"], "none")
        self.assertEqual(proj["effective"], "none")
        # Must never silently widen to bridge.
        self.assertNotEqual(proj["effective"], "bridge")

    def test_host_kept(self) -> None:
        with mock.patch.dict(os.environ, {"MUTEKI_WORKER_NETWORK": "muteki_net"}, clear=False):
            proj = project_worker_network("host")
        self.assertEqual(proj["effective"], "host")

    def test_compose_override_remaps_bridge_only(self) -> None:
        with mock.patch.dict(os.environ, {"MUTEKI_WORKER_NETWORK": "muteki_net"}, clear=False):
            proj = project_worker_network("bridge")
            self.assertEqual(proj["effective"], "muteki_net")
            self.assertEqual(proj["requested"], "bridge")
            with self.assertRaises(WorkerNetworkConfigError) as ctx:
                project_worker_network("none")
            self.assertIn("MUTEKI_WORKER_NETWORK", str(ctx.exception))
            self.assertIn("none", str(ctx.exception))

    def test_bridge_override_cannot_silently_select_host(self) -> None:
        for override in ("host", "none", "container:other"):
            with mock.patch.dict(os.environ, {"MUTEKI_WORKER_NETWORK": override}):
                with self.assertRaises(WorkerNetworkConfigError):
                    project_worker_network("bridge")

    def test_recreate_keeps_requested_mode_separate_from_compose_name(self) -> None:
        from muteki.solver import container_exec as ce
        handle = ce.ContainerHandle(run_id="test", run_workspace="/tmp/run", host_workspace="/tmp/run",
                                    container="test", network="compose_bridge", requested_network="bridge")
        with mock.patch.object(ce, "_container_state", return_value=None), \
             mock.patch.object(ce, "ensure_container", return_value=handle) as ensure:
            ce._ensure_alive(handle)
        self.assertEqual(ensure.call_args.kwargs["network"], "bridge")

    def test_invalid_rejected(self) -> None:
        with self.assertRaises(WorkerNetworkConfigError):
            project_worker_network("container")


class ResolveWorkerRunNetworkTests(unittest.TestCase):
    def tearDown(self) -> None:
        os.environ.pop("MUTEKI_WORKER_NETWORK", None)

    def test_none_rejected_when_egress_required(self) -> None:
        os.environ.pop("MUTEKI_WORKER_NETWORK", None)
        with self.assertRaises(WorkerNetworkConfigError) as ctx:
            resolve_worker_run_network("none", needs_egress=True)
        msg = str(ctx.exception)
        self.assertIn("none", msg)
        self.assertIn("bridge", msg.lower())  # explains we refuse to upgrade
        self.assertNotIn("rewrit", msg.lower())

    def test_none_allowed_when_egress_not_required(self) -> None:
        os.environ.pop("MUTEKI_WORKER_NETWORK", None)
        self.assertEqual(
            resolve_worker_run_network("none", needs_egress=False),
            "none",
        )

    def test_default_needs_egress_rejects_none(self) -> None:
        os.environ.pop("MUTEKI_WORKER_NETWORK", None)
        with self.assertRaises(WorkerNetworkConfigError):
            resolve_worker_run_network("none")

    def test_bridge_with_compose_override(self) -> None:
        with mock.patch.dict(os.environ, {"MUTEKI_WORKER_NETWORK": "muteki_net"}, clear=False):
            self.assertEqual(resolve_worker_run_network("bridge"), "muteki_net")

    def test_empty_defaults_to_bridge(self) -> None:
        os.environ.pop("MUTEKI_WORKER_NETWORK", None)
        self.assertEqual(resolve_worker_run_network(""), "bridge")
        self.assertEqual(resolve_worker_run_network(None), "bridge")


class SwarmPayloadHonestyTests(unittest.TestCase):
    def test_workspace_payload_does_not_rewrite_none(self) -> None:
        from muteki.swarm.stage_policy import StagePolicy
        from muteki.swarm.swarm import _workspace_runtime_payload

        os.environ.pop("MUTEKI_WORKER_NETWORK", None)
        payload = _workspace_runtime_payload(
            backend="container",
            network="none",
            run_id="test-171",
            web_access=False,
            kb=False,
            coordinator=True,
            cli_race=False,
            race_scout=False,
            max_workers=2,
            max_total_workers=None,
            cost_budget_usd=None,
            wall_clock_budget=0,
            stage_policy=StagePolicy(),
            worker_profiles=[],
        )
        self.assertEqual(payload["network_requested"], "none")
        self.assertEqual(payload["network"], "none")
        self.assertEqual(payload["effective_network"], "none")
        # offline flag is web-tool denial, not proof of Docker network none.
        self.assertTrue(payload["offline"])

    def test_require_mode_rejects_unknown(self) -> None:
        from muteki.swarm.swarm import _require_worker_network_mode

        self.assertEqual(_require_worker_network_mode("bridge"), "bridge")
        with self.assertRaises(ValueError):
            _require_worker_network_mode("weird")


if __name__ == "__main__":
    unittest.main()
