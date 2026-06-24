"""Sprint 0.3 acceptance: cost accumulates correctly, over_budget triggers + emits."""

import pytest

from muteki.core.cost import Budget, CostController, ModelPrice
from muteki.core.event_bus import EventBus
from muteki.core.events import EventType


async def test_accumulates_per_scope() -> None:
    cc = CostController()
    # pro: 0.55 in / 2.19 out per 1M
    await cc.record(
        model="deepseek-v4-pro",
        input_tokens=1_000_000,
        output_tokens=1_000_000,
        run_id="r",
        challenge_id="c1",
        solver_id="s1",
    )
    expected = 0.55 + 2.19
    assert cc.global_usd() == pytest.approx(expected)
    assert cc.challenge_usd("c1") == pytest.approx(expected)
    assert cc.solver_usd("s1") == pytest.approx(expected)

    # second call on a different solver, same challenge
    await cc.record(
        model="deepseek-v4-flash",
        input_tokens=1_000_000,
        output_tokens=0,
        run_id="r",
        challenge_id="c1",
        solver_id="s2",
    )
    assert cc.solver_usd("s2") == pytest.approx(0.07)
    assert cc.challenge_usd("c1") == pytest.approx(expected + 0.07)
    assert cc.global_usd() == pytest.approx(expected + 0.07)


async def test_global_tokens_accumulates() -> None:
    """global_tokens() reports total input/output/total token usage — used by the
    eval ledger to compare context/cost against the long-lived baseline."""
    cc = CostController()
    await cc.record(model="deepseek-v4-pro", input_tokens=1_000,
                    output_tokens=500, run_id="r")
    # a CLI worker reporting dollars + tokens (the eval's real accrual path)
    await cc.add_external_usd(0.05, run_id="r", input_tokens=2_000, output_tokens=800)
    tok = cc.global_tokens()
    assert tok["input_tokens"] == 3_000
    assert tok["output_tokens"] == 1_300
    assert tok["tokens"] == 4_300


async def test_unknown_model_uses_fallback_not_zero() -> None:
    cc = CostController()
    await cc.record(
        model="mystery-model",
        input_tokens=1_000_000,
        output_tokens=1_000_000,
        run_id="r",
    )
    assert cc.global_usd() == pytest.approx(1.0 + 3.0)


async def test_over_budget_scopes() -> None:
    cc = CostController(
        budget=Budget(global_usd=1.0, per_challenge_usd=0.5, per_solver_usd=0.3)
    )
    assert cc.over_budget("global") is False
    await cc.record(
        model="deepseek-v4-pro",
        input_tokens=0,
        output_tokens=200_000,  # 0.2 * 2.19 = 0.438
        run_id="r",
        challenge_id="c1",
        solver_id="s1",
    )
    # solver 0.438 >= 0.3 -> over; challenge 0.438 < 0.5 -> not yet
    assert cc.over_budget("solver:s1") is True
    assert cc.over_budget("challenge:c1") is False
    assert cc.over_budget("global") is False
    # push challenge over
    await cc.record(
        model="deepseek-v4-pro",
        input_tokens=0,
        output_tokens=200_000,
        run_id="r",
        challenge_id="c1",
        solver_id="s1",
    )
    assert cc.over_budget("challenge:c1") is True
    assert cc.over_budget("global") is False  # 0.876 < 1.0


async def test_emits_cost_update_with_most_specific_scope() -> None:
    bus = EventBus()
    cc = CostController(bus=bus)
    got = []

    async def consume() -> None:
        async for e in bus.subscribe():
            got.append(e)
            return

    import asyncio

    t = asyncio.create_task(consume())
    await asyncio.sleep(0.02)
    await cc.record(
        model="deepseek-v4-flash",
        input_tokens=1000,
        output_tokens=1000,
        run_id="r",
        challenge_id="c1",
        solver_id="s1",
    )
    await asyncio.wait_for(t, timeout=5)
    ev = got[0]
    assert ev.event_type is EventType.COST_UPDATE
    assert ev.payload["scope"] == "solver"  # most specific
    assert ev.payload["solver_id"] == "s1"
    assert ev.payload["tokens"] == 2000
    # the deck's token column reads the per-direction breakdown off the payload
    assert ev.payload["input_tokens"] == 1000
    assert ev.payload["output_tokens"] == 1000


def test_north_star_points_per_dollar_hour() -> None:
    import time

    cc = CostController(started_at=time.time() - 3600)  # 1 hour elapsed
    cc._global.usd = 2.0  # pretend $2 spent
    cc.add_points(100)
    # 100 points / ($2 * 1h) = 50
    assert cc.points_per_dollar_hour() == pytest.approx(50.0, rel=1e-3)


def test_north_star_zero_when_no_spend() -> None:
    cc = CostController()
    cc.add_points(100)
    assert cc.points_per_dollar_hour() == 0.0


def test_custom_price_table() -> None:
    cc = CostController(prices={"x": ModelPrice(input_per_m=10, output_per_m=20)})
    p = cc.price_for("x")
    assert p.cost(1_000_000, 1_000_000) == pytest.approx(30.0)
