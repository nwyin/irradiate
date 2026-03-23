# Decorator Handling in irradiate

irradiate uses a trampoline architecture to test multiple mutant variants within a single Python process. This page explains how decorators interact with that architecture, which decorators irradiate can handle, and why.

## Background: the trampoline model

For each function, irradiate generates:
- A renamed original (`x_add__irradiate_orig`)
- One renamed variant per mutation (`x_add__irradiate_1`, `x_add__irradiate_2`, ...)
- A lookup dict mapping variant names to functions
- A wrapper function with the original name that dispatches to the correct variant at runtime

```python
# Original source
def add(a, b):
    return a + b

# After trampolining (simplified)
def x_add__irradiate_orig(a, b):
    return a + b

def x_add__irradiate_1(a, b):
    return a - b  # mutant

x_add__irradiate_mutants = {'x_add__irradiate_1': x_add__irradiate_1}

def add(a, b):
    return _irradiate_trampoline(x_add__irradiate_orig, x_add__irradiate_mutants, (a, b), {}, None)
```

The wrapper has the original name and signature, so callers see no difference. The trampoline checks `irradiate_harness.active_mutant` to decide which variant to invoke.

## Why decorators are hard

Decorators execute at **definition time** (when the module is imported), not at call time. When irradiate replaces a function with a trampoline wrapper, the decorator wraps the *wrapper*, not the original function. This causes problems for decorators with side effects:

```python
@app.route("/users")       # registers the wrapper as the /users handler
def get_users():            # ← this is now the trampoline wrapper
    return _irradiate_trampoline(...)
```

Specific failure modes:
1. **Registration side effects**: `@app.route()` registers the function in a URL map. If variants also had the decorator, you'd get duplicate registrations.
2. **Descriptor protocol**: `@property` turns the function into a descriptor. The wrapper must also be a property, or attribute access like `obj.name` breaks.
3. **Calling convention changes**: `@classmethod` passes `cls` instead of `self`; `@staticmethod` passes neither. The wrapper's forwarding logic must match.

## What irradiate handles

### The Big Three: `@property`, `@classmethod`, `@staticmethod`

These three stdlib decorators account for ~80% of all decorated functions in real Python projects (measured across click, httpx, flask, rich, pydantic). They are special because:

- They have **no definition-time side effects** — they only change the calling convention
- Their behavior is **completely predictable** — they're part of the Python data model
- They are **stable** — their semantics haven't changed since Python 2

irradiate handles these by keeping the decorator on the wrapper and adjusting the trampoline dispatch:

For `@classmethod`, the wrapper receives `cls` as its first argument. Variant lookup uses `cls.` to resolve mangled names through the MRO:

```python
class Foo:
    @classmethod
    def make(cls, n):
        return _irradiate_trampoline(
            cls.xǁFooǁmake__irradiate_orig,
            cls.xǁFooǁmake__irradiate_mutants,
            (n,), {}, cls
        )
```

For `@staticmethod`, the wrapper receives no implicit argument. Variant lookup uses the class name directly:

```python
class Foo:
    @staticmethod
    def helper(x):
        return _irradiate_trampoline(
            Foo.xǁFooǁhelper__irradiate_orig,
            Foo.xǁFooǁhelper__irradiate_mutants,
            (x,), {}, None
        )
```

For `@property`, the wrapper is a property whose getter dispatches through the trampoline. Only the getter is trampolined; setter/deleter decorators on the same property are preserved as-is:

```python
class Foo:
    @property
    def name(self):
        return _irradiate_trampoline(
            _type(self).xǁFooǁname__irradiate_orig,
            _type(self).xǁFooǁname__irradiate_mutants,
            (), {}, self
        )
```

### Other "safe" decorators

Some decorators are effectively no-ops at runtime and don't interfere with trampolining:

- `@typing.overload` has no runtime effect (type-checker only). Functions decorated with `@overload` have empty bodies (`...`), so there's nothing to mutate. irradiate skips them because they produce zero mutations.
- `@abstractmethod` marks abstract methods for ABC enforcement. The function body is still real code. Currently skipped but could be handled like a regular instance method.
- `@functools.wraps` copies metadata inside other decorators. No runtime behavior change.

### Decorators that remain skipped

Decorators with definition-time side effects or complex wrapping behavior are still skipped. This includes registration decorators (`@app.route()`, `@click.command()`, `@pytest.fixture`), caching decorators (`@lru_cache`, `@cache`, `@cached_property`), wrapping decorators (`@contextmanager`, `@retry`, `@login_required`), and any user-defined decorators.

These require a different execution model (source-patching) that avoids the trampoline entirely. See [GitHub issue #13](https://github.com/nwyin/irradiate/issues/13) for the design.

## Real-world impact

Decorator frequency measured across 5 popular Python projects:

| Project | Functions | Decorated | @property/@classmethod/@staticmethod | Remaining skipped |
|---------|-----------|-----------|--------------------------------------|-------------------|
| click | 527 | 65 (12%) | 23 | 42 (8%) |
| httpx | 446 | 78 (17%) | 62 | 16 (4%) |
| flask | 367 | 82 (22%) | 15 | 67 (18%) |
| rich | 911 | 206 (23%) | 162 | 44 (5%) |
| pydantic | 1854 | 438 (24%) | 288 | 150 (8%) |

With the Big Three handled, irradiate covers 92-96% of functions in most codebases (flask is an outlier at 82% due to heavy use of `@setupmethod`).

## Why not source-patching for everything?

Source-patching (writing a modified copy of the file with one mutation applied, running tests in a fresh subprocess) would handle all decorators correctly. But it's slower:

- Trampoline mode: amortize pytest startup across hundreds of mutants in one session
- Source-patching: one subprocess per mutant (~1-3s overhead each)

For a codebase with 500 mutants, trampoline mode might take 60s total. Source-patching the same set would take ~1000s. The trampoline is irradiate's primary performance advantage over tools like cosmic-ray that use source-patching exclusively.

The hybrid approach — trampoline for the common case, source-patching for the long tail — gives the best of both worlds.
