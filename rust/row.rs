// The Row type: tuple-like result rows with access by column name.  Ported from
// src/row.cpp: rows support item/attribute assignment (to "fix up" fetched data),
// pickling via __reduce__, and compare like tuples.

use pyo3::exceptions::{PyAttributeError, PyIndexError, PyTypeError};
use pyo3::prelude::*;
use pyo3::pyclass::CompareOp;
use pyo3::types::{PyDict, PySlice, PyTuple, PyType};

#[pyclass(module = "aiodbc")]
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

    fn resolve_index(&self, py: Python<'_>, name: &str) -> PyResult<Option<usize>> {
        match self.name_map.bind(py).get_item(name)? {
            Some(index) => Ok(Some(index.extract()?)),
            None => Ok(None),
        }
    }
}

#[pymethods]
impl Row {
    /// Only used by unpickling: Row(description, name_map, *values), the state
    /// produced by __reduce__ (row.cpp new_check).
    #[new]
    #[pyo3(signature = (*args))]
    fn new(args: &Bound<'_, PyTuple>) -> PyResult<Self> {
        let usage =
            || PyTypeError::new_err("Row objects cannot be constructed directly (unpickling only)");
        if args.len() < 2 {
            return Err(usage());
        }
        let description = args.get_item(0)?;
        let name_map = args.get_item(1)?;
        if !description.is_instance_of::<PyTuple>() || !name_map.is_instance_of::<PyDict>() {
            return Err(usage());
        }
        let cols = description.downcast::<PyTuple>()?.len();
        if args.len() - 2 != cols {
            return Err(usage());
        }
        let values = (2..args.len())
            .map(|i| Ok(args.get_item(i)?.unbind()))
            .collect::<PyResult<Vec<_>>>()?;
        Ok(Row {
            values,
            description: description.unbind(),
            name_map: name_map.downcast::<PyDict>()?.clone().unbind(),
        })
    }

    /// Supports pickling: (RowType, (description, name_map, *values)).
    fn __reduce__<'py>(
        slf: &Bound<'py, Self>,
        py: Python<'py>,
    ) -> PyResult<(Bound<'py, PyType>, Bound<'py, PyTuple>)> {
        let this = slf.borrow();
        let mut state: Vec<Bound<'py, PyAny>> = Vec::with_capacity(2 + this.values.len());
        state.push(this.description.bind(py).clone());
        state.push(this.name_map.bind(py).clone().into_any());
        for v in &this.values {
            state.push(v.bind(py).clone());
        }
        Ok((slf.get_type(), PyTuple::new(py, state)?))
    }

    #[getter]
    fn cursor_description(&self, py: Python<'_>) -> Py<PyAny> {
        self.description.clone_ref(py)
    }

    fn __len__(&self) -> usize {
        self.values.len()
    }

    fn __getattr__(&self, py: Python<'_>, name: &str) -> PyResult<Py<PyAny>> {
        match self.resolve_index(py, name)? {
            Some(i) => Ok(self.values[i].clone_ref(py)),
            None => Err(PyAttributeError::new_err(format!(
                "Row object has no attribute '{name}'"
            ))),
        }
    }

    fn __setattr__(&mut self, py: Python<'_>, name: &str, value: Bound<'_, PyAny>) -> PyResult<()> {
        // Like Row_setattro: a column name assigns the column; anything else fails
        // (rows have no instance dict, like tuples).
        match self.resolve_index(py, name)? {
            Some(i) => {
                self.values[i] = value.unbind();
                Ok(())
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

        // Rows are sequences, not mappings (row.cpp Row_subscript).
        Err(PyTypeError::new_err(format!(
            "row indices must be integers, not {}",
            key.get_type().name()?
        )))
    }

    fn __setitem__(&mut self, key: &Bound<'_, PyAny>, value: Bound<'_, PyAny>) -> PyResult<()> {
        let n = self.values.len() as isize;
        let mut index: isize = key.extract().map_err(|_| {
            PyTypeError::new_err(format!(
                "row indices must be integers, not {}",
                key.get_type()
                    .name()
                    .map(|n| n.to_string())
                    .unwrap_or_default()
            ))
        })?;
        if index < 0 {
            index += n;
        }
        if index < 0 || index >= n {
            return Err(PyIndexError::new_err("Row assignment index out of range"));
        }
        self.values[index as usize] = value.unbind();
        Ok(())
    }

    fn __contains__(&self, py: Python<'_>, item: &Bound<'_, PyAny>) -> PyResult<bool> {
        for v in &self.values {
            if v.bind(py).eq(item)? {
                return Ok(true);
            }
        }
        Ok(false)
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
