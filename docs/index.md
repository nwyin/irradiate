# irradiate

Fast mutation testing for Python, written in Rust.

irradiate finds gaps in your test suite by modifying your code and checking whether your tests catch the changes. A test suite that passes with mutated code has a surviving mutant, a blind spot where bugs can hide.

## Why irradiate?

Mutation testing is slow because most tools invoke `pytest` from scratch for every mutant. Hundreds of cold starts, hundreds of seconds of overhead. irradiate keeps a pool of pre-warmed pytest workers alive and forks a child for each mutant. Pytest starts once. Tests run many times.

The parser uses tree-sitter with 27 mutation operator categories and runs in parallel via rayon. Results are cached with SHA-256 content addressing, so they survive rebases and branch switches. Incremental mode (`--diff main`) restricts testing to changed functions. Reports come in JSON (Stryker schema v2), HTML, and GitHub Actions annotations.

## Quick start

```bash
pip install irradiate

irradiate run --paths-to-mutate src/
irradiate results
irradiate show mymodule.x_my_function__irradiate_1
```

For a complete walkthrough, see the [Quick Start guide](getting-started/quickstart.md).
