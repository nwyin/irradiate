"""Tests for validators — cover happy paths and edge cases to kill mutants."""

import pytest
from synth.validators import (
    all_unique,
    is_palindrome,
    is_sorted,
    is_strong_password,
    validate_email,
    validate_ip_v4,
    validate_luhn,
    validate_password_strength,
    validate_range,
    validate_url,
)


def test_validate_email():
    assert validate_email("user@example.com") is True
    assert validate_email("a.b+c@sub.domain.org") is True
    assert validate_email("notanemail") is False
    assert validate_email("missing@tld") is False
    assert validate_email("@nodomain.com") is False


def test_validate_range():
    assert validate_range(5, 0, 10) is True
    assert validate_range(0, 0, 10) is True
    assert validate_range(10, 0, 10) is True
    assert validate_range(-1, 0, 10) is False
    assert validate_range(11, 0, 10) is False
    # exclusive
    assert validate_range(0, 0, 10, inclusive=False) is False
    assert validate_range(10, 0, 10, inclusive=False) is False
    assert validate_range(5, 0, 10, inclusive=False) is True


def test_password_strength():
    checks = validate_password_strength("Abcdef1!")
    assert checks["length"] is True
    assert checks["uppercase"] is True
    assert checks["lowercase"] is True
    assert checks["digit"] is True
    assert checks["special"] is True

    weak = validate_password_strength("abc")
    assert weak["length"] is False


def test_is_strong_password():
    assert is_strong_password("Abcdef1!") is True
    assert is_strong_password("password") is False
    assert is_strong_password("Password1") is False  # missing special


def test_is_palindrome():
    assert is_palindrome("racecar") is True
    assert is_palindrome("A man a plan a canal Panama") is True
    assert is_palindrome("hello") is False
    assert is_palindrome("") is True


def test_validate_luhn():
    # Visa test number
    assert validate_luhn("4532015112830366") is True
    # Invalid
    assert validate_luhn("1234567890123456") is False
    assert validate_luhn("1") is False


def test_validate_url():
    assert validate_url("https://example.com") is True
    assert validate_url("http://sub.domain.org/path") is True
    assert validate_url("ftp://example.com") is False
    assert validate_url("https://") is False
    assert validate_url("not a url") is False


def test_validate_ip_v4():
    assert validate_ip_v4("192.168.1.1") is True
    assert validate_ip_v4("0.0.0.0") is True
    assert validate_ip_v4("255.255.255.255") is True
    assert validate_ip_v4("256.0.0.1") is False
    assert validate_ip_v4("192.168.1") is False
    assert validate_ip_v4("abc.def.ghi.jkl") is False


def test_all_unique():
    assert all_unique([1, 2, 3]) is True
    assert all_unique([1, 2, 2]) is False
    assert all_unique([]) is True


def test_is_sorted():
    assert is_sorted([1, 2, 3]) is True
    assert is_sorted([3, 2, 1]) is False
    assert is_sorted([3, 2, 1], reverse=True) is True
    assert is_sorted([1, 1, 2]) is True
    assert is_sorted([]) is True
