# irradiate

Mutation testing for Python, written in Rust. Spiritual successor to mutmut.

## Reference implementation

mutmut source is at `vendor/mutmut/` for reference. Key files:

- `vendor/mutmut/src/mutmut/__main__.py` — runner, config, orchestration (66KB, the big one)
- `vendor/mutmut/src/mutmut/file_mutation.py` — mutation generation engine (libcst-based)
- `vendor/mutmut/src/mutmut/node_mutation.py` — 18+ mutation operators
- `vendor/mutmut/src/mutmut/trampoline_templates.py` — trampoline code generation and runtime dispatch
- `vendor/mutmut/src/mutmut/code_coverage.py` — coverage integration
- `vendor/mutmut/src/mutmut/type_checking.py` — type checker integration
- `vendor/mutmut/e2e_projects/` — test projects (my_lib, config, type_checking, etc.)

### mutmut naming conventions (we follow loosely)

- Top-level function `foo()` → mangled as `x_foo`
- Class method `Class.foo()` → mangled as `xǁClassǁfoo` (Unicode separator `ǁ` U+01C1)
- Mutant variants: `x_foo__mutmut_orig`, `x_foo__mutmut_1`, `x_foo__mutmut_2`, ...
- Mutant keys: `module.x_foo__mutmut_1`
- Metadata files: `mutants/path/to/file.py.meta`
- Runtime control: `MUTANT_UNDER_TEST` env var (mutmut) / `irradiate_harness.active_mutant` global (irradiate)

### mutmut exit codes

- `0` → survived, `1`/`3` → killed, `5`/`33` → no tests, `34` → skipped
- `36`/`24`/`152`/`255` → timeout, `37` → type check caught, `-11`/`-9` → segfault

## Building and testing

```bash
# Build
cargo build

# Check + lint (must pass before every commit)
cargo check && cargo clippy -- -D warnings

# Run unit tests
cargo test

# Run e2e tests (once e2e.sh exists)
bash tests/e2e.sh

# Full verification
cargo check && cargo clippy -- -D warnings && cargo test && bash tests/e2e.sh

# Module dependency graph (requires: cargo install cargo-modules)
cargo modules dependencies --lib --no-fns --no-traits --no-types  # dot format
cargo modules structure --lib                                      # tree format

# Render docs locally (requires: pip install mkdocs-material mkdocs-minify-plugin)
uvx --with mkdocs-material --with mkdocs-minify-plugin mkdocs serve
```

## Python environment

The integration tests need Python with pytest. Set up with:
```bash
cd tests/fixtures && uv venv && uv pip install pytest
```

## Git hooks

Install pre-commit hooks with:
```bash
bash scripts/install-hooks.sh
```

The hook runs `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test` before every commit.

## Commit guidelines

- Run `cargo check && cargo clippy -- -D warnings && cargo test` before every commit
- One logical change per commit
- Keep commits small and incremental — commit after completing each module/feature
- Do not note that PRs were authored by Claude Code

## Project structure

```
irradiate/
├── Cargo.toml
├── CLAUDE.md
├── src/
│   ├── main.rs             # binary entrypoint
│   ├── lib.rs              # library root
│   ├── harness.rs          # extract embedded Python harness at runtime
│   ├── orchestrator.rs     # tokio worker pool manager
│   ├── protocol.rs         # IPC message types
│   └── stats.rs            # stats collection
├── harness/                # Python harness files (embedded via include_str!)
│   ├── __init__.py         # irradiate_harness package (active_mutant global, etc.)
│   ├── worker.py           # pytest worker process
│   └── stats_plugin.py     # pytest plugin for stats collection
├── tests/
│   ├── worker_pool_integration.rs  # integration tests (real pytest workers)
│   ├── fixtures/            # minimal Python projects for testing
│   └── e2e.sh              # end-to-end test script
├── vendor/
│   └── mutmut/             # reference implementation (git clone, not modified)
└── docs/
    └── design.md           # architecture, design rationale, and execution model
```

## Design docs

- `docs/design.md` — full architecture and design rationale

## Pre-release checklist

Before every version bump / PyPI publish, run through this list:

1. **Tests pass**: `cargo check && cargo clippy -- -D warnings && cargo test && bash tests/e2e.sh`
2. **Version bump**: Update `version` in `Cargo.toml` (pyproject.toml reads it via maturin)
3. **CHANGELOG**: Add entry to `CHANGELOG.md` with date, version, and what changed (features, fixes, perf). Write for users, not developers.
4. **Docs current**: Skim `docs/` for stale content — especially `quickstart.md`, `cli.md`, `configuration.md`, `mutation-pruning.md`. Any new flags or changed defaults must be documented.
5. **README check**: Example output and feature list still accurate? Comparison table vs mutmut up to date?
6. **Smoke test on real project**: `cargo build --release && cd ~/projects/hive && irradiate run --sample 0.1` — verify no regressions on a real codebase, not just test fixtures.
7. **Git state clean**: No uncommitted changes, all work on main or a release branch.
8. **Tag + push**: `git tag v0.X.Y && git push && git push --tags` — the release workflow triggers on tags.
