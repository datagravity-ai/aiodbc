// The bridge between the per-connection ODBC worker threads and asyncio.
//
// Async methods create an asyncio.Future on the running loop, enqueue work on the
// connection's worker thread, and return the future.  When the work finishes, the
// worker completes the future from its own thread via loop.call_soon_threadsafe
// (asyncio futures may only be touched on their loop's thread).

use std::sync::OnceLock;

use pyo3::prelude::*;

static COMPLETE_HELPER: OnceLock<Py<PyAny>> = OnceLock::new();

/// Internal: complete an asyncio future from a worker thread.  Always invoked on the
/// event-loop thread via call_soon_threadsafe.
#[pyfunction]
fn _complete_future(
    fut: Bound<'_, PyAny>,
    error: Bound<'_, PyAny>,
    result: Bound<'_, PyAny>,
) -> PyResult<()> {
    // The future may have been cancelled while the ODBC call was running.
    if fut.call_method0("done")?.extract::<bool>()? {
        return Ok(());
    }
    if error.is_none() {
        fut.call_method1("set_result", (result,))?;
    } else {
        fut.call_method1("set_exception", (error,))?;
    }
    Ok(())
}

pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(_complete_future, m)?)?;
    let helper = m.getattr("_complete_future")?.unbind();
    let _ = COMPLETE_HELPER.set(helper);
    Ok(())
}

/// The worker-thread side of a pending future.
pub struct FutureHandle {
    fut: Py<PyAny>,
    event_loop: Py<PyAny>,
}

/// Create a future on the running event loop.  Returns the future to hand back to
/// Python and the handle the worker uses to complete it.
pub fn new_future(py: Python<'_>) -> PyResult<(Py<PyAny>, FutureHandle)> {
    let event_loop = py.import("asyncio")?.call_method0("get_running_loop")?;
    let fut = event_loop.call_method0("create_future")?;
    Ok((
        fut.clone().unbind(),
        FutureHandle {
            fut: fut.unbind(),
            event_loop: event_loop.unbind(),
        },
    ))
}

/// Create an already-completed future on the running event loop.  Used by async
/// methods whose result is known without a round-trip to the worker (for example
/// `__aenter__`, or `__aexit__` on an autocommit connection).
pub fn resolved_future<'py>(
    py: Python<'py>,
    value: Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    let event_loop = py.import("asyncio")?.call_method0("get_running_loop")?;
    let fut = event_loop.call_method0("create_future")?;
    fut.call_method1("set_result", (value,))?;
    Ok(fut)
}

impl FutureHandle {
    /// Complete the future with a result or exception.  Called from the worker
    /// thread; the actual set_result/set_exception runs on the loop thread.
    pub fn complete(self, py: Python<'_>, result: PyResult<Py<PyAny>>) {
        let helper = COMPLETE_HELPER
            .get()
            .expect("aiodbc._core not initialized")
            .bind(py);
        let (error, value) = match result {
            Ok(v) => (py.None(), v),
            Err(e) => (e.into_value(py).into_any(), py.None()),
        };
        // If the loop was closed (e.g. interpreter shutdown), there is nobody left
        // to deliver the result to; ignore the error.
        let _ = self.event_loop.bind(py).call_method1(
            "call_soon_threadsafe",
            (helper, self.fut.bind(py), error, value),
        );
    }
}
