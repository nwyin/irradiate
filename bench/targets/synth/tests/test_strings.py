"""Tests for string_ops — cover happy paths and edge cases to kill mutants."""

import pytest
from synth.string_ops import (
    camel_to_snake,
    common_prefix,
    count_words,
    mask_email,
    normalize_whitespace,
    pad_center,
    parse_bool,
    remove_prefix,
    remove_suffix,
    repeat_join,
    slugify,
    truncate,
)


def test_normalize_whitespace():
    assert normalize_whitespace("  hello   world  ") == "hello world"
    assert normalize_whitespace("a\t\nb") == "a b"
    assert normalize_whitespace("no change") == "no change"


def test_slugify():
    assert slugify("Hello World") == "hello-world"
    assert slugify("foo_bar") == "foo-bar"
    assert slugify("--trim--") == "trim"
    assert slugify("café au lait") == "café-au-lait"


def test_truncate():
    assert truncate("hello world", 20) == "hello world"
    assert truncate("hello world", 8) == "hello..."
    assert truncate("hello world", 8, ellipsis="…") == "hello w…"
    assert truncate("hi", 2) == "hi"


def test_camel_to_snake():
    assert camel_to_snake("camelCase") == "camel_case"
    assert camel_to_snake("PascalCase") == "pascal_case"
    assert camel_to_snake("HTMLParser") == "html_parser"
    assert camel_to_snake("already_snake") == "already_snake"


def test_parse_bool():
    assert parse_bool("true") is True
    assert parse_bool("yes") is True
    assert parse_bool("1") is True
    assert parse_bool("false") is False
    assert parse_bool("no") is False
    assert parse_bool("0") is False
    with pytest.raises(ValueError):
        parse_bool("maybe")


def test_mask_email():
    assert mask_email("alice@example.com") == "a****@example.com"
    assert mask_email("b@example.com") == "b@example.com"
    assert mask_email("notanemail") == "notanemail"


def test_count_words():
    assert count_words("hello world") == 2
    assert count_words("  spaces   ") == 1
    assert count_words("") == 0
    assert count_words("one") == 1


def test_pad_center():
    assert pad_center("hi", 6) == "  hi  "
    assert pad_center("hi", 5) == " hi  "
    assert pad_center("hello", 3) == "hello"
    assert pad_center("x", 5, "-") == "--x--"


def test_common_prefix():
    assert common_prefix(["flower", "flow", "flight"]) == "fl"
    assert common_prefix(["abc", "abc"]) == "abc"
    assert common_prefix([]) == ""
    assert common_prefix(["abc", "xyz"]) == ""


def test_repeat_join():
    assert repeat_join("x", 3, "-") == "x-x-x"
    assert repeat_join("x", 1) == "x"
    assert repeat_join("x", 0) == ""


def test_remove_prefix_suffix():
    assert remove_prefix("foobar", "foo") == "bar"
    assert remove_prefix("foobar", "baz") == "foobar"
    assert remove_suffix("foobar", "bar") == "foo"
    assert remove_suffix("foobar", "baz") == "foobar"
    assert remove_suffix("foobar", "") == "foobar"
