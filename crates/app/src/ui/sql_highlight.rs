//! Lightweight SQL highlighting for the console editor: keywords, strings,
//! quoted identifiers, numbers and comments.

use eframe::egui::{self, Color32, FontId, TextFormat, text::LayoutJob};

pub const KEYWORDS: &[&str] = &[
    "ADD",
    "ALL",
    "ALTER",
    "AND",
    "ANY",
    "AS",
    "ASC",
    "BEGIN",
    "BETWEEN",
    "BY",
    "CASCADE",
    "CASE",
    "CAST",
    "CHECK",
    "COLUMN",
    "COMMIT",
    "CONSTRAINT",
    "CREATE",
    "CROSS",
    "CURRENT_DATE",
    "CURRENT_TIMESTAMP",
    "DATABASE",
    "DEFAULT",
    "DELETE",
    "DESC",
    "DISTINCT",
    "DO",
    "DROP",
    "ELSE",
    "END",
    "EXCEPT",
    "EXISTS",
    "EXPLAIN",
    "FALSE",
    "FETCH",
    "FILTER",
    "FOR",
    "FOREIGN",
    "FROM",
    "FULL",
    "FUNCTION",
    "GRANT",
    "GROUP",
    "HAVING",
    "IF",
    "ILIKE",
    "IN",
    "INDEX",
    "INNER",
    "INSERT",
    "INTERSECT",
    "INTO",
    "IS",
    "JOIN",
    "KEY",
    "LATERAL",
    "LEFT",
    "LIKE",
    "LIMIT",
    "NATURAL",
    "NOT",
    "NULL",
    "OFFSET",
    "ON",
    "OR",
    "ORDER",
    "OUTER",
    "OVER",
    "PARTITION",
    "PRAGMA",
    "PRIMARY",
    "REFERENCES",
    "RETURNING",
    "REVOKE",
    "RIGHT",
    "ROLLBACK",
    "SCHEMA",
    "SELECT",
    "SET",
    "SHOW",
    "TABLE",
    "THEN",
    "TO",
    "TRANSACTION",
    "TRIGGER",
    "TRUE",
    "TRUNCATE",
    "UNION",
    "UNIQUE",
    "UPDATE",
    "USING",
    "VALUES",
    "VIEW",
    "WHEN",
    "WHERE",
    "WINDOW",
    "WITH",
];

struct Palette {
    plain: Color32,
    keyword: Color32,
    string: Color32,
    number: Color32,
    comment: Color32,
}

impl Palette {
    fn for_visuals(v: &egui::Visuals) -> Self {
        if v.dark_mode {
            Palette {
                plain: v.text_color(),
                keyword: Color32::from_rgb(204, 120, 50),
                string: Color32::from_rgb(106, 171, 115),
                number: Color32::from_rgb(104, 151, 187),
                comment: Color32::from_rgb(128, 128, 128),
            }
        } else {
            Palette {
                plain: v.text_color(),
                keyword: Color32::from_rgb(0, 51, 179),
                string: Color32::from_rgb(6, 125, 23),
                number: Color32::from_rgb(23, 80, 235),
                comment: Color32::from_rgb(140, 140, 140),
            }
        }
    }
}

pub fn layout(ui: &egui::Ui, text: &str, wrap_width: f32) -> LayoutJob {
    let palette = Palette::for_visuals(ui.visuals());
    let font = FontId::monospace(13.0);
    let mut job = LayoutJob::default();
    job.wrap.max_width = wrap_width;
    let mut push = |s: &str, color: Color32| {
        job.append(s, 0.0, TextFormat { font_id: font.clone(), color, ..Default::default() });
    };

    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        let b = bytes[i];
        let color = if b == b'-' && bytes.get(i + 1) == Some(&b'-') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            palette.comment
        } else if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            while i < bytes.len() && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/')) {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            palette.comment
        } else if b == b'\'' || b == b'"' {
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b && bytes.get(i + 1) == Some(&b) {
                    i += 2;
                } else if bytes[i] == b {
                    i += 1;
                    break;
                } else {
                    i += 1;
                }
            }
            if b == b'\'' { palette.string } else { palette.plain }
        } else if b.is_ascii_digit() {
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'.') {
                i += 1;
            }
            palette.number
        } else if b.is_ascii_alphabetic() || b == b'_' {
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'$') {
                i += 1;
            }
            let word = &text[start..i];
            if KEYWORDS.iter().any(|k| k.eq_ignore_ascii_case(word)) { palette.keyword } else { palette.plain }
        } else {
            // Advance one whole character so multi-byte UTF-8 stays intact.
            i += text[i..].chars().next().map_or(1, char::len_utf8);
            palette.plain
        };
        push(&text[start..i], color);
    }
    job
}
