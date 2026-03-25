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
import sys
from importlib.machinery import SourceFileLoader
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
        self._cache = {}  # fullname -> ("module"|"package", Path) | False
        # Top-level package names present in mutants/ for fast prefix rejection
        self._top_level_names = set()
        # Cached original search locations per package
        self._original_locations = {}
        self._prescan()

    def _prescan(self):  # pragma: no mutate
        """Walk mutants/ once at init to build top-level prefix set and
        pre-populate _cache with all known mutated modules."""
        if not self.mutants_dir.is_dir():
            return
        for py_file in self.mutants_dir.rglob("*.py"):
            rel = py_file.relative_to(self.mutants_dir)
            parts = list(rel.parts)
            if parts[-1] == "__init__.py":
                fullname = ".".join(parts[:-1])
                if fullname:
                    self._cache[fullname] = ("package", py_file)
            else:
                parts[-1] = parts[-1].removesuffix(".py")
                fullname = ".".join(parts)
                self._cache[fullname] = ("module", py_file)
        for fullname in self._cache:
            self._top_level_names.add(fullname.split(".")[0])

    def find_spec(self, fullname, path, target=None):  # pragma: no mutate
        # Never intercept the harness itself — circular import risk
        if fullname == "irradiate_harness" or fullname.startswith("irradiate_harness."):
            return None

        # Fast prefix rejection: if the top-level package isn't in mutants/,
        # skip immediately. Covers stdlib, test frameworks, third-party deps.
        top = fullname.split(".", 1)[0]
        if top not in self._top_level_names:
            return None

        result = self._resolve(fullname)
        if result is None:
            return None

        kind, resolved_path = result

        loader = SourceFileLoader(fullname, str(resolved_path))
        is_package = kind == "package"

        # For packages: include both the mutants dir AND the original package's
        # search locations so non-mutated submodules can still be found.
        search_locations = None
        if is_package:
            search_locations = [str(resolved_path.parent)]
            original_locations = self._find_original_search_locations(fullname)
            if original_locations:
                search_locations.extend(
                    loc for loc in original_locations
                    if loc not in search_locations
                )

        return importlib.util.spec_from_file_location(
            fullname,
            resolved_path,
            loader=loader,
            submodule_search_locations=search_locations,
        )

    def invalidate_caches(self):
        self._cache.clear()

    def _find_original_search_locations(self, fullname):
        """Find the original package's search locations by temporarily
        removing ourselves from sys.meta_path and re-resolving.
        Results are cached per package name."""
        if fullname in self._original_locations:
            return self._original_locations[fullname]

        idx = None
        for i, finder in enumerate(sys.meta_path):
            if finder is self:
                idx = i
                break
        if idx is None:
            self._original_locations[fullname] = None
            return None

        sys.meta_path.pop(idx)
        try:
            spec = importlib.util.find_spec(fullname)
            if spec and spec.submodule_search_locations:
                result = list(spec.submodule_search_locations)
                self._original_locations[fullname] = result
                return result
        except (ModuleNotFoundError, ValueError):
            pass
        finally:
            sys.meta_path.insert(idx, self)

        self._original_locations[fullname] = None
        return None

    def _resolve(self, fullname):
        """
        Check if fullname exists in mutants/. Returns (kind, Path) or None.

        Only intercepts when there is an actual file in mutants/ — either a
        module (.py file) or a package (__init__.py). Directories without
        __init__.py are NOT intercepted; Python loads the original package
        normally, and the hook intercepts individual mutated submodules.
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

        self._cache[fullname] = False
        return None
