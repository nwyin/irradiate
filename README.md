# irradiate

[![PyPI](https://img.shields.io/pypi/v/irradiate)](https://pypi.org/project/irradiate/)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Docs](https://img.shields.io/badge/docs-nwyin.github.io%2Firradiate-blue)](https://nwyin.github.io/irradiate/)

Fast mutation testing for Python, written in Rust. Built for CI.

Code coverage tells you which lines ran. [Mutation testing](https://en.wikipedia.org/wiki/Mutation_testing) tells you which lines are actually *tested*. irradiate makes small changes to your code — flipping `<` to `<=`, swapping `+` with `-`, replacing `True` with `False` — and checks whether your tests catch each one. If they don't, that's a gap.

## Quick start

```bash
pip install irradiate

# Test only functions changed in your PR
irradiate run --diff main
```

That's it. irradiate finds your `src/` and `tests/`, generates mutants for the changed code, and reports which ones survived.

### Add to CI in 3 lines

```yaml
- uses: nwyin/irradiate@v0
  with:
    diff: origin/main
    fail-under: "80"
```

This runs mutation testing on every PR, fails if the score drops below 80%, and posts inline annotations on surviving mutants.

### Example output

```
$ irradiate run --diff main
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

## Why irradiate

- **Fast** — pre-warmed pytest workers with fork-per-mutant execution. Pytest starts once. Tests run many times.
- **38 mutation operators** — arithmetic, comparison, boolean, string methods, return values, exception types, regex patterns, [and more](https://nwyin.github.io/irradiate/internals/mutation-operators/).
- **Incremental** — `--diff main` tests only functions changed since a git ref.
- **Cached** — content-addressed results survive rebases, branch switches, and `touch`.
- **CI-native** — `--fail-under` for gating, GitHub Actions annotations, JSON/HTML reports, [composite action](https://nwyin.github.io/irradiate/guide/ci-integration/).
- **Drop-in** — works with any pytest project.

## Install

```bash
pip install irradiate
```

Requires Python 3.10+ with pytest installed. See the [installation guide](https://nwyin.github.io/irradiate/getting-started/installation/) for more options.

## Usage

```bash
# Test functions changed since main (the CI use case)
irradiate run --diff main

# Run on the full codebase
irradiate run

# Sample 10% of mutants for fast feedback
irradiate run --sample 0.1

# Generate reports
irradiate run --report json   # Stryker mutation-testing-report-schema v2
irradiate run --report html   # self-contained HTML report

# Fail CI if score is below threshold
irradiate run --fail-under 80

# Explore results
irradiate results
irradiate show module.x_func__irradiate_1
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

Source paths can also be passed as positional arguments: `irradiate run src/mylib`. All settings can be overridden via CLI flags. Run `irradiate run --help` for the full list.

## Features

### Mutation operators (38 categories)

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

The [GitHub Actions composite action](https://nwyin.github.io/irradiate/guide/ci-integration/) (shown above) auto-detects the CI environment, emits inline `::warning` annotations on survived mutants, and writes a Markdown step summary. See the [CI integration guide](https://nwyin.github.io/irradiate/guide/ci-integration/) for advanced configuration, caching, and non-GitHub setups.

### Performance tuning

Parallelism defaults to CPU count (`--workers N` to override). Workers are recycled when RSS exceeds `--max-worker-memory N` MB. `--covered-only` skips mutants with no test coverage. `--no-stats` skips coverage collection when you want to test all mutants against all tests. Per-mutant timeout defaults to 10x baseline (`--timeout-multiplier N`).

## How it compares to mutmut

| | mutmut | irradiate |
|---|---|---|
| **Speed** | `pytest.main()` per mutant (~200ms each) | Fork-per-mutant, pytest starts once |
| **Parser** | LibCST (Python) | tree-sitter (Rust, parallel) |
| **Operators** | ~20 categories | 38 categories (incl. 11 regex) |
| **Cache** | mtime-based | Content-addressable (SHA-256) |
| **Orchestration** | Python multiprocessing | Rust + tokio async |
| **Incremental** | no | `--diff` with merge-base |
| **Reports** | Terminal only | JSON, HTML, GitHub Actions annotations |
| **Decorator support** | Skip all | @property/@classmethod/@staticmethod handled |
| **Sampling** | no | `--sample` with operator stratification |
| **CI integration** | Manual | `--fail-under`, GitHub Actions action, annotations, step summary |
| **Isolation** | Fork only | Warm-session + `--isolate` + `--verify-survivors` |

## Guides

- [Quick start](https://nwyin.github.io/irradiate/getting-started/quickstart/) — install, run, interpret results
- [CI integration](https://nwyin.github.io/irradiate/guide/ci-integration/) — GitHub Actions, caching, gating
- [Understanding results](https://nwyin.github.io/irradiate/guide/understanding-results/) — what mutation scores mean
- [Surviving mutants](https://nwyin.github.io/irradiate/guide/surviving-mutants/) — what to do when mutants survive
- [Performance tuning](https://nwyin.github.io/irradiate/guide/performance/) — workers, sampling, `--covered-only`
- [Configuration](https://nwyin.github.io/irradiate/getting-started/configuration/) — pyproject.toml reference
- [Comparison with mutmut](https://nwyin.github.io/irradiate/guide/comparison/) — detailed feature comparison

## Acknowledgments

irradiate's trampoline architecture and mutation operator design are informed by [mutmut](https://github.com/boxed/mutmut). The naming convention is partially compatible with mutmut to ease migration.

## License

MIT
