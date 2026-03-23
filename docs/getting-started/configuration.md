# Configuration

irradiate reads configuration from `pyproject.toml` under `[tool.irradiate]`. All settings are optional — CLI flags override config values.

## pyproject.toml

```toml
[tool.irradiate]
paths_to_mutate = "src/"
tests_dir = "tests/"
do_not_mutate = ["**/generated/*", "**/vendor/*"]
also_copy = ["data/"]
pytest_add_cli_args = ["-x", "--tb=short"]
```

### Settings

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `paths_to_mutate` | string | `"src"` | Source directory to mutate |
| `tests_dir` | string | `"tests"` | Test directory |
| `do_not_mutate` | list of strings | `[]` | Glob patterns for files to skip |
| `also_copy` | list of strings | `[]` | Extra directories to copy into the mutants tree |
| `pytest_add_cli_args` | list of strings | `[]` | Extra arguments passed to every pytest invocation |

### Backward compatibility

`[tool.mutmut]` is accepted with a deprecation warning. Rename to `[tool.irradiate]`.

## CLI flags

All flags are for `irradiate run`. Run `irradiate run --help` for the full list.

### Source and tests

```
--paths-to-mutate <PATH>    Source directory to mutate (overrides config)
--tests-dir <DIR>           Test directory (overrides config)
--python <PATH>             Python interpreter [default: python3]
--pytest-args <ARGS>        Extra pytest arguments (appends to config)
```

### Incremental and filtering

```
--diff <REF>          Only mutate functions changed since this git ref
--covered-only        Skip mutants with no test coverage
--no-stats            Skip coverage collection; test all mutants against all tests
--fail-under <SCORE>  Exit 1 if mutation score is below this threshold (0-100)
```

### Execution

```
--workers <N>                   Number of worker processes [default: CPU count]
--timeout-multiplier <FLOAT>    Per-mutant timeout multiplier [default: 10.0]
--worker-recycle-after <N>      Respawn workers after N mutants [default: auto]
--max-worker-memory <MB>        Recycle workers exceeding this RSS [default: 0 = off]
--isolate                       Fresh subprocess per mutant (slower, fully isolated)
--verify-survivors              Re-test survivors in isolate mode after the main run
```

### Reporting

```
--report <FORMAT>    Generate report: json or html
-o, --output <PATH>  Output path [default: irradiate-report.<format>]
```

GitHub Actions annotations and step summary are auto-detected when `GITHUB_ACTIONS=true`.

## Example

```toml
[tool.irradiate]
paths_to_mutate = "src/mylib"
tests_dir = "tests/unit"
do_not_mutate = ["**/migrations/*"]
pytest_add_cli_args = ["-x", "--timeout=30"]
```

```bash
irradiate run --workers 4 --diff main --fail-under 80 --report json
```
