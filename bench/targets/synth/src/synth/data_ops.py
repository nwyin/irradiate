"""Data manipulation utility functions."""

from __future__ import annotations

from typing import Any


def flatten_dict(d: dict, prefix: str = "", sep: str = ".") -> dict[str, Any]:
    """Flatten a nested dict into a single-level dict with dotted keys."""
    result: dict[str, Any] = {}
    for key, value in d.items():
        full_key = prefix + sep + key if prefix else key
        if isinstance(value, dict) and value:
            result.update(flatten_dict(value, full_key, sep))
        else:
            result[full_key] = value
    return result


def deep_merge(base: dict, override: dict) -> dict:
    """Recursively merge override into base. override values win on conflicts."""
    result = dict(base)
    for key, value in override.items():
        if key in result and isinstance(result[key], dict) and isinstance(value, dict):
            result[key] = deep_merge(result[key], value)
        else:
            result[key] = value
    return result


def chunk_list(items: list, size: int) -> list[list]:
    """Split items into consecutive chunks of given size."""
    if size <= 0:
        raise ValueError("Chunk size must be positive")
    chunks = []
    for i in range(0, len(items), size):
        chunks.append(items[i : i + size])
    return chunks


def dedupe_preserve_order(items: list) -> list:
    """Remove duplicates from items while preserving insertion order."""
    seen: set = set()
    result = []
    for item in items:
        if item not in seen:
            seen.add(item)
            result.append(item)
    return result


def group_by(items: list[dict], key: str) -> dict[Any, list[dict]]:
    """Group a list of dicts by the value of a given key."""
    groups: dict[Any, list[dict]] = {}
    for item in items:
        group_key = item.get(key)
        if group_key not in groups:
            groups[group_key] = []
        groups[group_key].append(item)
    return groups


def zip_to_dict(keys: list, values: list) -> dict:
    """Zip two lists into a dict, raising ValueError if lengths differ."""
    if len(keys) != len(values):
        raise ValueError("keys and values must have the same length")
    return dict(zip(keys, values))


def count_by(items: list, key: str) -> dict[Any, int]:
    """Count occurrences of each unique value for a given key in a list of dicts."""
    counts: dict[Any, int] = {}
    for item in items:
        k = item.get(key)
        if k in counts:
            counts[k] += 1
        else:
            counts[k] = 1
    return counts


def partition(items: list, predicate) -> tuple[list, list]:
    """Split items into (matching, non-matching) based on predicate."""
    yes, no = [], []
    for item in items:
        if predicate(item):
            yes.append(item)
        else:
            no.append(item)
    return yes, no


def sliding_window(items: list, size: int) -> list[list]:
    """Return all consecutive sub-lists of given size."""
    if size <= 0:
        raise ValueError("Window size must be positive")
    if size > len(items):
        return []
    return [items[i : i + size] for i in range(len(items) - size + 1)]
