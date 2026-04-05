---
title: Configuration — irradiate
description: Configure irradiate via pyproject.toml. Set source paths, test directories, exclusion patterns, and pytest arguments.
---

# Configuration

irradiate reads configuration from `pyproject.toml` under `[tool.irradiate]`. All settings are optional, and CLI flags override config values.

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

| Key                   | Type            | Default   | Description                                       |
| --------------------- | --------------- | --------- | ------------------------------------------------- |
| `paths_to_mutate`     | string          | `"src"`   | Source directory to mutate                        |
| `tests_dir`           | string          | `"tests"` | Test directory                                    |
| `do_not_mutate`       | list of strings | `[]`      | Glob patterns for files to skip                   |
| `also_copy`           | list of strings | `[]`      | Extra directories to copy into the mutants tree   |
| `pytest_add_cli_args` | list of strings | `[]`      | Extra arguments passed to every pytest invocation |
| `cache_pre_sync`      | string          | --        | Shell command run before mutation testing (e.g. download cache) |
| `cache_post_sync`     | string          | --        | Shell command run after mutation testing (e.g. upload cache) |
| `cache_max_age`       | string          | `"30d"`   | Default max-age for `irradiate cache gc` |
| `cache_max_size`      | string          | `"1gb"`   | Default max-size for `irradiate cache gc` |
| `type_checker`        | string          | --        | Type checker preset (`mypy`, `pyright`, `ty`) or raw command |
| `workers`             | integer         | CPU count | Number of worker processes |
| `max_worker_memory_mb`| integer         | 1024 (macOS), 0 (Linux) | Recycle workers exceeding this RSS in MB. 0 = off |

The `do_not_mutate` patterns can also be passed from the CLI with `--ignore` (merged with config values):

```bash
irradiate run --ignore "src/vendor/*" --ignore "src/generated/*"
```

These arguments are forwarded to **all** pytest invocations — stats collection, validation, and per-mutant test runs. This is useful for ignoring test directories that fail collection, setting timeouts, or enabling plugins:

```toml
[tool.irradiate]
pytest_add_cli_args = ["--ignore=tests/integration", "--timeout=30"]
```

The same arguments can be passed from the CLI with `--pytest-args`:

```bash
irradiate run src --pytest-args "--ignore=tests/integration"
```

See [Remote Cache](../guide/remote-cache.md) for details on cache sync hooks and garbage collection.

### `mutmut` compatibility

`[tool.mutmut]` is accepted with a deprecation warning. Rename to `[tool.irradiate]`.

## CLI flags

All flags are for `irradiate run`. Run `irradiate run --help` for the full list.

### Source and tests

```
[PATHS]...                  Source paths to mutate (positional, overrides config)
--paths-to-mutate <PATH>    Alias for positional PATHS (backward-compatible)
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
--sample <N>          Test a random subset of mutants (0.0-1.0 = fraction, >1 = count)
--sample-seed <N>     RNG seed for --sample [default: 0]
```

### Execution

```
--workers <N>                   Number of worker processes [default: CPU count]
--timeout-multiplier <FLOAT>    Per-mutant timeout multiplier [default: 10.0]
--max-worker-memory <MB>        Recycle workers exceeding this RSS [default: 1024 on macOS, 0 on Linux]
--no-fork                       Run tests in-process within workers [default on macOS]
--fork                          Force fork-per-mutant even on macOS
--isolate                       Fresh subprocess per mutant (slower, fully isolated)
--verify-survivors              Re-test survivors in isolate mode after the main run
```

### Memory management

On macOS, irradiate defaults to `--no-fork` mode and sets a 1024MB per-worker memory limit. These defaults prevent kernel panics from memory exhaustion (macOS has no OOM killer — when memory runs out, the entire system crashes). On Linux, fork mode is enabled and memory limits are off by default since the OOM killer provides a safety net.

To tune for your machine:

```toml
[tool.irradiate]
workers = 4                 # reduce from CPU count if RAM is limited
max_worker_memory_mb = 256  # recycle workers earlier for tighter memory control
```

Pass `--max-worker-memory 0` to disable memory recycling entirely.

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
irradiate run src/mylib --workers 4 --diff main --fail-under 80 --report json
```
