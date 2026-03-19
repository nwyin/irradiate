"""
irradiate pytest worker — connects to the orchestrator over a unix socket,
receives mutant assignments, runs pytest items directly, reports results.

Architecture: the IPC loop runs inside pytest_runtestloop so that all pytest
plugins are fully initialized when the worker executes selected items via
pytest's hook machinery.
"""

import json
import os
import socket
import sys
import time
import traceback


def send_message(sock, msg):
    data = json.dumps(msg) + "\n"
    sock.sendall(data.encode("utf-8"))


def recv_message(sock, buf):
    while b"\n" not in buf:
        chunk = sock.recv(4096)
        if not chunk:
            return None, buf
        buf += chunk
    line, _, buf = buf.partition(b"\n")
    return json.loads(line.decode("utf-8")), buf


def reports_indicate_failure(reports):
    """Return True if any pytest report represents a failed/error outcome."""
    return any(getattr(report, "failed", False) for report in reports)


class MutationWorkerPlugin:
    """
    pytest plugin that intercepts pytest_runtestloop to run the IPC loop.

    By running the IPC dispatch loop inside pytest_runtestloop, we ensure
    the session is fully initialized (stash keys set, capture manager active)
    when pytest's runtest hooks are invoked for each mutant.
    """

    def __init__(self, sock):
        self.sock = sock
        self.buf = b""
        self.items = {}  # nodeid -> Item
        self.item_order = {}  # nodeid -> collection index
        self.current_run_mutant = None
        self.current_run_nodeids = set()
        self.current_item_nodeid = None
        self.current_run_reports = []
        self._source_module_names = []  # modules loaded via MutantFinder
        self._module_snapshots = {}  # mod_name -> shallow copy of vars(mod)
        self.session_fixture_names = []  # populated by pytest_collection_finish

    def _detect_session_fixtures(self, session):
        """Check if any collected tests use session-scoped fixtures.

        Uses pytest's internal _fixturemanager and _arg2fixturedefs.
        These are underscore-prefixed but stable across pytest versions
        and used by major plugins (pytest-xdist, pytest-cov).
        Falls back gracefully if the attribute is unavailable (old pytest).
        """
        session_fixture_names = []
        fm = session.config.pluginmanager.get_plugin("funcmanage")
        if fm is None:
            fm = getattr(session, "_fixturemanager", None)
        if fm is not None:
            arg2fixturedefs = getattr(fm, "_arg2fixturedefs", None)
            if arg2fixturedefs is not None:
                for name, fixturedefs in arg2fixturedefs.items():
                    for fdef in fixturedefs:
                        if getattr(fdef, "scope", None) == "session":
                            session_fixture_names.append(name)
                            break  # one match per name is enough
        return session_fixture_names

    def pytest_collection_finish(self, session):
        for index, item in enumerate(session.items):
            self.items[item.nodeid] = item
            self.item_order[item.nodeid] = index
        self._identify_source_modules()
        self.session_fixture_names = self._detect_session_fixtures(session)
        if self.session_fixture_names:
            print(
                f"[irradiate] Session-scoped fixtures detected: {', '.join(self.session_fixture_names)}",
                file=sys.stderr,
            )

    def _identify_source_modules(self):
        """Detect which sys.modules entries were loaded via the MutantFinder import hook."""
        from irradiate_harness.import_hook import MutantFinder

        finder = None
        for hook in sys.meta_path:
            if isinstance(hook, MutantFinder):
                finder = hook
                break

        if finder is None:
            self._source_module_names = []
            return

        # Cache entries with truthy value (not False) are source-under-test
        source_names = {name for name, entry in finder._cache.items() if entry}
        # Intersect with currently loaded modules; exclude irradiate_harness itself
        self._source_module_names = [
            name for name in source_names if name in sys.modules and name != "irradiate_harness" and not name.startswith("irradiate_harness.")
        ]

    def _snapshot_source_modules(self):
        """Shallow-copy source module state before the first mutant run.

        This snapshot is taken once, before any test runs. Between mutant
        runs, _restore_source_modules() resets each module back to this state,
        preventing globals mutated by one test from leaking into the next run.
        """
        self._module_snapshots = {}
        for mod_name in self._source_module_names:
            mod = sys.modules.get(mod_name)
            if mod is not None:
                # Shallow copy: stores references, not deep copies.
                # Handles scalar globals, class defs, function refs correctly.
                # Mutable containers modified in-place (list.append, dict[k]=v)
                # are NOT isolated — recycling handles that case.
                self._module_snapshots[mod_name] = dict(vars(mod))

    def _restore_source_modules(self):
        """Restore source module state to the pre-run snapshot.

        Called in the finally block after each mutant run to reset module-level
        globals that tests may have modified. The shallow snapshot captures the
        module dict at collection time, so trampolined functions are preserved.
        irradiate_harness.active_mutant is NOT affected (separate module).
        """
        for mod_name, snapshot in self._module_snapshots.items():
            mod = sys.modules.get(mod_name)
            if mod is not None:
                current = vars(mod)
                current.clear()
                current.update(snapshot)

    def _reset_run_state(self):
        self.current_run_mutant = None
        self.current_run_nodeids = set()
        self.current_item_nodeid = None
        self.current_run_reports = []

    def _prepare_items(self, test_ids):
        items = [self.items[tid] for tid in test_ids if tid in self.items]
        items.sort(key=lambda item: self.item_order.get(item.nodeid, sys.maxsize))
        return items

    def _run_items_via_hooks(self, items):
        self.current_run_nodeids = {item.nodeid for item in items}

        for item in items:
            self.current_item_nodeid = item.nodeid
            start_idx = len(self.current_run_reports)

            # Safety-first phase: treat each item as a teardown boundary. This
            # avoids relying on private setup state when a mutant is killed.
            item.config.hook.pytest_runtest_protocol(item=item, nextitem=None)

            item_reports = self.current_run_reports[start_idx:]
            if reports_indicate_failure(item_reports):
                return 1

        return 0

    def pytest_runtest_logreport(self, report):
        if self.current_run_mutant is None:
            return
        if self.current_item_nodeid is None:
            return
        if report.nodeid != self.current_item_nodeid:
            return
        self.current_run_reports.append(report)

    def pytest_runtestloop(self, session) -> bool:
        """
        Intercept pytest's run loop to drive mutation testing via IPC.

        Runs after collection and before pytest_sessionfinish, so the session
        is fully alive and all plugins are ready for test execution.
        """
        import irradiate_harness

        if not self.items:
            print("WARNING: No tests collected", file=sys.stderr)

        # Send ready with collected test IDs and session fixture metadata
        send_message(
            self.sock,
            {
                "type": "ready",
                "pid": os.getpid(),
                "tests": list(self.items.keys()),
                "has_session_fixtures": bool(self.session_fixture_names),
                "session_fixture_count": len(self.session_fixture_names),
            },
        )

        # Snapshot module state AFTER collection (tests have imported source modules)
        # but BEFORE the first mutant run. Restored between runs to prevent leakage.
        self._snapshot_source_modules()

        while True:
            msg, self.buf = recv_message(self.sock, self.buf)
            if msg is None:
                break

            if msg["type"] == "shutdown":
                break

            if msg["type"] == "warmup":
                send_message(self.sock, {"type": "ready", "pid": os.getpid()})
                continue

            if msg["type"] == "run":
                mutant_name = msg["mutant"]
                test_ids = msg["tests"]

                start = time.monotonic()

                try:
                    items_to_run = self._prepare_items(test_ids)

                    if not items_to_run:
                        send_message(
                            self.sock,
                            {
                                "type": "result",
                                "mutant": mutant_name,
                                "exit_code": 33,
                                "duration": 0.0,
                            },
                        )
                        continue

                    self._reset_run_state()
                    self.current_run_mutant = mutant_name
                    irradiate_harness.active_mutant = mutant_name
                    run_exit_code = self._run_items_via_hooks(items_to_run)

                    duration = time.monotonic() - start
                    send_message(
                        self.sock,
                        {
                            "type": "result",
                            "mutant": mutant_name,
                            "exit_code": run_exit_code,
                            "duration": duration,
                        },
                    )
                except SystemExit as exc:
                    duration = time.monotonic() - start
                    exit_code = int(exc.code) if isinstance(exc.code, int) else 1
                    send_message(
                        self.sock,
                        {
                            "type": "result",
                            "mutant": mutant_name,
                            "exit_code": exit_code,
                            "duration": duration,
                        },
                    )
                except BaseException:
                    duration = time.monotonic() - start
                    send_message(
                        self.sock,
                        {
                            "type": "error",
                            "mutant": mutant_name,
                            "message": traceback.format_exc(),
                            "duration": duration,
                        },
                    )
                finally:
                    self._reset_run_state()
                    self._restore_source_modules()
                    irradiate_harness.active_mutant = None

        return True  # Signal to pytest that we handled the run loop


def main():  # pragma: no mutate
    socket_path = os.environ["IRRADIATE_SOCKET"]
    tests_dir = os.environ.get("IRRADIATE_TESTS_DIR", "tests")

    # Import irradiate_harness BEFORE pytest to install the MutantFinder import
    # hook. The hook intercepts imports of mutated modules from mutants/; it
    # must be active before pytest imports test files (which import source under
    # test) during collection.
    import irradiate_harness as _irradiate_harness  # noqa: F401

    import pytest

    # Connect to orchestrator. The orchestrator accepts our connection and then
    # blocks reading the "ready" message; we send "ready" from within
    # pytest_runtestloop after collection completes.
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.connect(socket_path)

    plugin = MutationWorkerPlugin(sock)

    # Run pytest: collection happens, then our plugin intercepts the run loop
    # to process mutant assignments via IPC.
    # The import hook (installed via irradiate_harness.__init__) handles mutant
    # module resolution — no sys.path manipulation needed.
    pytest.main([tests_dir], plugins=[plugin])

    sock.close()


if __name__ == "__main__":
    main()
