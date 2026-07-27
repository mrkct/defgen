//! Just enough JSON to describe a compilation result.
//!
//! The playground is the only consumer and the shapes it reads are fixed, so
//! a writer is all this needs — a serialization framework would be more
//! dependency than the five object shapes below are worth.
//!
//! Every function returns an encoded fragment, and [`obj`] and [`arr`] take
//! fragments, so a value is built bottom-up and there is no builder state to
//! get wrong.

use std::fmt::Display;

/// A quoted, escaped JSON string.
pub fn s(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // Everything else below the space is unprintable and has no short
            // escape; generated code and ariadne's box drawing never contain
            // these, but a schema's own text could.
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A JSON number. Only ever called with integers here, so the `NaN`/infinity
/// spellings that JSON has no syntax for cannot come up.
pub fn n(value: impl Display) -> String {
    value.to_string()
}

pub fn b(value: bool) -> String {
    if value { "true".to_string() } else { "false".to_string() }
}

pub fn null() -> String {
    "null".to_string()
}

/// An object, from `(name, encoded value)` pairs.
pub fn obj(fields: &[(&str, String)]) -> String {
    let mut out = String::from("{");
    for (i, (name, value)) in fields.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&s(name));
        out.push(':');
        out.push_str(value);
    }
    out.push('}');
    out
}

/// An array, from encoded values.
pub fn arr(items: impl IntoIterator<Item = String>) -> String {
    let mut out = String::from("[");
    for (i, item) in items.into_iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&item);
    }
    out.push(']');
    out
}
