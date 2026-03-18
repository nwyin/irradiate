"""Tests for data_ops — cover happy paths and edge cases to kill mutants."""

import pytest
from synth.data_ops import (
    chunk_list,
    count_by,
    dedupe_preserve_order,
    deep_merge,
    flatten_dict,
    group_by,
    partition,
    sliding_window,
    zip_to_dict,
)


def test_flatten_dict():
    d = {"a": 1, "b": {"c": 2, "d": {"e": 3}}}
    assert flatten_dict(d) == {"a": 1, "b.c": 2, "b.d.e": 3}
    assert flatten_dict({}) == {}
    assert flatten_dict({"x": 1}) == {"x": 1}


def test_deep_merge():
    base = {"a": 1, "b": {"c": 2, "d": 3}}
    override = {"b": {"c": 99}, "e": 5}
    result = deep_merge(base, override)
    assert result == {"a": 1, "b": {"c": 99, "d": 3}, "e": 5}
    # base is not mutated
    assert base["b"]["c"] == 2


def test_chunk_list():
    assert chunk_list([1, 2, 3, 4, 5], 2) == [[1, 2], [3, 4], [5]]
    assert chunk_list([], 3) == []
    assert chunk_list([1, 2, 3], 3) == [[1, 2, 3]]
    with pytest.raises(ValueError):
        chunk_list([1, 2], 0)


def test_dedupe_preserve_order():
    assert dedupe_preserve_order([3, 1, 2, 1, 3]) == [3, 1, 2]
    assert dedupe_preserve_order([]) == []
    assert dedupe_preserve_order([1]) == [1]


def test_group_by():
    items = [{"k": "a", "v": 1}, {"k": "b", "v": 2}, {"k": "a", "v": 3}]
    groups = group_by(items, "k")
    assert len(groups["a"]) == 2
    assert len(groups["b"]) == 1


def test_zip_to_dict():
    assert zip_to_dict(["a", "b"], [1, 2]) == {"a": 1, "b": 2}
    with pytest.raises(ValueError):
        zip_to_dict(["a"], [1, 2])


def test_count_by():
    items = [{"type": "x"}, {"type": "y"}, {"type": "x"}]
    counts = count_by(items, "type")
    assert counts["x"] == 2
    assert counts["y"] == 1


def test_partition():
    evens, odds = partition([1, 2, 3, 4, 5], lambda x: x % 2 == 0)
    assert evens == [2, 4]
    assert odds == [1, 3, 5]
    assert partition([], lambda x: True) == ([], [])


def test_sliding_window():
    assert sliding_window([1, 2, 3, 4], 2) == [[1, 2], [2, 3], [3, 4]]
    assert sliding_window([1, 2], 3) == []
    assert sliding_window([1, 2, 3], 3) == [[1, 2, 3]]
    with pytest.raises(ValueError):
        sliding_window([1, 2], 0)
