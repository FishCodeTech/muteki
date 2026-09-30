"""Cost controller — token usage and known dollar amounts per scope.

Feeds two things:
- L0 scheduler circuit-breaker (`over_budget(scope)`) — §4.2.
- The North Star metric `points per dollar-hour` — §12.

Prices are per-model, per-1M-tokens, configurable. Local prices produce
estimates; an unknown model remains unpriced instead of receiving a fallback.
"""

from __future__ import annotations

import time
from contextvars import ContextVar, Token
from dataclasses import dataclass, field
from typing import Optional, Callable, Any

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


# Explicit local estimate table. Reasoning tokens bill as output tokens.
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


@dataclass
class Ledger:
    usd: float = 0.0
    tokens: int = 0
    input_tokens: int = 0
    output_tokens: int = 0
    calls: int = 0
    unpriced_calls: int = 0

    def add(self, price: ModelPrice | None, input_tokens: int, output_tokens: int) -> float | None:
        c = price.cost(input_tokens, output_tokens) if price is not None else None
        if c is None:
            self.unpriced_calls += 1
        else:
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

    usage_sink: Optional[Callable[..., Any]] = None

    _global: Ledger = field(default_factory=Ledger)
    _by_challenge: dict[str, Ledger] = field(default_factory=dict)
    _by_solver: dict[str, Ledger] = field(default_factory=dict)
    _usage_windows: dict[str, Ledger] = field(default_factory=dict)
    _usage_context: ContextVar[str | None] = field(
        default_factory=lambda: ContextVar("muteki_usage_window", default=None)
    )
    _points: int = 0  # solved points, for the North Star metric

    def restore_run_totals(
        self, totals: dict[str, Any], *, event_usd: float | None = None,
    ) -> None:
        """Seed the run ledger from durable usage before a recovered Run resumes.

        Usage observations have stable identities, so the caller must do this
        once when constructing a new controller, before any invocation records
        more usage. Unknown fields remain unknown in UsageStore; the live ledger
        uses zero only as its additive starting value.
        """
        inp = max(0, int(totals.get("input_tokens") or 0))
        out = max(0, int(totals.get("output_tokens") or 0))
        reported = max(0.0, float(totals.get("reported_cost") or 0.0))
        estimated = max(0.0, float(totals.get("estimated_cost") or 0.0))
        self._global = Ledger(
            usd=(reported + estimated if totals.get("unverified_estimates")
                 else max(reported + estimated, max(0.0, float(event_usd or 0.0)))),
            tokens=inp + out,
            input_tokens=inp,
            output_tokens=out,
            calls=max(0, int(totals.get("records") or 0)),
            unpriced_calls=max(0, int(totals.get("unpriced") or 0)),
        )

    def price_for(self, model: str) -> ModelPrice | None:
        return self.prices.get(model)

    async def add_external_usd(
        self, usd: float, *, run_id: str, solver_id: Optional[str] = None,
        challenge_id: Optional[str] = None,
        input_tokens: int = 0, output_tokens: int = 0,
        usage: Optional[dict] = None, usage_id: Optional[str] = None,
        model: str = "", actor_kind: str = "worker", generation: Optional[int] = None, engine: str = "",
    ) -> float:
        """Charge a raw USD amount that did NOT go through our token pricing — e.g.
        a shelled CLI worker which reports its cost in dollars (claude's
        `total_cost_usd`) or for which the driver already derived an
        API-equivalent dollar cost from tokens (codex). Bumps the global +
        solver/challenge ledgers and emits COST_UPDATE so the deck + budget
        gate see the reported or driver-estimated amount.

        `input_tokens`/`output_tokens` are the run's reported usage; they land in
        the ledger's token counters (for the deck's token-usage column) but do NOT
        re-derive the cost. Pass 0 (the default) when the engine reports no tokens."""
        if self.usage_sink is not None:
            observed_usage = usage or {"input_tokens": input_tokens, "output_tokens": output_tokens, "reported_cost": usd}
            inserted = self.usage_sink(observed_usage, identity=usage_id, model=model, worker_id=solver_id, role=("worker" if actor_kind == "worker" else solver_id or "auxiliary"), actor_kind=actor_kind, generation=generation, engine=engine, price_source=("driver_estimate" if observed_usage.get("estimated_cost") is not None else None))
            if inserted is False:
                return 0.0
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
        window_id = self._usage_context.get()
        if window_id is not None and window_id in self._usage_windows:
            _bump(self._usage_windows[window_id])
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
            payload["unpriced_calls"] = led.unpriced_calls if (solver_id or challenge_id) else self._global.unpriced_calls
            payload["run_total"] = {"usd": self._global.usd, "unpriced_calls": self._global.unpriced_calls, **self.global_tokens()}
            payload["run_total_scope"] = "run"
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
        usage: Optional[dict] = None, usage_id: Optional[str] = None,
        actor_kind: str = "auxiliary", generation: Optional[int] = None, engine: str = "",
        estimate_cost: bool = True,
    ) -> float | None:
        """Record usage; emit COST_UPDATE; return None when the price is unknown."""
        price = self.price_for(model) if estimate_cost else None
        if self.usage_sink is not None:
            # record() charges the local price table. A CLI can supply reliable
            # token counts and an explicit native cost of zero; that zero is not
            # the amount this controller emits. Keep reported token metadata,
            # but persist the same *estimated* amount as COST_UPDATE.
            observed_usage = dict(usage or {
                "input_tokens": input_tokens, "output_tokens": output_tokens,
            })
            observed_usage.pop("reported_cost", None)
            observed_usage.pop("estimated_cost", None)
            if price is not None:
                observed_usage["estimated_cost"] = price.cost(input_tokens, output_tokens)
            inserted = self.usage_sink(observed_usage, identity=usage_id, model=model, worker_id=solver_id, role=("worker" if actor_kind == "worker" else solver_id or "auxiliary"), actor_kind=actor_kind, generation=generation, engine=engine, price_source=("model_price" if price is not None else None))
            if inserted is False:
                return 0.0
        cost = self._global.add(price, input_tokens, output_tokens)
        if challenge_id:
            self._by_challenge.setdefault(challenge_id, Ledger()).add(
                price, input_tokens, output_tokens
            )
        if solver_id:
            self._by_solver.setdefault(solver_id, Ledger()).add(
                price, input_tokens, output_tokens
            )
        window_id = self._usage_context.get()
        if window_id is not None and window_id in self._usage_windows:
            self._usage_windows[window_id].add(
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
            payload["unpriced_calls"] = led.unpriced_calls if (solver_id or challenge_id) else self._global.unpriced_calls
            payload["run_total"] = {"usd": self._global.usd, "unpriced_calls": self._global.unpriced_calls, **self.global_tokens()}
            payload["run_total_scope"] = "run"
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

    def solver_usage(self, solver_id: str) -> dict[str, int]:
        """Exact integer counters for a worker settlement receipt.

        Budget accounting must not scrape the presentation-oriented rounded
        ``snapshot()`` payload.  Micro-dollars keep the durable contract
        integer-only and deterministic.
        """
        usage = self.solver_usage_or_none(solver_id)
        if usage is None:
            return {
                "tokens": 0, "input_tokens": 0, "output_tokens": 0,
                "cost_micro_usd": 0, "calls": 0,
            }
        return usage

    def solver_usage_or_none(self, solver_id: str) -> dict[str, int] | None:
        """Return cumulative exact counters, preserving absent telemetry as None.

        A provider that emitted no usage record is accounted as UNKNOWN rather
        than silently converted into a zero charge.
        """
        led = self._by_solver.get(solver_id)
        if led is None:
            return None
        return {
            "tokens": int(led.tokens),
            "input_tokens": int(led.input_tokens),
            "output_tokens": int(led.output_tokens),
            "cost_micro_usd": int(round(led.usd * 1_000_000)),
            "calls": int(led.calls),
        }

    def begin_usage_window(self, window_id: str) -> Token[str | None]:
        """Bind subsequent cost records in this async context to one attempt."""
        if (
            type(window_id) is not str
            or not window_id
            or window_id != window_id.strip()
        ):
            raise ValueError("window_id must be a non-empty canonical string")
        if window_id in self._usage_windows:
            raise ValueError("usage window is already active")
        self._usage_windows[window_id] = Ledger()
        return self._usage_context.set(window_id)

    def finish_usage_window(
        self,
        window_id: str,
        token: Token[str | None],
    ) -> dict[str, int]:
        """Close an exact attempt window and return its non-cumulative counters."""
        if self._usage_context.get() != window_id:
            raise ValueError("usage window is not current in this context")
        ledger = self._usage_windows.pop(window_id, None)
        if ledger is None:
            raise ValueError("usage window is not active")
        self._usage_context.reset(token)
        return {
            "calls": int(ledger.calls),
            "cost_micro_usd": int(round(ledger.usd * 1_000_000)),
            "tokens": int(ledger.tokens),
        }

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
            "unpriced_calls": self._global.unpriced_calls,
            "points": self._points,
            "challenges": {
                k: round(v.usd, 6) for k, v in self._by_challenge.items()
            },
            "solvers": {k: round(v.usd, 6) for k, v in self._by_solver.items()},
        }
