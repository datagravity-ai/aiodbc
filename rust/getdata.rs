// Converting fetched column data into values.  Ported (simplified) from
// src/getdata.cpp: fetches use SQLGetData per column like the C++ implementation.
// Phase-1 scope: text, binary, integers, floats, bit, date/time/timestamp; decimals,
// UUIDs, output converters and configurable encodings come in later phases.

use odbc_sys::{CDataType, HStmt, Len, Pointer, SqlReturn};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDate, PyDateTime, PyTime};

use crate::cursor::ColInfo;
use crate::errors::ProgrammingError;

// SQL type codes (values verified by rust/constants.rs, which is generated from the
// ODBC headers).
pub const SQL_CHAR: i16 = 1;
pub const SQL_NUMERIC: i16 = 2;
pub const SQL_DECIMAL: i16 = 3;
pub const SQL_INTEGER: i16 = 4;
pub const SQL_SMALLINT: i16 = 5;
pub const SQL_FLOAT: i16 = 6;
pub const SQL_REAL: i16 = 7;
pub const SQL_DOUBLE: i16 = 8;
pub const SQL_VARCHAR: i16 = 12;
pub const SQL_TYPE_DATE: i16 = 91;
pub const SQL_TYPE_TIME: i16 = 92;
pub const SQL_TYPE_TIMESTAMP: i16 = 93;
pub const SQL_DATE: i16 = 9;
pub const SQL_TIME: i16 = 10;
pub const SQL_TIMESTAMP: i16 = 11;
pub const SQL_LONGVARCHAR: i16 = -1;
pub const SQL_BINARY: i16 = -2;
pub const SQL_VARBINARY: i16 = -3;
pub const SQL_LONGVARBINARY: i16 = -4;
pub const SQL_BIGINT: i16 = -5;
pub const SQL_TINYINT: i16 = -6;
pub const SQL_BIT: i16 = -7;
pub const SQL_WCHAR: i16 = -8;
pub const SQL_WVARCHAR: i16 = -9;
pub const SQL_WLONGVARCHAR: i16 = -10;
pub const SQL_GUID: i16 = -11;

/// A fetched cell, safe to move between threads before conversion to Python.
pub enum CellValue {
    Null,
    Bool(bool),
    I64(i64),
    F64(f64),
    Str(String),
    Bytes(Vec<u8>),
    Date {
        year: i32,
        month: u8,
        day: u8,
    },
    Time {
        hour: u8,
        minute: u8,
        second: u8,
        micro: u32,
    },
    DateTime {
        year: i32,
        month: u8,
        day: u8,
        hour: u8,
        minute: u8,
        second: u8,
        micro: u32,
    },
}

impl CellValue {
    pub fn into_py(self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        Ok(match self {
            CellValue::Null => py.None(),
            CellValue::Bool(v) => v.into_pyobject(py)?.to_owned().into_any().unbind(),
            CellValue::I64(v) => v.into_pyobject(py)?.into_any().unbind(),
            CellValue::F64(v) => v.into_pyobject(py)?.into_any().unbind(),
            CellValue::Str(v) => v.into_pyobject(py)?.into_any().unbind(),
            CellValue::Bytes(v) => PyBytes::new(py, &v).into_any().unbind(),
            CellValue::Date { year, month, day } => {
                PyDate::new(py, year, month, day)?.into_any().unbind()
            }
            CellValue::Time {
                hour,
                minute,
                second,
                micro,
            } => PyTime::new(py, hour, minute, second, micro, None)?
                .into_any()
                .unbind(),
            CellValue::DateTime {
                year,
                month,
                day,
                hour,
                minute,
                second,
                micro,
            } => {
                // Clamp like getdata.cpp so out-of-range driver values don't crash.
                let year = year.clamp(1, 9999);
                PyDateTime::new(py, year, month, day, hour, minute, second, micro, None)?
                    .into_any()
                    .unbind()
            }
        })
    }
}

fn succeeded(ret: SqlReturn) -> bool {
    matches!(ret, SqlReturn::SUCCESS | SqlReturn::SUCCESS_WITH_INFO)
}

/// Read a variable-length column via repeated SQLGetData calls.  A direct port of
/// ReadVarColumn in getdata.cpp.  Returns None for SQL NULL.
fn read_var_column(
    hstmt: HStmt,
    col: usize,
    ctype: CDataType,
    column_size: u64,
    initsize: usize,
    on_error: &impl Fn(&'static str) -> PyErr,
) -> PyResult<Option<Vec<u8>>> {
    let wide = ctype == CDataType::WChar;
    let cb_element: usize = if wide { 2 } else { 1 };
    let binary = ctype == CDataType::Binary;
    let cb_null_terminator: usize = if binary { 0 } else { cb_element };

    // Initial allocation, following ReadVarColumn in getdata.cpp: an explicit
    // readvar_initsize wins; 0 means size the buffer from the column descriptor so
    // one SQLGetData call can return everything (some drivers loop forever on
    // smaller buffers), clamped to a sane ceiling for absurd descriptor values.
    let cb_allocated = if initsize == 0 {
        const CEILING: usize = 32 * 1024 * 1024;
        let from_column = (column_size as usize)
            .saturating_mul(cb_element)
            .saturating_add(cb_null_terminator);
        if from_column == 0 || from_column > CEILING {
            CEILING
        } else {
            from_column
        }
    } else {
        // Never smaller than one element plus its terminator.
        initsize.max(cb_element + cb_null_terminator)
    };

    let mut buf: Vec<u8> = vec![0; cb_allocated];
    let mut cb_used: usize = 0;

    loop {
        let cb_available = buf.len() - cb_used;
        let mut cb_data: Len = 0;

        let ret = unsafe {
            odbc_sys::SQLGetData(
                hstmt,
                (col + 1) as u16,
                ctype,
                buf[cb_used..].as_mut_ptr() as Pointer,
                cb_available as Len,
                &mut cb_data,
            )
        };

        if ret == SqlReturn::NO_DATA {
            // No more data; everything was read in previous iterations.
            break;
        }
        if !succeeded(ret) {
            return Err(on_error("SQLGetData"));
        }

        // HACK from getdata.cpp: FreeTDS 0.91 returns -4 for NULL instead of -1;
        // treat all negative lengths on SQL_SUCCESS as NULL.
        if ret == SqlReturn::SUCCESS && cb_data < 0 {
            return Ok(None);
        }
        if cb_data == odbc_sys::NULL_DATA {
            return Ok(None);
        }

        if ret == SqlReturn::SUCCESS_WITH_INFO {
            // More data remains.  SQLGetData sets cb_data to bytes-just-read plus
            // bytes-remaining (or NO_TOTAL); the null terminator occupies buffer
            // space on every read.
            let (cb_read, cb_remaining) = if cb_data == odbc_sys::NO_TOTAL {
                (cb_available.saturating_sub(cb_null_terminator), 1024 * 1024)
            } else if cb_data as usize >= cb_available {
                let read = cb_available.saturating_sub(cb_null_terminator);
                (read, (cb_data as usize).saturating_sub(read))
            } else {
                ((cb_data as usize).saturating_sub(cb_null_terminator), 0)
            };

            cb_used += cb_read;
            if cb_remaining > 0 {
                buf.resize(cb_used + cb_remaining + cb_null_terminator, 0);
                continue;
            }
            break;
        } else {
            // SQL_SUCCESS: final batch; cb_data excludes the terminator.
            cb_used += cb_data as usize;
            break;
        }
    }

    buf.truncate(cb_used);
    Ok(Some(buf))
}

fn utf16_native(bytes: &[u8]) -> String {
    let (pairs, _remainder) = bytes.as_chunks::<2>();
    let units: Vec<u16> = pairs.iter().map(|c| u16::from_ne_bytes(*c)).collect();
    String::from_utf16_lossy(&units)
}

/// Fetch one cell.  Runs on the worker thread, no GIL.  The C-type choices follow
/// the pyodbc defaults: all text is read as SQL_C_WCHAR (UTF-16), matching the
/// default TextEnc configuration in connection.cpp.
pub fn get_data(
    hstmt: HStmt,
    col: usize,
    info: &ColInfo,
    initsize: usize,
    on_error: &impl Fn(&'static str) -> PyErr,
) -> PyResult<CellValue> {
    match info.sql_type {
        SQL_WCHAR | SQL_WVARCHAR | SQL_WLONGVARCHAR | SQL_CHAR | SQL_VARCHAR | SQL_LONGVARCHAR
        | SQL_GUID => match read_var_column(
            hstmt,
            col,
            CDataType::WChar,
            info.column_size,
            initsize,
            on_error,
        )? {
            None => Ok(CellValue::Null),
            Some(bytes) => Ok(CellValue::Str(utf16_native(&bytes))),
        },

        SQL_BINARY | SQL_VARBINARY | SQL_LONGVARBINARY => {
            match read_var_column(
                hstmt,
                col,
                CDataType::Binary,
                info.column_size,
                initsize,
                on_error,
            )? {
                None => Ok(CellValue::Null),
                Some(bytes) => Ok(CellValue::Bytes(bytes)),
            }
        }

        SQL_BIT => {
            let mut value: u8 = 0;
            match get_fixed(hstmt, col, CDataType::Bit, &mut value, 1, on_error)? {
                false => Ok(CellValue::Null),
                true => Ok(CellValue::Bool(value != 0)),
            }
        }

        SQL_TINYINT | SQL_SMALLINT | SQL_INTEGER | SQL_BIGINT => {
            let mut value: i64 = 0;
            match get_fixed(hstmt, col, CDataType::SBigInt, &mut value, 8, on_error)? {
                false => Ok(CellValue::Null),
                true => Ok(CellValue::I64(value)),
            }
        }

        SQL_REAL | SQL_FLOAT | SQL_DOUBLE => {
            let mut value: f64 = 0.0;
            match get_fixed(hstmt, col, CDataType::Double, &mut value, 8, on_error)? {
                false => Ok(CellValue::Null),
                true => Ok(CellValue::F64(value)),
            }
        }

        SQL_DATE | SQL_TYPE_DATE | SQL_TIME | SQL_TYPE_TIME | SQL_TIMESTAMP
        | SQL_TYPE_TIMESTAMP => {
            // Like GetDataTimestamp in getdata.cpp: always fetch as a timestamp
            // struct, then narrow by the column's SQL type.
            let mut value = odbc_sys::Timestamp::default();
            let size = std::mem::size_of::<odbc_sys::Timestamp>();
            match get_fixed(
                hstmt,
                col,
                CDataType::TypeTimestamp,
                &mut value,
                size,
                on_error,
            )? {
                false => Ok(CellValue::Null),
                true => {
                    let micro = value.fraction / 1000; // nanos -> micros
                    Ok(match info.sql_type {
                        SQL_TYPE_TIME | SQL_TIME => CellValue::Time {
                            hour: value.hour as u8,
                            minute: value.minute as u8,
                            second: value.second as u8,
                            micro,
                        },
                        SQL_TYPE_DATE | SQL_DATE => CellValue::Date {
                            year: value.year as i32,
                            month: value.month as u8,
                            day: value.day as u8,
                        },
                        _ => CellValue::DateTime {
                            year: value.year as i32,
                            month: value.month as u8,
                            day: value.day as u8,
                            hour: value.hour as u8,
                            minute: value.minute as u8,
                            second: value.second as u8,
                            micro,
                        },
                    })
                }
            }
        }

        other => Err(ProgrammingError::new_err((
            "HY106".to_string(),
            format!(
                "ODBC SQL type {other} is not yet supported in the Rust port.  column-index={col}"
            ),
        ))),
    }
}

/// SQLGetData for a fixed-size C type.  Returns false if the value was NULL.
fn get_fixed<T>(
    hstmt: HStmt,
    col: usize,
    ctype: CDataType,
    value: &mut T,
    size: usize,
    on_error: &impl Fn(&'static str) -> PyErr,
) -> PyResult<bool> {
    let mut indicator: Len = 0;
    let ret = unsafe {
        odbc_sys::SQLGetData(
            hstmt,
            (col + 1) as u16,
            ctype,
            value as *mut T as Pointer,
            size as Len,
            &mut indicator,
        )
    };
    if !succeeded(ret) {
        return Err(on_error("SQLGetData"));
    }
    Ok(indicator != odbc_sys::NULL_DATA)
}

/// The Python class corresponding to a SQL type, for Cursor.description.  Ported
/// from PythonTypeFromSqlType in getdata.cpp (decimal/uuid handling comes with the
/// phases that implement those types).
pub fn python_type_for_sql_type(py: Python<'_>, sql_type: i16) -> PyResult<Py<PyAny>> {
    use pyo3::type_object::PyTypeInfo;
    use pyo3::types::{PyBool, PyByteArray, PyFloat, PyInt, PyString};

    let ty = match sql_type {
        SQL_CHAR | SQL_VARCHAR | SQL_LONGVARCHAR | SQL_WCHAR | SQL_WVARCHAR | SQL_WLONGVARCHAR
        | SQL_GUID => PyString::type_object(py).into_any(),
        SQL_DECIMAL | SQL_NUMERIC => py.import("decimal")?.getattr("Decimal")?,
        SQL_REAL | SQL_FLOAT | SQL_DOUBLE => PyFloat::type_object(py).into_any(),
        SQL_SMALLINT | SQL_INTEGER | SQL_TINYINT | SQL_BIGINT => PyInt::type_object(py).into_any(),
        SQL_DATE | SQL_TYPE_DATE => py.import("datetime")?.getattr("date")?,
        SQL_TIME | SQL_TYPE_TIME => py.import("datetime")?.getattr("time")?,
        SQL_TIMESTAMP | SQL_TYPE_TIMESTAMP => py.import("datetime")?.getattr("datetime")?,
        SQL_BIT => PyBool::type_object(py).into_any(),
        _ => PyByteArray::type_object(py).into_any(),
    };
    Ok(ty.unbind())
}
