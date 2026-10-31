"""Separate follow-up spending from the solve budget, retaining durable usage."""
from muteki.core.cost import CostController


def standby_cost_controller(main: CostController, action: str) -> CostController:
    if action not in {"ask", "writeup"}:
        return main  # mark_false is an actual solve continuation.
    def record(raw, **scope):
        scope.update(role="standby", actor_kind="auxiliary")
        if main.usage_sink is not None:
            return main.usage_sink(raw, **scope)
    # No shared bus: auxiliary COST_UPDATE must not replace the main UI total.
    return CostController(prices=dict(main.prices), usage_sink=record)
