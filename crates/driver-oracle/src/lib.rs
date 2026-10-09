//! Oracle implementation of [`dbm_core::Connection`] on Oracle's pure-Rust
//! thin driver (`oracledb`): no Oracle Client libraries are needed.
//!
//! The connection runs with autocommit off: in auto mode each writing
//! statement is committed right after it, and `BEGIN` (which Oracle doesn't
//! have) holds commits until `COMMIT` or `ROLLBACK`, so the app's
//! transaction mode works as with the other engines.
//!
//! The thin driver can't interrupt a running statement, so Cancel abandons
//! it: the caller gets "cancelled" at once and later calls use a fresh
//! session. The old session finishes the statement on the server and is
//! then closed, which rolls back its uncommitted work.
//!
//! Oracle doesn't report which table a result column came from, so origins
//! are inferred for simple single-table queries (see [`origins`]).

mod origins;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use dbm_core::{
    Canceller, Column, ColumnInfo, Connection, DbError, DbResult, Dialect, ExecOutcome, ForeignKey, IncomingKey,
    IndexInfo, Relation, RelationKind, ResultSet, TableDetails, Value,
};
use oracledb::{
    ErrorKind, JsonValue, OracleIntervalDS, OracleIntervalYM, OracleNumber, OracleTimestamp, Row, ToDbValue,
};
use tokio::sync::Notify;

pub struct OracleParams<'a> {
    pub host: &'a str,
    pub port: u16,
    pub service: &'a str,
    pub user: &'a str,
    pub password: Option<&'a str>,
}

/// What it takes to open another session, for Cancel.
struct Login {
    user: String,
    password: String,
    connect_string: String,
}

/// One database session; its mutex serialises statements and holds whether
/// the user opened a transaction.
struct Session {
    conn: oracledb::Connection,
    in_tx: Mutex<bool>,
}

pub struct OracleConnection {
    /// The live session; Cancel swaps in a new one.
    session: Arc<Mutex<Arc<Session>>>,
    /// Wakes calls waiting on an abandoned session.
    cancelled: Arc<Notify>,
    login: Arc<Login>,
    /// The session's current schema, for unqualified table names.
    schema: String,
}

fn ora_err(e: oracledb::Error) -> DbError {
    match e.kind() {
        // Oracle 23 appends a "Help: <url>" line; keep it out of the headline.
        ErrorKind::DbError(db) => {
            let text = db.message().trim_end();
            let (message, detail) = match text.split_once('\n') {
                Some((first, rest)) => (first.to_string(), Some(rest.trim().to_string())),
                None => (text.to_string(), None),
            };
            // The parse offset is in bytes; statements are nearly always ASCII, so it
            // stands for the character offset. 0 also means "no position".
            let position = (db.offset() > 0).then_some(db.offset());
            DbError { message, detail, code: Some(format!("ORA-{:05}", db.code())), position }
        }
        _ => DbError::new(e.to_string()),
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

/// Opens and prepares a session (blocking).
fn open_session(login: &Login) -> DbResult<Session> {
    let config = oracledb::Config::default()
        .set_credentials(&login.user, &login.password)
        .set_connect_string(&login.connect_string)
        .map_err(ora_err)?;
    let conn = oracledb::connect(config).map_err(ora_err)?;
    for sql in SESSION_SETUP {
        conn.execute(sql, &[]).map_err(ora_err)?;
    }
    Ok(Session { conn, in_tx: Mutex::new(false) })
}

pub async fn connect(p: OracleParams<'_>) -> DbResult<OracleConnection> {
    let login = Arc::new(Login {
        user: p.user.to_string(),
        password: p.password.unwrap_or_default().to_string(),
        connect_string: format!("{}:{}/{}", p.host, p.port, p.service),
    });
    let l = login.clone();
    tokio::task::spawn_blocking(move || {
        let session = open_session(&l)?;
        let schema: String = session
            .conn
            .query_row("SELECT SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA') FROM dual", &[])
            .and_then(|r| r.get(0))
            .map_err(ora_err)?;
        Ok(OracleConnection {
            session: Arc::new(Mutex::new(Arc::new(session))),
            cancelled: Arc::new(Notify::new()),
            login: l,
            schema,
        })
    })
    .await
    .map_err(|e| DbError::new(e.to_string()))?
}

fn to_sql(v: &Value) -> Box<dyn ToDbValue + Send + Sync> {
    match v {
        Value::Null => Box::new(None::<String>),
        Value::Bool(b) => Box::new(*b),
        Value::Int(i) => Box::new(*i),
        Value::Float(f) => Box::new(*f),
        Value::Bytes(b) => Box::new(b.clone()),
        other => Box::new(other.to_string()),
    }
}

/// `YYYY-MM-DD HH:MM:SS[.fraction][+HH:MM]`, in the value's own time zone.
fn format_timestamp(ts: &OracleTimestamp, with_fraction: bool, with_tz: bool) -> String {
    let (h, m) = (ts.tz_hour_offset() as i64, ts.tz_minute_offset() as i64);
    let fields = (ts.year() as i32, ts.month() as u32, ts.day() as u32, ts.hour() as u32, ts.minute() as u32);
    let base = chrono::NaiveDate::from_ymd_opt(fields.0, fields.1, fields.2)
        .and_then(|d| d.and_hms_opt(fields.3, fields.4, ts.second() as u32));
    // Values with a time zone arrive in UTC; show the wall-clock time of their zone.
    let local = base.map(|t| if with_tz { t + chrono::Duration::minutes(h * 60 + m) } else { t });
    let mut s = match local {
        Some(t) => t.format("%Y-%m-%d %H:%M:%S").to_string(),
        None => ts.to_string(),
    };
    if with_fraction && ts.nanoseconds() > 0 {
        let frac = format!("{:09}", ts.nanoseconds());
        s.push('.');
        s.push_str(frac.trim_end_matches('0'));
    }
    if with_tz {
        let sign = if h < 0 || m < 0 { '-' } else { '+' };
        s.push_str(&format!("{sign}{:02}:{:02}", h.abs(), m.abs()));
    }
    s
}

/// A NUMBER as an exact integer when it is one, else its exact text.
fn number(n: OracleNumber) -> Value {
    let text = n.to_string();
    if !text.contains(['.', 'e', 'E'])
        && let Ok(i) = text.parse::<i64>()
    {
        return Value::Int(i);
    }
    Value::Numeric(text)
}

fn interval_ym(i: &OracleIntervalYM) -> String {
    let neg = i.years() < 0 || i.months() < 0;
    format!("{}{}-{}", if neg { "-" } else { "+" }, i.years().abs(), i.months().abs())
}

fn interval_ds(i: &OracleIntervalDS) -> String {
    let neg = i.days() < 0 || i.hours() < 0 || i.minutes() < 0 || i.seconds() < 0 || i.nanoseconds() < 0;
    let mut s = format!(
        "{}{} {:02}:{:02}:{:02}",
        if neg { "-" } else { "+" },
        i.days().abs(),
        i.hours().abs(),
        i.minutes().abs(),
        i.seconds().abs()
    );
    if i.nanoseconds() != 0 {
        s.push_str(&format!(".{:06}", i.nanoseconds().abs() / 1000));
    }
    s
}

fn json(v: &JsonValue) -> serde_json::Value {
    use serde_json::Value as J;
    match v {
        JsonValue::Null => J::Null,
        JsonValue::Boolean(b) => J::Bool(*b),
        JsonValue::String(s) => J::String(s.clone()),
        JsonValue::Number(n) => {
            let text = n.to_string();
            serde_json::from_str::<serde_json::Number>(&text).map(J::Number).unwrap_or(J::String(text))
        }
        JsonValue::BinaryDouble(f) => serde_json::Number::from_f64(*f).map_or(J::Null, J::Number),
        JsonValue::BinaryFloat(f) => serde_json::Number::from_f64(*f as f64).map_or(J::Null, J::Number),
        JsonValue::JsonArray(items) => J::Array(items.iter().map(json).collect()),
        JsonValue::JsonObject(map) => J::Object(map.iter().map(|(k, v)| (k.clone(), json(v))).collect()),
        JsonValue::Timestamp(t) => J::String(format_timestamp(t, true, false)),
        JsonValue::IntervalDS(i) => J::String(interval_ds(i)),
        JsonValue::IntervalYM(i) => J::String(interval_ym(i)),
        JsonValue::Raw(b) | JsonValue::JsonId(b) => J::String(b.iter().map(|x| format!("{x:02x}")).collect()),
        other => J::String(format!("{other:?}")),
    }
}

fn decode(row: &Row, i: usize, ty: &str) -> Value {
    let result = match ty {
        "DB_TYPE_NUMBER" | "DB_TYPE_BINARY_INTEGER" => {
            row.get::<Option<OracleNumber>>(i).map(|v| v.map_or(Value::Null, number))
        }
        "DB_TYPE_BINARY_DOUBLE" => row.get::<Option<f64>>(i).map(|v| v.map_or(Value::Null, Value::Float)),
        "DB_TYPE_BINARY_FLOAT" => row.get::<Option<f32>>(i).map(|v| v.map_or(Value::Null, |f| Value::Float(f as f64))),
        "DB_TYPE_DATE" => row
            .get::<Option<OracleTimestamp>>(i)
            .map(|v| v.map_or(Value::Null, |t| Value::Text(format_timestamp(&t, false, false)))),
        "DB_TYPE_TIMESTAMP" => row
            .get::<Option<OracleTimestamp>>(i)
            .map(|v| v.map_or(Value::Null, |t| Value::Text(format_timestamp(&t, true, false)))),
        "DB_TYPE_TIMESTAMP_TZ" | "DB_TYPE_TIMESTAMP_LTZ" => row
            .get::<Option<OracleTimestamp>>(i)
            .map(|v| v.map_or(Value::Null, |t| Value::Text(format_timestamp(&t, true, true)))),
        "DB_TYPE_RAW" | "DB_TYPE_BLOB" | "DB_TYPE_LONG_RAW" => {
            row.get::<Option<Vec<u8>>>(i).map(|v| v.map_or(Value::Null, Value::Bytes))
        }
        "DB_TYPE_BOOLEAN" => row.get::<Option<bool>>(i).map(|v| v.map_or(Value::Null, Value::Bool)),
        "DB_TYPE_JSON" => row.get::<Option<JsonValue>>(i).map(|v| v.map_or(Value::Null, |j| Value::Json(json(&j)))),
        "DB_TYPE_INTERVAL_YM" => {
            row.get::<Option<OracleIntervalYM>>(i).map(|v| v.map_or(Value::Null, |x| Value::Text(interval_ym(&x))))
        }
        "DB_TYPE_INTERVAL_DS" => {
            row.get::<Option<OracleIntervalDS>>(i).map(|v| v.map_or(Value::Null, |x| Value::Text(interval_ds(&x))))
        }
        _ => row.get::<Option<String>>(i).map(|v| v.map_or(Value::Null, Value::Text)),
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
    conn: &oracledb::Connection,
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
    let bound: Vec<Box<dyn ToDbValue + Send + Sync>> = params.iter().map(to_sql).collect();
    let names = bind_names(bound.len());
    let refs = named(&names, bound.iter().map(|b| b.as_ref() as &dyn ToDbValue));
    let mut stmt = conn.statement(sql).and_then(|b| b.build()).map_err(ora_err)?;
    if stmt.is_query() {
        let cursor = stmt.query_named(&refs).map_err(ora_err)?;
        let info: Vec<(String, String, String)> = cursor
            .columns()
            .iter()
            .map(|m| (m.name().to_string(), m.data_type(), m.db_type().name().to_string()))
            .collect();
        let origins =
            origins::infer(sql, current_schema, &info.iter().map(|(name, _, _)| name.clone()).collect::<Vec<_>>());
        let columns: Vec<Column> = info
            .iter()
            .zip(origins)
            .map(|((name, data_type, _), origin)| Column { name: name.clone(), type_name: data_type.clone(), origin })
            .collect();
        let mut out = Vec::new();
        let mut truncated = false;
        for row in cursor {
            let row = row.map_err(ora_err)?;
            if max_rows.is_some_and(|max| out.len() >= max) {
                truncated = true;
                break;
            }
            out.push(info.iter().enumerate().map(|(i, (_, _, ty))| decode(&row, i, ty)).collect());
        }
        return Ok(ExecOutcome::Rows(ResultSet { columns, rows: out, truncated }));
    }
    let result = stmt.execute_named(&refs).map_err(ora_err)?;
    let affected = if stmt.is_dml() { result.rows_affected() } else { 0 };
    // Auto mode: each writing statement commits on its own.
    if !*in_transaction && (stmt.is_dml() || stmt.is_plsql()) {
        conn.commit().map_err(ora_err)?;
    }
    Ok(ExecOutcome::Affected(affected))
}

impl OracleConnection {
    /// Runs `f` on the live session off the async runtime. Returns early,
    /// with a "cancelled" error, if Cancel abandons the session meanwhile.
    async fn blocking<T, F>(&self, f: F) -> DbResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&oracledb::Connection, &mut bool, &str) -> DbResult<T> + Send + 'static,
    {
        let session =
            self.session.lock().map_err(|_| DbError::new("Oracle connection poisoned by an earlier panic"))?.clone();
        let schema = self.schema.clone();
        let cancelled = self.cancelled.notified();
        tokio::pin!(cancelled);
        cancelled.as_mut().enable();
        let task = tokio::task::spawn_blocking(move || {
            let mut in_tx =
                session.in_tx.lock().map_err(|_| DbError::new("Oracle connection poisoned by an earlier panic"))?;
            f(&session.conn, &mut in_tx, &schema)
        });
        tokio::select! {
            result = task => result.map_err(|e| DbError::new(e.to_string()))?,
            _ = cancelled => Err(DbError {
                detail: Some(
                    "Oracle's thin driver can't interrupt a statement, so it was abandoned and this console \
                     now uses a new session. The database finishes the abandoned statement on its own and \
                     rolls back its uncommitted changes."
                        .into(),
                ),
                ..DbError::new("Query cancelled")
            }),
        }
    }
}

/// Runs a catalog query with string parameters.
/// "1", "2", … for binding `:1`, `:2`, … by name: the thin driver binds
/// positional values per occurrence, so a placeholder used twice would need
/// its value twice.
fn bind_names(n: usize) -> Vec<String> {
    (1..=n).map(|i| i.to_string()).collect()
}

fn named<'a>(
    names: &'a [String],
    values: impl Iterator<Item = &'a dyn ToDbValue>,
) -> Vec<(&'a str, &'a dyn ToDbValue)> {
    names.iter().map(String::as_str).zip(values).collect()
}

fn strings(conn: &oracledb::Connection, sql: &str, params: &[&str]) -> DbResult<Vec<Row>> {
    let names = bind_names(params.len());
    let refs = named(&names, params.iter().map(|p| p as &dyn ToDbValue));
    let rows = conn.query_named(sql, &refs).map_err(ora_err)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(ora_err)
}

fn get<'a, T: oracledb::FromDbValue<'a>>(row: &'a Row, i: usize) -> DbResult<T> {
    row.get(i).map_err(ora_err)
}

fn column_type(row: &Row) -> DbResult<String> {
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

fn table_details(conn: &oracledb::Connection, schema: &str, name: &str) -> DbResult<TableDetails> {
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
    conn: &oracledb::Connection,
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
            let mut ddl: String =
                strings(c, "SELECT DBMS_METADATA.GET_DDL(:1, :2, :3) FROM dual", &[object_type, &name, &schema])?
                    .first()
                    .map(|r| get(r, 0))
                    .transpose()?
                    .unwrap_or_default();
            // Indexes come separately; a table without any raises ORA-31608.
            if kind == RelationKind::Table
                && let Ok(rows) =
                    strings(c, "SELECT DBMS_METADATA.GET_DEPENDENT_DDL('INDEX', :1, :2) FROM dual", &[&name, &schema])
                && let Some(Ok(indexes)) = rows.first().map(|r| get::<String>(r, 0))
            {
                ddl.push('\n');
                ddl.push_str(&indexes);
            }
            Ok(ddl.trim().to_string())
        })
        .await
    }

    fn canceller(&self) -> Arc<dyn Canceller> {
        Arc::new(OracleCanceller {
            session: self.session.clone(),
            cancelled: self.cancelled.clone(),
            login: self.login.clone(),
        })
    }

    async fn in_transaction(&self) -> bool {
        self.blocking(|_, in_tx, _| Ok(*in_tx)).await.unwrap_or(false)
    }
}

/// Cancel by abandoning the session: open a new one, make it the live one and
/// wake the calls waiting on the old one.
struct OracleCanceller {
    session: Arc<Mutex<Arc<Session>>>,
    cancelled: Arc<Notify>,
    login: Arc<Login>,
}

#[async_trait]
impl Canceller for OracleCanceller {
    async fn cancel(&self) -> DbResult<()> {
        let login = self.login.clone();
        let fresh =
            tokio::task::spawn_blocking(move || open_session(&login)).await.map_err(|e| DbError::new(e.to_string()))?;
        // Wake the waiting call even if reconnecting failed, so the console isn't stuck.
        let result = fresh.map(|session| {
            if let Ok(mut live) = self.session.lock() {
                *live = Arc::new(session);
            }
        });
        self.cancelled.notify_waiters();
        result
    }
}
