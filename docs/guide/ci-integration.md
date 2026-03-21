# CI Integration

irradiate can run in CI to catch test coverage regressions before they land. This page shows how to set it up.

> **Note:** irradiate is pre-alpha. The setup below describes how you'd integrate it — there's no official CI action yet.

## GitHub Actions

A basic workflow that builds irradiate from source and runs it:

```yaml
name: Mutation Testing

on:
  pull_request:
    branches: [main]

jobs:
  mutation-test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Set up Rust
        uses: actions-rust-lang/setup-rust-toolchain@v1

      - name: Set up Python
        uses: actions/setup-python@v5
        with:
          python-version: "3.12"

      - name: Install pytest
        run: pip install pytest

      - name: Build irradiate
        run: cargo build --release

      - name: Run mutation testing
        run: ./target/release/irradiate run
        env:
          RUST_LOG: info

      - name: Check for survivors
        run: |
          ./target/release/irradiate results
          # Exit non-zero if any mutants survived (optional policy)
          # ./target/release/irradiate results | grep -q "survived" && exit 1 || exit 0
```

## Caching the build

Cache Rust build artifacts and the irradiate cache between runs:

```yaml
      - name: Cache Rust build
        uses: actions/cache@v4
        with:
          path: |
            ~/.cargo/registry
            ~/.cargo/git
            target/
          key: ${{ runner.os }}-cargo-${{ hashFiles('Cargo.lock') }}

      - name: Cache irradiate results
        uses: actions/cache@v4
        with:
          path: mutants/
          key: ${{ runner.os }}-irradiate-${{ hashFiles('src/**/*.py') }}
          restore-keys: |
            ${{ runner.os }}-irradiate-
```

irradiate stores mutation results as `.meta` files in `mutants/`. Caching this directory means unchanged functions get cached results on subsequent runs — dramatically reducing CI time for incremental changes.

## Stats mode (faster CI)

By default, irradiate runs your test suite once in stats mode to determine which tests cover which functions. This means each mutant only runs the relevant tests instead of the whole suite. Stats mode is enabled by default — disable it with `--no-stats` if you want every mutant to run against all tests:

```yaml
      # Faster: only run relevant tests per mutant (default)
      - run: ./target/release/irradiate run

      # Slower: run all tests for every mutant
      - run: ./target/release/irradiate run --no-stats
```

For most projects, stats mode is the right choice in CI — it's much faster and catches the same mutations.

## Verifying survivors

The `--verify-survivors` flag re-tests survived mutants in isolated subprocess mode after the main run. This catches false negatives from test state leakage in the warm worker pool:

```bash
irradiate run --verify-survivors
```

Use this when you want high confidence in survivor results, at the cost of extra time.

## Exit codes

irradiate exits 0 if the run completes (regardless of kill count). Check `irradiate results` output or parse `.meta` files to make CI fail on survivors.

## What to gate on

Rather than failing every CI run with survivors (which will block work on pre-existing gaps), consider:

1. **Fail on new survivors** — diff the mutation score between base and PR branch
2. **Report only** — add mutation results as a PR comment without blocking
3. **Gate per-module** — enforce a minimum score on critical modules, not the whole codebase

The right policy depends on your team's current mutation coverage and tolerance for friction.
