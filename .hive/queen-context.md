# Queen Context

Persistent project knowledge accumulated across queen sessions.
Update this file with architectural decisions, gotchas, and patterns.

## Architecture Decisions

### Import model (2025-03-18)
PYTHONPATH shadowing replaced with MutantFinder import hook (sys.meta_path). The hook runs before sys.path is consulted, eliminating all three prior bugs (partial mutation, pytest config interference, sys.path[0] cwd shadowing). PYTHONPATH is now just `harness_dir:source_parent`. The hook is installed in harness/__init__.py when IRRADIATE_MUTANTS_DIR env var is set.

### Class method trampolining (2025-03-18)
codegen.rs now tracks class context during line walk. For class methods, the wrapper stays inside the class body (indented), while mangled orig/variants/dict go to module level. generate_trampoline() in trampoline.rs returns TrampolineOutput { module_code, wrapper_code, mutant_keys }.

### conftest.py skip (2025-03-18)
conftest.py is never mutated (is_mutatable_python_file() rejects it) and the import hook skips it too. conftest contains test configuration/fixtures, not application logic.

## Infrastructure State

### What exists
- CI: .github/workflows/ci.yml (audit, build, clippy, test, coverage, e2e)
- Forced-fail validation in pipeline.rs (runs after clean validation)
- Bench infrastructure: bench/compare.sh (5 configs), bench/summarize.py, bench/setup.sh
- Bench targets: simple_project (~10 mutants), my_lib (~30 mutants) — both too small for meaningful perf comparison
- Import hook Python tests: 47 tests in tests/harness_tests/ (all pass with correct venv)

### Vendor repo pattern
Follow pycfg-rs convention: scripts/bootstrap-vendors.sh does shallow clones into bench/corpora/ (gitignored). Each vendor needs its own venv. Bench target configs in bench/targets/<name>.sh export PROJECT_DIR, PATHS_TO_MUTATE, TESTS_DIR, PYTHON.

## Gotchas for Workers

- Python harness files are embedded via include_str! — edits to harness/*.py require cargo build to take effect
- bench/.venv has mutmut installed from vendor/mutmut (editable install)
- /usr/bin/time -l is macOS-specific; Linux uses different format
- irradiate reads [tool.mutmut] from pyproject.toml (not [tool.irradiate]) for backward compat
- Bench compare.sh uses process substitution for /usr/bin/time capture — bash-specific
