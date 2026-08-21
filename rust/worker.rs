// The per-connection worker thread.
//
// Every Connection owns one OS thread that performs ALL ODBC calls for the
// connection and its cursors, in submission order.  This serializes access to the
// HDBC and its statements (the conservative reading of ODBC driver thread-safety)
// and matches DB API transaction semantics.  See docs/rust-asyncio-rewrite-plan.md.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

use odbc_sys::{CompletionType, Handle, HandleType};
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

use crate::async_bridge::new_future;
use crate::errors::ProgrammingError;

/// Driver capabilities probed at connect time (ported from CnxnInfo in
/// cnxninfo.cpp; the per-connection-string cache is not carried over - probing a
/// handful of attributes per connect is cheap and always current).
#[derive(Clone)]
pub struct CnxnInfo {
    pub supports_describeparam: bool,
    /// COLUMN_SIZE of SQL_TYPE_TIMESTAMP, e.g. 23 for SQL Server datetime.
    pub datetime_precision: i32,
    pub varchar_maxlength: usize,
    pub wvarchar_maxlength: usize,
    pub binary_maxlength: usize,
    /// SQLGetInfo(SQL_NEED_LONG_DATA_LEN): whether data-at-execution parameters
    /// must declare their length up front.
    pub need_long_data_len: bool,
}

impl Default for CnxnInfo {
    fn default() -> Self {
        // The defaults from CnxnInfo_New for drivers that can't be probed.
        CnxnInfo {
            supports_describeparam: false,
            datetime_precision: 19, // "yyyy-mm-dd hh:mm:ss"
            varchar_maxlength: 1 << 30,
            wvarchar_maxlength: 1 << 30,
            binary_maxlength: 1 << 30,
            need_long_data_len: false,
        }
    }
}

/// Connection state owned by (and only touched from) the worker thread, except for
/// the shared autocommit flag which the Connection reads for its sync property.
pub struct ConnState {
    pub hdbc: usize, // HDbc as usize; 0 = not connected
    pub autocommit: Arc<AtomicBool>,
    /// Mirror of hdbc readable from the Connection object (for the hdbc property).
    pub hdbc_public: Arc<AtomicUsize>,
    /// Driver capabilities probed at connect.
    pub cnxninfo: CnxnInfo,
    /// Buffers handed to the driver via pre-connect SQLSetConnectAttr.  Some
    /// drivers keep reading them after the call returns (pyodbc issue #1469), so
    /// they live as long as the connection.
    pub preconn_keepalive: Vec<Vec<u8>>,
}

impl ConnState {
    pub fn hdbc_raw(&self) -> odbc_sys::HDbc {
        self.hdbc as odbc_sys::HDbc
    }

    /// Roll back (if in manual-commit mode), disconnect, and free the HDBC.
    /// Ported from Connection_clear in connection.cpp.
    pub fn clear(&mut self) {
        if self.hdbc != 0 {
            let hdbc = self.hdbc_raw();
            self.hdbc = 0;
            self.hdbc_public.store(0, Ordering::Relaxed);
            unsafe {
                if !self.autocommit.load(Ordering::Relaxed) {
                    let _ = odbc_sys::SQLEndTran(
                        HandleType::Dbc,
                        hdbc as Handle,
                        CompletionType::Rollback,
                    );
                }
                let _ = odbc_sys::SQLDisconnect(hdbc);
                let _ = odbc_sys::SQLFreeHandle(HandleType::Dbc, hdbc as Handle);
            }
        }
    }
}

/// A unit of work.  Returns false to shut the worker down (used by close()).
pub type Task = Box<dyn FnOnce(&mut ConnState) -> bool + Send>;

/// Converts work done on the worker (no GIL) into a Python result (GIL held).
pub type Finisher = Box<dyn FnOnce(Python<'_>) -> PyResult<Py<PyAny>> + Send>;

pub fn spawn(autocommit: Arc<AtomicBool>, hdbc_public: Arc<AtomicUsize>) -> PyResult<Sender<Task>> {
    let (tx, rx): (Sender<Task>, Receiver<Task>) = channel();
    std::thread::Builder::new()
        .name("pyodbc-connection".into())
        .spawn(move || {
            let mut state = ConnState {
                hdbc: 0,
                autocommit,
                hdbc_public,
                cnxninfo: CnxnInfo::default(),
                preconn_keepalive: Vec::new(),
            };
            while let Ok(task) = rx.recv() {
                if !task(&mut state) {
                    break;
                }
            }
            // Safety net for connections dropped without close(): roll back and
            // free the handle so the database is not left with a dangling session.
            state.clear();
        })
        .map_err(|e| PyRuntimeError::new_err(format!("failed to spawn pyodbc worker: {e}")))?;
    Ok(tx)
}

pub fn closed_connection_err() -> PyErr {
    ProgrammingError::new_err("Attempt to use a closed connection.")
}

/// Enqueue `op` and return an asyncio future for its result.  `op` runs on the
/// worker without the GIL and returns a finisher that builds the Python-level
/// result under the GIL.
pub fn dispatch_future<F>(py: Python<'_>, tx: &Sender<Task>, op: F) -> PyResult<Py<PyAny>>
where
    F: FnOnce(&mut ConnState) -> PyResult<Finisher> + Send + 'static,
{
    dispatch_future_impl(py, tx, op, false)
}

/// Like dispatch_future, but shuts the worker down after completing.  Used by
/// Connection.close().
pub fn dispatch_future_terminal<F>(py: Python<'_>, tx: &Sender<Task>, op: F) -> PyResult<Py<PyAny>>
where
    F: FnOnce(&mut ConnState) -> PyResult<Finisher> + Send + 'static,
{
    dispatch_future_impl(py, tx, op, true)
}

fn dispatch_future_impl<F>(
    py: Python<'_>,
    tx: &Sender<Task>,
    op: F,
    terminal: bool,
) -> PyResult<Py<PyAny>>
where
    F: FnOnce(&mut ConnState) -> PyResult<Finisher> + Send + 'static,
{
    let (fut, handle) = new_future(py)?;
    let task: Task = Box::new(move |state| {
        let result = op(state);
        Python::attach(|py| {
            let resolved = match result {
                Ok(finish) => finish(py),
                Err(e) => Err(e),
            };
            handle.complete(py, resolved);
        });
        !terminal
    });
    tx.send(task).map_err(|_| closed_connection_err())?;
    Ok(fut)
}

/// Enqueue `op` and block the calling thread (with the GIL released) until it
/// completes.  Used for the few synchronous properties that must perform an ODBC
/// call, like the autocommit setter.
pub fn dispatch_sync<T, F>(py: Python<'_>, tx: &Sender<Task>, op: F) -> PyResult<T>
where
    T: Send + 'static,
    F: FnOnce(&mut ConnState) -> PyResult<T> + Send + 'static,
{
    let (rtx, rrx) = channel::<PyResult<T>>();
    let task: Task = Box::new(move |state| {
        let _ = rtx.send(op(state));
        true
    });
    tx.send(task).map_err(|_| closed_connection_err())?;
    py.detach(move || rrx.recv())
        .map_err(|_| closed_connection_err())?
}
