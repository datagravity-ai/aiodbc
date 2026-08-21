// The DB API exception hierarchy and the mapping from ODBC diagnostics to
// exceptions.  Ported from src/errors.cpp.

use odbc_sys::{Handle, HandleType, SqlReturn};
use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;

create_exception!(aiodbc, Warning, PyException);
create_exception!(aiodbc, Error, PyException);
create_exception!(aiodbc, InterfaceError, Error);
create_exception!(aiodbc, DatabaseError, Error);
create_exception!(aiodbc, DataError, DatabaseError);
create_exception!(aiodbc, OperationalError, DatabaseError);
create_exception!(aiodbc, IntegrityError, DatabaseError);
create_exception!(aiodbc, InternalError, DatabaseError);
create_exception!(aiodbc, ProgrammingError, DatabaseError);
create_exception!(aiodbc, NotSupportedError, DatabaseError);

pub fn register(py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("Warning", py.get_type::<Warning>())?;
    m.add("Error", py.get_type::<Error>())?;
    m.add("InterfaceError", py.get_type::<InterfaceError>())?;
    m.add("DatabaseError", py.get_type::<DatabaseError>())?;
    m.add("DataError", py.get_type::<DataError>())?;
    m.add("OperationalError", py.get_type::<OperationalError>())?;
    m.add("IntegrityError", py.get_type::<IntegrityError>())?;
    m.add("InternalError", py.get_type::<InternalError>())?;
    m.add("ProgrammingError", py.get_type::<ProgrammingError>())?;
    m.add("NotSupportedError", py.get_type::<NotSupportedError>())?;
    Ok(())
}

type Args = (String, String); // (sqlstate, message), matching the C++ GetError args order
type MakeErr = fn(Args) -> PyErr;

// SQLSTATE prefix -> exception class, in match order.  From errors.cpp.
static SQLSTATE_MAP: &[(&str, MakeErr)] = &[
    ("01002", |a| OperationalError::new_err(a)),
    ("08001", |a| OperationalError::new_err(a)),
    ("08003", |a| OperationalError::new_err(a)),
    ("08004", |a| OperationalError::new_err(a)),
    ("08007", |a| OperationalError::new_err(a)),
    ("08S01", |a| OperationalError::new_err(a)),
    ("0A000", |a| NotSupportedError::new_err(a)),
    ("28000", |a| InterfaceError::new_err(a)),
    ("40002", |a| IntegrityError::new_err(a)),
    ("22", |a| DataError::new_err(a)),
    ("23", |a| IntegrityError::new_err(a)),
    ("24", |a| ProgrammingError::new_err(a)),
    ("25", |a| ProgrammingError::new_err(a)),
    ("42", |a| ProgrammingError::new_err(a)),
    ("HY001", |a| OperationalError::new_err(a)),
    ("HY014", |a| OperationalError::new_err(a)),
    ("HYT00", |a| OperationalError::new_err(a)),
    ("HYT01", |a| OperationalError::new_err(a)),
    ("IM001", |a| InterfaceError::new_err(a)),
    ("IM002", |a| InterfaceError::new_err(a)),
    ("IM003", |a| InterfaceError::new_err(a)),
];

pub fn error_from_sqlstate(sqlstate: &str, message: String) -> PyErr {
    let state = if sqlstate.is_empty() {
        "HY000"
    } else {
        sqlstate
    };
    let args = (state.to_string(), message);
    for (prefix, make) in SQLSTATE_MAP {
        if state.starts_with(prefix) {
            return make(args);
        }
    }
    Error::new_err(args)
}

const DEFAULT_ERROR: &str = "The driver did not supply an error!";

/// Build an exception from the diagnostic records of an ODBC handle, formatted as
/// "[sqlstate] message (native_error) (function)".  The SQLSTATE of the first record
/// selects the exception class.
///
/// Like the C++ implementation, only the first record is read on non-Windows
/// platforms (some Unix drivers crash on repeated SQLGetDiagRec calls); on Windows
/// the remaining records are appended as "; [sqlstate] message (native_error)".
pub fn error_from_handle(function: &str, handle_type: HandleType, handle: Handle) -> PyErr {
    error_from_handle_ex(function, handle_type, handle, false)
}

/// error_from_handle with the compat_diagrec_byte_length workaround: some drivers
/// report the diagnostic text length in bytes instead of characters (issue #489).
pub fn error_from_handle_ex(
    function: &str,
    handle_type: HandleType,
    handle: Handle,
    text_length_in_bytes: bool,
) -> PyErr {
    let mut first_state = String::new();
    let mut msg = String::new();

    let mut record: i16 = 1;
    loop {
        let mut state_buf = [0u16; 6];
        let mut native: i32 = 0;
        let mut cch: i16 = 0;
        let mut buf = vec![0u16; 1024];

        let mut ret = unsafe {
            odbc_sys::SQLGetDiagRecW(
                handle_type,
                handle,
                record,
                state_buf.as_mut_ptr(),
                &mut native,
                buf.as_mut_ptr(),
                (buf.len() - 1) as i16,
                &mut cch,
            )
        };
        if !matches!(ret, SqlReturn::SUCCESS | SqlReturn::SUCCESS_WITH_INFO) {
            break;
        }
        if text_length_in_bytes {
            cch /= 2;
        }

        // If the message was truncated, retry with a buffer sized to fit.
        if cch as usize > buf.len() - 1 {
            buf = vec![0u16; cch as usize + 2];
            ret = unsafe {
                odbc_sys::SQLGetDiagRecW(
                    handle_type,
                    handle,
                    record,
                    state_buf.as_mut_ptr(),
                    &mut native,
                    buf.as_mut_ptr(),
                    (buf.len() - 1) as i16,
                    &mut cch,
                )
            };
            if !matches!(ret, SqlReturn::SUCCESS | SqlReturn::SUCCESS_WITH_INFO) {
                break;
            }
            if text_length_in_bytes {
                cch /= 2;
            }
        }

        let state: String = String::from_utf16_lossy(&state_buf[..5])
            .trim_end_matches('\0')
            .to_string();
        let text = String::from_utf16_lossy(&buf[..(cch.max(0) as usize).min(buf.len())]);

        if cch != 0 {
            if record == 1 {
                first_state = state;
                msg = format!("[{first_state}] {text} ({native}) ({function})");
            } else {
                msg.push_str(&format!("; [{state}] {text} ({native})"));
            }
        }

        record += 1;

        // See the errors.cpp comment: some Unix drivers crash if SQLGetDiagRec is
        // called more than once, so only Windows reads the whole record chain.
        if !cfg!(windows) {
            break;
        }
    }

    if msg.is_empty() {
        // Buggy driver or driver manager signaled a fault without diagnostics.
        first_state.clear();
        msg = DEFAULT_ERROR.to_string();
    }

    error_from_sqlstate(&first_state, msg)
}
