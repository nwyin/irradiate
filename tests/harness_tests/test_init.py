"""
Tests for harness/__init__.py — active_mutant, _hits, record_hit(), get_hits(),
ProgrammaticFailException.
"""

import sys

import pytest


def _fresh_harness():
    """Import harness freshly, removing any cached version from sys.modules."""
    for key in list(sys.modules):
        if key == "harness" or key.startswith("harness."):
            del sys.modules[key]
    import harness
    return harness


@pytest.fixture(autouse=True)
def clean_harness():
    """Reset harness state before each test."""
    h = _fresh_harness()
    h._hits.clear()
    yield h
    h._hits.clear()


def test_active_mutant_default_none(clean_harness):
    # IRRADIATE_ACTIVE_MUTANT not set in the test environment
    assert clean_harness.active_mutant is None


def test_record_hit_adds_to_set(clean_harness):
    clean_harness.record_hit("module.x_foo__mutmut_1")
    assert "module.x_foo__mutmut_1" in clean_harness._hits


def test_record_hit_deduplicates(clean_harness):
    clean_harness.record_hit("module.x_foo__mutmut_1")
    clean_harness.record_hit("module.x_foo__mutmut_1")
    assert len(clean_harness._hits) == 1


def test_get_hits_returns_copy_and_clears(clean_harness):
    clean_harness.record_hit("a")
    clean_harness.record_hit("b")
    hits = clean_harness.get_hits()
    assert hits == {"a", "b"}
    # _hits must be cleared after get_hits()
    assert len(clean_harness._hits) == 0


def test_get_hits_empty(clean_harness):
    hits = clean_harness.get_hits()
    assert hits == set()


def test_get_hits_returns_snapshot_not_live_ref(clean_harness):
    clean_harness.record_hit("x")
    hits = clean_harness.get_hits()
    # Mutating the returned set must not affect internal state
    hits.add("injected")
    assert "injected" not in clean_harness._hits


def test_programmatic_fail_exception_is_exception_subclass(clean_harness):
    assert issubclass(clean_harness.ProgrammaticFailException, Exception)


def test_programmatic_fail_exception_can_be_raised_and_caught(clean_harness):
    with pytest.raises(clean_harness.ProgrammaticFailException):
        raise clean_harness.ProgrammaticFailException("test failure")


def test_programmatic_fail_exception_not_caught_by_bare_valueerror(clean_harness):
    # It must be a distinct exception class so callers can target it specifically
    with pytest.raises(clean_harness.ProgrammaticFailException):
        try:
            raise clean_harness.ProgrammaticFailException("boom")
        except ValueError:
            pass  # must not be caught here
