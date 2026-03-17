# irradiate harness — imported as irradiate_harness in mutated Python files
import os

active_mutant = os.environ.get("IRRADIATE_ACTIVE_MUTANT")


class ProgrammaticFailException(Exception):
    pass


_hits = set()


def record_hit(func_key):
    """Record that a trampolined function was called (stats mode)."""
    _hits.add(func_key)


def get_hits():
    """Return all recorded function hits and clear the set."""
    result = set(_hits)
    _hits.clear()
    return result
