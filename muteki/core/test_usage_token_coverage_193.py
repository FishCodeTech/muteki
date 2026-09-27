"""#193: missing token fields must not aggregate into definitive zeros."""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from muteki.core.usage import UsageStore, normalize
from muteki.conversation.manager import _lightweight_statistics, _usage_token_coverage


class AggregateMissingVsZeroTests(unittest.TestCase):
    def _store(self) -> tuple[UsageStore, tempfile.TemporaryDirectory]:
        tmp = tempfile.TemporaryDirectory()
        return UsageStore(Path(tmp.name)), tmp

    def test_context_only_totals_stay_missing(self) -> None:
        store, tmp = self._store()
        try:
            store.record({"source": "context-only"}, identity="a", thread_id="t1")
            totals = store.query(thread_id="t1")["totals"]
            self.assertEqual(totals["records"], 1)
            self.assertEqual(totals["quality"]["missing"], 1)
            self.assertIsNone(totals["input_tokens"])
            self.assertIsNone(totals["output_tokens"])
            self.assertIsNone(totals["total_tokens"])
            self.assertEqual(totals["token_coverage"], "missing")
            self.assertEqual(totals["observed"]["input_tokens"], 0)
            self.assertEqual(totals["observed"]["output_tokens"], 0)
        finally:
            tmp.cleanup()

    def test_explicit_zero_remains_zero(self) -> None:
        store, tmp = self._store()
        try:
            store.record(
                {"input_tokens": 0, "output_tokens": 0, "source": "runtime"},
                identity="b",
                thread_id="t1",
            )
            totals = store.query(thread_id="t1")["totals"]
            self.assertEqual(totals["input_tokens"], 0)
            self.assertEqual(totals["output_tokens"], 0)
            self.assertEqual(totals["total_tokens"], 0)
            self.assertEqual(totals["quality"]["reported"], 1)
            self.assertEqual(totals["token_coverage"], "complete")
            self.assertEqual(totals["observed"]["input_tokens"], 1)
        finally:
            tmp.cleanup()

    def test_partial_input_only_no_fake_total(self) -> None:
        store, tmp = self._store()
        try:
            store.record({"input_tokens": 12, "source": "runtime"}, identity="c", thread_id="t1")
            totals = store.query(thread_id="t1")["totals"]
            self.assertEqual(totals["input_tokens"], 12)
            self.assertIsNone(totals["output_tokens"])
            self.assertIsNone(totals["total_tokens"])
            self.assertEqual(totals["token_coverage"], "partial")
            self.assertEqual(totals["quality"]["partial"], 1)
        finally:
            tmp.cleanup()

    def test_mixed_reported_and_missing_is_partial(self) -> None:
        store, tmp = self._store()
        try:
            store.record(
                {"input_tokens": 10, "output_tokens": 4, "source": "runtime"},
                identity="d",
                thread_id="t1",
            )
            store.record({"source": "context-only"}, identity="e", thread_id="t1")
            totals = store.query(thread_id="t1")["totals"]
            self.assertEqual(totals["input_tokens"], 10)
            self.assertEqual(totals["output_tokens"], 4)
            self.assertEqual(totals["total_tokens"], 14)
            self.assertEqual(totals["token_coverage"], "partial")
            self.assertEqual(totals["quality"]["reported"], 1)
            self.assertEqual(totals["quality"]["missing"], 1)
        finally:
            tmp.cleanup()

    def test_empty_query_missing(self) -> None:
        store, tmp = self._store()
        try:
            totals = store.query(thread_id="missing")["totals"]
            self.assertEqual(totals["records"], 0)
            self.assertIsNone(totals["input_tokens"])
            self.assertEqual(totals["token_coverage"], "missing")
        finally:
            tmp.cleanup()

    def test_normalize_context_only_quality(self) -> None:
        row = normalize({"source": "context-only"})
        self.assertIsNone(row["input_tokens"])
        self.assertEqual(row["quality"], "missing")


class StatisticsCoverageTests(unittest.TestCase):
    def test_empty_usage_statistics_missing(self) -> None:
        stats = _lightweight_statistics([], {})
        self.assertEqual(stats["token_coverage"], "missing")
        self.assertNotIn("input_tokens", stats)
        self.assertNotIn("output_tokens", stats)

    def test_context_only_ledger_statistics_missing(self) -> None:
        usage = {
            "input_tokens": None,
            "output_tokens": None,
            "total_tokens": None,
            "records": 1,
            "quality": {"reported": 0, "estimated": 0, "partial": 0, "missing": 1},
            "observed": {"input_tokens": 0, "output_tokens": 0},
            "token_coverage": "missing",
        }
        stats = _lightweight_statistics([], usage)
        self.assertEqual(stats["token_coverage"], "missing")
        self.assertNotIn("input_tokens", stats)
        self.assertNotIn("output_tokens", stats)

    def test_explicit_zero_statistics(self) -> None:
        usage = {
            "input_tokens": 0,
            "output_tokens": 0,
            "total_tokens": 0,
            "records": 1,
            "quality": {"reported": 1, "estimated": 0, "partial": 0, "missing": 0},
            "observed": {"input_tokens": 1, "output_tokens": 1},
            "token_coverage": "complete",
        }
        stats = _lightweight_statistics([], usage)
        self.assertEqual(stats["token_coverage"], "complete")
        self.assertEqual(stats["input_tokens"], 0)
        self.assertEqual(stats["output_tokens"], 0)

    def test_legacy_coerced_zeros_with_all_missing_quality(self) -> None:
        usage = {
            "input_tokens": 0,
            "output_tokens": 0,
            "total_tokens": 0,
            "records": 1,
            "quality": {"reported": 0, "estimated": 0, "partial": 0, "missing": 1},
        }
        self.assertEqual(_usage_token_coverage(usage), "missing")
        stats = _lightweight_statistics([], usage)
        self.assertEqual(stats["token_coverage"], "missing")
        self.assertNotIn("input_tokens", stats)

    def test_partial_statistics(self) -> None:
        usage = {
            "input_tokens": 8,
            "output_tokens": None,
            "total_tokens": None,
            "records": 1,
            "quality": {"reported": 0, "estimated": 0, "partial": 1, "missing": 0},
            "observed": {"input_tokens": 1, "output_tokens": 0},
            "token_coverage": "partial",
        }
        stats = _lightweight_statistics([], usage)
        self.assertEqual(stats["token_coverage"], "partial")
        self.assertEqual(stats["input_tokens"], 8)
        self.assertNotIn("output_tokens", stats)


if __name__ == "__main__":
    unittest.main()
