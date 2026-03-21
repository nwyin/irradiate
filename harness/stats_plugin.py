"""
irradiate stats plugin — pytest plugin that records which tests call
which trampolined functions when active_mutant == "stats".

Also performs in-process fail-path validation at session end, so the
pipeline needs only a single subprocess for the entire pre-mutation phase.

Usage: pytest --irradiate-stats to enable.
"""

import json
import os
import sys

import irradiate_harness


def pytest_addoption(parser):
    parser.addoption(
        "--irradiate-stats",
        action="store_true",
        default=False,
        help="Enable irradiate stats collection",
    )


def pytest_configure(config):
    if config.getoption("--irradiate-stats", default=False):
        config.pluginmanager.register(IrradiateStatsPlugin(config), "irradiate_stats")


class IrradiateStatsPlugin:
    def __init__(self, config):
        self.config = config
        self.tests_by_function = {}  # func_key -> set of test nodeids
        self.duration_by_test = {}  # test nodeid -> duration in seconds

    def pytest_runtest_setup(self, item):
        # Set stats mode before each test
        irradiate_harness.active_mutant = "stats"
        irradiate_harness._hits.clear()

    def pytest_runtest_teardown(self, item, nextitem):
        # Collect which functions were hit during this test
        hits = irradiate_harness.get_hits()
        for func_key in hits:
            if func_key not in self.tests_by_function:
                self.tests_by_function[func_key] = set()
            self.tests_by_function[func_key].add(item.nodeid)
        irradiate_harness.active_mutant = None

    def pytest_runtest_makereport(self, item, call):
        # Accumulate setup + call + teardown durations for accurate timing
        key = item.nodeid
        self.duration_by_test[key] = self.duration_by_test.get(key, 0.0) + call.duration

    def _verify_fail_path(self):
        """In-process fail probe: find a trampolined module and verify the fail path fires."""
        if not self.tests_by_function:
            return False
        for mod in sys.modules.values():
            trampoline = getattr(mod, "_irradiate_trampoline", None)
            if trampoline is not None:
                break
        else:
            return False

        def dummy():
            pass

        dummy.__module__ = "probe"
        dummy.__name__ = "probe"
        irradiate_harness.active_mutant = "fail"
        try:
            trampoline(dummy, {}, (), {})
            return False
        except irradiate_harness.ProgrammaticFailException:
            return True
        except Exception:
            return False
        finally:
            irradiate_harness.active_mutant = None

    def pytest_sessionfinish(self, session, exitstatus):
        output_path = os.environ.get("IRRADIATE_STATS_OUTPUT", ".irradiate/stats.json")
        os.makedirs(os.path.dirname(output_path), exist_ok=True)

        fail_validated = self._verify_fail_path()

        stats = {
            "tests_by_function": {k: sorted(v) for k, v in self.tests_by_function.items()},
            "duration_by_test": self.duration_by_test,
            "exit_status": exitstatus,
            "test_count": len(session.items),
            "fail_validated": fail_validated,
        }

        with open(output_path, "w") as f:
            json.dump(stats, f, indent=2)
