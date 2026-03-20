"""
Test that crashes the pytest worker process during mutation runs.

During stats collection (active_mutant == "stats") and baseline
runs (active_mutant is None or "fail"), the test passes normally.
Only when running under a real mutant does the test call os._exit(1),
which kills the Python process and forces the orchestrator to handle
an unexpected worker disconnect.
"""

import os


def test_worker_crash():
    """Crash the worker process when running under a real mutant."""
    import irradiate_harness

    active = irradiate_harness.active_mutant
    if active is not None and active not in ("stats", "fail"):
        os._exit(1)
    # Under stats collection or baseline: test passes normally
