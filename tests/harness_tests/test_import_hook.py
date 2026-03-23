"""
Tests for harness/import_hook.py — MutantFinder MetaPathFinder.

Tests cover the invariants required for the hook to work correctly in the
mutation testing pipeline. Does NOT test actual import loading (e2e territory).
"""

import importlib
import sys
from pathlib import Path

import pytest

# conftest.py adds repo root to sys.path so this import resolves to harness/import_hook.py
from harness.import_hook import MutantFinder


@pytest.fixture
def tmp_mutants(tmp_path):
    """Minimal mutants/ directory fixture. Callers populate it as needed."""
    return tmp_path / "mutants"


@pytest.fixture
def finder(tmp_mutants):
    """MutantFinder pointed at an empty mutants dir (does not need to exist)."""
    return MutantFinder(tmp_mutants)


# ---------------------------------------------------------------------------
# INV-1: Hook returns None for stdlib modules
# ---------------------------------------------------------------------------


def test_returns_none_for_stdlib_json(finder):
    assert finder.find_spec("json", None) is None


def test_returns_none_for_stdlib_os(finder):
    assert finder.find_spec("os", None) is None


def test_returns_none_for_stdlib_sys(finder):
    assert finder.find_spec("sys", None) is None


def test_returns_none_for_pathlib(finder):
    assert finder.find_spec("pathlib", None) is None


# ---------------------------------------------------------------------------
# INV-2 & INV-3: Hook returns None for irradiate_harness.* and test framework
# ---------------------------------------------------------------------------


def test_returns_none_for_irradiate_harness(finder):
    assert finder.find_spec("irradiate_harness", None) is None


def test_returns_none_for_irradiate_harness_submodule(finder):
    assert finder.find_spec("irradiate_harness.import_hook", None) is None


def test_returns_none_for_pytest(finder):
    assert finder.find_spec("pytest", None) is None


def test_returns_none_for_pytest_submodule(finder):
    assert finder.find_spec("pytest.mark", None) is None


def test_returns_none_for_private_pytest(finder):
    assert finder.find_spec("_pytest.runner", None) is None


def test_returns_none_for_pluggy(finder):
    assert finder.find_spec("pluggy.hooks", None) is None


# ---------------------------------------------------------------------------
# INV-4: conftest is excluded
# ---------------------------------------------------------------------------


def test_returns_none_for_conftest(finder):
    assert finder.find_spec("conftest", None) is None


# ---------------------------------------------------------------------------
# INV-5: Hook returns ModuleSpec for .py files that exist in mutants_dir
# ---------------------------------------------------------------------------


def test_finds_top_level_module(tmp_mutants):
    tmp_mutants.mkdir()
    (tmp_mutants / "mylib.py").write_text("x = 1")
    finder = MutantFinder(tmp_mutants)
    spec = finder.find_spec("mylib", None)
    assert spec is not None
    assert spec.name == "mylib"


def test_finds_nested_module(tmp_mutants):
    (tmp_mutants / "mypkg").mkdir(parents=True)
    (tmp_mutants / "mypkg" / "sub.py").write_text("y = 2")
    finder = MutantFinder(tmp_mutants)
    spec = finder.find_spec("mypkg.sub", None)
    assert spec is not None
    assert spec.name == "mypkg.sub"


def test_module_spec_origin_points_to_file(tmp_mutants):
    tmp_mutants.mkdir()
    target = tmp_mutants / "calc.py"
    target.write_text("def add(a, b): return a + b")
    finder = MutantFinder(tmp_mutants)
    spec = finder.find_spec("calc", None)
    assert spec is not None
    assert spec.origin == str(target)


def test_module_spec_has_no_submodule_search_locations(tmp_mutants):
    tmp_mutants.mkdir()
    (tmp_mutants / "util.py").write_text("")
    finder = MutantFinder(tmp_mutants)
    spec = finder.find_spec("util", None)
    assert spec is not None
    assert spec.submodule_search_locations is None


# ---------------------------------------------------------------------------
# INV-6: Hook returns ModuleSpec with is_package=True for packages
# ---------------------------------------------------------------------------


def test_finds_package(tmp_mutants):
    pkg = tmp_mutants / "mypkg"
    pkg.mkdir(parents=True)
    (pkg / "__init__.py").write_text("")
    finder = MutantFinder(tmp_mutants)
    spec = finder.find_spec("mypkg", None)
    assert spec is not None
    assert spec.name == "mypkg"
    assert spec.submodule_search_locations is not None


def test_package_spec_submodule_search_locations_is_pkg_dir(tmp_mutants):
    pkg = tmp_mutants / "mypkg"
    pkg.mkdir(parents=True)
    (pkg / "__init__.py").write_text("")
    finder = MutantFinder(tmp_mutants)
    spec = finder.find_spec("mypkg", None)
    assert spec is not None
    # submodule_search_locations must point to the package directory
    assert str(pkg.resolve()) in spec.submodule_search_locations


# ---------------------------------------------------------------------------
# INV-7: Hook does NOT intercept dirs without __init__.py (no namespace hijack)
# ---------------------------------------------------------------------------


def test_does_not_intercept_namespace_directory(tmp_mutants):
    """Directories without __init__.py should NOT be intercepted.
    Python loads the original package, and the hook intercepts individual
    mutated submodules. This prevents breaking packages where only some
    files are mutated (e.g. --paths-to-mutate httpx/_content.py)."""
    ns_dir = tmp_mutants / "namespace_pkg"
    ns_dir.mkdir(parents=True)
    finder = MutantFinder(tmp_mutants)
    spec = finder.find_spec("namespace_pkg", None)
    assert spec is None  # let Python handle it


# ---------------------------------------------------------------------------
# INV-8: Caching — second _resolve() call for same name skips filesystem
# ---------------------------------------------------------------------------


def test_resolve_caches_positive_result(tmp_mutants):
    tmp_mutants.mkdir()
    (tmp_mutants / "mod.py").write_text("")
    finder = MutantFinder(tmp_mutants)
    finder._resolve("mod")
    assert "mod" in finder._cache
    # Remove the file so a second filesystem hit would miss
    (tmp_mutants / "mod.py").unlink()
    # Cached result must be returned even though the file is gone
    result = finder._resolve("mod")
    assert result is not None
    assert result[0] == "module"


def test_resolve_caches_negative_result(tmp_mutants):
    tmp_mutants.mkdir()
    finder = MutantFinder(tmp_mutants)
    finder._resolve("nonexistent")
    assert finder._cache.get("nonexistent") is False
    # Result must still be None on second call
    assert finder._resolve("nonexistent") is None


def test_bare_directory_is_cached_as_negative(tmp_mutants):
    """Directories without __init__.py are cached as negative results,
    since the hook no longer intercepts them."""
    ns_dir = tmp_mutants / "nspkg"
    ns_dir.mkdir(parents=True)
    finder = MutantFinder(tmp_mutants)
    finder._resolve("nspkg")
    assert finder._cache.get("nspkg") is False


# ---------------------------------------------------------------------------
# INV-9: spec has_location=True so __file__ is set on loaded modules
# ---------------------------------------------------------------------------


def test_module_spec_has_location(tmp_mutants):
    tmp_mutants.mkdir()
    (tmp_mutants / "thing.py").write_text("")
    finder = MutantFinder(tmp_mutants)
    spec = finder.find_spec("thing", None)
    assert spec is not None
    assert spec.has_location is True
    assert spec.origin is not None


def test_package_spec_has_location(tmp_mutants):
    pkg = tmp_mutants / "pkg"
    pkg.mkdir(parents=True)
    (pkg / "__init__.py").write_text("")
    finder = MutantFinder(tmp_mutants)
    spec = finder.find_spec("pkg", None)
    assert spec is not None
    assert spec.has_location is True
    assert spec.origin is not None


# ---------------------------------------------------------------------------
# INV-10: invalidate_caches() clears the cache
# ---------------------------------------------------------------------------


def test_invalidate_caches_clears_cache(tmp_mutants):
    tmp_mutants.mkdir()
    (tmp_mutants / "cached_mod.py").write_text("")
    finder = MutantFinder(tmp_mutants)
    finder._resolve("cached_mod")
    assert finder._cache  # cache is non-empty
    finder.invalidate_caches()
    assert not finder._cache


# ---------------------------------------------------------------------------
# Failure modes
# ---------------------------------------------------------------------------


def test_missing_mutants_dir_returns_none():
    """Hook should not crash when mutants_dir does not exist."""
    finder = MutantFinder("/nonexistent/path/mutants")
    assert finder.find_spec("anything", None) is None


def test_missing_mutants_dir_caches_negative_result():
    """Hook should cache the negative result when mutants_dir does not exist."""
    finder = MutantFinder("/nonexistent/path/mutants")
    finder.find_spec("anything", None)
    assert finder._cache.get("anything") is False
