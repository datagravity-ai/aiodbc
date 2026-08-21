// The Cursor type.  Ported from src/cursor.cpp on the worker-thread model: every
// ODBC call is enqueued on the parent connection's worker and returned to Python as
// an asyncio future.  The HSTMT is allocated lazily on first use.

use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use odbc_sys::{
    CompletionType, FreeStmtOption, HStmt, Handle, HandleType, Len, Nullability, SqlDataType,
    SqlReturn,
};
use pyo3::exceptions::{PyStopAsyncIteration, PyTypeError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyTuple};

use crate::async_bridge;
use crate::connection::Connection;
use crate::errors::{error_from_handle, ProgrammingError};
use crate::getdata::{self, CellValue};
use crate::params::{self, BoundParam, ParamValue};
use crate::row::Row;
use crate::worker::{dispatch_future, Finisher, Task};

fn succeeded(ret: SqlReturn) -> bool {
    matches!(ret, SqlReturn::SUCCESS | SqlReturn::SUCCESS_WITH_INFO)
}

fn closed_cursor_err() -> PyErr {
    ProgrammingError::new_err("Attempt to use a closed cursor.")
}

fn no_results_err() -> PyErr {
    ProgrammingError::new_err("No results.  Previous SQL was not a query.")
}

/// Metadata for one result column, produced on the worker after an execute.
#[derive(Clone)]
pub struct ColInfo {
    pub sql_type: i16,
    // Used from phase 3 on (readvar buffer sizing); carried in the description now.
    #[allow(dead_code)]
    pub column_size: u64,
}

/// Raw column description read via SQLDescribeColW, converted into the Python
/// `description` tuple under the GIL by the execute finisher.
struct RawCol {
    name: String,
    sql_type: i16,
    column_size: u64,
    decimal_digits: i16,
    nullable: Nullability,
}

/// State shared between the Cursor object and the tasks it enqueues.  ODBC fields
/// are only touched on the worker; the Py fields are only touched under the GIL.
#[derive(Default)]
pub struct CursorShared {
    pub hstmt: usize,
    pub colinfos: Vec<ColInfo>,
    pub description: Option<Py<PyAny>>,
    pub name_map: Option<Py<PyDict>>,
    pub rowcount: i64,
}

#[pyclass(module = "pyodbc")]
pub struct Cursor {
    tx: Sender<Task>,
    connection: Py<Connection>,
    shared: Arc<Mutex<CursorShared>>,
    closed: bool,
    #[pyo3(get, set)]
    arraysize: usize,
}

enum ExecuteRows {
    One(Vec<ParamValue>),
    Many(Vec<Vec<ParamValue>>),
}

enum FetchMode {
    One,
    Many(usize),
    All,
    Val,
    Skip(usize),
    Next, // __anext__: raises StopAsyncIteration when exhausted
}

/// Ported from IsSequence in cursor.cpp: only list, tuple, and Row count as a
/// parameter collection.
fn is_param_sequence(obj: &Bound<'_, PyAny>) -> bool {
    obj.is_instance_of::<PyList>() || obj.is_instance_of::<PyTuple>() || obj.is_instance_of::<Row>()
}

fn extract_param_row(row: &Bound<'_, PyAny>) -> PyResult<Vec<ParamValue>> {
    let mut out = Vec::new();
    for (i, cell) in row.try_iter()?.enumerate() {
        out.push(params::extract(&cell?, i)?);
    }
    Ok(out)
}

impl Cursor {
    pub fn new(tx: Sender<Task>, connection: Py<Connection>) -> Self {
        Cursor {
            tx,
            connection,
            shared: Arc::new(Mutex::new(CursorShared::default())),
            closed: false,
            arraysize: 1,
        }
    }

    fn validate(&self, py: Python<'_>) -> PyResult<()> {
        if self.closed {
            return Err(closed_cursor_err());
        }
        // Cursor_Validate also checks the parent connection.
        self.connection.bind(py).borrow().channel()?;
        Ok(())
    }

    /// Shared by Cursor.execute and Connection.execute.
    pub fn execute_on(
        slf: &Bound<'_, Cursor>,
        py: Python<'_>,
        sql: &str,
        params_tuple: &Bound<'_, PyTuple>,
    ) -> PyResult<Py<PyAny>> {
        // Figure out how parameters were passed (cursor.cpp Cursor_execute): a
        // single list/tuple/Row argument is the parameter collection; otherwise
        // the positional arguments themselves are the parameters.
        let values = if params_tuple.len() == 1 {
            let first = params_tuple.get_item(0)?;
            if is_param_sequence(&first) {
                extract_param_row(&first)?
            } else {
                extract_param_row(params_tuple.as_any())?
            }
        } else {
            extract_param_row(params_tuple.as_any())?
        };
        Self::run_execute(slf, py, sql, ExecuteRows::One(values))
    }

    fn run_execute(
        slf: &Bound<'_, Cursor>,
        py: Python<'_>,
        sql: &str,
        rows: ExecuteRows,
    ) -> PyResult<Py<PyAny>> {
        let this = slf.borrow();
        this.validate(py)?;

        let lowercase: bool = py
            .import("pyodbc")
            .and_then(|m| m.getattr("lowercase"))
            .and_then(|v| v.extract())
            .unwrap_or(false);

        let shared = this.shared.clone();
        let sql_w: Vec<u16> = sql.encode_utf16().collect();
        let cursor_obj: Py<PyAny> = slf.clone().into_any().unbind();

        dispatch_future(py, &this.tx, move |state| {
            if state.hdbc == 0 {
                return Err(crate::worker::closed_connection_err());
            }
            let hstmt = ensure_hstmt(state, &shared)?;
            free_results(hstmt);

            let (rowcount, raw_cols) = execute_odbc(hstmt, &sql_w, rows)?;

            {
                let mut guard = shared.lock().unwrap();
                guard.colinfos = raw_cols
                    .iter()
                    .map(|c| ColInfo {
                        sql_type: c.sql_type,
                        column_size: c.column_size,
                    })
                    .collect();
                guard.rowcount = rowcount;
            }

            Ok(Box::new(move |py: Python<'_>| {
                let (description, name_map) = build_description(py, &raw_cols, lowercase)?;
                let mut guard = shared.lock().unwrap();
                guard.description = description;
                guard.name_map = name_map;
                drop(guard);
                Ok(cursor_obj)
            }) as Finisher)
        })
    }

    fn fetch_future(&self, py: Python<'_>, mode: FetchMode) -> PyResult<Py<PyAny>> {
        self.validate(py)?;
        let shared = self.shared.clone();

        dispatch_future(py, &self.tx, move |_state| {
            let (hstmt, colinfos) = {
                let guard = shared.lock().unwrap();
                if guard.hstmt == 0 || guard.colinfos.is_empty() {
                    return Err(no_results_err());
                }
                (guard.hstmt as HStmt, guard.colinfos.clone())
            };
            let on_err = move |func: &'static str| {
                error_from_handle(func, HandleType::Stmt, hstmt as Handle)
            };

            let limit = match mode {
                FetchMode::One | FetchMode::Val | FetchMode::Next => Some(1),
                FetchMode::Many(n) => Some(n),
                FetchMode::All => None,
                FetchMode::Skip(n) => Some(n),
            };

            let mut rows: Vec<Vec<CellValue>> = Vec::new();
            loop {
                if let Some(n) = limit {
                    if rows.len() >= n {
                        break;
                    }
                }
                let ret = unsafe { odbc_sys::SQLFetch(hstmt) };
                if ret == SqlReturn::NO_DATA {
                    break;
                }
                if !succeeded(ret) {
                    return Err(on_err("SQLFetch"));
                }
                if matches!(mode, FetchMode::Skip(_)) {
                    // Rows are skipped, not read; SQLFetch alone advances.
                    rows.push(Vec::new());
                    continue;
                }
                let mut cells = Vec::with_capacity(colinfos.len());
                for (i, info) in colinfos.iter().enumerate() {
                    cells.push(getdata::get_data(hstmt, i, info, &on_err)?);
                }
                rows.push(cells);
            }

            Ok(Box::new(move |py: Python<'_>| {
                let (description, name_map) = {
                    let guard = shared.lock().unwrap();
                    (
                        guard
                            .description
                            .as_ref()
                            .map(|d| d.clone_ref(py))
                            .unwrap_or_else(|| py.None()),
                        match guard.name_map.as_ref() {
                            Some(m) => m.clone_ref(py),
                            None => PyDict::new(py).unbind(),
                        },
                    )
                };

                let mut py_rows = Vec::with_capacity(rows.len());
                for cells in rows {
                    let values = cells
                        .into_iter()
                        .map(|c| c.into_py(py))
                        .collect::<PyResult<Vec<_>>>()?;
                    py_rows.push(Py::new(
                        py,
                        Row {
                            values,
                            description: description.clone_ref(py),
                            name_map: name_map.clone_ref(py),
                        },
                    )?);
                }

                match mode {
                    FetchMode::One => Ok(match py_rows.into_iter().next() {
                        Some(r) => r.into_any(),
                        None => py.None(),
                    }),
                    FetchMode::Next => match py_rows.into_iter().next() {
                        Some(r) => Ok(r.into_any()),
                        None => Err(PyStopAsyncIteration::new_err(())),
                    },
                    FetchMode::Val => Ok(match py_rows.into_iter().next() {
                        Some(r) => {
                            let row = r.borrow(py);
                            match row.values.first() {
                                Some(v) => v.clone_ref(py),
                                None => py.None(),
                            }
                        }
                        None => py.None(),
                    }),
                    FetchMode::Many(_) | FetchMode::All => {
                        Ok(PyList::new(py, py_rows)?.into_any().unbind())
                    }
                    FetchMode::Skip(_) => Ok(py.None()),
                }
            }) as Finisher)
        })
    }
}

#[pymethods]
impl Cursor {
    #[getter]
    fn connection(&self, py: Python<'_>) -> Py<Connection> {
        self.connection.clone_ref(py)
    }

    #[getter]
    fn description(&self, py: Python<'_>) -> Py<PyAny> {
        let guard = self.shared.lock().unwrap();
        match guard.description.as_ref() {
            Some(d) => d.clone_ref(py),
            None => py.None(),
        }
    }

    #[getter]
    fn rowcount(&self) -> i64 {
        self.shared.lock().unwrap().rowcount
    }

    #[pyo3(signature = (sql, *params))]
    fn execute(
        slf: &Bound<'_, Self>,
        py: Python<'_>,
        sql: &str,
        params: &Bound<'_, PyTuple>,
    ) -> PyResult<Py<PyAny>> {
        Self::execute_on(slf, py, sql, params)
    }

    fn executemany(
        slf: &Bound<'_, Self>,
        py: Python<'_>,
        sql: &str,
        param_rows: &Bound<'_, PyAny>,
    ) -> PyResult<Py<PyAny>> {
        let mut rows = Vec::new();
        for row in param_rows.try_iter()? {
            let row = row?;
            if !is_param_sequence(&row) {
                return Err(PyTypeError::new_err(
                    "Params must be in a list, tuple, or Row",
                ));
            }
            rows.push(extract_param_row(&row)?);
        }
        if rows.is_empty() {
            return Err(ProgrammingError::new_err(
                "The second parameter to executemany must not be empty.",
            ));
        }
        Self::run_execute(slf, py, sql, ExecuteRows::Many(rows))
    }

    fn fetchone(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        self.fetch_future(py, FetchMode::One)
    }

    #[pyo3(signature = (size=None))]
    fn fetchmany(&self, py: Python<'_>, size: Option<usize>) -> PyResult<Py<PyAny>> {
        self.fetch_future(py, FetchMode::Many(size.unwrap_or(self.arraysize)))
    }

    fn fetchall(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        self.fetch_future(py, FetchMode::All)
    }

    fn fetchval(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        self.fetch_future(py, FetchMode::Val)
    }

    fn skip(&self, py: Python<'_>, count: usize) -> PyResult<Py<PyAny>> {
        self.fetch_future(py, FetchMode::Skip(count))
    }

    /// Switch to the next result set.  Resolves to True if one exists.
    fn nextset(slf: &Bound<'_, Self>, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let this = slf.borrow();
        this.validate(py)?;
        let shared = this.shared.clone();
        let lowercase: bool = py
            .import("pyodbc")
            .and_then(|m| m.getattr("lowercase"))
            .and_then(|v| v.extract())
            .unwrap_or(false);

        dispatch_future(py, &this.tx, move |_state| {
            let hstmt = {
                let guard = shared.lock().unwrap();
                if guard.hstmt == 0 {
                    return Err(no_results_err());
                }
                guard.hstmt as HStmt
            };
            let ret = unsafe { odbc_sys::SQLMoreResults(hstmt) };
            if ret == SqlReturn::NO_DATA {
                return Ok(Box::new(move |py: Python<'_>| {
                    Ok(false.into_pyobject(py)?.to_owned().into_any().unbind())
                }) as Finisher);
            }
            if !succeeded(ret) {
                return Err(error_from_handle(
                    "SQLMoreResults",
                    HandleType::Stmt,
                    hstmt as Handle,
                ));
            }

            let raw_cols = describe_columns(hstmt)?;
            {
                let mut guard = shared.lock().unwrap();
                guard.colinfos = raw_cols
                    .iter()
                    .map(|c| ColInfo {
                        sql_type: c.sql_type,
                        column_size: c.column_size,
                    })
                    .collect();
            }

            Ok(Box::new(move |py: Python<'_>| {
                let (description, name_map) = build_description(py, &raw_cols, lowercase)?;
                let mut guard = shared.lock().unwrap();
                guard.description = description;
                guard.name_map = name_map;
                drop(guard);
                Ok(true.into_pyobject(py)?.to_owned().into_any().unbind())
            }) as Finisher)
        })
    }

    fn commit(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        self.validate(py)?;
        Connection::end_tran_future(py, &self.tx, CompletionType::Commit)
    }

    fn rollback(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        self.validate(py)?;
        Connection::end_tran_future(py, &self.tx, CompletionType::Rollback)
    }

    /// Cancel the running statement.  Synchronous by design: SQLCancel is the one
    /// ODBC call intended to be made from another thread while the worker is busy.
    fn cancel(&self, py: Python<'_>) -> PyResult<()> {
        self.validate(py)?;
        let hstmt = self.shared.lock().unwrap().hstmt;
        if hstmt != 0 {
            let ret = unsafe { odbc_sys::SQLCancel(hstmt as HStmt) };
            if !succeeded(ret) {
                return Err(error_from_handle(
                    "SQLCancel",
                    HandleType::Stmt,
                    hstmt as Handle,
                ));
            }
        }
        Ok(())
    }

    fn close(&mut self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        self.validate(py)?;
        self.closed = true;
        let shared = self.shared.clone();
        dispatch_future(py, &self.tx, move |_state| {
            let hstmt = {
                let mut guard = shared.lock().unwrap();
                let h = guard.hstmt;
                guard.hstmt = 0;
                h
            };
            if hstmt != 0 {
                unsafe {
                    let _ = odbc_sys::SQLFreeHandle(HandleType::Stmt, hstmt as Handle);
                }
            }
            Ok(Box::new(|py: Python<'_>| Ok(py.None())) as Finisher)
        })
    }

    fn __aiter__(slf: &Bound<'_, Self>) -> Py<Self> {
        slf.clone().unbind()
    }

    fn __anext__(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        self.fetch_future(py, FetchMode::Next)
    }

    fn __aenter__(slf: &Bound<'_, Self>, py: Python<'_>) -> PyResult<Py<PyAny>> {
        slf.borrow().validate(py)?;
        Ok(async_bridge::resolved_future(py, slf.clone().into_any())?.unbind())
    }

    /// Commits on clean exit unless the connection is in autocommit mode; does not
    /// roll back or close (Cursor_exit in cursor.cpp).
    #[pyo3(signature = (exc_type=None, exc_value=None, traceback=None))]
    fn __aexit__(
        &self,
        py: Python<'_>,
        exc_type: Option<&Bound<'_, PyAny>>,
        exc_value: Option<&Bound<'_, PyAny>>,
        traceback: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let _ = (exc_value, traceback);
        self.validate(py)?;
        let clean_exit = match exc_type {
            None => true,
            Some(t) => t.is_none(),
        };
        let autocommit = self.connection.bind(py).borrow().autocommit();
        if clean_exit && !autocommit {
            Connection::end_tran_future(py, &self.tx, CompletionType::Commit)
        } else {
            Ok(async_bridge::resolved_future(py, py.None().into_bound(py))?.unbind())
        }
    }
}

/// Allocate the statement handle on first use.  Runs on the worker.
fn ensure_hstmt(
    state: &crate::worker::ConnState,
    shared: &Arc<Mutex<CursorShared>>,
) -> PyResult<HStmt> {
    let mut guard = shared.lock().unwrap();
    if guard.hstmt != 0 {
        return Ok(guard.hstmt as HStmt);
    }
    let mut hstmt: Handle = std::ptr::null_mut();
    let ret =
        unsafe { odbc_sys::SQLAllocHandle(HandleType::Stmt, state.hdbc as Handle, &mut hstmt) };
    if !succeeded(ret) {
        return Err(error_from_handle(
            "SQLAllocHandle",
            HandleType::Dbc,
            state.hdbc as Handle,
        ));
    }
    guard.hstmt = hstmt as usize;
    Ok(hstmt as HStmt)
}

/// Discard any previous result set and parameter bindings before re-executing.
fn free_results(hstmt: HStmt) {
    unsafe {
        let _ = odbc_sys::SQLFreeStmt(hstmt, FreeStmtOption::Close);
        let _ = odbc_sys::SQLFreeStmt(hstmt, FreeStmtOption::ResetParams);
    }
}

fn describe_columns(hstmt: HStmt) -> PyResult<Vec<RawCol>> {
    let on_err = |func: &'static str| error_from_handle(func, HandleType::Stmt, hstmt as Handle);

    let mut col_count: i16 = 0;
    let ret = unsafe { odbc_sys::SQLNumResultCols(hstmt, &mut col_count) };
    if !succeeded(ret) {
        return Err(on_err("SQLNumResultCols"));
    }

    let mut cols = Vec::with_capacity(col_count.max(0) as usize);
    for i in 0..col_count.max(0) as u16 {
        let mut name_buf = vec![0u16; 300];
        let mut name_len: i16 = 0;
        let mut data_type = SqlDataType::UNKNOWN_TYPE;
        let mut column_size: usize = 0;
        let mut decimal_digits: i16 = 0;
        let mut nullable = Nullability::UNKNOWN;

        loop {
            let ret = unsafe {
                odbc_sys::SQLDescribeColW(
                    hstmt,
                    i + 1,
                    name_buf.as_mut_ptr(),
                    name_buf.len() as i16,
                    &mut name_len,
                    &mut data_type,
                    &mut column_size,
                    &mut decimal_digits,
                    &mut nullable,
                )
            };
            if !succeeded(ret) {
                return Err(on_err("SQLDescribeCol"));
            }
            if name_len as usize > name_buf.len() - 1 {
                name_buf = vec![0u16; name_len as usize + 1];
                continue;
            }
            break;
        }

        let name = String::from_utf16_lossy(&name_buf[..name_len.max(0) as usize]);
        cols.push(RawCol {
            name,
            sql_type: data_type.0,
            column_size: column_size as u64,
            decimal_digits,
            nullable,
        });
    }
    Ok(cols)
}

/// Build the DB API description tuple and the shared name->index map.  GIL held.
type DescriptionAndMap = (Option<Py<PyAny>>, Option<Py<PyDict>>);

fn build_description(
    py: Python<'_>,
    raw_cols: &[RawCol],
    lowercase: bool,
) -> PyResult<DescriptionAndMap> {
    if raw_cols.is_empty() {
        return Ok((None, None));
    }
    let map = PyDict::new(py);
    let mut items = Vec::with_capacity(raw_cols.len());
    for (i, col) in raw_cols.iter().enumerate() {
        let name = if lowercase {
            col.name.to_lowercase()
        } else {
            col.name.clone()
        };
        let type_obj = getdata::python_type_for_sql_type(py, col.sql_type)?;
        let nullable: Py<PyAny> = match col.nullable {
            Nullability::NO_NULLS => false.into_pyobject(py)?.to_owned().into_any().unbind(),
            Nullability::NULLABLE => true.into_pyobject(py)?.to_owned().into_any().unbind(),
            _ => py.None(),
        };
        let info = (
            name.clone(),
            type_obj,
            py.None(),
            col.column_size,
            col.column_size,
            col.decimal_digits,
            nullable,
        )
            .into_pyobject(py)?;
        map.set_item(name, i)?;
        items.push(info);
    }
    let desc = PyTuple::new(py, items)?;
    Ok((Some(desc.into_any().unbind()), Some(map.unbind())))
}

/// Prepare/bind/execute on the worker.  Returns (rowcount, columns).
fn execute_odbc(hstmt: HStmt, sql_w: &[u16], rows: ExecuteRows) -> PyResult<(i64, Vec<RawCol>)> {
    let on_err = |func: &'static str| error_from_handle(func, HandleType::Stmt, hstmt as Handle);

    match rows {
        ExecuteRows::One(values) if values.is_empty() => {
            let ret =
                unsafe { odbc_sys::SQLExecDirectW(hstmt, sql_w.as_ptr(), sql_w.len() as i32) };
            if !succeeded(ret) && ret != SqlReturn::NO_DATA {
                return Err(on_err("SQLExecDirectW"));
            }
        }
        ExecuteRows::One(values) => {
            let ret = unsafe { odbc_sys::SQLPrepareW(hstmt, sql_w.as_ptr(), sql_w.len() as i32) };
            if !succeeded(ret) {
                return Err(on_err("SQLPrepare"));
            }
            let mut bound: Vec<BoundParam> = Vec::with_capacity(values.len());
            for (i, value) in values.into_iter().enumerate() {
                bound.push(params::bind(hstmt, i, value, on_err)?);
            }
            let ret = unsafe { odbc_sys::SQLExecute(hstmt) };
            if !succeeded(ret) && ret != SqlReturn::NO_DATA {
                return Err(on_err("SQLExecute"));
            }
            drop(bound); // buffers must outlive SQLExecute
        }
        ExecuteRows::Many(param_rows) => {
            let ret = unsafe { odbc_sys::SQLPrepareW(hstmt, sql_w.as_ptr(), sql_w.len() as i32) };
            if !succeeded(ret) {
                return Err(on_err("SQLPrepare"));
            }
            for values in param_rows {
                unsafe {
                    let _ = odbc_sys::SQLFreeStmt(hstmt, FreeStmtOption::ResetParams);
                }
                let mut bound: Vec<BoundParam> = Vec::with_capacity(values.len());
                for (i, value) in values.into_iter().enumerate() {
                    bound.push(params::bind(hstmt, i, value, on_err)?);
                }
                let ret = unsafe { odbc_sys::SQLExecute(hstmt) };
                if !succeeded(ret) && ret != SqlReturn::NO_DATA {
                    return Err(on_err("SQLExecute"));
                }
                drop(bound);
            }
        }
    }

    let mut rowcount: Len = 0;
    let ret = unsafe { odbc_sys::SQLRowCount(hstmt, &mut rowcount) };
    if !succeeded(ret) {
        return Err(on_err("SQLRowCount"));
    }

    let cols = describe_columns(hstmt)?;
    Ok((rowcount as i64, cols))
}
