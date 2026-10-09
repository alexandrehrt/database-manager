//! SQL completion: what is being typed at the cursor, which tables the
//! statement refers to, and the matching keywords, tables and columns.

use std::collections::HashMap;

use dbm_core::{Dialect, TableDetails};

use crate::ui::sql_highlight::KEYWORDS;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Column,
    Table,
    Schema,
    Keyword,
}

#[derive(Clone, Debug)]
pub struct Item {
    pub label: String,
    /// Text that replaces the word being typed.
    pub insert: String,
    /// Shown dimmed next to the label: a column's type, a table's schema.
    pub detail: String,
    pub kind: Kind,
}

/// The word being completed.
#[derive(Debug, PartialEq)]
pub struct Context {
    /// Byte offset where the word starts; the completion replaces
    /// `start..cursor`.
    pub start: usize,
    pub prefix: String,
    /// The identifier before a `.` right in front of the word (`o` in `o.cu`).
    pub qualifier: Option<String>,
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// Reads an identifier, or a "quoted" one, ending at byte `end`.
fn ident_before(text: &str, end: usize) -> Option<(usize, String)> {
    let before = &text[..end];
    if let Some(stripped) = before.strip_suffix('"') {
        let open = stripped.rfind('"')?;
        return Some((open, stripped[open + 1..].to_string()));
    }
    let start = before.char_indices().rev().take_while(|(_, c)| is_ident(*c)).last().map(|(i, _)| i)?;
    Some((start, before[start..].to_string()))
}

pub fn context(text: &str, cursor: usize) -> Context {
    let cursor = cursor.min(text.len());
    let before = &text[..cursor];
    let start = before.char_indices().rev().take_while(|(_, c)| is_ident(*c)).last().map_or(cursor, |(i, _)| i);
    let prefix = before[start..].to_string();
    let qualifier = before[..start].strip_suffix('.').and_then(|q| ident_before(q, q.len())).map(|(_, name)| name);
    Context { start, prefix, qualifier }
}

/// Whether `cursor` is inside a string literal or a line comment of its line,
/// where completion would only get in the way.
pub fn in_literal_or_comment(text: &str, cursor: usize) -> bool {
    let cursor = cursor.min(text.len());
    let line = &text[text[..cursor].rfind('\n').map_or(0, |i| i + 1)..cursor];
    let mut in_string = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => in_string = !in_string,
            '-' if !in_string && chars.peek() == Some(&'-') => return true,
            _ => {}
        }
    }
    in_string
}

/// A table named in FROM / JOIN / UPDATE / INTO, with its alias.
#[derive(Debug, PartialEq)]
pub struct TableRef {
    pub schema: Option<String>,
    pub table: String,
    pub alias: Option<String>,
}

#[derive(Debug, PartialEq)]
enum Token {
    Word(String),
    Quoted(String),
    Punct(char),
}

fn tokenize(sql: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let mut chars = sql.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c.is_whitespace() {
            continue;
        }
        if c == '-' && chars.peek().map(|(_, n)| *n) == Some('-') {
            while chars.peek().is_some_and(|(_, n)| *n != '\n') {
                chars.next();
            }
        } else if c == '\'' {
            while chars.next().is_some_and(|(_, n)| n != '\'') {}
        } else if c == '"' {
            let mut s = String::new();
            for (_, n) in chars.by_ref() {
                if n == '"' {
                    break;
                }
                s.push(n);
            }
            out.push(Token::Quoted(s));
        } else if is_ident(c) {
            let mut end = i + c.len_utf8();
            while let Some((j, n)) = chars.peek().copied() {
                if !is_ident(n) {
                    break;
                }
                end = j + n.len_utf8();
                chars.next();
            }
            out.push(Token::Word(sql[i..end].to_string()));
        } else {
            out.push(Token::Punct(c));
        }
    }
    out
}

const NOT_ALIAS: &[&str] = &[
    "WHERE",
    "JOIN",
    "INNER",
    "LEFT",
    "RIGHT",
    "FULL",
    "CROSS",
    "NATURAL",
    "ON",
    "USING",
    "GROUP",
    "ORDER",
    "LIMIT",
    "OFFSET",
    "HAVING",
    "UNION",
    "EXCEPT",
    "INTERSECT",
    "SET",
    "VALUES",
    "RETURNING",
    "WINDOW",
    "FOR",
    "SELECT",
    "DEFAULT",
    "OUTER",
    "LATERAL",
];

pub fn table_refs(sql: &str) -> Vec<TableRef> {
    let tokens = tokenize(sql);
    let name = |t: &Token| match t {
        Token::Word(w) => Some(w.clone()),
        Token::Quoted(q) => Some(q.clone()),
        Token::Punct(_) => None,
    };
    let is_kw = |t: &Token, kws: &[&str]| matches!(t, Token::Word(w) if kws.iter().any(|k| w.eq_ignore_ascii_case(k)));
    let mut refs = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        if !is_kw(&tokens[i], &["FROM", "JOIN", "UPDATE", "INTO"]) {
            i += 1;
            continue;
        }
        i += 1;
        // A FROM list may hold several comma-separated tables.
        while let Some(first) = tokens.get(i).and_then(name) {
            if is_kw(&tokens[i], NOT_ALIAS) {
                break;
            }
            i += 1;
            let (schema, table) = if tokens.get(i) == Some(&Token::Punct('.')) {
                match tokens.get(i + 1).and_then(name) {
                    Some(t) => {
                        i += 2;
                        (Some(first), t)
                    }
                    None => (None, first),
                }
            } else {
                (None, first)
            };
            if is_kw(tokens.get(i).unwrap_or(&Token::Punct(' ')), &["AS"]) {
                i += 1;
            }
            let alias = match tokens.get(i) {
                Some(t @ (Token::Word(_) | Token::Quoted(_))) if !is_kw(t, NOT_ALIAS) => {
                    i += 1;
                    name(t)
                }
                _ => None,
            };
            refs.push(TableRef { schema, table, alias });
            if tokens.get(i) == Some(&Token::Punct(',')) {
                i += 1;
            } else {
                break;
            }
        }
    }
    refs
}

/// What the console knows about the data source's objects.
pub struct Catalog<'a> {
    /// (schema, table) of every loaded table and view.
    pub tables: &'a [(String, String)],
    pub details: &'a HashMap<(String, String), TableDetails>,
    /// Schema assumed for unqualified table names.
    pub default_schema: &'a str,
}

impl Catalog<'_> {
    pub fn resolve(&self, schema: Option<&str>, table: &str) -> Option<(String, String)> {
        let schema = schema.unwrap_or(self.default_schema);
        let exact = self.tables.iter().find(|(s, t)| s == schema && t.eq_ignore_ascii_case(table));
        exact.or_else(|| self.tables.iter().find(|(_, t)| t.eq_ignore_ascii_case(table))).cloned()
    }
}

/// Identifier as typed, quoted when the dialect would otherwise fold or reject it.
fn insert_ident(dialect: Dialect, name: &str) -> String {
    let plain = name.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !KEYWORDS.iter().any(|k| k.eq_ignore_ascii_case(name));
    // Oracle folds unquoted names to upper case, so upper-case names need no quotes.
    let oracle_plain = dialect == Dialect::Oracle
        && name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || matches!(c, '_' | '$' | '#'))
        && !KEYWORDS.iter().any(|k| k.eq_ignore_ascii_case(name));
    if plain
        || oracle_plain
        || (dialect == Dialect::Sqlite && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
    {
        name.to_string()
    } else {
        dialect.quote_ident(name)
    }
}

/// Candidates for `ctx` in `statement`, plus tables whose columns are needed
/// but not loaded yet.
pub fn candidates(
    dialect: Dialect,
    ctx: &Context,
    statement: &str,
    catalog: &Catalog<'_>,
) -> (Vec<Item>, Vec<(String, String)>) {
    let refs = table_refs(statement);
    let mut missing = Vec::new();
    let mut items = Vec::new();
    let mut push_columns = |items: &mut Vec<Item>, key: (String, String)| match catalog.details.get(&key) {
        Some(d) => items.extend(d.columns.iter().map(|c| Item {
            label: c.name.clone(),
            insert: insert_ident(dialect, &c.name),
            detail: format!("{} · {}", c.data_type, d.name),
            kind: Kind::Column,
        })),
        None => missing.push(key),
    };

    if let Some(q) = &ctx.qualifier {
        let by_alias = refs.iter().find(|r| {
            r.alias.as_deref().is_some_and(|a| a.eq_ignore_ascii_case(q))
                || (r.alias.is_none() && r.table.eq_ignore_ascii_case(q))
        });
        let table =
            by_alias.and_then(|r| catalog.resolve(r.schema.as_deref(), &r.table)).or_else(|| catalog.resolve(None, q));
        if let Some(key) = table {
            push_columns(&mut items, key);
        } else {
            // `schema.` lists that schema's tables.
            items.extend(catalog.tables.iter().filter(|(s, _)| s.eq_ignore_ascii_case(q)).map(|(s, t)| Item {
                label: t.clone(),
                insert: insert_ident(dialect, t),
                detail: s.clone(),
                kind: Kind::Table,
            }));
        }
    } else {
        for r in &refs {
            if let Some(key) = catalog.resolve(r.schema.as_deref(), &r.table) {
                push_columns(&mut items, key);
            }
        }
        items.extend(catalog.tables.iter().map(|(s, t)| Item {
            label: t.clone(),
            insert: if s == catalog.default_schema {
                insert_ident(dialect, t)
            } else {
                format!("{}.{}", insert_ident(dialect, s), insert_ident(dialect, t))
            },
            detail: s.clone(),
            kind: Kind::Table,
        }));
        let mut schemas: Vec<&String> =
            catalog.tables.iter().map(|(s, _)| s).filter(|s| *s != catalog.default_schema).collect();
        schemas.dedup();
        items.extend(schemas.into_iter().map(|s| Item {
            label: s.clone(),
            insert: insert_ident(dialect, s),
            detail: "schema".into(),
            kind: Kind::Schema,
        }));
        let upper = ctx.prefix.chars().next().is_none_or(|c| c.is_uppercase());
        items.extend(KEYWORDS.iter().map(|k| Item {
            label: k.to_string(),
            insert: if upper { k.to_string() } else { k.to_lowercase() },
            detail: "keyword".into(),
            kind: Kind::Keyword,
        }));
    }

    let prefix = ctx.prefix.to_lowercase();
    items.retain(|i| i.label.to_lowercase().starts_with(&prefix) && !i.label.eq_ignore_ascii_case(&ctx.prefix));
    let mut seen = std::collections::HashSet::new();
    items.retain(|i| seen.insert((i.kind as u8, i.insert.clone())));
    missing.sort();
    missing.dedup();
    (items, missing)
}
