"""Validation utility functions."""

from __future__ import annotations

import re


def validate_email(email: str) -> bool:
    """Return True if email looks like a valid RFC-5321 address."""
    pattern = r"^[a-zA-Z0-9._%+\-]+@[a-zA-Z0-9.\-]+\.[a-zA-Z]{2,}$"
    return bool(re.match(pattern, email))


def validate_range(value: float, lo: float, hi: float, inclusive: bool = True) -> bool:
    """Return True if value is within [lo, hi] (or (lo, hi) if not inclusive)."""
    if inclusive:
        return lo <= value <= hi
    return lo < value < hi


def validate_password_strength(password: str) -> dict[str, bool]:
    """
    Check password strength rules.

    Returns a dict with keys: length, uppercase, lowercase, digit, special.
    """
    result = {
        "length": len(password) >= 8,
        "uppercase": any(c.isupper() for c in password),
        "lowercase": any(c.islower() for c in password),
        "digit": any(c.isdigit() for c in password),
        "special": any(not c.isalnum() for c in password),
    }
    return result


def is_strong_password(password: str) -> bool:
    """Return True only when all password strength checks pass."""
    checks = validate_password_strength(password)
    return all(checks.values())


def is_palindrome(text: str) -> bool:
    """Return True if text reads the same forwards and backwards (ignoring case/spaces)."""
    cleaned = re.sub(r"[^a-zA-Z0-9]", "", text).lower()
    return cleaned == cleaned[::-1]


def validate_luhn(card_number: str) -> bool:
    """Validate a credit card number using the Luhn algorithm."""
    digits = [int(d) for d in card_number if d.isdigit()]
    if len(digits) < 2:
        return False
    total = 0
    for i, digit in enumerate(reversed(digits)):
        if i % 2 == 1:
            doubled = digit * 2
            if doubled > 9:
                doubled -= 9
            total += doubled
        else:
            total += digit
    return total % 10 == 0


def validate_url(url: str) -> bool:
    """Return True if url starts with http:// or https:// and has a host."""
    if not (url.startswith("http://") or url.startswith("https://")):
        return False
    remainder = url[url.index("//") + 2 :]
    host = remainder.split("/")[0]
    return len(host) > 0 and "." in host


def validate_ip_v4(ip: str) -> bool:
    """Return True if ip is a valid dotted-decimal IPv4 address."""
    parts = ip.split(".")
    if len(parts) != 4:
        return False
    for part in parts:
        if not part.isdigit():
            return False
        value = int(part)
        if value < 0 or value > 255:
            return False
    return True


def all_unique(items: list) -> bool:
    """Return True if all items in the list are unique."""
    return len(items) == len(set(items))


def is_sorted(items: list, reverse: bool = False) -> bool:
    """Return True if items is sorted in ascending (or descending if reverse) order."""
    for i in range(len(items) - 1):
        if reverse:
            if items[i] < items[i + 1]:
                return False
        else:
            if items[i] > items[i + 1]:
                return False
    return True
