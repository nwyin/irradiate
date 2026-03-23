# Import Hook Design: Replacing PYTHONPATH Shadowing

This document describes the design for replacing irradiate's PYTHONPATH-based import shadowing with a custom Python import hook. It's useful for contributors working on import system reliability or understanding why import resolution works the way it does.

## The problem

irradiate currently uses PYTHONPATH ordering to make Python import trampolined code from `mutants/` instead of original source. This has produced three bugs in quick succession:

1. **Partial mutation** (commit 5683cec) — only mutated files were in `mutants/`, so sibling imports broke
2. **pytest pythonpath config** (commit a2c0919) — pytest inserted paths before ours, shadowing mutants
3. **sys.path[0] = ''** (unfixed) — Python always puts cwd first; flat-layout projects find originals before mutants

Each fix added another band-aid (`-o pythonpath=`, copy all files, proposed `-P` flag). The underlying issue is that PYTHONPATH shadowing is the wrong mechanism for controlling which code Python loads.

## The idea

Replace PYTHONPATH shadowing with a **custom import hook** (`sys.meta_path` finder). When Python encounters `import mylib`, the hook intercepts the import, checks if a trampolined version exists in `mutants/mylib/`, and loads it. If not, it returns `None` and Python uses its normal resolution.

Import hooks run **before** `sys.path` is consulted. This eliminates all three bugs — path ordering, cwd shadowing, and pytest config interference become irrelevant.

## Prior art

### mutmut's approach (and why it differs)

mutmut does NOT use import hooks. Its author tried and abandoned them in 2016 due to import system fragility. Instead, mutmut:

1. Copies the **entire source tree** into `mutants/` (full mirror)
2. Changes cwd to `mutants/` before running tests (`change_cwd("mutants")`)
3. Removes original source directories from `sys.path` to prevent shadowing
4. Forks a new process per mutant (complete isolation)

This works but is slow — every mutant pays full pytest startup (~200ms). irradiate can't use the cwd trick because its pre-warmed workers run multiple mutants in one process.

### Tools that successfully use import hooks

- **typeguard** — installs a `MetaPathFinder` to instrument typed functions at import time. Focused scope (only instruments specific packages), returns `None` for everything else.
- **pytest `--import-mode=importlib`** — uses `importlib.import_module()` with synthetic names to avoid polluting `sys.path`.
- **coverage.py** — uses `sys.settrace()` rather than import hooks, but the principle is similar: intercept at the right level rather than manipulating paths.

The pattern that works: a focused hook that handles a small, known set of modules and returns `None` quickly for everything else.

## Design

### MutantFinder class

```python
# harness/import_hook.py

import importlib.abc
import importlib.machinery
import importlib.util
import os
import sys
from pathlib import Path


class MutantFinder(importlib.abc.MetaPathFinder):
    """
    Intercepts imports and loads trampolined code from mutants/.

    Installed at position 0 of sys.meta_path so it runs before Python's
    default path-based finder. For modules that exist in mutants/, returns
    a spec pointing to the trampolined file. For everything else, returns
    None and lets Python resolve normally.

    The hook handles WHICH code is loaded. The trampoline (inside the loaded
    code) handles WHICH variant runs based on irradiate_harness.active_mutant.
    """

    def __init__(self, mutants_dir):
        self.mutants_dir = Path(mutants_dir).resolve()
        self._cache = {}  # fullname -> Path | False

    def find_spec(self, fullname, path, target=None):
        # Never intercept the harness itself (circular import risk)
        if fullname == "irradiate_harness" or fullname.startswith("irradiate_harness."):
            return None

        # Fast exit for known non-mutant prefixes
        if fullname.startswith(("_pytest.", "pytest.", "pluggy.")):
            return None

        spec_path = self._resolve(fullname)
        if spec_path is None:
            return None

        loader = importlib.machinery.SourceFileLoader(fullname, str(spec_path))
        is_package = spec_path.name == "__init__.py"

        return importlib.util.spec_from_file_location(
            fullname,
            spec_path,
            loader=loader,
            submodule_search_locations=[str(spec_path.parent)] if is_package else None,
        )

    def invalidate_caches(self):
        self._cache.clear()

    def _resolve(self, fullname):
        """Check if fullname exists in mutants/. Returns Path or None."""
        if fullname in self._cache:
            hit = self._cache[fullname]
            return hit if hit else None

        parts = fullname.split(".")

        # Try as module: mutants/foo/bar.py
        module_path = self.mutants_dir.joinpath(*parts[:-1], parts[-1] + ".py")
        if module_path.is_file():
            self._cache[fullname] = module_path
            return module_path

        # Try as package: mutants/foo/bar/__init__.py
        package_path = self.mutants_dir.joinpath(*parts, "__init__.py")
        if package_path.is_file():
            self._cache[fullname] = package_path
            return package_path

        self._cache[fullname] = False
        return None
```

### How it resolves imports

For `from mylib.sub import func`:

```
1. Python imports 'mylib'
   → find_spec('mylib', None)
   → checks mutants/mylib/__init__.py → found
   → loads trampolined __init__.py

2. Python imports 'mylib.sub'
   → find_spec('mylib.sub', ['mutants/mylib'])
   → checks mutants/mylib/sub.py → found
   → loads trampolined sub.py (contains trampoline for func())

3. Test calls func(1, 2)
   → trampoline checks irradiate_harness.active_mutant
   → dispatches to original or mutant variant
```

For `import json` (stdlib):
```
1. find_spec('json', None)
   → checks mutants/json.py → not found
   → checks mutants/json/__init__.py → not found
   → returns None
   → Python uses default finder → finds stdlib json ✓
```

### What happens with relative imports

Inside `mylib/sub.py`:
```python
from . import sibling
```

Python converts this to an absolute import `from mylib import sibling`, then calls:
```
find_spec('mylib.sibling', ['mutants/mylib'])
```

The hook checks `mutants/mylib/sibling.py` — if it exists (mutated or copied), the hook returns it. If not, returns `None` and Python resolves from the source directory. **Relative imports work without special handling.**

### What about partial mutation?

With the hook, we have a choice:

**Option A (current): Full mirror.** Copy all source files to `mutants/`, overwriting mutated ones. The hook finds everything in `mutants/`. Simple, correct, slightly wasteful on disk.

**Option B (future optimization): Selective loading.** Only write mutated files to `mutants/`. The hook finds mutated files there; for non-mutated files, it returns `None` and Python falls through to the source directory (still on PYTHONPATH as `source_parent`).

Option B eliminates the need to copy unmutated files entirely. The hook handles the "partial mutation" problem that previously required copying. This is a follow-up optimization — start with Option A for safety.

## Installation points

### Worker process (harness/worker.py)

The hook must be installed **before pytest imports any source modules**:

```python
def main():
    mutants_dir = os.environ.get("IRRADIATE_MUTANTS_DIR", "mutants")

    # Install import hook BEFORE importing pytest
    from irradiate_harness.import_hook import MutantFinder
    sys.meta_path.insert(0, MutantFinder(mutants_dir))
    importlib.invalidate_caches()

    import pytest
    # ... rest of worker startup
```

### Subprocess invocations (validate, discover, stats)

These run pytest as `python -m pytest ...`. The hook needs to be active before test collection. Two options:

**Option A: `-p` plugin.** Load `irradiate_harness` as a pytest plugin (`-p irradiate_harness`). The harness `__init__.py` installs the hook at import time. Requires adding hook installation to `harness/__init__.py`:

```python
# harness/__init__.py
import os
import sys

active_mutant = os.environ.get("IRRADIATE_ACTIVE_MUTANT")

# Install import hook if mutants dir is specified
_mutants_dir = os.environ.get("IRRADIATE_MUTANTS_DIR")
if _mutants_dir:
    from irradiate_harness.import_hook import MutantFinder
    sys.meta_path.insert(0, MutantFinder(_mutants_dir))

# ... rest of harness
```

Then all pytest invocations from Rust add `-p irradiate_harness`:
```rust
Command::new(python)
    .arg("-m").arg("pytest")
    .arg("-p").arg("irradiate_harness")  // triggers hook installation
    .env("IRRADIATE_MUTANTS_DIR", &mutants_dir)
    // ...
```

**Option B: sitecustomize.py.** Place a `sitecustomize.py` in the harness directory that installs the hook. Since `harness_dir` is first on PYTHONPATH, Python executes it at startup.

Option A is cleaner — it's explicit and doesn't rely on Python startup hooks.

## PYTHONPATH simplification

### Before (current)

```
harness_dir : mutants_dir : source_parent
```

Three paths. `mutants_dir` is there for path-based import shadowing. `source_parent` is there for sibling module fallback.

### After (with import hook)

```
harness_dir : source_parent
```

Two paths. `mutants_dir` is handled by the import hook. `source_parent` provides fallback for non-mutated modules (until we implement Option B selective loading, which would also remove `source_parent`).

### After selective loading (future)

```
harness_dir
```

One path. The hook handles mutated modules. Non-mutated modules resolve via Python's default finder (they're installed in the venv or on the default path). `source_parent` is no longer needed.

## What we can remove

Once the hook is working:

1. **`-o pythonpath=`** — no longer needed. pytest's pythonpath config can't interfere because the hook runs first.
2. **`-P` flag** — not needed. cwd on sys.path doesn't matter because the hook runs first.
3. **Full file copying** — eventually. With selective loading, only mutated files need to be in `mutants/`.
4. **`mutants_dir` on PYTHONPATH** — immediately. The hook handles it.
5. **`source_parent` on PYTHONPATH** — eventually, once selective loading is implemented.

## Edge cases

### C extensions (.so/.pyd)

C extensions can't be mutated (they're compiled). The hook won't find them in `mutants/`, returns `None`, and Python loads them normally. No issue.

### Namespace packages (no `__init__.py`)

If the source uses implicit namespace packages, the hook needs to handle this. Currently, irradiate skips files without `__init__.py` in the mutation pipeline, so this is not an immediate concern. If needed, set `submodule_search_locations=[]` (empty list, not None) in the spec to indicate a namespace package portion.

### Editable installs (`pip install -e .`)

Editable installs use `.pth` files or `MetaPathFinder` entries in site-packages. Since our hook is at position 0 of `sys.meta_path`, it runs before any editable-install finders. If the module is in `mutants/`, we load it. If not, the editable install's finder handles it. No conflict.

### sys.modules caching

Python caches imported modules in `sys.modules`. Once `mylib` is imported, the hook is never called again for `mylib`. This is correct for irradiate — the trampoline handles variant switching within an already-imported module. The hook's job is to ensure the **trampolined version** is what gets imported initially.

### Bytecode caching (.pyc)

Python caches compiled bytecode in `__pycache__/`. Since `mutants/` is regenerated on each `irradiate run`, stale `.pyc` files could theoretically cause issues. Mitigations:

1. `irradiate run` already deletes and recreates `mutants/` (including `__pycache__/`)
2. Call `importlib.invalidate_caches()` after hook installation
3. Python checks `.pyc` timestamps against `.py` — regenerated files get new timestamps

### Worker recycling

The hook is installed once per worker process. When a worker is recycled (respawned), the new process gets a fresh hook installation. No state leakage between recycled workers.

## Performance

The hook adds overhead to every `import` statement:

**Non-matching imports** (stdlib, third-party): ~1-2 microseconds
- One string prefix check (`irradiate_harness`, `_pytest`)
- One dict lookup (cache)
- One or two `Path.is_file()` calls on first import (cached after)

**Matching imports** (mutated modules): ~5 microseconds
- Same as above, plus ModuleSpec construction
- SourceFileLoader handles compilation

For a typical test run with ~100 imports, total hook overhead is <200 microseconds. This is negligible compared to test execution time.

## Interaction with the trampoline

The hook and trampoline serve different purposes and don't interfere:

```
Import time (once per worker startup):
  import mylib.calc
    → MutantFinder.find_spec('mylib.calc')
    → loads mutants/mylib/calc.py (contains trampoline)
    → trampoline is now the module's `add` function

Test time (many times per mutant):
  add(1, 2)
    → trampoline checks active_mutant
    → dispatches to x_add__irradiate_orig or x_add__irradiate_1
    → no import system involvement at all
```

The hook ensures the right code is loaded. The trampoline ensures the right variant runs. Clean separation.

## Migration plan

### Phase 1: Add the hook alongside PYTHONPATH (safe)

1. Create `harness/import_hook.py` with MutantFinder
2. Install hook in `harness/__init__.py` (triggered by `IRRADIATE_MUTANTS_DIR` env var)
3. Keep existing PYTHONPATH construction (harness + mutants + source_parent)
4. Pass `IRRADIATE_MUTANTS_DIR` from all Rust subprocess invocations
5. Add `-p irradiate_harness` to all pytest invocations

At this point, both the hook AND PYTHONPATH work. The hook takes priority. If anything breaks, the PYTHONPATH fallback catches it. This is a safe transition.

**Verify:** All existing tests pass. Vendor smoke tests pass for flat-layout projects.

### Phase 2: Remove PYTHONPATH shadowing

1. Remove `mutants_dir` from `build_pythonpath()`
2. Remove `-o pythonpath=` from all pytest invocations
3. Remove `-P` flag if it was added
4. PYTHONPATH is now just `harness_dir:source_parent`

**Verify:** All tests still pass. The hook is doing all the work.

### Phase 3: Selective loading (optional optimization)

1. Stop copying unmutated files to `mutants/`
2. Remove `source_parent` from PYTHONPATH
3. Non-mutated modules resolve via Python's default finder
4. PYTHONPATH is now just `harness_dir`

**Verify:** All tests pass. Mutation generation is faster (less disk I/O).

## Why this is better than `-P`

| Concern | `-P` flag | Import hook |
|---|---|---|
| Solves flat-layout shadowing | Yes | Yes |
| Solves pytest config interference | No (still need `-o pythonpath=`) | Yes (hook runs first) |
| Solves partial mutation | No (still need full mirror) | Yes (selective loading possible) |
| Minimum Python version | 3.11 | 3.4 (find_spec protocol) |
| Number of invocation sites to modify | 7 | 1 (harness __init__.py) |
| Fragility | Adds another flag to track | Self-contained in one module |
| Future path simplification | None | Can eventually drop PYTHONPATH entirely |

## Design decisions

1. **Always-on, no escape hatch.** The hook is always active when `IRRADIATE_MUTANTS_DIR` is set. No opt-out flag. If the hook has bugs, we fix them — shipping a broken fallback path just hides problems. Users who hit issues file bug reports and we fix the hook.

2. **Exclude `conftest.py` from the hook.** pytest discovers `conftest.py` by walking the filesystem, not through the import system. The hook should never intercept conftest imports. Additionally, the mutation pipeline should skip `conftest.py` files entirely — they contain test configuration and fixtures, not application logic worth mutating. The hook's exclusion list should include `conftest` as a module name.

3. **Disable bytecode caching for mutated modules.** Set `spec.cached = None` for all modules loaded by the hook. This avoids stale `.pyc` issues when `mutants/` is regenerated between runs. The performance impact is likely negligible (compilation is fast for the small files irradiate generates), but this should be benchmarked — see [GitHub issue #5](https://github.com/nwyin/irradiate/issues/5).

4. **Support namespace packages from day one.** Implicit namespace packages (no `__init__.py`) are common in modern Python. The hook handles them by returning a spec with `submodule_search_locations=[]` (empty list) when a directory exists in `mutants/` but has no `__init__.py`. The mutation pipeline should also be updated to discover and process files in namespace packages.

### Namespace package handling in the hook

```python
def _resolve(self, fullname):
    parts = fullname.split(".")

    # Try as module: mutants/foo/bar.py
    module_path = self.mutants_dir.joinpath(*parts[:-1], parts[-1] + ".py")
    if module_path.is_file():
        return ("module", module_path)

    # Try as package: mutants/foo/bar/__init__.py
    package_dir = self.mutants_dir.joinpath(*parts)
    init_path = package_dir / "__init__.py"
    if init_path.is_file():
        return ("package", init_path)

    # Try as namespace package: mutants/foo/bar/ (directory, no __init__.py)
    if package_dir.is_dir():
        return ("namespace", package_dir)

    return None
```

For namespace packages, the spec is:
```python
spec = ModuleSpec(
    fullname,
    loader=None,  # namespace packages have no loader
    is_package=True,
)
spec.submodule_search_locations = [str(package_dir)]
```
