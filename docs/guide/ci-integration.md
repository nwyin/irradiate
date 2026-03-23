# CI Integration

## GitHub Actions

irradiate provides a composite action for drop-in CI integration. It auto-detects GitHub Actions and emits inline `::warning` annotations on survived mutants, plus a Markdown step summary.

### Quick start (3 lines)

```yaml
- uses: actions/checkout@v4
  with:
    fetch-depth: 0
- uses: nwyin/irradiate@v0.1.1
  with:
    diff: origin/main
    fail-under: "80"
```

This will:

- Install irradiate and your project's test dependencies
- Only test functions changed in the PR (`diff: origin/main`)
- Fail the check if mutation score drops below 80%
- Add inline annotations on survived mutants
- Write a summary table to the step summary

### Full workflow example

```yaml
name: Mutation Testing

on:
  pull_request:
    branches: [main]

jobs:
  mutation-test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0

      - uses: nwyin/irradiate@v0.1.1
        id: mutants
        with:
          diff: origin/main
          fail-under: "80"
          report: json

      - name: Print score
        run: echo "Mutation score: ${{ steps.mutants.outputs.score }}% (${{ steps.mutants.outputs.killed }}/${{ steps.mutants.outputs.total }})"
```

### Action inputs

| Input | Description | Default |
|-------|-------------|---------|
| `version` | irradiate version to install | latest |
| `paths-to-mutate` | Source paths (space-separated) | from pyproject.toml |
| `tests-dir` | Test directory | from pyproject.toml |
| `diff` | Git ref for incremental mode (e.g. `origin/main`) | disabled |
| `fail-under` | Minimum mutation score (0-100) | no threshold |
| `sample` | Random sample fraction (0.1) or count (100) | all mutants |
| `report` | Report format (`json` or `html`) | none |
| `report-output` | Report output path | auto |
| `workers` | Number of parallel workers | CPU count |
| `python-version` | Python version | 3.12 |
| `extra-args` | Additional `irradiate run` arguments | none |

### Action outputs

| Output | Description | Example |
|--------|-------------|---------|
| `score` | Mutation score percentage | `85.7` |
| `killed` | Killed mutant count | `120` |
| `survived` | Survived mutant count | `21` |
| `total` | Total tested mutants | `141` |

### Caching results

Cache irradiate's content-addressable result store between runs:

```yaml
      - name: Cache irradiate results
        uses: actions/cache@v4
        with:
          path: .irradiate/cache/
          key: irradiate-${{ runner.os }}-${{ hashFiles('src/**/*.py') }}
          restore-keys: irradiate-${{ runner.os }}-
```

### Full run (scheduled)

For nightly or release-branch checks, run against all code:

```yaml
      - uses: nwyin/irradiate@v0.1.1
        with:
          fail-under: "70"
          report: html

      - uses: actions/upload-artifact@v4
        with:
          name: mutation-report
          path: irradiate-report.html
```

### Manual setup (without the action)

If you prefer not to use the composite action:

```yaml
      - uses: actions/setup-python@v5
        with:
          python-version: "3.12"

      - run: pip install pytest irradiate

      - run: irradiate run --diff origin/main --fail-under 80 --report json
```

## Exit codes

| Code | Meaning |
|------|---------|
| 0 | Run completed, score above `--fail-under` (or no threshold set) |
| 1 | Score below `--fail-under` threshold |

## What to gate on

Start with `--diff main --fail-under 80` on PRs. This only tests changed code and adds minimal friction. You can also run `--report json` without a threshold to upload as an artifact for manual review. For release branches, consider `--fail-under 70` on the whole codebase. Tighten over time.

## Other CI systems

irradiate is a standalone binary. Any CI that can run `pip install irradiate` and `irradiate run` works. The GitHub Actions annotations are specific to GitHub, but `--report json` and `--fail-under` work everywhere.
