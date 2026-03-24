# Quick Start

Run irradiate on a Python project with an existing pytest test suite.

## Step 1: Run mutation testing

From the project root:

```bash
irradiate run
```

irradiate will:

1. Parse all `.py` files under `src/` and generate mutants
2. Run one pytest session to map which tests cover which functions
3. Fork a child process per mutant inside pre-warmed workers
4. Print a summary

Example output:

```
Generating mutants...
  done in 3ms (142 mutants across 8 files)
Running stats + validation...
  done in 1.2s
Running mutation testing (142 mutants, 8 workers)...

Mutation testing complete (142 mutants in 4.2s, 34 mutants/sec)
  Killed:    128
  Survived:  9
  No tests:  3
  Timeout:   2
  Score:     93.4%
```

## Step 2: View results

```bash
irradiate results
```

Shows survived mutants — the ones your tests missed:

```
Survived mutants:
  mylib.x_compute__irradiate_3
  mylib.x_validate__irradiate_1
```

## Step 3: Inspect a specific mutant

```bash
irradiate show mylib.x_compute__irradiate_3
```

Shows the diff:

```diff
--- original
+++ mutant
 def compute(x, y):
-    return x + y
+    return x - y
```

A survived mutant here means no test caught the `+` to `-` change.

## Step 4: Incremental mode

On a feature branch, test only functions you changed:

```bash
irradiate run --diff main
```

## Step 5: Reports

```bash
# JSON report (Stryker mutation-testing-report-schema v2)
irradiate run --report json

# Self-contained HTML report
irradiate run --report html
```

In GitHub Actions, irradiate auto-detects CI and adds inline annotations on survived mutants.

## Step 6: CI gating

Fail the build if mutation score drops below a threshold:

```bash
irradiate run --fail-under 80
```

## Cache behavior

Results are cached by content hash. Unchanged functions with unchanged tests skip on re-runs:

```bash
irradiate cache clean   # clear if needed
```

## GitHub Actions

Add mutation testing to your PR checks:

```yaml
- uses: actions/checkout@v4
  with:
    fetch-depth: 0
- uses: nwyin/irradiate@v0
  with:
    diff: origin/main
    fail-under: "80"
```

This installs irradiate, tests only functions changed in the PR, fails if the mutation score drops below 80%, and adds inline annotations on survived mutants. See [CI Integration](../guide/ci-integration.md) for the full reference.

## Next steps

- [Configuration](configuration.md) — pyproject.toml settings
- [CI Integration](../guide/ci-integration.md) — GitHub Actions setup
- [Understanding Results](../guide/understanding-results.md) — what survived/killed/timeout mean
