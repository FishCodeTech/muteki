"""Cost controller — real-time token/$ accounting per solver / challenge / global.

Feeds two things:
- L0 scheduler circuit-breaker (`over_budget(scope)`) — §4.2.
- The North Star metric `points per dollar-hour` — §12.

Prices are per-model, per-1M-tokens, configurable. The numbers below are
placeholders for the temporary DeepSeek endpoint; correctness lives in the
accounting, not the exact rate. Update PRICES when real rates are known.
"""

from __future__ import annotations

import time
from dataclasses import dataclass, field
from typing import Optional

from muteki.core.event_bus import EventBus
from muteki.core.events import Event, EventType, cost_payload


@dataclass(frozen=True)
class ModelPrice:
    """USD per 1M tokens."""

    input_per_m: float
    output_per_m: float

    def cost(self, input_tokens: int, output_tokens: int) -> float:
        return (
            input_tokens / 1_000_000 * self.input_per_m
            + output_tokens / 1_000_000 * self.output_per_m
        )


# Placeholder price table. Reasoning tokens bill as output tokens.
PRICES: dict[str, ModelPrice] = {
    "deepseek-v4-pro": ModelPrice(input_per_m=0.55, output_per_m=2.19),
    "deepseek-v4-flash": ModelPrice(input_per_m=0.07, output_per_m=0.28),
    # codex (GPT-5 class) — subscription CLIs no longer report total_cost_usd,
    # so we re-derive an API-EQUIVALENT cost from the tokens it does report (the
    # same "what would this have cost on the API" lens we keep for claude). GPT-5
    # list price per 1M: input $1.25, output $10.00 (reasoning bills as output).
    # Cached input ($0.125/M) is folded into input here; cli_driver discounts it.
    "codex": ModelPrice(input_per_m=1.25, output_per_m=10.0),
    "gpt-5": ModelPrice(input_per_m=1.25, output_per_m=10.0),
}
# Cached-input rate for codex/GPT-5 (per 1M). cli_driver prices cached tokens at
# this rate and fresh tokens at the full input rate when computing codex cost.
CODEX_CACHED_INPUT_PER_M = 0.125
# Fallback for unknown models so accounting never silently drops to zero.
_DEFAULT_PRICE = ModelPrice(input_per_m=1.0, output_per_m=3.0)


@dataclass
class Ledger:
    usd: float = 0.0
    tokens: int = 0
    input_tokens: int = 0
    output_tokens: int = 0
    calls: int = 0

    def add(self, price: ModelPrice, input_tokens: int, output_tokens: int) -> float:
        c = price.cost(input_tokens, output_tokens)
        self.usd += c
        self.input_tokens += input_tokens
        self.output_tokens += output_tokens
        self.tokens += input_tokens + output_tokens
        self.calls += 1
        return c


@dataclass
class Budget:
    """USD ceilings per scope. None == unlimited."""

    global_usd: Optional[float] = None
    per_challenge_usd: Optional[float] = None
    per_solver_usd: Optional[float] = None


@dataclass
class CostController:
    bus: Optional[EventBus] = None
    budget: Budget = field(default_factory=Budget)
    prices: dict[str, ModelPrice] = field(default_factory=lambda: dict(PRICES))
    started_at: float = field(default_factory=time.time)

    _global: Ledger = field(default_factory=Ledger)
    _by_challenge: dict[str, Ledger] = field(default_factory=dict)
    _by_solver: dict[str, Ledger] = field(default_factory=dict)
    _points: int = 0  # solved points, for the North Star metric

    def price_for(self, model: str) -> ModelPrice:
        return self.prices.get(model, _DEFAULT_PRICE)

    async def add_external_usd(
        self, usd: float, *, run_id: str, solver_id: Optional[str] = None,
        challenge_id: Optional[str] = None,
        input_tokens: int = 0, output_tokens: int = 0,
    ) -> float:
        """Charge a raw USD amount that did NOT go through our token pricing — e.g.
        a shelled CLI worker which reports its cost in dollars (claude's
        `total_cost_usd`) or for which the driver already derived an
        API-equivalent dollar cost from tokens (codex). Bumps the global +
        solver/challenge ledgers and emits COST_UPDATE so the deck + budget
        breaker see real spend.

        `input_tokens`/`output_tokens` are the run's reported usage; they land in
        the ledger's token counters (for the deck's token-usage column) but do NOT
        re-derive the cost — `usd` is authoritative here (the driver already priced
        it). Pass 0 (the default) when the engine reports no token counts."""
        usd = max(0.0, float(usd))
        inp, outp = max(0, int(input_tokens)), max(0, int(output_tokens))

        def _bump(led: Ledger) -> None:
            led.usd += usd
            led.input_tokens += inp
            led.output_tokens += outp
            led.tokens += inp + outp
            led.calls += 1

        _bump(self._global)
        if challenge_id:
            _bump(self._by_challenge.setdefault(challenge_id, Ledger()))
        if solver_id:
            _bump(self._by_solver.setdefault(solver_id, Ledger()))
        if self.bus is not None:
            if solver_id:
                led = self._by_solver[solver_id]
                payload = cost_payload("solver", led.usd, led.tokens, solver_id=solver_id,
                                       input_tokens=led.input_tokens, output_tokens=led.output_tokens)
            elif challenge_id:
                led = self._by_challenge[challenge_id]
                payload = cost_payload("challenge", led.usd, led.tokens, challenge_id=challenge_id,
                                       input_tokens=led.input_tokens, output_tokens=led.output_tokens)
            else:
                payload = cost_payload("global", self._global.usd, self._global.tokens,
                                       input_tokens=self._global.input_tokens,
                                       output_tokens=self._global.output_tokens)
            await self.bus.emit(Event(
                event_type=EventType.COST_UPDATE, run_id=run_id,
                challenge_id=challenge_id, solver_id=solver_id, payload=payload))
        return usd

    async def record(
        self,
        *,
        model: str,
        input_tokens: int,
        output_tokens: int,
        run_id: str,
        challenge_id: Optional[str] = None,
        solver_id: Optional[str] = None,
    ) -> float:
        """Record one LLM call's usage; emit COST_UPDATE; return its USD cost."""
        price = self.price_for(model)
        cost = self._global.add(price, input_tokens, output_tokens)
        if challenge_id:
            self._by_challenge.setdefault(challenge_id, Ledger()).add(
                price, input_tokens, output_tokens
            )
        if solver_id:
            self._by_solver.setdefault(solver_id, Ledger()).add(
                price, input_tokens, output_tokens
            )
        if self.bus is not None:
            # emit the most specific scope available
            if solver_id:
                led = self._by_solver[solver_id]
                payload = cost_payload("solver", led.usd, led.tokens, solver_id=solver_id,
                                       input_tokens=led.input_tokens, output_tokens=led.output_tokens)
            elif challenge_id:
                led = self._by_challenge[challenge_id]
                payload = cost_payload(
                    "challenge", led.usd, led.tokens, challenge_id=challenge_id,
                    input_tokens=led.input_tokens, output_tokens=led.output_tokens
                )
            else:
                payload = cost_payload("global", self._global.usd, self._global.tokens,
                                       input_tokens=self._global.input_tokens,
                                       output_tokens=self._global.output_tokens)
            await self.bus.emit(
                Event(
                    event_type=EventType.COST_UPDATE,
                    run_id=run_id,
                    challenge_id=challenge_id,
                    solver_id=solver_id,
                    payload=payload,
                )
            )
        return cost

    # -- budget circuit breaker (§4.2) ------------------------------------
    def over_budget(self, scope: str) -> bool:
        """scope: 'global' | 'challenge:<id>' | 'solver:<id>'."""
        if scope == "global":
            return (
                self.budget.global_usd is not None
                and self._global.usd >= self.budget.global_usd
            )
        if scope.startswith("challenge:"):
            cid = scope.split(":", 1)[1]
            led = self._by_challenge.get(cid)
            return (
                self.budget.per_challenge_usd is not None
                and led is not None
                and led.usd >= self.budget.per_challenge_usd
            )
        if scope.startswith("solver:"):
            sid = scope.split(":", 1)[1]
            led = self._by_solver.get(sid)
            return (
                self.budget.per_solver_usd is not None
                and led is not None
                and led.usd >= self.budget.per_solver_usd
            )
        return False

    # -- reporting / North Star -------------------------------------------
    def add_points(self, points: int) -> None:
        self._points += points

    def global_usd(self) -> float:
        return self._global.usd

    def global_tokens(self) -> dict:
        """Total token usage across the whole run — for eval ledgers / baseline
        comparison. Mirrors global_usd()."""
        return {
            "tokens": self._global.tokens,
            "input_tokens": self._global.input_tokens,
            "output_tokens": self._global.output_tokens,
        }

    def challenge_usd(self, challenge_id: str) -> float:
        led = self._by_challenge.get(challenge_id)
        return led.usd if led else 0.0

    def solver_usd(self, solver_id: str) -> float:
        led = self._by_solver.get(solver_id)
        return led.usd if led else 0.0

    def points_per_dollar_hour(self, now: Optional[float] = None) -> float:
        """North Star: points / (USD * hours). 0 when no spend yet."""
        now = now if now is not None else time.time()
        hours = max((now - self.started_at) / 3600.0, 1e-9)
        denom = self._global.usd * hours
        if denom <= 0:
            return 0.0
        return self._points / denom

    def snapshot(self) -> dict:
        return {
            "global_usd": round(self._global.usd, 6),
            "global_tokens": self._global.tokens,
            "calls": self._global.calls,
            "points": self._points,
            "challenges": {
                k: round(v.usd, 6) for k, v in self._by_challenge.items()
            },
            "solvers": {k: round(v.usd, 6) for k, v in self._by_solver.items()},
        }
