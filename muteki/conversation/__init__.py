"""CONV-01：Conversation 后端与 ExternalAgentSessionExecutor（任务书 9.1）。

通用对话工作区后端：Project / Workspace / Thread / Task / Run /
AgentSession 的创建与恢复、Thread 级 CapabilityBinding 生命周期、
幂等 Turn 交付、Public Event 与 snapshot watermark、Artifact / usage /
Runtime error / unread 读模型。

入口：``module.ConversationService``；HTTP 装配见
``apps.web.conversation_api.create_conversation_router``。
"""

from .commands import (
    COMMAND_TYPES,
    ConversationCommandHandler,
    ConversationThreadListQueryHandler,
    ConversationThreadViewQueryHandler,
    register_conversation_handlers,
)
from .events import PRODUCER
from .executor import ExternalAgentSessionExecutor
from .manager import (
    ConversationError,
    ConversationManager,
    WORKSPACE_GIT,
    WORKSPACE_ISOLATED,
    WORKSPACE_LOCAL,
)
from .models import (
    ConversationMessage,
    ThreadRuntimeSelection,
    ThreadState,
    TurnRecord,
    TurnRunRef,
)
from .module import ConversationService
from .projections import ConversationProjection
from .store import ConversationStore

__all__ = [
    "COMMAND_TYPES",
    "PRODUCER",
    "WORKSPACE_GIT",
    "WORKSPACE_ISOLATED",
    "WORKSPACE_LOCAL",
    "ConversationCommandHandler",
    "ConversationError",
    "ConversationManager",
    "ConversationMessage",
    "ConversationProjection",
    "ConversationService",
    "ConversationStore",
    "ConversationThreadListQueryHandler",
    "ConversationThreadViewQueryHandler",
    "ExternalAgentSessionExecutor",
    "ThreadRuntimeSelection",
    "ThreadState",
    "TurnRecord",
    "TurnRunRef",
    "register_conversation_handlers",
]
