# irradiate

Mutation testing for Python, written in Rust. Spiritual successor to mutmut.

## Building and testing

```bash
# Check + lint (must pass before every commit)
cargo check && cargo clippy -- -D warnings

# Run unit tests
cargo test

# Run e2e tests
bash tests/e2e.sh

# Full verification
cargo check && cargo clippy -- -D warnings && cargo test && bash tests/e2e.sh
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

## Design docs

- `docs/internals/architecture.md` — module dependency graph, execution pipeline, design rationale
- `docs/internals/contributing.md` — project structure, layering, pre-release checklist, analysis tools
- `docs/internals/mutation-operators.md` — cross-framework operator catalog
- `docs/internals/regex-mutation-plan.md` — regex mutation implementation plan
- `docs/mutation-pruning.md` — mutation pruning strategies with academic citations

## Module layering

```
Layer 0 (types):      protocol, mutation, config
Layer 1 (engines):    tree_sitter_mutation, regex_mutation, trampoline, cache, stats, git_diff, harness
Layer 2 (subsystems): codegen, orchestrator, report, progress, trace
Layer 3 (conductor):  pipeline
Layer 4 (entry):      main
```

Each layer only imports from layers below it. No cycles.

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
