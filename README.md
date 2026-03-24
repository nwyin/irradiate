# irradiate

[![PyPI](https://img.shields.io/pypi/v/irradiate)](https://pypi.org/project/irradiate/)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Fast mutation testing for Python, written in Rust.

## Why

Mutation testing is slow. The bottleneck isn't generating mutants, it's running the test suite once per mutant. A typical pytest startup costs 200-500ms, and with hundreds of mutants that adds up to minutes of pure overhead.

irradiate keeps a pool of pre-warmed pytest workers. Pytest starts once, collects tests once, then forks a child process for each mutant. 30-60 mutants/sec on real codebases.

## How it works

1. Parse Python source with [tree-sitter](https://tree-sitter.github.io/) (28+ mutation operator categories including regex patterns)
2. Generate trampolined mutants: each function gets an original, N mutated variants, and a runtime dispatcher
3. Collect test coverage and timing in a single pytest run
4. Fork a child process per mutant inside pre-warmed workers (no pytest restart)
5. Report results as terminal output, JSON (Stryker schema v2), HTML, or GitHub Actions annotations

## Install

```bash
pip install irradiate
```

Or build from source:

```bash
cargo build --release
```

Requires Python 3.10+ with pytest installed.

## Usage

```bash
# Run mutation testing (auto-detects src/ and tests/)
irradiate run

# Only test functions changed since main
irradiate run --diff main

# Test 10% of mutants (fast CI feedback)
irradiate run --sample 0.1

# Generate JSON report (Stryker mutation-testing-report-schema v2)
irradiate run --report json

# Generate self-contained HTML report
irradiate run --report html

# Fail CI if mutation score is below threshold
irradiate run --fail-under 80

# See cached results
irradiate results

# Show diff for a specific mutant
irradiate show module.x_func__irradiate_1
```

### Example output

```
$ irradiate run
Generating mutants...
  done in 3ms (14 mutants across 1 files)
Running stats + validation...
  done in 195ms
Running mutation testing (14 mutants, 10 workers)...

Mutation testing complete (14 mutants in 0.1s, 175 mutants/sec)
  Cache hits: 0
  Cache misses: 12
  Killed:    11
  Survived:  1
  No tests:  2
  Score:     91.7%

Survived mutants:

  number_mutation (1):
    simple_lib/__init__.py:6  replaced `0` with `1`  [simple_lib.x_add__irradiate_3]
```

## Configuration

Configure via `[tool.irradiate]` in `pyproject.toml`:

```toml
[tool.irradiate]
paths_to_mutate = "src"
tests_dir = "tests"
do_not_mutate = ["**/generated/*", "**/vendor/*"]
pytest_add_cli_args = ["-x", "--tb=short"]
```

All settings can be overridden via CLI flags. Run `irradiate run --help` for the full list.

## Features

### Mutation operators (28+ categories)

Arithmetic, comparison, boolean, augmented assignment, unary, string mutation/emptying, number literals, constant replacement, lambda bodies, return values, assignments, default arguments, argument removal, method swaps, dict kwargs, exception types, match/case removal, condition negation, condition replacement, statement deletion, keyword swap, loop mutation, ternary swap, slice index removal, regex pattern mutations (11 operators: anchor removal, charclass negation, shorthand negation, quantifier removal/change, lookaround negation, alternation removal, and more).

Functions can be excluded with `# pragma: no mutate`.

### Execution model

By default, workers fork after pytest collection. Each mutant runs in an isolated child process with no restart overhead. For projects with complex test infrastructure, `--isolate` runs each mutant in a fresh subprocess instead. `--verify-survivors` re-tests survivors in isolate mode after the main run to catch false negatives from warm-session state leakage.

### Incremental mode (`--diff`)

Only mutate functions touched by a git diff. Uses `git merge-base` to compare against the divergence point, so `--diff main` does the right thing on feature branches.

### Reporting

Terminal output groups survived mutants by operator. `--report json` writes [Stryker mutation-testing-report-schema v2](https://github.com/stryker-mutator/mutation-testing-elements/tree/master/packages/report-schema), compatible with the Stryker Dashboard. `--report html` generates a self-contained report using [mutation-testing-elements](https://github.com/stryker-mutator/mutation-testing-elements). On GitHub Actions, irradiate auto-emits `::warning` annotations on survived mutants and writes a Markdown step summary.

### Caching

Content-addressable cache keyed on SHA-256 of function body, test IDs, and operator. Survives rebases, branch switches, and `touch` (mtime-based caches don't). Use `--no-cache` to force a full re-run.

### Decorator support

`@property`, `@classmethod`, and `@staticmethod` are handled natively via a descriptor-aware trampoline. Other decorated functions are currently skipped; a source-patching fallback is planned ([#13](https://github.com/nwyin/irradiate/issues/13)).

### Sampling (`--sample`)

Test a random subset of mutants for fast CI feedback. Academic research shows 5-10% random sampling gives 99% R² correlation with the full mutation score.

- `--sample 0.1`: test 10% of mutants
- `--sample 100`: test exactly 100 mutants
- `--sample-seed 42`: override RNG seed (default: 0 for reproducibility)

Sampling is operator-stratified, so every mutation category is proportionally represented.

### CI integration

Drop-in [GitHub Actions composite action](docs/guide/ci-integration.md):

```yaml
- uses: nwyin/irradiate@v0
  with:
    diff: origin/main
    fail-under: "80"
```

Auto-detects GitHub Actions and emits inline `::warning` annotations on survived mutants, plus a Markdown step summary.

### Performance tuning

Parallelism defaults to CPU count (`--workers N` to override). Workers are recycled automatically to limit memory growth, tunable with `--worker-recycle-after N` and `--max-worker-memory N`. `--covered-only` skips mutants with no test coverage. `--no-stats` skips coverage collection when you want to test all mutants against all tests. Per-mutant timeout defaults to 10x baseline (`--timeout-multiplier N`).

## How it compares to mutmut

| | mutmut | irradiate |
|---|---|---|
| **Speed** | `pytest.main()` per mutant (~200ms each) | Fork-per-mutant, pytest starts once |
| **Parser** | LibCST (Python) | tree-sitter (Rust, parallel) |
| **Operators** | ~20 categories | 28+ categories (incl. regex) |
| **Cache** | mtime-based | Content-addressable (SHA-256) |
| **Orchestration** | Python multiprocessing | Rust + tokio async |
| **Incremental** | no | `--diff` with merge-base |
| **Reports** | Terminal only | JSON, HTML, GitHub Actions annotations |
| **Decorator support** | Skip all | @property/@classmethod/@staticmethod handled |
| **Sampling** | no | `--sample` with operator stratification |
| **CI integration** | Manual | `--fail-under`, GitHub Actions action, annotations, step summary |
| **Isolation** | Fork only | Warm-session + `--isolate` + `--verify-survivors` |

## Acknowledgments

irradiate's trampoline architecture and mutation operator design are informed by [mutmut](https://github.com/boxed/mutmut). The naming convention is partially compatible with mutmut to ease migration.

## License

MIT
