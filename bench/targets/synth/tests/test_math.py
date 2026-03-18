"""Tests for math_ops — cover happy paths and edge cases to kill mutants."""

import pytest
from synth.math_ops import (
    abs_diff,
    add,
    clamp,
    divide,
    floor_div,
    lerp,
    modulo,
    multiply,
    percentage,
    power,
    remap,
    round_to,
    safe_divide,
    sign,
    subtract,
    weighted_average,
)


def test_basic_arithmetic():
    assert add(3, 4) == 7
    assert subtract(10, 3) == 7
    assert multiply(3, 4) == 12
    assert divide(10, 2) == 5.0


def test_divide_zero_raises():
    with pytest.raises(ValueError):
        divide(1, 0)


def test_safe_divide_fallback():
    assert safe_divide(10, 0) == 0.0
    assert safe_divide(10, 0, default=99) == 99
    assert safe_divide(10, 4) == 2.5


def test_percentage():
    assert percentage(50, 200) == 25.0
    assert percentage(0, 100) == 0.0
    assert percentage(100, 100) == 100.0
    assert percentage(1, 0) == 0.0


def test_clamp():
    assert clamp(5, 0, 10) == 5
    assert clamp(-3, 0, 10) == 0
    assert clamp(15, 0, 10) == 10
    assert clamp(0, 0, 10) == 0
    assert clamp(10, 0, 10) == 10


def test_lerp():
    assert lerp(0, 10, 0.0) == 0.0
    assert lerp(0, 10, 1.0) == 10.0
    assert lerp(0, 10, 0.5) == 5.0
    assert lerp(2, 4, 0.5) == 3.0


def test_remap():
    assert remap(5, 0, 10, 0, 100) == 50.0
    assert remap(0, 0, 10, 0, 100) == 0.0
    assert remap(10, 0, 10, 0, 100) == 100.0
    # degenerate range returns out_lo
    assert remap(5, 3, 3, 7, 9) == 7


def test_floor_div_and_modulo():
    assert floor_div(10, 3) == 3
    assert modulo(10, 3) == 1
    with pytest.raises(ValueError):
        floor_div(5, 0)
    with pytest.raises(ValueError):
        modulo(5, 0)


def test_power_and_round():
    assert power(2, 10) == 1024.0
    assert round_to(3.14159, 2) == 3.14


def test_abs_diff():
    assert abs_diff(5, 3) == 2.0
    assert abs_diff(3, 5) == 2.0
    assert abs_diff(0, 0) == 0.0


def test_sign():
    assert sign(5) == 1
    assert sign(-3) == -1
    assert sign(0) == 0


def test_weighted_average():
    assert weighted_average([1.0, 2.0, 3.0], [1.0, 1.0, 1.0]) == 2.0
    assert weighted_average([0.0, 10.0], [3.0, 1.0]) == 2.5
    with pytest.raises(ValueError):
        weighted_average([1.0], [])
    with pytest.raises(ValueError):
        weighted_average([1.0], [0.0])
