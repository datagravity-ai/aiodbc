// Decimal/NUMERIC support.  Ported from src/decimal.cpp: values are fetched via a
// binary SQL_NUMERIC_STRUCT by default (locale-independent), with a string-parsing
// fallback that honors the configurable decimal separator.

use std::sync::Mutex;

use pyo3::prelude::*;
use pyo3::types::PyTuple;

// The locale's decimal separator, initialized by the Python layer from
// locale.localeconv() at import (see python/aiodbc/__init__.py).
static DECIMAL_SEPARATOR: Mutex<Option<String>> = Mutex::new(None);

#[pyfunction]
pub fn set_decimal_separator(sep: &str) {
    *DECIMAL_SEPARATOR.lock().unwrap() = Some(sep.to_string());
}

#[pyfunction]
pub fn get_decimal_separator() -> String {
    DECIMAL_SEPARATOR
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_else(|| ".".to_string())
}

/// Clean a textual NUMERIC value for decimal.Decimal: drop everything that is not a
/// digit, sign, or the separator (currency symbols, grouping), and normalize the
/// separator to '.'.  Ported from DecimalFromText.
pub fn clean_decimal_text(text: &str) -> String {
    let sep = get_decimal_separator();
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        if rest.starts_with(&sep) {
            out.push('.');
            rest = &rest[sep.len()..];
            continue;
        }
        let ch = rest.chars().next().unwrap();
        if ch.is_ascii_digit() || ch == '-' {
            out.push(ch);
        }
        rest = &rest[ch.len_utf8()..];
    }
    out
}

/// Build decimal.Decimal((sign, digits, exponent)) from a SQL_NUMERIC_STRUCT.
/// Ported from DecimalFromNumericStruct.
pub fn decimal_from_numeric_parts(
    py: Python<'_>,
    sign: u8,
    scale: i8,
    val: [u8; 16],
) -> PyResult<Py<PyAny>> {
    // Little-endian unsigned 16-byte magnitude.
    let magnitude = u128::from_le_bytes(val);
    let digits_str = magnitude.to_string();
    let digits = PyTuple::new(py, digits_str.bytes().map(|b| (b - b'0') as u32))?;

    // ODBC sign: 1 = positive; Decimal sign: 0 = positive.
    let decimal_sign: i32 = if sign == 1 { 0 } else { 1 };
    let exponent: i32 = -(scale as i32);

    let decimal_cls = py.import("decimal")?.getattr("Decimal")?;
    Ok(decimal_cls
        .call1(((decimal_sign, digits, exponent),))?
        .unbind())
}

/// Build decimal.Decimal from cleaned text ('.'-separated).
pub fn decimal_from_text(py: Python<'_>, text: &str) -> PyResult<Py<PyAny>> {
    let decimal_cls = py.import("decimal")?.getattr("Decimal")?;
    Ok(decimal_cls.call1((text,))?.unbind())
}

/// Convert a Python Decimal parameter into the plain string ODBC binding plus its
/// precision/scale.  Ported from GetDecimalInfo/CreateDecimalString in params.cpp.
pub fn decimal_param(cell: &Bound<'_, PyAny>) -> PyResult<(String, u64, i16)> {
    let t = cell.call_method0("as_tuple")?;
    let sign: i64 = t.get_item(0)?.extract()?;
    let digits: Vec<u8> = t.get_item(1)?.extract::<Vec<u8>>()?;
    let exp: i64 = t.get_item(2)?.extract()?;
    let count = digits.len() as i64;

    let mut text = String::new();
    if sign != 0 {
        text.push('-');
    }
    let dig = |d: &u8| (b'0' + d) as char;

    let (precision, scale): (u64, i16);
    if exp >= 0 {
        // (1 2 3) exp = 2 -> '12300'
        digits.iter().for_each(|d| text.push(dig(d)));
        (0..exp).for_each(|_| text.push('0'));
        precision = (count + exp) as u64;
        scale = 0;
    } else if -exp <= count {
        // (1 2 3) exp = -2 -> '1.23': precision 3, scale 2
        let point = (count + exp) as usize;
        for (i, d) in digits.iter().enumerate() {
            if i == point {
                text.push('.');
            }
            text.push(dig(d));
        }
        if point == 0 {
            // all digits fractional, e.g. (1 2 3) exp = -3 -> '.123'
            text.insert(if sign != 0 { 1 } else { 0 }, '0');
        }
        precision = count as u64;
        scale = (-exp) as i16;
    } else {
        // (1 2 3) exp = -5 -> '0.00123': precision 5, scale 5
        text.push_str("0.");
        (0..(-exp - count)).for_each(|_| text.push('0'));
        digits.iter().for_each(|d| text.push(dig(d)));
        precision = (-exp) as u64;
        scale = (-exp) as i16;
    }
    Ok((text, precision, scale))
}
