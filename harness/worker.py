"""
irradiate pytest worker — connects to the orchestrator over a unix socket,
receives mutant assignments, runs pytest items directly, reports results.

Architecture: the IPC loop runs inside pytest_runtestloop so that all pytest
plugins are fully initialized when the worker executes selected items via
pytest's hook machinery.
"""

import gc
import json
import os
import resource
import signal
import socket
import sys
import threading
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
        self._fork_mode = False  # set in pytest_runtestloop based on env var

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
        # Preserve order from orchestrator (sorted by duration for fail-fast)
        return [self.items[tid] for tid in test_ids if tid in self.items]

    def _run_items_via_hooks(self, items):
        self.current_run_nodeids = {item.nodeid for item in items}

        for i, item in enumerate(items):
            self.current_item_nodeid = item.nodeid
            start_idx = len(self.current_run_reports)

            # Pass the real next item so pytest only tears down fixtures that
            # the next item doesn't share (session, module, class scopes).
            # Last item gets None → full teardown before next mutant.
            nextitem = items[i + 1] if i + 1 < len(items) else None
            item.config.hook.pytest_runtest_protocol(item=item, nextitem=nextitem)

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

    def _run_in_process(self, mutant_name, items_to_run, start):  # pragma: no mutate
        """Run tests in the current process (legacy mode, --no-fork)."""
        import irradiate_harness

        try:
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

    def _run_forked(self, mutant_name, items_to_run, start, timeout_secs=None):  # pragma: no mutate
        """Fork a child to run tests. Parent waits and reports result."""
        import irradiate_harness

        pid = os.fork()

        if pid == 0:
            # === CHILD PROCESS ===
            try:
                # Close parent socket fd — child communicates only via exit code
                self.sock.close()

                # Reset signal handlers to defaults
                signal.signal(signal.SIGTERM, signal.SIG_DFL)
                signal.signal(signal.SIGINT, signal.SIG_DFL)

                # Set CPU time limit as orphan safety net
                if timeout_secs is not None:
                    try:
                        limit = int(timeout_secs) + 5
                        resource.setrlimit(resource.RLIMIT_CPU, (limit, limit + 1))
                    except (ValueError, resource.error):
                        pass

                self._reset_run_state()
                self.current_run_mutant = mutant_name
                irradiate_harness.active_mutant = mutant_name
                exit_code = self._run_items_via_hooks(items_to_run)
            except SystemExit as exc:
                exit_code = int(exc.code) if isinstance(exc.code, int) else 1
            except BaseException:
                traceback.print_exc()
                exit_code = 99
            finally:
                os._exit(exit_code)
        else:
            # === PARENT PROCESS ===
            try:
                _, wait_status = os.waitpid(pid, 0)
            except ChildProcessError:
                wait_status = 0

            duration = time.monotonic() - start

            if os.WIFEXITED(wait_status):
                exit_code = os.WEXITSTATUS(wait_status)
                send_message(self.sock, {
                    "type": "result", "mutant": mutant_name,
                    "exit_code": exit_code, "duration": duration,
                })
            elif os.WIFSIGNALED(wait_status):
                sig = os.WTERMSIG(wait_status)
                if sig in (signal.SIGKILL, signal.SIGXCPU):
                    send_message(self.sock, {
                        "type": "result", "mutant": mutant_name,
                        "exit_code": -sig, "duration": duration,
                    })
                else:
                    send_message(self.sock, {
                        "type": "error", "mutant": mutant_name,
                        "message": f"child killed by signal {sig}",
                        "duration": duration,
                    })
            else:
                send_message(self.sock, {
                    "type": "error", "mutant": mutant_name,
                    "message": "unknown child wait status",
                    "duration": duration,
                })

    def pytest_runtestloop(self, session) -> bool:
        """
        Intercept pytest's run loop to drive mutation testing via IPC.

        Runs after collection and before pytest_sessionfinish, so the session
        is fully alive and all plugins are ready for test execution.
        """
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

        # Choose execution model based on IRRADIATE_NO_FORK env var.
        # Fork mode (default): freeze GC to prevent COW faults, then fork per mutant.
        # No-fork mode (--no-fork): snapshot module state and restore between runs.
        self._fork_mode = os.environ.get("IRRADIATE_NO_FORK") != "1"
        if self._fork_mode:
            gc.freeze()  # prevent COW faults from GC refcount updates in children
            thread_count = threading.active_count()
            if thread_count > 1:
                print(
                    f"[irradiate] WARNING: {thread_count} threads active at fork time; "
                    "fork-unsafe plugins may cause hangs",
                    file=sys.stderr,
                )
        else:
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

                if self._fork_mode:
                    self._run_forked(mutant_name, items_to_run, start, timeout_secs=msg.get("timeout_secs"))
                else:
                    self._run_in_process(mutant_name, items_to_run, start)

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
