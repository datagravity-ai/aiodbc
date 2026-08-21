// Binding Python parameter values into SQL statements.  Ported (simplified) from
// src/params.cpp.  Covers the scalar types plus Decimal and UUID; strings are
// encoded per the connection's unicode write encoding, and values longer than
// Connection.maxwrite are bound as data-at-execution (streamed via SQLPutData).
// TVPs and fast_executemany come with phase 4.

use odbc_sys::{
    CDataType, HStmt, Len, Nullability, ParamType, Pointer, SqlDataType, SqlReturn, ULen,
};
use pyo3::prelude::*;
use pyo3::types::{
    PyBool, PyByteArray, PyBytes, PyDate, PyDateTime, PyFloat, PyInt, PyString, PyTime,
};

use crate::decimal_support;
use crate::errors::ProgrammingError;
use crate::textenc::{self, TextEnc};

// SQL_DATA_AT_EXEC / SQL_LEN_DATA_AT_EXEC(length) from sqlext.h.
const SQL_DATA_AT_EXEC: Len = -2;
const SQL_LEN_DATA_AT_EXEC_OFFSET: Len = -100;

/// A parameter value extracted from Python, safe to move to the worker thread.
pub enum ParamValue {
    Null,
    Bool(bool),
    I32(i32),
    I64(i64),
    F64(f64),
    /// Encoded per the connection's unicode write encoding.
    Text {
        bytes: Vec<u8>,
        wide: bool,
        /// ColumnSize, in characters.
        chars: usize,
    },
    Bytes(Vec<u8>),
    Decimal {
        /// Plain "-123.45"-style string, always '.'-separated.
        text: String,
        precision: u64,
        scale: i16,
    },
    Uuid([u8; 16]), // bytes_le, matching the SQLGUID memory layout
    Date(odbc_sys::Date),
    Time(odbc_sys::Time),
    DateTime(odbc_sys::Timestamp),
}

fn invalid_type_err(index: usize, type_name: &str) -> PyErr {
    // Matches params.cpp: RaiseErrorV("HY105", ProgrammingError, ...)
    ProgrammingError::new_err((
        "HY105".to_string(),
        format!("Invalid parameter type.  param-index={index} param-type={type_name}"),
    ))
}

/// Extract one parameter cell.  The type-check order mirrors GetParameterInfo in
/// params.cpp (bool before int, datetime before date).  Runs under the GIL.
pub fn extract(
    cell: &Bound<'_, PyAny>,
    index: usize,
    unicode_enc: &TextEnc,
) -> PyResult<ParamValue> {
    let py = cell.py();
    if cell.is_none() {
        return Ok(ParamValue::Null);
    }
    if cell.is_instance_of::<PyBool>() {
        return Ok(ParamValue::Bool(cell.extract::<bool>()?));
    }
    if cell.is_instance_of::<PyInt>() {
        let value: i64 = cell.extract()?;
        // Like params.cpp, use INTEGER when possible since some drivers lack BIGINT.
        if (-2147483647..=2147483647).contains(&value) {
            return Ok(ParamValue::I32(value as i32));
        }
        return Ok(ParamValue::I64(value));
    }
    if cell.is_instance_of::<PyFloat>() {
        return Ok(ParamValue::F64(cell.extract()?));
    }
    if cell.is_instance_of::<PyString>() {
        let s: String = cell.extract()?;
        let bytes = textenc::encode(py, &s, unicode_enc)?;
        let chars = (bytes.len() / textenc::column_size_denominator(unicode_enc)).max(1);
        return Ok(ParamValue::Text {
            bytes,
            wide: unicode_enc.wide,
            chars,
        });
    }
    if cell.is_instance_of::<PyBytes>() || cell.is_instance_of::<PyByteArray>() {
        return Ok(ParamValue::Bytes(cell.extract()?));
    }
    // datetime.datetime is a subclass of datetime.date: check it first.
    if cell.is_instance_of::<PyDateTime>() {
        return Ok(ParamValue::DateTime(odbc_sys::Timestamp {
            year: cell.getattr("year")?.extract()?,
            month: cell.getattr("month")?.extract()?,
            day: cell.getattr("day")?.extract()?,
            hour: cell.getattr("hour")?.extract()?,
            minute: cell.getattr("minute")?.extract()?,
            second: cell.getattr("second")?.extract()?,
            fraction: cell.getattr("microsecond")?.extract::<u32>()? * 1000, // micro -> nano
        }));
    }
    if cell.is_instance_of::<PyDate>() {
        return Ok(ParamValue::Date(odbc_sys::Date {
            year: cell.getattr("year")?.extract()?,
            month: cell.getattr("month")?.extract()?,
            day: cell.getattr("day")?.extract()?,
        }));
    }
    if cell.is_instance_of::<PyTime>() {
        return Ok(ParamValue::Time(odbc_sys::Time {
            hour: cell.getattr("hour")?.extract()?,
            minute: cell.getattr("minute")?.extract()?,
            second: cell.getattr("second")?.extract()?,
        }));
    }
    let decimal_cls = py.import("decimal")?.getattr("Decimal")?;
    if cell.is_instance(&decimal_cls)? {
        let (text, precision, scale) = decimal_support::decimal_param(cell)?;
        return Ok(ParamValue::Decimal {
            text,
            precision,
            scale,
        });
    }
    let uuid_cls = py.import("uuid")?.getattr("UUID")?;
    if cell.is_instance(&uuid_cls)? {
        let bytes_le: Vec<u8> = cell.getattr("bytes_le")?.extract()?;
        let mut buf = [0u8; 16];
        buf.copy_from_slice(&bytes_le);
        return Ok(ParamValue::Uuid(buf));
    }
    Err(invalid_type_err(index, cell.get_type().name()?.to_str()?))
}

/// One bound parameter: owns the value buffer and indicator for the duration of the
/// execute.  The boxes keep addresses stable while ODBC holds pointers to them —
/// BoundParam itself is moved (into a Vec) after binding, so nothing ODBC points at
/// may live inline in this struct.
pub struct BoundParam {
    _value: Box<ParamValue>,
    indicator: Box<Len>,
    /// Set when the value is bound as data-at-execution; SQLPutData streams it in
    /// chunks of `chunk` bytes after SQLExecute returns SQL_NEED_DATA.
    dae: bool,
}

impl BoundParam {
    pub fn is_dae(&self) -> bool {
        self.dae
    }

    /// The raw bytes to stream for a DAE parameter.
    pub fn dae_bytes(&self) -> &[u8] {
        match &*self._value {
            ParamValue::Text { bytes, .. } => bytes,
            ParamValue::Bytes(b) => b,
            _ => &[],
        }
    }
}

fn describe_param_type(hstmt: HStmt, index: usize) -> SqlDataType {
    // For None we ask the driver for the parameter's type via SQLDescribeParam,
    // falling back to VARCHAR for drivers that cannot describe (params.cpp
    // GetParamType / GetNullInfo).
    let mut data_type = SqlDataType::UNKNOWN_TYPE;
    let mut size: ULen = 0;
    let mut digits: i16 = 0;
    let mut nullable = Nullability::UNKNOWN;
    let ret = unsafe {
        odbc_sys::SQLDescribeParam(
            hstmt,
            (index + 1) as u16,
            &mut data_type,
            &mut size,
            &mut digits,
            &mut nullable,
        )
    };
    if matches!(ret, SqlReturn::SUCCESS | SqlReturn::SUCCESS_WITH_INFO)
        && data_type != SqlDataType::UNKNOWN_TYPE
    {
        data_type
    } else {
        SqlDataType::VARCHAR
    }
}

/// Bind one parameter (1-based position = index + 1).  Runs on the worker thread.
/// Returns the BoundParam whose buffers must outlive SQLExecute (and the SQLPutData
/// loop for DAE parameters).
pub fn bind(
    hstmt: HStmt,
    index: usize,
    value: ParamValue,
    maxwrite: usize,
    need_long_data_len: bool,
    on_error: impl Fn(&'static str) -> PyErr,
) -> PyResult<BoundParam> {
    let mut bound = BoundParam {
        _value: Box::new(value),
        indicator: Box::new(0),
        dae: false,
    };

    struct BindArgs {
        ctype: CDataType,
        sqltype: SqlDataType,
        column_size: ULen,
        decimal_digits: i16,
        ptr: Pointer,
        buffer_len: Len,
        indicator: Len,
    }

    // Values longer than maxwrite (when set) are provided at execution time via
    // SQLPutData; the "pointer" becomes a token SQLParamData hands back (we use the
    // 1-based parameter number).
    let dae_indicator = |cb: Len| -> Len {
        if need_long_data_len {
            SQL_LEN_DATA_AT_EXEC_OFFSET - cb // SQL_LEN_DATA_AT_EXEC(cb)
        } else {
            SQL_DATA_AT_EXEC
        }
    };

    let mut is_dae = false;
    let args = match &*bound._value {
        ParamValue::Null => BindArgs {
            ctype: CDataType::Default,
            sqltype: describe_param_type(hstmt, index),
            column_size: 1,
            decimal_digits: 0,
            ptr: std::ptr::null_mut(),
            buffer_len: 0,
            indicator: odbc_sys::NULL_DATA,
        },
        ParamValue::Bool(v) => BindArgs {
            ctype: CDataType::Bit,
            sqltype: SqlDataType::EXT_BIT,
            column_size: 0,
            decimal_digits: 0,
            ptr: v as *const bool as Pointer,
            buffer_len: 1,
            indicator: 1,
        },
        ParamValue::I32(v) => BindArgs {
            ctype: CDataType::SLong,
            sqltype: SqlDataType::INTEGER,
            column_size: 0,
            decimal_digits: 0,
            ptr: v as *const i32 as Pointer,
            buffer_len: 4,
            indicator: 4,
        },
        ParamValue::I64(v) => BindArgs {
            ctype: CDataType::SBigInt,
            sqltype: SqlDataType::EXT_BIG_INT,
            column_size: 0,
            decimal_digits: 0,
            ptr: v as *const i64 as Pointer,
            buffer_len: 8,
            indicator: 8,
        },
        ParamValue::F64(v) => BindArgs {
            ctype: CDataType::Double,
            sqltype: SqlDataType::DOUBLE,
            column_size: 15,
            decimal_digits: 0,
            ptr: v as *const f64 as Pointer,
            buffer_len: 8,
            indicator: 8,
        },
        ParamValue::Text { bytes, wide, chars } => {
            let cb = bytes.len() as Len;
            let (ctype, varchar, longvarchar) = if *wide {
                (
                    CDataType::WChar,
                    SqlDataType::EXT_W_VARCHAR,
                    SqlDataType::EXT_W_LONG_VARCHAR,
                )
            } else {
                (
                    CDataType::Char,
                    SqlDataType::VARCHAR,
                    SqlDataType::EXT_LONG_VARCHAR,
                )
            };
            if maxwrite != 0 && bytes.len() > maxwrite {
                is_dae = true;
                BindArgs {
                    ctype,
                    sqltype: longvarchar,
                    column_size: (*chars).max(1) as ULen,
                    decimal_digits: 0,
                    ptr: (index + 1) as Pointer,
                    buffer_len: 0,
                    indicator: dae_indicator(cb),
                }
            } else {
                BindArgs {
                    ctype,
                    sqltype: varchar,
                    column_size: (*chars).max(1) as ULen,
                    decimal_digits: 0,
                    ptr: bytes.as_ptr() as Pointer,
                    buffer_len: cb,
                    indicator: cb,
                }
            }
        }
        ParamValue::Bytes(v) => {
            if maxwrite != 0 && v.len() > maxwrite {
                is_dae = true;
                BindArgs {
                    ctype: CDataType::Binary,
                    sqltype: SqlDataType::EXT_LONG_VAR_BINARY,
                    column_size: v.len().max(1) as ULen,
                    decimal_digits: 0,
                    ptr: (index + 1) as Pointer,
                    buffer_len: 0,
                    indicator: dae_indicator(v.len() as Len),
                }
            } else {
                BindArgs {
                    ctype: CDataType::Binary,
                    sqltype: SqlDataType::EXT_VAR_BINARY,
                    column_size: v.len().max(1) as ULen,
                    decimal_digits: 0,
                    ptr: v.as_ptr() as Pointer,
                    buffer_len: v.len() as Len,
                    indicator: v.len() as Len,
                }
            }
        }
        ParamValue::Decimal {
            text,
            precision,
            scale,
        } => BindArgs {
            // Bound as a string: SQL_NUMERIC_STRUCT input is unreliable across
            // drivers (params.cpp GetDecimalInfo).
            ctype: CDataType::Char,
            sqltype: SqlDataType::NUMERIC,
            column_size: *precision as ULen,
            decimal_digits: *scale,
            ptr: text.as_ptr() as Pointer,
            buffer_len: text.len() as Len,
            indicator: text.len() as Len,
        },
        ParamValue::Uuid(v) => BindArgs {
            ctype: CDataType::Guid,
            sqltype: SqlDataType::EXT_GUID,
            column_size: 16,
            decimal_digits: 0,
            ptr: v.as_ptr() as Pointer,
            buffer_len: 16,
            indicator: 16,
        },
        ParamValue::Date(v) => BindArgs {
            ctype: CDataType::TypeDate,
            sqltype: SqlDataType::DATE,
            column_size: 10,
            decimal_digits: 0,
            ptr: v as *const odbc_sys::Date as Pointer,
            buffer_len: std::mem::size_of::<odbc_sys::Date>() as Len,
            indicator: std::mem::size_of::<odbc_sys::Date>() as Len,
        },
        ParamValue::Time(v) => BindArgs {
            ctype: CDataType::TypeTime,
            sqltype: SqlDataType::TIME,
            column_size: 8,
            decimal_digits: 0,
            ptr: v as *const odbc_sys::Time as Pointer,
            buffer_len: std::mem::size_of::<odbc_sys::Time>() as Len,
            indicator: std::mem::size_of::<odbc_sys::Time>() as Len,
        },
        ParamValue::DateTime(v) => BindArgs {
            // TODO(phase 4): trim the fraction to the driver's datetime precision
            // (CnxnInfo), as params.cpp GetDateTimeInfo does for SQL Server.
            ctype: CDataType::TypeTimestamp,
            sqltype: SqlDataType::TIMESTAMP,
            column_size: 26,
            decimal_digits: 6,
            ptr: v as *const odbc_sys::Timestamp as Pointer,
            buffer_len: std::mem::size_of::<odbc_sys::Timestamp>() as Len,
            indicator: std::mem::size_of::<odbc_sys::Timestamp>() as Len,
        },
    };

    bound.dae = is_dae;
    *bound.indicator = args.indicator;

    let ret = unsafe {
        odbc_sys::SQLBindParameter(
            hstmt,
            (index + 1) as u16,
            ParamType::Input,
            args.ctype,
            args.sqltype,
            args.column_size,
            args.decimal_digits,
            args.ptr,
            args.buffer_len,
            &mut *bound.indicator,
        )
    };
    if !matches!(ret, SqlReturn::SUCCESS | SqlReturn::SUCCESS_WITH_INFO) {
        return Err(on_error("SQLBindParameter"));
    }
    Ok(bound)
}
