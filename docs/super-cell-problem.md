# The `super()` `__class__` Cell Problem

## Background

Python 3's `super()` without arguments (PEP 3135) relies on a compiler-injected
`__class__` cell variable. When the compiler sees `super` referenced inside a
class body method, it:

1. Adds `__class__` to the method's `co_freevars` (closure variables)
2. Generates `LOAD_DEREF __class__` + `LOAD_SUPER_ATTR` bytecode
3. After the class object is created, populates the cell with a reference to it

A function compiled at **module level** gets fundamentally different bytecode:
`LOAD_GLOBAL super` + regular `CALL`. You cannot fix this by patching
`co_freevars` after the fact — the bytecode instructions are wrong.

## How irradiate triggers this

irradiate's trampoline architecture extracts class methods to module level:

```python
# Original (inside class body — __class__ cell works):
class Markup(str):
    def __add__(self, value):
        return super().__add__(self.escape(value))

# After irradiate codegen (module level — __class__ cell missing):
def xǁMarkupǁ__add____mutmut_orig(self, value):
    return super().__add__(self.escape(value))  # RuntimeError!
```

The wrapper stays inside the class body, but the mangled orig/variants at module
level lose the `__class__` cell. Any method using `super()` fails with:
`RuntimeError: super(): __class__ cell not found`

Found via markupsafe vendor testing — `Markup.__add__`, `__radd__`, `__mod__`,
`format_map`, etc. all use `super()`.

## How other tools handle this

### mutmut — keep everything inside the class body

mutmut keeps orig, variants, and lookup dict **inside the class body**
(`file_mutation.py:220-238`). The mangled names become class attributes rather
than module globals. Since everything is compiled in the class context, the
`__class__` cell is always present.

mutmut has zero code, tests, or documentation related to `super()` / `__class__`
because their architecture avoids the problem entirely.

### cosmic-ray, MutPy — in-place single-mutation edits

These tools apply one mutation at a time to the original source, without moving
code. The method definition stays in the class body. No `super()` problem.

### mutatest — bytecode-only modification

Modifies compiled `.pyc` files in `__pycache__`. Compilation happens in the
correct context. No `super()` problem.

## Possible solutions for irradiate

### A. Keep mangled code inside the class body (mutmut's approach)

Instead of extracting `module_code` to module level for class methods, emit
orig, variants, and lookup dict inside the class body (indented).

- **Pros**: Completely eliminates the problem. No detection needed. Simplest
  correctness guarantee.
- **Cons**: Class bodies become large. Mangled names become class attributes
  rather than module globals. May interact with metaclasses or
  `__init_subclass__` that introspect class attributes.

### B. Rewrite `super()` to explicit `super(ClassName, self)`

Scan each method's source for `super()` calls. When found, rewrite to
`super(ClassName, self)` before extracting to module level.

- **Pros**: Minimal architectural change. Only affects methods that use `super()`.
  Preserves module-level extraction for everything else.
- **Cons**: Must handle edge cases: `super` aliased, `super` in nested
  functions/lambdas, classmethods (`cls` instead of `self`). Subtly changes
  behavior in exotic multiple-inheritance scenarios (though in practice
  identical for direct subclass methods).
- **Implementation**: Simple regex/text replacement in Rust codegen:
  `super()` → `super(ClassName, self)`. Covers 99% of real-world code.

### C. Closure factory wrapper

Wrap extracted functions in a closure that provides `__class__`:

```python
def _make_method(cls):
    __class__ = cls
    def xǁChildǁgreet__mutmut_orig(self):
        return super().greet()
    return xǁChildǁgreet__mutmut_orig
xǁChildǁgreet__mutmut_orig = _make_method(Child)
```

- **Pros**: Preserves original `super()` semantics perfectly. Works with all
  inheritance patterns.
- **Cons**: Adds indirection. Class must be defined before the factory call
  (ordering constraint). One factory per method. Runtime overhead at import.

### D. Hybrid — detect `super()` and choose strategy per-method

If a method (or its mutant variants) uses `super()`, keep it in the class body.
Otherwise, extract to module level.

- **Pros**: Best of both worlds — most methods get module-level extraction,
  only `super()`-using methods stay in class.
- **Cons**: Two code paths to maintain. Detection must be reliable.

## Recommendation

**Approach A (keep mangled code inside the class body)** — matching mutmut's
proven strategy.

Approach B (rewrite `super()`) was initially tempting as a minimal change, but
it has real edge-case risks: `super` aliased to a variable, nested in
comprehensions/lambdas, classmethods with `cls` instead of `self`, and subtle
behavioral differences in multiple-inheritance diamonds where explicit
`super(ClassName, self)` is not identical to implicit `super()`.

Approach A avoids the problem entirely with no detection or rewriting needed.
The downsides are cosmetic:

- **Class attribute pollution**: mangled names like `xǁClassǁmethod__irradiate_orig`
  appear in `dir(cls)` / `vars(cls)`. Unlikely to collide with anything due to
  the `xǁ` prefix. Metaclasses that introspect `__dict__` would see them, but
  this is the same tradeoff mutmut makes and hasn't been a problem in practice.
- **Larger class bodies**: more code inside the class. No runtime impact.

mutmut has been shipping this approach for years with real users. It works.

### What changes in codegen

Currently (`src/codegen.rs` and `src/trampoline.rs`):
- Wrapper stays inside the class body (indented) ✓
- Orig, variants, and lookup dict are extracted to module level ✗

After the fix:
- Wrapper stays inside the class body ✓
- Orig, variants, and lookup dict also stay inside the class body ✓
- Top-level functions continue to use module-level placement (no change)

## References

- [PEP 3135 — New Super](https://peps.python.org/pep-3135/)
- [CPython issue #29944 — super() fails in type()-constructed classes](https://bugs.python.org/issue29944)
- mutmut source: `vendor/mutmut/src/mutmut/file_mutation.py` lines 220-238
