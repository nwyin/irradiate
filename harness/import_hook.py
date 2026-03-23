"""
MutantFinder — sys.meta_path import hook for irradiate.

Intercepts Python imports and loads trampolined code from mutants/ instead of
original source. Installed at position 0 of sys.meta_path so it runs before
Python's default path-based finder.

The hook handles WHICH code is loaded. The trampoline (inside the loaded code)
handles WHICH variant runs based on irradiate_harness.active_mutant.
"""

import importlib.abc
import importlib.util
from importlib.machinery import ModuleSpec, SourceFileLoader
from pathlib import Path


class MutantFinder(importlib.abc.MetaPathFinder):
    """
    MetaPathFinder that loads trampolined modules from mutants/.

    For modules that exist in mutants/, returns a ModuleSpec pointing to the
    trampolined file. For everything else, returns None and lets Python resolve
    normally. Caches positive and negative lookups to avoid repeated filesystem
    checks on hot import paths.
    """

    def __init__(self, mutants_dir):
        self.mutants_dir = Path(mutants_dir).resolve()
        self._cache = {}  # fullname -> ("module"|"package"|"namespace", Path) | False

    def find_spec(self, fullname, path, target=None):  # pragma: no mutate
        # Never intercept the harness itself — circular import risk
        if fullname == "irradiate_harness" or fullname.startswith("irradiate_harness."):
            return None

        # Fast exit for test framework internals we never mutate
        if fullname.startswith(("_pytest.", "pytest.", "pluggy.")):
            return None

        # pytest discovers conftest by filesystem walk, not the import system
        if fullname == "conftest":
            return None

        result = self._resolve(fullname)
        if result is None:
            return None

        kind, resolved_path = result

        if kind == "namespace":
            spec = ModuleSpec(fullname, loader=None, is_package=True)
            spec.submodule_search_locations = [str(resolved_path)]
            return spec

        loader = SourceFileLoader(fullname, str(resolved_path))
        is_package = kind == "package"

        return importlib.util.spec_from_file_location(
            fullname,
            resolved_path,
            loader=loader,
            submodule_search_locations=[str(resolved_path.parent)] if is_package else None,
        )

    def invalidate_caches(self):
        self._cache.clear()

    def _resolve(self, fullname):
        """
        Check if fullname exists in mutants/. Returns (kind, Path) or None.

        kind is one of "module", "package", or "namespace".
        Positive and negative results are cached; namespace packages are NOT
        cached because they may gain __init__.py between calls.
        """
        if fullname in self._cache:
            hit = self._cache[fullname]
            return hit if hit is not False else None

        parts = fullname.split(".")

        # Try as module: mutants/foo/bar.py
        module_path = self.mutants_dir.joinpath(*parts[:-1], parts[-1] + ".py")
        if module_path.is_file():
            result = ("module", module_path)
            self._cache[fullname] = result
            return result

        # Try as package: mutants/foo/bar/__init__.py
        package_init = self.mutants_dir.joinpath(*parts, "__init__.py")
        if package_init.is_file():
            result = ("package", package_init)
            self._cache[fullname] = result
            return result

        # Try as namespace package: mutants/foo/bar/ (directory without __init__.py)
        # Do NOT cache — directory may gain __init__.py before next call
        dir_path = self.mutants_dir.joinpath(*parts)
        if dir_path.is_dir():
            return ("namespace", dir_path)

        self._cache[fullname] = False
        return None
