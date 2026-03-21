# Installation

irradiate is pre-alpha software. There are no binary releases yet — you need to build from source.

## Prerequisites

- **Rust 1.70+** — install via [rustup](https://rustup.rs/)
- **Python 3.10+** — with `pytest` installed in your target project's environment
- **Git** — to clone the repository

## Build from source

```bash
git clone https://github.com/tau/irradiate
cd irradiate
cargo build --release
```

The binary is at `target/release/irradiate`. Add it to your `PATH`:

```bash
# Add to PATH for the current session
export PATH="$PWD/target/release:$PATH"

# Or copy to a directory already on PATH
cp target/release/irradiate ~/.local/bin/
```

## Python environment

irradiate embeds its own test harness — you don't install anything extra into your project. Your project just needs `pytest` available:

```bash
# Using uv (recommended)
uv pip install pytest

# Or pip
pip install pytest
```

irradiate discovers the Python interpreter via `python3` by default. Override with `--python`:

```bash
irradiate run --python .venv/bin/python
```

## Verify the install

```bash
irradiate --version
```

## What irradiate does NOT require

- No changes to your `pyproject.toml` (though you can add config there)
- No pytest plugins in your project
- No `conftest.py` modifications
- No coverage configuration

irradiate injects its harness at runtime via `PYTHONPATH`.
