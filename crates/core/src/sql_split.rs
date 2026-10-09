//! Splits a script into statements on top-level semicolons.
//!
//! Semicolons inside string literals, quoted identifiers, comments,
//! Postgres dollar-quoted bodies, SQLite trigger bodies (`BEGIN … END`) and
//! Postgres `BEGIN ATOMIC … END` function bodies do not end a statement.

use std::ops::Range;

use crate::Dialect;

/// Byte range of a statement's code: leading comments and whitespace and the
/// terminating semicolon are excluded.
pub type Span = Range<usize>;

pub fn split(sql: &str, dialect: Dialect) -> Vec<Span> {
    Splitter { src: sql.as_bytes(), dialect, i: 0 }.run()
}

struct Splitter<'a> {
    src: &'a [u8],
    dialect: Dialect,
    i: usize,
}

#[derive(Default)]
struct Statement {
    code_start: Option<usize>,
    code_end: usize,
    /// First few keywords, upper-cased, used to recognise CREATE TRIGGER/FUNCTION.
    head: Vec<String>,
    prev_word: String,
    block_depth: u32,
}

impl Statement {
    fn mark_code(&mut self, start: usize, end: usize) {
        self.code_start.get_or_insert(start);
        self.code_end = end;
    }

    fn has_body_blocks(&self, dialect: Dialect) -> bool {
        let head: Vec<&str> = self.head.iter().map(String::as_str).filter(|w| *w != "OR" && *w != "REPLACE").collect();
        if head.first() != Some(&"CREATE") {
            return false;
        }
        match dialect {
            Dialect::Sqlite => head.iter().take(3).any(|w| *w == "TRIGGER"),
            Dialect::Postgres => matches!(head.get(1), Some(&"FUNCTION") | Some(&"PROCEDURE")),
        }
    }

    fn on_word(&mut self, word: &str, dialect: Dialect) {
        let upper = word.to_ascii_uppercase();
        if self.head.len() < 4 {
            self.head.push(upper.clone());
        }
        if self.has_body_blocks(dialect) {
            let opens_body = match dialect {
                Dialect::Sqlite => upper == "BEGIN" && self.block_depth == 0,
                Dialect::Postgres => upper == "ATOMIC" && self.prev_word == "BEGIN",
            };
            if opens_body || (self.block_depth > 0 && upper == "CASE") {
                self.block_depth += 1;
            } else if self.block_depth > 0 && upper == "END" {
                self.block_depth -= 1;
            }
        }
        self.prev_word = upper;
    }
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

impl Splitter<'_> {
    fn peek(&self, offset: usize) -> Option<u8> {
        self.src.get(self.i + offset).copied()
    }

    fn run(mut self) -> Vec<Span> {
        let mut spans = Vec::new();
        let mut stmt = Statement::default();
        while self.i < self.src.len() {
            let start = self.i;
            let b = self.src[self.i];
            match b {
                b';' if stmt.block_depth == 0 => {
                    if let Some(s) = stmt.code_start {
                        spans.push(s..stmt.code_end);
                    }
                    stmt = Statement::default();
                    self.i += 1;
                }
                b'-' if self.peek(1) == Some(b'-') => self.skip_line_comment(),
                b'/' if self.peek(1) == Some(b'*') => self.skip_block_comment(),
                b'\'' => {
                    let backslash_escapes = self.dialect == Dialect::Postgres
                        && start > 0
                        && matches!(self.src[start - 1], b'E' | b'e')
                        && (start < 2 || !is_word_byte(self.src[start - 2]));
                    self.skip_quoted(b'\'', backslash_escapes);
                    stmt.mark_code(start, self.i);
                }
                b'"' => {
                    self.skip_quoted(b'"', false);
                    stmt.mark_code(start, self.i);
                }
                b'`' if self.dialect == Dialect::Sqlite => {
                    self.skip_quoted(b'`', false);
                    stmt.mark_code(start, self.i);
                }
                b'[' if self.dialect == Dialect::Sqlite => {
                    self.skip_until(b']');
                    stmt.mark_code(start, self.i);
                }
                b'$' if self.dialect == Dialect::Postgres && self.try_skip_dollar_quote() => {
                    stmt.mark_code(start, self.i);
                }
                b if is_word_byte(b) => {
                    while self.i < self.src.len() && is_word_byte(self.src[self.i]) {
                        self.i += 1;
                    }
                    // Word boundaries are ASCII, so this slice is valid UTF-8.
                    let word = std::str::from_utf8(&self.src[start..self.i]).unwrap_or("");
                    stmt.on_word(word, self.dialect);
                    stmt.mark_code(start, self.i);
                }
                b if b.is_ascii_whitespace() => self.i += 1,
                _ => {
                    self.i += 1;
                    stmt.mark_code(start, self.i);
                }
            }
        }
        if let Some(s) = stmt.code_start {
            spans.push(s..stmt.code_end);
        }
        spans
    }

    fn skip_line_comment(&mut self) {
        while self.i < self.src.len() && self.src[self.i] != b'\n' {
            self.i += 1;
        }
    }

    fn skip_block_comment(&mut self) {
        // Postgres block comments nest; SQLite's do not.
        let nests = self.dialect == Dialect::Postgres;
        let mut depth = 0u32;
        while self.i < self.src.len() {
            if self.peek(0) == Some(b'/') && self.peek(1) == Some(b'*') && (nests || depth == 0) {
                depth += 1;
                self.i += 2;
            } else if self.peek(0) == Some(b'*') && self.peek(1) == Some(b'/') {
                depth -= 1;
                self.i += 2;
                if depth == 0 {
                    return;
                }
            } else {
                self.i += 1;
            }
        }
    }

    /// Skips a literal delimited by `quote`, where a doubled quote is an escape.
    fn skip_quoted(&mut self, quote: u8, backslash_escapes: bool) {
        self.i += 1;
        while self.i < self.src.len() {
            let b = self.src[self.i];
            if backslash_escapes && b == b'\\' {
                self.i += 2;
            } else if b == quote {
                if self.peek(1) == Some(quote) {
                    self.i += 2;
                } else {
                    self.i += 1;
                    return;
                }
            } else {
                self.i += 1;
            }
        }
        self.i = self.src.len();
    }

    fn skip_until(&mut self, close: u8) {
        while self.i < self.src.len() && self.src[self.i] != close {
            self.i += 1;
        }
        self.i = (self.i + 1).min(self.src.len());
    }

    /// Recognises `$tag$ … $tag$` (tag may be empty). `$1` placeholders and
    /// `$` inside identifiers are left alone.
    fn try_skip_dollar_quote(&mut self) -> bool {
        let start = self.i;
        if start > 0 && is_word_byte(self.src[start - 1]) {
            return false;
        }
        let mut j = start + 1;
        if j < self.src.len() && self.src[j].is_ascii_digit() {
            return false;
        }
        while j < self.src.len() && is_word_byte(self.src[j]) {
            j += 1;
        }
        if j >= self.src.len() || self.src[j] != b'$' {
            return false;
        }
        let tag = &self.src[start..=j];
        let body_start = j + 1;
        self.i = self.src[body_start..]
            .windows(tag.len())
            .position(|w| w == tag)
            .map_or(self.src.len(), |p| body_start + p + tag.len());
        true
    }
}
