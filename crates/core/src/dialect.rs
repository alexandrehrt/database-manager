use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Dialect {
    Postgres,
    Sqlite,
}

impl Dialect {
    /// Double-quoted identifier; both engines accept this form.
    pub fn quote_ident(self, ident: &str) -> String {
        format!("\"{}\"", ident.replace('"', "\"\""))
    }

    pub fn qualified(self, schema: &str, name: &str) -> String {
        format!("{}.{}", self.quote_ident(schema), self.quote_ident(name))
    }

    /// Placeholder for the `n`th (1-based) bound parameter.
    pub fn placeholder(self, n: usize) -> String {
        match self {
            Dialect::Postgres => format!("${n}"),
            Dialect::Sqlite => format!("?{n}"),
        }
    }

    pub fn select_all(self, schema: &str, name: &str) -> String {
        format!("SELECT * FROM {}", self.qualified(schema, name))
    }
}
