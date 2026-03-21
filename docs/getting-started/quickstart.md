# Quick Start

This guide walks through running irradiate on a Python project with an existing pytest test suite.

## Step 1: Pick a project

You need a Python project with:
- Source code in a `src/` or similar directory
- Tests in a `tests/` directory
- `pytest` installed in the project's Python environment

For the examples below, we'll use a project with source at `src/` and tests at `tests/`.

## Step 2: Run mutation testing

From the project root:

```bash
irradiate run --paths-to-mutate src/
```

irradiate will:

1. Parse all `.py` files under `src/` and generate mutants
2. Run one "stats" pytest session to map which tests cover which functions
3. Dispatch mutants to a worker pool, running only the relevant tests per mutant
4. Write results to `.irradiate/` and print a summary

Example output:

```
Collecting stats...  [done in 2.3s]
Running 847 mutants across 4 workers...
████████████████████████████████████████ 847/847

Results:
  Killed:    731  (86.3%)
  Survived:   98  (11.6%)
  No cover:   18   (2.1%)
  Total:      847

Elapsed: 94s
```

## Step 3: View results

```bash
irradiate results
```

Shows survived mutants — the ones your tests missed:

```
Survived mutants:
  mylib.x_compute__mutmut_3
  mylib.x_validate__mutmut_1
  mylib.x_validate__mutmut_7
  ...
```

To see all mutants including killed ones:

```bash
irradiate results --all
```

## Step 4: Inspect a specific mutant

```bash
irradiate show mylib.x_compute__mutmut_3
```

Output shows the diff — what changed:

```diff
--- original
+++ mutant
@@ -4,7 +4,7 @@
 def compute(x, y):
-    return x + y
+    return x - y
```

A survived mutant here means no test caught the `+` → `-` change. That's either a missing test or dead code.

## Step 5: Cache behavior

Results are cached by content hash. If you run irradiate again without changing source or tests, it skips cached mutants:

```bash
irradiate run --paths-to-mutate src/
# Skipped: 731 cached results
# Running: 116 new/changed mutants
```

To clear the cache:

```bash
irradiate cache clean
```

## Next steps

- Configure paths and options in [pyproject.toml](configuration.md)
- Use `--isolate` for strict subprocess isolation (slower, no state leakage)
- Use `--verify-survivors` to re-test survived mutants in isolation mode to catch false negatives
