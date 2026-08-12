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


class EnvironmentAwareBaseline(unittest.TestCase):
    """The floor must depend on whether the machine HAS the hardware.

    scx_perf is a PMU abstraction layer whose tests self-skip without a PMU, so
    a single floor means two different things on two machines -- the same class
    of bug as a lint set that floats per host. Both numbers stay committed: the
    PMU floor is still enforced where a PMU exists, so the PMU-only paths do not
    stop being measured anywhere.
    """

    def _csv(self, body: str) -> Path:
        import tempfile

        handle = tempfile.NamedTemporaryFile(
            "w", suffix=".csv", delete=False, newline=""
        )
        handle.write(body)
        handle.close()
        self.addCleanup(lambda: Path(handle.name).unlink(missing_ok=True))
        return Path(handle.name)

    BOTH = (
        "crate,coverage_pct,coverage_pct_nopmu\n"
        "scx_simulator,73.5,\n"
        "scx_perf,78.4,32.5\n"
    )

    def test_pmu_host_gets_the_hardware_floor(self):
        got = cr.read_baseline(self._csv(self.BOTH), has_pmu=True)
        self.assertEqual(got["scx_perf"], 78.4)

    def test_nopmu_host_gets_the_reachable_floor(self):
        got = cr.read_baseline(self._csv(self.BOTH), has_pmu=False)
        self.assertEqual(got["scx_perf"], 32.5)

    def test_blank_nopmu_cell_means_hardware_independent(self):
        """A crate with no nopmu entry is held to the SAME floor everywhere."""
        with_pmu = cr.read_baseline(self._csv(self.BOTH), has_pmu=True)
        without = cr.read_baseline(self._csv(self.BOTH), has_pmu=False)
        self.assertEqual(with_pmu["scx_simulator"], 73.5)
        self.assertEqual(without["scx_simulator"], 73.5)

    def test_legacy_single_column_csv_still_reads(self):
        """A CSV predating the nopmu column must not break either environment."""
        legacy = self._csv("crate,coverage_pct\nscx_perf,78.4\n")
        self.assertEqual(cr.read_baseline(legacy, has_pmu=True)["scx_perf"], 78.4)
        self.assertEqual(cr.read_baseline(legacy, has_pmu=False)["scx_perf"], 78.4)

    def test_probe_returns_a_bool_and_does_not_raise(self):
        self.assertIsInstance(cr.pmu_available(), bool)


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
