"""
irradiate stats plugin — pytest plugin that records which tests call
which trampolined functions when active_mutant == "stats".

Usage: pytest --irradiate-stats to enable.
"""

import json
import os

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
        if call.when == "call":
            self.duration_by_test[item.nodeid] = call.duration

    def pytest_sessionfinish(self, session, exitstatus):
        # Write stats to file
        output_path = os.environ.get("IRRADIATE_STATS_OUTPUT", ".irradiate/stats.json")
        os.makedirs(os.path.dirname(output_path), exist_ok=True)

        stats = {
            "tests_by_function": {k: sorted(v) for k, v in self.tests_by_function.items()},
            "duration_by_test": self.duration_by_test,
        }

        with open(output_path, "w") as f:
            json.dump(stats, f, indent=2)
