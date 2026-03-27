# Contributing

Developer guide for working on irradiate.

## Building and testing

```bash
# Build
cargo build

# Check + lint (must pass before every commit)
cargo check && cargo clippy -- -D warnings

# Run unit tests
cargo test

# Run e2e tests
bash tests/e2e.sh

# Full verification (run before every commit)
cargo check && cargo clippy -- -D warnings && cargo test && bash tests/e2e.sh
```

### Python environment

The integration tests and e2e tests need Python 3.10+ with pytest. Set up the fixture venvs with:

```bash
cd tests/fixtures/simple_project && uv venv --python 3.12 && uv pip install pytest
cd tests/fixtures/regex_project && uv venv --python 3.12 && uv pip install pytest
```

### Git hooks

Install pre-commit hooks:

```bash
bash scripts/install-hooks.sh
```

Runs `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test` before every commit.

## Analysis tools

```bash
# Module dependency graph — outputs graphviz dot format
# Requires: cargo install cargo-modules
cargo modules dependencies --lib --no-fns --no-traits --no-types

# Module tree — shows the crate structure
cargo modules structure --lib

# Render docs locally
# Requires: pip install mkdocs-material mkdocs-minify-plugin
uvx --with mkdocs-material --with mkdocs-minify-plugin mkdocs serve
```

## Project structure

```
irradiate/
├── Cargo.toml
├── src/
│   ├── main.rs               # binary entrypoint (clap CLI)
│   ├── lib.rs                # library root
│   ├── pipeline.rs           # pipeline conductor (phases 1-5)
│   ├── mutation.rs           # shared types (Mutation, FunctionMutations)
│   ├── tree_sitter_mutation.rs # mutation engine (27 operators)
│   ├── regex_mutation.rs     # regex pattern mutation operators
│   ├── codegen.rs            # trampolined source file generation
│   ├── trampoline.rs         # trampoline code generation
│   ├── orchestrator.rs       # tokio worker pool manager
│   ├── cache.rs              # content-addressable result cache
│   ├── stats.rs              # coverage + timing collection
│   ├── report.rs             # output: terminal, JSON, HTML, GitHub
│   ├── protocol.rs           # IPC message types
│   ├── config.rs             # pyproject.toml parsing
│   ├── git_diff.rs           # incremental mode (--diff)
│   ├── harness.rs            # extract embedded Python harness
│   ├── progress.rs           # terminal progress bar
│   └── trace.rs              # chrome tracing output
├── harness/                  # Python harness (embedded via include_str!)
│   ├── __init__.py           # active_mutant global, hit recording
│   ├── worker.py             # pytest worker process (fork-per-mutant)
│   ├── stats_plugin.py       # pytest plugin for stats collection
│   └── import_hook.py        # MutantFinder import hook
├── tests/
│   ├── mutation_tests.rs     # 282 mutation operator tests
│   ├── proptest_mutation.rs  # property-based mutation tests
│   ├── worker_pool_integration.rs
│   ├── fixtures/             # minimal Python projects for testing
│   └── e2e.sh               # end-to-end test script
├── vendor/
│   └── mutmut/              # reference implementation (read-only)
└── docs/                    # mkdocs site source
```

## Module layering

The dependency graph is a clean DAG with four layers. Each layer only imports from layers below it.

```
Layer 0 (types):      protocol, mutation, config
Layer 1 (engines):    tree_sitter_mutation, regex_mutation, trampoline, cache, stats, git_diff, harness
Layer 2 (subsystems): codegen, orchestrator, report, progress, trace
Layer 3 (conductor):  pipeline
Layer 4 (entry):      main
```

See the [Architecture](architecture.md) page for the full dependency graph.

## Commit guidelines

- Run `cargo check && cargo clippy -- -D warnings && cargo test` before every commit
- One logical change per commit
- Keep commits small and incremental

## Pre-release checklist

1. **Tests pass**: `cargo check && cargo clippy -- -D warnings && cargo test && bash tests/e2e.sh`
2. **Version bump**: Update `version` in `Cargo.toml` (pyproject.toml reads it via maturin)
3. **CHANGELOG**: Add entry with date, version, and what changed. Write for users.
4. **Docs current**: Skim `docs/` for stale content. New flags or changed defaults must be documented.
5. **README check**: Example output, feature list, comparison table still accurate?
6. **Smoke test**: `cargo build --release && cd ~/projects/hive && irradiate run --sample 0.1`
7. **Git state clean**: No uncommitted changes.
8. **Tag + push**: `git tag v0.X.Y && git push && git push --tags`

## Reference: mutmut naming conventions

irradiate follows mutmut's naming loosely:

- Top-level function `foo()` → mangled as `x_foo`
- Class method `Class.foo()` → mangled as `xǁClassǁfoo` (Unicode separator `ǁ` U+01C1)
- Mutant variants: `x_foo__irradiate_orig`, `x_foo__irradiate_1`, `x_foo__irradiate_2`, ...
- Mutant keys: `module.x_foo__irradiate_1`
- Metadata files: `mutants/path/to/file.py.meta`
- Runtime control: `irradiate_harness.active_mutant` global
