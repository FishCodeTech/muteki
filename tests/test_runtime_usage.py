from __future__ import annotations

import pytest

from muteki.runtime.usage import (
    UsageMeasurement,
    UsageNotEstimable,
    UsageReport,
    UsageStatus,
)


def test_usage_report_is_exactly_axis_complete_and_pessimistic():
    report = UsageReport.from_observed_and_reservation(
        reserved={"tokens": 100, "wall_ms": 500},
        observed={"tokens": 30, "wall_ms": 120},
        complete_axes=frozenset({"wall_ms"}),
    )
    assert report.pessimistic_usage() == {"tokens": 100, "wall_ms": 120}
    assert report.has_unknown is False
    assert report.all_observed is False
    report.validate_reservation({"tokens": 100, "wall_ms": 500})
    with pytest.raises(ValueError, match="exact reservation"):
        report.validate_reservation({"tokens": 101, "wall_ms": 500})


def test_missing_provider_telemetry_is_unknown_and_charged_to_ceiling():
    report = UsageReport.from_observed_and_reservation(
        reserved={"cost_micro_usd": 900, "tokens": 100},
        observed={},
        complete_axes=frozenset(),
    )
    assert report.has_unknown is True
    assert report.pessimistic_usage() == {"cost_micro_usd": 900, "tokens": 100}


def test_unknown_without_finite_ceiling_is_not_estimable():
    with pytest.raises(UsageNotEstimable, match="finite reservation"):
        UsageMeasurement("tokens", UsageStatus.UNKNOWN, 0, None)


def test_usage_rejects_negative_missing_duplicate_or_unreserved_axes():
    with pytest.raises(ValueError, match="non-negative"):
        UsageMeasurement("tokens", UsageStatus.OBSERVED, -1, 10)
    with pytest.raises(ValueError, match="canonically sorted"):
        UsageReport(
            (
                UsageMeasurement("wall_ms", UsageStatus.OBSERVED, 1, 10),
                UsageMeasurement("tokens", UsageStatus.OBSERVED, 1, 10),
            )
        )
    with pytest.raises(ValueError, match="unreserved"):
        UsageReport.from_observed_and_reservation(
            reserved={"tokens": 10},
            observed={"tokens": 1, "wall_ms": 1},
            complete_axes=frozenset({"tokens", "wall_ms"}),
        )


@pytest.mark.parametrize("value", [True, "1", 1.0])
def test_usage_amounts_do_not_coerce_non_integer_values(value):
    with pytest.raises(ValueError, match="non-negative integer"):
        UsageMeasurement("tokens", UsageStatus.OBSERVED, value, 10)
    with pytest.raises(ValueError, match="non-negative integer"):
        UsageReport.from_observed_and_reservation(
            reserved={"tokens": value},
            observed={},
            complete_axes=frozenset(),
        )


def test_usage_axes_and_completeness_markers_do_not_coerce():
    with pytest.raises(ValueError, match="canonical string"):
        UsageMeasurement(1, UsageStatus.OBSERVED, 1, 10)
    with pytest.raises(ValueError, match="canonical string"):
        UsageMeasurement(" tokens", UsageStatus.OBSERVED, 1, 10)
    with pytest.raises(TypeError, match="frozenset"):
        UsageReport.from_observed_and_reservation(
            reserved={"tokens": 10},
            observed={"tokens": 1},
            complete_axes={"tokens"},
        )


def test_partial_overage_charges_observed_amount_above_ceiling():
    report = UsageReport.from_observed_and_reservation(
        reserved={"tokens": 10},
        observed={"tokens": 12},
        complete_axes=frozenset(),
    )
    assert report.pessimistic_usage() == {"tokens": 12}
