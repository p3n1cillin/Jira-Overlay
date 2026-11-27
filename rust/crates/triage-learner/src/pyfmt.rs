//! Python's text conversions, so files and messages read exactly as the
//! Python learner wrote them (`str()`, `repr()`, `{:.1%}` and friends).

use serde_json::{Map, Value};

/// `repr(float)` / `str(float)`: shortest round-trip digits, scientific
/// notation outside `1e-4 <= |x| < 1e16`, always with a fractional part.
pub fn float_repr(x: f64) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0.0".into() } else { "0.0".into() };
    }
    let sci = format!("{:e}", x.abs());
    let (mantissa, exp) = sci.split_once('e').expect("{:e} has an exponent");
    let exp: i32 = exp.parse().expect("integer exponent");
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let sign = if x < 0.0 { "-" } else { "" };
    if (-4..16).contains(&exp) {
        let n = digits.len() as i32;
        let body = if exp >= 0 {
            let int_len = exp + 1;
            if n <= int_len {
                format!("{}{}.0", digits, "0".repeat((int_len - n) as usize))
            } else {
                format!("{}.{}", &digits[..int_len as usize], &digits[int_len as usize..])
            }
        } else {
            format!("0.{}{}", "0".repeat((-exp - 1) as usize), digits)
        };
        format!("{sign}{body}")
    } else {
        let mant = if digits.len() > 1 { format!("{}.{}", &digits[..1], &digits[1..]) } else { digits };
        let esign = if exp < 0 { '-' } else { '+' };
        format!("{sign}{mant}e{esign}{:02}", exp.abs())
    }
}

/// A number as Python prints it, given whether it was an `int` in Python.
pub fn number_str(n: &serde_json::Number) -> String {
    if let Some(i) = n.as_i64() {
        i.to_string()
    } else if let Some(u) = n.as_u64() {
        u.to_string()
    } else {
        float_repr(n.as_f64().unwrap_or(0.0))
    }
}

/// A float that Python would have held as an `int` (sums of counters).
pub fn int_or_float(x: f64) -> String {
    if x.fract() == 0.0 && x.abs() < 9.0e15 { format!("{}", x as i64) } else { float_repr(x) }
}

/// Python `str()` of a JSON-loaded value.
pub fn py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => py_repr(other),
    }
}

/// Python `repr()` of a JSON-loaded value (`None`, `True`, `'text'`, ...).
pub fn py_repr(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Number(n) => number_str(n),
        Value::String(s) => str_repr(s),
        Value::Array(items) => format!("[{}]", items.iter().map(py_repr).collect::<Vec<_>>().join(", ")),
        Value::Object(map) => dict_repr(map),
    }
}

pub fn dict_repr(map: &Map<String, Value>) -> String {
    let parts: Vec<String> = map.iter().map(|(k, v)| format!("{}: {}", str_repr(k), py_repr(v))).collect();
    format!("{{{}}}", parts.join(", "))
}

/// `repr(list_of_str)`.
pub fn list_repr<S: AsRef<str>>(items: &[S]) -> String {
    format!("[{}]", items.iter().map(|s| str_repr(s.as_ref())).collect::<Vec<_>>().join(", "))
}

/// `repr(str)`: single quotes unless the text holds one and no double quote.
pub fn str_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// Python `format(x, ".{d}%")`.
pub fn pct(x: f64, decimals: usize) -> String {
    format!("{:.*}%", decimals, x * 100.0)
}

/// The first `n` characters (Python `s[:n]`).
pub fn prefix(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Python truthiness of an optional JSON value.
pub fn truthy(v: Option<&Value>) -> bool {
    v.is_some_and(triage_engine::text::truthy)
}

/// `json.dumps(..., ensure_ascii=True)`: escape every non-ASCII character.
pub fn ascii_escape(json: &str) -> String {
    let mut out = String::with_capacity(json.len());
    for c in json.chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            let mut buf = [0_u16; 2];
            for unit in c.encode_utf16(&mut buf) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn float_repr_matches_python() {
        for (x, want) in [
            (0.997, "0.997"),
            (1.0, "1.0"),
            (1e-5, "1e-05"),
            (0.0001, "0.0001"),
            (1e16, "1e+16"),
            (123456.0, "123456.0"),
            (1.5e20, "1.5e+20"),
            (-2.5, "-2.5"),
            (0.1 + 0.2, "0.30000000000000004"),
            (97.0, "97.0"),
        ] {
            assert_eq!(float_repr(x), want);
        }
    }

    #[test]
    fn repr_matches_python() {
        let v = json!({"a": [1, 2.5, null, true], "it's": "x\"y", "n": 0.0});
        assert_eq!(py_repr(&v), r#"{'a': [1, 2.5, None, True], "it's": 'x"y', 'n': 0.0}"#);
        assert_eq!(list_repr(&["Closed"]), "['Closed']");
        assert_eq!(pct(0.8465, 1), "84.7%");
        assert_eq!(pct(0.5, 0), "50%");
        assert_eq!(ascii_escape("\"Équipe α\" 😀"), "\"\\u00c9quipe \\u03b1\" \\ud83d\\ude00");
    }
}
