# Installation

## From PyPI (recommended)

```bash
pip install irradiate
```

This installs a pre-built binary wheel. No Rust toolchain needed.

## Prerequisites

- **Python 3.10+** with `pytest` installed in your project's environment
- **macOS** (arm64, x86_64) or **Linux** (x86_64, aarch64)

## Build from source

If you need a development build or an unsupported platform:

```bash
git clone https://github.com/nwyin/irradiate
cd irradiate
cargo build --release
```

Requires Rust 1.70+. The binary is at `target/release/irradiate`.

## Python environment

irradiate embeds its own test harness at runtime — you don't install anything into your project. Your project just needs `pytest`:

```bash
uv pip install pytest
# or: pip install pytest
```

irradiate uses `python3` by default. Override with `--python`:

```bash
irradiate run --python .venv/bin/python
```

## Verify

```bash
irradiate --version
```

## What irradiate does NOT require

- No changes to your `pyproject.toml` (though you can add config there)
- No pytest plugins in your project
- No `conftest.py` modifications
- No coverage configuration
