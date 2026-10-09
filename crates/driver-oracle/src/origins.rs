//! Column origins for Oracle results. ODPI-C doesn't say which table a
//! result column came from, so they are inferred for simple queries:
//!
//! `SELECT * | col, col … FROM [schema.]table [alias] [WHERE …] [ORDER BY …] [FETCH …]`
//!
//! with no joins, subqueries, expressions or column aliases. That covers
//! table data, FK navigation and grid editing; anything else gets no origins
//! (read-only, no links).

use dbm_core::ColumnOrigin;

#[derive(Debug, PartialEq)]
enum Token {
    /// Unquoted identifier or keyword, upper-cased as Oracle folds it.
    Word(String),
    /// "Quoted" identifier, case kept.
    Quoted(String),
    Punct(char),
}

fn tokenize(sql: &str) -> Option<Vec<Token>> {
    let mut out = Vec::new();
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_whitespace() {
            continue;
        }
        if c == '-' && chars.peek() == Some(&'-') {
            for n in chars.by_ref() {
                if n == '\n' {
                    break;
                }
            }
        } else if c == '"' {
            let mut s = String::new();
            loop {
                match chars.next()? {
                    '"' => break,
                    n => s.push(n),
                }
            }
            out.push(Token::Quoted(s));
        } else if c.is_alphanumeric() || c == '_' || c == '$' || c == '#' {
            let mut s = c.to_string();
            while let Some(&n) = chars.peek() {
                if !(n.is_alphanumeric() || n == '_' || n == '$' || n == '#') {
                    break;
                }
                s.push(n);
                chars.next();
            }
            out.push(Token::Word(s.to_uppercase()));
        } else {
            out.push(Token::Punct(c));
        }
    }
    Some(out)
}

fn name(t: &Token) -> Option<String> {
    match t {
        Token::Word(w) => Some(w.clone()),
        Token::Quoted(q) => Some(q.clone()),
        Token::Punct(_) => None,
    }
}

fn is_word(t: Option<&Token>, w: &str) -> bool {
    matches!(t, Some(Token::Word(x)) if x == w)
}

/// One origin per result column, `None` where it can't be inferred.
pub fn infer(sql: &str, current_schema: &str, columns: &[String]) -> Vec<Option<ColumnOrigin>> {
    simple_select(sql, current_schema, columns).unwrap_or_else(|| vec![None; columns.len()])
}

fn simple_select(sql: &str, current_schema: &str, columns: &[String]) -> Option<Vec<Option<ColumnOrigin>>> {
    let tokens = tokenize(sql)?;
    let mut i = 0;
    if !is_word(tokens.first(), "SELECT") {
        return None;
    }
    i += 1;
    // Select list: `*` or plain column names separated by commas.
    let mut list: Option<Vec<String>> = Some(Vec::new());
    if tokens.get(i) == Some(&Token::Punct('*')) {
        list = None;
        i += 1;
    } else {
        loop {
            let col = name(tokens.get(i)?)?;
            if col == "FROM" || col == "DISTINCT" {
                return None;
            }
            list.as_mut()?.push(col);
            i += 1;
            match tokens.get(i)? {
                Token::Punct(',') => i += 1,
                Token::Word(w) if w == "FROM" => break,
                _ => return None,
            }
        }
    }
    if !is_word(tokens.get(i), "FROM") {
        return None;
    }
    i += 1;
    let first = name(tokens.get(i)?)?;
    i += 1;
    let (schema, table) = if tokens.get(i) == Some(&Token::Punct('.')) {
        let t = name(tokens.get(i + 1)?)?;
        i += 2;
        (first, t)
    } else {
        (current_schema.to_string(), first)
    };
    // Optional alias, then only WHERE / ORDER BY / FETCH / OFFSET / FOR may follow.
    if let Some(Token::Word(w)) = tokens.get(i)
        && !matches!(w.as_str(), "WHERE" | "ORDER" | "FETCH" | "OFFSET" | "FOR")
    {
        i += 1;
    } else if let Some(Token::Quoted(_)) = tokens.get(i) {
        i += 1;
    }
    match tokens.get(i) {
        None => {}
        Some(Token::Word(w)) if matches!(w.as_str(), "WHERE" | "ORDER" | "FETCH" | "OFFSET" | "FOR") => {
            // A join or subquery hidden in the tail would mean other tables.
            let rest = &tokens[i..];
            if rest.iter().any(|t| matches!(t, Token::Word(w) if w == "JOIN" || w == "SELECT")) {
                return None;
            }
        }
        _ => return None,
    }
    let names: Vec<String> = match list {
        None => columns.to_vec(),
        Some(cols) if cols.len() == columns.len() => cols,
        Some(_) => return None,
    };
    Some(
        names
            .into_iter()
            .map(|column| Some(ColumnOrigin { schema: schema.clone(), table: table.clone(), column }))
            .collect(),
    )
}
