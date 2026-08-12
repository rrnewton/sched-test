#!/usr/bin/env python3
"""Unit tests for scripts/coverage_ratchet.py (run by validate.sh)."""

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import coverage_ratchet as cr  # noqa: E402  (path inserted above)

CRATES = cr.LIBRARY_CRATES


def _at(pct_count: int, covered: int) -> dict[str, tuple[int, int]]:
    """Same (count, covered) for every guarded crate."""
    return {crate: (pct_count, covered) for crate in CRATES}


class ParseTotalLines(unittest.TestCase):
    def test_extracts_count_and_covered(self) -> None:
        payload = '{"data":[{"totals":{"lines":{"count":200,"covered":150,"percent":75.0}}}]}'
        self.assertEqual(cr.parse_total_lines(payload), (200, 150))


class LinePct(unittest.TestCase):
    def test_normal_ratio(self) -> None:
        self.assertAlmostEqual(cr.line_pct(200, 150), 75.0)

    def test_zero_lines_is_zero_not_division_error(self) -> None:
        self.assertEqual(cr.line_pct(0, 0), 0.0)


class GateFailures(unittest.TestCase):
    def test_at_baseline_passes(self) -> None:
        measured = _at(1000, 800)  # 80.0%
        baseline = {c: 80.0 for c in CRATES}
        self.assertEqual(cr.gate_failures(measured, baseline, 0.5), [])

    def test_within_epsilon_passes(self) -> None:
        measured = _at(1000, 797)  # 79.7% vs floor 79.5
        baseline = {c: 80.0 for c in CRATES}
        self.assertEqual(cr.gate_failures(measured, baseline, 0.5), [])

    def test_just_below_floor_fails(self) -> None:
        measured = _at(1000, 794)  # 79.4% vs floor 79.5
        baseline = {c: 80.0 for c in CRATES}
        failures = cr.gate_failures(measured, baseline, 0.5)
        self.assertEqual(len(failures), len(CRATES))
        self.assertTrue(all("< baseline" in m for m in failures))

    def test_zero_lines_fails_even_with_baseline(self) -> None:
        measured = _at(0, 0)
        baseline = {c: 80.0 for c in CRATES}
        failures = cr.gate_failures(measured, baseline, 0.5)
        self.assertTrue(all("0 instrumentable" in m for m in failures))

    def test_missing_baseline_fails(self) -> None:
        failures = cr.gate_failures(_at(1000, 900), {}, 0.5)
        self.assertTrue(all("no committed baseline" in m for m in failures))

    def test_missing_measurement_fails(self) -> None:
        baseline = {c: 80.0 for c in CRATES}
        failures = cr.gate_failures({}, baseline, 0.5)
        self.assertTrue(all("not measured" in m for m in failures))


class RaiseOnlyBaseline(unittest.TestCase):
    def test_raises_when_higher(self) -> None:
        measured = _at(1000, 900)  # 90.0%
        prior = {c: 80.0 for c in CRATES}
        new, refused = cr.raise_only_baseline(measured, prior, allow_lower=False)
        self.assertEqual(refused, [])
        self.assertTrue(all(v == 90.0 for v in new.values()))

    def test_refuses_lowering_and_keeps_prior(self) -> None:
        measured = _at(1000, 700)  # 70.0%
        prior = {c: 80.0 for c in CRATES}
        new, refused = cr.raise_only_baseline(measured, prior, allow_lower=False)
        self.assertEqual(len(refused), len(CRATES))
        self.assertTrue(all(v == 80.0 for v in new.values()))

    def test_allow_lower_permits_lowering(self) -> None:
        measured = _at(1000, 700)  # 70.0%
        prior = {c: 80.0 for c in CRATES}
        new, refused = cr.raise_only_baseline(measured, prior, allow_lower=True)
        self.assertEqual(refused, [])
        self.assertTrue(all(v == 70.0 for v in new.values()))


if __name__ == "__main__":
    unittest.main()
