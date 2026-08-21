// Binding Python parameter values into SQL statements.  Ported (simplified) from
// src/params.cpp.  Covers the scalar types plus Decimal and UUID; strings are
// encoded per the connection's unicode write encoding, and values longer than
// Connection.maxwrite are bound as data-at-execution (streamed via SQLPutData).
// TVPs and fast_executemany come with phase 4.

use std::cell::Cell;

use odbc_sys::{
    CDataType, HDesc, HStmt, Len, Nullability, ParamType, Pointer, SqlDataType, SqlReturn, ULen,
};

// SQL Server-specific statement/descriptor fields for TVPs (pyodbc.h).
const SQL_SOPT_SS_PARAM_FOCUS: i32 = 1236;
const SQL_CA_SS_TYPE_NAME: u16 = 1227;
const SQL_CA_SS_SCHEMA_NAME: u16 = 1226;
const SQL_SS_TABLE: i16 = -153;

extern "system" {
    #[link_name = "SQLSetStmtAttr"]
    fn RawSQLSetStmtAttr(
        hstmt: HStmt,
        attribute: i32,
        value: Pointer,
        string_length: i32,
    ) -> SqlReturn;
    #[link_name = "SQLSetDescFieldW"]
    fn RawSQLSetDescFieldW(
        hdesc: HDesc,
        rec_number: i16,
        field_identifier: u16,
        value: Pointer,
        buffer_length: i32,
    ) -> SqlReturn;
}
use pyo3::prelude::*;
use pyo3::types::{
    PyBool, PyByteArray, PyBytes, PyDate, PyDateTime, PyFloat, PyInt, PyString, PyTime,
};

use crate::decimal_support;
use crate::errors::ProgrammingError;
use crate::textenc::{self, TextEnc};
use crate::worker::CnxnInfo;

/// Everything the binder needs to decide direct-vs-DAE and precision handling.
pub struct ParamLimits {
    pub maxwrite: usize,
    pub cnxninfo: CnxnInfo,
}

impl ParamLimits {
    /// GetMaxLength in connection.h: an explicit maxwrite wins, otherwise the
    /// driver's maximum for the C type.
    fn max_length(&self, wide_text: bool, binary: bool) -> usize {
        if self.maxwrite != 0 {
            self.maxwrite
        } else if binary {
            self.cnxninfo.binary_maxlength
        } else if wide_text {
            self.cnxninfo.wvarchar_maxlength
        } else {
            self.cnxninfo.varchar_maxlength
        }
    }
}

/// A per-parameter override from Cursor.setinputsizes.
#[derive(Clone, Default)]
pub struct InputSize {
    pub sqltype: Option<i16>,
    pub column_size: Option<u64>,
    pub scale: Option<i16>,
}

// SQL_DATA_AT_EXEC / SQL_LEN_DATA_AT_EXEC(length) from sqlext.h.
const SQL_DATA_AT_EXEC: Len = -2;
const SQL_DEFAULT_PARAM: Len = -5;
const SQL_LEN_DATA_AT_EXEC_OFFSET: Len = -100;

/// A parameter value extracted from Python, safe to move to the worker thread.
pub enum ParamValue {
    Null,
    NullBinary,
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
    /// SQL Server table-valued parameter: optional type/schema names plus rows of
    /// scalar cells.
    Tvp {
        type_name: Option<String>,
        schema: Option<String>,
        rows: Vec<Vec<ParamValue>>,
    },
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
    extract_impl(cell, index, unicode_enc, false)
}

fn extract_impl(
    cell: &Bound<'_, PyAny>,
    index: usize,
    unicode_enc: &TextEnc,
    in_tvp: bool,
) -> PyResult<ParamValue> {
    let py = cell.py();
    if cell.is_none() {
        return Ok(ParamValue::Null);
    }
    // The BinaryNull sentinel distinguishes binary NULLs from char NULLs when the
    // driver cannot describe parameters (GetNullBinaryInfo in params.cpp).
    if let Ok(bn) = py.import("pyodbc").and_then(|m| m.getattr("BinaryNull")) {
        if cell.is(&bn) {
            return Ok(ParamValue::NullBinary);
        }
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
    // A sequence parameter is a SQL Server table-valued parameter (GetTableInfo in
    // params.cpp).  Leading string elements name the table type and its schema.
    if !in_tvp
        && (cell.is_instance_of::<pyo3::types::PyList>()
            || cell.is_instance_of::<pyo3::types::PyTuple>()
            || cell.is_instance_of::<crate::row::Row>())
    {
        let items: Vec<Bound<'_, PyAny>> = cell.try_iter()?.collect::<PyResult<Vec<_>>>()?;
        let mut type_name = None;
        let mut schema = None;
        let mut start = 0;
        if let Some(first) = items.first() {
            if let Ok(s) = first.extract::<String>() {
                type_name = Some(s);
                start = 1;
                if let Some(second) = items.get(1) {
                    if let Ok(s) = second.extract::<String>() {
                        schema = Some(s);
                        start = 2;
                    }
                }
            }
        }
        let mut rows = Vec::with_capacity(items.len().saturating_sub(start));
        for row in &items[start..] {
            let mut cells = Vec::new();
            for (i, c) in row.try_iter()?.enumerate() {
                cells.push(extract_impl(&c?, i, unicode_enc, true)?);
            }
            rows.push(cells);
        }
        return Ok(ParamValue::Tvp {
            type_name,
            schema,
            rows,
        });
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
    /// chunks of `dae_chunk` bytes after SQLExecute returns SQL_NEED_DATA.
    dae: bool,
    dae_chunk: usize,
    /// Streaming state for a table-valued parameter.
    tvp: Option<TvpBound>,
}

impl BoundParam {
    pub fn is_dae(&self) -> bool {
        self.dae
    }

    pub fn tvp(&self) -> Option<&TvpBound> {
        self.tvp.as_ref()
    }

    /// The ParamData token for this parameter when it is a TVP (the address of its
    /// outer indicator, which was stored in the APD's DATA_PTR).
    pub fn tvp_token(&self) -> usize {
        std::ptr::from_ref::<Len>(&*self.indicator) as usize
    }

    /// SQLPutData chunk size for a DAE parameter (GetMaxLength in connection.h).
    pub fn dae_chunk(&self) -> usize {
        self.dae_chunk.max(1)
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

pub fn describe_param_type(hstmt: HStmt, index: usize) -> SqlDataType {
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
    limits: &ParamLimits,
    size_override: Option<&InputSize>,
    described_type: Option<SqlDataType>,
    on_error: impl Fn(&'static str) -> PyErr,
) -> PyResult<BoundParam> {
    let need_long_data_len = limits.cnxninfo.need_long_data_len;

    let value = match value {
        ParamValue::Tvp {
            type_name,
            schema,
            rows,
        } => return bind_tvp(hstmt, index, type_name, schema, rows, limits, &on_error),
        other => other,
    };
    let mut value = value;
    // Trim datetime fractions up front (the match below borrows immutably).
    let mut dt_decimal_digits: i16 = 0;
    if let ParamValue::DateTime(ts) = &mut value {
        let precision = limits.cnxninfo.datetime_precision - 20; // 20 = date + '.'
        if precision <= 0 {
            ts.fraction = 0;
        } else {
            let keep = 10u32.pow(9 - precision.min(9) as u32);
            ts.fraction = ts.fraction / keep * keep;
            dt_decimal_digits = precision as i16;
        }
    }

    let mut bound = BoundParam {
        _value: Box::new(value),
        indicator: Box::new(0),
        dae: false,
        dae_chunk: 0,
        tvp: None,
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
            // Described before any binds (SQLDescribeParam misreports once
            // SQLBindParameter has been called; see test_none_param).
            sqltype: described_type.unwrap_or(SqlDataType::VARCHAR),
            column_size: 1,
            decimal_digits: 0,
            ptr: std::ptr::null_mut(),
            buffer_len: 0,
            indicator: odbc_sys::NULL_DATA,
        },
        ParamValue::NullBinary => BindArgs {
            ctype: CDataType::Binary,
            sqltype: SqlDataType::EXT_BINARY,
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
            if bytes.len() > limits.max_length(*wide, false) {
                is_dae = true;
                bound.dae_chunk = limits.max_length(*wide, false);
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
            if v.len() > limits.max_length(false, true) {
                is_dae = true;
                bound.dae_chunk = limits.max_length(false, true);
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
        ParamValue::Tvp { .. } => unreachable!("TVPs bound via bind_tvp"),
        ParamValue::DateTime(v) => BindArgs {
            // Fraction already trimmed above (GetDateTimeInfo in params.cpp).
            ctype: CDataType::TypeTimestamp,
            sqltype: SqlDataType::TIMESTAMP,
            column_size: limits.cnxninfo.datetime_precision.max(19) as ULen,
            decimal_digits: dt_decimal_digits,
            ptr: v as *const odbc_sys::Timestamp as Pointer,
            buffer_len: std::mem::size_of::<odbc_sys::Timestamp>() as Len,
            indicator: std::mem::size_of::<odbc_sys::Timestamp>() as Len,
        },
    };

    let mut args = args;
    if let Some(over) = size_override {
        if let Some(t) = over.sqltype {
            args.sqltype = SqlDataType(t);
        }
        if let Some(s) = over.column_size {
            args.column_size = s as ULen;
        }
        if let Some(sc) = over.scale {
            args.decimal_digits = sc;
        }
    }

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

// ---------------------------------------------------------------------------
// fast_executemany: column-wise parameter arrays
// ---------------------------------------------------------------------------

/// One bound parameter array column.  The heap buffers must stay alive (and the
/// struct unmoved is not required - Vec contents are heap-stable) until SQLExecute
/// completes.
enum ColumnStore {
    I64(Vec<i64>),
    F64(Vec<f64>),
    Bit(Vec<u8>),
    Date(Vec<odbc_sys::Date>),
    Time(Vec<odbc_sys::Time>),
    Ts(Vec<odbc_sys::Timestamp>),
    /// Variable-length cells packed at a fixed stride (the stride is carried in
    /// the matching ColBind's buffer_len).
    Var(Vec<u8>),
}

pub struct BoundColumn {
    store: ColumnStore,
    indicators: Vec<Len>,
}

#[derive(Clone, Copy, PartialEq)]
enum ColKind {
    I64,
    F64,
    Bool,
    Date,
    Time,
    Ts,
    Text { wide: bool },
    Bytes,
}

fn cell_kind(v: &ParamValue) -> Option<Option<ColKind>> {
    // Outer None = type unsupported in the fast path; inner None = NULL cell.
    Some(match v {
        ParamValue::Null => None,
        ParamValue::Bool(_) => Some(ColKind::Bool),
        ParamValue::I32(_) | ParamValue::I64(_) => Some(ColKind::I64),
        ParamValue::F64(_) => Some(ColKind::F64),
        ParamValue::Date(_) => Some(ColKind::Date),
        ParamValue::Time(_) => Some(ColKind::Time),
        ParamValue::DateTime(_) => Some(ColKind::Ts),
        ParamValue::Text { wide, .. } => Some(ColKind::Text { wide: *wide }),
        ParamValue::Bytes(_) => Some(ColKind::Bytes),
        _ => return None,
    })
}

/// Bind all rows as column-wise parameter arrays (fast_executemany).  Returns
/// Ok(None) when the data doesn't fit the fast path (mixed/unsupported types,
/// all-NULL columns, values needing data-at-execution) so the caller can fall back
/// to the per-row loop.  On success the statement's PARAMSET_SIZE is set to the
/// row count; the caller must reset it to 1 after executing.
pub fn bind_parameter_arrays(
    hstmt: HStmt,
    rows: &[Vec<ParamValue>],
    limits: &ParamLimits,
    on_error: impl Fn(&'static str) -> PyErr,
) -> PyResult<Option<Vec<BoundColumn>>> {
    let nrows = rows.len();
    let ncols = rows[0].len();
    if rows.iter().any(|r| r.len() != ncols) {
        return Ok(None);
    }

    // Classify each column.
    let mut kinds: Vec<Option<ColKind>> = vec![None; ncols];
    for row in rows {
        for (c, cell) in row.iter().enumerate() {
            let Some(k) = cell_kind(cell) else {
                return Ok(None);
            };
            match (kinds[c], k) {
                (_, None) => {}
                (None, Some(k)) => kinds[c] = Some(k),
                (Some(a), Some(b)) if a == b => {}
                _ => return Ok(None), // mixed types in one column
            }
        }
    }
    if kinds.iter().any(|k| k.is_none()) {
        return Ok(None); // an all-NULL column: let the loop describe it per row
    }

    let dt_precision = limits.cnxninfo.datetime_precision;
    let mut columns: Vec<BoundColumn> = Vec::with_capacity(ncols);

    struct ColBind {
        ctype: CDataType,
        sqltype: SqlDataType,
        column_size: ULen,
        decimal_digits: i16,
        buffer_len: Len,
    }

    let mut binds: Vec<ColBind> = Vec::with_capacity(ncols);

    for c in 0..ncols {
        let kind = kinds[c].unwrap();
        let mut indicators: Vec<Len> = Vec::with_capacity(nrows);

        let (store, bind) = match kind {
            ColKind::I64 => {
                let mut data = Vec::with_capacity(nrows);
                for row in rows {
                    let (v, ind) = match &row[c] {
                        ParamValue::I32(v) => (*v as i64, 8),
                        ParamValue::I64(v) => (*v, 8),
                        _ => (0, odbc_sys::NULL_DATA),
                    };
                    data.push(v);
                    indicators.push(ind);
                }
                (
                    ColumnStore::I64(data),
                    ColBind {
                        ctype: CDataType::SBigInt,
                        sqltype: SqlDataType::EXT_BIG_INT,
                        column_size: 0,
                        decimal_digits: 0,
                        buffer_len: 8,
                    },
                )
            }
            ColKind::F64 => {
                let mut data = Vec::with_capacity(nrows);
                for row in rows {
                    let (v, ind) = match &row[c] {
                        ParamValue::F64(v) => (*v, 8),
                        _ => (0.0, odbc_sys::NULL_DATA),
                    };
                    data.push(v);
                    indicators.push(ind);
                }
                (
                    ColumnStore::F64(data),
                    ColBind {
                        ctype: CDataType::Double,
                        sqltype: SqlDataType::DOUBLE,
                        column_size: 15,
                        decimal_digits: 0,
                        buffer_len: 8,
                    },
                )
            }
            ColKind::Bool => {
                let mut data = Vec::with_capacity(nrows);
                for row in rows {
                    let (v, ind) = match &row[c] {
                        ParamValue::Bool(v) => (*v as u8, 1),
                        _ => (0, odbc_sys::NULL_DATA),
                    };
                    data.push(v);
                    indicators.push(ind);
                }
                (
                    ColumnStore::Bit(data),
                    ColBind {
                        ctype: CDataType::Bit,
                        sqltype: SqlDataType::EXT_BIT,
                        column_size: 0,
                        decimal_digits: 0,
                        buffer_len: 1,
                    },
                )
            }
            ColKind::Date => {
                let mut data = Vec::with_capacity(nrows);
                for row in rows {
                    let (v, ind) = match &row[c] {
                        ParamValue::Date(v) => (*v, std::mem::size_of::<odbc_sys::Date>() as Len),
                        _ => (odbc_sys::Date::default(), odbc_sys::NULL_DATA),
                    };
                    data.push(v);
                    indicators.push(ind);
                }
                (
                    ColumnStore::Date(data),
                    ColBind {
                        ctype: CDataType::TypeDate,
                        sqltype: SqlDataType::DATE,
                        column_size: 10,
                        decimal_digits: 0,
                        buffer_len: std::mem::size_of::<odbc_sys::Date>() as Len,
                    },
                )
            }
            ColKind::Time => {
                let mut data = Vec::with_capacity(nrows);
                for row in rows {
                    let (v, ind) = match &row[c] {
                        ParamValue::Time(v) => (*v, std::mem::size_of::<odbc_sys::Time>() as Len),
                        _ => (odbc_sys::Time::default(), odbc_sys::NULL_DATA),
                    };
                    data.push(v);
                    indicators.push(ind);
                }
                (
                    ColumnStore::Time(data),
                    ColBind {
                        ctype: CDataType::TypeTime,
                        sqltype: SqlDataType::TIME,
                        column_size: 8,
                        decimal_digits: 0,
                        buffer_len: std::mem::size_of::<odbc_sys::Time>() as Len,
                    },
                )
            }
            ColKind::Ts => {
                let precision = dt_precision - 20;
                let mut decimal_digits: i16 = 0;
                let mut data = Vec::with_capacity(nrows);
                for row in rows {
                    let (mut v, ind) = match &row[c] {
                        ParamValue::DateTime(v) => {
                            (*v, std::mem::size_of::<odbc_sys::Timestamp>() as Len)
                        }
                        _ => (odbc_sys::Timestamp::default(), odbc_sys::NULL_DATA),
                    };
                    if precision <= 0 {
                        v.fraction = 0;
                    } else {
                        let keep = 10u32.pow(9 - precision.min(9) as u32);
                        v.fraction = v.fraction / keep * keep;
                        decimal_digits = precision as i16;
                    }
                    data.push(v);
                    indicators.push(ind);
                }
                (
                    ColumnStore::Ts(data),
                    ColBind {
                        ctype: CDataType::TypeTimestamp,
                        sqltype: SqlDataType::TIMESTAMP,
                        column_size: dt_precision.max(19) as ULen,
                        decimal_digits,
                        buffer_len: std::mem::size_of::<odbc_sys::Timestamp>() as Len,
                    },
                )
            }
            ColKind::Text { wide } => {
                let max_bytes = rows
                    .iter()
                    .filter_map(|row| match &row[c] {
                        ParamValue::Text { bytes, .. } => Some(bytes.len()),
                        _ => None,
                    })
                    .max()
                    .unwrap_or(0);
                if max_bytes > limits.max_length(wide, false) {
                    return Ok(None); // needs DAE: fall back to the loop
                }
                let elem = if wide { 2 } else { 1 };
                let cell = max_bytes.max(elem);
                let mut buf = vec![0u8; cell * nrows];
                let mut max_chars = 1usize;
                for (r, row) in rows.iter().enumerate() {
                    match &row[c] {
                        ParamValue::Text { bytes, chars, .. } => {
                            buf[r * cell..r * cell + bytes.len()].copy_from_slice(bytes);
                            indicators.push(bytes.len() as Len);
                            max_chars = max_chars.max(*chars);
                        }
                        _ => indicators.push(odbc_sys::NULL_DATA),
                    }
                }
                let (ctype, sqltype) = if wide {
                    (CDataType::WChar, SqlDataType::EXT_W_VARCHAR)
                } else {
                    (CDataType::Char, SqlDataType::VARCHAR)
                };
                (
                    ColumnStore::Var(buf),
                    ColBind {
                        ctype,
                        sqltype,
                        column_size: max_chars as ULen,
                        decimal_digits: 0,
                        buffer_len: cell as Len,
                    },
                )
            }
            ColKind::Bytes => {
                let max_bytes = rows
                    .iter()
                    .filter_map(|row| match &row[c] {
                        ParamValue::Bytes(b) => Some(b.len()),
                        _ => None,
                    })
                    .max()
                    .unwrap_or(0);
                if max_bytes > limits.max_length(false, true) {
                    return Ok(None);
                }
                let cell = max_bytes.max(1);
                let mut buf = vec![0u8; cell * nrows];
                for (r, row) in rows.iter().enumerate() {
                    match &row[c] {
                        ParamValue::Bytes(b) => {
                            buf[r * cell..r * cell + b.len()].copy_from_slice(b);
                            indicators.push(b.len() as Len);
                        }
                        _ => indicators.push(odbc_sys::NULL_DATA),
                    }
                }
                (
                    ColumnStore::Var(buf),
                    ColBind {
                        ctype: CDataType::Binary,
                        sqltype: SqlDataType::EXT_VAR_BINARY,
                        column_size: cell as ULen,
                        decimal_digits: 0,
                        buffer_len: cell as Len,
                    },
                )
            }
        };

        columns.push(BoundColumn { store, indicators });
        binds.push(bind);
    }

    // Column-wise binding with a paramset covering every row.
    let set_attr = |attr: odbc_sys::StatementAttribute, value: usize| unsafe {
        odbc_sys::SQLSetStmtAttr(hstmt, attr, value as Pointer, 0)
    };
    if !matches!(
        set_attr(odbc_sys::StatementAttribute::ParamBindType, 0),
        SqlReturn::SUCCESS | SqlReturn::SUCCESS_WITH_INFO
    ) {
        return Err(on_error("SQLSetStmtAttr(SQL_ATTR_PARAM_BIND_TYPE)"));
    }
    if !matches!(
        set_attr(odbc_sys::StatementAttribute::ParamsetSize, nrows),
        SqlReturn::SUCCESS | SqlReturn::SUCCESS_WITH_INFO
    ) {
        return Err(on_error("SQLSetStmtAttr(SQL_ATTR_PARAMSET_SIZE)"));
    }

    for (c, (col, bind)) in columns.iter_mut().zip(binds.iter()).enumerate() {
        let ptr: Pointer = match &col.store {
            ColumnStore::I64(v) => v.as_ptr() as Pointer,
            ColumnStore::F64(v) => v.as_ptr() as Pointer,
            ColumnStore::Bit(v) => v.as_ptr() as Pointer,
            ColumnStore::Date(v) => v.as_ptr() as Pointer,
            ColumnStore::Time(v) => v.as_ptr() as Pointer,
            ColumnStore::Ts(v) => v.as_ptr() as Pointer,
            ColumnStore::Var(buf) => buf.as_ptr() as Pointer,
        };
        let ret = unsafe {
            odbc_sys::SQLBindParameter(
                hstmt,
                (c + 1) as u16,
                ParamType::Input,
                bind.ctype,
                bind.sqltype,
                bind.column_size,
                bind.decimal_digits,
                ptr,
                bind.buffer_len,
                col.indicators.as_mut_ptr(),
            )
        };
        if !matches!(ret, SqlReturn::SUCCESS | SqlReturn::SUCCESS_WITH_INFO) {
            return Err(on_error("SQLBindParameter"));
        }
    }

    Ok(Some(columns))
}

/// Reset the paramset size to 1 after a fast_executemany batch.
pub fn reset_paramset_size(hstmt: HStmt) {
    unsafe {
        let _ = odbc_sys::SQLSetStmtAttr(
            hstmt,
            odbc_sys::StatementAttribute::ParamsetSize,
            1 as Pointer,
            0,
        );
    }
}

// ---------------------------------------------------------------------------
// Table-valued parameters (SQL Server)
// ---------------------------------------------------------------------------

extern "system" {
    #[link_name = "SQLGetStmtAttr"]
    fn RawSQLGetStmtAttr(
        hstmt: HStmt,
        attribute: i32,
        value: Pointer,
        buffer_length: i32,
        string_length: *mut i32,
    ) -> SqlReturn;
}

const SQL_ATTR_APP_PARAM_DESC: i32 = 10011;
const SQL_ATTR_IMP_PARAM_DESC: i32 = 10013;
const SQL_DESC_DATA_PTR: u16 = 1010;

/// The scalar kind of one TVP column, unified across rows.
#[derive(Clone, Copy, PartialEq)]
enum TvpColKind {
    Text {
        wide: bool,
    },
    Bytes,
    I64,
    F64,
    Bool,
    Date,
    Time,
    Ts,
    Decimal,
    Uuid,
    /// Every value in the column was None.
    NullOnly,
}

fn tvp_kind(v: &ParamValue) -> PyResult<Option<TvpColKind>> {
    Ok(match v {
        ParamValue::Null | ParamValue::NullBinary => None,
        ParamValue::Text { wide, .. } => Some(TvpColKind::Text { wide: *wide }),
        ParamValue::Bytes(_) => Some(TvpColKind::Bytes),
        ParamValue::I32(_) | ParamValue::I64(_) => Some(TvpColKind::I64),
        ParamValue::F64(_) => Some(TvpColKind::F64),
        ParamValue::Bool(_) => Some(TvpColKind::Bool),
        ParamValue::Date(_) => Some(TvpColKind::Date),
        ParamValue::Time(_) => Some(TvpColKind::Time),
        ParamValue::DateTime(_) => Some(TvpColKind::Ts),
        ParamValue::Decimal { .. } => Some(TvpColKind::Decimal),
        ParamValue::Uuid(_) => Some(TvpColKind::Uuid),
        ParamValue::Tvp { .. } => return Err(ProgrammingError::new_err("TVPs cannot be nested")),
    })
}

/// Worker-side state for one bound TVP: the row data plus the tokens/indicators
/// the driver refers back to while streaming.
pub struct TvpBound {
    rows: Vec<Vec<ParamValue>>,
    kinds: Vec<TvpColKind>,
    /// Next row to hand to the driver.
    cur: Cell<usize>,
    /// The row whose columns are currently being requested.
    active: Cell<usize>,
    /// Per-column ParamData tokens.  Boxed on purpose (not clippy's Vec<Len>):
    /// each Box's heap address is the token registered with the driver, so the
    /// addresses must survive any Vec reallocation.
    #[allow(clippy::vec_box)]
    tokens: Vec<Box<Len>>,
    #[allow(clippy::vec_box)]
    _indicators: Vec<Box<Len>>,
    datetime_precision: i32,
}

impl TvpBound {
    /// Handle SQLParamData returning the TVP itself: present the next row (or
    /// end-of-rows) via SQLPutData.
    pub fn advance_row(&self, hstmt: HStmt) -> SqlReturn {
        if self.cur.get() < self.rows.len() {
            self.active.set(self.cur.get());
            self.cur.set(self.cur.get() + 1);
            unsafe { odbc_sys::SQLPutData(hstmt, 1 as Pointer, 1) }
        } else {
            unsafe { odbc_sys::SQLPutData(hstmt, std::ptr::null_mut(), 0) }
        }
    }

    /// The column whose token address equals `token`, if any.
    pub fn find_column(&self, token: usize) -> Option<usize> {
        self.tokens
            .iter()
            .position(|t| std::ptr::from_ref::<Len>(&**t) as usize == token)
    }

    /// Supply the active row's value for one column via SQLPutData.
    pub fn put_column(
        &self,
        hstmt: HStmt,
        col: usize,
        on_error: &impl Fn(&'static str) -> PyErr,
    ) -> PyResult<()> {
        let cell = &self.rows[self.active.get()][col];
        if tvp_kind(cell)?.is_some_and(|k| k != self.kinds[col]) {
            return Err(ProgrammingError::new_err(
                "Type mismatch between TVP row values",
            ));
        }

        // Fixed-size temporaries live across the SQLPutData call only; ODBC copies
        // DAE data during the call.
        let mut i64_tmp: i64;
        let mut f64_tmp: f64;
        let mut bit_tmp: u8;
        let mut ts_tmp: odbc_sys::Timestamp;

        let (ptr, len): (Pointer, Len) = match cell {
            ParamValue::Null | ParamValue::NullBinary => {
                (std::ptr::null_mut(), odbc_sys::NULL_DATA)
            }
            ParamValue::Text { bytes, .. } => (bytes.as_ptr() as Pointer, bytes.len() as Len),
            ParamValue::Bytes(b) => (b.as_ptr() as Pointer, b.len() as Len),
            ParamValue::Decimal { text, .. } => (text.as_ptr() as Pointer, text.len() as Len),
            ParamValue::Uuid(b) => (b.as_ptr() as Pointer, 16),
            ParamValue::I32(v) => {
                i64_tmp = *v as i64;
                (&mut i64_tmp as *mut i64 as Pointer, 8)
            }
            ParamValue::I64(v) => {
                i64_tmp = *v;
                (&mut i64_tmp as *mut i64 as Pointer, 8)
            }
            ParamValue::F64(v) => {
                f64_tmp = *v;
                (&mut f64_tmp as *mut f64 as Pointer, 8)
            }
            ParamValue::Bool(v) => {
                bit_tmp = *v as u8;
                (&mut bit_tmp as *mut u8 as Pointer, 1)
            }
            ParamValue::Date(v) => (
                v as *const odbc_sys::Date as Pointer,
                std::mem::size_of::<odbc_sys::Date>() as Len,
            ),
            ParamValue::Time(v) => (
                v as *const odbc_sys::Time as Pointer,
                std::mem::size_of::<odbc_sys::Time>() as Len,
            ),
            ParamValue::DateTime(v) => {
                ts_tmp = *v;
                let precision = self.datetime_precision - 20;
                if precision <= 0 {
                    ts_tmp.fraction = 0;
                } else {
                    let keep = 10u32.pow(9 - precision.min(9) as u32);
                    ts_tmp.fraction = ts_tmp.fraction / keep * keep;
                }
                (
                    &mut ts_tmp as *mut odbc_sys::Timestamp as Pointer,
                    std::mem::size_of::<odbc_sys::Timestamp>() as Len,
                )
            }
            ParamValue::Tvp { .. } => unreachable!("nested TVPs rejected at extract"),
        };

        let ret = unsafe { odbc_sys::SQLPutData(hstmt, ptr, len) };
        if !matches!(ret, SqlReturn::SUCCESS | SqlReturn::SUCCESS_WITH_INFO) {
            return Err(on_error("SQLPutData"));
        }
        Ok(())
    }
}

/// Bind a table-valued parameter (BindTVPColumns in params.cpp): the outer
/// SQL_SS_TABLE parameter is data-at-execution with its rows streamed via
/// SQLPutData, and the columns are bound under SQL_SOPT_SS_PARAM_FOCUS.
fn bind_tvp(
    hstmt: HStmt,
    index: usize,
    type_name: Option<String>,
    schema: Option<String>,
    rows: Vec<Vec<ParamValue>>,
    limits: &ParamLimits,
    on_error: &impl Fn(&'static str) -> PyErr,
) -> PyResult<BoundParam> {
    let ok = |ret: SqlReturn| matches!(ret, SqlReturn::SUCCESS | SqlReturn::SUCCESS_WITH_INFO);

    let ncols = rows.first().map(|r| r.len()).unwrap_or(0);
    if rows.iter().any(|r| r.len() != ncols) {
        return Err(ProgrammingError::new_err(
            "A TVP's rows must all be the same size.",
        ));
    }

    // Without an explicit type name, describing the parameter makes the driver
    // fill the IPD with the TVP's type (GetTableInfo in params.cpp).
    if type_name.is_none() {
        let _ = describe_param_type(hstmt, index);
    }

    let mut outer_indicator: Box<Len> = Box::new(SQL_DATA_AT_EXEC);
    let outer_token = std::ptr::from_mut::<Len>(&mut *outer_indicator) as usize;
    let ret = unsafe {
        odbc_sys::SQLBindParameter(
            hstmt,
            (index + 1) as u16,
            ParamType::Input,
            CDataType::Binary,
            SqlDataType(SQL_SS_TABLE),
            rows.len() as ULen,
            0,
            std::ptr::null_mut(),
            0,
            &mut *outer_indicator,
        )
    };
    if !ok(ret) {
        return Err(on_error("SQLBindParameter"));
    }

    // The ParamData token for the TVP itself is delivered through the APD's
    // DATA_PTR field (the bind above passed a null pointer).
    let mut apd: HDesc = std::ptr::null_mut();
    let ret = unsafe {
        RawSQLGetStmtAttr(
            hstmt,
            SQL_ATTR_APP_PARAM_DESC,
            &mut apd as *mut HDesc as Pointer,
            0,
            std::ptr::null_mut(),
        )
    };
    if !ok(ret) {
        return Err(on_error("SQLGetStmtAttr"));
    }
    let ret = unsafe {
        RawSQLSetDescFieldW(
            apd,
            (index + 1) as i16,
            SQL_DESC_DATA_PTR,
            outer_token as Pointer,
            0,
        )
    };
    if !ok(ret) {
        return Err(on_error("SQLSetDescField"));
    }

    // Explicit type (and schema) names go into the IPD.
    if let Some(name) = &type_name {
        let mut ipd: HDesc = std::ptr::null_mut();
        let ret = unsafe {
            RawSQLGetStmtAttr(
                hstmt,
                SQL_ATTR_IMP_PARAM_DESC,
                &mut ipd as *mut HDesc as Pointer,
                0,
                std::ptr::null_mut(),
            )
        };
        if !ok(ret) {
            return Err(on_error("SQLGetStmtAttr"));
        }
        let wname: Vec<u16> = name.encode_utf16().collect();
        let ret = unsafe {
            RawSQLSetDescFieldW(
                ipd,
                (index + 1) as i16,
                SQL_CA_SS_TYPE_NAME,
                wname.as_ptr() as Pointer,
                (wname.len() * 2) as i32,
            )
        };
        if !ok(ret) {
            return Err(on_error("SQLSetDescField(SQL_CA_SS_TYPE_NAME)"));
        }
        if let Some(schema) = &schema {
            let wschema: Vec<u16> = schema.encode_utf16().collect();
            let ret = unsafe {
                RawSQLSetDescFieldW(
                    ipd,
                    (index + 1) as i16,
                    SQL_CA_SS_SCHEMA_NAME,
                    wschema.as_ptr() as Pointer,
                    (wschema.len() * 2) as i32,
                )
            };
            if !ok(ret) {
                return Err(on_error("SQLSetDescField(SQL_CA_SS_SCHEMA_NAME)"));
            }
        }
    }

    // An empty TVP is sent as a default parameter, not data-at-execution: the
    // driver crashes at SQLExecute on a zero-row DAE table with no bound columns
    // (BindTVPColumns's "If the TVP is empty we're done").  The indicator is
    // rewritten after the bind above, which registered its address.
    if ncols == 0 {
        *outer_indicator = SQL_DEFAULT_PARAM;
        return Ok(BoundParam {
            _value: Box::new(ParamValue::Null),
            indicator: outer_indicator,
            dae: false,
            dae_chunk: 0,
            tvp: None,
        });
    }

    // Scan every row to determine each column's binding (issue #1481: decimal
    // precision/scale must cover all rows, and NULL-led columns need a later row
    // to reveal their type).
    struct ColBindInfo {
        kind: TvpColKind,
        colsize: ULen,
        ddigits: i16,
        idigits: i64,
    }
    let mut cols: Vec<ColBindInfo> = (0..ncols)
        .map(|_| ColBindInfo {
            kind: TvpColKind::NullOnly,
            colsize: 1,
            ddigits: 0,
            idigits: 0,
        })
        .collect();

    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            let Some(kind) = tvp_kind(cell)? else {
                continue;
            };
            let info = &mut cols[i];
            if info.kind == TvpColKind::NullOnly {
                info.kind = kind;
                match cell {
                    // TVP text/binary columns bind with ColumnSize 0 ("max"), like
                    // GetUnicodeInfo/GetBytesInfo with isTVP=true.
                    ParamValue::Text { .. } => info.colsize = 0,
                    ParamValue::Bytes(_) => info.colsize = 0,
                    ParamValue::DateTime(_) => {
                        info.colsize = limits.cnxninfo.datetime_precision.max(19) as ULen;
                        info.ddigits = (limits.cnxninfo.datetime_precision - 20).clamp(0, 9) as i16;
                    }
                    ParamValue::Uuid(_) => info.colsize = 16,
                    ParamValue::F64(_) => info.colsize = 15,
                    ParamValue::Date(_) => info.colsize = 10,
                    ParamValue::Time(_) => info.colsize = 8,
                    _ => info.colsize = 0,
                }
            } else if info.kind != kind {
                return Err(ProgrammingError::new_err(
                    "Type mismatch between TVP row values",
                ));
            }
            if let ParamValue::Decimal {
                precision, scale, ..
            } = cell
            {
                info.idigits = info.idigits.max(*precision as i64 - *scale as i64);
                info.ddigits = info.ddigits.max(*scale);
            }
        }
    }
    for info in cols.iter_mut() {
        if info.kind == TvpColKind::Decimal {
            info.colsize = (info.idigits + info.ddigits as i64).max(1) as ULen;
        }
    }

    // Bind the TVP's columns with the parameter focus pointing at it.
    const SQL_IS_INTEGER: i32 = -6;
    let ret = unsafe {
        RawSQLSetStmtAttr(
            hstmt,
            SQL_SOPT_SS_PARAM_FOCUS,
            (index + 1) as Pointer,
            SQL_IS_INTEGER,
        )
    };
    if !ok(ret) {
        return Err(on_error("SQLSetStmtAttr(SQL_SOPT_SS_PARAM_FOCUS)"));
    }

    let mut tokens: Vec<Box<Len>> = Vec::with_capacity(ncols);
    let mut indicators: Vec<Box<Len>> = Vec::with_capacity(ncols);
    let mut focus_result: PyResult<()> = Ok(());
    for (i, info) in cols.iter().enumerate() {
        let (vtype, ptype): (CDataType, SqlDataType) = match info.kind {
            TvpColKind::Text { wide: true } => (CDataType::WChar, SqlDataType::EXT_W_VARCHAR),
            TvpColKind::Text { wide: false } => (CDataType::Char, SqlDataType::VARCHAR),
            TvpColKind::Bytes => (CDataType::Binary, SqlDataType::EXT_VAR_BINARY),
            TvpColKind::I64 => (CDataType::SBigInt, SqlDataType::EXT_BIG_INT),
            TvpColKind::F64 => (CDataType::Double, SqlDataType::DOUBLE),
            TvpColKind::Bool => (CDataType::Bit, SqlDataType::EXT_BIT),
            TvpColKind::Date => (CDataType::TypeDate, SqlDataType::DATE),
            TvpColKind::Time => (CDataType::TypeTime, SqlDataType::TIME),
            TvpColKind::Ts => (CDataType::TypeTimestamp, SqlDataType::TIMESTAMP),
            TvpColKind::Decimal => (CDataType::Char, SqlDataType::NUMERIC),
            TvpColKind::Uuid => (CDataType::Guid, SqlDataType::EXT_GUID),
            TvpColKind::NullOnly => (CDataType::Default, SqlDataType::VARCHAR),
        };
        let mut token: Box<Len> = Box::new(i as Len);
        let mut indicator: Box<Len> = Box::new(SQL_DATA_AT_EXEC);
        let token_ptr = std::ptr::from_mut::<Len>(&mut *token) as Pointer;
        let ret = unsafe {
            odbc_sys::SQLBindParameter(
                hstmt,
                (i + 1) as u16,
                ParamType::Input,
                vtype,
                ptype,
                info.colsize,
                info.ddigits,
                token_ptr,
                0,
                &mut *indicator,
            )
        };
        if !ok(ret) {
            focus_result = Err(on_error("SQLBindParameter"));
            break;
        }
        tokens.push(token);
        indicators.push(indicator);
    }

    // Always restore the focus to the statement's own parameters.
    let ret = unsafe {
        RawSQLSetStmtAttr(
            hstmt,
            SQL_SOPT_SS_PARAM_FOCUS,
            std::ptr::null_mut(),
            SQL_IS_INTEGER,
        )
    };
    focus_result?;
    if !ok(ret) {
        return Err(on_error("SQLSetStmtAttr(SQL_SOPT_SS_PARAM_FOCUS)"));
    }

    let kinds = cols.iter().map(|c| c.kind).collect();
    Ok(BoundParam {
        _value: Box::new(ParamValue::Null), // row data lives in the TvpBound
        indicator: outer_indicator,
        dae: true,
        dae_chunk: 0,
        tvp: Some(TvpBound {
            rows,
            kinds,
            cur: Cell::new(0),
            active: Cell::new(0),
            tokens,
            _indicators: indicators,
            datetime_precision: limits.cnxninfo.datetime_precision,
        }),
    })
}
