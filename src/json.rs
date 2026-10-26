//! JSON output in the exact byte format the app has always written (config, state and API
//! answers), plus the value coercions the logic relies on.
//!
//! - [`dumps`]: `json.dumps(obj, ensure_ascii=False)`: separators `", "` and `": "`, raw UTF-8.
//!   Every HTTP JSON response uses it.
//! - [`dump_pretty`]: `json.dump(obj, indent=2, ensure_ascii=False)` (config and state files).
//! - Floats are written as the shortest repr that round-trips: 19.0, 1761500000.123,
//!   1e-05, 1e+16.
//!
//! serde_json is built with `preserve_order`, so objects keep insertion order.

use serde_json::ser::{CompactFormatter, Formatter, PrettyFormatter};
use serde_json::{Map, Value};
use std::io;

/// `round(x, ndigits)`: correctly rounded on the exact binary value, ties to even
/// (`round(0.125, 2) == 0.12`, `round(19.04, 1) == 19.0`).
pub fn py_round(x: f64, ndigits: usize) -> f64 {
    if !x.is_finite() {
        return x;
    }
    format!("{:.*}", ndigits, x).parse().unwrap_or(x)
}

/// `repr(float)` for a finite f64: shortest round-trip digits, `.0` for integral values.
pub fn py_float_repr(x: f64) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0.0".into() } else { "0.0".into() };
    }
    // Rust's {:e} gives the shortest round-trip digits: "1.5e16", "-1e-5", "1.761500000123e9".
    let e = format!("{:e}", x);
    let (mant, exp) = e.split_once('e').expect("exponent form");
    let exp: i32 = exp.parse().expect("exponent");
    let neg = mant.starts_with('-');
    let digits: String = mant.chars().filter(|c| c.is_ascii_digit()).collect();
    let sign = if neg { "-" } else { "" };
    if (-4..16).contains(&exp) {
        // fixed notation
        let n = digits.len() as i32;
        let point = exp + 1; // digits before the decimal point
        let s = if point <= 0 {
            format!("0.{}{}", "0".repeat((-point) as usize), digits)
        } else if point >= n {
            format!("{}{}.0", digits, "0".repeat((point - n) as usize))
        } else {
            format!("{}.{}", &digits[..point as usize], &digits[point as usize..])
        };
        format!("{sign}{s}")
    } else {
        let m = if digits.len() > 1 { format!("{}.{}", &digits[..1], &digits[1..]) } else { digits };
        let es = if exp < 0 { "-" } else { "+" };
        format!("{sign}{m}e{es}{:02}", exp.abs())
    }
}

fn write_py_f64<W: ?Sized + io::Write>(w: &mut W, x: f64) -> io::Result<()> {
    w.write_all(py_float_repr(x).as_bytes())
}

/// `json.dumps` default separators: `", "` between items, `": "` after keys.
struct PyCompact;

impl Formatter for PyCompact {
    fn write_f64<W: ?Sized + io::Write>(&mut self, w: &mut W, v: f64) -> io::Result<()> {
        write_py_f64(w, v)
    }
    fn write_f32<W: ?Sized + io::Write>(&mut self, w: &mut W, v: f32) -> io::Result<()> {
        write_py_f64(w, v as f64)
    }
    fn begin_array_value<W: ?Sized + io::Write>(&mut self, w: &mut W, first: bool) -> io::Result<()> {
        if first { Ok(()) } else { w.write_all(b", ") }
    }
    fn begin_object_key<W: ?Sized + io::Write>(&mut self, w: &mut W, first: bool) -> io::Result<()> {
        if first { Ok(()) } else { w.write_all(b", ") }
    }
    fn begin_object_value<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
        w.write_all(b": ")
    }
}

/// `indent=2`: serde's pretty layout, with floats written by [`py_float_repr`].
struct PyPretty<'a>(PrettyFormatter<'a>);

macro_rules! delegate {
    ($($name:ident($($arg:ident: $t:ty),*);)*) => {
        $(fn $name<W: ?Sized + io::Write>(&mut self, w: &mut W $(, $arg: $t)*) -> io::Result<()> {
            self.0.$name(w $(, $arg)*)
        })*
    };
}

impl Formatter for PyPretty<'_> {
    fn write_f64<W: ?Sized + io::Write>(&mut self, w: &mut W, v: f64) -> io::Result<()> {
        write_py_f64(w, v)
    }
    fn write_f32<W: ?Sized + io::Write>(&mut self, w: &mut W, v: f32) -> io::Result<()> {
        write_py_f64(w, v as f64)
    }
    delegate! {
        begin_array();
        end_array();
        begin_array_value(first: bool);
        end_array_value();
        begin_object();
        end_object();
        begin_object_key(first: bool);
        end_object_key();
        begin_object_value();
        end_object_value();
    }
}

fn serialize<F: Formatter>(v: &Value, f: F) -> Vec<u8> {
    let mut out = Vec::with_capacity(128);
    let mut ser = serde_json::Serializer::with_formatter(&mut out, f);
    serde::Serialize::serialize(v, &mut ser).expect("serializing a Value cannot fail");
    out
}

/// `json.dumps(v, ensure_ascii=False).encode("utf-8")`.
pub fn dumps(v: &Value) -> Vec<u8> {
    serialize(v, PyCompact)
}

/// `json.dump(v, f, indent=2, ensure_ascii=False)` (no trailing newline).
pub fn dump_pretty(v: &Value) -> Vec<u8> {
    serialize(v, PyPretty(PrettyFormatter::with_indent(b"  ")))
}

/// Compact without spaces (serde default); only for logs/debugging.
pub fn compact(v: &Value) -> Vec<u8> {
    serialize(v, CompactFormatter)
}

/// Recursively sorted copy (`sort_keys=True`). Code-point order = UTF-8 byte order.
pub fn sorted(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let mut out = Map::new();
            for k in keys {
                out.insert(k.clone(), sorted(&m[k]));
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(sorted).collect()),
        other => other.clone(),
    }
}

/// Truthiness of a JSON value: null, false, 0, "" and empty arrays/objects are false.
pub fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// `int(x)` for a JSON value: ints, floats (truncated), bools, and strings of an
/// optionally signed decimal integer (surrounding whitespace and `_` separators allowed).
pub fn py_int(v: &Value) -> Option<i64> {
    match v {
        Value::Bool(b) => Some(*b as i64),
        Value::Number(n) => n.as_i64().or_else(|| {
            n.as_f64().filter(|f| f.is_finite()).map(|f| f.trunc() as i64)
        }),
        Value::String(s) => {
            let t = s.trim();
            let (neg, body) = match t.as_bytes().first() {
                Some(b'-') => (true, &t[1..]),
                Some(b'+') => (false, &t[1..]),
                _ => (false, t),
            };
            if body.is_empty()
                || body.starts_with('_')
                || body.ends_with('_')
                || body.contains("__")
                || !body.chars().all(|c| c.is_ascii_digit() || c == '_')
            {
                return None;
            }
            let digits: String = body.chars().filter(|c| *c != '_').collect();
            let n: i64 = digits.parse().ok()?;
            Some(if neg { -n } else { n })
        }
        _ => None,
    }
}

/// `str(x)` for a JSON value (only what a config value can hold).
pub fn py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Null => "None".into(),
        Value::Number(n) => match n.as_f64() {
            Some(f) if !n.is_i64() && !n.is_u64() => py_float_repr(f),
            _ => n.to_string(),
        },
        other => String::from_utf8_lossy(&dumps(other)).into_owned(),
    }
}

/// `repr(dict)` of a flat `{str: bool}` map, e.g. `{'auto_monitors_off': True}`.
pub fn py_repr_bool_map(pairs: &[(&str, bool)]) -> String {
    let items: Vec<String> = pairs
        .iter()
        .map(|(k, v)| format!("'{}': {}", k, if *v { "True" } else { "False" }))
        .collect();
    format!("{{{}}}", items.join(", "))
}

/// `json.dumps(s)[1:-1]` with the default `ensure_ascii=True`: a JSON string body where every
/// non-ASCII char is a `\uXXXX` escape (surrogate pairs above U+FFFF).
pub fn json_str_ascii_body(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{:04x}", unit));
                }
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn float_repr() {
        let cases = [
            (19.0, "19.0"),
            (0.5, "0.5"),
            (1761500000.123, "1761500000.123"),
            (1e16, "1e+16"),
            (1.5e16, "1.5e+16"),
            (1e-5, "1e-05"),
            (0.0001, "0.0001"),
            (123456789012345678.0, "1.2345678901234568e+17"),
            (-2.5, "-2.5"),
            (100.0, "100.0"),
            (1e15, "1000000000000000.0"),
        ];
        for (x, want) in cases {
            assert_eq!(py_float_repr(x), want, "{x}");
        }
    }

    #[test]
    fn round_half_even() {
        assert_eq!(py_round(0.125, 2), 0.12);
        assert_eq!(py_round(2.5, 0), 2.0);
        assert_eq!(py_round(2.675, 2), 2.67); // 2.67499999... in binary
        assert_eq!(py_round(28.54, 1), 28.5);
        assert_eq!(py_round(62.0, 1), 62.0);
    }

    #[test]
    fn dumps_format() {
        let v = json!({"error": "Autentificare necesară.", "a": [1, 2.0, true, null], "b": {}});
        assert_eq!(
            String::from_utf8(dumps(&v)).unwrap(),
            r#"{"error": "Autentificare necesară.", "a": [1, 2.0, true, null], "b": {}}"#
        );
    }

    #[test]
    fn pretty_format() {
        let v = json!({"secret": "x", "settings": {"auto_monitors_off": true}, "l": [], "f": 1.0});
        assert_eq!(
            String::from_utf8(dump_pretty(&v)).unwrap(),
            "{\n  \"secret\": \"x\",\n  \"settings\": {\n    \"auto_monitors_off\": true\n  },\n  \"l\": [],\n  \"f\": 1.0\n}"
        );
    }

    #[test]
    fn ascii_body() {
        assert_eq!(json_str_ascii_body("Plex \"Couch\" ș😀"), "Plex \\\"Couch\\\" \\u0219\\ud83d\\ude00");
    }

    #[test]
    fn py_int_rules() {
        assert_eq!(py_int(&json!(2)), Some(2));
        assert_eq!(py_int(&json!(2.7)), Some(2));
        assert_eq!(py_int(&json!(" 3 ")), Some(3));
        assert_eq!(py_int(&json!("1_0")), Some(10));
        assert_eq!(py_int(&json!("3.0")), None);
        assert_eq!(py_int(&json!("abc")), None);
        assert_eq!(py_int(&json!(true)), Some(1));
        assert_eq!(py_int(&json!([1])), None);
    }
}
