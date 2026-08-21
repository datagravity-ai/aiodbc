# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this
repository.

## What this is

pyodbc is a Python module implementing the DB API 2.0 spec on top of ODBC, with an
**asyncio-native API**, implemented in **Rust**.  The compiled core is the extension module
`pyodbc._core` (sources in `rust/`, built with maturin/PyO3 against the abi3 for Python
3.10+); the public package is assembled in `python/pyodbc/__init__.py` on top of it
(`python/pyodbc/_core.pyi` is the type stub for the compiled module).  It links against an
ODBC driver manager (unixODBC/iODBC on Unix, built-in on Windows).

The rewrite from the previous synchronous C++ extension is documented in
`docs/rust-asyncio-rewrite-plan.md`; the C++ sources were removed at cutover and live in git
history (last present at the parent of the cutover commit).

## Build & test

Fast development loop (requires a Rust toolchain and unixODBC headers, e.g. `unixodbc-dev`):

```sh
pip install maturin
maturin develop                                     # build + install into the active env
pytest tests/sqlite_test.py -vxk test_text          # single test by name substring
```

`pip install .` builds the same module via the PEP 517 backend.  To see Rust panics and
backtraces from a crashing test, run pytest with `-s` and `RUST_BACKTRACE=1`.

Full multi-version test matrix uses tox (`pipx install tox`), covering py310–py314:

```sh
tox                 # all interpreters + all databases
tox -e py312        # one interpreter
tox -e py312 -- -rA # pass pytest args after --
```

Lint: `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` for the Rust core;
`flake8` (max line length 95; see `.flake8`) for the Python layer and tests.  Install dev
deps with `pip install -r requirements-dev.txt`.

### Test database configuration

There is one test file per backend (`tests/sqlite_test.py`, `sqlserver_test.py`,
`postgresql_test.py`, `mysql_test.py`), written in asyncio idiom (pytest-asyncio runs in
auto mode; see `pyproject.toml`).  Each reads its connection string from an environment
variable, falling back to a DSN default:

- `PYODBC_SQLITE`, `PYODBC_SQLSERVER`, `PYODBC_POSTGRESQL`, `PYODBC_MYSQL`

Set these in the shell or in a `pytest.ini` (with `pytest-env`); `tox.ini`'s header has an
example.  SQLite needs no server (`driver=SQLite3;Database=:memory:` with the Ubuntu
`libsqliteodbc` package) and is the easiest target for quick iteration.  The real database
must be running and the matching ODBC driver installed before tests pass — see
`.github/workflows/ubuntu_build.yml` for the exact driver names and connection strings CI
uses.

## Scratch scripts vs. project tooling

Decide by **audience**, not by who happens to type the command:

- **Scripts only Claude runs** — issue-repro scripts, one-off experiments, debug helpers — go
  in **`.claude/scratch/`**. That directory is git-ignored (`/.claude/scratch/` in
  `.gitignore`) and must **never** be committed. It is local scratch space, not part of the
  project.
- **Scripts a human would ever run** are ordinary project tooling. They belong in
  **`utils/`**, committed and reviewed like any other code with a neutral name — not in an
  "AI" directory. The moment a script has to be readable/maintained for a human, it is
  bucket two.

Before writing a human-facing test runner, note that **`tox` already builds and tests across
all supported Python versions** (see "Build & test"); a custom runner is only a thin
convenience and often unnecessary.

## Architecture

**Concurrency model** (see `docs/rust-asyncio-rewrite-plan.md`): every `Connection` owns one
OS worker thread (`rust/worker.rs`) that performs ALL ODBC calls for the connection and its
cursors, in submission order.  Async methods enqueue a task and return an asyncio future
completed from the worker via `loop.call_soon_threadsafe` (`rust/async_bridge.rs`).  The few
synchronous properties that must call ODBC (e.g. the `autocommit` setter) dispatch to the
worker and block briefly with the GIL released.

The Rust core, one module per concern:

- **`rust/lib.rs`** — module init: generated constants, class/function registration, the
  shared `HENV` (`rust/env.rs`, which reads module-level `pooling`/`odbcversion` at
  allocation), `drivers()`/`data_sources()`.
- **`rust/connection.rs`** — the `Connection` type: connect (incl. `attrs_before`),
  autocommit/timeout/searchescape, `getinfo` (typed per info-id via
  `rust/getinfo_types.rs`), `set_attr`, encodings, output converters.
- **`rust/cursor.rs`** — the `Cursor` type (largest file): execute/executemany/fetch,
  `description` and the name→index map shared with rows, `messages` diagnostics, catalog
  functions (always-lowercase catalog column names), `nextset`.
- **`rust/params.rs`** — binding Python parameters into SQL statements: type detection, NULL
  handling via `SQLDescribeParam` (which must run after `SQLPrepare` but before any
  `SQLBindParameter`), data-at-execution streaming, `fast_executemany` column-wise arrays,
  and SQL Server table-valued parameters (TVPs).
- **`rust/getdata.rs`** — converting fetched column data into Python objects, including
  chunked reads of variable-length columns, output converters, `sql_variant`, GUIDs.
- **`rust/row.rs`** — the `Row` type: tuple-like, column-name attribute access, pickling,
  usable after the cursor/connection closes.
- **`rust/textenc.rs`** — the four per-connection text encodings; **`rust/errors.rs`** — the
  SQLSTATE→exception mapping and `[state] msg (native) (Function)` error format;
  **`rust/decimal_support.rs`** — Decimal fetch (binary NUMERIC_STRUCT and string paths) and
  parameter formatting; **`rust/constants.rs`** — generated by
  `utils/generate-odbc-constants.py` (CI checks it is up to date).

The Python layer (`python/pyodbc/__init__.py`) supplies the PEP 249 globals, type
constructors, the `connect()` keyword handling (returning an awaitable that is also an async
context manager), module attributes (`pooling`, `lowercase`, `native_uuid`, `odbcversion`),
and the `BinaryNull` sentinel.

### Text encoding is the subtle part

Drivers disagree wildly about Unicode, so encoding is *not* uniform.  A separate `TextEnc`
is configured for reading SQL_CHAR, reading SQL_WCHAR, writing unicode, and **reading
metadata** (column names).  Metadata gets its own encoding because PostgreSQL/MySQL return
column names as UTF-16LE from `SQLDescribeCol` regardless of connection settings.
`setencoding()`/`setdecoding()` on the Connection adjust these.  Wide data is always
exchanged as 16-bit code units (`SQLWCHAR_SIZE == 2`) even where unixODBC defines `SQLWCHAR`
as 32-bit `wchar_t` (see `HACKING.md`).  When touching encoding code, consult `notes.txt`
for the ODBC length-argument rules (count-of-characters vs. count-of-bytes).

## Versioning

The single source of truth for the version is the `version = "..."` line in
`pyproject.toml`; maturin records it in the wheel metadata and `pyodbc.version` reads it
back via `importlib.metadata` at import time.  Keep the `Cargo.toml` version in sync (same
number, with pre-release parts in Cargo's `-a1` style rather than PEP 440's `a1`).
