//! Decoding of the PostgreSQL binary format to text.
//!
//! tokio-postgres always requests results in binary format. Instead of
//! relying on the generic conversions (which fail on NUMERIC NaN/Infinity,
//! infinite dates, extension types, etc.), each type is decoded here and
//! formatted the way the server itself would in text output.

use std::fmt::Write as _;

use tokio_postgres::types::{FromSql, Kind, Type};

use crate::model::ValueKind;

type BoxError = Box<dyn std::error::Error + Sync + Send>;

/// Maximum bytes shown for very large binary values.
const BINARY_PREVIEW_BYTES: usize = 32 * 1024;

/// An already formatted cell value. Accepts any type.
pub struct PgCell(pub Option<String>);

impl<'a> FromSql<'a> for PgCell {
    fn from_sql(ty: &Type, raw: &'a [u8]) -> Result<Self, BoxError> {
        Ok(PgCell(Some(decode(ty, raw)?)))
    }

    fn from_sql_null(_: &Type) -> Result<Self, BoxError> {
        Ok(PgCell(None))
    }

    fn accepts(_: &Type) -> bool {
        true
    }
}

mod oid {
    pub const BOOL: u32 = 16;
    pub const BYTEA: u32 = 17;
    pub const CHAR: u32 = 18;
    pub const NAME: u32 = 19;
    pub const INT8: u32 = 20;
    pub const INT2: u32 = 21;
    pub const INT4: u32 = 23;
    pub const REGPROC: u32 = 24;
    pub const TEXT: u32 = 25;
    pub const OID: u32 = 26;
    pub const TID: u32 = 27;
    pub const XID: u32 = 28;
    pub const CID: u32 = 29;
    pub const JSON: u32 = 114;
    pub const XML: u32 = 142;
    pub const POINT: u32 = 600;
    pub const LSEG: u32 = 601;
    pub const BOX: u32 = 603;
    pub const LINE: u32 = 628;
    pub const CIDR: u32 = 650;
    pub const FLOAT4: u32 = 700;
    pub const FLOAT8: u32 = 701;
    pub const UNKNOWN: u32 = 705;
    pub const CIRCLE: u32 = 718;
    pub const MACADDR8: u32 = 774;
    pub const MONEY: u32 = 790;
    pub const MACADDR: u32 = 829;
    pub const INET: u32 = 869;
    pub const BPCHAR: u32 = 1042;
    pub const VARCHAR: u32 = 1043;
    pub const DATE: u32 = 1082;
    pub const TIME: u32 = 1083;
    pub const TIMESTAMP: u32 = 1114;
    pub const TIMESTAMPTZ: u32 = 1184;
    pub const INTERVAL: u32 = 1186;
    pub const TIMETZ: u32 = 1266;
    pub const BIT: u32 = 1560;
    pub const VARBIT: u32 = 1562;
    pub const NUMERIC: u32 = 1700;
    pub const REFCURSOR: u32 = 1790;
    pub const REGPROCEDURE: u32 = 2202;
    pub const REGOPER: u32 = 2203;
    pub const REGOPERATOR: u32 = 2204;
    pub const REGCLASS: u32 = 2205;
    pub const REGTYPE: u32 = 2206;
    pub const RECORD: u32 = 2249;
    pub const CSTRING: u32 = 2275;
    pub const VOID: u32 = 2278;
    pub const UUID: u32 = 2950;
    pub const PG_LSN: u32 = 3220;
    pub const REGCONFIG: u32 = 3734;
    pub const REGDICTIONARY: u32 = 3769;
    pub const JSONB: u32 = 3802;
    pub const JSONPATH: u32 = 4072;
    pub const REGNAMESPACE: u32 = 4089;
    pub const REGROLE: u32 = 4096;
    pub const REGCOLLATION: u32 = 4191;
    pub const XID8: u32 = 5069;
}

pub fn value_kind(ty: &Type) -> ValueKind {
    match ty.kind() {
        Kind::Domain(inner) => return value_kind(inner),
        Kind::Enum(_) => return ValueKind::Text,
        Kind::Array(_) | Kind::Range(_) | Kind::Multirange(_) | Kind::Composite(_) => {
            return ValueKind::Other
        }
        _ => {}
    }
    match ty.oid() {
        oid::INT2
        | oid::INT4
        | oid::INT8
        | oid::FLOAT4
        | oid::FLOAT8
        | oid::NUMERIC
        | oid::MONEY
        | oid::OID
        | oid::XID
        | oid::XID8
        | oid::CID => ValueKind::Number,
        oid::BOOL => ValueKind::Bool,
        oid::DATE | oid::TIME | oid::TIMETZ | oid::TIMESTAMP | oid::TIMESTAMPTZ | oid::INTERVAL => {
            ValueKind::Temporal
        }
        oid::BYTEA => ValueKind::Binary,
        oid::JSON | oid::JSONB | oid::JSONPATH => ValueKind::Json,
        oid::TEXT
        | oid::VARCHAR
        | oid::BPCHAR
        | oid::NAME
        | oid::CHAR
        | oid::UUID
        | oid::XML
        | oid::UNKNOWN
        | oid::CSTRING => ValueKind::Text,
        _ => ValueKind::Other,
    }
}

/// Readable type name (`varchar`, `int4[]`, `schema.type`).
pub fn type_label(ty: &Type) -> String {
    let base = match ty.kind() {
        Kind::Array(member) => return format!("{}[]", type_label(member)),
        _ => ty.name().to_string(),
    };
    match ty.schema() {
        "pg_catalog" | "public" | "" => base,
        schema => format!("{schema}.{base}"),
    }
}

pub fn decode(ty: &Type, raw: &[u8]) -> Result<String, BoxError> {
    match ty.kind() {
        Kind::Domain(inner) => return decode(inner, raw),
        Kind::Enum(_) => return utf8(raw),
        Kind::Array(member) => return decode_array(member, raw),
        Kind::Range(inner) => return decode_range(inner, raw),
        Kind::Multirange(inner) => return decode_multirange(inner, raw),
        Kind::Composite(fields) => {
            let types: Vec<Type> = fields.iter().map(|f| f.type_().clone()).collect();
            return decode_record(Some(&types), raw);
        }
        _ => {}
    }
    let mut r = Reader::new(raw);
    let text = match ty.oid() {
        oid::BOOL => (if r.u8()? != 0 { "true" } else { "false" }).to_string(),
        oid::INT2 => r.i16()?.to_string(),
        oid::INT4 => r.i32()?.to_string(),
        oid::INT8 => r.i64()?.to_string(),
        oid::OID
        | oid::XID
        | oid::CID
        | oid::REGPROC
        | oid::REGPROCEDURE
        | oid::REGOPER
        | oid::REGOPERATOR
        | oid::REGCLASS
        | oid::REGTYPE
        | oid::REGCONFIG
        | oid::REGDICTIONARY
        | oid::REGNAMESPACE
        | oid::REGROLE
        | oid::REGCOLLATION => r.u32()?.to_string(),
        oid::XID8 => r.u64()?.to_string(),
        oid::FLOAT4 => float(f64::from(r.f32()?), true),
        oid::FLOAT8 => float(r.f64()?, false),
        oid::NUMERIC => numeric(raw)?,
        oid::MONEY => money(r.i64()?),
        oid::TEXT
        | oid::VARCHAR
        | oid::BPCHAR
        | oid::NAME
        | oid::UNKNOWN
        | oid::XML
        | oid::REFCURSOR
        | oid::CSTRING
        | oid::JSON => utf8(raw)?,
        oid::CHAR => char::from(r.u8()?).to_string(),
        oid::JSONB | oid::JSONPATH => {
            // The first byte is the format version (1).
            utf8(raw.get(1..).unwrap_or_default())?
        }
        oid::UUID => uuid::Uuid::from_slice(raw)?.hyphenated().to_string(),
        oid::BYTEA => hex_bytea(raw),
        oid::DATE => date(r.i32()?),
        oid::TIME => time_of_day(r.i64()?),
        oid::TIMETZ => {
            let micros = r.i64()?;
            // The zone comes in seconds WEST of UTC.
            let zone = r.i32()?;
            format!("{}{}", time_of_day(micros), offset(-zone))
        }
        oid::TIMESTAMP => timestamp(r.i64()?, false),
        oid::TIMESTAMPTZ => timestamp(r.i64()?, true),
        oid::INTERVAL => {
            let micros = r.i64()?;
            let days = r.i32()?;
            let months = r.i32()?;
            interval(micros, days, months)
        }
        oid::INET | oid::CIDR => inet(raw, ty.oid() == oid::CIDR)?,
        oid::MACADDR | oid::MACADDR8 => raw
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(":"),
        oid::POINT => format!("({},{})", float(r.f64()?, false), float(r.f64()?, false)),
        oid::LSEG | oid::BOX => {
            let (x1, y1, x2, y2) = (r.f64()?, r.f64()?, r.f64()?, r.f64()?);
            let (open, close) = if ty.oid() == oid::LSEG {
                ("[", "]")
            } else {
                ("", "")
            };
            format!(
                "{open}({},{}),({},{}){close}",
                float(x1, false),
                float(y1, false),
                float(x2, false),
                float(y2, false)
            )
        }
        oid::LINE => format!(
            "{{{},{},{}}}",
            float(r.f64()?, false),
            float(r.f64()?, false),
            float(r.f64()?, false)
        ),
        oid::CIRCLE => format!(
            "<({},{}),{}>",
            float(r.f64()?, false),
            float(r.f64()?, false),
            float(r.f64()?, false)
        ),
        oid::BIT | oid::VARBIT => {
            let len = r.i32()?.max(0) as usize;
            let mut s = String::with_capacity(len);
            for i in 0..len {
                let byte = raw.get(4 + i / 8).copied().unwrap_or(0);
                s.push(if byte & (0x80 >> (i % 8)) != 0 {
                    '1'
                } else {
                    '0'
                });
            }
            s
        }
        oid::PG_LSN => {
            let v = r.u64()?;
            format!("{:X}/{:X}", v >> 32, v & 0xFFFF_FFFF)
        }
        oid::TID => {
            let block = r.u32()?;
            let offset = r.u16()?;
            format!("({block},{offset})")
        }
        oid::RECORD => decode_record(None, raw)?,
        oid::VOID => String::new(),
        _ => match ty.name() {
            "hstore" => hstore(raw)?,
            // citext and other textual extension types use plain UTF-8.
            _ => fallback(raw),
        },
    };
    Ok(text)
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], BoxError> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.buf.len());
        match end {
            Some(end) => {
                let slice = &self.buf[self.pos..end];
                self.pos = end;
                Ok(slice)
            }
            None => Err("truncated binary value".into()),
        }
    }

    fn u8(&mut self) -> Result<u8, BoxError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, BoxError> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into()?))
    }
    fn i16(&mut self) -> Result<i16, BoxError> {
        Ok(i16::from_be_bytes(self.take(2)?.try_into()?))
    }
    fn i32(&mut self) -> Result<i32, BoxError> {
        Ok(i32::from_be_bytes(self.take(4)?.try_into()?))
    }
    fn u32(&mut self) -> Result<u32, BoxError> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into()?))
    }
    fn i64(&mut self) -> Result<i64, BoxError> {
        Ok(i64::from_be_bytes(self.take(8)?.try_into()?))
    }
    fn u64(&mut self) -> Result<u64, BoxError> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into()?))
    }
    fn f32(&mut self) -> Result<f32, BoxError> {
        Ok(f32::from_be_bytes(self.take(4)?.try_into()?))
    }
    fn f64(&mut self) -> Result<f64, BoxError> {
        Ok(f64::from_be_bytes(self.take(8)?.try_into()?))
    }

    /// Length-prefixed value (-1 = NULL).
    fn sized(&mut self) -> Result<Option<&'a [u8]>, BoxError> {
        let len = self.i32()?;
        if len < 0 {
            Ok(None)
        } else {
            Ok(Some(self.take(len as usize)?))
        }
    }
}

fn utf8(raw: &[u8]) -> Result<String, BoxError> {
    Ok(String::from_utf8_lossy(raw).into_owned())
}

/// Text when it looks like text; otherwise hexadecimal, like bytea.
fn fallback(raw: &[u8]) -> String {
    match std::str::from_utf8(raw) {
        Ok(s)
            if !s
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t')) =>
        {
            s.to_string()
        }
        _ => hex_bytea(raw),
    }
}

fn hex_bytea(raw: &[u8]) -> String {
    let shown = &raw[..raw.len().min(BINARY_PREVIEW_BYTES)];
    let mut s = String::with_capacity(2 + shown.len() * 2);
    s.push_str("\\x");
    for b in shown {
        let _ = write!(s, "{b:02x}");
    }
    if raw.len() > shown.len() {
        let _ = write!(s, "… ({} bytes)", raw.len());
    }
    s
}

fn float(v: f64, single: bool) -> String {
    if v.is_nan() {
        "NaN".into()
    } else if v.is_infinite() {
        if v > 0.0 {
            "Infinity".into()
        } else {
            "-Infinity".into()
        }
    } else if single {
        // Shortest representation that preserves the 32-bit value.
        (v as f32).to_string()
    } else {
        v.to_string()
    }
}

fn money(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let abs = cents.unsigned_abs();
    format!("{sign}{}.{:02}", abs / 100, abs % 100)
}

/// NUMERIC: base-10000 digits with weight and display scale.
pub fn numeric(raw: &[u8]) -> Result<String, BoxError> {
    let mut r = Reader::new(raw);
    let ndigits = r.i16()?.max(0) as usize;
    let weight = r.i16()? as i32;
    let sign = r.u16()?;
    let dscale = r.u16()? as usize;
    match sign {
        0xC000 => return Ok("NaN".into()),
        0xD000 => return Ok("Infinity".into()),
        0xF000 => return Ok("-Infinity".into()),
        _ => {}
    }
    let mut digits = Vec::with_capacity(ndigits);
    for _ in 0..ndigits {
        digits.push(r.i16()?);
    }
    let digit = |i: i32| -> i16 {
        if i < 0 {
            0
        } else {
            digits.get(i as usize).copied().unwrap_or(0)
        }
    };

    let mut s = String::new();
    if sign == 0x4000 {
        s.push('-');
    }
    if weight < 0 {
        s.push('0');
    } else {
        for i in 0..=weight {
            if i == 0 {
                let _ = write!(s, "{}", digit(i));
            } else {
                let _ = write!(s, "{:04}", digit(i));
            }
        }
    }
    if dscale > 0 {
        s.push('.');
        let mut frac = String::with_capacity(dscale + 4);
        let mut i = weight + 1;
        while frac.len() < dscale {
            let _ = write!(frac, "{:04}", digit(i));
            i += 1;
        }
        frac.truncate(dscale);
        s.push_str(&frac);
    }
    Ok(s)
}

/// `num_days_from_ce()` of 2000-01-01, the PostgreSQL epoch.
const DAYS_2000_FROM_CE: i32 = 730_120;

fn ymd_from_days(days_since_2000: i64) -> Option<(i32, u32, u32)> {
    use chrono::Datelike;
    let ce = i64::from(DAYS_2000_FROM_CE) + days_since_2000;
    let date = chrono::NaiveDate::from_num_days_from_ce_opt(i32::try_from(ce).ok()?)?;
    Some((date.year(), date.month(), date.day()))
}

fn format_ymd(year: i32, month: u32, day: u32) -> (String, bool) {
    // Proleptic calendar: year 0 is 1 BC.
    if year <= 0 {
        (format!("{:04}-{month:02}-{day:02}", 1 - year), true)
    } else {
        (format!("{year:04}-{month:02}-{day:02}"), false)
    }
}

fn date(days: i32) -> String {
    match days {
        i32::MAX => "infinity".into(),
        i32::MIN => "-infinity".into(),
        _ => match ymd_from_days(i64::from(days)) {
            Some((y, m, d)) => {
                let (s, bc) = format_ymd(y, m, d);
                if bc {
                    format!("{s} BC")
                } else {
                    s
                }
            }
            None => days.to_string(),
        },
    }
}

fn fraction(micros: i64) -> String {
    if micros == 0 {
        return String::new();
    }
    let s = format!(".{micros:06}");
    s.trim_end_matches('0').to_string()
}

fn time_of_day(micros: i64) -> String {
    let secs = micros.div_euclid(1_000_000);
    let frac = micros.rem_euclid(1_000_000);
    format!(
        "{:02}:{:02}:{:02}{}",
        secs / 3600,
        (secs / 60) % 60,
        secs % 60,
        fraction(frac)
    )
}

/// Offset in seconds east of UTC → `+03`, `-03:30`.
fn offset(east_secs: i32) -> String {
    let sign = if east_secs < 0 { '-' } else { '+' };
    let abs = east_secs.unsigned_abs();
    let (h, m, s) = (abs / 3600, (abs / 60) % 60, abs % 60);
    match (m, s) {
        (0, 0) => format!("{sign}{h:02}"),
        (_, 0) => format!("{sign}{h:02}:{m:02}"),
        _ => format!("{sign}{h:02}:{m:02}:{s:02}"),
    }
}

/// TIMESTAMP(TZ): microseconds since 2000-01-01. With a time zone, shows the
/// machine's local zone, as a client with a local `TimeZone` would.
fn timestamp(micros: i64, with_tz: bool) -> String {
    match micros {
        i64::MAX => return "infinity".into(),
        i64::MIN => return "-infinity".into(),
        _ => {}
    }
    let (micros, tz) = if with_tz {
        let utc_secs = micros.div_euclid(1_000_000) + 946_684_800;
        let east = chrono::DateTime::from_timestamp(utc_secs, 0)
            .map(|dt| {
                use chrono::Offset;
                dt.with_timezone(&chrono::Local)
                    .offset()
                    .fix()
                    .local_minus_utc()
            })
            .unwrap_or(0);
        (micros + i64::from(east) * 1_000_000, Some(offset(east)))
    } else {
        (micros, None)
    };
    let days = micros.div_euclid(86_400_000_000);
    let in_day = micros.rem_euclid(86_400_000_000);
    let Some((y, m, d)) = ymd_from_days(days) else {
        return micros.to_string();
    };
    let (ymd, bc) = format_ymd(y, m, d);
    let mut s = format!("{ymd} {}", time_of_day(in_day));
    if let Some(tz) = tz {
        s.push_str(&tz);
    }
    if bc {
        s.push_str(" BC");
    }
    s
}

/// `postgres` style (the server default): `1 year 2 mons 3 days 04:05:06`.
fn interval(micros: i64, days: i32, months: i32) -> String {
    let mut parts: Vec<String> = Vec::new();
    let years = months / 12;
    let mons = months % 12;
    let plural =
        |n: i64, one: &str, many: &str| format!("{n} {}", if n.abs() == 1 { one } else { many });
    if years != 0 {
        parts.push(plural(i64::from(years), "year", "years"));
    }
    if mons != 0 {
        parts.push(plural(i64::from(mons), "mon", "mons"));
    }
    if days != 0 {
        parts.push(plural(i64::from(days), "day", "days"));
    }
    if micros != 0 || parts.is_empty() {
        let sign = if micros < 0 { "-" } else { "" };
        let abs = micros.unsigned_abs();
        let secs = abs / 1_000_000;
        let frac = (abs % 1_000_000) as i64;
        parts.push(format!(
            "{sign}{:02}:{:02}:{:02}{}",
            secs / 3600,
            (secs / 60) % 60,
            secs % 60,
            fraction(frac)
        ));
    }
    parts.join(" ")
}

fn inet(raw: &[u8], is_cidr_type: bool) -> Result<String, BoxError> {
    let mut r = Reader::new(raw);
    let family = r.u8()?;
    let bits = r.u8()?;
    let _is_cidr = r.u8()?;
    let len = r.u8()? as usize;
    let addr = r.take(len)?;
    let (text, max_bits) = match family {
        2 if len == 4 => (
            std::net::Ipv4Addr::new(addr[0], addr[1], addr[2], addr[3]).to_string(),
            32,
        ),
        3 if len == 16 => {
            let octets: [u8; 16] = addr.try_into()?;
            (std::net::Ipv6Addr::from(octets).to_string(), 128)
        }
        _ => return Ok(hex_bytea(raw)),
    };
    if is_cidr_type || bits != max_bits {
        Ok(format!("{text}/{bits}"))
    } else {
        Ok(text)
    }
}

fn hstore(raw: &[u8]) -> Result<String, BoxError> {
    let mut r = Reader::new(raw);
    let count = r.i32()?.max(0);
    let mut pairs = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let key = r.sized()?.map(String::from_utf8_lossy).unwrap_or_default();
        let value = match r.sized()? {
            Some(v) => format!("\"{}\"", escape_quoted(&String::from_utf8_lossy(v))),
            None => "NULL".into(),
        };
        pairs.push(format!("\"{}\"=>{value}", escape_quoted(&key)));
    }
    Ok(pairs.join(", "))
}

fn escape_quoted(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Quoting in array output style: empty strings, a literal NULL, whitespace
/// or special characters require quotes.
fn array_element(s: &str) -> String {
    let needs_quotes = s.is_empty()
        || s.eq_ignore_ascii_case("null")
        || s.chars()
            .any(|c| matches!(c, '{' | '}' | ',' | '"' | '\\') || c.is_whitespace());
    if needs_quotes {
        format!("\"{}\"", escape_quoted(s))
    } else {
        s.to_string()
    }
}

fn decode_array(member: &Type, raw: &[u8]) -> Result<String, BoxError> {
    let mut r = Reader::new(raw);
    let ndim = r.i32()?.max(0) as usize;
    let _has_nulls = r.i32()?;
    let _elem_oid = r.u32()?;
    if ndim == 0 {
        return Ok("{}".into());
    }
    let mut dims = Vec::with_capacity(ndim);
    for _ in 0..ndim {
        let len = r.i32()?.max(0) as usize;
        let lower = r.i32()?;
        dims.push((len, lower));
    }
    let mut prefix = String::new();
    if dims.iter().any(|&(_, lower)| lower != 1) {
        for &(len, lower) in &dims {
            let _ = write!(prefix, "[{}:{}]", lower, lower + len as i32 - 1);
        }
        prefix.push('=');
    }
    let total: usize = dims.iter().map(|d| d.0).product();
    let mut elems = Vec::with_capacity(total);
    for _ in 0..total {
        elems.push(match r.sized()? {
            Some(bytes) => array_element(&decode(member, bytes)?),
            None => "NULL".into(),
        });
    }
    let mut out = prefix;
    let mut idx = 0;
    write_dims(&mut out, &dims, &elems, &mut idx);
    Ok(out)
}

fn write_dims(out: &mut String, dims: &[(usize, i32)], elems: &[String], idx: &mut usize) {
    out.push('{');
    let (len, _) = dims[0];
    for i in 0..len {
        if i > 0 {
            out.push(',');
        }
        if dims.len() > 1 {
            write_dims(out, &dims[1..], elems, idx);
        } else if let Some(e) = elems.get(*idx) {
            out.push_str(e);
            *idx += 1;
        }
    }
    out.push('}');
}

fn decode_range(inner: &Type, raw: &[u8]) -> Result<String, BoxError> {
    const EMPTY: u8 = 0x01;
    const LB_INC: u8 = 0x02;
    const UB_INC: u8 = 0x04;
    const LB_INF: u8 = 0x08;
    const UB_INF: u8 = 0x10;
    let mut r = Reader::new(raw);
    let flags = r.u8()?;
    if flags & EMPTY != 0 {
        return Ok("empty".into());
    }
    let mut bound = |inf: bool| -> Result<String, BoxError> {
        if inf {
            return Ok(String::new());
        }
        match r.sized()? {
            Some(b) => {
                let v = decode(inner, b)?;
                let quote = v
                    .chars()
                    .any(|c| matches!(c, ',' | '(' | ')' | '[' | ']' | '"') || c.is_whitespace());
                Ok(if quote {
                    format!("\"{}\"", escape_quoted(&v))
                } else {
                    v
                })
            }
            None => Ok(String::new()),
        }
    };
    let lower = bound(flags & LB_INF != 0)?;
    let upper = bound(flags & UB_INF != 0)?;
    Ok(format!(
        "{}{lower},{upper}{}",
        if flags & LB_INC != 0 { '[' } else { '(' },
        if flags & UB_INC != 0 { ']' } else { ')' }
    ))
}

fn decode_multirange(inner: &Type, raw: &[u8]) -> Result<String, BoxError> {
    let mut r = Reader::new(raw);
    let count = r.i32()?.max(0);
    let mut parts = Vec::with_capacity(count as usize);
    for _ in 0..count {
        if let Some(b) = r.sized()? {
            parts.push(decode_range(inner, b)?);
        }
    }
    Ok(format!("{{{}}}", parts.join(",")))
}

/// Composite type or `record`. Without the field types (anonymous record),
/// uses the OID carried in the value itself.
fn decode_record(types: Option<&[Type]>, raw: &[u8]) -> Result<String, BoxError> {
    let mut r = Reader::new(raw);
    let count = r.i32()?.max(0) as usize;
    let mut parts = Vec::with_capacity(count);
    for i in 0..count {
        let field_oid = r.u32()?;
        let ty = types
            .and_then(|t| t.get(i).cloned())
            .or_else(|| Type::from_oid(field_oid));
        let text = match r.sized()? {
            None => String::new(),
            Some(bytes) => {
                let v = match &ty {
                    Some(ty) => decode(ty, bytes)?,
                    None => fallback(bytes),
                };
                let quote = v.is_empty()
                    || v.chars()
                        .any(|c| matches!(c, ',' | '(' | ')' | '"' | '\\') || c.is_whitespace());
                if quote {
                    format!("\"{}\"", escape_quoted(&v))
                } else {
                    v
                }
            }
        };
        parts.push(text);
    }
    Ok(format!("({})", parts.join(",")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numeric_bytes(ndigits: i16, weight: i16, sign: u16, dscale: u16, digits: &[i16]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&ndigits.to_be_bytes());
        v.extend_from_slice(&weight.to_be_bytes());
        v.extend_from_slice(&sign.to_be_bytes());
        v.extend_from_slice(&dscale.to_be_bytes());
        for d in digits {
            v.extend_from_slice(&d.to_be_bytes());
        }
        v
    }

    #[test]
    fn numeric_formats_like_server() {
        // 12345.678 → digits [1, 2345, 6780], weight 1, scale 3
        assert_eq!(
            numeric(&numeric_bytes(3, 1, 0, 3, &[1, 2345, 6780])).unwrap(),
            "12345.678"
        );
        // -0.0012 → [12], weight -1, scale 4
        assert_eq!(
            numeric(&numeric_bytes(1, -1, 0x4000, 4, &[12])).unwrap(),
            "-0.0012"
        );
        // 0.00000001 → groups 0000 0001: digit [1], weight -2, scale 8
        assert_eq!(
            numeric(&numeric_bytes(1, -2, 0, 8, &[1])).unwrap(),
            "0.00000001"
        );
        // 10000 → [1], weight 1, scale 0 (trailing zeros omitted from the digits)
        assert_eq!(numeric(&numeric_bytes(1, 1, 0, 0, &[1])).unwrap(), "10000");
        // 1.50 → scale 2 keeps the zero
        assert_eq!(
            numeric(&numeric_bytes(2, 0, 0, 2, &[1, 5000])).unwrap(),
            "1.50"
        );
        assert_eq!(numeric(&numeric_bytes(0, 0, 0, 0, &[])).unwrap(), "0");
        assert_eq!(
            numeric(&numeric_bytes(0, 0, 0xC000, 0, &[])).unwrap(),
            "NaN"
        );
        assert_eq!(
            numeric(&numeric_bytes(0, 0, 0xF000, 0, &[])).unwrap(),
            "-Infinity"
        );
    }

    #[test]
    fn dates_and_times() {
        assert_eq!(date(0), "2000-01-01");
        assert_eq!(date(-1), "1999-12-31");
        assert_eq!(date(i32::MAX), "infinity");
        assert_eq!(
            timestamp(86_400_000_000 + 3_723_500_000, false),
            "2000-01-02 01:02:03.5"
        );
        assert_eq!(time_of_day(45_296_000_001), "12:34:56.000001");
        assert_eq!(offset(-3 * 3600), "-03");
        assert_eq!(offset(5 * 3600 + 1800), "+05:30");
    }

    #[test]
    fn intervals() {
        assert_eq!(interval(0, 0, 0), "00:00:00");
        assert_eq!(
            interval(14_706_000_000, 3, 14),
            "1 year 2 mons 3 days 04:05:06"
        );
        assert_eq!(interval(-1_500_000, 1, 0), "1 day -00:00:01.5");
    }

    #[test]
    fn arrays() {
        let mut raw = Vec::new();
        raw.extend_from_slice(&1i32.to_be_bytes()); // ndim
        raw.extend_from_slice(&1i32.to_be_bytes()); // has nulls
        raw.extend_from_slice(&25u32.to_be_bytes()); // text
        raw.extend_from_slice(&3i32.to_be_bytes()); // len
        raw.extend_from_slice(&1i32.to_be_bytes()); // lower
        for item in [Some("a b"), None, Some("x")] {
            match item {
                Some(s) => {
                    raw.extend_from_slice(&(s.len() as i32).to_be_bytes());
                    raw.extend_from_slice(s.as_bytes());
                }
                None => raw.extend_from_slice(&(-1i32).to_be_bytes()),
            }
        }
        assert_eq!(decode_array(&Type::TEXT, &raw).unwrap(), "{\"a b\",NULL,x}");
    }

    #[test]
    fn inet_values() {
        assert_eq!(
            inet(&[2, 32, 0, 4, 10, 0, 0, 1], false).unwrap(),
            "10.0.0.1"
        );
        assert_eq!(
            inet(&[2, 24, 1, 4, 10, 0, 0, 0], true).unwrap(),
            "10.0.0.0/24"
        );
    }
}
