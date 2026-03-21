# Markupsafe Benchmark Notes

Date: 2026-03-21

This note records the ad hoc benchmark and profiling work done against
`bench/corpora/markupsafe`. It is an internal reference, not a polished
public benchmark report.

## Scope

Goals:

- Check whether the current "irradiate is much slower than mutmut on
  markupsafe" framing is actually supported by the local evidence.
- Identify where irradiate spends time on markupsafe.
- Use `samply` to separate Rust-orchestrator overhead from Python-worker
  overhead.

## Environment

- Repo: `irradiate`
- Target corpus: `bench/corpora/markupsafe`
- Python test env: `bench/corpora/markupsafe/.venv/bin/python3`
- Binary under test: `target/release/irradiate`
- Profiler: `samply`

For symbolized release profiling, the binary was rebuilt with debuginfo:

```bash
CARGO_PROFILE_RELEASE_DEBUG=1 cargo build --release
dsymutil target/release/irradiate -o target/release/irradiate.dSYM
```

## Methodology

### 1. Baseline corpus checks

Verify the corpus test suite passes:

```bash
cd bench/corpora/markupsafe
.venv/bin/python -m pytest tests -q --tb=short -x
```

Observed result:

- `79 passed, 1 skipped in 0.10s`

### 2. Existing irradiate run artifacts

The corpus already had recent irradiate artifacts in `.irradiate/` and
`mutants/`. Those were used to extract:

- total mutant count,
- per-mutant cached durations,
- stats coverage output.

Key commands used:

```bash
find .irradiate/cache -type f -name '*.json' | wc -l
python3 - <<'PY'
import json, pathlib, statistics
paths = list(pathlib.Path('.irradiate/cache').rglob('*.json'))
durs = [json.loads(p.read_text())['duration'] for p in paths]
print(len(durs), statistics.mean(durs), statistics.median(durs), sum(durs))
PY
```

### 3. `samply` capture

`samply` was run against a temp copy of the corpus so the local working copy
was not disturbed. The command used a Python timeout wrapper so the profile
would terminate cleanly after a few seconds of mutation execution:

```bash
TMP=/tmp/irradiate-markupsafe-profile
rsync -a --delete \
  --exclude '.git' \
  --exclude '.venv312' \
  --exclude '.irradiate' \
  --exclude 'mutants' \
  bench/corpora/markupsafe/ "$TMP"/

cd "$TMP"
samply record -s --unstable-presymbolicate \
  -o /tmp/irradiate-markupsafe-5s-debug.json.gz \
  -- python3 -c 'import subprocess; subprocess.run(
    ["/Users/tau/projects/irradiate/target/release/irradiate",
     "run",
     "--paths-to-mutate", "src/markupsafe",
     "--tests-dir", "tests",
     "--workers", "1",
     "--python", ".venv/bin/python3"],
    timeout=5
  )'
```

This profile includes startup, stats collection, validation, and early mutation
execution. It is good enough to answer "Rust or Python?" but not good enough to
attribute time to Python source lines.

### 4. Worker microbench

The worker hot path was measured by importing
`MutationWorkerPlugin` from `harness/worker.py` inside the markupsafe corpus,
letting pytest collect normally, and then timing:

- `_prepare_items(test_ids)`
- `_run_items_via_hooks(items)`
- `_restore_source_modules()`

This was done both with no active mutant and with real markupsafe mutant names.

### 5. Fresh pytest subprocess baseline

For the same selected test subsets, a fresh pytest subprocess was also timed
against the same mutated tree and harness import hook:

```bash
IRRADIATE_MUTANTS_DIR=$PWD/mutants \
PYTHONPATH=$PWD/.irradiate/harness \
.venv/bin/python3 -m pytest -q -p irradiate_harness <selected tests...>
```

This is not a full mutmut benchmark, but it is a good proxy for the vendored
mutmut execution path, which ultimately forks and calls `pytest.main(...)` in a
fresh child per mutant.

### 6. Session-fixture behavior check

Because the worker always calls:

```python
item.config.hook.pytest_runtest_protocol(item=item, nextitem=None)
```

an explicit probe was run to compare:

- `nextitem=None` for every item, versus
- real sequential `nextitem`

on markupsafe's autouse session fixture in `tests/conftest.py`.

## Results

### Corpus-level irradiate numbers

From the local markupsafe irradiate artifacts:

- total mutants: `479`
- killed: `267`
- survived: `210`
- errors: `2`

Cached worker-reported durations:

- mean per-mutant duration: `173.275ms`
- median per-mutant duration: `245.271ms`
- p90 per-mutant duration: `344.596ms`
- summed worker duration across all mutants: `82.999s`

### Stats coverage output

From `.irradiate/stats.json`:

- covered functions: `24`
- collected test nodeids: `39`
- total recorded `call.duration`: `107.81ms`
- mean recorded `call.duration` per collected test: `2.764ms`

Important caveat: irradiate currently stores only `call.duration`, not full
setup/call/teardown time.

Measured on a clean full-suite run:

- `call_total_ms`: `59.183`
- `setup_total_ms`: `5.650`
- `teardown_total_ms`: `2.787`
- `full_total_ms`: `67.620`
- `full_over_call_ratio`: `1.143`

So the stats file understates actual per-test runtime.

### Worker microbench results

Measured medians from the real worker code:

| Case | Selected tests | `_prepare_items` | `_run_items_via_hooks` | `_restore_source_modules` | Total |
| --- | ---: | ---: | ---: | ---: | ---: |
| `x_escape` with no active mutant | 30 | `0.013ms` | `70.591ms` | `0.006ms` | `70.609ms` |
| `x_escape_silent` with no active mutant | 1 | `0.001ms` | `0.376ms` | `0.002ms` | `0.379ms` |
| `markupsafe.x_escape__irradiate_3` | 30 | n/a | `102.093ms` | included | `102.099ms` |
| `markupsafe.x_escape__irradiate_1` | 30 | n/a | `105.257ms` | included | `105.263ms` |
| `markupsafe.x_escape_silent__irradiate_1` | 1 | n/a | `2.348ms` | included | `2.351ms` |

Takeaways:

- `_prepare_items()` is noise on markupsafe.
- `_restore_source_modules()` is also noise on markupsafe.
- The dominant cost is `_run_items_via_hooks()`.
- Real mutant execution adds cost on top of the no-mutant warm replay path.

### Fresh pytest subprocess baseline

Median wall-clock for a fresh subprocess running the same selected subsets:

| Case | Selected tests | Fresh subprocess median |
| --- | ---: | ---: |
| `x_escape` subset | 30 | `250.399ms` |
| `x_escape_silent` subset | 1 | `156.044ms` |

Takeaway:

- irradiate's warm worker is still significantly faster than a fresh pytest
  subprocess on the same selected subset.
- The local evidence does **not** support the idea that irradiate's worker path
  is slower than the old "fresh pytest per mutant" path.

### Session fixture behavior

markupsafe defines an autouse session fixture:

```python
@pytest.fixture(scope="session", autouse=True, params=(...))
def _mod(...):
    markupsafe._escape_inner = mod._escape_inner
```

Measured behavior for the first 3 items:

- with `nextitem=None` for every item: fixture setup count `3`, teardown count `3`
- with real sequential `nextitem`: fixture setup count `1`, teardown count `1`

Takeaway:

- the worker currently forces broader teardown/setup churn than a normal pytest
  run,
- but on markupsafe that fixture is cheap, so this is a correctness/perf smell,
  not the primary 70-100ms cost center.

### `samply` findings

High-level `samply` result:

- Rust Tokio worker threads were mostly parked in wait paths such as
  `Context::park_internal`, `tokio::runtime::time::Driver::park_internal`,
  `mio::Poll::poll`, and parking-lot waits.
- The active work was in the Python worker process.

Interpretation:

- Rust orchestration is not the hotspot on markupsafe.
- IPC is not the main bottleneck.
- The hot path is the Python-side replay of pytest items inside the persistent
  worker session.

## Mutmut comparison caveat

The current repository benchmark framing around mutmut is muddled.

Observed issues:

- The vendored mutmut code in `vendor/mutmut/` is not the same thing as the old
  mutmut package line used by `bench/.venv`.
- The packaged mutmut benchmark target did not run cleanly on modern
  markupsafe without a temp-copy workaround because it could not handle the
  corpus `pyproject.toml`.
- In a temp-copy workaround, the old mutmut package generated `144` mutants,
  while irradiate generated `479` on the same corpus.

Therefore:

- raw wall-clock numbers are not directly comparable,
- any "X times slower/faster than mutmut" claim must state the mutmut version,
  execution model, and mutant counts,
- `ms/mutant` is the minimum sane normalization.

## Why mutmut 3 can beat irradiate

The vendored mutmut version is `3.5.0`, and its execution model matters.

Relevant implementation details:

- mutmut uses a trampoline too; its per-call mutant switch is not
  fundamentally different from irradiate's. The switch itself is probably not
  where the large gap comes from.
- mutmut sets `fork` mode and then forks a child per mutant from a warmed
  parent process. This gives cheap copy-on-write isolation without replaying
  pytest item hooks inside a long-lived session.
- mutmut runs tests through normal `pytest.main(...)` in the child, so pytest
  keeps its usual fixture/setup/teardown behavior.
- mutmut sorts mutants by estimated cost and sorts selected tests by recorded
  duration before running each mutant.

Relevant irradiate differences:

- irradiate replays collected pytest items directly inside a long-lived worker
  session via `pytest_runtest_protocol(...)`.
- irradiate currently preserves collection order when replaying tests instead
  of running the fastest likely-killing tests first.
- irradiate currently calls `pytest_runtest_protocol(item=item, nextitem=None)`
  for every item, which forces broader teardown/setup churn than a normal
  sequential pytest run.

Practical interpretation:

- On fast suites, mutmut's `fork`-from-warm-parent model can be cheaper than
  irradiate's current "manual replay inside one pytest session" model.
- The markupsafe measurements line up with this: the hot path is the worker's
  pytest replay, not Rust orchestration and not the trampoline switch.

## Where irradiate should do better

There are still classes of projects where irradiate should be competitive or
better than mutmut.

Most likely cases:

- Projects where a mutant maps to many tests and a persistent warm worker can
  amortize collection/startup costs across lots of work.
- Projects with expensive interpreter or plugin startup, where reusing a warm
  pytest session matters more than perfect per-mutant isolation.
- Projects that benefit from irradiate's worker pool more than from mutmut's
  current child scheduling.
- Cases where irradiate's isolate mode is not needed and warm-session replay
  remains correct.

Less likely cases:

- Tiny library test suites with very low per-test overhead. Those are exactly
  where replay overhead becomes a large fraction of total time, and where
  mutmut's warmed-parent `fork` model is especially attractive.

## Can irradiate close the gap?

Probably yes in part, and maybe substantially, but not by optimizing Rust-side
orchestration.

High-confidence improvements:

- pass the real `nextitem` while replaying a batch,
- sort selected tests by recorded duration before replay,
- sort mutants by estimated duration before dispatch,
- record full setup+call+teardown timing instead of only `call.duration`.

Possible larger architectural move:

- keep a warm parent worker and `fork` per mutant from that warmed state,
  instead of replaying pytest items manually inside one long-lived session.

If markupsafe is representative, the biggest wins are likely to come from the
Python execution model, not from socket or Tokio tuning.

## Conclusions

1. On markupsafe, the main irradiate hotspot is the Python worker's
   `pytest_runtest_protocol(...)` replay path, not Rust, not sockets, not item
   preparation, and not module restore.
2. The worker's correctness-first `nextitem=None` behavior is real and should be
   fixed, but it does not explain most of the measured markupsafe overhead.
3. The stats pipeline currently understates real runtime because it records only
   `call.duration`.
4. The current mutmut comparison story in the repo should be treated as
   provisional until a version-pinned, mutant-count-normalized markupsafe target
   is wired into `bench/`.

## Most likely next steps

- Change worker batching to pass the real `nextitem` when replaying a batch.
- Record or estimate full setup+call+teardown duration, not only `call.duration`.
- Add a first-class `markupsafe` benchmark target under `bench/`.
- Re-run the comparison with explicit mutant counts and `ms/mutant`.
