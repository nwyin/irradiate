"""
Tests for harness/stats_plugin.py — pytest_addoption, pytest_configure,
IrradiateStatsPlugin.__init__, and all five hook methods.

Uses mock objects (not pytester) to keep tests fast and dependency-free.
conftest.py adds the repo root to sys.path so `import harness` resolves.
"""

import json
import os
import sys
from unittest.mock import MagicMock, patch

import pytest

# Register harness as irradiate_harness BEFORE importing stats_plugin,
# since stats_plugin.py does `import irradiate_harness` at module load time.
import harness as _harness_module  # noqa: E402 (must precede stats_plugin import)

sys.modules["irradiate_harness"] = _harness_module

from harness import stats_plugin  # noqa: E402
from harness.stats_plugin import IrradiateStatsPlugin  # noqa: E402

# Convenience alias — the same object that stats_plugin refers to as irradiate_harness
_ih = _harness_module


# ---------------------------------------------------------------------------
# Shared fixtures
# ---------------------------------------------------------------------------


@pytest.fixture(autouse=True)
def reset_harness_state():
    """Reset irradiate_harness global state before and after every test."""
    _ih._hits.clear()
    _ih.active_mutant = None
    yield
    _ih._hits.clear()
    _ih.active_mutant = None


@pytest.fixture
def plugin():
    """A fresh IrradiateStatsPlugin with a mock config."""
    return IrradiateStatsPlugin(MagicMock())


# ---------------------------------------------------------------------------
# pytest_addoption: --irradiate-stats flag must be registered exactly once
# ---------------------------------------------------------------------------


def test_addoption_registers_irradiate_stats_flag():
    """pytest_addoption must call parser.addoption with '--irradiate-stats'."""
    parser = MagicMock()
    stats_plugin.pytest_addoption(parser)
    parser.addoption.assert_called_once()
    assert parser.addoption.call_args[0][0] == "--irradiate-stats"


def test_addoption_uses_store_true_action():
    """The flag must be a boolean toggle (action='store_true'), not value-taking."""
    parser = MagicMock()
    stats_plugin.pytest_addoption(parser)
    kwargs = parser.addoption.call_args[1]
    assert kwargs.get("action") == "store_true"


def test_addoption_default_is_false():
    """The default value must be False so the flag is opt-in."""
    parser = MagicMock()
    stats_plugin.pytest_addoption(parser)
    kwargs = parser.addoption.call_args[1]
    assert kwargs.get("default") is False


# ---------------------------------------------------------------------------
# pytest_configure: plugin registered only when flag is set
# ---------------------------------------------------------------------------


def test_configure_registers_plugin_when_flag_is_set():
    """IrradiateStatsPlugin must be registered when --irradiate-stats is True."""
    config = MagicMock()
    config.getoption.return_value = True
    stats_plugin.pytest_configure(config)
    config.pluginmanager.register.assert_called_once()
    registered = config.pluginmanager.register.call_args[0][0]
    assert isinstance(registered, IrradiateStatsPlugin)


def test_configure_registers_under_name_irradiate_stats():
    """The plugin must be registered under the name 'irradiate_stats'."""
    config = MagicMock()
    config.getoption.return_value = True
    stats_plugin.pytest_configure(config)
    name_arg = config.pluginmanager.register.call_args[0][1]
    assert name_arg == "irradiate_stats"


def test_configure_does_not_register_when_flag_is_false():
    """Plugin must NOT be registered when --irradiate-stats is absent/False."""
    config = MagicMock()
    config.getoption.return_value = False
    stats_plugin.pytest_configure(config)
    config.pluginmanager.register.assert_not_called()


# ---------------------------------------------------------------------------
# IrradiateStatsPlugin.__init__: internal state initialization
# ---------------------------------------------------------------------------


def test_init_tests_by_function_starts_empty(plugin):
    assert plugin.tests_by_function == {}


def test_init_duration_by_test_starts_empty(plugin):
    assert plugin.duration_by_test == {}


def test_init_stores_config_reference():
    """Config object passed to __init__ must be stored for later access."""
    cfg = MagicMock()
    p = IrradiateStatsPlugin(cfg)
    assert p.config is cfg


# ---------------------------------------------------------------------------
# pytest_runtest_setup: active_mutant set to "stats", _hits cleared
# ---------------------------------------------------------------------------


def test_runtest_setup_sets_active_mutant_to_stats(plugin):
    plugin.pytest_runtest_setup(MagicMock())
    assert _ih.active_mutant == "stats"


def test_runtest_setup_clears_stale_hits(plugin):
    """Hits from any previous test must be wiped before the new test begins."""
    _ih._hits.add("stale.func.key")
    plugin.pytest_runtest_setup(MagicMock())
    assert len(_ih._hits) == 0


# ---------------------------------------------------------------------------
# pytest_runtest_teardown: hits collected into tests_by_function, active_mutant reset
# ---------------------------------------------------------------------------


def test_runtest_teardown_records_hit_for_test(plugin):
    """A function hit during the test must appear in tests_by_function."""
    _ih._hits.add("module.x_add")
    item = MagicMock()
    item.nodeid = "tests/test_math.py::test_add"
    plugin.pytest_runtest_teardown(item, nextitem=None)
    assert item.nodeid in plugin.tests_by_function["module.x_add"]


def test_runtest_teardown_accumulates_multiple_tests_per_function(plugin):
    """Multiple tests exercising the same function all appear in its set."""
    item_a = MagicMock()
    item_a.nodeid = "tests/test_a.py::test_one"
    item_b = MagicMock()
    item_b.nodeid = "tests/test_b.py::test_two"

    _ih._hits.add("mod.x_func")
    plugin.pytest_runtest_teardown(item_a, nextitem=None)

    _ih._hits.add("mod.x_func")
    plugin.pytest_runtest_teardown(item_b, nextitem=None)

    assert plugin.tests_by_function["mod.x_func"] == {item_a.nodeid, item_b.nodeid}


def test_runtest_teardown_resets_active_mutant_to_none(plugin):
    """active_mutant must be cleared after teardown so production code is unaffected."""
    _ih.active_mutant = "stats"
    item = MagicMock()
    item.nodeid = "test_something"
    plugin.pytest_runtest_teardown(item, nextitem=None)
    assert _ih.active_mutant is None


def test_runtest_teardown_no_hits_leaves_tests_by_function_empty(plugin):
    """If the test called no trampolined functions, tests_by_function stays empty."""
    item = MagicMock()
    item.nodeid = "tests/test_x.py::test_nothing"
    plugin.pytest_runtest_teardown(item, nextitem=None)
    assert plugin.tests_by_function == {}


# ---------------------------------------------------------------------------
# pytest_runtest_makereport: duration recorded for 'call' phase only
# ---------------------------------------------------------------------------


def test_makereport_records_duration_on_call_phase(plugin):
    item = MagicMock()
    item.nodeid = "tests/test_foo.py::test_fast"
    call = MagicMock()
    call.when = "call"
    call.duration = 0.42
    plugin.pytest_runtest_makereport(item, call)
    assert plugin.duration_by_test[item.nodeid] == pytest.approx(0.42)


def test_makereport_ignores_setup_phase(plugin):
    """Duration must not be recorded for the setup phase."""
    item = MagicMock()
    item.nodeid = "tests/test_foo.py::test_setup_ignored"
    call = MagicMock()
    call.when = "setup"
    call.duration = 0.01
    plugin.pytest_runtest_makereport(item, call)
    assert item.nodeid not in plugin.duration_by_test


def test_makereport_ignores_teardown_phase(plugin):
    """Duration must not be recorded for the teardown phase."""
    item = MagicMock()
    item.nodeid = "tests/test_foo.py::test_teardown_ignored"
    call = MagicMock()
    call.when = "teardown"
    call.duration = 0.05
    plugin.pytest_runtest_makereport(item, call)
    assert item.nodeid not in plugin.duration_by_test


# ---------------------------------------------------------------------------
# pytest_sessionfinish: JSON stats file written with correct structure
# ---------------------------------------------------------------------------


def test_sessionfinish_writes_json_file(tmp_path, plugin):
    """pytest_sessionfinish must produce a JSON file at the configured path."""
    output_path = tmp_path / "stats.json"
    plugin.tests_by_function = {"mod.x_foo": {"test_a", "test_b"}}
    plugin.duration_by_test = {"test_a": 0.1}
    with patch.dict(os.environ, {"IRRADIATE_STATS_OUTPUT": str(output_path)}):
        plugin.pytest_sessionfinish(session=MagicMock(), exitstatus=0)
    assert output_path.exists()
    data = json.loads(output_path.read_text())
    assert "tests_by_function" in data
    assert "duration_by_test" in data


def test_sessionfinish_test_list_is_sorted(tmp_path, plugin):
    """Tests per function must be sorted for deterministic diffs."""
    output_path = tmp_path / "stats.json"
    plugin.tests_by_function = {"mod.x_foo": {"test_z", "test_a", "test_m"}}
    plugin.duration_by_test = {}
    with patch.dict(os.environ, {"IRRADIATE_STATS_OUTPUT": str(output_path)}):
        plugin.pytest_sessionfinish(session=MagicMock(), exitstatus=0)
    data = json.loads(output_path.read_text())
    tests = data["tests_by_function"]["mod.x_foo"]
    assert tests == sorted(tests)


def test_sessionfinish_duration_by_test_preserved(tmp_path, plugin):
    """duration_by_test values must be written verbatim to the output JSON."""
    output_path = tmp_path / "stats.json"
    plugin.tests_by_function = {}
    plugin.duration_by_test = {"test_fast": 0.01, "test_slow": 1.23}
    with patch.dict(os.environ, {"IRRADIATE_STATS_OUTPUT": str(output_path)}):
        plugin.pytest_sessionfinish(session=MagicMock(), exitstatus=0)
    data = json.loads(output_path.read_text())
    assert data["duration_by_test"]["test_fast"] == pytest.approx(0.01)
    assert data["duration_by_test"]["test_slow"] == pytest.approx(1.23)


def test_sessionfinish_creates_missing_parent_directories(tmp_path, plugin):
    """Parent directories must be created if they don't exist."""
    output_path = tmp_path / "nested" / "deep" / "stats.json"
    plugin.tests_by_function = {}
    plugin.duration_by_test = {}
    with patch.dict(os.environ, {"IRRADIATE_STATS_OUTPUT": str(output_path)}):
        plugin.pytest_sessionfinish(session=MagicMock(), exitstatus=0)
    assert output_path.exists()


def test_sessionfinish_empty_run_produces_valid_json(tmp_path, plugin):
    """A session with no trampolined calls must still produce valid JSON."""
    output_path = tmp_path / "stats.json"
    with patch.dict(os.environ, {"IRRADIATE_STATS_OUTPUT": str(output_path)}):
        plugin.pytest_sessionfinish(session=MagicMock(), exitstatus=0)
    data = json.loads(output_path.read_text())
    assert data["tests_by_function"] == {}
    assert data["duration_by_test"] == {}
