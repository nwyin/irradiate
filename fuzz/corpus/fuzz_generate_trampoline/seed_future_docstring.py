from __future__ import annotations


def validate(x: int | None) -> bool:
    """Check if x is a positive integer."""
    if x is None:
        return False
    return x > 0
