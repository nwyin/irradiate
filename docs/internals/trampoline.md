---
title: The Trampoline — How irradiate Switches Mutations at Runtime
description: How irradiate's trampoline architecture enables fast mutation testing by switching mutant code at runtime without restarting pytest.
---

# The Trampoline: How irradiate switches mutations at runtime

## The problem

Mutation testing needs to run your test suite once per mutation. If you have 500 mutations and each pytest startup takes 250ms, that's 125 seconds of just starting pytest.

The naive approach: fork a process for each mutant, re-import everything, run tests, exit.

irradiate's approach: import everything once, then _switch_ which mutation is active between test runs without restarting. The mechanism that enables this is the **trampoline**.

## What the trampoline looks like

Every mutated function is replaced by a thin wrapper that checks a global variable to decide whether to run the original code, a mutated variant, or a special mode (stats collection, forced failure).

Given this Python source:

```python
def add(a, b):
    return a + b
```

irradiate produces this in `mutants/my_module/__init__.py`:

```python
import irradiate_harness as _ih

def _irradiate_trampoline(orig, mutants, call_args, call_kwargs, self_arg=None, args=None):
    active = _ih.active_mutant
    if not active:
        return orig(*call_args, **call_kwargs)      # hot path: no mutation
    if active == 'fail':
        raise _ih.ProgrammaticFailException()        # validation mode
    if active == 'stats':
        _ih.record_hit(orig.__module__ + '.' + orig.__name__)
        return orig(*call_args, **call_kwargs)       # stats collection mode
    prefix = orig.__module__ + '.' + orig.__name__ + '__irradiate_'
    if not active.startswith(prefix):
        return orig(*call_args, **call_kwargs)       # not our mutant
    variant_key = active.rpartition('.')[-1]
    return mutants[variant_key](*call_args, **call_kwargs)  # run the mutated variant

# --- Original function, renamed ---
def x_add__irradiate_orig(a, b):
    return a + b

# --- Mutated variant: + swapped to - ---
def x_add__irradiate_1(a, b):
    return a - b

# --- Lookup table ---
x_add__irradiate_mutants = {
    'x_add__irradiate_1': x_add__irradiate_1,
}
x_add__irradiate_orig.__name__ = 'x_add'

# --- Wrapper (takes the original function name) ---
def add(a, b):
    return _irradiate_trampoline(
        x_add__irradiate_orig,
        x_add__irradiate_mutants,
        (a, b), {},
        None,
    )
```

When your test calls `add(1, 2)`, it hits the wrapper, which calls `_irradiate_trampoline`. What happens next then depends on the value of `irradiate_harness.active_mutant`:

| `active_mutant` value              | What runs                                             | Why                                                   |
| ---------------------------------- | ----------------------------------------------------- | ----------------------------------------------------- |
| `None`                             | `x_add__irradiate_orig(1, 2)` → `3`                   | Normal execution, no mutation                         |
| `"fail"`                           | Raises `ProgrammaticFailException`                    | Forced-fail validation — confirms trampoline is wired |
| `"stats"`                          | Records hit, then `x_add__irradiate_orig(1, 2)` → `3` | Collects which functions each test touches            |
| `"my_module.x_add__irradiate_1"`   | `x_add__irradiate_1(1, 2)` → `-1`                     | Runs the mutated variant                              |
| `"my_module.x_greet__irradiate_1"` | `x_add__irradiate_orig(1, 2)` → `3`                   | Different function's mutant, not ours — run original  |

## Why a global variable, not an environment variable

The trampoline reads `_ih.active_mutant` — a Python module attribute. This is a dict lookup (nanoseconds) vs reading `os.environ["..."]` (a syscall, microseconds).

The worker process sets this global directly:

```python
irradiate_harness.active_mutant = "my_module.x_add__irradiate_1"
# ... run tests ...
irradiate_harness.active_mutant = None  # reset
```

No process restart or reimport. Just update a global and run tests again.

## The full lifecycle

Given a source file and test:

```python
# my_module.py
def add(a, b):
    return a + b
```

```python
# test_add.py
from my_module import add

def test_add():
    assert add(1, 2) == 3
```

The lifecycle from build through execution:

```
 BUILD (Rust, once)
 ──────────────────
 1. Parse my_module.py with tree-sitter
 2. Find mutations: `a + b` can become `a - b`
 3. Assemble trampolined file:
    - x_add__irradiate_orig(a, b): return a + b
    - x_add__irradiate_1(a, b):    return a - b
    - def add(a, b): → trampoline dispatch
 4. Write to mutants/my_module/__init__.py

 WORKER STARTUP (Python, once per worker)
 ────────────────────────────────────────
 5. pytest starts, import hook intercepts `import my_module`
 6. Loads trampolined code from mutants/
 7. Collects test items
 8. Connects to Rust orchestrator, sends "ready"

 MUTANT LOOP (Python, repeated per mutant)
 ──────────────────────────────────────────
 9.  Orchestrator sends mutant name
10.  active_mutant = "my_module.x_add__irradiate_1"
11.  Run test_add()
12.    test calls add(1, 2)
13.      → wrapper → trampoline checks active_mutant
14.      → dispatches to x_add__irradiate_1
15.      → returns -1
16.    assert 3 == -1 → FAIL → mutant killed ✓
17.  active_mutant = None
18.  Report result, receive next mutant
```

## Naming conventions

irradiate follows mutmut's naming to keep compatibility:

| Python source              | Mangled name                   | Why                                                     |
| -------------------------- | ------------------------------ | ------------------------------------------------------- |
| `def foo()` (top-level)    | `x_foo`                        | `x_` prefix avoids collisions                           |
| `class Bar` method `baz()` | `xǁBarǁbaz`                    | Unicode separator `ǁ` (U+01C1) encodes class membership |
| Original function          | `x_foo__irradiate_orig`        | Preserved for non-mutant execution                      |
| Mutant variant N           | `x_foo__irradiate_N`           | N is 1-indexed                                          |
| Fully qualified key        | `my_module.x_foo__irradiate_1` | Module prefix for cross-file dispatch                   |

## How imports work

irradiate uses a custom import hook (`MutantFinder`) installed at `sys.meta_path[0]`. When Python encounters `import mylib`, the hook checks if a trampolined version exists in `mutants/mylib/` and loads it. If not, it returns `None` and Python resolves normally.

This replaced the earlier PYTHONPATH-shadowing approach, which was fragile (path ordering, pytest config interference, flat-layout projects). See [Import Hook Design](import-hook.md) for details.

## What gets mutated, what doesn't

The trampoline wraps **functions and methods only**. Module-level code (imports, constants, class definitions) is copied verbatim — it runs once at import time and is not subject to runtime switching.

Currently skipped:

- Functions with non-descriptor decorators (@cache, @app.route, etc.)
- `__getattribute__`, `__setattr__`, `__new__`
- Enum subclass methods, functions with `nonlocal`
- `# pragma: no mutate` lines

Handled by descriptor-aware trampoline:

- `@property`, `@classmethod`, `@staticmethod` (see [Decorator Handling](decorators.md))

## The three special modes

### `active_mutant = None` (normal)

Hot path. Every function call goes through the trampoline and immediately dispatches to the original. This is the baseline — tests should pass identically to running without irradiate.

### `active_mutant = "fail"` (validation)

Every function call raises `ProgrammaticFailException`. If the test suite still passes, the trampoline is not wired correctly (tests aren't calling through the mutated imports). irradiate runs this check before mutation testing to catch PYTHONPATH and import issues early.

### `active_mutant = "stats"` (coverage)

Every function call records a hit (`module.x_func`), then runs the original. After each test, the worker reads which functions were hit. This builds a function→test mapping so irradiate only runs relevant tests per mutant (e.g., only `test_add` for mutations in `add()`).

## Performance characteristics

The trampoline adds overhead to every instrumented function call:

- One attribute read (`_ih.active_mutant`)
- One `if not active` check (truthy test)
- One function call forwarding

In the common case (`active_mutant is None`), this is ~100ns per call. For mutation testing runs where thousands of function calls happen per test, the overhead is negligible compared to the time saved by not restarting pytest.

The real performance win is at the **worker pool** level: a single `pytest.main()` call at worker startup (250ms) is amortized across hundreds of mutant runs, instead of paying it per mutant.
