// The Row type: tuple-like result rows with access by column name.  Ported
// (simplified) from src/row.cpp.  Pickling and column value assignment come in a
// later phase.

use pyo3::exceptions::{PyAttributeError, PyIndexError, PyKeyError};
use pyo3::prelude::*;
use pyo3::pyclass::CompareOp;
use pyo3::types::{PyDict, PySlice, PyString, PyTuple};

#[pyclass(module = "pyodbc")]
pub struct Row {
    /// Column values, in select order.
    pub values: Vec<Py<PyAny>>,
    /// The parent cursor's description at fetch time (shared, not copied).
    pub description: Py<PyAny>,
    /// Shared name -> index dict, built with the description.
    pub name_map: Py<PyDict>,
}

impl Row {
    fn as_tuple<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyTuple>> {
        PyTuple::new(py, self.values.iter().map(|v| v.bind(py)))
    }
}

#[pymethods]
impl Row {
    #[getter]
    fn cursor_description(&self, py: Python<'_>) -> Py<PyAny> {
        self.description.clone_ref(py)
    }

    fn __len__(&self) -> usize {
        self.values.len()
    }

    fn __getattr__(&self, py: Python<'_>, name: &str) -> PyResult<Py<PyAny>> {
        match self.name_map.bind(py).get_item(name)? {
            Some(index) => {
                let i: usize = index.extract()?;
                Ok(self.values[i].clone_ref(py))
            }
            None => Err(PyAttributeError::new_err(format!(
                "Row object has no attribute '{name}'"
            ))),
        }
    }

    fn __getitem__(
        slf: &Bound<'_, Self>,
        py: Python<'_>,
        key: &Bound<'_, PyAny>,
    ) -> PyResult<Py<PyAny>> {
        let this = slf.borrow();
        let n = this.values.len() as isize;

        if let Ok(mut index) = key.extract::<isize>() {
            if index < 0 {
                index += n;
            }
            if index < 0 || index >= n {
                return Err(PyIndexError::new_err("tuple index out of range"));
            }
            return Ok(this.values[index as usize].clone_ref(py));
        }

        if let Ok(slice) = key.downcast::<PySlice>() {
            let idx = slice.indices(n)?;
            // Like row.cpp: a slice covering the entire row returns the Row itself.
            if idx.start == 0 && idx.stop == n && idx.step == 1 {
                return Ok(slf.clone().into_any().unbind());
            }
            let mut out = Vec::new();
            let mut i = idx.start;
            while if idx.step > 0 {
                i < idx.stop
            } else {
                i > idx.stop
            } {
                out.push(this.values[i as usize].bind(py));
                i += idx.step;
            }
            return Ok(PyTuple::new(py, out)?.into_any().unbind());
        }

        // Allow row['colname'] as row.cpp does for mapping-style access.
        if let Ok(name) = key.downcast::<PyString>() {
            if let Some(index) = this.name_map.bind(py).get_item(name)? {
                let i: usize = index.extract()?;
                return Ok(this.values[i].clone_ref(py));
            }
            return Err(PyKeyError::new_err(name.to_string()));
        }

        Err(pyo3::exceptions::PyTypeError::new_err(
            "row indices must be integers, slices, or column names",
        ))
    }

    fn __iter__(slf: &Bound<'_, Self>, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let tuple = slf.borrow().as_tuple(py)?;
        Ok(tuple.try_iter()?.into_any().unbind())
    }

    fn __repr__(&self, py: Python<'_>) -> PyResult<String> {
        let tuple = self.as_tuple(py)?;
        Ok(tuple.repr()?.to_string())
    }

    fn __richcmp__(
        &self,
        py: Python<'_>,
        other: &Bound<'_, PyAny>,
        op: CompareOp,
    ) -> PyResult<Py<PyAny>> {
        // Compare as tuples, like row.cpp.  Rows compare against other Rows and
        // against plain sequences.
        let mine = self.as_tuple(py)?;
        let theirs: Bound<'_, PyAny> = if let Ok(other_row) = other.downcast::<Row>() {
            other_row.borrow().as_tuple(py)?.into_any()
        } else {
            other.clone()
        };
        Ok(mine.rich_compare(theirs, op)?.unbind())
    }
}
