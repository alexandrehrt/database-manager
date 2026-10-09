//! Editor inspections: likely mistakes flagged while typing. Each check is
//! cautious and stays quiet when it lacks the metadata to be sure: unknown
//! names are only reported for schemas and tables that are loaded.

use std::ops::Range;

use dbm_core::{Dialect, sql_split};

use crate::ui::completion::Catalog;
use crate::ui::sql_format::{self, Tok, tokenize};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Severity {
    Warning,
    Error,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Issue {
    /// Byte range in the editor text.
    pub range: Range<usize>,
    pub message: String,
    pub severity: Severity,
}

/// A token with its byte range in the statement.
struct T<'a> {
    tok: Tok<'a>,
    range: Range<usize>,
}

fn word(t: &Tok<'_>) -> Option<String> {
    match t {
        Tok::Word(w) => Some(w.to_ascii_uppercase()),
        _ => None,
    }
}

fn is(t: Option<&T<'_>>, w: &str) -> bool {
    matches!(t.map(|t| &t.tok), Some(Tok::Word(x)) if x.eq_ignore_ascii_case(w))
}

/// An identifier as the database sees it: quoted names keep their case.
fn ident(t: &Tok<'_>) -> Option<(String, bool)> {
    match t {
        Tok::Word(w) => Some((w.to_string(), false)),
        Tok::Verbatim(v) if v.len() >= 2 && (v.starts_with('"') || v.starts_with('`')) => {
            Some((v[1..v.len() - 1].replace("\"\"", "\""), true))
        }
        _ => None,
    }
}

/// System objects that are never in the explorer's lists.
fn system_name(name: &str, dialect: Dialect) -> bool {
    let n = name.to_ascii_lowercase();
    match dialect {
        Dialect::Postgres => n.starts_with("pg_") || n == "information_schema",
        Dialect::Sqlite => n.starts_with("sqlite_") || n.starts_with("pragma_"),
        Dialect::Oracle => {
            n == "dual"
                || n.starts_with("all_")
                || n.starts_with("user_")
                || n.starts_with("dba_")
                || n.starts_with("v$")
                || n.starts_with("gv$")
        }
    }
}

/// Inspects every statement of `sql`. Tables whose columns are needed but not
/// loaded yet are added to `wanted` as (schema, table).
pub fn inspect(sql: &str, dialect: Dialect, catalog: &Catalog<'_>, wanted: &mut Vec<(String, String)>) -> Vec<Issue> {
    let mut issues = Vec::new();
    for span in sql_split::split(sql, dialect) {
        let base = span.start;
        let stmt = &sql[span];
        let toks: Vec<T<'_>> = tokenize(stmt)
            .into_iter()
            .map(|(tok, _)| {
                let text = match &tok {
                    Tok::Word(s) | Tok::Verbatim(s) | Tok::LineComment(s) | Tok::BlockComment(s) | Tok::Punct(s) => *s,
                };
                let start = text.as_ptr() as usize - stmt.as_ptr() as usize;
                T { tok, range: start..start + text.len() }
            })
            .filter(|t| !matches!(t.tok, Tok::LineComment(_)))
            .collect();
        let mut push = |range: Range<usize>, severity: Severity, message: String| {
            issues.push(Issue { range: base + range.start..base + range.end, message, severity });
        };
        syntax(&toks, &mut push);
        conditions(&toks, &mut push);
        joins(&toks, &mut push);
        if let Some((target, _)) = sql_format::unrestricted_write(stmt)
            && !target.starts_with("DROP")
            && !target.starts_with("TRUNCATE")
            && let Some(first) = toks.iter().find(|t| matches!(t.tok, Tok::Word(_)))
        {
            push(first.range.clone(), Severity::Warning, format!("{target} has no WHERE clause: it affects every row"));
        }
        names(&toks, dialect, catalog, wanted, &mut push);
    }
    issues
}

/// Unterminated strings / comments and unbalanced parentheses.
fn syntax(toks: &[T<'_>], push: &mut impl FnMut(Range<usize>, Severity, String)) {
    let mut open = Vec::new();
    for t in toks {
        match &t.tok {
            Tok::Verbatim(v) if v.starts_with('\'') && (v.len() < 2 || !v.ends_with('\'')) => {
                push(t.range.clone(), Severity::Error, "Unterminated string".into());
            }
            Tok::BlockComment(c) if !c.ends_with("*/") || c.len() < 4 => {
                push(t.range.start..t.range.start + 2, Severity::Error, "Unterminated comment".into());
            }
            Tok::Punct("(") => open.push(t.range.clone()),
            Tok::Punct(")") if open.pop().is_none() => {
                push(t.range.clone(), Severity::Error, "Unmatched closing parenthesis".into());
            }
            _ => {}
        }
    }
    for range in open {
        push(range, Severity::Error, "Unclosed parenthesis".into());
    }
}

/// `= NULL` / `<> NULL` in conditions, which are never true.
fn conditions(toks: &[T<'_>], push: &mut impl FnMut(Range<usize>, Severity, String)) {
    let mut in_condition = false;
    for (i, t) in toks.iter().enumerate() {
        if let Some(w) = word(&t.tok) {
            match w.as_str() {
                "WHERE" | "ON" | "HAVING" | "WHEN" => in_condition = true,
                "SET" | "SELECT" | "VALUES" | "FROM" | "THEN" | "ELSE" => in_condition = false,
                _ => {}
            }
        }
        if in_condition
            && let Tok::Punct(op @ ("=" | "<>" | "!=")) = t.tok
            && is(toks.get(i + 1), "NULL")
        {
            let fix = if op == "=" { "IS NULL" } else { "IS NOT NULL" };
            let end = toks[i + 1].range.end;
            push(t.range.start..end, Severity::Warning, format!("Comparing with NULL is never true; use {fix}"));
        }
    }
}

/// JOIN without ON / USING (other than CROSS and NATURAL joins).
fn joins(toks: &[T<'_>], push: &mut impl FnMut(Range<usize>, Severity, String)) {
    for (i, t) in toks.iter().enumerate() {
        if !is(Some(t), "JOIN") || (i > 0 && (is(toks.get(i - 1), "CROSS") || is(toks.get(i - 1), "NATURAL"))) {
            continue;
        }
        let mut depth = 0i32;
        let mut found = false;
        for n in &toks[i + 1..] {
            match &n.tok {
                Tok::Punct("(") => depth += 1,
                Tok::Punct(")") if depth == 0 => break,
                Tok::Punct(")") => depth -= 1,
                Tok::Punct(";") => break,
                Tok::Word(w) if depth == 0 => {
                    let w = w.to_ascii_uppercase();
                    if w == "ON" || w == "USING" {
                        found = true;
                        break;
                    }
                    if matches!(
                        w.as_str(),
                        "JOIN"
                            | "WHERE"
                            | "GROUP"
                            | "ORDER"
                            | "LIMIT"
                            | "UNION"
                            | "EXCEPT"
                            | "INTERSECT"
                            | "HAVING"
                            | "FETCH"
                            | "OFFSET"
                    ) {
                        break;
                    }
                }
                _ => {}
            }
        }
        if !found {
            push(t.range.clone(), Severity::Warning, "JOIN without ON or USING: every row matches every row".into());
        }
    }
}

/// Unknown tables after FROM / JOIN / UPDATE / INTO, and unknown columns after
/// a table or alias qualifier.
fn names(
    toks: &[T<'_>],
    dialect: Dialect,
    catalog: &Catalog<'_>,
    wanted: &mut Vec<(String, String)>,
    push: &mut impl FnMut(Range<usize>, Severity, String),
) {
    // Names defined by WITH, which are not tables.
    let mut ctes: Vec<String> = Vec::new();
    for (i, t) in toks.iter().enumerate() {
        if let Some((name, _)) = ident(&t.tok)
            && is(toks.get(i + 1), "AS")
            && matches!(toks.get(i + 2).map(|t| &t.tok), Some(Tok::Punct("(")))
            && i > 0
            && (is(toks.get(i - 1), "WITH")
                || matches!(toks[i - 1].tok, Tok::Punct(","))
                || is(toks.get(i - 1), "RECURSIVE"))
        {
            ctes.push(name.to_ascii_lowercase());
        }
    }
    let schemas_loaded = |schema: &str| catalog.tables.iter().any(|(s, _)| s.eq_ignore_ascii_case(schema));
    // (qualifier as written, resolved table) for column checks.
    let mut scope: Vec<(String, (String, String))> = Vec::new();
    let mut table_tokens = Vec::new();
    // Whether each token sits inside parentheses that aren't a subquery, where
    // FROM is part of a function call: extract(year FROM d), substring(s FROM 2).
    let mut in_call = Vec::with_capacity(toks.len());
    let mut stack: Vec<bool> = Vec::new();
    for (k, t) in toks.iter().enumerate() {
        in_call.push(stack.last().is_some_and(|subquery| !subquery));
        match t.tok {
            Tok::Punct("(") => stack.push(is(toks.get(k + 1), "SELECT") || is(toks.get(k + 1), "WITH")),
            Tok::Punct(")") => {
                stack.pop();
            }
            _ => {}
        }
    }
    let mut i = 0;
    while i < toks.len() {
        let starts_list = !in_call[i] && ["FROM", "JOIN", "UPDATE", "INTO"].iter().any(|k| is(toks.get(i), k));
        let into = is(toks.get(i), "INTO");
        i += 1;
        if !starts_list {
            continue;
        }
        while let Some((first, first_quoted)) = toks.get(i).and_then(|t| ident(&t.tok)) {
            if !first_quoted
                && sql_format::is_keyword(&first)
                && !matches!(toks.get(i + 1).map(|t| &t.tok), Some(Tok::Punct(".")))
            {
                break;
            }
            let start = toks[i].range.start;
            let (schema, table, quoted, end) = if matches!(toks.get(i + 1).map(|t| &t.tok), Some(Tok::Punct(".")))
                && let Some((t2, q2)) = toks.get(i + 2).and_then(|t| ident(&t.tok))
            {
                i += 3;
                (Some(first), t2, q2, toks[i - 1].range.end)
            } else {
                i += 1;
                (None, first, first_quoted, toks[i - 1].range.end)
            };
            table_tokens.push(start..end);
            // A function in FROM (generate_series(…), pragma_table_info(…)) is not a table.
            // After INTO the parenthesis is the column list.
            let function = !into && matches!(toks.get(i).map(|t| &t.tok), Some(Tok::Punct("(")));
            let known = catalog.resolve(schema.as_deref(), &table);
            let schema_for_check = schema.clone().unwrap_or_else(|| catalog.default_schema.to_string());
            if !function
                && known.is_none()
                && schema.is_none_or(|s| !system_name(&s, dialect))
                && !system_name(&table, dialect)
                && !ctes.contains(&table.to_ascii_lowercase())
                && schemas_loaded(&schema_for_check)
            {
                let shown = if quoted { format!("\"{table}\"") } else { table.clone() };
                push(start..end, Severity::Warning, format!("Unknown table {shown}"));
            }
            // Alias, with or without AS.
            if is(toks.get(i), "AS") {
                i += 1;
            }
            let alias = match toks.get(i).and_then(|t| ident(&t.tok)) {
                Some((a, q)) if q || !sql_format::is_keyword(&a) => {
                    i += 1;
                    Some(a)
                }
                _ => None,
            };
            if let Some(resolved) = known {
                scope.push((alias.unwrap_or_else(|| table.clone()), resolved.clone()));
                scope.push((table.clone(), resolved));
            }
            if matches!(toks.get(i).map(|t| &t.tok), Some(Tok::Punct(","))) {
                i += 1;
            } else {
                break;
            }
        }
    }
    // qualifier.column
    for w in toks.windows(3) {
        let (Some((qualifier, _)), Tok::Punct("."), Some((column, quoted))) =
            (ident(&w[0].tok), &w[1].tok, ident(&w[2].tok))
        else {
            continue;
        };
        if table_tokens.iter().any(|r| r.contains(&w[0].range.start)) {
            continue;
        }
        let Some((_, (schema, table))) = scope.iter().find(|(q, _)| q.eq_ignore_ascii_case(&qualifier)) else {
            continue;
        };
        let Some(details) = catalog.details.get(&(schema.clone(), table.clone())) else {
            let key = (schema.clone(), table.clone());
            if !wanted.contains(&key) {
                wanted.push(key);
            }
            continue;
        };
        let exists = details
            .columns
            .iter()
            .any(|c| if quoted { c.name == column } else { c.name.eq_ignore_ascii_case(&column) });
        if !exists {
            push(w[2].range.clone(), Severity::Warning, format!("{table} has no column {column}"));
        }
    }
}
