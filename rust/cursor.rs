// The Cursor type.  Ported from src/cursor.cpp on the worker-thread model: every
// ODBC call is enqueued on the parent connection's worker and returned to Python as
// an asyncio future.  The HSTMT is allocated lazily on first use.

use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use odbc_sys::{
    CompletionType, Desc, FreeStmtOption, HDesc, HStmt, Handle, HandleType, Len, Nullability,
    Pointer, SqlDataType, SqlReturn, StatementAttribute,
};
use pyo3::exceptions::{PyStopAsyncIteration, PyTypeError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList, PyTuple};

use crate::async_bridge;
use crate::connection::{Connection, ConverterMap};
use crate::errors::{error_from_handle, error_from_handle_ex, ProgrammingError};
use crate::getdata::{self, CellValue, FetchCtx};
use crate::params::{self, BoundParam, ParamValue};
use crate::row::Row;
use crate::textenc::{self, DecodedText, TextEnc};
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

// SQL_C_NUMERIC, for the decimal ARD setup (sqlext.h).
const SQL_C_NUMERIC: usize = 2;

/// Metadata for one result column, produced on the worker after an execute.
#[derive(Clone)]
pub struct ColInfo {
    pub sql_type: i16,
    pub column_size: u64,
    pub use_decimal_binary: bool,
    pub is_unsigned: bool,
}

/// Raw column description read via SQLDescribeColW, converted into the Python
/// `description` tuple under the GIL by the execute finisher.
struct RawCol {
    name: DecodedText,
    sql_type: i16,
    column_size: u64,
    decimal_digits: i16,
    nullable: Nullability,
    use_decimal_binary: bool,
    is_unsigned: bool,
}

/// One diagnostic record for Cursor.messages.
struct RawDiag {
    state: String,
    native: i32,
    text: MsgText,
}

enum MsgText {
    Decoded(DecodedText),
    Raw(Vec<u8>),
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
    pub messages: Option<Py<PyAny>>,
}

#[pyclass(module = "pyodbc")]
pub struct Cursor {
    tx: Sender<Task>,
    connection: Py<Connection>,
    shared: Arc<Mutex<CursorShared>>,
    closed: bool,
    #[pyo3(get, set)]
    arraysize: usize,
    /// If True, rows are returned as dicts keyed by column name.
    #[pyo3(get, set)]
    rows_as_dicts: bool,
    /// If True, executemany binds all rows as parameter arrays in one execute.
    #[pyo3(get, set)]
    fast_executemany: bool,
    /// Overrides from setinputsizes, one entry per parameter position.
    inputsizes: Vec<Option<params::InputSize>>,
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

/// Everything the worker needs for one execute, snapshotted under the GIL.
struct ExecCtx {
    sql_bytes: Vec<u8>,
    sql_wide: bool,
    maxwrite: usize,
    metadata_enc: TextEnc,
    fetch_decimal_as_string: bool,
    byte_len_diag: bool,
    inputsizes: Vec<Option<params::InputSize>>,
    fast_executemany: bool,
}

/// Ported from IsSequence in cursor.cpp: only list, tuple, and Row count as a
/// parameter collection.
fn is_param_sequence(obj: &Bound<'_, PyAny>) -> bool {
    obj.is_instance_of::<PyList>() || obj.is_instance_of::<PyTuple>() || obj.is_instance_of::<Row>()
}

fn extract_param_row(row: &Bound<'_, PyAny>, unicode_enc: &TextEnc) -> PyResult<Vec<ParamValue>> {
    let mut out = Vec::new();
    for (i, cell) in row.try_iter()?.enumerate() {
        out.push(params::extract(&cell?, i, unicode_enc)?);
    }
    Ok(out)
}

fn module_flag(py: Python<'_>, name: &str) -> bool {
    py.import("pyodbc")
        .and_then(|m| m.getattr(name))
        .and_then(|v| v.extract())
        .unwrap_or(false)
}

impl Cursor {
    pub fn new(tx: Sender<Task>, connection: Py<Connection>) -> Self {
        Cursor {
            tx,
            connection,
            shared: Arc::new(Mutex::new(CursorShared::default())),
            closed: false,
            arraysize: 1,
            rows_as_dicts: false,
            fast_executemany: false,
            inputsizes: Vec::new(),
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

    fn fetch_ctx(&self, py: Python<'_>) -> FetchCtx {
        let conn = self.connection.bind(py).borrow();
        let encs = conn.encodings_snapshot();
        FetchCtx {
            sqlchar_enc: encs.sqlchar,
            sqlwchar_enc: encs.sqlwchar,
            initsize: conn.readvar_initsize_value(),
            native_uuid: module_flag(py, "native_uuid"),
            converter_types: conn.converter_types(),
        }
    }

    /// Shared by Cursor.execute and Connection.execute.
    pub fn execute_on(
        slf: &Bound<'_, Cursor>,
        py: Python<'_>,
        sql: &str,
        params_tuple: &Bound<'_, PyTuple>,
    ) -> PyResult<Py<PyAny>> {
        let unicode_enc = slf
            .borrow()
            .connection
            .bind(py)
            .borrow()
            .encodings_snapshot()
            .unicode;
        // Figure out how parameters were passed (cursor.cpp Cursor_execute): a
        // single list/tuple/Row argument is the parameter collection; otherwise
        // the positional arguments themselves are the parameters.
        let values = if params_tuple.len() == 1 {
            let first = params_tuple.get_item(0)?;
            if is_param_sequence(&first) {
                extract_param_row(&first, &unicode_enc)?
            } else {
                extract_param_row(params_tuple.as_any(), &unicode_enc)?
            }
        } else {
            extract_param_row(params_tuple.as_any(), &unicode_enc)?
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

        let lowercase = module_flag(py, "lowercase");
        let native_uuid = module_flag(py, "native_uuid");

        let (ctx, converter_types) = {
            let conn = this.connection.bind(py).borrow();
            let encs = conn.encodings_snapshot();
            let sql_bytes = textenc::encode(py, sql, &encs.unicode)?;
            (
                ExecCtx {
                    sql_bytes,
                    sql_wide: encs.unicode.wide,
                    maxwrite: conn.maxwrite_setting(),
                    metadata_enc: encs.metadata,
                    fetch_decimal_as_string: conn.fetch_decimal_as_string_value(),
                    byte_len_diag: conn.diagrec_byte_length(),
                    inputsizes: this.inputsizes.clone(),
                    fast_executemany: this.fast_executemany,
                },
                conn.converter_types(),
            )
        };

        let shared = this.shared.clone();
        let cursor_obj: Py<PyAny> = slf.clone().into_any().unbind();

        dispatch_future(py, &this.tx, move |state| {
            if state.hdbc == 0 {
                return Err(crate::worker::closed_connection_err());
            }
            let hstmt = ensure_hstmt(state, &shared)?;
            free_results(hstmt);

            let cnxninfo = state.cnxninfo.clone();
            let (rowcount, raw_cols, diags) = execute_odbc(hstmt, &ctx, rows, cnxninfo)?;

            {
                let mut guard = shared.lock().unwrap();
                guard.colinfos = raw_cols
                    .iter()
                    .map(|c| ColInfo {
                        sql_type: c.sql_type,
                        column_size: c.column_size,
                        use_decimal_binary: c.use_decimal_binary,
                        is_unsigned: c.is_unsigned,
                    })
                    .collect();
                guard.rowcount = rowcount;
            }

            Ok(Box::new(move |py: Python<'_>| {
                let (description, name_map) =
                    build_description(py, &raw_cols, lowercase, native_uuid, &converter_types)?;
                let messages = build_messages(py, diags)?;
                let mut guard = shared.lock().unwrap();
                guard.description = description;
                guard.name_map = name_map;
                guard.messages = Some(messages);
                drop(guard);
                Ok(cursor_obj)
            }) as Finisher)
        })
    }

    fn fetch_future(&self, py: Python<'_>, mode: FetchMode) -> PyResult<Py<PyAny>> {
        self.validate(py)?;
        let shared = self.shared.clone();
        let ctx = self.fetch_ctx(py);
        let converters: ConverterMap = self.connection.bind(py).borrow().converter_map();
        let as_dicts = self.rows_as_dicts;

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
                    cells.push(getdata::get_data(hstmt, i, info, &ctx, &on_err)?);
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

                let cell_to_py = |py: Python<'_>, cell: CellValue| -> PyResult<Py<PyAny>> {
                    match cell {
                        CellValue::Converted { sql_type, data } => {
                            call_converter(py, &converters, sql_type, data)
                        }
                        other => other.into_py(py),
                    }
                };

                let mut py_rows: Vec<Py<PyAny>> = Vec::with_capacity(rows.len());
                for cells in rows {
                    let values = cells
                        .into_iter()
                        .map(|c| cell_to_py(py, c))
                        .collect::<PyResult<Vec<_>>>()?;
                    if as_dicts && !matches!(mode, FetchMode::Val | FetchMode::Skip(_)) {
                        // Ticket #171: rows as dicts, keyed by column name from the
                        // description.
                        let d = PyDict::new(py);
                        let desc = description.bind(py);
                        for (i, value) in values.iter().enumerate() {
                            let name = desc.get_item(i)?.get_item(0)?;
                            d.set_item(name, value)?;
                        }
                        py_rows.push(d.into_any().unbind());
                    } else {
                        py_rows.push(
                            Py::new(
                                py,
                                Row {
                                    values,
                                    description: description.clone_ref(py),
                                    name_map: name_map.clone_ref(py),
                                },
                            )?
                            .into_any(),
                        );
                    }
                }

                match mode {
                    FetchMode::One => Ok(match py_rows.into_iter().next() {
                        Some(r) => r,
                        None => py.None(),
                    }),
                    FetchMode::Next => match py_rows.into_iter().next() {
                        Some(r) => Ok(r),
                        None => Err(PyStopAsyncIteration::new_err(())),
                    },
                    FetchMode::Val => Ok(match py_rows.into_iter().next() {
                        Some(r) => {
                            let row_ref = r.bind(py);
                            if let Ok(row) = row_ref.downcast::<Row>() {
                                match row.borrow().values.first() {
                                    Some(v) => v.clone_ref(py),
                                    None => py.None(),
                                }
                            } else {
                                py.None()
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

    /// Diagnostic messages from the last execute (e.g. PRINT output), or None.
    #[getter]
    fn messages(&self, py: Python<'_>) -> Py<PyAny> {
        let guard = self.shared.lock().unwrap();
        match guard.messages.as_ref() {
            Some(m) => m.clone_ref(py),
            None => py.None(),
        }
    }

    /// The raw ODBC statement handle as ctypes.c_void_p, or None once closed.
    #[getter]
    fn hstmt(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        if self.closed {
            return Ok(py.None());
        }
        let value = self.shared.lock().unwrap().hstmt;
        Ok(py
            .import("ctypes")?
            .getattr("c_void_p")?
            .call1((value,))?
            .unbind())
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
        // Ported from Cursor_executemany: only sequences, iterators, and
        // generators are accepted as the parameter collection.
        let acceptable = is_param_sequence(param_rows)
            || param_rows.hasattr("__next__")?
            || param_rows.hasattr("gi_frame")?;
        if !acceptable {
            return Err(ProgrammingError::new_err(
                "The second parameter to executemany must be a sequence, iterator, or generator.",
            ));
        }
        let unicode_enc = slf
            .borrow()
            .connection
            .bind(py)
            .borrow()
            .encodings_snapshot()
            .unicode;
        let mut rows = Vec::new();
        for row in param_rows.try_iter()? {
            let row = row?;
            if !is_param_sequence(&row) {
                return Err(PyTypeError::new_err(
                    "Params must be in a list, tuple, or Row",
                ));
            }
            rows.push(extract_param_row(&row, &unicode_enc)?);
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

    /// Declare parameter types/sizes ahead of an execute; None clears them.
    /// Entries may be an int (column size) or a tuple (sqltype, size, scale).
    #[pyo3(signature = (sizes, /))]
    fn setinputsizes(&mut self, sizes: Option<Bound<'_, PyAny>>) -> PyResult<()> {
        let Some(sizes) = sizes else {
            self.inputsizes.clear();
            return Ok(());
        };
        let iterable = sizes.is_instance_of::<PyList>()
            || sizes.is_instance_of::<PyTuple>()
            || sizes.hasattr("__next__")?
            || sizes.hasattr("gi_frame")?;
        if !iterable {
            return Err(ProgrammingError::new_err(
                "A non-None parameter to setinputsizes must be a sequence, iterator, or generator.",
            ));
        }
        let mut out = Vec::new();
        for entry in sizes.try_iter()? {
            let entry = entry?;
            if entry.is_none() {
                out.push(None);
            } else if let Ok(size) = entry.extract::<u64>() {
                out.push(Some(params::InputSize {
                    column_size: Some(size),
                    ..Default::default()
                }));
            } else {
                // (sqltype, size, scale) with trailing entries optional.
                let tup = entry;
                let mut over = params::InputSize::default();
                if let Ok(t) = tup.get_item(0) {
                    if !t.is_none() {
                        over.sqltype = Some(t.extract::<i32>()? as i16);
                    }
                }
                if let Ok(s) = tup.get_item(1) {
                    if !s.is_none() {
                        over.column_size = Some(s.extract()?);
                    }
                }
                if let Ok(sc) = tup.get_item(2) {
                    if !sc.is_none() {
                        over.scale = Some(sc.extract()?);
                    }
                }
                out.push(Some(over));
            }
        }
        self.inputsizes = out;
        Ok(())
    }

    /// Whether the driver scans SQL for escape sequences (SQL_ATTR_NOSCAN,
    /// inverted).  Reads/writes the live statement attribute like cursor.cpp.
    #[getter]
    fn noscan(&self, py: Python<'_>) -> PyResult<bool> {
        self.validate(py)?;
        let shared = self.shared.clone();
        crate::worker::dispatch_sync(py, &self.tx, move |_state| {
            let hstmt = shared.lock().unwrap().hstmt;
            if hstmt == 0 {
                return Ok(false); // no statement yet: the default is off
            }
            let mut value: usize = 0;
            let ret = unsafe {
                odbc_sys::SQLGetStmtAttr(
                    hstmt as HStmt,
                    StatementAttribute::NoScan,
                    &mut value as *mut usize as Pointer,
                    std::mem::size_of::<usize>() as i32,
                    std::ptr::null_mut(),
                )
            };
            // Not supported?  Assume 'no', like Cursor_getnoscan.
            Ok(succeeded(ret) && value != 0)
        })
    }

    #[setter]
    fn set_noscan(&self, py: Python<'_>, value: bool) -> PyResult<()> {
        self.validate(py)?;
        let shared = self.shared.clone();
        crate::worker::dispatch_sync(py, &self.tx, move |state| {
            let hstmt = ensure_hstmt(state, &shared)?;
            let ret = unsafe {
                odbc_sys::SQLSetStmtAttr(
                    hstmt,
                    StatementAttribute::NoScan,
                    usize::from(value) as Pointer,
                    0,
                )
            };
            if !succeeded(ret) {
                return Err(error_from_handle(
                    "SQLSetStmtAttr(SQL_ATTR_NOSCAN)",
                    HandleType::Stmt,
                    hstmt as Handle,
                ));
            }
            Ok(())
        })
    }

    // -- catalog functions ---------------------------------------------------

    #[pyo3(signature = (table=None, catalog=None, schema=None, tableType=None))]
    #[allow(non_snake_case)]
    fn tables(
        slf: &Bound<'_, Self>,
        py: Python<'_>,
        table: Option<&str>,
        catalog: Option<&str>,
        schema: Option<&str>,
        tableType: Option<&str>,
    ) -> PyResult<Py<PyAny>> {
        Self::run_catalog(
            slf,
            py,
            CatalogCall::Tables(optw(catalog), optw(schema), optw(table), optw(tableType)),
        )
    }

    #[pyo3(name = "tablePrivileges", signature = (table=None, catalog=None, schema=None))]
    fn table_privileges(
        slf: &Bound<'_, Self>,
        py: Python<'_>,
        table: Option<&str>,
        catalog: Option<&str>,
        schema: Option<&str>,
    ) -> PyResult<Py<PyAny>> {
        Self::run_catalog(
            slf,
            py,
            CatalogCall::TablePrivileges(optw(catalog), optw(schema), optw(table)),
        )
    }

    #[pyo3(signature = (table=None, catalog=None, schema=None, column=None))]
    fn columns(
        slf: &Bound<'_, Self>,
        py: Python<'_>,
        table: Option<&str>,
        catalog: Option<&str>,
        schema: Option<&str>,
        column: Option<&str>,
    ) -> PyResult<Py<PyAny>> {
        Self::run_catalog(
            slf,
            py,
            CatalogCall::Columns(optw(catalog), optw(schema), optw(table), optw(column)),
        )
    }

    #[pyo3(signature = (table, catalog=None, schema=None, unique=false, quick=true))]
    fn statistics(
        slf: &Bound<'_, Self>,
        py: Python<'_>,
        table: &str,
        catalog: Option<&str>,
        schema: Option<&str>,
        unique: bool,
        quick: bool,
    ) -> PyResult<Py<PyAny>> {
        // SQL_INDEX_UNIQUE=0 / SQL_INDEX_ALL=1; SQL_QUICK=0 / SQL_ENSURE=1
        let unique_flag = if unique { 0 } else { 1 };
        let quick_flag = if quick { 0 } else { 1 };
        Self::run_catalog(
            slf,
            py,
            CatalogCall::Statistics(
                optw(catalog),
                optw(schema),
                optw(Some(table)),
                unique_flag,
                quick_flag,
            ),
        )
    }

    #[pyo3(name = "rowIdColumns", signature = (table, catalog=None, schema=None, nullable=true))]
    fn row_id_columns(
        slf: &Bound<'_, Self>,
        py: Python<'_>,
        table: &str,
        catalog: Option<&str>,
        schema: Option<&str>,
        nullable: bool,
    ) -> PyResult<Py<PyAny>> {
        // SQL_BEST_ROWID = 1
        Self::run_catalog(
            slf,
            py,
            CatalogCall::SpecialColumns(
                1,
                optw(catalog),
                optw(schema),
                optw(Some(table)),
                u16::from(nullable),
            ),
        )
    }

    #[pyo3(name = "rowVerColumns", signature = (table, catalog=None, schema=None, nullable=true))]
    fn row_ver_columns(
        slf: &Bound<'_, Self>,
        py: Python<'_>,
        table: &str,
        catalog: Option<&str>,
        schema: Option<&str>,
        nullable: bool,
    ) -> PyResult<Py<PyAny>> {
        // SQL_ROWVER = 2
        Self::run_catalog(
            slf,
            py,
            CatalogCall::SpecialColumns(
                2,
                optw(catalog),
                optw(schema),
                optw(Some(table)),
                u16::from(nullable),
            ),
        )
    }

    #[pyo3(name = "primaryKeys", signature = (table, catalog=None, schema=None))]
    fn primary_keys(
        slf: &Bound<'_, Self>,
        py: Python<'_>,
        table: &str,
        catalog: Option<&str>,
        schema: Option<&str>,
    ) -> PyResult<Py<PyAny>> {
        Self::run_catalog(
            slf,
            py,
            CatalogCall::PrimaryKeys(optw(catalog), optw(schema), optw(Some(table))),
        )
    }

    #[pyo3(name = "foreignKeys", signature = (table=None, catalog=None, schema=None, foreignTable=None, foreignCatalog=None, foreignSchema=None))]
    #[allow(non_snake_case)]
    #[allow(clippy::too_many_arguments)]
    fn foreign_keys(
        slf: &Bound<'_, Self>,
        py: Python<'_>,
        table: Option<&str>,
        catalog: Option<&str>,
        schema: Option<&str>,
        foreignTable: Option<&str>,
        foreignCatalog: Option<&str>,
        foreignSchema: Option<&str>,
    ) -> PyResult<Py<PyAny>> {
        Self::run_catalog(
            slf,
            py,
            CatalogCall::ForeignKeys(
                optw(catalog),
                optw(schema),
                optw(table),
                optw(foreignCatalog),
                optw(foreignSchema),
                optw(foreignTable),
            ),
        )
    }

    #[pyo3(signature = (procedure=None, catalog=None, schema=None))]
    fn procedures(
        slf: &Bound<'_, Self>,
        py: Python<'_>,
        procedure: Option<&str>,
        catalog: Option<&str>,
        schema: Option<&str>,
    ) -> PyResult<Py<PyAny>> {
        Self::run_catalog(
            slf,
            py,
            CatalogCall::Procedures(optw(catalog), optw(schema), optw(procedure)),
        )
    }

    #[pyo3(name = "procedureColumns", signature = (procedure=None, catalog=None, schema=None))]
    fn procedure_columns(
        slf: &Bound<'_, Self>,
        py: Python<'_>,
        procedure: Option<&str>,
        catalog: Option<&str>,
        schema: Option<&str>,
    ) -> PyResult<Py<PyAny>> {
        Self::run_catalog(
            slf,
            py,
            CatalogCall::ProcedureColumns(optw(catalog), optw(schema), optw(procedure), None),
        )
    }

    #[pyo3(name = "getTypeInfo", signature = (sqlType=None, /))]
    #[allow(non_snake_case)]
    fn get_type_info(
        slf: &Bound<'_, Self>,
        py: Python<'_>,
        sqlType: Option<i32>,
    ) -> PyResult<Py<PyAny>> {
        // SQL_ALL_TYPES = 0
        Self::run_catalog(slf, py, CatalogCall::TypeInfo(sqlType.unwrap_or(0) as i16))
    }

    /// Switch to the next result set.  Resolves to True if one exists.
    fn nextset(slf: &Bound<'_, Self>, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let this = slf.borrow();
        this.validate(py)?;
        let shared = this.shared.clone();
        let lowercase = module_flag(py, "lowercase");
        let native_uuid = module_flag(py, "native_uuid");
        let (metadata_enc, fetch_dec_str, byte_len, converter_types) = {
            let conn = this.connection.bind(py).borrow();
            (
                conn.encodings_snapshot().metadata,
                conn.fetch_decimal_as_string_value(),
                conn.diagrec_byte_length(),
                conn.converter_types(),
            )
        };

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
                // Like Cursor_nextset: free_results clears the result state and
                // resets messages before returning False.
                shared.lock().unwrap().colinfos.clear();
                return Ok(Box::new(move |py: Python<'_>| {
                    let mut guard = shared.lock().unwrap();
                    guard.description = None;
                    guard.name_map = None;
                    guard.messages = Some(PyList::empty(py).into_any().unbind());
                    drop(guard);
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
            let diags = if ret == SqlReturn::SUCCESS_WITH_INFO {
                collect_diag(hstmt, &metadata_enc, byte_len)
            } else {
                Vec::new()
            };

            let raw_cols = describe_columns(hstmt, &metadata_enc, fetch_dec_str)?;
            {
                let mut guard = shared.lock().unwrap();
                guard.colinfos = raw_cols
                    .iter()
                    .map(|c| ColInfo {
                        sql_type: c.sql_type,
                        column_size: c.column_size,
                        use_decimal_binary: c.use_decimal_binary,
                        is_unsigned: c.is_unsigned,
                    })
                    .collect();
            }

            Ok(Box::new(move |py: Python<'_>| {
                let (description, name_map) =
                    build_description(py, &raw_cols, lowercase, native_uuid, &converter_types)?;
                let messages = build_messages(py, diags)?;
                let mut guard = shared.lock().unwrap();
                guard.description = description;
                guard.name_map = name_map;
                guard.messages = Some(messages);
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

// ---------------------------------------------------------------------------
// Catalog functions (SQLTables, SQLColumns, ...)
// ---------------------------------------------------------------------------

// W catalog entry points missing from odbc-sys.
extern "system" {
    fn SQLStatisticsW(
        hstmt: HStmt,
        catalog: *const u16,
        catalog_len: i16,
        schema: *const u16,
        schema_len: i16,
        table: *const u16,
        table_len: i16,
        unique: u16,
        reserved: u16,
    ) -> SqlReturn;
    fn SQLPrimaryKeysW(
        hstmt: HStmt,
        catalog: *const u16,
        catalog_len: i16,
        schema: *const u16,
        schema_len: i16,
        table: *const u16,
        table_len: i16,
    ) -> SqlReturn;
    fn SQLProceduresW(
        hstmt: HStmt,
        catalog: *const u16,
        catalog_len: i16,
        schema: *const u16,
        schema_len: i16,
        proc_name: *const u16,
        proc_len: i16,
    ) -> SqlReturn;
    fn SQLProcedureColumnsW(
        hstmt: HStmt,
        catalog: *const u16,
        catalog_len: i16,
        schema: *const u16,
        schema_len: i16,
        proc_name: *const u16,
        proc_len: i16,
        column: *const u16,
        column_len: i16,
    ) -> SqlReturn;
    fn SQLTablePrivilegesW(
        hstmt: HStmt,
        catalog: *const u16,
        catalog_len: i16,
        schema: *const u16,
        schema_len: i16,
        table: *const u16,
        table_len: i16,
    ) -> SqlReturn;
    fn SQLSpecialColumnsW(
        hstmt: HStmt,
        identifier_type: u16,
        catalog: *const u16,
        catalog_len: i16,
        schema: *const u16,
        schema_len: i16,
        table: *const u16,
        table_len: i16,
        scope: u16,
        nullable: u16,
    ) -> SqlReturn;
}

type OptW = Option<Vec<u16>>;

fn optw(s: Option<&str>) -> OptW {
    s.map(|s| s.encode_utf16().collect())
}

fn wptr(s: &OptW) -> (*const u16, i16) {
    match s {
        Some(v) => (v.as_ptr(), v.len() as i16),
        None => (std::ptr::null(), 0),
    }
}

/// One catalog call, with pre-encoded (UTF-16) string arguments.
enum CatalogCall {
    Tables(OptW, OptW, OptW, OptW),         // catalog, schema, table, type
    Columns(OptW, OptW, OptW, OptW),        // catalog, schema, table, column
    Statistics(OptW, OptW, OptW, u16, u16), // ..., unique, quick
    PrimaryKeys(OptW, OptW, OptW),
    ForeignKeys(OptW, OptW, OptW, OptW, OptW, OptW), // pk cat/schema/table, fk ...
    Procedures(OptW, OptW, OptW),
    ProcedureColumns(OptW, OptW, OptW, OptW),
    TablePrivileges(OptW, OptW, OptW),
    SpecialColumns(u16, OptW, OptW, OptW, u16), // id_type, ..., nullable
    TypeInfo(i16),
}

impl CatalogCall {
    fn function_name(&self) -> &'static str {
        match self {
            CatalogCall::Tables(..) => "SQLTables",
            CatalogCall::Columns(..) => "SQLColumns",
            CatalogCall::Statistics(..) => "SQLStatistics",
            CatalogCall::PrimaryKeys(..) => "SQLPrimaryKeys",
            CatalogCall::ForeignKeys(..) => "SQLForeignKeys",
            CatalogCall::Procedures(..) => "SQLProcedures",
            CatalogCall::ProcedureColumns(..) => "SQLProcedureColumns",
            CatalogCall::TablePrivileges(..) => "SQLTablePrivileges",
            CatalogCall::SpecialColumns(..) => "SQLSpecialColumns",
            CatalogCall::TypeInfo(..) => "SQLGetTypeInfo",
        }
    }

    fn run(&self, hstmt: HStmt) -> SqlReturn {
        unsafe {
            match self {
                CatalogCall::Tables(cat, sch, tbl, ty) => {
                    let (c, cl) = wptr(cat);
                    let (s, sl) = wptr(sch);
                    let (t, tl) = wptr(tbl);
                    let (y, yl) = wptr(ty);
                    odbc_sys::SQLTablesW(hstmt, c, cl, s, sl, t, tl, y, yl)
                }
                CatalogCall::Columns(cat, sch, tbl, col) => {
                    let (c, cl) = wptr(cat);
                    let (s, sl) = wptr(sch);
                    let (t, tl) = wptr(tbl);
                    let (o, ol) = wptr(col);
                    odbc_sys::SQLColumnsW(hstmt, c, cl, s, sl, t, tl, o, ol)
                }
                CatalogCall::Statistics(cat, sch, tbl, unique, quick) => {
                    let (c, cl) = wptr(cat);
                    let (s, sl) = wptr(sch);
                    let (t, tl) = wptr(tbl);
                    SQLStatisticsW(hstmt, c, cl, s, sl, t, tl, *unique, *quick)
                }
                CatalogCall::PrimaryKeys(cat, sch, tbl) => {
                    let (c, cl) = wptr(cat);
                    let (s, sl) = wptr(sch);
                    let (t, tl) = wptr(tbl);
                    SQLPrimaryKeysW(hstmt, c, cl, s, sl, t, tl)
                }
                CatalogCall::ForeignKeys(pc, ps, pt, fc, fs, ft) => {
                    let (a, al) = wptr(pc);
                    let (b, bl) = wptr(ps);
                    let (c, cl) = wptr(pt);
                    let (d, dl) = wptr(fc);
                    let (e, el) = wptr(fs);
                    let (f, fl) = wptr(ft);
                    odbc_sys::SQLForeignKeysW(hstmt, a, al, b, bl, c, cl, d, dl, e, el, f, fl)
                }
                CatalogCall::Procedures(cat, sch, proc) => {
                    let (c, cl) = wptr(cat);
                    let (s, sl) = wptr(sch);
                    let (p, pl) = wptr(proc);
                    SQLProceduresW(hstmt, c, cl, s, sl, p, pl)
                }
                CatalogCall::ProcedureColumns(cat, sch, proc, col) => {
                    let (c, cl) = wptr(cat);
                    let (s, sl) = wptr(sch);
                    let (p, pl) = wptr(proc);
                    let (o, ol) = wptr(col);
                    SQLProcedureColumnsW(hstmt, c, cl, s, sl, p, pl, o, ol)
                }
                CatalogCall::TablePrivileges(cat, sch, tbl) => {
                    let (c, cl) = wptr(cat);
                    let (s, sl) = wptr(sch);
                    let (t, tl) = wptr(tbl);
                    SQLTablePrivilegesW(hstmt, c, cl, s, sl, t, tl)
                }
                CatalogCall::SpecialColumns(id_type, cat, sch, tbl, nullable) => {
                    let (c, cl) = wptr(cat);
                    let (s, sl) = wptr(sch);
                    let (t, tl) = wptr(tbl);
                    // scope: SQL_SCOPE_CURROW (0)
                    SQLSpecialColumnsW(hstmt, *id_type, c, cl, s, sl, t, tl, 0, *nullable)
                }
                CatalogCall::TypeInfo(sqltype) => {
                    odbc_sys::SQLGetTypeInfo(hstmt, SqlDataType(*sqltype))
                }
            }
        }
    }
}

impl Cursor {
    /// Run a catalog function and expose its result set on this cursor, like an
    /// execute.  Resolves to the cursor.
    fn run_catalog(
        slf: &Bound<'_, Cursor>,
        py: Python<'_>,
        call: CatalogCall,
    ) -> PyResult<Py<PyAny>> {
        let this = slf.borrow();
        this.validate(py)?;

        let native_uuid = module_flag(py, "native_uuid");
        let (metadata_enc, fetch_dec_str, converter_types) = {
            let conn = this.connection.bind(py).borrow();
            (
                conn.encodings_snapshot().metadata,
                conn.fetch_decimal_as_string_value(),
                conn.converter_types(),
            )
        };

        let shared = this.shared.clone();
        let cursor_obj: Py<PyAny> = slf.clone().into_any().unbind();

        dispatch_future(py, &this.tx, move |state| {
            if state.hdbc == 0 {
                return Err(crate::worker::closed_connection_err());
            }
            let hstmt = ensure_hstmt(state, &shared)?;
            free_results(hstmt);

            let ret = call.run(hstmt);
            if !succeeded(ret) {
                return Err(error_from_handle(
                    call.function_name(),
                    HandleType::Stmt,
                    hstmt as Handle,
                ));
            }

            let mut rowcount: Len = 0;
            let ret = unsafe { odbc_sys::SQLRowCount(hstmt, &mut rowcount) };
            if !succeeded(ret) {
                return Err(error_from_handle(
                    "SQLRowCount",
                    HandleType::Stmt,
                    hstmt as Handle,
                ));
            }

            let raw_cols = describe_columns(hstmt, &metadata_enc, fetch_dec_str)?;
            {
                let mut guard = shared.lock().unwrap();
                guard.colinfos = raw_cols
                    .iter()
                    .map(|c| ColInfo {
                        sql_type: c.sql_type,
                        column_size: c.column_size,
                        use_decimal_binary: c.use_decimal_binary,
                        is_unsigned: c.is_unsigned,
                    })
                    .collect();
                guard.rowcount = rowcount as i64;
            }

            Ok(Box::new(move |py: Python<'_>| {
                // Catalog result columns are ALWAYS lowercased, regardless of
                // pyodbc.lowercase (create_name_map(cur, cCols, true) in cursor.cpp).
                let (description, name_map) =
                    build_description(py, &raw_cols, true, native_uuid, &converter_types)?;
                let mut guard = shared.lock().unwrap();
                guard.description = description;
                guard.name_map = name_map;
                guard.messages = Some(PyList::empty(py).into_any().unbind());
                drop(guard);
                Ok(cursor_obj)
            }) as Finisher)
        })
    }
}

/// Invoke a registered output converter (GIL held).  NULL values become None
/// without calling the converter (GetDataUser in getdata.cpp).
fn call_converter(
    py: Python<'_>,
    converters: &ConverterMap,
    sql_type: i16,
    data: Option<Vec<u8>>,
) -> PyResult<Py<PyAny>> {
    let Some(bytes) = data else {
        return Ok(py.None());
    };
    let func = {
        let map = converters.lock().unwrap();
        map.get(&(sql_type as i32)).map(|f| f.clone_ref(py))
    };
    match func {
        Some(f) => Ok(f.bind(py).call1((PyBytes::new(py, &bytes),))?.unbind()),
        // The converter was removed between fetch and conversion; return bytes.
        None => Ok(PyBytes::new(py, &bytes).into_any().unbind()),
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

/// Read all diagnostic records for Cursor.messages (GetDiagRecs in cursor.cpp).
fn collect_diag(hstmt: HStmt, metadata_enc: &TextEnc, byte_len: bool) -> Vec<RawDiag> {
    let mut out = Vec::new();
    let mut record: i16 = 1;
    loop {
        let mut state_buf = [0u16; 6];
        let mut native: i32 = 0;
        let mut cch: i16 = 0;
        let mut buf = vec![0u16; 1024];

        let mut ret = unsafe {
            odbc_sys::SQLGetDiagRecW(
                HandleType::Stmt,
                hstmt as Handle,
                record,
                state_buf.as_mut_ptr(),
                &mut native,
                buf.as_mut_ptr(),
                (buf.len() - 1) as i16,
                &mut cch,
            )
        };
        if !succeeded(ret) {
            break;
        }
        if byte_len {
            cch /= 2;
        }
        if cch as usize > buf.len() - 1 {
            buf = vec![0u16; cch as usize + 2];
            ret = unsafe {
                odbc_sys::SQLGetDiagRecW(
                    HandleType::Stmt,
                    hstmt as Handle,
                    record,
                    state_buf.as_mut_ptr(),
                    &mut native,
                    buf.as_mut_ptr(),
                    (buf.len() - 1) as i16,
                    &mut cch,
                )
            };
            if !succeeded(ret) {
                break;
            }
            if byte_len {
                cch /= 2;
            }
        }

        let state = String::from_utf16_lossy(&state_buf[..5])
            .trim_end_matches('\0')
            .to_string();
        let raw: Vec<u8> = buf[..(cch.max(0) as usize).min(buf.len())]
            .iter()
            .flat_map(|u| u.to_ne_bytes())
            .collect();
        let text = match textenc::decode(&raw, metadata_enc) {
            Ok(d) => MsgText::Decoded(d),
            Err(_) => MsgText::Raw(raw),
        };
        out.push(RawDiag {
            state,
            native,
            text,
        });
        record += 1;
    }
    out
}

/// Build the Python list for Cursor.messages (GIL held).
fn build_messages(py: Python<'_>, diags: Vec<RawDiag>) -> PyResult<Py<PyAny>> {
    let list = PyList::empty(py);
    for d in diags {
        let class = format!("[{}] ({})", d.state, d.native);
        let value = match d.text {
            MsgText::Decoded(t) => t.into_py_or_bytes(py)?,
            MsgText::Raw(b) => PyBytes::new(py, &b).into_any().unbind(),
        };
        list.append((class, value))?;
    }
    Ok(list.into_any().unbind())
}

fn describe_columns(
    hstmt: HStmt,
    metadata_enc: &TextEnc,
    fetch_decimal_as_string: bool,
) -> PyResult<Vec<RawCol>> {
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

        // The name buffer holds SQLWCHAR data; interpret it per the metadata
        // encoding (create_name_map in cursor.cpp).  The byte count is the
        // character count times the element size of the configured C type.
        let cb = (name_len.max(0) as usize) * if metadata_enc.wide { 2 } else { 1 };
        let raw: Vec<u8> = name_buf
            .iter()
            .flat_map(|u| u.to_ne_bytes())
            .take(cb)
            .collect();
        let name = textenc::decode(&raw, metadata_enc)?;

        // For DECIMAL/NUMERIC columns, configure the ARD once so SQLGetData can
        // fill a SQL_NUMERIC_STRUCT with the right precision/scale.
        let mut use_decimal_binary = false;
        if matches!(data_type.0, 2 | 3)  // SQL_NUMERIC | SQL_DECIMAL
            && !fetch_decimal_as_string
            && (0..=127).contains(&decimal_digits)
        {
            let mut hdesc: HDesc = std::ptr::null_mut();
            let ret = unsafe {
                odbc_sys::SQLGetStmtAttr(
                    hstmt,
                    StatementAttribute::AppRowDesc,
                    &mut hdesc as *mut HDesc as Pointer,
                    0,
                    std::ptr::null_mut(),
                )
            };
            if succeeded(ret) {
                let ok = unsafe {
                    succeeded(odbc_sys::SQLSetDescField(
                        hdesc,
                        (i + 1) as i16,
                        Desc::Type,
                        SQL_C_NUMERIC as Pointer,
                        0,
                    )) && succeeded(odbc_sys::SQLSetDescField(
                        hdesc,
                        (i + 1) as i16,
                        Desc::Precision,
                        column_size as Pointer,
                        0,
                    )) && succeeded(odbc_sys::SQLSetDescField(
                        hdesc,
                        (i + 1) as i16,
                        Desc::Scale,
                        decimal_digits as usize as Pointer,
                        0,
                    ))
                };
                use_decimal_binary = ok;
            }
        }

        // SQL_DESC_UNSIGNED drives whether integer columns fetch as unsigned
        // (MySQL unsigned BIGINT, for example).
        let mut is_unsigned = false;
        if matches!(data_type.0, 4 | 5 | -5 | -6) {
            let mut unsigned_attr: Len = 0;
            let ret = unsafe {
                odbc_sys::SQLColAttributeW(
                    hstmt,
                    i + 1,
                    Desc::Unsigned,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    &mut unsigned_attr,
                )
            };
            is_unsigned = succeeded(ret) && unsigned_attr != 0; // SQL_TRUE
        }

        cols.push(RawCol {
            name,
            sql_type: data_type.0,
            column_size: column_size as u64,
            decimal_digits,
            nullable,
            use_decimal_binary,
            is_unsigned,
        });
    }
    Ok(cols)
}

type DescriptionAndMap = (Option<Py<PyAny>>, Option<Py<PyDict>>);

/// Build the DB API description tuple and the shared name->index map.  GIL held.
fn build_description(
    py: Python<'_>,
    raw_cols: &[RawCol],
    lowercase: bool,
    native_uuid: bool,
    converter_types: &[i32],
) -> PyResult<DescriptionAndMap> {
    if raw_cols.is_empty() {
        return Ok((None, None));
    }
    let map = PyDict::new(py);
    let mut items = Vec::with_capacity(raw_cols.len());
    for (i, col) in raw_cols.iter().enumerate() {
        let name_obj = match &col.name {
            DecodedText::Native(s) => s.clone().into_pyobject(py)?.into_any(),
            DecodedText::Codec(bytes, codec) => py.import("codecs")?.call_method1(
                "decode",
                (PyBytes::new(py, bytes), codec.as_str(), "strict"),
            )?,
        };
        let name_obj = if lowercase {
            name_obj.call_method0("lower")?
        } else {
            name_obj
        };
        let has_conv = converter_types.contains(&(col.sql_type as i32));
        let type_obj = getdata::python_type_for_sql_type(py, col.sql_type, native_uuid, has_conv)?;
        let nullable: Py<PyAny> = match col.nullable {
            Nullability::NO_NULLS => false.into_pyobject(py)?.to_owned().into_any().unbind(),
            Nullability::NULLABLE => true.into_pyobject(py)?.to_owned().into_any().unbind(),
            _ => py.None(),
        };
        let info = (
            &name_obj,
            type_obj,
            py.None(),
            col.column_size,
            col.column_size,
            col.decimal_digits,
            nullable,
        )
            .into_pyobject(py)?;
        map.set_item(&name_obj, i)?;
        items.push(info);
    }
    let desc = PyTuple::new(py, items)?;
    Ok((Some(desc.into_any().unbind()), Some(map.unbind())))
}

/// Prepare/bind/execute on the worker.  Returns (rowcount, columns, diagnostics).
fn execute_odbc(
    hstmt: HStmt,
    ctx: &ExecCtx,
    rows: ExecuteRows,
    cnxninfo: crate::worker::CnxnInfo,
) -> PyResult<(i64, Vec<RawCol>, Vec<RawDiag>)> {
    let limits = params::ParamLimits {
        maxwrite: ctx.maxwrite,
        cnxninfo,
    };
    let override_for = |i: usize| ctx.inputsizes.get(i).and_then(|o| o.as_ref());
    let on_err = |func: &'static str| {
        error_from_handle_ex(func, HandleType::Stmt, hstmt as Handle, ctx.byte_len_diag)
    };

    let exec_prepared = |bound: &[BoundParam], diags: &mut Vec<RawDiag>| -> PyResult<SqlReturn> {
        let mut ret = unsafe { odbc_sys::SQLExecute(hstmt) };

        // Diagnostics (e.g. PRINT output) must be read before any further ODBC
        // call on the handle resets them (cursor.cpp collects before the DAE loop).
        if ret == SqlReturn::SUCCESS_WITH_INFO {
            diags.extend(collect_diag(hstmt, &ctx.metadata_enc, ctx.byte_len_diag));
        }

        // One or more parameters were bound as data-at-execution: stream them via
        // SQLParamData/SQLPutData (cursor.cpp execute).
        while ret == SqlReturn::NEED_DATA {
            let mut token: Pointer = std::ptr::null_mut();
            ret = unsafe { odbc_sys::SQLParamData(hstmt, &mut token) };
            if ret == SqlReturn::NEED_DATA {
                let t = token as usize;
                if let Some(tvp) = bound
                    .iter()
                    .find(|p| p.tvp().is_some() && p.tvp_token() == t)
                    .and_then(|p| p.tvp())
                {
                    // The driver wants the next TVP row (or end-of-rows).
                    let put = tvp.advance_row(hstmt);
                    if !succeeded(put) {
                        return Err(on_err("SQLPutData"));
                    }
                } else if t >= 1 && t <= bound.len() && bound[t - 1].is_dae() {
                    let param = &bound[t - 1];
                    {
                        // Stream a long value in chunks.
                        let data = param.dae_bytes();
                        let chunk = param.dae_chunk();
                        let mut offset = 0usize;
                        loop {
                            let remaining = chunk.min(data.len() - offset);
                            let put = unsafe {
                                odbc_sys::SQLPutData(
                                    hstmt,
                                    data[offset..].as_ptr() as Pointer,
                                    remaining as Len,
                                )
                            };
                            if !succeeded(put) {
                                return Err(on_err("SQLPutData"));
                            }
                            offset += remaining;
                            if offset >= data.len() {
                                break;
                            }
                        }
                    }
                } else {
                    // A TVP column token: find which TVP owns it.
                    let mut handled = false;
                    for param in bound {
                        if let Some(tvp) = param.tvp() {
                            if let Some(col) = tvp.find_column(t) {
                                tvp.put_column(hstmt, col, &on_err)?;
                                handled = true;
                                break;
                            }
                        }
                    }
                    if !handled {
                        return Err(ProgrammingError::new_err(
                            "driver requested data for an unknown parameter",
                        ));
                    }
                }
            } else if ret == SqlReturn::SUCCESS_WITH_INFO {
                // The final SQLParamData carries diagnostics for the whole
                // statement (e.g. PRINT output; issue #1140).
                diags.extend(collect_diag(hstmt, &ctx.metadata_enc, ctx.byte_len_diag));
            } else if ret != SqlReturn::NO_DATA && !succeeded(ret) {
                return Err(on_err("SQLParamData"));
            }
        }
        Ok(ret)
    };

    let mut diags: Vec<RawDiag> = Vec::new();
    let mut with_info = false;
    match rows {
        ExecuteRows::One(values) if values.is_empty() => {
            let ret = if ctx.sql_wide {
                unsafe {
                    odbc_sys::SQLExecDirectW(
                        hstmt,
                        ctx.sql_bytes.as_ptr() as *const u16,
                        (ctx.sql_bytes.len() / 2) as i32,
                    )
                }
            } else {
                unsafe {
                    odbc_sys::SQLExecDirect(
                        hstmt,
                        ctx.sql_bytes.as_ptr(),
                        ctx.sql_bytes.len() as i32,
                    )
                }
            };
            if !succeeded(ret) && ret != SqlReturn::NO_DATA {
                return Err(on_err("SQLExecDirectW"));
            }
            with_info = ret == SqlReturn::SUCCESS_WITH_INFO;
            if with_info {
                diags.extend(collect_diag(hstmt, &ctx.metadata_enc, ctx.byte_len_diag));
            }
        }
        ExecuteRows::One(values) => {
            prepare(hstmt, ctx, &on_err)?;
            let described = describe_null_params(hstmt, std::slice::from_ref(&values), &limits);
            let mut bound: Vec<BoundParam> = Vec::with_capacity(values.len());
            for (i, value) in values.into_iter().enumerate() {
                bound.push(params::bind(
                    hstmt,
                    i,
                    value,
                    &limits,
                    override_for(i),
                    described.get(i).copied().flatten(),
                    on_err,
                )?);
            }
            let ret = exec_prepared(&bound, &mut diags)?;
            if !succeeded(ret) && ret != SqlReturn::NO_DATA {
                return Err(on_err("SQLExecute"));
            }
            with_info = ret == SqlReturn::SUCCESS_WITH_INFO;
            drop(bound); // buffers must outlive SQLExecute and the SQLPutData loop
        }
        ExecuteRows::Many(param_rows) => {
            prepare(hstmt, ctx, &on_err)?;

            // fast_executemany: bind every row as column-wise parameter arrays and
            // execute once.  Falls back to the per-row loop when the data doesn't
            // fit the fast path.
            let mut fast_done = false;
            if ctx.fast_executemany && !param_rows.is_empty() {
                if let Some(columns) =
                    params::bind_parameter_arrays(hstmt, &param_rows, &limits, on_err)?
                {
                    let ret = unsafe { odbc_sys::SQLExecute(hstmt) };
                    with_info = ret == SqlReturn::SUCCESS_WITH_INFO;
                    if with_info {
                        // Read before reset_paramset_size clears the diagnostics.
                        diags.extend(collect_diag(hstmt, &ctx.metadata_enc, ctx.byte_len_diag));
                    }
                    params::reset_paramset_size(hstmt);
                    if !succeeded(ret) && ret != SqlReturn::NO_DATA {
                        return Err(on_err("SQLExecute"));
                    }
                    drop(columns); // arrays must outlive SQLExecute
                    fast_done = true;
                }
            }

            if !fast_done {
                let described = describe_null_params(hstmt, &param_rows, &limits);
                for values in param_rows {
                    unsafe {
                        let _ = odbc_sys::SQLFreeStmt(hstmt, FreeStmtOption::ResetParams);
                    }
                    let mut bound: Vec<BoundParam> = Vec::with_capacity(values.len());
                    for (i, value) in values.into_iter().enumerate() {
                        bound.push(params::bind(
                            hstmt,
                            i,
                            value,
                            &limits,
                            override_for(i),
                            described.get(i).copied().flatten(),
                            on_err,
                        )?);
                    }
                    let ret = exec_prepared(&bound, &mut diags)?;
                    if !succeeded(ret) && ret != SqlReturn::NO_DATA {
                        return Err(on_err("SQLExecute"));
                    }
                    with_info = with_info || ret == SqlReturn::SUCCESS_WITH_INFO;
                    drop(bound);
                }
            }
        }
    }

    let _ = with_info;

    let mut rowcount: Len = 0;
    let ret = unsafe { odbc_sys::SQLRowCount(hstmt, &mut rowcount) };
    if !succeeded(ret) {
        return Err(on_err("SQLRowCount"));
    }

    let cols = describe_columns(hstmt, &ctx.metadata_enc, ctx.fetch_decimal_as_string)?;
    Ok((rowcount as i64, cols, diags))
}

/// Describe (via SQLDescribeParam) every parameter position that is NULL in any
/// row.  Must run right after SQLPrepare, before any SQLBindParameter: some drivers
/// misreport parameter types once binding has started (see test_none_param).
fn describe_null_params(
    hstmt: HStmt,
    rows: &[Vec<ParamValue>],
    limits: &params::ParamLimits,
) -> Vec<Option<SqlDataType>> {
    let ncols = rows.first().map(|r| r.len()).unwrap_or(0);
    let mut out: Vec<Option<SqlDataType>> = vec![None; ncols];
    if !limits.cnxninfo.supports_describeparam {
        return out;
    }
    for (i, slot) in out.iter_mut().enumerate() {
        if rows
            .iter()
            .any(|row| matches!(row.get(i), Some(ParamValue::Null)))
        {
            *slot = Some(params::describe_param_type(hstmt, i));
        }
    }
    out
}

fn prepare(hstmt: HStmt, ctx: &ExecCtx, on_err: &impl Fn(&'static str) -> PyErr) -> PyResult<()> {
    let ret = if ctx.sql_wide {
        unsafe {
            odbc_sys::SQLPrepareW(
                hstmt,
                ctx.sql_bytes.as_ptr() as *const u16,
                (ctx.sql_bytes.len() / 2) as i32,
            )
        }
    } else {
        unsafe { odbc_sys::SQLPrepare(hstmt, ctx.sql_bytes.as_ptr(), ctx.sql_bytes.len() as i32) }
    };
    if !succeeded(ret) {
        return Err(on_err("SQLPrepare"));
    }
    Ok(())
}
