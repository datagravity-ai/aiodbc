// The Connection type.  Ported from src/connection.cpp with the concurrency model
// from docs/rust-asyncio-rewrite-plan.md: every connection owns a worker thread and
// all ODBC calls for it (and its cursors) run there, in order.  Async methods
// return asyncio futures; a few sync properties (autocommit, timeout) dispatch to
// the worker and block briefly.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

use odbc_sys::{
    CompletionType, ConnectionAttribute, DriverConnectOption, HDbc, Handle, HandleType, Pointer,
    SqlReturn,
};
use pyo3::prelude::*;
use pyo3::types::PyTuple;

use crate::cursor::Cursor;
use crate::errors::{error_from_handle, OperationalError, ProgrammingError};
use crate::worker::{
    self, closed_connection_err, dispatch_future, dispatch_future_terminal, dispatch_sync,
    ConnState, Finisher, Task,
};
use crate::{async_bridge, env};

fn succeeded(ret: SqlReturn) -> bool {
    matches!(ret, SqlReturn::SUCCESS | SqlReturn::SUCCESS_WITH_INFO)
}

// SQL_ATTR_AUTOCOMMIT values / SQL_ATTR_ACCESS_MODE values (sqlext.h)
const SQL_AUTOCOMMIT_OFF: usize = 0;
const SQL_AUTOCOMMIT_ON: usize = 1;
const SQL_MODE_READ_ONLY: usize = 1;

#[pyclass(module = "pyodbc")]
pub struct Connection {
    tx: Option<Sender<Task>>,
    closed: bool,
    autocommit_flag: Arc<AtomicBool>,
    timeout_cache: Arc<AtomicU32>,
    readvar_initsize: Arc<AtomicUsize>,
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

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    pub fn autocommit_shared(&self) -> Arc<AtomicBool> {
        self.autocommit_flag.clone()
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

    /// Create a new cursor.  Synchronous: the statement handle is allocated lazily
    /// on the worker at first use.
    fn cursor(slf: &Bound<'_, Self>) -> PyResult<Cursor> {
        let this = slf.borrow();
        let tx = this.channel()?.clone();
        Ok(Cursor::new(
            tx,
            slf.clone().unbind(),
            this.readvar_initsize.clone(),
        ))
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

    state.hdbc = hdbc as usize;
    Ok(())
}

fn encode_connection_string(connstring: &str, encoding: Option<&str>) -> PyResult<Vec<u16>> {
    // The driver manager's W entry point wants UTF-16 in native byte order.  The
    // encoding parameter exists for drivers with unusual expectations; only the
    // UTF-16 family is supported so far (others come with the textenc port).
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
    let tx = worker::spawn(autocommit_flag.clone())?;

    let conn = Py::new(
        py,
        Connection {
            tx: Some(tx.clone()),
            closed: false,
            autocommit_flag,
            timeout_cache: Arc::new(AtomicU32::new(0)),
            readvar_initsize: Arc::new(AtomicUsize::new(4096)),
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
