# Configuration

irradiate reads configuration from `pyproject.toml` under `[tool.mutmut]` (the `mutmut` section — intentional, for backward compatibility with mutmut projects).

## pyproject.toml

```toml
[tool.mutmut]
paths_to_mutate = "src/"
tests_dir = "tests/"
```

All settings are optional. CLI flags override config file values.

### Available settings

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `paths_to_mutate` | string | `"src"` | Directory containing source code to mutate |
| `tests_dir` | string | `"tests"` | Directory containing the test suite |
| `do_not_mutate` | list of strings | `[]` | Patterns of code to skip (e.g., `["# pragma: no mutate"]`) |

## CLI flags

All flags are for `irradiate run`. Run `irradiate run --help` for the full list.

### Core options

```
--paths-to-mutate <PATH>
    Path(s) to source code to mutate
    Overrides pyproject.toml paths_to_mutate

--tests-dir <DIR>
    Path to test directory
    Overrides pyproject.toml tests_dir

--python <PATH>
    Python interpreter to use (default: python3)
    Use this to point at a virtualenv: --python .venv/bin/python
```

### Worker pool

```
--workers <N>
    Number of worker processes (default: number of CPUs)

--worker-recycle-after <N>
    Respawn workers after N mutants to prevent pytest state accumulation
    Default: auto-tune (100 normally, 20 when session-scoped fixtures detected)
    Set 0 to disable recycling

--max-worker-memory <MB>
    Recycle workers whose RSS exceeds this threshold in megabytes
    Default: 0 (disabled)
```

### Timing and timeouts

```
--timeout-multiplier <FLOAT>
    Timeout per mutant as a multiple of the baseline test duration (default: 10.0)
    A baseline of 0.5s means each mutant gets 5s before being killed
```

### Test selection

```
--no-stats
    Skip the stats collection run; test all mutants against all tests
    Slower but avoids the stats run overhead for small projects

--covered-only
    Skip mutants with no test coverage (no stats data)
```

### Isolation and correctness

```
--isolate
    Run each mutant in a fresh subprocess
    Slower — eliminates warm-session state entirely
    Use when you suspect state leakage between mutants

--verify-survivors
    After the main run, re-test survived mutants in isolate mode
    Catches false negatives from warm-session state leakage
    No-op when --isolate is already set
```

## Example: full pyproject.toml config

```toml
[tool.mutmut]
paths_to_mutate = "src/mylib"
tests_dir = "tests/unit"
do_not_mutate = ["# pragma: no mutate"]
```

Then run with extra CLI options:

```bash
irradiate run --workers 8 --verify-survivors
```
