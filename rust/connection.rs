// The Connection type.  Ported from src/connection.cpp with the concurrency model
// from docs/rust-asyncio-rewrite-plan.md: every connection owns a worker thread and
// all ODBC calls for it (and its cursors) run there, in order.  Async methods
// return asyncio futures; a few sync properties (autocommit, timeout) dispatch to
// the worker and block briefly.  Pure-configuration state (encodings, converters,
// maxwrite, ...) is stored on the connection and snapshotted per operation.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use odbc_sys::{
    CompletionType, ConnectionAttribute, DriverConnectOption, HDbc, Handle, HandleType, Pointer,
    SqlReturn,
};
use pyo3::prelude::*;
use pyo3::types::PyTuple;

use crate::cursor::Cursor;
use crate::errors::{error_from_handle, OperationalError, ProgrammingError};
use crate::getinfo_types::{InfoKind, GETINFO_TYPES};
use crate::textenc::{self, ConnEncodings, SQL_CHAR, SQL_WCHAR, SQL_WMETADATA};
use crate::worker::{
    self, closed_connection_err, dispatch_future, dispatch_future_terminal, dispatch_sync,
    ConnState, Finisher, Task,
};
use crate::{async_bridge, env};

// SQLGetInfo bound with a raw u16 info type: odbc-sys models InfoType as an enum
// that doesn't cover everything in the aInfoTypes table.
extern "system" {
    #[link_name = "SQLSetConnectAttrW"]
    fn RawSQLSetConnectAttrW(
        hdbc: HDbc,
        attribute: i32,
        value: Pointer,
        string_length: i32,
    ) -> SqlReturn;

    #[link_name = "SQLGetInfo"]
    fn RawSQLGetInfo(
        hdbc: HDbc,
        info_type: u16,
        value: Pointer,
        buffer_length: i16,
        string_length: *mut i16,
    ) -> SqlReturn;
}

fn succeeded(ret: SqlReturn) -> bool {
    matches!(ret, SqlReturn::SUCCESS | SqlReturn::SUCCESS_WITH_INFO)
}

// SQL_ATTR_AUTOCOMMIT values / SQL_ATTR_ACCESS_MODE values (sqlext.h)
const SQL_AUTOCOMMIT_OFF: usize = 0;
const SQL_AUTOCOMMIT_ON: usize = 1;
const SQL_MODE_READ_ONLY: usize = 1;
const SQL_NEED_LONG_DATA_LEN: u16 = 111;
const SQL_DESCRIBE_PARAMETER: u16 = 10002;

pub type ConverterMap = Arc<Mutex<HashMap<i32, Py<PyAny>>>>;

#[pyclass(module = "pyodbc")]
pub struct Connection {
    tx: Option<Sender<Task>>,
    closed: bool,
    autocommit_flag: Arc<AtomicBool>,
    timeout_cache: Arc<AtomicU32>,
    readvar_initsize: Arc<AtomicUsize>,
    maxwrite_value: Arc<AtomicUsize>,
    fetch_decimal_as_string_flag: Arc<AtomicBool>,
    compat_diagrec: Arc<AtomicBool>,
    hdbc_public: Arc<AtomicUsize>,
    encodings: Arc<Mutex<ConnEncodings>>,
    converters: ConverterMap,
}

impl Connection {
    /// The command channel, or the closed-connection error.  (Ported from
    /// Connection_Validate.)
    pub fn channel(&self) -> PyResult<&Sender<Task>> {
        if self.closed {
            return Err(closed_connection_err());
        }
        self.tx.as_ref().ok_or_else(closed_connection_err)
    }

    pub fn encodings_snapshot(&self) -> ConnEncodings {
        self.encodings.lock().unwrap().clone()
    }

    pub fn converter_map(&self) -> ConverterMap {
        self.converters.clone()
    }

    pub fn converter_types(&self) -> Vec<i32> {
        self.converters.lock().unwrap().keys().copied().collect()
    }

    pub fn readvar_initsize_value(&self) -> usize {
        self.readvar_initsize.load(Ordering::Relaxed)
    }

    pub fn maxwrite_setting(&self) -> usize {
        self.maxwrite_value.load(Ordering::Relaxed)
    }

    pub fn fetch_decimal_as_string_value(&self) -> bool {
        self.fetch_decimal_as_string_flag.load(Ordering::Relaxed)
    }

    pub fn diagrec_byte_length(&self) -> bool {
        self.compat_diagrec.load(Ordering::Relaxed)
    }

    /// Enqueue a commit or rollback and return the future.  Shared with Cursor.
    pub fn end_tran_future(
        py: Python<'_>,
        tx: &Sender<Task>,
        completion: CompletionType,
    ) -> PyResult<Py<PyAny>> {
        dispatch_future(py, tx, move |state| {
            if state.hdbc == 0 {
                return Err(closed_connection_err());
            }
            let ret =
                unsafe { odbc_sys::SQLEndTran(HandleType::Dbc, state.hdbc as Handle, completion) };
            if !succeeded(ret) {
                let func = match completion {
                    CompletionType::Commit => "SQLEndTran(SQL_COMMIT)",
                    _ => "SQLEndTran(SQL_ROLLBACK)",
                };
                return Err(error_from_handle(
                    func,
                    HandleType::Dbc,
                    state.hdbc as Handle,
                ));
            }
            Ok(Box::new(|py: Python<'_>| Ok(py.None())) as Finisher)
        })
    }
}

#[pymethods]
impl Connection {
    #[getter]
    pub fn autocommit(&self) -> bool {
        self.autocommit_flag.load(Ordering::Relaxed)
    }

    #[setter]
    fn set_autocommit(&self, py: Python<'_>, value: bool) -> PyResult<()> {
        // A synchronous property setter cannot be awaited, so this dispatches to
        // the worker and blocks (GIL released) until the attribute is set.
        let tx = self.channel()?;
        let flag = self.autocommit_flag.clone();
        dispatch_sync(py, tx, move |state| {
            if state.hdbc == 0 {
                return Err(closed_connection_err());
            }
            let n = if value {
                SQL_AUTOCOMMIT_ON
            } else {
                SQL_AUTOCOMMIT_OFF
            };
            let ret = unsafe {
                odbc_sys::SQLSetConnectAttrW(
                    state.hdbc as HDbc,
                    ConnectionAttribute::AutoCommit,
                    n as Pointer,
                    odbc_sys::IS_UINTEGER,
                )
            };
            if !succeeded(ret) {
                return Err(error_from_handle(
                    "SQLSetConnectAttr",
                    HandleType::Dbc,
                    state.hdbc as Handle,
                ));
            }
            flag.store(value, Ordering::Relaxed);
            Ok(())
        })
    }

    #[getter]
    fn closed(&self) -> bool {
        self.closed
    }

    /// The raw ODBC connection handle as ctypes.c_void_p, or None once closed.
    #[getter]
    fn hdbc(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        if self.closed {
            return Ok(py.None());
        }
        let value = self.hdbc_public.load(Ordering::Relaxed);
        Ok(py
            .import("ctypes")?
            .getattr("c_void_p")?
            .call1((value,))?
            .unbind())
    }

    #[getter]
    fn timeout(&self) -> u32 {
        self.timeout_cache.load(Ordering::Relaxed)
    }

    #[setter]
    fn set_timeout(&self, py: Python<'_>, value: u32) -> PyResult<()> {
        let tx = self.channel()?;
        let cache = self.timeout_cache.clone();
        dispatch_sync(py, tx, move |state| {
            if state.hdbc == 0 {
                return Err(closed_connection_err());
            }
            let ret = unsafe {
                odbc_sys::SQLSetConnectAttrW(
                    state.hdbc as HDbc,
                    ConnectionAttribute::ConnectionTimeout,
                    value as usize as Pointer,
                    odbc_sys::IS_UINTEGER,
                )
            };
            if !succeeded(ret) {
                return Err(error_from_handle(
                    "SQLSetConnectAttr(SQL_ATTR_CONNECTION_TIMEOUT)",
                    HandleType::Dbc,
                    state.hdbc as Handle,
                ));
            }
            cache.store(value, Ordering::Relaxed);
            Ok(())
        })
    }

    /// Initial buffer size in bytes for reading variable-length columns; 0 means
    /// size the buffer from the column descriptor.  Stored only - it is consulted
    /// on the worker at fetch time, so the setter needs no ODBC round-trip.
    #[getter]
    fn readvar_initsize(&self) -> usize {
        self.readvar_initsize.load(Ordering::Relaxed)
    }

    #[setter]
    fn set_readvar_initsize(&self, value: i64) -> PyResult<()> {
        if value < 0 {
            return Err(pyo3::exceptions::PyTypeError::new_err(
                "Cannot set readvar_initsize to a negative value.",
            ));
        }
        self.readvar_initsize
            .store(value as usize, Ordering::Relaxed);
        Ok(())
    }

    /// Maximum bytes to write per SQLBindParameter buffer; longer values stream via
    /// SQLPutData.  0 (the default) means no maximum.
    #[getter]
    fn maxwrite(&self) -> usize {
        self.maxwrite_value.load(Ordering::Relaxed)
    }

    #[setter]
    fn set_maxwrite(&self, value: i64) -> PyResult<()> {
        if value < 0 {
            return Err(pyo3::exceptions::PyTypeError::new_err(
                "Cannot set maxwrite to a negative value.",
            ));
        }
        self.maxwrite_value.store(value as usize, Ordering::Relaxed);
        Ok(())
    }

    /// If True, DECIMAL/NUMERIC values are fetched via the legacy locale-aware
    /// string path instead of the binary SQL_NUMERIC_STRUCT representation.
    #[getter]
    fn fetch_decimal_as_string(&self) -> bool {
        self.fetch_decimal_as_string_flag.load(Ordering::Relaxed)
    }

    #[setter]
    fn set_fetch_decimal_as_string(&self, value: bool) {
        self.fetch_decimal_as_string_flag
            .store(value, Ordering::Relaxed);
    }

    /// Workaround for drivers that report diagnostic text length in bytes instead
    /// of characters (https://github.com/mkleehammer/pyodbc/issues/489).
    #[getter]
    fn compat_diagrec_byte_length(&self) -> bool {
        self.compat_diagrec.load(Ordering::Relaxed)
    }

    #[setter]
    fn set_compat_diagrec_byte_length(&self, value: bool) {
        self.compat_diagrec.store(value, Ordering::Relaxed);
    }

    /// Set the text encoding for SQL statements and textual parameters.
    #[pyo3(signature = (encoding=None, ctype=None))]
    fn setencoding(
        &self,
        py: Python<'_>,
        encoding: Option<&str>,
        ctype: Option<i32>,
    ) -> PyResult<()> {
        let encoding = encoding
            .ok_or_else(|| pyo3::exceptions::PyValueError::new_err("encoding is required"))?;
        let enc = textenc::make_text_enc(py, encoding, ctype)?;
        self.encodings.lock().unwrap().unicode = enc;
        Ok(())
    }

    /// Set the decoding used when reading SQL_CHAR, SQL_WCHAR, or metadata.
    #[pyo3(signature = (sqltype, encoding=None, ctype=None))]
    fn setdecoding(
        &self,
        py: Python<'_>,
        sqltype: i32,
        encoding: Option<&str>,
        ctype: Option<i32>,
    ) -> PyResult<()> {
        if sqltype != SQL_CHAR && sqltype != SQL_WCHAR && sqltype != SQL_WMETADATA {
            return Err(pyo3::exceptions::PyValueError::new_err(format!(
                "Invalid sqltype {sqltype}.  Must be SQL_CHAR or SQL_WCHAR or SQL_WMETADATA"
            )));
        }
        let encoding = encoding
            .ok_or_else(|| pyo3::exceptions::PyValueError::new_err("encoding is required"))?;
        let enc = textenc::make_text_enc(py, encoding, ctype)?;
        let mut encodings = self.encodings.lock().unwrap();
        match sqltype {
            SQL_CHAR => encodings.sqlchar = enc,
            SQL_WMETADATA => encodings.metadata = enc,
            _ => encodings.sqlwchar = enc,
        }
        Ok(())
    }

    /// Register an output converter for a SQL type (None removes it).
    #[pyo3(signature = (sqltype, func, /))]
    fn add_output_converter(&self, sqltype: i32, func: Option<Bound<'_, PyAny>>) -> PyResult<()> {
        let mut map = self.converters.lock().unwrap();
        match func {
            Some(f) => {
                map.insert(sqltype, f.unbind());
            }
            None => {
                map.remove(&sqltype);
            }
        }
        Ok(())
    }

    #[pyo3(signature = (sqltype, /))]
    fn get_output_converter(&self, py: Python<'_>, sqltype: i32) -> Option<Py<PyAny>> {
        self.converters
            .lock()
            .unwrap()
            .get(&sqltype)
            .map(|f| f.clone_ref(py))
    }

    #[pyo3(signature = (sqltype, /))]
    fn remove_output_converter(&self, sqltype: i32) {
        self.converters.lock().unwrap().remove(&sqltype);
    }

    fn clear_output_converters(&self) {
        self.converters.lock().unwrap().clear();
    }

    /// Set a raw connection attribute via SQLSetConnectAttr (int or string value).
    #[pyo3(signature = (attr_id, value, /))]
    fn set_attr(
        &self,
        py: Python<'_>,
        attr_id: i32,
        value: Bound<'_, PyAny>,
    ) -> PyResult<Py<PyAny>> {
        enum AttrValue {
            Int(isize),
            Text(Vec<u16>),
        }
        let attr = if let Ok(i) = value.extract::<isize>() {
            AttrValue::Int(i)
        } else if let Ok(s) = value.extract::<String>() {
            // NUL-terminated: the length is passed as SQL_NTS.
            AttrValue::Text(s.encode_utf16().chain(std::iter::once(0)).collect())
        } else {
            return Err(pyo3::exceptions::PyTypeError::new_err(format!(
                "set_attr value must be a string or integer, not '{}'",
                value.get_type().name()?
            )));
        };
        dispatch_future(py, self.channel()?, move |state| {
            if state.hdbc == 0 {
                return Err(closed_connection_err());
            }
            let ret = match &attr {
                AttrValue::Int(i) => unsafe {
                    RawSQLSetConnectAttrW(
                        state.hdbc as HDbc,
                        attr_id,
                        *i as Pointer,
                        odbc_sys::IS_INTEGER,
                    )
                },
                AttrValue::Text(w) => unsafe {
                    RawSQLSetConnectAttrW(
                        state.hdbc as HDbc,
                        attr_id,
                        w.as_ptr() as Pointer,
                        odbc_sys::NTS as i32,
                    )
                },
            };
            if !succeeded(ret) {
                return Err(error_from_handle(
                    "SQLSetConnectAttr",
                    HandleType::Dbc,
                    state.hdbc as Handle,
                ));
            }
            Ok(Box::new(|py: Python<'_>| Ok(py.None())) as Finisher)
        })
    }

    /// Retrieve driver/data source information via SQLGetInfo.
    #[pyo3(signature = (infotype, /))]
    fn getinfo(&self, py: Python<'_>, infotype: u32) -> PyResult<Py<PyAny>> {
        let kind = GETINFO_TYPES
            .iter()
            .find(|(t, _)| *t as u32 == infotype)
            .map(|(_, k)| *k)
            .ok_or_else(|| {
                ProgrammingError::new_err(format!("Unsupported getinfo value: {infotype}"))
            })?;

        dispatch_future(py, self.channel()?, move |state| {
            if state.hdbc == 0 {
                return Err(closed_connection_err());
            }
            let mut buffer = [0u8; 0x1000];
            let mut cch: i16 = 0;
            let ret = unsafe {
                RawSQLGetInfo(
                    state.hdbc as HDbc,
                    infotype as u16,
                    buffer.as_mut_ptr() as Pointer,
                    buffer.len() as i16,
                    &mut cch,
                )
            };
            if !succeeded(ret) {
                return Err(error_from_handle(
                    "SQLGetInfo",
                    HandleType::Dbc,
                    state.hdbc as Handle,
                ));
            }

            enum Info {
                Bool(bool),
                Text(String),
                UInt(u32),
                USmallInt(u16),
            }
            let value = match kind {
                InfoKind::YesNo => Info::Bool(buffer[0] == b'Y'),
                InfoKind::Str => {
                    let len = (cch.max(0) as usize).min(buffer.len());
                    Info::Text(String::from_utf8_lossy(&buffer[..len]).into_owned())
                }
                InfoKind::UInt => Info::UInt(u32::from_ne_bytes(buffer[..4].try_into().unwrap())),
                InfoKind::USmallInt => {
                    Info::USmallInt(u16::from_ne_bytes(buffer[..2].try_into().unwrap()))
                }
            };

            Ok(Box::new(move |py: Python<'_>| {
                Ok(match value {
                    Info::Bool(v) => v.into_pyobject(py)?.to_owned().into_any().unbind(),
                    Info::Text(v) => v.into_pyobject(py)?.into_any().unbind(),
                    Info::UInt(v) => v.into_pyobject(py)?.into_any().unbind(),
                    Info::USmallInt(v) => v.into_pyobject(py)?.into_any().unbind(),
                })
            }) as Finisher)
        })
    }

    /// Create a new cursor.  Synchronous: the statement handle is allocated lazily
    /// on the worker at first use.
    fn cursor(slf: &Bound<'_, Self>) -> PyResult<Cursor> {
        let this = slf.borrow();
        let tx = this.channel()?.clone();
        Ok(Cursor::new(tx, slf.clone().unbind()))
    }

    /// Convenience: create a cursor and execute on it.  Returns a future resolving
    /// to the new cursor.
    #[pyo3(signature = (sql, *params))]
    fn execute(
        slf: &Bound<'_, Self>,
        py: Python<'_>,
        sql: &str,
        params: &Bound<'_, PyTuple>,
    ) -> PyResult<Py<PyAny>> {
        let cursor = Py::new(py, Self::cursor(slf)?)?;
        Cursor::execute_on(cursor.bind(py), py, sql, params)
    }

    fn commit(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        Self::end_tran_future(py, self.channel()?, CompletionType::Commit)
    }

    fn rollback(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        Self::end_tran_future(py, self.channel()?, CompletionType::Rollback)
    }

    /// Close the connection.  Uncommitted work is rolled back (like the C++
    /// implementation).  The worker thread shuts down after this completes.
    fn close(&mut self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let tx = self.channel()?.clone();
        self.closed = true;
        self.tx = None;
        dispatch_future_terminal(py, &tx, move |state| {
            state.clear();
            Ok(Box::new(|py: Python<'_>| Ok(py.None())) as Finisher)
        })
    }

    fn __aenter__(slf: &Bound<'_, Self>, py: Python<'_>) -> PyResult<Py<PyAny>> {
        slf.borrow().channel()?; // raise now if already closed
        Ok(async_bridge::resolved_future(py, slf.clone().into_any())?.unbind())
    }

    /// Commits on clean exit, rolls back if an exception occurred; does NOT close
    /// (the documented pyodbc Connection.__exit__ semantics).
    #[pyo3(signature = (exc_type=None, exc_value=None, traceback=None))]
    fn __aexit__(
        &self,
        py: Python<'_>,
        exc_type: Option<&Bound<'_, PyAny>>,
        exc_value: Option<&Bound<'_, PyAny>>,
        traceback: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let _ = (exc_value, traceback);
        if self.autocommit_flag.load(Ordering::Relaxed) {
            return Ok(async_bridge::resolved_future(py, py.None().into_bound(py))?.unbind());
        }
        let completion = match exc_type {
            None => CompletionType::Commit,
            Some(t) if t.is_none() => CompletionType::Commit,
            Some(_) => CompletionType::Rollback,
        };
        Self::end_tran_future(py, self.channel()?, completion)
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // A connection dropped without close(): tell the worker to shut down.  Its
        // loop exit path rolls back and frees the handle; we never block here.
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Box::new(|state: &mut ConnState| {
                state.clear();
                false
            }));
        }
    }
}

/// Probe driver capabilities (ported from CnxnInfo_New in cnxninfo.cpp).
fn probe_cnxninfo(hdbc: Handle) -> worker::CnxnInfo {
    let mut info = worker::CnxnInfo::default();

    let yesno = |infotype: u16| -> Option<bool> {
        let mut buf = [0u8; 4];
        let mut cch: i16 = 0;
        let ret = unsafe {
            RawSQLGetInfo(
                hdbc as HDbc,
                infotype,
                buf.as_mut_ptr() as Pointer,
                buf.len() as i16,
                &mut cch,
            )
        };
        succeeded(ret).then_some(buf[0] == b'Y')
    };
    info.need_long_data_len = yesno(SQL_NEED_LONG_DATA_LEN).unwrap_or(false);
    info.supports_describeparam = yesno(SQL_DESCRIBE_PARAMETER).unwrap_or(false);

    // COLUMN_SIZE (3rd column) of SQLGetTypeInfo, per type (GetColumnSize in
    // cnxninfo.cpp; a fresh HSTMT each time, as some drivers dislike reuse).
    let column_size = |sqltype: i16, out: &mut usize| {
        let mut hstmt: Handle = std::ptr::null_mut();
        if !succeeded(unsafe { odbc_sys::SQLAllocHandle(HandleType::Stmt, hdbc, &mut hstmt) }) {
            return;
        }
        let hstmt = hstmt as odbc_sys::HStmt;
        let mut size: i32 = 0;
        let mut ind: odbc_sys::Len = 0;
        let ok = unsafe {
            succeeded(odbc_sys::SQLGetTypeInfo(
                hstmt,
                odbc_sys::SqlDataType(sqltype),
            )) && succeeded(odbc_sys::SQLFetch(hstmt))
                && succeeded(odbc_sys::SQLGetData(
                    hstmt,
                    3,
                    odbc_sys::CDataType::SLong,
                    &mut size as *mut i32 as Pointer,
                    4,
                    &mut ind,
                ))
        };
        if ok && size >= 1 && ind != odbc_sys::NULL_DATA {
            *out = size as usize;
        }
        unsafe {
            let _ = odbc_sys::SQLFreeHandle(HandleType::Stmt, hstmt as Handle);
        }
    };
    column_size(12, &mut info.varchar_maxlength); // SQL_VARCHAR
    column_size(-9, &mut info.wvarchar_maxlength); // SQL_WVARCHAR
    column_size(-3, &mut info.binary_maxlength); // SQL_VARBINARY
    let mut dt: usize = info.datetime_precision as usize;
    column_size(93, &mut dt); // SQL_TYPE_TIMESTAMP
    info.datetime_precision = dt as i32;

    info
}

fn driver_completion_from_int(value: i32) -> PyResult<DriverConnectOption> {
    // Values validated by the Python-level connect() wrapper, like mod_connect.
    match value {
        0 => Ok(DriverConnectOption::NoPrompt),
        1 => Ok(DriverConnectOption::Complete),
        2 => Ok(DriverConnectOption::Prompt),
        3 => Ok(DriverConnectOption::CompleteRequired),
        _ => Err(ProgrammingError::new_err(
            "Invalid value for driver_completion",
        )),
    }
}

fn do_connect(
    state: &mut ConnState,
    henv: usize,
    connstring: Vec<u16>,
    timeout: u32,
    autocommit: bool,
    readonly: bool,
    completion: DriverConnectOption,
) -> PyResult<()> {
    let mut hdbc: Handle = std::ptr::null_mut();
    let ret = unsafe { odbc_sys::SQLAllocHandle(HandleType::Dbc, henv as Handle, &mut hdbc) };
    if !succeeded(ret) {
        return Err(error_from_handle(
            "SQLAllocHandle",
            HandleType::Env,
            henv as Handle,
        ));
    }

    let fail = |func: &'static str, hdbc: Handle| -> PyErr {
        let err = error_from_handle(func, HandleType::Dbc, hdbc);
        unsafe {
            let _ = odbc_sys::SQLFreeHandle(HandleType::Dbc, hdbc);
        }
        err
    };

    if timeout > 0 {
        let ret = unsafe {
            odbc_sys::SQLSetConnectAttrW(
                hdbc as HDbc,
                ConnectionAttribute::LoginTimeout,
                timeout as usize as Pointer,
                odbc_sys::IS_UINTEGER,
            )
        };
        if !succeeded(ret) {
            return Err(fail("SQLSetConnectAttr(SQL_ATTR_LOGIN_TIMEOUT)", hdbc));
        }
    }

    let ret = unsafe {
        odbc_sys::SQLDriverConnectW(
            hdbc as HDbc,
            std::ptr::null_mut(),
            connstring.as_ptr(),
            connstring.len() as i16,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            completion,
        )
    };
    if ret == SqlReturn::NO_DATA {
        unsafe {
            let _ = odbc_sys::SQLFreeHandle(HandleType::Dbc, hdbc);
        }
        return Err(OperationalError::new_err(
            "User cancelled connection request",
        ));
    }
    if !succeeded(ret) {
        return Err(fail("SQLDriverConnect", hdbc));
    }

    // The DB API requires manual-commit by default, but ODBC defaults to
    // auto-commit; turn it off unless the caller asked for autocommit.
    if !autocommit {
        let ret = unsafe {
            odbc_sys::SQLSetConnectAttrW(
                hdbc as HDbc,
                ConnectionAttribute::AutoCommit,
                SQL_AUTOCOMMIT_OFF as Pointer,
                odbc_sys::IS_UINTEGER,
            )
        };
        if !succeeded(ret) {
            return Err(fail("SQLSetConnectAttr(SQL_ATTR_AUTOCOMMIT)", hdbc));
        }
    }

    if readonly {
        let ret = unsafe {
            odbc_sys::SQLSetConnectAttrW(
                hdbc as HDbc,
                ConnectionAttribute::AccessMode,
                SQL_MODE_READ_ONLY as Pointer,
                0,
            )
        };
        if !succeeded(ret) {
            return Err(fail("SQLSetConnectAttr(SQL_ATTR_ACCESS_MODE)", hdbc));
        }
    }

    state.cnxninfo = probe_cnxninfo(hdbc);

    state.hdbc = hdbc as usize;
    state.hdbc_public.store(hdbc as usize, Ordering::Relaxed);
    Ok(())
}

fn encode_connection_string(connstring: &str, encoding: Option<&str>) -> PyResult<Vec<u16>> {
    // The driver manager's W entry point wants UTF-16 in native byte order.  The
    // encoding parameter exists for drivers with unusual expectations; only the
    // UTF-16 family is supported so far.
    let normalized = encoding.unwrap_or("utf-16le").to_ascii_lowercase();
    let native: Vec<u16> = connstring.encode_utf16().collect();
    match normalized.as_str() {
        "utf-16" | "utf-16le" | "utf-16-le" => Ok(native),
        "utf-16be" | "utf-16-be" => Ok(native.iter().map(|u| u.swap_bytes()).collect()),
        other => Err(crate::errors::NotSupportedError::new_err(format!(
            "connect(encoding='{other}') is not supported by the Rust port yet; \
             only UTF-16 encodings are currently accepted"
        ))),
    }
}

/// Open a connection and return an asyncio future resolving to it.  The public
/// pyodbc.connect() wrapper builds the connection string and keyword handling on
/// top of this.
#[pyfunction]
#[pyo3(signature = (connstring, *, autocommit=false, readonly=false, timeout=0, encoding=None, driver_completion=0))]
pub fn connect(
    py: Python<'_>,
    connstring: &str,
    autocommit: bool,
    readonly: bool,
    timeout: u32,
    encoding: Option<&str>,
    driver_completion: i32,
) -> PyResult<Py<PyAny>> {
    let henv = env::get_env(py)? as usize;
    let completion = driver_completion_from_int(driver_completion)?;
    let wcs = encode_connection_string(connstring, encoding)?;

    let autocommit_flag = Arc::new(AtomicBool::new(autocommit));
    let hdbc_public = Arc::new(AtomicUsize::new(0));
    let tx = worker::spawn(autocommit_flag.clone(), hdbc_public.clone())?;

    let conn = Py::new(
        py,
        Connection {
            tx: Some(tx.clone()),
            closed: false,
            autocommit_flag,
            timeout_cache: Arc::new(AtomicU32::new(0)),
            readvar_initsize: Arc::new(AtomicUsize::new(4096)),
            maxwrite_value: Arc::new(AtomicUsize::new(0)),
            fetch_decimal_as_string_flag: Arc::new(AtomicBool::new(false)),
            compat_diagrec: Arc::new(AtomicBool::new(false)),
            hdbc_public,
            encodings: Arc::new(Mutex::new(ConnEncodings::default())),
            converters: Arc::new(Mutex::new(HashMap::new())),
        },
    )?;
    let conn_result: Py<PyAny> = conn.clone_ref(py).into_any();

    let (fut, handle) = async_bridge::new_future(py)?;
    let task: Task = Box::new(move |state| {
        let result = do_connect(state, henv, wcs, timeout, autocommit, readonly, completion);
        let ok = result.is_ok();
        Python::attach(|py| {
            handle.complete(py, result.map(|_| conn_result));
        });
        ok // a failed connect shuts the worker down
    });
    tx.send(task).map_err(|_| closed_connection_err())?;

    Ok(fut)
}
