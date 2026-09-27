"""Configurable worker container + stream/workdir budgets (MNT-09.03 / #170).

These are general full/slim defaults for Muteki RCP workers — not TSec hosted
defaults (64MiB / 256MiB / 4GiB / 70%). Callers may override via worker_config,
explicit kwargs, or MUTEKI_WORKER_* environment variables.
"""

from __future__ import annotations

import os
import math
from decimal import Decimal, InvalidOperation
import re
from dataclasses import dataclass
from typing import Any, Mapping, Optional

# Docker --memory accepts this shape; we also parse it to bytes for stream/disk.
_SIZE_RE = re.compile(
    r"^\s*(\d+(?:\.\d+)?)\s*([kmgtpe]i?b?|b)?\s*$",
    re.IGNORECASE,
)

_UNIT_BYTES = {
    "": 1,
    "b": 1,
    "k": 1024,
    "kb": 1024,
    "ki": 1024,
    "kib": 1024,
    "m": 1024**2,
    "mb": 1024**2,
    "mi": 1024**2,
    "mib": 1024**2,
    "g": 1024**3,
    "gb": 1024**3,
    "gi": 1024**3,
    "gib": 1024**3,
    "t": 1024**4,
    "tb": 1024**4,
    "ti": 1024**4,
    "tib": 1024**4,
    "p": 1024**5,
    "pb": 1024**5,
    "pi": 1024**5,
    "pib": 1024**5,
    "e": 1024**6,
    "eb": 1024**6,
    "ei": 1024**6,
    "eib": 1024**6,
}


@dataclass(frozen=True)
class WorkerResourceLimits:
    """Cgroup + stream/workdir budgets for one worker container / RCP worker."""

    memory: str
    cpus: str
    pids_limit: int
    output_limit: str
    disk_limit: str
    profile: str = "full"  # "full" | "slim"

    @property
    def output_limit_bytes(self) -> int:
        return parse_byte_size(self.output_limit)

    @property
    def disk_limit_bytes(self) -> int:
        return parse_byte_size(self.disk_limit)


# Sensible product defaults — deliberately not TSec's 64MiB/256MiB/4GiB/70%.
FULL_DEFAULTS = WorkerResourceLimits(
    memory="2g",
    cpus="2",
    pids_limit=512,
    output_limit="64m",
    disk_limit="4g",
    profile="full",
)
SLIM_DEFAULTS = WorkerResourceLimits(
    memory="1g",
    cpus="1",
    pids_limit=256,
    output_limit="32m",
    disk_limit="2g",
    profile="slim",
)


def is_slim_worker_image(image: str | None = None) -> bool:
    """True when the configured worker image name looks like the slim variant."""
    raw = (image if image is not None else os.environ.get("MUTEKI_WORKER_IMAGE") or "")
    return "slim" in str(raw).strip().lower()


def defaults_for_image(image: str | None = None) -> WorkerResourceLimits:
    return SLIM_DEFAULTS if is_slim_worker_image(image) else FULL_DEFAULTS


def parse_byte_size(value: Any, *, field: str = "size") -> int:
    """Parse a docker-style size (``2g``, ``512m``, ``1048576``) into bytes."""
    if isinstance(value, bool):
        raise ValueError(f"{field} must be a positive size")
    text = str(value).strip()
    match = _SIZE_RE.fullmatch(text)
    if not match:
        raise ValueError(f"{field} must look like 512m or 2g")
    try:
        amount = Decimal(match.group(1)) * _UNIT_BYTES[(match.group(2) or "").lower()]
    except (InvalidOperation, KeyError) as exc:
        raise ValueError(f"invalid {field}") from exc
    if not amount.is_finite() or amount < 1 or amount > 2**63-1 or amount != amount.to_integral_value():
        raise ValueError(f"{field} must be a positive whole byte count within int64")
    return int(amount)


def validate_memory(value: Any, *, field: str = "worker_memory") -> str:
    """Return a normalized docker ``--memory`` string after validating it."""
    text = str(value or "").strip()
    if not text:
        raise ValueError(f"{field} must be a positive size like 2g")
    # Reject bare percentages / empty units that docker would misread.
    parse_byte_size(text, field=field)
    return text.lower().replace(" ", "")


def validate_cpus(value: Any, *, field: str = "worker_cpus") -> str:
    if isinstance(value, bool):
        raise ValueError(f"{field} must be a positive number")
    try:
        amount = float(value)
    except (TypeError, ValueError, OverflowError) as exc:
        raise ValueError(f"{field} must be a positive number") from exc
    if not math.isfinite(amount) or amount <= 0:
        raise ValueError(f"{field} must be finite and > 0")
    return str(int(amount)) if amount.is_integer() else str(amount)


def validate_pids_limit(value: Any, *, field: str = "worker_pids_limit") -> int:
    if isinstance(value, bool) or value is None:
        raise ValueError(f"{field} must be a positive integer")
    try:
        n = int(value)
    except (TypeError, ValueError) as exc:
        raise ValueError(f"{field} must be a positive integer") from exc
    if n <= 0 or str(value).strip() != str(n):
        raise ValueError(f"{field} must be a positive integer")
    return n


def _env_first(*names: str) -> Optional[str]:
    for name in names:
        raw = os.environ.get(name)
        if raw is not None and str(raw).strip():
            return str(raw).strip()
    return None


def resolve_worker_resource_limits(
    *,
    image: str | None = None,
    memory: Any = None,
    cpus: Any = None,
    pids_limit: Any = None,
    output_limit: Any = None,
    disk_limit: Any = None,
    config: Mapping[str, Any] | None = None,
) -> WorkerResourceLimits:
    """Resolve limits with precedence: explicit > config map > env > image defaults.

    Env keys:
      MUTEKI_WORKER_MEMORY / MUTEKI_WORKER_CPUS / MUTEKI_WORKER_PIDS
      MUTEKI_WORKER_OUTPUT_LIMIT / MUTEKI_WORKER_DISK_LIMIT
    """
    base = defaults_for_image(image)
    cfg = config or {}

    mem = (
        memory
        if memory is not None
        else cfg.get("worker_memory")
        if cfg.get("worker_memory") is not None
        else _env_first("MUTEKI_WORKER_MEMORY")
    )
    cpu = (
        cpus
        if cpus is not None
        else cfg.get("worker_cpus")
        if cfg.get("worker_cpus") is not None
        else _env_first("MUTEKI_WORKER_CPUS")
    )
    pids = (
        pids_limit
        if pids_limit is not None
        else cfg.get("worker_pids_limit")
        if cfg.get("worker_pids_limit") is not None
        else _env_first("MUTEKI_WORKER_PIDS", "MUTEKI_WORKER_PIDS_LIMIT")
    )
    out_lim = (
        output_limit
        if output_limit is not None
        else cfg.get("worker_output_limit")
        if cfg.get("worker_output_limit") is not None
        else _env_first("MUTEKI_WORKER_OUTPUT_LIMIT")
    )
    disk_lim = (
        disk_limit
        if disk_limit is not None
        else cfg.get("worker_disk_limit")
        if cfg.get("worker_disk_limit") is not None
        else _env_first("MUTEKI_WORKER_DISK_LIMIT")
    )

    return WorkerResourceLimits(
        memory=validate_memory(mem if mem is not None else base.memory),
        cpus=validate_cpus(cpu if cpu is not None else base.cpus),
        pids_limit=validate_pids_limit(
            pids if pids is not None else base.pids_limit),
        output_limit=validate_memory(
            out_lim if out_lim is not None else base.output_limit,
            field="worker_output_limit",
        ),
        disk_limit=validate_memory(
            disk_lim if disk_lim is not None else base.disk_limit,
            field="worker_disk_limit",
        ),
        profile=base.profile,
    )
