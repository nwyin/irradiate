import irradiate_harness as _ih


def _irradiate_trampoline(orig, mutants, call_args, call_kwargs, self_arg=None):
    active = _ih.active_mutant
    if not active:
        return orig(*call_args, **call_kwargs)
    if active == 'fail':
        raise _ih.ProgrammaticFailException()
    if active == 'stats':
        _ih.record_hit(orig.__module__ + '.' + orig.__name__)
        return orig(*call_args, **call_kwargs)
    prefix = orig.__module__ + '.' + orig.__name__ + '__mutmut_'
    if not active.startswith(prefix):
        return orig(*call_args, **call_kwargs)
    variant = active.rpartition('.')[-1]
    if self_arg is not None:
        return mutants[variant](self_arg, *call_args, **call_kwargs)
    return mutants[variant](*call_args, **call_kwargs)


# --- add ---

def x_add__mutmut_orig(a, b):
    return a + b

def x_add__mutmut_1(a, b):
    return a - b  # + -> -

def x_add__mutmut_2(a, b):
    return a * b  # + -> *

x_add__mutmut_mutants = {
    'x_add__mutmut_1': x_add__mutmut_1,
    'x_add__mutmut_2': x_add__mutmut_2,
}
x_add__mutmut_orig.__name__ = 'x_add'

def add(a, b):
    return _irradiate_trampoline(x_add__mutmut_orig, x_add__mutmut_mutants, (a, b), {})


# --- is_positive ---

def x_is_positive__mutmut_orig(n):
    if n > 0:
        return True
    return False

def x_is_positive__mutmut_1(n):
    if n >= 0:  # > -> >=
        return True
    return False

def x_is_positive__mutmut_2(n):
    if n > 0:
        return False  # True -> False
    return False

def x_is_positive__mutmut_3(n):
    if n > 0:
        return True
    return True  # False -> True

x_is_positive__mutmut_mutants = {
    'x_is_positive__mutmut_1': x_is_positive__mutmut_1,
    'x_is_positive__mutmut_2': x_is_positive__mutmut_2,
    'x_is_positive__mutmut_3': x_is_positive__mutmut_3,
}
x_is_positive__mutmut_orig.__name__ = 'x_is_positive'

def is_positive(n):
    return _irradiate_trampoline(x_is_positive__mutmut_orig, x_is_positive__mutmut_mutants, (n,), {})


# --- greet ---

def x_greet__mutmut_orig(name):
    return "Hello, " + name

def x_greet__mutmut_1(name):
    return "XXHello, XX" + name  # string mutation

def x_greet__mutmut_2(name):
    return "Hello, " - name  # + -> -

x_greet__mutmut_mutants = {
    'x_greet__mutmut_1': x_greet__mutmut_1,
    'x_greet__mutmut_2': x_greet__mutmut_2,
}
x_greet__mutmut_orig.__name__ = 'x_greet'

def greet(name):
    return _irradiate_trampoline(x_greet__mutmut_orig, x_greet__mutmut_mutants, (name,), {})


# --- untested_func (no mutations, just pass-through) ---

def untested_func():
    return "this function has no tests"
