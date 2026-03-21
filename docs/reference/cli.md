# CLI Reference

irradiate provides four subcommands: `run`, `results`, `show`, and `cache`.

## Global

```
irradiate [OPTIONS] <COMMAND>
```

| Flag | Description |
|------|-------------|
| `--version` | Print version |
| `--help` | Print help |

---

## `irradiate run`

Run mutation testing against your project.

```bash
irradiate run [OPTIONS] [MUTANT_NAMES]...
```

### Arguments

| Argument | Description |
|----------|-------------|
| `[MUTANT_NAMES]...` | Specific mutant names to test. If omitted, all mutants are tested. |

### Options

| Flag | Type | Default | Description |
|------|------|---------|-------------|
| `--paths-to-mutate` | `String` | `"src"` (or from config) | Path(s) to source code to mutate. Overrides `pyproject.toml`. |
| `--tests-dir` | `String` | `"tests"` (or from config) | Path to test directory. Overrides `pyproject.toml`. |
| `--workers` | `usize` | CPU count | Number of worker processes. |
| `--timeout-multiplier` | `f64` | `10.0` | Timeout multiplier applied to baseline test duration. |
| `--no-stats` | flag | — | Skip stats collection; test all mutants against all tests. |
| `--covered-only` | flag | — | Skip mutants with no test coverage. |
| `--python` | `String` | `"python3"` | Python interpreter path. |
| `--worker-recycle-after` | `usize` | auto | Respawn workers after N mutants (default: 100, or 20 when session-scoped fixtures detected; 0 to disable). |
| `--max-worker-memory` | `usize` | `0` | Recycle workers whose RSS exceeds this threshold in MB. 0 disables. |
| `--isolate` | flag | — | Run each mutant in a fresh subprocess. Slower, but fully isolated. |
| `--verify-survivors` | flag | — | After the main run, re-test survived mutants in isolate mode to detect false negatives. No-op when `--isolate` is set. |

### Examples

```bash
# Basic run
irradiate run

# Faster: skip stats, test only covered functions
irradiate run --covered-only

# Full isolation (slow but correct)
irradiate run --isolate

# Re-check survivors after warm-session run
irradiate run --verify-survivors

# Use a specific Python interpreter
irradiate run --python .venv/bin/python

# Mutate only a specific module
irradiate run --paths-to-mutate src/mylib/core.py

# Test only a specific mutant
irradiate run mylib.x_add__mutmut_3

# Control parallelism
irradiate run --workers 4
```

### How `run` works

1. **Mutation generation** — parse Python source with libcst (parallel via rayon), generate trampolined source in `mutants/`
2. **Stats collection** — unless `--no-stats`, run the test suite once with `active_mutant = "stats"` to build a function→test mapping
3. **Validation** — run the test suite with no mutation active (clean run), then with `active_mutant = "fail"` (forced-fail) to verify the trampoline is wired
4. **Mutation testing** — dispatch mutants to the worker pool; workers set `active_mutant`, run the relevant tests, report back
5. **Results** — aggregate `.meta` files and print a summary

---

## `irradiate results`

Display mutation testing results from a previous `run`.

```bash
irradiate results [OPTIONS]
```

### Options

| Flag | Description |
|------|-------------|
| `--all` | Show all mutants, not just survived ones. |

### Output

By default, shows only survived mutants:

```
survived: my_module.x_add__mutmut_3
survived: my_module.x_process__mutmut_1
```

With `--all`, also shows killed, timeout, skipped, and no-tests mutants.

---

## `irradiate show`

Show the diff for a specific mutant.

```bash
irradiate show <MUTANT_NAME>
```

### Arguments

| Argument | Description |
|----------|-------------|
| `<MUTANT_NAME>` | Mutant name, e.g. `my_module.x_func__mutmut_1` |

### Output

Prints a diff of the original function vs. the mutated variant:

```diff
--- original
+++ mutant
 def add(a, b):
-    return a + b
+    return a - b
```

Use `irradiate results` to get mutant names, then `irradiate show` to inspect them.

---

## `irradiate cache clean`

Remove the local cache directory.

```bash
irradiate cache clean
```

Deletes `.irradiate/cache/` (the content-addressable result store). Does not affect `mutants/` or `.meta` result files.

Use this to force a full re-run if you suspect stale cache entries, or to free disk space.

---

## Configuration

All `run` options can also be set in `pyproject.toml` under `[tool.irradiate]`. CLI flags override config file values. See [Configuration](../getting-started/configuration.md) for details.

## Mutant name format

Mutant names follow the pattern: `module.mangled_function__mutmut_N`

| Source | Mutant name |
|--------|-------------|
| `def add()` in `my_module` | `my_module.x_add__mutmut_1` |
| `class Foo` method `bar()` in `my_module` | `my_module.xǁFooǁbar__mutmut_1` |

The `x_` prefix and `ǁ` (U+01C1) separator follow mutmut's naming convention for compatibility.
