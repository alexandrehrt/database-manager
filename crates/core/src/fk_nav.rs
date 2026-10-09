//! Foreign-key navigation: deciding which result cells are links and building
//! the query that fetches the referenced row.

use std::collections::HashMap;

use crate::{Column, ColumnOrigin, Dialect, ForeignKey, Value};

/// Foreign keys of the tables a result set reads from, keyed by `(schema, table)`.
pub type ForeignKeyIndex = HashMap<(String, String), Vec<ForeignKey>>;

/// A navigable link from one result cell.
#[derive(Debug, Clone, PartialEq)]
pub struct FkLink {
    pub foreign_key: ForeignKey,
    /// Values of the FK columns, in `foreign_key.columns` order.
    pub values: Vec<Value>,
}

/// Returns the link for cell `col` of `row`, if its column is part of a
/// foreign key and every column of that key is present in the row and non-NULL.
pub fn link_for_cell(columns: &[Column], row: &[Value], col: usize, fks: &ForeignKeyIndex) -> Option<FkLink> {
    let origin = columns.get(col)?.origin.as_ref()?;
    let candidates = fks.get(&(origin.schema.clone(), origin.table.clone()))?;
    candidates.iter().filter(|fk| fk.columns.contains(&origin.column)).find_map(|fk| {
        let values = fk
            .columns
            .iter()
            .map(|fk_col| {
                let wanted =
                    ColumnOrigin { schema: origin.schema.clone(), table: origin.table.clone(), column: fk_col.clone() };
                let idx = columns.iter().position(|c| c.origin.as_ref() == Some(&wanted))?;
                row.get(idx).filter(|v| !v.is_null()).cloned()
            })
            .collect::<Option<Vec<_>>>()?;
        Some(FkLink { foreign_key: fk.clone(), values })
    })
}

/// Parameterised `SELECT * FROM <referenced table> WHERE <ref cols> = <values>`.
/// Postgres parameters are bound as text and cast to the referenced column's
/// type so the comparison can use its index.
pub fn navigation_query(dialect: Dialect, link: &FkLink) -> (String, Vec<Value>) {
    let fk = &link.foreign_key;
    let conditions: Vec<String> = fk
        .ref_columns
        .iter()
        .enumerate()
        .map(|(i, col)| {
            let param = dialect.placeholder(i + 1);
            let rhs = match (dialect, fk.ref_column_types.get(i)) {
                (Dialect::Postgres, Some(ty)) => format!("CAST({param}::text AS {ty})"),
                _ => param,
            };
            format!("{} = {rhs}", dialect.quote_ident(col))
        })
        .collect();
    let sql = format!(
        "SELECT * FROM {} WHERE {}",
        dialect.qualified(&fk.ref_schema, &fk.ref_table),
        conditions.join(" AND ")
    );
    (sql, link.values.clone())
}
