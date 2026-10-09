//! Oracle implementation of [`dbm_core::Connection`] on the `oracle` crate
//! (ODPI-C), which loads Oracle Instant Client at runtime.
//!
//! The connection runs with autocommit off: in auto mode each writing
//! statement is committed right after it, and `BEGIN` (which Oracle doesn't
//! have) holds commits until `COMMIT` or `ROLLBACK`, so the app's
//! transaction mode works as with the other engines.
//!
//! Oracle doesn't report which table a result column came from, so origins
//! are inferred for simple single-table queries (see [`origins`]).

mod origins;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use dbm_core::{
    Canceller, Column, ColumnInfo, Connection, DbError, DbResult, Dialect, ExecOutcome, ForeignKey, IncomingKey,
    IndexInfo, Relation, RelationKind, ResultSet, TableDetails, Value,
};
use oracle::sql_type::{OracleType, Timestamp, ToSql};

pub struct OracleParams<'a> {
    pub host: &'a str,
    pub port: u16,
    pub service: &'a str,
    pub user: &'a str,
    pub password: Option<&'a str>,
    pub client_dir: Option<&'a Path>,
}

pub struct OracleConnection {
    conn: Arc<oracle::Connection>,
    /// Serialises statements; holds whether the user opened a transaction.
    state: Arc<Mutex<bool>>,
    /// The session's current schema, for unqualified table names.
    schema: String,
}

const INSTANT_CLIENT_HELP: &str = "Oracle Instant Client is needed to connect to Oracle. Install the Basic package \
     from https://www.oracle.com/database/technologies/instant-client/downloads.html and set its folder in this \
     connection's settings (or put it on the system library path).";

/// Instant Client is loaded once per process; the first directory wins.
fn init_client(dir: Option<&Path>) -> DbResult<()> {
    static INIT: OnceLock<Result<Option<PathBuf>, String>> = OnceLock::new();
    let result = INIT.get_or_init(|| {
        let mut params = oracle::InitParams::new();
        if let Some(d) = dir {
            params.oracle_client_lib_dir(d).map_err(|e| e.to_string())?;
        }
        params.init().map_err(|e| e.to_string())?;
        Ok(dir.map(Path::to_path_buf))
    });
    match result {
        Ok(_) => Ok(()),
        Err(e) => Err(DbError { message: INSTANT_CLIENT_HELP.into(), detail: Some(e.clone()), code: None }),
    }
}

fn ora_err(e: oracle::Error) -> DbError {
    match e.db_error() {
        // Oracle 23 appends a "Help: <url>" line; keep it out of the headline.
        Some(db) => {
            let text = db.message().trim_end();
            let (message, detail) = match text.split_once('\n') {
                Some((first, rest)) => (first.to_string(), Some(rest.trim().to_string())),
                None => (text.to_string(), None),
            };
            DbError { message, detail, code: Some(format!("ORA-{:05}", db.code())) }
        }
        None => DbError::new(e.to_string()),
    }
}

const SESSION_SETUP: &[&str] = &[
    "ALTER SESSION SET NLS_DATE_FORMAT = 'YYYY-MM-DD HH24:MI:SS'",
    "ALTER SESSION SET NLS_TIMESTAMP_FORMAT = 'YYYY-MM-DD HH24:MI:SS.FF'",
    "ALTER SESSION SET NLS_TIMESTAMP_TZ_FORMAT = 'YYYY-MM-DD HH24:MI:SS.FF TZH:TZM'",
    "ALTER SESSION SET NLS_NUMERIC_CHARACTERS = '.,'",
    "BEGIN \
       DBMS_METADATA.SET_TRANSFORM_PARAM(DBMS_METADATA.SESSION_TRANSFORM, 'SEGMENT_ATTRIBUTES', FALSE); \
       DBMS_METADATA.SET_TRANSFORM_PARAM(DBMS_METADATA.SESSION_TRANSFORM, 'SQLTERMINATOR', TRUE); \
     END;",
];

pub async fn connect(p: OracleParams<'_>) -> DbResult<OracleConnection> {
    let (user, password) = (p.user.to_string(), p.password.unwrap_or_default().to_string());
    let connect_string = format!("//{}:{}/{}", p.host, p.port, p.service);
    let client_dir = p.client_dir.map(Path::to_path_buf);
    tokio::task::spawn_blocking(move || {
        init_client(client_dir.as_deref())?;
        let conn = oracle::Connection::connect(&user, &password, &connect_string).map_err(ora_err)?;
        for sql in SESSION_SETUP {
            conn.execute(sql, &[]).map_err(ora_err)?;
        }
        let schema: String =
            conn.query_row_as("SELECT SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA') FROM dual", &[]).map_err(ora_err)?;
        Ok(OracleConnection { conn: Arc::new(conn), state: Arc::new(Mutex::new(false)), schema })
    })
    .await
    .map_err(|e| DbError::new(e.to_string()))?
}

fn to_sql(v: &Value) -> Box<dyn ToSql + Send> {
    match v {
        Value::Null => Box::new(None::<String>),
        Value::Bool(b) => Box::new(*b),
        Value::Int(i) => Box::new(*i),
        Value::Float(f) => Box::new(*f),
        Value::Bytes(b) => Box::new(b.clone()),
        other => Box::new(other.to_string()),
    }
}

fn format_timestamp(ts: &Timestamp, with_fraction: bool, with_tz: bool) -> String {
    let mut s = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        ts.year(),
        ts.month(),
        ts.day(),
        ts.hour(),
        ts.minute(),
        ts.second()
    );
    if with_fraction && ts.nanosecond() > 0 {
        let frac = format!("{:09}", ts.nanosecond());
        s.push('.');
        s.push_str(frac.trim_end_matches('0'));
    }
    if with_tz {
        let (h, m) = (ts.tz_hour_offset(), ts.tz_minute_offset());
        let sign = if h < 0 || m < 0 { '-' } else { '+' };
        s.push_str(&format!("{sign}{:02}:{:02}", h.abs(), m.abs()));
    }
    s
}

/// A NUMBER as an exact integer when it is one, else its exact text.
fn number(text: String) -> Value {
    if !text.contains(['.', 'e', 'E'])
        && let Ok(i) = text.parse::<i64>()
    {
        return Value::Int(i);
    }
    Value::Numeric(text)
}

fn decode(row: &oracle::Row, i: usize, ty: &OracleType) -> Value {
    let text = |row: &oracle::Row| row.get::<_, Option<String>>(i).map(|v| v.map_or(Value::Null, Value::Text));
    let result = match ty {
        OracleType::Number(_, _) | OracleType::Int64 => {
            row.get::<_, Option<String>>(i).map(|v| v.map_or(Value::Null, number))
        }
        OracleType::BinaryFloat | OracleType::BinaryDouble | OracleType::Float(_) => {
            row.get::<_, Option<f64>>(i).map(|v| v.map_or(Value::Null, Value::Float))
        }
        OracleType::Date => row
            .get::<_, Option<Timestamp>>(i)
            .map(|v| v.map_or(Value::Null, |t| Value::Text(format_timestamp(&t, false, false)))),
        OracleType::Timestamp(_) => row
            .get::<_, Option<Timestamp>>(i)
            .map(|v| v.map_or(Value::Null, |t| Value::Text(format_timestamp(&t, true, false)))),
        OracleType::TimestampTZ(_) | OracleType::TimestampLTZ(_) => row
            .get::<_, Option<Timestamp>>(i)
            .map(|v| v.map_or(Value::Null, |t| Value::Text(format_timestamp(&t, true, true)))),
        OracleType::Raw(_) | OracleType::BLOB | OracleType::LongRaw => {
            row.get::<_, Option<Vec<u8>>>(i).map(|v| v.map_or(Value::Null, Value::Bytes))
        }
        OracleType::Boolean => row.get::<_, Option<bool>>(i).map(|v| v.map_or(Value::Null, Value::Bool)),
        _ => text(row),
    };
    result.unwrap_or_else(|e| Value::Text(format!("<{ty}: {e}>")))
}

/// What a statement does to the user's transaction.
enum TxControl {
    Begin,
    Commit,
    Rollback,
}

fn tx_control(sql: &str) -> Option<TxControl> {
    let words: Vec<String> =
        sql.split_whitespace().take(3).map(|w| w.trim_end_matches(';').to_ascii_uppercase()).collect();
    match words.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["BEGIN"] | ["BEGIN", "TRANSACTION"] | ["START", "TRANSACTION"] => Some(TxControl::Begin),
        ["COMMIT"] | ["COMMIT", "WORK"] => Some(TxControl::Commit),
        // ROLLBACK TO SAVEPOINT keeps the transaction open.
        ["ROLLBACK"] | ["ROLLBACK", "WORK"] => Some(TxControl::Rollback),
        _ => None,
    }
}

fn run(
    conn: &oracle::Connection,
    in_transaction: &mut bool,
    current_schema: &str,
    sql: &str,
    params: &[Value],
    max_rows: Option<usize>,
) -> DbResult<ExecOutcome> {
    match tx_control(sql) {
        Some(TxControl::Begin) => {
            *in_transaction = true;
            return Ok(ExecOutcome::Affected(0));
        }
        Some(TxControl::Commit) => {
            conn.commit().map_err(ora_err)?;
            *in_transaction = false;
            return Ok(ExecOutcome::Affected(0));
        }
        Some(TxControl::Rollback) => {
            conn.rollback().map_err(ora_err)?;
            *in_transaction = false;
            return Ok(ExecOutcome::Affected(0));
        }
        None => {}
    }
    let bound: Vec<Box<dyn ToSql + Send>> = params.iter().map(to_sql).collect();
    let refs: Vec<&dyn ToSql> = bound.iter().map(|b| b.as_ref() as &dyn ToSql).collect();
    let mut stmt = conn.statement(sql).build().map_err(ora_err)?;
    if stmt.is_query() {
        let rows = stmt.query(&refs).map_err(ora_err)?;
        let info = rows.column_info().to_vec();
        let origins =
            origins::infer(sql, current_schema, &info.iter().map(|c| c.name().to_string()).collect::<Vec<_>>());
        let columns: Vec<Column> = info
            .iter()
            .zip(origins)
            .map(|(c, origin)| Column { name: c.name().to_string(), type_name: c.oracle_type().to_string(), origin })
            .collect();
        let mut out = Vec::new();
        let mut truncated = false;
        for row in rows {
            let row = row.map_err(ora_err)?;
            if max_rows.is_some_and(|max| out.len() >= max) {
                truncated = true;
                break;
            }
            out.push(info.iter().enumerate().map(|(i, c)| decode(&row, i, c.oracle_type())).collect());
        }
        return Ok(ExecOutcome::Rows(ResultSet { columns, rows: out, truncated }));
    }
    stmt.execute(&refs).map_err(ora_err)?;
    let affected = if stmt.is_dml() { stmt.row_count().map_err(ora_err)? } else { 0 };
    // Auto mode: each writing statement commits on its own.
    if !*in_transaction && (stmt.is_dml() || stmt.is_plsql()) {
        conn.commit().map_err(ora_err)?;
    }
    Ok(ExecOutcome::Affected(affected))
}

impl OracleConnection {
    async fn blocking<T, F>(&self, f: F) -> DbResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&oracle::Connection, &mut bool, &str) -> DbResult<T> + Send + 'static,
    {
        let (conn, state, schema) = (self.conn.clone(), self.state.clone(), self.schema.clone());
        tokio::task::spawn_blocking(move || {
            let mut in_tx = state.lock().map_err(|_| DbError::new("Oracle connection poisoned by an earlier panic"))?;
            f(&conn, &mut in_tx, &schema)
        })
        .await
        .map_err(|e| DbError::new(e.to_string()))?
    }
}

/// Runs a catalog query with string parameters.
fn strings(conn: &oracle::Connection, sql: &str, params: &[&str]) -> DbResult<Vec<oracle::Row>> {
    let refs: Vec<&dyn ToSql> = params.iter().map(|p| p as &dyn ToSql).collect();
    let rows = conn.query(sql, &refs).map_err(ora_err)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(ora_err)
}

fn get<T: oracle::sql_type::FromSql>(row: &oracle::Row, i: usize) -> DbResult<T> {
    row.get(i).map_err(ora_err)
}

fn column_type(row: &oracle::Row) -> DbResult<String> {
    let data_type: String = get(row, 1)?;
    let precision: Option<i64> = get(row, 2)?;
    let scale: Option<i64> = get(row, 3)?;
    let length: Option<i64> = get(row, 4)?;
    Ok(match data_type.as_str() {
        "VARCHAR2" | "NVARCHAR2" | "CHAR" | "NCHAR" | "RAW" => format!("{data_type}({})", length.unwrap_or_default()),
        "NUMBER" => match (precision, scale) {
            (Some(p), Some(0)) => format!("NUMBER({p})"),
            (Some(p), Some(s)) => format!("NUMBER({p},{s})"),
            _ => "NUMBER".into(),
        },
        _ => data_type,
    })
}

fn table_details(conn: &oracle::Connection, schema: &str, name: &str) -> DbResult<TableDetails> {
    let kind = strings(
        conn,
        "SELECT CASE WHEN EXISTS (SELECT 1 FROM all_mviews WHERE owner = :1 AND mview_name = :2) THEN 'M' \
                     WHEN EXISTS (SELECT 1 FROM all_views WHERE owner = :1 AND view_name = :2) THEN 'V' \
                     ELSE 'T' END FROM dual",
        &[schema, name],
    )?
    .first()
    .map(|r| get::<String>(r, 0))
    .transpose()?
    .map_or(RelationKind::Table, |k| match k.as_str() {
        "M" => RelationKind::MaterializedView,
        "V" => RelationKind::View,
        _ => RelationKind::Table,
    });

    let pk: Vec<(String, i64)> = strings(
        conn,
        "SELECT cc.column_name, cc.position FROM all_constraints c \
         JOIN all_cons_columns cc ON cc.owner = c.owner AND cc.constraint_name = c.constraint_name \
         WHERE c.owner = :1 AND c.table_name = :2 AND c.constraint_type = 'P'",
        &[schema, name],
    )?
    .iter()
    .map(|r| Ok((get(r, 0)?, get(r, 1)?)))
    .collect::<DbResult<_>>()?;

    let columns = strings(
        conn,
        "SELECT column_name, data_type, data_precision, data_scale, char_length, nullable, data_default \
         FROM all_tab_columns WHERE owner = :1 AND table_name = :2 ORDER BY column_id",
        &[schema, name],
    )?
    .iter()
    .map(|r| {
        let name: String = get(r, 0)?;
        let default: Option<String> = get(r, 6)?;
        Ok(ColumnInfo {
            pk_position: pk.iter().find(|(c, _)| *c == name).map(|(_, p)| *p as u32),
            data_type: column_type(r)?,
            nullable: get::<String>(r, 5)? == "Y",
            default: default.map(|d| d.trim().to_string()).filter(|d| !d.is_empty()),
            name,
        })
    })
    .collect::<DbResult<Vec<_>>>()?;

    let pk_index: Option<String> = strings(
        conn,
        "SELECT index_name FROM all_constraints WHERE owner = :1 AND table_name = :2 AND constraint_type = 'P'",
        &[schema, name],
    )?
    .first()
    .map(|r| get::<Option<String>>(r, 0))
    .transpose()?
    .flatten();
    let mut indexes: Vec<IndexInfo> = Vec::new();
    for r in strings(
        conn,
        "SELECT i.index_name, i.uniqueness, ic.column_name FROM all_indexes i \
         JOIN all_ind_columns ic ON ic.index_owner = i.owner AND ic.index_name = i.index_name \
         WHERE i.table_owner = :1 AND i.table_name = :2 ORDER BY i.index_name, ic.column_position",
        &[schema, name],
    )? {
        let index: String = get(&r, 0)?;
        let column: String = get(&r, 2)?;
        match indexes.last_mut() {
            Some(last) if last.name == index => last.columns.push(column),
            _ => indexes.push(IndexInfo {
                primary: pk_index.as_deref() == Some(index.as_str()),
                unique: get::<String>(&r, 1)? == "UNIQUE",
                name: index,
                columns: vec![column],
                definition: None,
            }),
        }
    }

    Ok(TableDetails {
        schema: schema.to_string(),
        name: name.to_string(),
        kind,
        columns,
        indexes,
        foreign_keys: foreign_keys(conn, "c.owner = :1 AND c.table_name = :2", schema, name)?
            .into_iter()
            .map(|(_, _, fk)| fk)
            .collect(),
    })
}

/// Foreign keys matching `filter` (over the referencing constraint `c` and
/// the referenced table `r`), as (referencing owner, referencing table, key).
fn foreign_keys(
    conn: &oracle::Connection,
    filter: &str,
    schema: &str,
    name: &str,
) -> DbResult<Vec<(String, String, ForeignKey)>> {
    let sql = format!(
        "SELECT c.owner, c.table_name, c.constraint_name, cc.column_name, r.owner, r.table_name, rc.column_name \
         FROM all_constraints c \
         JOIN all_cons_columns cc ON cc.owner = c.owner AND cc.constraint_name = c.constraint_name \
         JOIN all_constraints r ON r.owner = c.r_owner AND r.constraint_name = c.r_constraint_name \
         JOIN all_cons_columns rc ON rc.owner = r.owner AND rc.constraint_name = r.constraint_name \
              AND rc.position = cc.position \
         WHERE c.constraint_type = 'R' AND {filter} \
         ORDER BY c.owner, c.table_name, c.constraint_name, cc.position"
    );
    let mut out: Vec<(String, String, ForeignKey)> = Vec::new();
    for r in strings(conn, &sql, &[schema, name])? {
        let (owner, table, constraint): (String, String, String) = (get(&r, 0)?, get(&r, 1)?, get(&r, 2)?);
        let (column, ref_column): (String, String) = (get(&r, 3)?, get(&r, 6)?);
        match out.last_mut() {
            Some((o, t, fk)) if *o == owner && *t == table && fk.name == constraint => {
                fk.columns.push(column);
                fk.ref_columns.push(ref_column);
            }
            _ => out.push((
                owner,
                table,
                ForeignKey {
                    name: constraint,
                    columns: vec![column],
                    ref_schema: get(&r, 4)?,
                    ref_table: get(&r, 5)?,
                    ref_columns: vec![ref_column],
                    ref_column_types: Vec::new(),
                },
            )),
        }
    }
    Ok(out)
}

#[async_trait]
impl Connection for OracleConnection {
    fn dialect(&self) -> Dialect {
        Dialect::Oracle
    }

    async fn execute(&self, sql: &str, params: &[Value], max_rows: Option<usize>) -> DbResult<ExecOutcome> {
        let (sql, params) = (sql.to_string(), params.to_vec());
        self.blocking(move |c, in_tx, schema| run(c, in_tx, schema, &sql, &params, max_rows)).await
    }

    async fn schemas(&self) -> DbResult<Vec<String>> {
        self.blocking(|c, _, schema| {
            strings(
                c,
                "SELECT username FROM all_users WHERE oracle_maintained = 'N' OR username = :1 \
                 ORDER BY CASE WHEN username = :1 THEN 0 ELSE 1 END, username",
                &[schema],
            )?
            .iter()
            .map(|r| get(r, 0))
            .collect()
        })
        .await
    }

    async fn relations(&self, schema: &str) -> DbResult<Vec<Relation>> {
        let schema = schema.to_string();
        self.blocking(move |c, _, _| {
            strings(
                c,
                "SELECT table_name, 'T' FROM all_tables WHERE owner = :1 AND nested = 'NO' AND secondary = 'N' \
                   AND table_name NOT IN (SELECT mview_name FROM all_mviews WHERE owner = :1) \
                 UNION ALL SELECT view_name, 'V' FROM all_views WHERE owner = :1 \
                 UNION ALL SELECT mview_name, 'M' FROM all_mviews WHERE owner = :1 \
                 ORDER BY 1",
                &[&schema],
            )?
            .iter()
            .map(|r| {
                let kind: String = get(r, 1)?;
                Ok(Relation {
                    name: get(r, 0)?,
                    kind: match kind.as_str() {
                        "V" => RelationKind::View,
                        "M" => RelationKind::MaterializedView,
                        _ => RelationKind::Table,
                    },
                })
            })
            .collect()
        })
        .await
    }

    async fn table_details(&self, schema: &str, name: &str) -> DbResult<TableDetails> {
        let (schema, name) = (schema.to_string(), name.to_string());
        self.blocking(move |c, _, _| table_details(c, &schema, &name)).await
    }

    async fn referencing_keys(&self, schema: &str, name: &str) -> DbResult<Vec<IncomingKey>> {
        let (schema, name) = (schema.to_string(), name.to_string());
        self.blocking(move |c, _, _| {
            Ok(foreign_keys(c, "r.owner = :1 AND r.table_name = :2", &schema, &name)?
                .into_iter()
                .map(|(owner, table, foreign_key)| IncomingKey {
                    schema: owner,
                    table,
                    foreign_key,
                    column_types: Vec::new(),
                })
                .collect())
        })
        .await
    }

    async fn ddl(&self, schema: &str, name: &str) -> DbResult<String> {
        let (schema, name) = (schema.to_string(), name.to_string());
        self.blocking(move |c, _, _| {
            let kind = table_details(c, &schema, &name)?.kind;
            let object_type = match kind {
                RelationKind::Table => "TABLE",
                RelationKind::View => "VIEW",
                RelationKind::MaterializedView => "MATERIALIZED_VIEW",
            };
            let mut ddl: String = c
                .query_row_as("SELECT DBMS_METADATA.GET_DDL(:1, :2, :3) FROM dual", &[&object_type, &name, &schema])
                .map_err(ora_err)?;
            // Indexes come separately; a table without any raises ORA-31608.
            if kind == RelationKind::Table
                && let Ok(indexes) = c.query_row_as::<String>(
                    "SELECT DBMS_METADATA.GET_DEPENDENT_DDL('INDEX', :1, :2) FROM dual",
                    &[&name, &schema],
                )
            {
                ddl.push('\n');
                ddl.push_str(&indexes);
            }
            Ok(ddl.trim().to_string())
        })
        .await
    }

    fn canceller(&self) -> Arc<dyn Canceller> {
        Arc::new(OracleCanceller(self.conn.clone()))
    }

    async fn in_transaction(&self) -> bool {
        self.blocking(|_, in_tx, _| Ok(*in_tx)).await.unwrap_or(false)
    }
}

struct OracleCanceller(Arc<oracle::Connection>);

#[async_trait]
impl Canceller for OracleCanceller {
    async fn cancel(&self) -> DbResult<()> {
        let conn = self.0.clone();
        tokio::task::spawn_blocking(move || conn.break_execution().map_err(ora_err))
            .await
            .map_err(|e| DbError::new(e.to_string()))?
    }
}
