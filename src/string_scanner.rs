//! String-aware scanning of Python source fragments.
//!
//! Provides utilities for parsing Python parameter lists, counting bracket depth,
//! and splitting at depth-0 separators — all while correctly handling single-quoted,
//! double-quoted, and triple-quoted strings, escape sequences, and optionally comments.

/// Which bracket types affect the depth counter.
#[derive(Clone, Copy)]
pub enum BracketMode {
    /// Only `(` and `)` affect depth (used by codegen for multi-line signatures).
    ParensOnly,
    /// `()`, `[]`, and `{}` all affect depth (used by trampoline for parameter lists).
    AllBrackets,
}

/// Byte-level scanner that tracks quote and bracket state across a Python fragment.
struct Scanner<'a> {
    bytes: &'a [u8],
    pos: usize,
    in_single: bool,
    in_double: bool,
    escape: bool,
    depth: i32,
    bracket_mode: BracketMode,
    comment_aware: bool,
}

/// What the scanner found at a given position.
enum ScanItem {
    /// A regular byte at this position, outside strings, at the current depth.
    Char { pos: usize, byte: u8, depth: i32 },
    /// The rest of the line is a comment (only when comment_aware is true).
    Comment,
    /// Inside a string literal — no action needed.
    InString,
}

impl<'a> Scanner<'a> {
    fn new(input: &'a [u8], bracket_mode: BracketMode, comment_aware: bool) -> Self {
        Self {
            bytes: input,
            pos: 0,
            in_single: false,
            in_double: false,
            escape: false,
            depth: 0,
            bracket_mode,
            comment_aware,
        }
    }

    /// Advance by one logical character, returning what was found.
    fn next(&mut self) -> Option<ScanItem> {
        if self.pos >= self.bytes.len() {
            return None;
        }
        let ch = self.bytes[self.pos];

        // Handle escape sequences inside strings.
        if self.escape {
            self.escape = false;
            self.pos += 1;
            return Some(ScanItem::InString);
        }
        if ch == b'\\' && (self.in_single || self.in_double) {
            self.escape = true;
            self.pos += 1;
            return Some(ScanItem::InString);
        }

        if !self.in_single && !self.in_double {
            // Check for triple-quote openers before single-char quotes.
            if ch == b'\'' && self.pos + 2 < self.bytes.len()
                && self.bytes[self.pos + 1] == b'\'' && self.bytes[self.pos + 2] == b'\''
            {
                self.in_single = true;
                self.pos += 3;
                return Some(ScanItem::InString);
            }
            if ch == b'"' && self.pos + 2 < self.bytes.len()
                && self.bytes[self.pos + 1] == b'"' && self.bytes[self.pos + 2] == b'"'
            {
                self.in_double = true;
                self.pos += 3;
                return Some(ScanItem::InString);
            }

            // Single-char quotes.
            if ch == b'\'' {
                self.in_single = true;
                self.pos += 1;
                return Some(ScanItem::InString);
            }
            if ch == b'"' {
                self.in_double = true;
                self.pos += 1;
                return Some(ScanItem::InString);
            }

            // Comment.
            if self.comment_aware && ch == b'#' {
                self.pos = self.bytes.len();
                return Some(ScanItem::Comment);
            }

            // Brackets.
            match self.bracket_mode {
                BracketMode::ParensOnly => match ch {
                    b'(' => self.depth += 1,
                    b')' => self.depth -= 1,
                    _ => {}
                },
                BracketMode::AllBrackets => match ch {
                    b'[' | b'(' | b'{' => self.depth += 1,
                    b']' | b')' | b'}' => self.depth -= 1,
                    _ => {}
                },
            }

            let item = ScanItem::Char { pos: self.pos, byte: ch, depth: self.depth };
            self.pos += 1;
            Some(item)
        } else if self.in_single {
            // Inside single-quoted string: check for triple-quote closer.
            if ch == b'\'' && self.pos + 2 < self.bytes.len()
                && self.bytes[self.pos + 1] == b'\'' && self.bytes[self.pos + 2] == b'\''
            {
                self.in_single = false;
                self.pos += 3;
            } else if ch == b'\'' {
                self.in_single = false;
                self.pos += 1;
            } else {
                self.pos += 1;
            }
            Some(ScanItem::InString)
        } else {
            // Inside double-quoted string: check for triple-quote closer.
            if ch == b'"' && self.pos + 2 < self.bytes.len()
                && self.bytes[self.pos + 1] == b'"' && self.bytes[self.pos + 2] == b'"'
            {
                self.in_double = false;
                self.pos += 3;
            } else if ch == b'"' {
                self.in_double = false;
                self.pos += 1;
            } else {
                self.pos += 1;
            }
            Some(ScanItem::InString)
        }
    }
}

/// Count the net parenthesis depth change across a line, ignoring parens inside
/// string literals and stopping at `#` comments.
///
/// Replaces `codegen::count_parens_string_aware`. Call once per line, accumulating
/// the returned delta into your running depth counter.
pub fn paren_depth_change(line: &str) -> i32 {
    let mut scanner = Scanner::new(line.as_bytes(), BracketMode::ParensOnly, true);
    while scanner.next().is_some() {}
    scanner.depth
}

/// Find the first occurrence of `sep` at bracket depth 0, outside string literals.
/// Returns `(before, Some(after))` if found, or `(whole_string, None)` if not.
///
/// Replaces `trampoline::split_at_depth0`.
pub fn split_at_depth0(s: &str, sep: char) -> (&str, Option<&str>) {
    let sep_byte = sep as u8;
    let mut scanner = Scanner::new(s.as_bytes(), BracketMode::AllBrackets, false);
    while let Some(item) = scanner.next() {
        if let ScanItem::Char { pos, byte, depth } = item {
            if byte == sep_byte && depth == 0 {
                return (&s[..pos], Some(&s[pos + 1..]));
            }
        }
    }
    (s, None)
}

/// Split on all occurrences of `sep` at bracket depth 0, outside string literals.
/// Each part is trimmed. Empty trailing parts are omitted.
///
/// Replaces `trampoline::split_params`.
pub fn split_all_at_depth0(s: &str, sep: u8) -> Vec<String> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut scanner = Scanner::new(s.as_bytes(), BracketMode::AllBrackets, false);
    while let Some(item) = scanner.next() {
        if let ScanItem::Char { pos, byte, depth } = item {
            if byte == sep && depth == 0 {
                parts.push(s[start..pos].trim().to_string());
                start = pos + 1;
            }
        }
    }
    let trimmed = s[start..].trim().to_string();
    if !trimmed.is_empty() {
        parts.push(trimmed);
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- paren_depth_change (codegen replacement) ---

    #[test]
    fn test_paren_depth_simple() {
        assert_eq!(paren_depth_change("def foo(x, y):"), 0);
    }

    #[test]
    fn test_paren_depth_open() {
        assert_eq!(paren_depth_change("def foo(x,"), 1);
    }

    #[test]
    fn test_paren_depth_nested() {
        assert_eq!(paren_depth_change("def foo(x=(1, 2),"), 1);
    }

    #[test]
    fn test_paren_depth_string_parens() {
        // Parens inside strings should not count.
        assert_eq!(paren_depth_change(r#"def foo(x="(", y="#), 1);
    }

    #[test]
    fn test_paren_depth_comment() {
        // '#' ends the line — paren after it is ignored.
        assert_eq!(paren_depth_change("def foo(x): # (not counted"), 0);
    }

    #[test]
    fn test_paren_depth_triple_quote() {
        assert_eq!(paren_depth_change(r#"x = """(not counted)""""#), 0);
    }

    #[test]
    fn test_paren_depth_accumulation() {
        let mut depth: i32 = 0;
        depth += paren_depth_change("def foo(x,");
        assert_eq!(depth, 1);
        depth += paren_depth_change("         y):");
        assert_eq!(depth, 0);
    }

    // --- split_at_depth0 (trampoline replacement) ---

    #[test]
    fn test_split_at_depth0_colon() {
        let (b, a) = split_at_depth0("x: int = 5", ':');
        assert_eq!(b, "x");
        assert_eq!(a.unwrap(), " int = 5");
    }

    #[test]
    fn test_split_at_depth0_no_match() {
        let (b, a) = split_at_depth0("x = 5", ':');
        assert_eq!(b, "x = 5");
        assert!(a.is_none());
    }

    #[test]
    fn test_split_at_depth0_string_with_colon() {
        let (before, after) = split_at_depth0(r#"x = "a:b""#, ':');
        assert_eq!(before, r#"x = "a:b""#);
        assert!(after.is_none());
    }

    #[test]
    fn test_split_at_depth0_colon_before_string() {
        let (b, a) = split_at_depth0(r#"x: str = "a:b""#, ':');
        assert_eq!(b, "x");
        assert_eq!(a.unwrap(), r#" str = "a:b""#);
    }

    #[test]
    fn test_split_at_depth0_nested_brackets() {
        let (b, a) = split_at_depth0("x: Dict[str, int] = {}", '=');
        assert_eq!(b, "x: Dict[str, int] ");
        assert_eq!(a.unwrap(), " {}");
    }

    // --- split_all_at_depth0 (split_params replacement) ---

    #[test]
    fn test_split_all_simple() {
        let parts = split_all_at_depth0("a, b, c", b',');
        assert_eq!(parts, vec!["a", "b", "c"]);
    }

    #[test]
    fn test_split_all_nested() {
        let parts = split_all_at_depth0("a, (b, c), d", b',');
        assert_eq!(parts, vec!["a", "(b, c)", "d"]);
    }

    #[test]
    fn test_split_all_string_with_comma() {
        let parts = split_all_at_depth0(r#"value, *, delimiter=", ", **kwargs"#, b',');
        assert_eq!(parts, vec!["value", "*", r#"delimiter=", ""#, "**kwargs"]);
    }

    #[test]
    fn test_split_all_single_quoted_comma() {
        let parts = split_all_at_depth0("x='a, b', y=1", b',');
        assert_eq!(parts, vec!["x='a, b'", "y=1"]);
    }

    #[test]
    fn test_split_all_escaped_quote() {
        let parts = split_all_at_depth0(r#"x="it\'s, ok", y=2"#, b',');
        assert_eq!(parts, vec![r#"x="it\'s, ok""#, "y=2"]);
    }

    #[test]
    fn test_split_all_triple_quoted() {
        let parts = split_all_at_depth0(r#"x="""a, b""", y=1"#, b',');
        assert_eq!(parts, vec![r#"x="""a, b""""#, "y=1"]);
    }

    #[test]
    fn test_split_all_empty() {
        let parts = split_all_at_depth0("", b',');
        assert!(parts.is_empty());
    }

    #[test]
    fn test_split_all_single_param() {
        let parts = split_all_at_depth0("self", b',');
        assert_eq!(parts, vec!["self"]);
    }
}
