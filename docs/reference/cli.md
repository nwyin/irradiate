# CLI Reference

## Global

```
irradiate [OPTIONS] <COMMAND>
```

| Flag | Description |
|------|-------------|
| `--version` | Print version |
| `--help` | Print help |


## `irradiate run`

Run mutation testing.

```bash
irradiate run [OPTIONS] [MUTANT_NAMES]...
```

### Arguments

| Argument | Description |
|----------|-------------|
| `[MUTANT_NAMES]...` | Specific mutant names to test. If omitted, all mutants are tested. |

### Options

| Flag | Type | Default | Description |
|------|------|---------|-------------|
| `--paths-to-mutate` | path | `"src"` | Source directory to mutate |
| `--tests-dir` | path | `"tests"` | Test directory |
| `--workers` | int | CPU count | Number of worker processes |
| `--timeout-multiplier` | float | `10.0` | Per-mutant timeout as multiple of baseline duration |
| `--no-stats` | flag | -- | Skip coverage collection; test all mutants against all tests |
| `--covered-only` | flag | -- | Skip mutants with no test coverage |
| `--python` | path | `"python3"` | Python interpreter |
| `--worker-recycle-after` | int | auto | Respawn workers after N mutants (0 to disable) |
| `--max-worker-memory` | int | `0` | Recycle workers exceeding this RSS in MB (0 = off) |
| `--isolate` | flag | -- | Fresh subprocess per mutant (slower, fully isolated) |
| `--verify-survivors` | flag | -- | Re-test survivors in isolate mode after main run |
| `--diff` | string | -- | Only mutate functions changed since this git ref |
| `--fail-under` | float | -- | Exit 1 if mutation score below this threshold (0-100) |
| `--report` | string | -- | Generate report: `json` or `html` |
| `-o, --output` | path | auto | Report output path |
| `--sample` | float | -- | Random mutant sample. 0.0-1.0 = fraction, >1 = count |
| `--sample-seed` | int | `0` | RNG seed for `--sample` (deterministic by default) |
| `--pytest-args` | string | -- | Extra arguments appended to every pytest invocation |

### Examples

```bash
irradiate run                                    # basic run
irradiate run --diff main                        # incremental: changed functions only
irradiate run --fail-under 80                    # CI gate
irradiate run --report html -o report.html       # HTML report
irradiate run --isolate                          # full subprocess isolation
irradiate run --verify-survivors                 # re-check survivors after warm run
irradiate run --python .venv/bin/python          # specific interpreter
irradiate run mylib.x_add__irradiate_3           # test one specific mutant
irradiate run --workers 4 --covered-only         # tuning
irradiate run --sample 0.1                       # test 10% of mutants (fast CI)
irradiate run --sample 50 --sample-seed 42       # test exactly 50, reproducible
```


## `irradiate results`

Display results from a previous run.

```bash
irradiate results [OPTIONS]
```

| Flag | Description |
|------|-------------|
| `--all` | Show all mutants (default: survived only) |
| `--json` | Output as JSON |


## `irradiate show`

Show the diff for a specific mutant.

```bash
irradiate show <MUTANT_NAME>
```

| Argument | Description |
|----------|-------------|
| `<MUTANT_NAME>` | e.g. `mylib.x_func__irradiate_1` |


## `irradiate cache clean`

Remove the local result cache.

```bash
irradiate cache clean
```


## Configuration

`run` options can also be set in `pyproject.toml` under `[tool.irradiate]`. CLI flags override config. See [Configuration](../getting-started/configuration.md).

## Mutant name format

| Python source | Mutant name |
|--------|-------------|
| `def add()` in `mylib` | `mylib.x_add__irradiate_1` |
| `class Foo` method `bar()` | `mylib.xǁFooǁbar__irradiate_1` |

The `x_` prefix avoids collisions. The `ǁ` (U+01C1) separator encodes class membership.
