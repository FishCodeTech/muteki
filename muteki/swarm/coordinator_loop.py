"""Public mixin façade for the coordinator scheduling loop."""

from muteki.swarm import coordinator_progress as _coord_progress
from muteki.swarm import coordinator_scheduler as _coord_scheduler


class _CoordinatorLoopMixin:
    """Coordinator loop façade. Method bodies live in scheduler/progress modules."""

_CoordinatorLoopMixin._budget_elapsed = _coord_progress._budget_elapsed
_CoordinatorLoopMixin._persist_winner = _coord_progress._persist_winner
_CoordinatorLoopMixin._reopen_stalled_intent = _coord_progress._reopen_stalled_intent
_CoordinatorLoopMixin.config_poll_interval = _coord_progress.config_poll_interval
_CoordinatorLoopMixin._run_coordinator = _coord_scheduler._run_coordinator
