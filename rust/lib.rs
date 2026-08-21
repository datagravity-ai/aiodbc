// The pyodbc._core extension module: the Rust replacement for the C++ sources in
// src/, being ported per docs/rust-asyncio-rewrite-plan.md.  The public package is
// assembled in python/pyodbc/__init__.py.

use odbc_sys::{FetchOrientation, Handle, HandleType, SqlReturn};
use pyo3::prelude::*;
use pyo3::types::PyDict;

mod async_bridge;
mod connection;
mod constants;
mod cursor;
mod env;
mod errors;
mod getdata;
mod params;
mod row;
mod worker;

fn succeeded(ret: SqlReturn) -> bool {
    matches!(ret, SqlReturn::SUCCESS | SqlReturn::SUCCESS_WITH_INFO)
}

/// Return the names of the installed ODBC drivers, from SQLDrivers.
#[pyfunction]
fn drivers(py: Python<'_>) -> PyResult<Vec<String>> {
    let henv = env::get_env(py)?;

    let mut result = Vec::new();
    let mut direction = FetchOrientation::First;

    loop {
        let mut desc = [0u8; 500];
        let mut cb_desc: i16 = 0;
        let mut cb_attrs: i16 = 0;

        let ret = unsafe {
            odbc_sys::SQLDrivers(
                henv,
                direction,
                desc.as_mut_ptr(),
                desc.len() as i16,
                &mut cb_desc,
                std::ptr::null_mut(),
                0,
                &mut cb_attrs,
            )
        };
        if !succeeded(ret) {
            if ret == SqlReturn::NO_DATA {
                break;
            }
            return Err(errors::error_from_handle(
                "SQLDrivers",
                HandleType::Env,
                henv as Handle,
            ));
        }

        // The driver manager reports names as UTF-8 (see the C++ mod_drivers note).
        let len = (cb_desc.max(0) as usize).min(desc.len());
        result.push(String::from_utf8_lossy(&desc[..len]).into_owned());
        direction = FetchOrientation::Next;
    }

    Ok(result)
}

/// Return a dict of data source names -> descriptions, from SQLDataSources.
///
/// TODO(windows): the C++ implementation uses SQLDataSourcesW/UTF-16 on Windows so
/// non-ASCII DSN names survive; port that with #[cfg(windows)] when Windows builds
/// are stood up (Phase 6 of docs/rust-asyncio-rewrite-plan.md).
#[pyfunction]
#[pyo3(signature = (*, scope=None))]
fn data_sources<'py>(py: Python<'py>, scope: Option<&str>) -> PyResult<Bound<'py, PyDict>> {
    let mut direction = match scope {
        None => FetchOrientation::First,
        Some("user") => FetchOrientation::FirstUser,
        Some("system") => FetchOrientation::FirstSystem,
        Some(_) => {
            return Err(pyo3::exceptions::PyValueError::new_err(
                "scope must be 'user' or 'system'",
            ))
        }
    };

    let henv = env::get_env(py)?;
    let result = PyDict::new(py);

    loop {
        // Buffers larger than SQL_MAX_DSN_LENGTH + 1, for systems that ignore it.
        let mut dsn = [0u8; 500];
        let mut desc = [0u8; 500];
        let mut cb_dsn: i16 = 0;
        let mut cb_desc: i16 = 0;

        let ret = unsafe {
            odbc_sys::SQLDataSources(
                henv,
                direction,
                dsn.as_mut_ptr(),
                dsn.len() as i16,
                &mut cb_dsn,
                desc.as_mut_ptr(),
                desc.len() as i16,
                &mut cb_desc,
            )
        };
        if !succeeded(ret) {
            if ret == SqlReturn::NO_DATA {
                break;
            }
            return Err(errors::error_from_handle(
                "SQLDataSources",
                HandleType::Env,
                henv as Handle,
            ));
        }

        let key_len = (cb_dsn.max(0) as usize).min(dsn.len());
        let val_len = (cb_desc.max(0) as usize).min(desc.len());
        result.set_item(
            String::from_utf8_lossy(&dsn[..key_len]),
            String::from_utf8_lossy(&desc[..val_len]),
        )?;

        direction = FetchOrientation::Next;
    }

    Ok(result)
}

#[pymodule]
fn _core(py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    for (name, value) in constants::CONSTANTS {
        m.add(*name, *value)?;
    }

    // pyodbc always treats SQLWCHAR data as 16-bit, even where the driver manager
    // defines SQLWCHAR as 32-bit wchar_t (see HACKING.md).
    m.add("SQLWCHAR_SIZE", 2)?;

    errors::register(py, m)?;
    async_bridge::register(m)?;

    m.add_class::<connection::Connection>()?;
    m.add_class::<cursor::Cursor>()?;
    m.add_class::<row::Row>()?;

    m.add_function(wrap_pyfunction!(connection::connect, m)?)?;
    m.add_function(wrap_pyfunction!(drivers, m)?)?;
    m.add_function(wrap_pyfunction!(data_sources, m)?)?;

    Ok(())
}
