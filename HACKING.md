# Hacking on pyodbc

pyodbc 6.x is implemented in Rust with an asyncio-native API.  The compiled core
lives in rust/ (built by Cargo.toml + maturin into the extension module
`pyodbc._core`); the public package is assembled in python/pyodbc/.  The design and
the history of the rewrite from C++ are in docs/rust-asyncio-rewrite-plan.md.

## Development loop

You need a Rust toolchain (https://rustup.rs) and the unixODBC headers (e.g. the
`unixodbc-dev` package; on Windows the ODBC headers ship with the SDK).

    pip install maturin
    maturin develop                          # build + install into the active env
    pytest tests/sqlite_test.py -vxk test_text

`maturin develop` is the fast path; `pip install .` builds the same thing via the
PEP 517 backend.  The SQLite suite needs no database server and is the quickest
target for iteration; the other suites read their connection strings from
environment variables (see tests/ and tox.ini).

Lint before pushing:

    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    flake8

For complete testing across Python versions use tox (`pipx install tox`), which
builds the wheel per environment and runs every suite.

## Concurrency model

Every `Connection` owns one OS worker thread that performs ALL ODBC calls for the
connection and its cursors, in submission order.  Async methods enqueue a task and
return an asyncio future which the worker completes via
`loop.call_soon_threadsafe`.  This serializes access to each `HDBC` (the
conservative reading of ODBC thread-safety) while keeping the event loop free.
The few synchronous properties that must call ODBC (e.g. the `autocommit` setter)
dispatch to the worker and block briefly with the GIL released.

## Text encoding notes

Drivers disagree wildly about Unicode, so a connection carries four independent
encodings (reading SQL_CHAR, reading SQL_WCHAR, writing unicode, and reading
metadata such as column names).  Wide data is exchanged as 16-bit code units
(`SQLWCHAR_SIZE == 2`) regardless of the platform's `wchar_t`: the unixODBC
headers may define SQLWCHAR as a 32-bit `wchar_t`, but the buffer data is still
16-bit.  See rust/textenc.rs and notes.txt for the ODBC length-argument rules
(count-of-characters vs. count-of-bytes).
