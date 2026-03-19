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

    def __init__(self, sock, use_legacy):
        self.sock = sock
        self.use_legacy = use_legacy
        self.buf = b""
        self.items = {}  # nodeid -> Item
        self.item_order = {}  # nodeid -> collection index
        self.current_run_mutant = None
        self.current_run_nodeids = set()
        self.current_item_nodeid = None
        self.current_run_reports = []

    def pytest_collection_finish(self, session):
        for index, item in enumerate(session.items):
            self.items[item.nodeid] = item
            self.item_order[item.nodeid] = index

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
        import pytest

        if not self.items:
            print("WARNING: No tests collected", file=sys.stderr)

        # Send ready with collected test IDs
        send_message(
            self.sock,
            {"type": "ready", "pid": os.getpid(), "tests": list(self.items.keys())},
        )

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

                    if self.use_legacy:
                        # Legacy path: re-invokes full pytest machinery each time.
                        # Enable with IRRADIATE_WORKER_LEGACY=1 to aid debugging.
                        irradiate_harness.active_mutant = mutant_name
                        test_args = ["-x", "--no-header", "-q", "-o", "pythonpath="] + test_ids
                        run_exit_code = pytest.main(test_args)
                    else:
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
                    irradiate_harness.active_mutant = None

        return True  # Signal to pytest that we handled the run loop


def main():  # pragma: no mutate
    socket_path = os.environ["IRRADIATE_SOCKET"]
    tests_dir = os.environ.get("IRRADIATE_TESTS_DIR", "tests")
    use_legacy = os.environ.get("IRRADIATE_WORKER_LEGACY", "").strip() == "1"

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

    plugin = MutationWorkerPlugin(sock, use_legacy)

    # Run pytest: collection happens, then our plugin intercepts the run loop
    # to process mutant assignments via IPC.
    # The import hook (installed via irradiate_harness.__init__) handles mutant
    # module resolution — no sys.path manipulation needed.
    pytest.main([tests_dir], plugins=[plugin])

    sock.close()


if __name__ == "__main__":
    main()
