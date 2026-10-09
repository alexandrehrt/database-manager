//! Decoding of binary-format column values into [`Value`].
//!
//! Every column is first read as raw bytes so that any server type can be
//! displayed; types without a decoder fall back to a hex dump tagged with the
//! type name.

use std::error::Error;

use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use dbm_core::Value;
use tokio_postgres::types::{FromSql, Kind, Type};

/// The undecoded wire bytes of a column, `None` for SQL NULL.
pub struct Raw<'a>(pub Option<&'a [u8]>);

impl<'a> FromSql<'a> for Raw<'a> {
    fn from_sql(_: &Type, raw: &'a [u8]) -> Result<Self, Box<dyn Error + Sync + Send>> {
        Ok(Raw(Some(raw)))
    }

    fn from_sql_null(_: &Type) -> Result<Self, Box<dyn Error + Sync + Send>> {
        Ok(Raw(None))
    }

    fn accepts(_: &Type) -> bool {
        true
    }
}

pub fn decode(ty: &Type, raw: &[u8]) -> Value {
    decode_known(ty, raw).unwrap_or_else(|| fallback(ty, raw))
}

fn fallback(ty: &Type, raw: &[u8]) -> Value {
    Value::Text(format!("<{}> {}", ty.name(), Value::Bytes(raw.to_vec())))
}

fn get<'a, T: FromSql<'a>>(ty: &Type, raw: &'a [u8]) -> Option<T> {
    T::from_sql(ty, raw).ok()
}

fn decode_known(ty: &Type, raw: &[u8]) -> Option<Value> {
    let text = || Value::Text(String::from_utf8_lossy(raw).into_owned());
    Some(match *ty {
        Type::BOOL => Value::Bool(get(ty, raw)?),
        Type::INT2 => Value::Int(get::<i16>(ty, raw)?.into()),
        Type::INT4 => Value::Int(get::<i32>(ty, raw)?.into()),
        Type::INT8 => Value::Int(get(ty, raw)?),
        Type::OID => Value::Int(get::<u32>(ty, raw)?.into()),
        // Round-trip through f32's shortest representation so 0.1 stays 0.1.
        Type::FLOAT4 => Value::Float(get::<f32>(ty, raw)?.to_string().parse().ok()?),
        Type::FLOAT8 => Value::Float(get(ty, raw)?),
        Type::NUMERIC => numeric(raw)?,
        Type::MONEY => Value::Numeric(scaled(i64::from_be_bytes(raw.try_into().ok()?), 2)),
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::UNKNOWN | Type::XML => text(),
        Type::CHAR => Value::Text(char::from(*raw.first()?).to_string()),
        Type::BYTEA => Value::Bytes(raw.to_vec()),
        Type::JSON => Value::Json(serde_json::from_slice(raw).ok()?),
        Type::JSONB => Value::Json(serde_json::from_slice(raw.get(1..)?).ok()?),
        Type::UUID => Value::Text(uuid::Uuid::from_slice(raw).ok()?.to_string()),
        Type::DATE => special_time(raw, 4).or_else(|| Some(Value::Text(get::<NaiveDate>(ty, raw)?.to_string())))?,
        Type::TIME => Value::Text(trim_fraction(&get::<NaiveTime>(ty, raw)?.format("%H:%M:%S%.f").to_string())),
        Type::TIMETZ => timetz(raw)?,
        Type::TIMESTAMP => special_time(raw, 8).or_else(|| {
            let ts = get::<NaiveDateTime>(ty, raw)?;
            Some(Value::Text(trim_fraction(&ts.format("%Y-%m-%d %H:%M:%S%.f").to_string())))
        })?,
        Type::TIMESTAMPTZ => special_time(raw, 8).or_else(|| {
            let ts = get::<DateTime<Utc>>(ty, raw)?;
            Some(Value::Text(format!("{}+00", trim_fraction(&ts.format("%Y-%m-%d %H:%M:%S%.f").to_string()))))
        })?,
        Type::POINT => Value::Text(format!("({},{})", f64::from_be_bytes(raw.get(0..8)?.try_into().ok()?), f64::from_be_bytes(raw.get(8..16)?.try_into().ok()?))),
        Type::INTERVAL => interval(raw)?,
        Type::INET | Type::CIDR => inet(raw)?,
        _ => match ty.kind() {
            Kind::Enum(_) => text(),
            Kind::Domain(base) => decode_known(base, raw)?,
            Kind::Array(elem) => array(elem, raw)?,
            // Extension types such as citext use the text wire format.
            _ if matches!(ty.name(), "citext" | "ltree" | "lquery") => text(),
            _ => return None,
        },
    })
}

/// Drops trailing zeros of fractional seconds, as Postgres prints them.
fn trim_fraction(s: &str) -> String {
    match s.rsplit_once('.') {
        Some((head, frac)) if frac.bytes().all(|b| b.is_ascii_digit()) => {
            let frac = frac.trim_end_matches('0');
            if frac.is_empty() { head.to_string() } else { format!("{head}.{frac}") }
        }
        _ => s.to_string(),
    }
}

fn special_time(raw: &[u8], width: usize) -> Option<Value> {
    let (pos, neg) = match width {
        4 => (i32::MAX.to_be_bytes().to_vec(), i32::MIN.to_be_bytes().to_vec()),
        _ => (i64::MAX.to_be_bytes().to_vec(), i64::MIN.to_be_bytes().to_vec()),
    };
    if raw == pos.as_slice() {
        Some(Value::Text("infinity".into()))
    } else if raw == neg.as_slice() {
        Some(Value::Text("-infinity".into()))
    } else {
        None
    }
}

fn be_i16(raw: &[u8], at: usize) -> Option<i16> {
    Some(i16::from_be_bytes(raw.get(at..at + 2)?.try_into().ok()?))
}

fn be_i32(raw: &[u8], at: usize) -> Option<i32> {
    Some(i32::from_be_bytes(raw.get(at..at + 4)?.try_into().ok()?))
}

fn be_i64(raw: &[u8], at: usize) -> Option<i64> {
    Some(i64::from_be_bytes(raw.get(at..at + 8)?.try_into().ok()?))
}

/// Integer `v` shown with `scale` fractional digits.
fn scaled(v: i64, scale: u32) -> String {
    let div = 10i64.pow(scale);
    let sign = if v < 0 { "-" } else { "" };
    let (whole, frac) = ((v / div).unsigned_abs(), (v % div).unsigned_abs());
    format!("{sign}{whole}.{frac:0width$}", width = scale as usize)
}

/// NUMERIC wire format: ndigits, weight, sign, dscale, then base-10000 digits.
fn numeric(raw: &[u8]) -> Option<Value> {
    let ndigits = be_i16(raw, 0)? as usize;
    let weight = be_i16(raw, 2)? as i32;
    let sign = be_i16(raw, 4)? as u16;
    let dscale = be_i16(raw, 6)? as usize;
    let digits: Vec<i16> = (0..ndigits).map(|i| be_i16(raw, 8 + i * 2)).collect::<Option<_>>()?;
    match sign {
        0xC000 => return Some(Value::Numeric("NaN".into())),
        0xD000 => return Some(Value::Numeric("Infinity".into())),
        0xF000 => return Some(Value::Numeric("-Infinity".into())),
        _ => {}
    }
    let group = |i: i32| if i < 0 { 0 } else { digits.get(i as usize).copied().unwrap_or(0) };

    let mut out = String::new();
    if sign == 0x4000 {
        out.push('-');
    }
    if weight < 0 {
        out.push('0');
    } else {
        for i in 0..=weight {
            if i == 0 {
                out.push_str(&group(i).to_string());
            } else {
                out.push_str(&format!("{:04}", group(i)));
            }
        }
    }
    if dscale > 0 {
        let mut frac = String::new();
        let mut i = weight + 1;
        while frac.len() < dscale {
            frac.push_str(&format!("{:04}", group(i)));
            i += 1;
        }
        frac.truncate(dscale);
        out.push('.');
        out.push_str(&frac);
    }
    Some(Value::Numeric(out))
}

fn timetz(raw: &[u8]) -> Option<Value> {
    let micros = be_i64(raw, 0)?;
    // The wire offset is seconds west of UTC.
    let offset = -be_i32(raw, 8)?;
    let time = NaiveTime::from_num_seconds_from_midnight_opt(
        (micros / 1_000_000) as u32,
        ((micros % 1_000_000) * 1000) as u32,
    )?;
    let time = trim_fraction(&time.format("%H:%M:%S%.f").to_string());
    let (sign, abs) = if offset < 0 { ('-', -offset) } else { ('+', offset) };
    Some(Value::Text(format!("{time}{sign}{:02}:{:02}", abs / 3600, abs % 3600 / 60)))
}

/// Formatted like Postgres' default `IntervalStyle = postgres`.
fn interval(raw: &[u8]) -> Option<Value> {
    let micros = be_i64(raw, 0)?;
    let days = be_i32(raw, 8)?;
    let months = be_i32(raw, 12)?;
    let mut parts = Vec::new();
    let plural = |n: i32, unit: &str, units: &str| format!("{n} {}", if n.abs() == 1 { unit } else { units });
    if months / 12 != 0 {
        parts.push(plural(months / 12, "year", "years"));
    }
    if months % 12 != 0 {
        parts.push(plural(months % 12, "mon", "mons"));
    }
    if days != 0 {
        parts.push(plural(days, "day", "days"));
    }
    if micros != 0 || parts.is_empty() {
        let sign = if micros < 0 { "-" } else { "" };
        let m = micros.unsigned_abs();
        let (h, min, s, us) = (m / 3_600_000_000, m / 60_000_000 % 60, m / 1_000_000 % 60, m % 1_000_000);
        let mut t = format!("{sign}{h:02}:{min:02}:{s:02}");
        if us != 0 {
            t.push_str(format!(".{us:06}").trim_end_matches('0'));
        }
        parts.push(t);
    }
    Some(Value::Text(parts.join(" ")))
}

fn inet(raw: &[u8]) -> Option<Value> {
    let (family, bits, is_cidr, len) = (*raw.first()?, *raw.get(1)?, *raw.get(2)?, *raw.get(3)? as usize);
    let addr = raw.get(4..4 + len)?;
    let (ip, full): (std::net::IpAddr, u8) = match family {
        2 => (<[u8; 4]>::try_from(addr).ok()?.into(), 32),
        3 => (<[u8; 16]>::try_from(addr).ok()?.into(), 128),
        _ => return None,
    };
    Some(Value::Text(if bits == full && is_cidr == 0 { ip.to_string() } else { format!("{ip}/{bits}") }))
}

/// Array wire format: ndim, has-null flag, element OID, (len, lower bound)
/// per dimension, then length-prefixed elements in row-major order.
fn array(elem: &Type, raw: &[u8]) -> Option<Value> {
    let ndim = be_i32(raw, 0)? as usize;
    if ndim == 0 {
        return Some(Value::Array(Vec::new()));
    }
    let dims: Vec<usize> = (0..ndim).map(|d| be_i32(raw, 12 + d * 8).map(|n| n as usize)).collect::<Option<_>>()?;
    let mut pos = 12 + ndim * 8;
    let mut flat = Vec::with_capacity(dims.iter().product());
    for _ in 0..dims.iter().product::<usize>() {
        let len = be_i32(raw, pos)?;
        pos += 4;
        if len < 0 {
            flat.push(Value::Null);
        } else {
            let bytes = raw.get(pos..pos + len as usize)?;
            flat.push(decode(elem, bytes));
            pos += len as usize;
        }
    }
    Some(nest(&dims, &mut flat.into_iter()))
}

fn nest(dims: &[usize], items: &mut impl Iterator<Item = Value>) -> Value {
    match dims {
        [last] => Value::Array(items.take(*last).collect()),
        [first, rest @ ..] => Value::Array((0..*first).map(|_| nest(rest, items)).collect()),
        [] => Value::Array(Vec::new()),
    }
}
