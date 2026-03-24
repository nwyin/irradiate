import re


def validate_email(email):
    """Check if email matches a basic pattern."""
    return re.match(r"^[^@]+@[^@]+\.[^@]+$", email) is not None


def extract_numbers(text):
    """Find all integer sequences in text."""
    return re.findall(r"\d+", text)


def is_phone_number(text):
    """Check if text is a US phone number like 123-456-7890."""
    return re.fullmatch(r"\d{3}-\d{3}-\d{4}", text) is not None


def split_words(text):
    """Split text on whitespace runs."""
    return re.split(r"\s+", text)


def has_strong_password(pw):
    """Check password has uppercase, digit, and 8+ chars."""
    return re.search(r"(?=.*[A-Z])(?=.*\d).{8,}", pw) is not None


def strip_html_tags(text):
    """Remove HTML tags from text."""
    return re.sub(r"<[^>]+>", "", text)


def find_words(text):
    """Find all word-boundary words."""
    return re.findall(r"\b\w+\b", text)
