"""
irradiate pytest worker — connects to the orchestrator over a unix socket,
receives mutant assignments, runs pytest items directly, reports results.

Architecture: the IPC loop runs inside pytest_runtestloop so that all pytest
plugins are properly initialized for direct item execution via runtestprotocol.
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


def run_items_directly(items, fail_fast=True):
    """
    Run pre-collected pytest items directly using pytest's internal runner API.

    Must be called from within an active pytest session (i.e. from pytest_runtestloop)
    so that the session's stash and all plugin state are fully initialized.

    Returns:
        0 = all tests passed (mutant survived)
        1 = at least one test failed/errored (mutant killed)
    """
    from _pytest.runner import runtestprotocol

    failed = False
    for i, item in enumerate(items):
        # Pass nextitem so within-run module-scoped fixtures are not redundantly
        # torn down between tests. The last item gets nextitem=None to force full
        # teardown, leaving the session clean for the next mutant run.
        nextitem = items[i + 1] if i + 1 < len(items) else None

        try:
            reports = runtestprotocol(item, nextitem=nextitem, log=True)
        except SystemExit as exc:
            return int(exc.code) if isinstance(exc.code, int) else 1
        except BaseException:
            failed = True
            if fail_fast:
                _force_teardown(item)
                break
            continue

        for report in reports:
            if report.failed:
                failed = True
                break

        if failed and fail_fast:
            # Ensure module/session-scoped fixtures are torn down so the next
            # mutant run starts with a clean fixture state.
            _force_teardown(item)
            break

    return 1 if failed else 0


def _force_teardown(item):  # pragma: no mutate
    """Force teardown of all active fixtures on the session's setup state."""
    try:
        state = item.session._setupstate
        # pytest 7+ exposes teardown_all(); older versions use teardown_with_finalize(None)
        if hasattr(state, "teardown_all"):
            state.teardown_all()
        elif hasattr(state, "teardown_with_finalize"):
            state.teardown_with_finalize(None)
    except Exception:
        pass


def reset_run_state(items):
    """
    Reset per-item state that accumulated during a mutant run so the next run
    starts clean (INV-2: no stdout/stderr pollution between runs).
    """
    for item in items:
        if hasattr(item, "_report_sections"):
            item._report_sections.clear()


class MutationWorkerPlugin:
    """
    pytest plugin that intercepts pytest_runtestloop to run the IPC loop.

    By running the IPC dispatch loop inside pytest_runtestloop, we ensure
    the session is fully initialized (stash keys set, capture manager active)
    when runtestprotocol is called for each mutant.
    """

    def __init__(self, sock, use_legacy):
        self.sock = sock
        self.use_legacy = use_legacy
        self.buf = b""
        self.items = {}  # nodeid -> Item

    def pytest_collection_finish(self, session):
        for item in session.items:
            self.items[item.nodeid] = item

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

                irradiate_harness.active_mutant = mutant_name
                start = time.monotonic()

                try:
                    items_to_run = [self.items[tid] for tid in test_ids if tid in self.items]

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
                        irradiate_harness.active_mutant = None
                        continue

                    if self.use_legacy:
                        # Legacy path: re-invokes full pytest machinery each time.
                        # Enable with IRRADIATE_WORKER_LEGACY=1 to aid debugging.
                        test_args = ["-x", "--no-header", "-q", "-o", "pythonpath="] + test_ids
                        run_exit_code = pytest.main(test_args)
                    else:
                        # Fast path: run pre-collected items directly within this session.
                        run_exit_code = run_items_directly(items_to_run, fail_fast=True)
                        # Reset per-item state between runs (INV-2)
                        reset_run_state(items_to_run)

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
                except Exception:
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
