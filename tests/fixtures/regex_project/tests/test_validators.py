from validators import (
    validate_email,
    extract_numbers,
    is_phone_number,
    split_words,
    has_strong_password,
    strip_html_tags,
    find_words,
)


class TestValidateEmail:
    def test_valid_email(self):
        assert validate_email("user@example.com")

    def test_invalid_no_at(self):
        assert not validate_email("userexample.com")

    def test_invalid_no_domain(self):
        assert not validate_email("user@")

    def test_invalid_no_local(self):
        assert not validate_email("@example.com")

    def test_multiple_at_signs(self):
        assert not validate_email("user@@example.com")


class TestExtractNumbers:
    def test_basic(self):
        assert extract_numbers("abc 123 def 456") == ["123", "456"]

    def test_no_numbers(self):
        assert extract_numbers("hello world") == []

    def test_adjacent(self):
        assert extract_numbers("12ab34") == ["12", "34"]


class TestIsPhoneNumber:
    def test_valid(self):
        assert is_phone_number("123-456-7890")

    def test_too_short(self):
        assert not is_phone_number("12-456-7890")

    def test_too_long(self):
        assert not is_phone_number("1234-456-7890")

    def test_no_dashes(self):
        assert not is_phone_number("1234567890")

    def test_letters(self):
        assert not is_phone_number("abc-def-ghij")


class TestSplitWords:
    def test_single_space(self):
        assert split_words("hello world") == ["hello", "world"]

    def test_multiple_spaces(self):
        assert split_words("hello   world") == ["hello", "world"]

    def test_tabs_and_newlines(self):
        assert split_words("a\tb\nc") == ["a", "b", "c"]

    def test_single_word(self):
        assert split_words("hello") == ["hello"]


class TestHasStrongPassword:
    def test_strong(self):
        assert has_strong_password("Abcdefg1")

    def test_no_uppercase(self):
        assert not has_strong_password("abcdefg1")

    def test_no_digit(self):
        assert not has_strong_password("Abcdefgh")

    def test_too_short(self):
        assert not has_strong_password("Ab1")


class TestStripHtmlTags:
    def test_basic(self):
        assert strip_html_tags("<b>hello</b>") == "hello"

    def test_nested(self):
        assert strip_html_tags("<div><p>text</p></div>") == "text"

    def test_no_tags(self):
        assert strip_html_tags("plain text") == "plain text"

    def test_self_closing(self):
        assert strip_html_tags("a<br/>b") == "ab"


class TestFindWords:
    def test_basic(self):
        assert find_words("hello world") == ["hello", "world"]

    def test_punctuation(self):
        assert find_words("hello, world!") == ["hello", "world"]

    def test_empty(self):
        assert find_words("") == []
