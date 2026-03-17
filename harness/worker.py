"""
irradiate pytest worker — connects to the orchestrator over a unix socket,
receives mutant assignments, runs pytest items directly, reports results.
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


def main():
    socket_path = os.environ["IRRADIATE_SOCKET"]
    mutants_dir = os.environ.get("IRRADIATE_MUTANTS_DIR", "mutants")
    tests_dir = os.environ.get("IRRADIATE_TESTS_DIR", "tests")

    # Defensively add mutants_dir to sys.path so mutated modules can be
    # imported even if PYTHONPATH was not set by the caller. In normal
    # operation the orchestrator sets PYTHONPATH (via pipeline::build_pythonpath)
    # which already includes mutants_dir, so this is a no-op guard against
    # misconfigured invocations (e.g. running worker.py by hand).
    if mutants_dir not in sys.path:
        sys.path.insert(0, os.path.abspath(mutants_dir))

    # Import harness (should be on sys.path already)
    import irradiate_harness

    # Import pytest and collect tests
    import pytest

    # Collect all test items
    class ItemCollector:
        def __init__(self):
            self.items = {}
            self.session = None

        def pytest_collection_finish(self, session):
            self.session = session
            for item in session.items:
                self.items[item.nodeid] = item

    collector = ItemCollector()
    # Collect tests without running them
    exit_code = pytest.main(
        ["--collect-only", "-q", tests_dir],
        plugins=[collector],
    )

    if not collector.items:
        print(f"WARNING: No tests collected from {tests_dir}", file=sys.stderr)

    # Connect to orchestrator
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.connect(socket_path)
    buf = b""

    # Send ready
    send_message(
        sock,
        {"type": "ready", "pid": os.getpid(), "tests": list(collector.items.keys())},
    )

    while True:
        msg, buf = recv_message(sock, buf)
        if msg is None:
            break

        if msg["type"] == "shutdown":
            break

        if msg["type"] == "warmup":
            send_message(sock, {"type": "ready", "pid": os.getpid()})
            continue

        if msg["type"] == "run":
            mutant_name = msg["mutant"]
            test_ids = msg["tests"]

            # Set active mutant
            irradiate_harness.active_mutant = mutant_name

            start = time.monotonic()
            try:
                # Find the test items to run
                items_to_run = []
                for tid in test_ids:
                    if tid in collector.items:
                        items_to_run.append(collector.items[tid])

                if not items_to_run:
                    send_message(
                        sock,
                        {
                            "type": "result",
                            "mutant": mutant_name,
                            "exit_code": 33,
                            "duration": 0.0,
                        },
                    )
                    irradiate_harness.active_mutant = None
                    continue

                # Run the selected tests using pytest's internal API
                # We use pytest.main with specific test nodeids for simplicity
                # and -x for fail-fast
                test_args = ["-x", "--no-header", "-q"] + test_ids
                exit_code = pytest.main(test_args)

                duration = time.monotonic() - start

                send_message(
                    sock,
                    {
                        "type": "result",
                        "mutant": mutant_name,
                        "exit_code": exit_code,
                        "duration": duration,
                    },
                )
            except Exception:
                duration = time.monotonic() - start
                send_message(
                    sock,
                    {
                        "type": "error",
                        "mutant": mutant_name,
                        "message": traceback.format_exc(),
                        "duration": duration,
                    },
                )
            finally:
                irradiate_harness.active_mutant = None

    sock.close()


if __name__ == "__main__":
    main()
