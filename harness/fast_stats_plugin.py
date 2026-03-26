"""
irradiate fast stats plugin -- trampoline-free stats collection.

Collects per-test function coverage using sys.monitoring (Python 3.12+)
or sys.settrace (Python 3.10-3.11) instead of the trampoline dispatcher.
Runs against original unmodified source -- no import hook, no trampoline overhead.

Produces the same stats.json format as the trampoline-based stats_plugin.

Usage: pytest --irradiate-fast-stats to enable.
"""

import json
import os
import sys

# Tool ID for sys.monitoring (3.12+). Use OPTIMIZER_ID (5) to minimize collision.
TOOL_ID = 5

# Populated from mutated_functions.json at configure time.
# Structure: {module_name: {qualname: func_key}}
_function_map = {}
_tracked_modules = set()


def _load_function_map():
    path = os.environ.get("IRRADIATE_FUNCTION_MAP", ".irradiate/mutated_functions.json")
    if not os.path.exists(path):
        return
    with open(path) as f:
        data = json.load(f)
    global _function_map, _tracked_modules
    _function_map = data
    _tracked_modules = set(data.keys())


def _derive_qualname(frame, code):
    """Derive a function's qualified name from the frame.

    On Python 3.11+, uses code.co_qualname directly.
    On Python 3.10, derives class context from frame.f_locals.
    """
    if hasattr(code, "co_qualname"):
        return code.co_qualname
    name = code.co_name
    self_val = frame.f_locals.get("self")
    if self_val is not None:
        return f"{type(self_val).__name__}.{name}"
    cls_val = frame.f_locals.get("cls")
    if cls_val is not None:
        return f"{cls_val.__name__}.{name}"
    return name


def pytest_addoption(parser):
    parser.addoption(
        "--irradiate-fast-stats",
        action="store_true",
        default=False,
        help="Enable irradiate fast stats collection (no trampoline)",
    )


def pytest_configure(config):
    if config.getoption("--irradiate-fast-stats", default=False):
        _load_function_map()
        config.pluginmanager.register(FastStatsPlugin(config), "irradiate_fast_stats")


class FastStatsPlugin:
    """Trampoline-free stats collection.

    Uses sys.monitoring (3.12+) or sys.settrace (3.10-3.11) to record
    which mutated functions each test calls.
    """

    def __init__(self, config):
        self.config = config
        self.tests_by_function = {}  # func_key -> set of test nodeids
        self.duration_by_test = {}  # test nodeid -> duration in seconds
        self._current_hits = set()
        self._recording = False
        self._use_monitoring = sys.version_info >= (3, 12)

    # --- lifecycle ---

    def pytest_sessionstart(self, session):
        if self._use_monitoring:
            self._setup_monitoring()
        else:
            self._setup_settrace()

    def _setup_monitoring(self):
        sys.monitoring.use_tool_id(TOOL_ID, "irradiate")
        sys.monitoring.register_callback(TOOL_ID, sys.monitoring.events.CALL, self._monitoring_callback)
        sys.monitoring.set_events(TOOL_ID, sys.monitoring.events.CALL)

    def _setup_settrace(self):
        self._orig_trace = sys.gettrace()
        sys.settrace(self._settrace_callback)

    # --- sys.monitoring callback (3.12+) ---

    def _monitoring_callback(self, code, instruction_offset, callable_obj, arg0):
        if not self._recording:
            return
        module = getattr(callable_obj, "__module__", None)
        if module is None or module not in _tracked_modules:
            return
        module_map = _function_map.get(module)
        if module_map is None:
            return
        qualname = getattr(callable_obj, "__qualname__", None)
        if qualname is None:
            return
        func_key = module_map.get(qualname)
        if func_key is not None:
            self._current_hits.add(func_key)

    # --- sys.settrace callback (3.10-3.11) ---

    def _settrace_callback(self, frame, event, arg):
        if event != "call":
            return None
        if not self._recording:
            return None
        module = frame.f_globals.get("__name__")
        if module is None or module not in _tracked_modules:
            return None
        module_map = _function_map.get(module)
        if module_map is None:
            return None
        qualname = _derive_qualname(frame, frame.f_code)
        func_key = module_map.get(qualname)
        if func_key is not None:
            self._current_hits.add(func_key)
        return None  # don't trace lines/returns within this frame

    # --- pytest hooks ---

    def pytest_runtest_setup(self, item):
        self._current_hits = set()
        self._recording = True

    def pytest_runtest_teardown(self, item, nextitem):
        self._recording = False
        for func_key in self._current_hits:
            if func_key not in self.tests_by_function:
                self.tests_by_function[func_key] = set()
            self.tests_by_function[func_key].add(item.nodeid)
        self._current_hits = set()

    def pytest_runtest_makereport(self, item, call):
        key = item.nodeid
        self.duration_by_test[key] = self.duration_by_test.get(key, 0.0) + call.duration

    def pytest_sessionfinish(self, session, exitstatus):
        # Cleanup tracing
        if self._use_monitoring:
            try:
                sys.monitoring.set_events(TOOL_ID, 0)
                sys.monitoring.free_tool_id(TOOL_ID)
            except (ValueError, RuntimeError):
                pass
        else:
            sys.settrace(self._orig_trace)

        output_path = os.environ.get("IRRADIATE_STATS_OUTPUT", ".irradiate/stats.json")
        os.makedirs(os.path.dirname(output_path), exist_ok=True)

        stats = {
            "tests_by_function": {k: sorted(v) for k, v in self.tests_by_function.items()},
            "duration_by_test": self.duration_by_test,
            "exit_status": exitstatus,
            "test_count": len(session.items),
            "fail_validated": None,  # fast stats does not do trampoline validation
        }

        with open(output_path, "w") as f:
            json.dump(stats, f, indent=2)
