from typed_lib import (
    Point, distance, clamp, repeat_string,
    invert_mapping, safe_divide, count_positives, format_pair,
)


def test_distance():
    p1 = Point(0.0, 0.0)
    p2 = Point(3.0, 4.0)
    assert distance(p1, p2) == 5.0


def test_clamp():
    assert clamp(5, 0, 10) == 5
    assert clamp(-1, 0, 10) == 0
    assert clamp(15, 0, 10) == 10


def test_repeat_string():
    assert repeat_string("ab", 3) == "ababab"
    assert repeat_string("x", 0) == ""


def test_invert_mapping():
    assert invert_mapping({"a": 1, "b": 2}) == {1: "a", 2: "b"}


def test_safe_divide():
    assert safe_divide(10.0, 2.0) == 5.0
    assert safe_divide(1.0, 0.0) is None


def test_count_positives():
    assert count_positives([1, -2, 3, 0, -5]) == 2
    assert count_positives([]) == 0


def test_format_pair():
    assert format_pair("x", 42) == "x=42"
