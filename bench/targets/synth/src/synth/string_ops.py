"""String utility functions."""

from __future__ import annotations

import re


def normalize_whitespace(text: str) -> str:
    """Collapse all whitespace runs to a single space and strip ends."""
    return re.sub(r"\s+", " ", text).strip()


def slugify(text: str) -> str:
    """Convert text to a URL-friendly slug."""
    text = text.lower()
    text = re.sub(r"[^\w\s-]", "", text)
    text = re.sub(r"[\s_]+", "-", text)
    return text.strip("-")


def truncate(text: str, max_len: int, ellipsis: str = "...") -> str:
    """Truncate text to max_len chars, appending ellipsis if truncated."""
    if len(text) <= max_len:
        return text
    return text[: max_len - len(ellipsis)] + ellipsis


def camel_to_snake(name: str) -> str:
    """Convert camelCase or PascalCase to snake_case."""
    s = re.sub(r"([A-Z]+)([A-Z][a-z])", r"\1_\2", name)
    s = re.sub(r"([a-z\d])([A-Z])", r"\1_\2", s)
    return s.lower()


def parse_bool(value: str) -> bool:
    """Parse a string into a bool. Raises ValueError on unrecognised input."""
    normalized = value.strip().lower()
    if normalized in ("1", "true", "yes", "on"):
        return True
    if normalized in ("0", "false", "no", "off"):
        return False
    raise ValueError(f"Cannot parse {value!r} as bool")


def mask_email(email: str) -> str:
    """Mask an email address, showing only first char and domain."""
    if "@" not in email:
        return email
    local, domain = email.split("@", 1)
    if len(local) <= 1:
        return email
    masked_local = local[0] + "*" * (len(local) - 1)
    return masked_local + "@" + domain


def count_words(text: str) -> int:
    """Count the number of whitespace-separated words in text."""
    stripped = text.strip()
    if not stripped:
        return 0
    return len(stripped.split())


def pad_center(text: str, width: int, fill: str = " ") -> str:
    """Center text in a field of given width using fill character."""
    if len(text) >= width:
        return text
    total_pad = width - len(text)
    left = total_pad // 2
    right = total_pad - left
    return fill * left + text + fill * right


def common_prefix(strings: list[str]) -> str:
    """Find the longest common prefix of a list of strings."""
    if not strings:
        return ""
    prefix = strings[0]
    for s in strings[1:]:
        while not s.startswith(prefix):
            prefix = prefix[:-1]
            if not prefix:
                return ""
    return prefix


def repeat_join(token: str, count: int, sep: str = "") -> str:
    """Repeat token count times, joining with sep."""
    if count <= 0:
        return ""
    parts = [token] * count
    return sep.join(parts)


def remove_prefix(text: str, prefix: str) -> str:
    """Remove prefix from text if present, otherwise return text unchanged."""
    if text.startswith(prefix):
        return text[len(prefix) :]
    return text


def remove_suffix(text: str, suffix: str) -> str:
    """Remove suffix from text if present, otherwise return text unchanged."""
    if suffix and text.endswith(suffix):
        return text[: -len(suffix)]
    return text
