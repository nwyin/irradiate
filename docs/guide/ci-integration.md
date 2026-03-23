# CI Integration

## GitHub Actions

irradiate auto-detects GitHub Actions and emits inline `::warning` annotations on survived mutants, plus a Markdown step summary.

### Basic workflow

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
          fetch-depth: 0  # needed for --diff

      - uses: actions/setup-python@v5
        with:
          python-version: "3.12"

      - name: Install dependencies
        run: pip install pytest irradiate

      - name: Run mutation testing (incremental)
        run: irradiate run --diff origin/main --fail-under 80 --report json
```

This will:
- Only test functions changed in the PR (`--diff origin/main`)
- Fail the check if mutation score drops below 80%
- Generate a JSON report
- Add inline annotations on survived mutants (automatic in GH Actions)
- Write a summary table to the step summary

### Caching results

Cache irradiate's content-addressable result store between runs:

```yaml
      - name: Cache irradiate results
        uses: actions/cache@v4
        with:
          path: mutants/
          key: irradiate-${{ runner.os }}-${{ hashFiles('src/**/*.py') }}
          restore-keys: irradiate-${{ runner.os }}-
```

### Full run (not incremental)

For scheduled or release-branch checks, run against all code:

```yaml
      - name: Full mutation test
        run: irradiate run --fail-under 70 --report html

      - name: Upload HTML report
        uses: actions/upload-artifact@v4
        with:
          name: mutation-report
          path: irradiate-report.html
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
