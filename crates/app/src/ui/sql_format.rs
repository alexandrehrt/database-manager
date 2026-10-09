//! A small SQL formatter: keywords upper-cased, main clauses on their own
//! lines, SELECT / SET lists one item per line, AND / OR conditions on new
//! lines and subqueries indented. String literals, quoted identifiers,
//! comments and dollar-quoted bodies are copied unchanged. Procedural blocks
//! (BEGIN … END, CREATE FUNCTION / PROCEDURE / TRIGGER / PACKAGE) are left as
//! they are, since their layout carries meaning a token pass can't see.

use crate::ui::sql_highlight::KEYWORDS;

const INDENT: &str = "    ";

#[derive(Debug, Clone, PartialEq)]
enum Tok<'a> {
    Word(&'a str),
    /// Literal text copied verbatim: strings, quoted identifiers, dollar bodies, placeholders.
    Verbatim(&'a str),
    LineComment(&'a str),
    BlockComment(&'a str),
    Punct(&'a str),
}

/// Tokens, each with whether whitespace preceded it in the input.
fn tokenize(sql: &str) -> Vec<(Tok<'_>, bool)> {
    let b = sql.as_bytes();
    let mut out: Vec<Tok<'_>> = Vec::new();
    let mut raw = Vec::new();
    let mut spaced = false;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let start = i;
        if c.is_ascii_whitespace() {
            i += 1;
            spaced = true;
            continue;
        }
        let before = out.len();
        if c == b'-' && b.get(i + 1) == Some(&b'-') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            out.push(Tok::LineComment(sql[start..i].trim_end()));
        } else if c == b'/' && b.get(i + 1) == Some(&b'*') {
            i += 2;
            while i < b.len() && !(b[i] == b'*' && b.get(i + 1) == Some(&b'/')) {
                i += 1;
            }
            i = (i + 2).min(b.len());
            out.push(Tok::BlockComment(&sql[start..i]));
        } else if c == b'\'' || c == b'"' || c == b'`' {
            i += 1;
            while i < b.len() {
                if b[i] == c && b.get(i + 1) == Some(&c) {
                    i += 2;
                } else if b[i] == c {
                    i += 1;
                    break;
                } else {
                    i += 1;
                }
            }
            out.push(Tok::Verbatim(&sql[start..i]));
        } else if c == b'$' {
            // $1 placeholder, or a $tag$ … $tag$ body.
            i += 1;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            let tag = &sql[start..i];
            if b.get(i) == Some(&b'$') && !tag[1..].starts_with(|ch: char| ch.is_ascii_digit()) {
                let tag = &sql[start..=i];
                i += 1;
                i = sql[i..].find(tag).map_or(b.len(), |p| i + p + tag.len());
            }
            out.push(Tok::Verbatim(&sql[start..i]));
        } else if (c == b':' || c == b'?') && b.get(i + 1).is_some_and(|n| n.is_ascii_alphanumeric() || *n == b'_') {
            // :name / ?1 bind placeholders (but not :: casts).
            i += 1;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            out.push(Tok::Verbatim(&sql[start..i]));
        } else if c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80 {
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$' || b[i] >= 0x80) {
                i += 1;
            }
            // Decimal numbers stay one token.
            if c.is_ascii_digit() && b.get(i) == Some(&b'.') {
                i += 1;
                while i < b.len() && b[i].is_ascii_alphanumeric() {
                    i += 1;
                }
            }
            out.push(Tok::Word(&sql[start..i]));
        } else if matches!(c, b'(' | b')' | b',' | b';' | b'.') {
            i += 1;
            out.push(Tok::Punct(&sql[start..i]));
        } else {
            // Operator run such as <=, ||, ::, ->>.
            i += 1;
            while i < b.len() && b"<>=!|&+-*/%^~:#@".contains(&b[i]) && !(b[i] == b'-' && b.get(i + 1) == Some(&b'-')) {
                i += 1;
            }
            out.push(Tok::Punct(&sql[start..i]));
        }
        if out.len() > before {
            let last = out.pop().unwrap_or(Tok::Punct(""));
            raw.push((last, std::mem::take(&mut spaced)));
        }
    }
    raw
}

fn upper(word: &str) -> String {
    word.to_ascii_uppercase()
}

fn is_keyword(word: &str) -> bool {
    KEYWORDS.iter().any(|k| k.eq_ignore_ascii_case(word))
}

/// Clauses that start a new line, as sequences of words.
const CLAUSES: &[&[&str]] = &[
    &["SELECT"],
    &["FROM"],
    &["WHERE"],
    &["GROUP", "BY"],
    &["ORDER", "BY"],
    &["HAVING"],
    &["LIMIT"],
    &["OFFSET"],
    &["FETCH"],
    &["UNION", "ALL"],
    &["UNION"],
    &["INTERSECT"],
    &["EXCEPT"],
    &["MINUS"],
    &["INSERT", "INTO"],
    &["VALUES"],
    &["UPDATE"],
    &["SET"],
    &["DELETE", "FROM"],
    &["RETURNING"],
    &["WITH"],
    &["ON", "CONFLICT"],
    &["LEFT", "OUTER", "JOIN"],
    &["RIGHT", "OUTER", "JOIN"],
    &["FULL", "OUTER", "JOIN"],
    &["LEFT", "JOIN"],
    &["RIGHT", "JOIN"],
    &["FULL", "JOIN"],
    &["INNER", "JOIN"],
    &["CROSS", "JOIN"],
    &["NATURAL", "JOIN"],
    &["JOIN"],
];

/// Clauses whose comma-separated list puts one item per line.
const LIST_CLAUSES: &[&str] = &["SELECT", "SET"];

fn clause_at(toks: &[Tok<'_>], i: usize) -> Option<&'static [&'static str]> {
    CLAUSES.iter().copied().find(|clause| {
        clause
            .iter()
            .enumerate()
            .all(|(k, w)| matches!(toks.get(i + k), Some(Tok::Word(x)) if x.eq_ignore_ascii_case(w)))
    })
}

/// Whether the statement is procedural code that is left untouched.
fn is_procedural(toks: &[Tok<'_>]) -> bool {
    let words: Vec<String> =
        toks.iter().filter_map(|t| if let Tok::Word(w) = t { Some(upper(w)) } else { None }).take(6).collect();
    match words.first().map(String::as_str) {
        Some("BEGIN" | "DECLARE") => true,
        Some("CREATE") => words
            .iter()
            .any(|w| matches!(w.as_str(), "FUNCTION" | "PROCEDURE" | "TRIGGER" | "PACKAGE" | "BODY" | "TYPE")),
        _ => false,
    }
}

/// Formats `sql`, one or more statements.
pub fn format(sql: &str) -> String {
    let (toks, spaced): (Vec<Tok<'_>>, Vec<bool>) = tokenize(sql).into_iter().unzip();
    if toks.is_empty() || is_procedural(&toks) {
        return sql.to_string();
    }
    let mut out = String::new();
    // One entry per open parenthesis: whether it holds a subquery (indented block).
    let mut parens: Vec<bool> = Vec::new();
    // Clause in effect at each block level (index 0 = top level).
    let mut clause: Vec<&str> = vec![""];
    let level = |parens: &[bool]| parens.iter().filter(|&&b| b).count();
    let at_block = |parens: &[bool]| parens.last().is_none_or(|&b| b);
    let mut prev: Option<Tok<'_>> = None;
    let mut need_newline = false;

    let newline = |out: &mut String, indent: usize| {
        while out.ends_with(' ') {
            out.pop();
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&INDENT.repeat(indent));
    };

    let mut i = 0;
    while i < toks.len() {
        let tok = toks[i].clone();
        let ctx = Spacing {
            spaced: spaced[i],
            // A sign right after an operator, "(", "," or a keyword is unary: -1.
            unary_before: i >= 2
                && matches!(toks[i - 1], Tok::Punct("-" | "+"))
                && match &toks[i - 2] {
                    Tok::Punct(p) => *p != ")",
                    Tok::Word(w) => is_keyword(w),
                    _ => false,
                },
        };
        if need_newline {
            newline(&mut out, level(&parens));
            need_newline = false;
            prev = None;
        }
        // Clause keywords start a line when they belong to the current block.
        if at_block(&parens)
            && let Some(words) = clause_at(&toks, i)
        {
            let is_first = out.trim().is_empty() || matches!(prev, None | Some(Tok::Punct("(")));
            if !is_first {
                newline(&mut out, level(&parens));
            } else if matches!(prev, Some(Tok::Punct("("))) {
                // Subquery: its first clause goes on a fresh, indented line.
                newline(&mut out, level(&parens));
            }
            let text: Vec<String> = words.iter().map(|w| w.to_string()).collect();
            out.push_str(&text.join(" "));
            if let Some(c) = clause.last_mut() {
                *c = words[0];
            }
            i += words.len();
            prev = Some(Tok::Word(words[words.len() - 1]));
            // List clauses with several items put each on its own line.
            if LIST_CLAUSES.contains(&words[0]) && list_has_comma(&toks, i) {
                newline(&mut out, level(&parens) + 1);
                prev = None;
            }
            continue;
        }
        let in_list = clause.last().is_some_and(|c| LIST_CLAUSES.contains(c));
        match &tok {
            Tok::Word(w) if at_block(&parens) && (w.eq_ignore_ascii_case("AND") || w.eq_ignore_ascii_case("OR")) => {
                // Conditions of WHERE / HAVING / ON on their own lines, but not BETWEEN … AND.
                let between = toks[..i]
                    .iter()
                    .rev()
                    .take(4)
                    .any(|t| matches!(t, Tok::Word(x) if x.eq_ignore_ascii_case("BETWEEN")));
                if between {
                    space(&mut out, prev.as_ref(), &tok, &ctx);
                } else {
                    newline(&mut out, level(&parens) + 1);
                }
                out.push_str(&upper(w));
            }
            Tok::Word(w) => {
                space(&mut out, prev.as_ref(), &tok, &ctx);
                // A keyword used as a qualified name (t.order) keeps its case.
                let qualified =
                    matches!(prev, Some(Tok::Punct("."))) || matches!(toks.get(i + 1), Some(Tok::Punct(".")));
                if is_keyword(w) && !qualified { out.push_str(&upper(w)) } else { out.push_str(w) }
            }
            Tok::Punct("(") => {
                space(&mut out, prev.as_ref(), &tok, &ctx);
                out.push('(');
                let subquery = matches!(toks.get(i + 1), Some(Tok::Word(w)) if w.eq_ignore_ascii_case("SELECT") || w.eq_ignore_ascii_case("WITH"));
                parens.push(subquery);
                if subquery {
                    clause.push("");
                }
            }
            Tok::Punct(")") => {
                if parens.pop() == Some(true) {
                    clause.pop();
                    newline(&mut out, level(&parens));
                }
                out.push(')');
            }
            Tok::Punct(",") => {
                out.push(',');
                if at_block(&parens) && in_list {
                    newline(&mut out, level(&parens) + 1);
                    prev = None;
                    i += 1;
                    continue;
                }
            }
            Tok::Punct(";") => {
                out.push(';');
                parens.clear();
                clause = vec![""];
                if i + 1 < toks.len() {
                    out.push('\n');
                    need_newline = true;
                }
            }
            Tok::LineComment(c) => {
                space(&mut out, prev.as_ref(), &tok, &ctx);
                out.push_str(c);
                need_newline = true;
            }
            _ => {
                space(&mut out, prev.as_ref(), &tok, &ctx);
                out.push_str(text(&tok));
            }
        }
        prev = Some(tok);
        i += 1;
    }
    out.trim_end().to_string()
}

/// Whether the list starting at `i` has a comma before its clause ends.
fn list_has_comma(toks: &[Tok<'_>], i: usize) -> bool {
    let mut depth = 0usize;
    for (j, t) in toks.iter().enumerate().skip(i) {
        match t {
            Tok::Punct("(") => depth += 1,
            Tok::Punct(")") if depth == 0 => return false,
            Tok::Punct(")") => depth -= 1,
            Tok::Punct(",") if depth == 0 => return true,
            Tok::Punct(";") => return false,
            _ if depth == 0 && clause_at(toks, j).is_some() => return false,
            _ => {}
        }
    }
    false
}

fn text<'a>(t: &Tok<'a>) -> &'a str {
    match t {
        Tok::Word(s) | Tok::Verbatim(s) | Tok::LineComment(s) | Tok::BlockComment(s) | Tok::Punct(s) => s,
    }
}

/// What decides the space before a token besides the tokens themselves.
struct Spacing {
    /// The input had whitespace before it.
    spaced: bool,
    /// The previous token is a unary sign.
    unary_before: bool,
}

/// Adds the space that goes between `prev` and `next`, if any.
fn space(out: &mut String, prev: Option<&Tok<'_>>, next: &Tok<'_>, ctx: &Spacing) {
    let Some(prev) = prev else { return };
    if out.ends_with(' ') || out.ends_with('\n') || out.is_empty() {
        return;
    }
    let no_space = ctx.unary_before
        || match (prev, next) {
            (Tok::Punct("(" | "." | "::"), _) => true,
            (_, Tok::Punct("." | "::" | ")" | "," | ";")) => true,
            // Function calls and type sizes keep name(…) as written; keywords like IN ( get a space.
            (Tok::Word(w), Tok::Punct("(")) => !ctx.spaced && (!is_keyword(w) || is_function_keyword(w)),
            (Tok::Verbatim(_), Tok::Punct("(")) => !ctx.spaced,
            _ => false,
        };
    if !no_space {
        out.push(' ');
    }
}

/// Keywords that are also function or type names, written without a space before "(".
fn is_function_keyword(w: &str) -> bool {
    matches!(
        upper(w).as_str(),
        "COUNT"
            | "SUM"
            | "AVG"
            | "MIN"
            | "MAX"
            | "COALESCE"
            | "NULLIF"
            | "CAST"
            | "EXISTS"
            | "VARCHAR"
            | "CHAR"
            | "NUMERIC"
            | "DECIMAL"
            | "NUMBER"
            | "VARCHAR2"
            | "TIMESTAMP"
            | "REPLACE"
            | "LEFT"
            | "RIGHT"
            | "ROUND"
            | "UPPER"
            | "LOWER"
            | "SUBSTRING"
            | "EXTRACT"
            | "ROW_NUMBER"
            | "RANK"
            | "OVER"
    )
}

/// Toggles `--` comments on the lines `start..end` (byte offsets) touch. Returns
/// the new text and the byte range of the affected lines in it.
pub fn toggle_comment(sql: &str, start: usize, end: usize) -> (String, std::ops::Range<usize>) {
    let line_start = sql[..start].rfind('\n').map_or(0, |p| p + 1);
    // A selection ending at the start of a line doesn't include that line.
    let end = if end > start && sql[..end].ends_with('\n') { end - 1 } else { end };
    let line_end = sql[end..].find('\n').map_or(sql.len(), |p| end + p);
    let block = &sql[line_start..line_end];
    let lines: Vec<&str> = block.split('\n').collect();
    let filled: Vec<&&str> = lines.iter().filter(|l| !l.trim().is_empty()).collect();
    let all_commented = !filled.is_empty() && filled.iter().all(|l| l.trim_start().starts_with("--"));
    let indent = filled.iter().map(|l| l.len() - l.trim_start().len()).min().unwrap_or(0);
    let new_lines: Vec<String> = lines
        .iter()
        .map(|l| {
            if l.trim().is_empty() {
                l.to_string()
            } else if all_commented {
                let pad = l.len() - l.trim_start().len();
                let rest = &l[pad + 2..];
                format!("{}{}", &l[..pad], rest.strip_prefix(' ').unwrap_or(rest))
            } else {
                format!("{}-- {}", &l[..indent], &l[indent..])
            }
        })
        .collect();
    let replaced = new_lines.join("\n");
    let mut out = String::with_capacity(sql.len() + lines.len() * 3);
    out.push_str(&sql[..line_start]);
    out.push_str(&replaced);
    out.push_str(&sql[line_end..]);
    (out, line_start..line_start + replaced.len())
}

/// Byte ranges of `needle` in `hay`, optionally ignoring case.
pub fn find_all(hay: &str, needle: &str, case_sensitive: bool) -> Vec<std::ops::Range<usize>> {
    if needle.is_empty() {
        return Vec::new();
    }
    if case_sensitive {
        return hay.match_indices(needle).map(|(i, m)| i..i + m.len()).collect();
    }
    let mut out = Vec::new();
    let mut from = 0;
    while from < hay.len() {
        let Some((start, len)) = hay[from..].char_indices().find_map(|(i, _)| {
            let rest = &hay[from + i..];
            let mut hs = rest.char_indices();
            let mut matched = 0;
            for n in needle.chars() {
                let (j, h) = hs.next()?;
                if !h.to_lowercase().eq(n.to_lowercase()) {
                    return None;
                }
                matched = j + h.len_utf8();
            }
            Some((from + i, matched))
        }) else {
            break;
        };
        out.push(start..start + len);
        from = start + len.max(1);
    }
    out
}

/// For an UPDATE or DELETE without a WHERE clause: what it targets, such as
/// "DELETE FROM orders", for a confirmation. None for anything else.
pub fn unrestricted_write(sql: &str) -> Option<String> {
    let (toks, _): (Vec<Tok<'_>>, Vec<bool>) = tokenize(sql).into_iter().unzip();
    let words = |t: &Tok<'_>| if let Tok::Word(w) = t { Some(w.to_ascii_uppercase()) } else { None };
    let first = toks.iter().find_map(words)?;
    if first != "UPDATE" && first != "DELETE" {
        return None;
    }
    let mut depth = 0usize;
    for t in &toks {
        match t {
            Tok::Punct("(") => depth += 1,
            Tok::Punct(")") => depth = depth.saturating_sub(1),
            Tok::Word(w) if depth == 0 && w.eq_ignore_ascii_case("WHERE") => return None,
            _ => {}
        }
    }
    // The verb (with FROM / ONLY) upper-cased, then the table name as written.
    let mut head = Vec::new();
    let mut rest = toks.iter().skip_while(|t| words(t).is_none()).peekable();
    while let Some(Tok::Word(w)) = rest.peek() {
        if !matches!(w.to_ascii_uppercase().as_str(), "UPDATE" | "DELETE" | "FROM" | "ONLY") {
            break;
        }
        head.push(w.to_ascii_uppercase());
        rest.next();
    }
    let mut name = String::new();
    for t in rest {
        match t {
            Tok::Word(_) | Tok::Verbatim(_) if !name.is_empty() && !name.ends_with('.') => break,
            Tok::Word(w) | Tok::Verbatim(w) => name.push_str(w),
            Tok::Punct(".") => name.push('.'),
            _ => break,
        }
    }
    head.push(name);
    Some(head.join(" "))
}
