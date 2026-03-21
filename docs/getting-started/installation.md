# Installation

irradiate is a Rust binary that runs against your Python project. You need Rust to build it and Python with pytest to run tests.

## Prerequisites

- **Rust** 1.70+
- **Python** 3.10+
- **pytest** installed in your project's virtual environment

## Build from source

```bash
git clone https://github.com/nwyin/irradiate
cd irradiate
cargo build --release
```

The binary lands at `target/release/irradiate`. Add it to your `PATH` or invoke it directly.

## Verify

```bash
irradiate --version
```

## Python dependencies

irradiate ships a small Python harness (`irradiate_harness`) that it extracts automatically at runtime. You don't need to install it separately — just make sure pytest is available in the Python environment irradiate will use.

```bash
# With uv:
uv venv && uv pip install pytest

# With pip:
python -m venv .venv && .venv/bin/pip install pytest
```

## Next step

See [Quickstart](quickstart.md) to run your first mutation test.
