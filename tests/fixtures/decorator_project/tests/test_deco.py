"""Tests for decorated functions — each test must detect mutations."""

from deco_lib import (
    cached_add,
    cached_multiply,
    managed_resource,
    compute,
    secret_value,
    decorated_subtract,
    stacked_fn,
    Config,
    Calculator,
    plain_add,
)


def test_cached_add():
    # Clear cache to get a fresh call each time.
    cached_add.cache_clear()
    assert cached_add(2, 3) == 5
    assert cached_add(-1, 1) == 0


def test_cached_multiply():
    cached_multiply.cache_clear()
    assert cached_multiply(3, 4) == 12
    assert cached_multiply(0, 5) == 0


def test_managed_resource():
    with managed_resource(5) as val:
        assert val == 15


def test_compute():
    assert compute(3) == 7  # 3*2 + 1


def test_secret_value():
    assert secret_value(0) == 100
    assert secret_value(50) == 150


def test_decorated_subtract():
    assert decorated_subtract(10, 3) == 7
    assert decorated_subtract(5, 5) == 0


def test_stacked_fn():
    assert stacked_fn(4) == 12


def test_config_cached_property():
    c = Config()
    assert c.computed == 42


def test_calculator_property():
    calc = Calculator()
    assert calc.name == "calc"


def test_calculator_power():
    calc = Calculator()
    assert calc.power(2, 3) == 8
    assert calc.power(3, 2) == 9


def test_plain_add():
    assert plain_add(1, 2) == 3
