from __future__ import annotations

from decimal import Decimal, ROUND_CEILING, ROUND_HALF_UP
from typing import Any, Mapping, Optional

from pydantic import Field

from muteki.platform.contracts.base import ContractModel
from muteki.src.store import SrcStoreError

CALCULATOR_VERSION = "cvss-3.1-official-v1"
BASE_ORDER = ("AV", "AC", "PR", "UI", "S", "C", "I", "A")
TEMPORAL_ORDER = ("E", "RL", "RC")
ENVIRONMENTAL_ORDER = (
    "CR", "IR", "AR", "MAV", "MAC", "MPR", "MUI", "MS", "MC", "MI", "MA",
)
ORDER = (*BASE_ORDER, *TEMPORAL_ORDER, *ENVIRONMENTAL_ORDER)

VALUES = {
    "AV": {"N": 0.85, "A": 0.62, "L": 0.55, "P": 0.2},
    "AC": {"L": 0.77, "H": 0.44},
    "PR": {"N": 0.85, "L": 0.62, "H": 0.27},
    "UI": {"N": 0.85, "R": 0.62},
    "S": {"U": 1.0, "C": 1.0},
    "C": {"H": 0.56, "L": 0.22, "N": 0.0},
    "I": {"H": 0.56, "L": 0.22, "N": 0.0},
    "A": {"H": 0.56, "L": 0.22, "N": 0.0},
    "E": {"X": 1.0, "U": 0.91, "P": 0.94, "F": 0.97, "H": 1.0},
    "RL": {"X": 1.0, "O": 0.95, "T": 0.96, "W": 0.97, "U": 1.0},
    "RC": {"X": 1.0, "U": 0.92, "R": 0.96, "C": 1.0},
    "CR": {"X": 1.0, "L": 0.5, "M": 1.0, "H": 1.5},
    "IR": {"X": 1.0, "L": 0.5, "M": 1.0, "H": 1.5},
    "AR": {"X": 1.0, "L": 0.5, "M": 1.0, "H": 1.5},
    "MAV": {"X": 0.85, "N": 0.85, "A": 0.62, "L": 0.55, "P": 0.2},
    "MAC": {"X": 0.77, "L": 0.77, "H": 0.44},
    "MPR": {"X": 0.85, "N": 0.85, "L": 0.62, "H": 0.27},
    "MUI": {"X": 0.85, "N": 0.85, "R": 0.62},
    "MS": {"X": 1.0, "U": 1.0, "C": 1.0},
    "MC": {"X": 0.56, "N": 0.0, "L": 0.22, "H": 0.56},
    "MI": {"X": 0.56, "N": 0.0, "L": 0.22, "H": 0.56},
    "MA": {"X": 0.56, "N": 0.0, "L": 0.22, "H": 0.56},
}

PR_CHANGED = {"N": 0.85, "L": 0.68, "H": 0.5}
MPR_CHANGED = {"X": 0.85, "N": 0.85, "L": 0.68, "H": 0.5}
SEVERITIES = (
    (Decimal("4.0"), "LOW"),
    (Decimal("7.0"), "MEDIUM"),
    (Decimal("9.0"), "HIGH"),
    (Decimal("10.0"), "CRITICAL"),
)


def _error(code: str, message: str, detail: Optional[dict[str, Any]] = None) -> SrcStoreError:
    return SrcStoreError(code, message, detail=detail or {})


def roundup(value: Decimal | float | int) -> Decimal:
    number = Decimal(str(value))
    int_input = int((number * Decimal("100000")).to_integral_value(rounding=ROUND_HALF_UP))
    if int_input % 10000 == 0:
        return (Decimal(int_input) / Decimal("100000")).quantize(Decimal("0.1"))
    return (Decimal((int_input // 10000) + 1) / Decimal("10")).quantize(Decimal("0.1"))


def parse_vector(vector: str) -> dict[str, str]:
    text = str(vector or "").strip()
    if not text.startswith("CVSS:3.1/"):
        raise _error("cvss_vector_invalid", "CVSS vector must start with CVSS:3.1/")
    metrics: dict[str, str] = {}
    for part in text.removeprefix("CVSS:3.1/").split("/"):
        pieces = part.split(":")
        if len(pieces) != 2 or pieces[0] not in VALUES or pieces[0] in metrics:
            raise _error("cvss_vector_invalid", f"invalid or duplicate CVSS metric: {part}")
        name, value = pieces
        if value not in VALUES[name]:
            raise _error("cvss_vector_invalid", f"invalid value {value} for {name}")
        metrics[name] = value
    missing = [name for name in BASE_ORDER if name not in metrics]
    if missing:
        raise _error("cvss_vector_incomplete", f"missing base metrics: {', '.join(missing)}")
    return metrics


def canonical_vector(metrics: Mapping[str, str]) -> str:
    return "CVSS:3.1/" + "/".join(f"{name}:{metrics[name]}" for name in ORDER if name in metrics)


def _pr(metrics: Mapping[str, str], modified: bool = False) -> float:
    scope = metrics.get("MS", "X") if modified else metrics["S"]
    if scope == "X":
        scope = metrics["S"]
    if modified:
        value = metrics["MPR"]
        if value == "X":
            return PR_CHANGED[metrics["PR"]] if scope == "C" else VALUES["PR"][metrics["PR"]]
        return MPR_CHANGED[value] if scope == "C" else VALUES["MPR"][value]
    return PR_CHANGED[metrics["PR"]] if scope == "C" else VALUES["PR"][metrics["PR"]]


def _exploitability(metrics: Mapping[str, str], modified: bool = False) -> Decimal:
    if modified:
        av = VALUES["MAV"][metrics["MAV"]] if metrics["MAV"] != "X" else VALUES["AV"][metrics["AV"]]
        ac = VALUES["MAC"][metrics["MAC"]] if metrics["MAC"] != "X" else VALUES["AC"][metrics["AC"]]
        ui = VALUES["MUI"][metrics["MUI"]] if metrics["MUI"] != "X" else VALUES["UI"][metrics["UI"]]
        value = 8.22 * av * ac * _pr(metrics, modified=True) * ui
    else:
        value = 8.22 * VALUES["AV"][metrics["AV"]] * VALUES["AC"][metrics["AC"]]
        value *= _pr(metrics) * VALUES["UI"][metrics["UI"]]
    return Decimal(str(value))


def _temporal_factor(metrics: Mapping[str, str]) -> Decimal:
    return Decimal(str(VALUES["E"][metrics.get("E", "X")] * VALUES["RL"][metrics.get("RL", "X")] * VALUES["RC"][metrics.get("RC", "X")]))


def base_score(metrics: Mapping[str, str]) -> Decimal:
    isc_base = 1 - (1 - VALUES["C"][metrics["C"]]) * (1 - VALUES["I"][metrics["I"]]) * (1 - VALUES["A"][metrics["A"]])
    if metrics["S"] == "U":
        impact = Decimal(str(6.42 * isc_base))
    else:
        impact = Decimal(str(7.52 * (isc_base - 0.029) - 3.25 * (isc_base - 0.02) ** 15))
    if impact <= 0:
        return Decimal("0.0")
    exploitability = _exploitability(metrics)
    if metrics["S"] == "U":
        return roundup(min(impact + exploitability, Decimal("10")))
    return roundup(min(Decimal("1.08") * (impact + exploitability), Decimal("10")))


def temporal_score(metrics: Mapping[str, str]) -> Decimal:
    return roundup(base_score(metrics) * _temporal_factor(metrics))


def environmental_score(metrics: Mapping[str, str]) -> Decimal:
    metrics = {
        **{name: "X" for name in (*TEMPORAL_ORDER, *ENVIRONMENTAL_ORDER)},
        **metrics,
    }
    cr = VALUES["CR"].get(metrics.get("CR", "X"), 1.0)
    ir = VALUES["IR"].get(metrics.get("IR", "X"), 1.0)
    ar = VALUES["AR"].get(metrics.get("AR", "X"), 1.0)
    mc = VALUES["MC"][metrics["MC"]] if metrics["MC"] != "X" else VALUES["C"][metrics["C"]]
    mi = VALUES["MI"][metrics["MI"]] if metrics["MI"] != "X" else VALUES["I"][metrics["I"]]
    ma = VALUES["MA"][metrics["MA"]] if metrics["MA"] != "X" else VALUES["A"][metrics["A"]]
    modified_isc_base = min(1 - (1 - mc * cr) * (1 - mi * ir) * (1 - ma * ar), 0.915)
    scope = metrics.get("MS", "X")
    if scope == "C" or (scope == "X" and metrics["S"] == "C"):
        modified_impact = Decimal(str(7.52 * (modified_isc_base - 0.029) - 3.25 * (modified_isc_base * 0.9731 - 0.02) ** 13))
    else:
        modified_impact = Decimal(str(6.42 * modified_isc_base))
    if modified_impact <= 0:
        return Decimal("0.0")
    modified_exploitability = _exploitability(metrics, modified=True)
    if scope == "C" or (scope == "X" and metrics["S"] == "C"):
        interim = roundup(min(Decimal("1.08") * (modified_impact + modified_exploitability), Decimal("10")))
    else:
        interim = roundup(min(modified_impact + modified_exploitability, Decimal("10")))
    return roundup(interim * _temporal_factor(metrics))


def severity(score: Decimal | float | int) -> str:
    value = Decimal(str(score))
    if value < 0 or value > 10:
        raise _error("cvss_score_invalid", "CVSS score must be between 0.0 and 10.0")
    if value == 0:
        return "NONE"
    for threshold, label in SEVERITIES:
        if value < threshold:
            return label
    return "CRITICAL"


class CVSSAssessment(ContractModel):
    vector: str
    base_score: Decimal
    temporal_score: Decimal
    environmental_score: Decimal
    base_severity: str
    temporal_severity: str
    environmental_severity: str
    calculator_version: str = CALCULATOR_VERSION
    metric_reasons: dict[str, dict[str, Any]] = Field(default_factory=dict)


def evaluate_cvss(vector: str, reasons: Optional[Mapping[str, Any]] = None) -> CVSSAssessment:
    metrics = parse_vector(vector)
    supplied_reasons = dict(reasons or {})
    unknown_reasons = set(supplied_reasons) - set(VALUES)
    if unknown_reasons:
        raise _error("cvss_reason_invalid", f"reasons reference unknown metrics: {sorted(unknown_reasons)}")
    normalized: dict[str, dict[str, Any]] = {}
    for name in BASE_ORDER:
        raw = supplied_reasons.get(name)
        if isinstance(raw, str):
            raw = {"reason": raw}
        if not isinstance(raw, Mapping) or not str(raw.get("reason") or "").strip():
            raise _error("cvss_reason_required", f"metric {name} needs an evidence-based reason")
        normalized[name] = {**dict(raw), "reason": str(raw["reason"]).strip()}
    for name in (*TEMPORAL_ORDER, *ENVIRONMENTAL_ORDER):
        raw = supplied_reasons.get(name)
        if raw is None:
            continue
        if isinstance(raw, str):
            raw = {"reason": raw}
        if not isinstance(raw, Mapping) or not str(raw.get("reason") or "").strip():
            raise _error("cvss_reason_required", f"metric {name} needs an evidence-based reason")
        normalized[name] = {**dict(raw), "reason": str(raw["reason"]).strip()}
    resolved = dict(metrics)
    for name in (*TEMPORAL_ORDER, *ENVIRONMENTAL_ORDER):
        resolved.setdefault(name, "X")
    base = base_score(resolved)
    temporal = temporal_score(resolved)
    environmental = environmental_score(resolved)
    return CVSSAssessment(
        vector=canonical_vector(metrics), base_score=base,
        temporal_score=temporal, environmental_score=environmental,
        base_severity=severity(base), temporal_severity=severity(temporal),
        environmental_severity=severity(environmental), metric_reasons=normalized,
    )


__all__ = [
    "CALCULATOR_VERSION", "CVSSAssessment", "base_score", "canonical_vector",
    "environmental_score", "evaluate_cvss", "parse_vector", "roundup",
    "severity", "temporal_score",
]
