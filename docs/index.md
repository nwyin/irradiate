# irradiate

Fast mutation testing for Python, written in Rust.

irradiate finds gaps in your test suite by modifying your code and checking whether your tests catch the changes. A test suite that passes with mutated code has a surviving mutant — a blind spot where bugs can hide.

## Why irradiate?

Mutation testing is slow because most tools invoke `pytest` from scratch for every mutant — hundreds of cold starts, hundreds of seconds of overhead. irradiate keeps a pool of pre-warmed pytest workers alive and forks a child for each mutant. Pytest starts once. Tests run many times.

- **Fork-per-mutant isolation** — no state leakage, no pytest restart
- **Rust orchestration** — tokio async, no GIL, native signal handling
- **27 mutation operators** — tree-sitter parser, parallel via rayon
- **Content-addressable cache** — SHA-256 of function body + tests; survives rebases and branch switches
- **Incremental mode** — `--diff main` to test only changed functions
- **Reporting** — JSON (Stryker schema v2), HTML, GitHub Actions annotations

## Quick start

```bash
pip install irradiate

irradiate run --paths-to-mutate src/
irradiate results
irradiate show mymodule.x_my_function__irradiate_1
```

For a complete walkthrough, see the [Quick Start guide](getting-started/quickstart.md).
