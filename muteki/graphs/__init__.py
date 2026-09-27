"""GraphService 三类正式实现（任务书 8.3，GRAPH-01）。

- ``ctf.shared_graph.v1``：现有 CTF SharedGraph 的薄 Adapter（见
  ``ctf_shared_graph.py``），历史数据库无迁移即可继续读写。
- ``collaboration.graph.v1``：通用协作图（见 ``collaboration.py``），
  fact / intent / branch / review / directive，独立 SQLite 库。
- ``memory.timeline.v1``：通用对话长期记忆（见 ``memory_timeline.py``），
  用户允许的记忆事件、来源记录与 tombstone 删除。

三者共享 ``base.py`` 的事件 / 租约 / 投影基础代码，但不共享数据库，
也不混用领域事件。
"""

from muteki.graphs.base import GraphError, SQLiteGraphBase
from muteki.graphs.collaboration import CollaborationGraph
from muteki.graphs.ctf_shared_graph import CtfSharedGraphService
from muteki.graphs.memory_timeline import MemoryTimelineGraph

__all__ = [
    "CollaborationGraph",
    "CtfSharedGraphService",
    "GraphError",
    "MemoryTimelineGraph",
    "SQLiteGraphBase",
]
