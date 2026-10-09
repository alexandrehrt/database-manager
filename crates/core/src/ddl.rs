use crate::{Dialect, TableDetails};

/// CREATE TABLE plus CREATE INDEX statements rebuilt from introspected
/// metadata, for engines that do not keep the original DDL text.
pub fn create_table(dialect: Dialect, t: &TableDetails) -> String {
    let q = |s: &str| dialect.quote_ident(s);
    let mut lines: Vec<String> = t
        .columns
        .iter()
        .map(|c| {
            let mut line = format!("    {} {}", q(&c.name), c.data_type);
            if let Some(default) = &c.default {
                line.push_str(&format!(" DEFAULT {default}"));
            }
            if !c.nullable {
                line.push_str(" NOT NULL");
            }
            line
        })
        .collect();

    let mut pk: Vec<_> = t.columns.iter().filter_map(|c| c.pk_position.map(|p| (p, &c.name))).collect();
    pk.sort();
    if !pk.is_empty() {
        let name = t.indexes.iter().find(|i| i.primary).map(|i| format!("CONSTRAINT {} ", q(&i.name)));
        let cols: Vec<_> = pk.iter().map(|(_, c)| q(c)).collect();
        lines.push(format!("    {}PRIMARY KEY ({})", name.unwrap_or_default(), cols.join(", ")));
    }
    for fk in &t.foreign_keys {
        let cols: Vec<_> = fk.columns.iter().map(|c| q(c)).collect();
        let ref_cols: Vec<_> = fk.ref_columns.iter().map(|c| q(c)).collect();
        lines.push(format!(
            "    CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({})",
            q(&fk.name),
            cols.join(", "),
            dialect.qualified(&fk.ref_schema, &fk.ref_table),
            ref_cols.join(", ")
        ));
    }

    let mut out = format!("CREATE TABLE {} (\n{}\n);\n", dialect.qualified(&t.schema, &t.name), lines.join(",\n"));
    for idx in t.indexes.iter().filter(|i| !i.primary) {
        let stmt = idx.definition.clone().unwrap_or_else(|| {
            let cols: Vec<_> = idx.columns.iter().map(|c| q(c)).collect();
            format!(
                "CREATE {}INDEX {} ON {} ({})",
                if idx.unique { "UNIQUE " } else { "" },
                q(&idx.name),
                dialect.qualified(&t.schema, &t.name),
                cols.join(", ")
            )
        });
        out.push_str(&format!("\n{stmt};"));
    }
    out
}
