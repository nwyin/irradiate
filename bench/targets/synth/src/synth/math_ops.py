"""Arithmetic utility functions."""

from __future__ import annotations


def add(a: float, b: float) -> float:
    """Add two numbers."""
    return a + b


def subtract(a: float, b: float) -> float:
    """Subtract b from a."""
    return a - b


def multiply(a: float, b: float) -> float:
    """Multiply two numbers."""
    return a * b


def divide(a: float, b: float) -> float:
    """Divide a by b, raising ValueError on zero divisor."""
    if b == 0:
        raise ValueError("Division by zero")
    return a / b


def safe_divide(a: float, b: float, default: float = 0.0) -> float:
    """Divide a by b, returning default when divisor is zero."""
    if b == 0:
        return default
    return a / b


def percentage(value: float, total: float) -> float:
    """Compute value as a percentage of total."""
    if total == 0:
        return 0.0
    return value / total * 100.0


def clamp(value: float, lo: float, hi: float) -> float:
    """Clamp value to the range [lo, hi]."""
    if value < lo:
        return lo
    if value > hi:
        return hi
    return value


def lerp(a: float, b: float, t: float) -> float:
    """Linear interpolation between a and b at parameter t in [0, 1]."""
    return a + (b - a) * t


def remap(value: float, in_lo: float, in_hi: float, out_lo: float, out_hi: float) -> float:
    """Remap value from [in_lo, in_hi] to [out_lo, out_hi]."""
    if in_hi == in_lo:
        return out_lo
    t = (value - in_lo) / (in_hi - in_lo)
    return out_lo + (out_hi - out_lo) * t


def floor_div(a: int, b: int) -> int:
    """Integer floor division, raising ValueError on zero divisor."""
    if b == 0:
        raise ValueError("Division by zero")
    return a // b


def power(base: float, exp: float) -> float:
    """Raise base to the given exponent."""
    return base**exp


def modulo(a: int, b: int) -> int:
    """Modulo operation, raising ValueError on zero divisor."""
    if b == 0:
        raise ValueError("Modulo by zero")
    return a % b


def abs_diff(a: float, b: float) -> float:
    """Absolute difference between two numbers."""
    diff = a - b
    if diff < 0:
        return -diff
    return diff


def sign(x: float) -> int:
    """Return +1, -1, or 0 depending on the sign of x."""
    if x > 0:
        return 1
    if x < 0:
        return -1
    return 0


def round_to(value: float, places: int) -> float:
    """Round value to the given number of decimal places."""
    factor = 10**places
    return round(value * factor) / factor


def weighted_average(values: list[float], weights: list[float]) -> float:
    """Compute weighted average of values using corresponding weights."""
    if len(values) != len(weights):
        raise ValueError("values and weights must have the same length")
    total_weight = sum(weights)
    if total_weight == 0:
        raise ValueError("Weights sum to zero")
    total = 0.0
    for v, w in zip(values, weights):
        total += v * w
    return total / total_weight
