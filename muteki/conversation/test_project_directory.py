"""C25: missing project root_path is a typed ConversationError."""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from muteki.conversation.manager import ConversationError, ConversationManager
from muteki.conversation.projections import ConversationProjection
from muteki.conversation.store import ConversationStore
from muteki.platform.capability_bindings import CapabilityBindingService
from muteki.platform.store import PlatformStore


class ProjectDirectoryReadinessTest(unittest.TestCase):
    def test_missing_local_root_raises_conversation_error(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = PlatformStore(db_path=Path(tmp) / "platform.db")
            conv = ConversationStore(store)
            bindings = CapabilityBindingService(store)
            projection = ConversationProjection(store, conv)
            manager = ConversationManager(
                store,
                conv,
                bindings,
                projection,
                workspace_root=Path(tmp) / "workspaces",
            )
            missing = str(Path(tmp) / "no-such-dir")
            with self.assertRaises(ConversationError) as ctx:
                manager.plan_workspace(kind="local", root_path=missing)
            self.assertIn("不存在", str(ctx.exception))


if __name__ == "__main__":
    unittest.main()
