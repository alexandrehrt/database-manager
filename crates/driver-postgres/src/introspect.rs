//! Catalog queries against `pg_catalog`.

use dbm_core::{
    ColumnInfo, DbError, DbResult, Dialect, ForeignKey, IncomingKey, IndexInfo, Relation, RelationKind, TableDetails,
};
use tokio_postgres::Client;

use crate::pg_err;

pub async fn schemas(client: &Client) -> DbResult<Vec<String>> {
    let rows = client
        .query(
            "SELECT nspname::text FROM pg_namespace \
             WHERE nspname NOT IN ('pg_catalog', 'information_schema', 'pg_toast') \
               AND nspname NOT LIKE 'pg\\_temp\\_%' AND nspname NOT LIKE 'pg\\_toast\\_temp\\_%' \
             ORDER BY nspname = 'public' DESC, nspname",
            &[],
        )
        .await
        .map_err(pg_err)?;
    Ok(rows.iter().map(|r| r.get(0)).collect())
}

fn relation_kind(relkind: i8) -> RelationKind {
    match relkind as u8 {
        b'v' => RelationKind::View,
        b'm' => RelationKind::MaterializedView,
        _ => RelationKind::Table,
    }
}

pub async fn relations(client: &Client, schema: &str) -> DbResult<Vec<Relation>> {
    let rows = client
        .query(
            "SELECT c.relname::text, c.relkind FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relkind IN ('r', 'p', 'v', 'm', 'f') ORDER BY c.relname",
            &[&schema],
        )
        .await
        .map_err(pg_err)?;
    Ok(rows.iter().map(|r| Relation { name: r.get(0), kind: relation_kind(r.get(1)) }).collect())
}

async fn relation_oid(client: &Client, schema: &str, name: &str) -> DbResult<(u32, RelationKind)> {
    let row = client
        .query_opt(
            "SELECT c.oid, c.relkind FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relname = $2",
            &[&schema, &name],
        )
        .await
        .map_err(pg_err)?
        .ok_or_else(|| DbError::new(format!("relation {schema}.{name} does not exist")))?;
    Ok((row.get(0), relation_kind(row.get(1))))
}

pub async fn table_details(client: &Client, schema: &str, name: &str) -> DbResult<TableDetails> {
    let (oid, kind) = relation_oid(client, schema, name).await?;

    let columns = client
        .query(
            "SELECT a.attname::text, format_type(a.atttypid, a.atttypmod), NOT a.attnotnull, \
                    pg_get_expr(d.adbin, d.adrelid), array_position(pk.conkey, a.attnum) \
             FROM pg_attribute a \
             LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
             LEFT JOIN pg_constraint pk ON pk.conrelid = a.attrelid AND pk.contype = 'p' \
             WHERE a.attrelid = $1 AND a.attnum > 0 AND NOT a.attisdropped \
             ORDER BY a.attnum",
            &[&oid],
        )
        .await
        .map_err(pg_err)?
        .iter()
        .map(|r| ColumnInfo {
            name: r.get(0),
            data_type: r.get(1),
            nullable: r.get(2),
            default: r.get(3),
            pk_position: r.get::<_, Option<i32>>(4).map(|p| p as u32),
        })
        .collect();

    let indexes = client
        .query(
            "SELECT i.relname::text, ix.indisunique, ix.indisprimary, pg_get_indexdef(ix.indexrelid), \
                    ARRAY(SELECT pg_get_indexdef(ix.indexrelid, k, true) \
                          FROM generate_series(1, ix.indnkeyatts) k ORDER BY k) \
             FROM pg_index ix JOIN pg_class i ON i.oid = ix.indexrelid \
             WHERE ix.indrelid = $1 ORDER BY ix.indisprimary DESC, i.relname",
            &[&oid],
        )
        .await
        .map_err(pg_err)?
        .iter()
        .map(|r| IndexInfo {
            name: r.get(0),
            unique: r.get(1),
            primary: r.get(2),
            definition: Some(r.get(3)),
            columns: r.get(4),
        })
        .collect();

    let foreign_keys = client
        .query(
            "SELECT con.conname::text, \
                    ARRAY(SELECT a.attname::text FROM unnest(con.conkey) WITH ORDINALITY k(n, ord) \
                          JOIN pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = k.n ORDER BY k.ord), \
                    rn.nspname::text, rc.relname::text, \
                    ARRAY(SELECT a.attname::text FROM unnest(con.confkey) WITH ORDINALITY k(n, ord) \
                          JOIN pg_attribute a ON a.attrelid = con.confrelid AND a.attnum = k.n ORDER BY k.ord), \
                    ARRAY(SELECT format_type(a.atttypid, a.atttypmod) FROM unnest(con.confkey) WITH ORDINALITY k(n, ord) \
                          JOIN pg_attribute a ON a.attrelid = con.confrelid AND a.attnum = k.n ORDER BY k.ord) \
             FROM pg_constraint con \
             JOIN pg_class rc ON rc.oid = con.confrelid \
             JOIN pg_namespace rn ON rn.oid = rc.relnamespace \
             WHERE con.contype = 'f' AND con.conrelid = $1 ORDER BY con.conname",
            &[&oid],
        )
        .await
        .map_err(pg_err)?
        .iter()
        .map(|r| ForeignKey {
            name: r.get(0),
            columns: r.get(1),
            ref_schema: r.get(2),
            ref_table: r.get(3),
            ref_columns: r.get(4),
            ref_column_types: r.get(5),
        })
        .collect();

    Ok(TableDetails { schema: schema.to_string(), name: name.to_string(), kind, columns, indexes, foreign_keys })
}

pub async fn referencing_keys(client: &Client, schema: &str, name: &str) -> DbResult<Vec<IncomingKey>> {
    let (oid, _) = relation_oid(client, schema, name).await?;
    let rows = client
        .query(
            "SELECT rn.nspname::text, rc.relname::text, con.conname::text, \
                    ARRAY(SELECT a.attname::text FROM unnest(con.conkey) WITH ORDINALITY k(n, ord) \
                          JOIN pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = k.n ORDER BY k.ord), \
                    ARRAY(SELECT a.attname::text FROM unnest(con.confkey) WITH ORDINALITY k(n, ord) \
                          JOIN pg_attribute a ON a.attrelid = con.confrelid AND a.attnum = k.n ORDER BY k.ord), \
                    ARRAY(SELECT format_type(a.atttypid, a.atttypmod) FROM unnest(con.conkey) WITH ORDINALITY k(n, ord) \
                          JOIN pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = k.n ORDER BY k.ord) \
             FROM pg_constraint con \
             JOIN pg_class rc ON rc.oid = con.conrelid \
             JOIN pg_namespace rn ON rn.oid = rc.relnamespace \
             WHERE con.contype = 'f' AND con.confrelid = $1 \
             ORDER BY rn.nspname, rc.relname, con.conname",
            &[&oid],
        )
        .await
        .map_err(pg_err)?;
    Ok(rows
        .iter()
        .map(|r| IncomingKey {
            schema: r.get(0),
            table: r.get(1),
            foreign_key: ForeignKey {
                name: r.get(2),
                columns: r.get(3),
                ref_schema: schema.to_string(),
                ref_table: name.to_string(),
                ref_columns: r.get(4),
                ref_column_types: Vec::new(),
            },
            column_types: r.get(5),
        })
        .collect())
}

pub async fn ddl(client: &Client, schema: &str, name: &str) -> DbResult<String> {
    let (oid, kind) = relation_oid(client, schema, name).await?;
    let qualified = Dialect::Postgres.qualified(schema, name);
    match kind {
        RelationKind::View | RelationKind::MaterializedView => {
            let def: String =
                client.query_one("SELECT pg_get_viewdef($1::oid, true)", &[&oid]).await.map_err(pg_err)?.get(0);
            let keyword = if kind == RelationKind::View { "VIEW" } else { "MATERIALIZED VIEW" };
            Ok(format!("CREATE {keyword} {qualified} AS\n{}", def.trim_end()))
        }
        RelationKind::Table => {
            let details = table_details(client, schema, name).await?;
            Ok(dbm_core::ddl::create_table(Dialect::Postgres, &details))
        }
    }
}
