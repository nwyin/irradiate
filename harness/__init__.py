# irradiate harness — imported as irradiate_harness in mutated Python files
import os
import sys

active_mutant = os.environ.get("IRRADIATE_ACTIVE_MUTANT")

_mutants_dir = os.environ.get("IRRADIATE_MUTANTS_DIR")
if _mutants_dir:
    import importlib

    from irradiate_harness.import_hook import MutantFinder

    sys.meta_path.insert(0, MutantFinder(_mutants_dir))
    importlib.invalidate_caches()


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
