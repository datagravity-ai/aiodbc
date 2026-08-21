// The process-wide ODBC environment handle (HENV), allocated lazily on first use.
// Ported from AllocateEnv in src/pyodbcmodule.cpp.

use std::sync::Mutex;

use odbc_sys::{
    AttrConnectionPooling, AttrOdbcVersion, EnvironmentAttribute, HEnv, Handle, HandleType,
    SqlReturn,
};
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

// The raw handle, stored as usize because raw pointers are not Send/Sync.  Guarded by
// a mutex only for the allocate-once race; after that the handle itself is never
// mutated and ODBC environment handles may be used from any thread.
static HENV: Mutex<Option<usize>> = Mutex::new(None);

fn succeeded(ret: SqlReturn) -> bool {
    matches!(ret, SqlReturn::SUCCESS | SqlReturn::SUCCESS_WITH_INFO)
}

/// Return the shared environment handle, allocating it on first call.
///
/// Like the C++ implementation, the module-level `pyodbc.pooling` and
/// `pyodbc.odbcversion` attributes are read at allocation time, which is why they
/// must be set before the first connection (or drivers()/dataSources() call).
pub fn get_env(py: Python<'_>) -> PyResult<HEnv> {
    let mut guard = HENV
        .lock()
        .map_err(|_| PyRuntimeError::new_err("pyodbc environment lock poisoned"))?;

    if let Some(h) = *guard {
        return Ok(h as HEnv);
    }

    let module = PyModule::import(py, "pyodbc")?;
    let pooling: bool = module.getattr("pooling")?.extract().unwrap_or(false);
    let odbcversion: String = module
        .getattr("odbcversion")?
        .extract()
        .unwrap_or_else(|_| String::from("3.X"));

    if pooling {
        let ret = unsafe {
            odbc_sys::SQLSetEnvAttr(
                std::ptr::null_mut(),
                EnvironmentAttribute::ConnectionPooling,
                AttrConnectionPooling::OnePerHenv.into(),
                std::mem::size_of::<i32>() as i32,
            )
        };
        if !succeeded(ret) {
            return Err(PyRuntimeError::new_err(
                "Unable to set SQL_ATTR_CONNECTION_POOLING attribute.",
            ));
        }
    }

    let mut henv: Handle = std::ptr::null_mut();
    let ret = unsafe { odbc_sys::SQLAllocHandle(HandleType::Env, std::ptr::null_mut(), &mut henv) };
    if !succeeded(ret) {
        return Err(PyRuntimeError::new_err(
            "Can't initialize module pyodbc.  SQLAllocEnv failed.",
        ));
    }

    let version = if odbcversion == "3.8" {
        AttrOdbcVersion::Odbc3_80
    } else {
        AttrOdbcVersion::Odbc3
    };
    let ret = unsafe {
        odbc_sys::SQLSetEnvAttr(
            henv as HEnv,
            EnvironmentAttribute::OdbcVersion,
            version.into(),
            std::mem::size_of::<i32>() as i32,
        )
    };
    if !succeeded(ret) {
        return Err(PyRuntimeError::new_err(
            "Unable to set SQL_ATTR_ODBC_VERSION attribute.",
        ));
    }

    *guard = Some(henv as usize);
    Ok(henv as HEnv)
}
