# irradiate

Mutation testing for Python, written in Rust.

irradiate finds gaps in your test suite by systematically modifying your code and checking whether your tests catch the changes. A test suite that passes with mutated code has a surviving mutant — a potential blind spot where bugs can hide undetected.

## Why irradiate?

Traditional mutation testing is slow because it invokes `pytest` from scratch for every mutant — hundreds of cold starts, hundreds of import cycles, hundreds of seconds of overhead. irradiate eliminates this by keeping a pool of pre-warmed pytest workers alive across the entire run.

- **Pre-warmed workers** — pytest starts once per worker process, not once per mutant
- **Rust orchestration** — no GIL, native signal handling, tokio async worker pool
- **Parallel mutation generation** — libcst + rayon across all source files
- **Content-addressable cache** — SHA-256 of function body + tests; survives rebases, branch switches, and `touch`
- **Targeted test selection** — one stats run to map which tests cover which functions; then only the relevant tests run per mutant

## Quick start

```bash
# Build from source
cargo build --release

# Run on your project
irradiate run --paths-to-mutate src/

# See what survived
irradiate results

# Inspect a specific mutant
irradiate show mymodule.x_my_function__mutmut_1
```

For a complete walkthrough, see the [Quick Start guide](getting-started/quickstart.md).

## Status

Pre-alpha. The full pipeline works end-to-end on real projects. APIs and output formats will change without notice.

What's working: mutation generation, worker pool, stats collection, caching, `--isolate` flag, `results` and `show` subcommands.

What's missing: TUI browser, `--test-command` fallback, remote cache, type checker integration.
