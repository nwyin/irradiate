---
title: Import Hook Design — irradiate Internals
description: How irradiate uses a sys.meta_path import hook to control which Python code gets loaded during mutation testing.
---

# Import Hook Design

How irradiate controls which Python code gets loaded during mutation testing, and why it uses a `sys.meta_path` import hook instead of PYTHONPATH manipulation.

## Background

irradiate initially used PYTHONPATH ordering to make Python import trampolined code from `mutants/` instead of original source. This produced a string of bugs:

1. **Partial mutation** (5683cec) — only mutated files were in `mutants/`, so sibling imports broke.
2. **pytest pythonpath config** (a2c0919) — pytest inserted paths before ours, shadowing mutants.
3. **sys.path[0] = ''** — Python always puts cwd first; flat-layout projects find originals before mutants.

Each fix added another band-aid. The underlying issue is that PYTHONPATH shadowing is the wrong mechanism for controlling which code Python loads.

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

A custom `sys.meta_path` finder (`MutantFinder`) intercepts imports before `sys.path` is consulted. When Python encounters `import mylib`, the hook checks if a trampolined version exists in `mutants/` and loads it. If not, it returns `None` and Python uses its normal resolution.

This eliminates all three bugs — path ordering, cwd shadowing, and pytest config interference become irrelevant.

### Implementation

The hook lives in `harness/import_hook.py` and is installed automatically when the `IRRADIATE_MUTANTS_DIR` environment variable is set (see `harness/__init__.py`).

Key commits:
- 6fc1b0b — initial `MutantFinder` implementation and harness embedding
- feb3de3 — wired into pipeline, removed PYTHONPATH shadowing of `mutants_dir`
- ce2cb0f — fixed partial-mutation packages (hook merges original search locations for non-mutated submodules)
- 3925781 — prescan optimization: walks `mutants/` once at init to build a top-level prefix set for fast rejection

### How it resolves imports

For `from mylib.sub import func`:

```
1. Python imports 'mylib'
   -> MutantFinder.find_spec('mylib', None)
   -> finds mutants/mylib/__init__.py -> loads trampolined __init__.py

2. Python imports 'mylib.sub'
   -> MutantFinder.find_spec('mylib.sub', ...)
   -> finds mutants/mylib/sub.py -> loads trampolined sub.py

3. Test calls func(1, 2)
   -> trampoline checks irradiate_harness.active_mutant
   -> dispatches to original or mutant variant
```

For stdlib/third-party imports (e.g. `import json`): the top-level name isn't in the prescan set, so `find_spec` returns `None` in a single dict lookup. Python uses its default finder.

### Separation from the trampoline

The hook and trampoline serve different purposes:

- **Hook** (import time, once per module): ensures the trampolined version of the code is what gets loaded.
- **Trampoline** (call time, every invocation): dispatches to the correct mutant variant based on `active_mutant`.

After initial import, `sys.modules` caching means the hook is never called again for that module.

## PYTHONPATH today

With the hook handling mutant resolution, PYTHONPATH is simplified to:

```
harness_dir : source_parent(s)
```

`mutants_dir` is no longer on PYTHONPATH — it's passed via `IRRADIATE_MUTANTS_DIR` and handled entirely by the hook. `source_parent` provides fallback for non-mutated modules. See `build_pythonpath()` in `src/pipeline.rs`.

## Edge cases

- **C extensions** (.so/.pyd): Can't be mutated. Hook won't find them in `mutants/`, returns `None`, Python loads normally.
- **Namespace packages** (no `__init__.py`): The hook only intercepts when there's an actual file. Directories without `__init__.py` fall through to Python's default finder.
- **Editable installs**: Hook is at position 0 of `sys.meta_path`, runs before editable-install finders. No conflict.
- **Relative imports**: Python converts them to absolute imports before calling finders. Works without special handling.
- **Partial mutation**: When only some submodules are mutated, the hook merges the original package's `submodule_search_locations` so non-mutated submodules resolve normally (ce2cb0f).

## Design decisions

1. **Always-on, no escape hatch.** The hook activates whenever `IRRADIATE_MUTANTS_DIR` is set. No opt-out flag.
2. **Prescan at init.** Walking `mutants/` once upfront avoids per-import filesystem checks and enables fast prefix rejection for the vast majority of imports (stdlib, third-party).
3. **Bytecode caching left to Python.** `mutants/` is regenerated each run (including `__pycache__/`), and `importlib.invalidate_caches()` is called after hook installation. Python's `.pyc` timestamp checking handles the rest.

## Future: selective loading

Currently irradiate copies all source files to `mutants/` (full mirror). With the hook in place, only mutated files need to be written — non-mutated modules would resolve via Python's default finder. This would reduce disk I/O during the generate phase.
