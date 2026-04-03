"""Library with decorated functions for testing source-patch mutations."""

import functools
import contextlib


# --- Caching decorators ---

@functools.lru_cache(maxsize=128)
def cached_add(a, b):
    return a + b


@functools.cache
def cached_multiply(a, b):
    return a * b


# --- Context manager decorator ---

@contextlib.contextmanager
def managed_resource(value):
    yield value + 10


# --- Registration-style decorator (simulated @app.route) ---

_registry = {}


def register(name):
    def decorator(func):
        _registry[name] = func
        return func
    return decorator


@register("compute")
def compute(x):
    return x * 2 + 1


# --- Wrapping decorator (simulated @login_required / @retry) ---

def requires_auth(func):
    @functools.wraps(func)
    def wrapper(*args, **kwargs):
        return func(*args, **kwargs)
    return wrapper


@requires_auth
def secret_value(x):
    return x + 100


# --- Custom bare decorator ---

def my_decorator(func):
    return func


@my_decorator
def decorated_subtract(a, b):
    return a - b


# --- Stacked decorators ---

@my_decorator
@requires_auth
def stacked_fn(x):
    return x * 3


# --- Cached property (simulated) ---

class Config:
    @functools.cached_property
    def computed(self):
        return 21 + 21


# --- Mixed: @property stays trampoline, @lru_cache goes source-patch ---

class Calculator:
    @property
    def name(self):
        return "calc"

    @functools.lru_cache(maxsize=None)
    def power(self, base, exp):
        return base ** exp


# --- Plain undecorated function (should still work via trampoline) ---

def plain_add(a, b):
    return a + b
