//! Lenient reads of upstream JSON (`d.get(k) or default`, `float(x or 0)`, `str(x)`).

use crate::json::{py_int, py_str, truthy};
use serde_json::Value;

/// `d.get(k)` (None for a missing key or a non-object).
pub fn get<'a>(d: &'a Value, k: &str) -> Option<&'a Value> {
    d.get(k)
}

/// `d.get(k)` if truthy.
pub fn get_truthy<'a>(d: &'a Value, k: &str) -> Option<&'a Value> {
    d.get(k).filter(|v| truthy(Some(v)))
}

/// `str(d.get(k) or "")`.
pub fn s(d: &Value, k: &str) -> String {
    get_truthy(d, k).map(py_str).unwrap_or_default()
}

/// `float(x or 0)` (strings are parsed; anything else is 0).
pub fn f(v: Option<&Value>) -> f64 {
    match v {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::Bool(b)) => *b as i64 as f64,
        Some(Value::String(t)) => t.trim().parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// `int(x or 0)`.
pub fn i(v: Option<&Value>) -> i64 {
    match v {
        Some(x) if truthy(Some(x)) => match x {
            Value::String(t) => py_int(x).unwrap_or_else(|| t.trim().parse::<f64>().map(|f| f as i64).unwrap_or(0)),
            _ => py_int(x).unwrap_or(0),
        },
        _ => 0,
    }
}

/// A JSON value as a map key (`2` and `"2"` differ).
pub fn key(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

/// `str(ident if ident is not None else "")` for poster ids.
pub fn ident(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(x) => py_str(x),
    }
}

/// `(x or {})` as an object reference.
pub fn obj<'a>(d: &'a Value, k: &str) -> &'a Value {
    static EMPTY: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    match d.get(k) {
        Some(v @ Value::Object(_)) => v,
        _ => EMPTY.get_or_init(|| Value::Object(Default::default())),
    }
}

/// `x or []` as a slice of a list value.
pub fn arr<'a>(d: &'a Value, k: &str) -> &'a [Value] {
    match d.get(k) {
        Some(Value::Array(a)) => a,
        _ => &[],
    }
}

/// The raw value or null (`d.get(k)` in a JSON output).
pub fn raw(d: &Value, k: &str) -> Value {
    d.get(k).cloned().unwrap_or(Value::Null)
}
