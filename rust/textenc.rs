// Text encoding configuration and conversion.  Ported from src/textenc.{h,cpp} and
// SetTextEncCommon in src/connection.cpp.
//
// A connection carries four independent encodings: reading SQL_CHAR data, reading
// SQL_WCHAR data, writing unicode (SQL statements and str parameters), and reading
// metadata such as column names (PostgreSQL/MySQL return UTF-16 column names from
// SQLDescribeColW regardless of connection settings).
//
// The common encodings are converted natively on the worker thread; anything else
// falls back to Python's codec machinery under the GIL.

use pyo3::exceptions::{PyUnicodeDecodeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyBytes;

pub const SQL_CHAR: i32 = 1;
pub const SQL_WCHAR: i32 = -8;
pub const SQL_WMETADATA: i32 = -888;

#[derive(Clone, Copy, PartialEq)]
pub enum OptEnc {
    Utf8,
    // "utf-16" means native order (pyodbc never adds a BOM when encoding).
    Utf16Ne,
    Utf16Le,
    Utf16Be,
    Utf32Ne,
    Utf32Le,
    Utf32Be,
    Latin1,
    // Not an optimized encoding: use Python's codec by name.
    Custom,
}

#[derive(Clone)]
pub struct TextEnc {
    pub opt: OptEnc,
    pub name: String,
    /// True when data is exchanged as SQL_C_WCHAR (16-bit buffers).
    pub wide: bool,
}

impl TextEnc {
    pub fn utf16_native() -> TextEnc {
        TextEnc {
            opt: OptEnc::Utf16Ne,
            name: if cfg!(target_endian = "little") {
                "utf-16le".to_string()
            } else {
                "utf-16be".to_string()
            },
            wide: true,
        }
    }
}

/// The four per-connection encodings, with the pyodbc defaults (all UTF-16 in
/// native byte order, read/written as SQL_C_WCHAR; see connection.cpp).
#[derive(Clone)]
pub struct ConnEncodings {
    pub sqlchar: TextEnc,
    pub sqlwchar: TextEnc,
    pub metadata: TextEnc,
    pub unicode: TextEnc, // the write encoding
}

impl Default for ConnEncodings {
    fn default() -> Self {
        ConnEncodings {
            sqlchar: TextEnc::utf16_native(),
            sqlwchar: TextEnc::utf16_native(),
            metadata: TextEnc::utf16_native(),
            unicode: TextEnc::utf16_native(),
        }
    }
}

/// Build a TextEnc from setencoding/setdecoding arguments.  Ported from
/// SetTextEncCommon.
pub fn make_text_enc(py: Python<'_>, encoding: &str, ctype: Option<i32>) -> PyResult<TextEnc> {
    if py
        .import("codecs")?
        .call_method1("lookup", (encoding,))
        .is_err()
    {
        return Err(PyValueError::new_err(format!(
            "not a registered codec: '{encoding}'"
        )));
    }
    if let Some(c) = ctype {
        if c != 0 && c != SQL_CHAR && c != SQL_WCHAR {
            return Err(PyValueError::new_err(format!(
                "Invalid ctype {c}.  Must be SQL_CHAR or SQL_WCHAR"
            )));
        }
    }
    let ctype = ctype.unwrap_or(0);
    let wide_or = |default_wide: bool| match ctype {
        SQL_CHAR => false,
        SQL_WCHAR => true,
        _ => default_wide,
    };

    let normalized = encoding.to_ascii_lowercase().replace('_', "-");
    let (opt, wide) = match normalized.as_str() {
        "utf-8" | "utf8" => (OptEnc::Utf8, wide_or(false)),
        "utf-16" | "utf16" => (OptEnc::Utf16Ne, wide_or(true)),
        "utf-16-be" | "utf-16be" | "utf16be" => (OptEnc::Utf16Be, wide_or(true)),
        "utf-16-le" | "utf-16le" | "utf16le" => (OptEnc::Utf16Le, wide_or(true)),
        "utf-32" | "utf32" => (OptEnc::Utf32Ne, wide_or(true)),
        "utf-32-be" | "utf-32be" | "utf32be" => (OptEnc::Utf32Be, wide_or(true)),
        "utf-32-le" | "utf-32le" | "utf32le" => (OptEnc::Utf32Le, wide_or(true)),
        "latin-1" | "latin1" | "iso-8859-1" | "iso8859-1" => (OptEnc::Latin1, wide_or(false)),
        // Like SetTextEncCommon, custom codecs are always exchanged as SQL_C_CHAR.
        _ => (OptEnc::Custom, false),
    };
    Ok(TextEnc {
        opt,
        name: encoding.to_string(),
        wide,
    })
}

/// Text decoded on the worker, or raw bytes plus a codec name for the GIL-holding
/// finisher to decode via Python.
pub enum DecodedText {
    Native(String),
    Codec(Vec<u8>, String),
}

impl DecodedText {
    pub fn into_py(self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        match self {
            DecodedText::Native(s) => Ok(s.into_pyobject(py)?.into_any().unbind()),
            DecodedText::Codec(bytes, codec) => Ok(py
                .import("codecs")?
                .call_method1("decode", (PyBytes::new(py, &bytes), codec, "strict"))?
                .unbind()),
        }
    }

    /// Like into_py, but a decode failure returns the raw bytes instead of raising
    /// (used for diagnostic messages, matching GetDiagRecs in cursor.cpp).
    pub fn into_py_or_bytes(self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        match self {
            DecodedText::Native(s) => Ok(s.into_pyobject(py)?.into_any().unbind()),
            DecodedText::Codec(bytes, codec) => {
                let b = PyBytes::new(py, &bytes);
                match py
                    .import("codecs")?
                    .call_method1("decode", (&b, codec, "strict"))
                {
                    Ok(s) => Ok(s.unbind()),
                    Err(_) => Ok(b.into_any().unbind()),
                }
            }
        }
    }
}

fn decode_err(enc: &TextEnc) -> PyErr {
    PyUnicodeDecodeError::new_err(format!(
        "could not decode column data using encoding '{}'",
        enc.name
    ))
}

fn u16s(bytes: &[u8], swap: bool) -> Vec<u16> {
    let (pairs, _) = bytes.as_chunks::<2>();
    pairs
        .iter()
        .map(|c| {
            if swap {
                u16::from_ne_bytes(*c).swap_bytes()
            } else {
                u16::from_ne_bytes(*c)
            }
        })
        .collect()
}

/// Decode a fetched text buffer per the encoding.  Runs on the worker (no GIL).
pub fn decode(bytes: &[u8], enc: &TextEnc) -> PyResult<DecodedText> {
    let native_le = cfg!(target_endian = "little");
    Ok(match enc.opt {
        OptEnc::Utf8 => {
            DecodedText::Native(String::from_utf8(bytes.to_vec()).map_err(|_| decode_err(enc))?)
        }
        OptEnc::Utf16Ne => DecodedText::Native(
            String::from_utf16(&u16s(bytes, false)).map_err(|_| decode_err(enc))?,
        ),
        OptEnc::Utf16Le => DecodedText::Native(
            String::from_utf16(&u16s(bytes, !native_le)).map_err(|_| decode_err(enc))?,
        ),
        OptEnc::Utf16Be => DecodedText::Native(
            String::from_utf16(&u16s(bytes, native_le)).map_err(|_| decode_err(enc))?,
        ),
        OptEnc::Utf32Ne | OptEnc::Utf32Le | OptEnc::Utf32Be => {
            let swap = match enc.opt {
                OptEnc::Utf32Le => !native_le,
                OptEnc::Utf32Be => native_le,
                _ => false,
            };
            let (quads, _) = bytes.as_chunks::<4>();
            let mut s = String::with_capacity(quads.len());
            for q in quads {
                let mut v = u32::from_ne_bytes(*q);
                if swap {
                    v = v.swap_bytes();
                }
                s.push(char::from_u32(v).ok_or_else(|| decode_err(enc))?);
            }
            DecodedText::Native(s)
        }
        OptEnc::Latin1 => DecodedText::Native(bytes.iter().map(|&b| b as char).collect()),
        OptEnc::Custom => DecodedText::Codec(bytes.to_vec(), enc.name.clone()),
    })
}

/// Encode a Python string for writing (SQL statements, str parameters).  Runs under
/// the GIL so custom codecs can use Python's machinery.
pub fn encode(py: Python<'_>, s: &str, enc: &TextEnc) -> PyResult<Vec<u8>> {
    let native_le = cfg!(target_endian = "little");
    Ok(match enc.opt {
        OptEnc::Utf8 => s.as_bytes().to_vec(),
        OptEnc::Utf16Ne => s.encode_utf16().flat_map(|u| u.to_ne_bytes()).collect(),
        OptEnc::Utf16Le => {
            if native_le {
                s.encode_utf16().flat_map(|u| u.to_ne_bytes()).collect()
            } else {
                s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
            }
        }
        OptEnc::Utf16Be => s.encode_utf16().flat_map(|u| u.to_be_bytes()).collect(),
        OptEnc::Utf32Ne => s.chars().flat_map(|c| (c as u32).to_ne_bytes()).collect(),
        OptEnc::Utf32Le => s.chars().flat_map(|c| (c as u32).to_le_bytes()).collect(),
        OptEnc::Utf32Be => s.chars().flat_map(|c| (c as u32).to_be_bytes()).collect(),
        OptEnc::Latin1 | OptEnc::Custom => {
            let name = if enc.opt == OptEnc::Latin1 {
                "latin-1"
            } else {
                enc.name.as_str()
            };
            py.import("codecs")?
                .call_method1("encode", (s, name, "strict"))?
                .extract::<Vec<u8>>()?
        }
    })
}

/// The divisor used to turn an encoded byte count into the ColumnSize (character
/// count) when binding a string parameter.
pub fn column_size_denominator(enc: &TextEnc) -> usize {
    match enc.opt {
        OptEnc::Utf16Ne | OptEnc::Utf16Le | OptEnc::Utf16Be => 2,
        OptEnc::Utf32Ne | OptEnc::Utf32Le | OptEnc::Utf32Be => 4,
        _ => 1,
    }
}
