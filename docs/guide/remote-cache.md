---
title: Remote Cache — irradiate
description: Share irradiate mutation testing cache across CI runs using sync hooks and any remote storage backend (S3, GCS, rsync).
---

# Remote Cache

irradiate caches mutation test results locally in `.irradiate/cache/`. Each entry is a small JSON file keyed by a SHA-256 hash of the mutant, its surrounding function, and the test files. When the same mutation is encountered again with unchanged tests, the cached result is reused instantly.

A **remote cache** lets CI pipelines share these results, so the same mutant is never re-tested across runs, branches, or developers. irradiate doesn't include a built-in remote backend — instead, it provides **sync hooks** that shell out to your preferred tool (`aws s3 sync`, `gsutil rsync`, `rsync`, etc.).

## How it works

```
┌─────────────────────────────────────────────────┐
│  irradiate run                                  │
│                                                 │
│  1. cache_pre_sync  →  download remote → local  │
│  2. mutation testing (reads/writes local cache)  │
│  3. cache_post_sync →  upload local → remote     │
└─────────────────────────────────────────────────┘
```

The hooks run **once per `irradiate run`** — before cache lookups and after cache writes. If a hook fails (non-zero exit), irradiate logs a warning and continues. A sync failure never blocks mutation testing.

## Configuration

### pyproject.toml

```toml
[tool.irradiate]
cache_pre_sync = "aws s3 sync s3://my-bucket/irradiate-cache .irradiate/cache --quiet"
cache_post_sync = "aws s3 sync .irradiate/cache s3://my-bucket/irradiate-cache --quiet"
```

### CLI flags

CLI flags override pyproject.toml values:

```bash
irradiate run \
  --cache-pre-sync "aws s3 sync s3://bucket/cache .irradiate/cache" \
  --cache-post-sync "aws s3 sync .irradiate/cache s3://bucket/cache"
```

### Environment variables

Hook commands receive these environment variables:

| Variable               | Description                                       |
| ---------------------- | ------------------------------------------------- |
| `IRRADIATE_CACHE_DIR`  | Absolute path to `.irradiate/cache/`              |
| `IRRADIATE_PROJECT_DIR`| Absolute path to the project root                 |

These are useful for writing generic sync scripts that don't hardcode paths.

### Behavior notes

- Hooks execute via `sh -c "<command>"` with the project root as working directory.
- If a hook exits non-zero, a warning is printed but the run continues.
- Hooks fire even when `--no-cache` is set (you may want to sync other artifacts).
- If no hook is configured, nothing happens — no log noise.

## Cache garbage collection

Over time the cache grows. Use `irradiate cache gc` to prune old or excess entries:

```bash
# Prune entries older than 30 days, cap at 1 GB (defaults)
irradiate cache gc

# Custom thresholds
irradiate cache gc --max-age 7d --max-size 500mb

# Preview what would be pruned
irradiate cache gc --dry-run
```

### GC options

| Flag         | Default | Description                                                          |
| ------------ | ------- | -------------------------------------------------------------------- |
| `--max-age`  | `30d`   | Delete entries whose file modification time is older than this       |
| `--max-size` | `1gb`   | After age pruning, evict oldest entries until total size is under this |
| `--dry-run`  |         | Show what would be pruned without deleting                           |

### Duration format

Combine units: `30d`, `24h`, `1h30m`, `90m`, `3600s`, `1d12h`.

### Size format

Case-insensitive: `500mb`, `1gb`, `100kb`, `1024b`.

### pyproject.toml defaults

```toml
[tool.irradiate]
cache_max_age = "14d"
cache_max_size = "500mb"
```

CLI flags override config values. If neither is set, defaults are `30d` and `1gb`.

## GitHub Actions recipe

This workflow downloads the shared cache before running irradiate and uploads it afterward. Cache entries are naturally keyed by content, so stale entries don't cause wrong results — they just take up space (use GC to manage).

```yaml
name: Mutation Testing
on: [push]

jobs:
  mutate:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Set up Python
        uses: actions/setup-python@v5
        with:
          python-version: "3.12"

      - name: Install dependencies
        run: |
          pip install pytest
          pip install irradiate

      - name: Restore cache from S3
        env:
          AWS_ACCESS_KEY_ID: ${{ secrets.AWS_ACCESS_KEY_ID }}
          AWS_SECRET_ACCESS_KEY: ${{ secrets.AWS_SECRET_ACCESS_KEY }}
        run: |
          mkdir -p .irradiate/cache
          aws s3 sync s3://my-bucket/irradiate-cache .irradiate/cache --quiet || true

      - name: Run mutation testing
        run: irradiate run --fail-under 80

      - name: Upload cache to S3
        if: always()
        env:
          AWS_ACCESS_KEY_ID: ${{ secrets.AWS_ACCESS_KEY_ID }}
          AWS_SECRET_ACCESS_KEY: ${{ secrets.AWS_SECRET_ACCESS_KEY }}
        run: |
          irradiate cache gc --max-age 14d --max-size 500mb
          aws s3 sync .irradiate/cache s3://my-bucket/irradiate-cache --quiet || true
```

### Using sync hooks instead

Alternatively, configure the sync commands as hooks so irradiate manages the timing:

```yaml
      - name: Run mutation testing
        env:
          AWS_ACCESS_KEY_ID: ${{ secrets.AWS_ACCESS_KEY_ID }}
          AWS_SECRET_ACCESS_KEY: ${{ secrets.AWS_SECRET_ACCESS_KEY }}
        run: |
          irradiate run --fail-under 80 \
            --cache-pre-sync "aws s3 sync s3://my-bucket/irradiate-cache .irradiate/cache --quiet || true" \
            --cache-post-sync "irradiate cache gc --max-age 14d --max-size 500mb && aws s3 sync .irradiate/cache s3://my-bucket/irradiate-cache --quiet || true"
```

### Using GitHub Actions cache (no S3)

For simpler setups without S3, use the built-in GitHub Actions cache:

```yaml
      - name: Cache irradiate results
        uses: actions/cache@v4
        with:
          path: .irradiate/cache
          key: irradiate-${{ runner.os }}-${{ hashFiles('src/**/*.py', 'tests/**/*.py') }}
          restore-keys: |
            irradiate-${{ runner.os }}-

      - name: Run mutation testing
        run: irradiate run --fail-under 80
```

This approach doesn't need sync hooks at all — GitHub Actions handles the save/restore automatically based on the cache key.

## Config reference

All new configuration keys:

| Key               | Type   | CLI flag             | Default | Description                                   |
| ----------------- | ------ | -------------------- | ------- | --------------------------------------------- |
| `cache_pre_sync`  | string | `--cache-pre-sync`   | --      | Shell command to run before mutation testing   |
| `cache_post_sync` | string | `--cache-post-sync`  | --      | Shell command to run after mutation testing    |
| `cache_max_age`   | string | `--max-age`          | `30d`   | Default max age for `irradiate cache gc`       |
| `cache_max_size`  | string | `--max-size`         | `1gb`   | Default max size for `irradiate cache gc`      |
