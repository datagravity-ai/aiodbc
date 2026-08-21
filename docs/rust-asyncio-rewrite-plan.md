# Plan: rewrite pyodbc in Rust with an asyncio-native API

## Goals

1. Replace the C++ extension (`src/*.cpp`) with a Rust implementation.
2. Make the public API asyncio-native: `await pyodbc.connect(...)`,
   `await cursor.execute(...)`, `async for row in cursor`, etc.
3. Keep the existing pytest suites (`tests/*_test.py`) as the behavioral spec.
   The only permitted change to the tests is converting them to the asyncio
   idiom — no assertion changes, no deleted tests.

## Non-goals

- No synchronous API. The library becomes async-only; there is no plan to ship
  a blocking compatibility shim.
- No new features beyond the port (the tests define "done").
- No support for ODBC's native async modes (`SQL_ATTR_ASYNC_ENABLE`,
  ODBC 3.8 notification mode). Driver support is effectively SQL Server-only;
  asyncio integration is achieved by thread offload instead (see below).

## Why the tests are a strong spec

The four suites (`sqlite_test.py` 801 lines, `sqlserver_test.py` 1981,
`postgresql_test.py` 706, `mysql_test.py` 528) exercise the subtle parts:
fencepost sizes across the three variable-length read paths, encodings,
decimal/NUMERIC_STRUCT, output converters, `fast_executemany`, TVPs,
context-manager semantics, Row pickling and name access. Passing all four
against real drivers is the parity bar.

---

## Key technical decisions

### Build stack: PyO3 + maturin

- **PyO3** (latest stable) with `abi3-py310`, matching the current
  `requires-python = ">=3.10"`. One wheel per platform/arch instead of one per
  Python minor version.
- **maturin** replaces `setup.py` as the build backend (mixed Rust/Python
  project layout). `pyproject.toml` stays the single source of truth for the
  version; the `PYODBC_VERSION` macro trick disappears (Rust reads the version
  via `env!("CARGO_PKG_VERSION")`, and Cargo.toml gets it injected by maturin
  from pyproject.toml — or vice versa; one file must win, and pyproject.toml
  stays the winner per CLAUDE.md).
- Dev loop replacement for `python setup.py build_ext --inplace`:
  `maturin develop` (documented in HACKING.md). `PYODBC_TESTLOCAL` /
  `tests/conftest.py` build-dir logic becomes unnecessary but is kept working
  until the final phase.

### ODBC bindings: `odbc-sys` + an in-house safe layer, not `odbc-api`

Use the **`odbc-sys`** crate (raw FFI declarations only) and port pyodbc's own
handle/quirk logic on top of it, rather than adopting the higher-level
`odbc-api` crate.

Rationale: pyodbc's actual value is two decades of driver-quirk handling, and
`odbc-api` makes its own opinionated choices in exactly the places pyodbc
needs control:

- the four independent `TextEnc` configurations (read SQL_CHAR, read
  SQL_WCHAR, write, and `SQL_WMETADATA` for column names — Postgres/MySQL
  return UTF-16LE metadata regardless of connection settings);
- output converters operating on raw bytes before decoding;
- `SQLDescribeParam`-driven NULL binding and the `CnxnInfo` capability cache;
- `SQLPutData` streaming (`maxwrite`), `readvar_initsize` chunked reads,
  `need_long_data_len`;
- TVPs and the array-binding `fast_executemany` path;
- `attrs_before`, `driver_completion`, and raw `set_attr`/`getinfo`.

The in-house layer is small: RAII wrappers for `HENV`/`HDBC`/`HSTMT` (the Rust
equivalent of `wrapper.h`), a diagnostics reader, and typed wrappers for the
~30 ODBC entry points pyodbc calls. `unsafe` is confined to this module.

SQLWCHAR: keep the current code's position — buffers are always 16-bit
(`u16`), never `wchar_t` (see HACKING.md). Target unixODBC and Windows first;
iODBC (32-bit SQLWCHAR builds) is explicitly deferred (open question below).

### Concurrency model: one dedicated OS thread per connection

ODBC calls block, so asyncio integration means offloading them. The model:

- Every `Connection` owns **one worker OS thread** and an MPSC command queue.
  All ODBC calls for that connection *and all its cursors* run on that thread,
  in submission order. This serializes access to the `HDBC`/`HSTMT`s (the
  safest interpretation of driver thread-safety, and consistent with DB API
  transaction semantics — pyodbc already documents `threadsafety = 1`).
- An async method builds an `asyncio.Future`, enqueues a command, and returns
  the future. The worker executes the ODBC call **without the GIL**, acquires
  the GIL only to build result objects, and completes the future with
  `loop.call_soon_threadsafe`.
- **No tokio.** Every operation is a blocking FFI call executed on its
  connection's thread; a multiplexed async runtime adds a dependency and an
  extra hop for zero benefit. (`pyo3-async-runtimes` stays a fallback if the
  hand-rolled bridge proves fiddly, but the oneshot-per-command design is
  ~200 lines.)
- `fetchall()`/`fetchmany(n)` execute the whole fetch loop in **one** dispatch
  to the worker (one future per call, not per row), so per-row round-trip
  overhead only applies to `fetchone`/`async for`.
- `cursor.cancel()` stays **synchronous** and calls `SQLCancel` directly from
  the calling thread — that is exactly the cross-thread use `SQLCancel` is
  designed for, and it must not queue behind the operation it is cancelling.
- Connection close joins the worker thread. Dropping an unclosed connection
  at GC enqueues a close and detaches (never block the event loop in a
  destructor); `close()` remains the correct explicit path, as today.
- The environment handle (`HENV`), `pooling`, `odbcversion`, `drivers()`,
  `dataSources()` stay process-global as today; driver-manager-only calls are
  fast and remain synchronous.

Python-level callbacks that run under the GIL on the worker thread: output
converters and exotic-codec text decoding (UTF-8/UTF-16LE/BE/ASCII/latin-1 are
decoded natively in Rust; anything else falls back to Python's codec
machinery, matching current behavior).

### API shape: what becomes `await`, what stays plain

Async (coroutine / awaitable):

| Object | Methods |
|---|---|
| module | `connect()` |
| Connection | `execute()`, `commit()`, `rollback()`, `close()`, `getinfo()`, `set_attr()`, `__aenter__`/`__aexit__` |
| Cursor | `execute()`, `executemany()`, `fetchone()`, `fetchmany()`, `fetchall()`, `fetchval()`, `skip()`, `nextset()`, `commit()`, `rollback()`, `close()`, `__aenter__`/`__aexit__`, `__anext__`, and all catalog methods (`tables()`, `columns()`, `statistics()`, `primaryKeys()`, `foreignKeys()`, `procedures()`, `procedureColumns()`, `getTypeInfo()`, `rowIdColumns()`, `rowVerColumns()`, `tablePrivileges()`) |

Synchronous (no ODBC round-trip, or config-only, or by design):

- `Connection.cursor()` — allocates no `HSTMT`; the statement handle is
  created lazily on the worker at first use. Keeps fixtures sync.
- `setencoding()`/`setdecoding()`, `add_output_converter()` and friends,
  `arraysize`, `fast_executemany`, `rows_as_dicts`, `noscan` (stored, applied
  on the worker before the next execute), `maxwrite`, `readvar_initsize`.
- `description`, `rowcount`, `messages`, `searchescape` (cached), `closed`.
- `Row` — entirely sync, pickleable, name access, usable after close (as now).
- `cancel()` — sync by design (see above).
- Module level: `drivers()`, `dataSources()`, `get/setDecimalSeparator()`,
  `lowercase`, `native_uuid`, `pooling`, `odbcversion`, exceptions, constants.

Deliberate wrinkles:

- **`autocommit` / `timeout` property setters** perform an ODBC call but
  cannot be awaited. Decision: the setter dispatches to the worker and blocks
  the calling thread until done (these are rare, short calls), so
  `cnxn.autocommit = False` keeps working in tests verbatim. Async variants
  (`await cnxn.set_autocommit(...)`) are provided for purists.
- **`connect()`** returns an object that is *both* awaitable and an async
  context manager, so `async with pyodbc.connect(...) as cnxn:` works in one
  line (aiohttp-style), as does `cnxn = await pyodbc.connect(...)`.
- **`Connection.__aexit__` commits, it does not close** — preserving pyodbc's
  documented (and surprising) `__exit__` semantics that the tests rely on.
- `execute()` keeps returning the cursor so chained calls survive as
  `await (await cursor.execute(sql)).fetchone()` (or two statements).

### Package layout

```
Cargo.toml                  # crate: pyodbc-rs → extension module pyodbc._core
rust/
  lib.rs                    # #[pymodule], constants, exceptions
  handles.rs                # RAII HENV/HDBC/HSTMT, diagnostics (was wrapper.h/errors.cpp)
  worker.rs                 # per-connection thread, command queue, future bridge
  connection.rs             # (was connection.cpp)
  cursor.rs                 # (was cursor.cpp)
  params.rs                 # binding, executemany, fast path, TVPs (was params.cpp)
  getdata.rs                # fetch-side conversion, output converters (was getdata.cpp)
  row.rs                    # Row type (was row.cpp)
  textenc.rs                # the four TextEncs (was textenc.cpp)
  decimal.rs                # NUMERIC_STRUCT ↔ decimal.Decimal (was decimal.cpp)
  cnxninfo.rs               # per-connstring capability cache (was cnxninfo.cpp)
  errors.rs                 # SQLSTATE → DB API exception mapping
python/pyodbc/
  __init__.py               # re-exports; thin async glue (awaitable-connect wrapper)
  __init__.pyi              # evolved from src/pyodbc.pyi, async signatures
```

Most logic lives in Rust; the Python layer is kept to a few dozen lines of
ergonomic glue. The C++ `src/` tree stays in place, untouched, until the final
phase (parity proven), then is deleted in one commit.

---

## Test migration (the only test changes allowed)

Mechanical transforms, applied uniformly to all four suites:

1. Add `pytest-asyncio` to `requirements-dev.txt`; set
   `asyncio_mode = auto` in a `pytest.ini`/`pyproject.toml` block so test
   functions don't each need a marker.
2. Fixtures: `cnxn` becomes an async fixture (`await pyodbc.connect(...)`,
   `await c.close()`); `cursor` becomes async (its setup runs
   `await cur.execute("drop table ...")`).
3. Test functions: `def test_x(cursor)` → `async def test_x(cursor)`; every
   `execute`/`fetch*`/`commit`/`rollback`/`close`/`nextset`/catalog call gets
   `await`; chained `cursor.execute(sql).fetchall()` becomes
   `(await cursor.execute(sql)).fetchall()` → with the fetch awaited too
   (~135 chain sites across the four files).
4. `for row in cursor:` → `async for row in cursor:`;
   `with pyodbc.connect(...) as cnxn:` → `async with pyodbc.connect(...) as cnxn:`.
5. Nothing else: same assertions, same test names, same parametrization, same
   fencepost data, same env-var connection strings.

`cnxn.autocommit = False` sites (3 across suites) stay verbatim thanks to the
blocking-setter decision.

---

## Phases

Each phase ends with a commit on this branch and a green exit criterion.

### Phase 0 — Scaffolding
Maturin/PyO3 project builds an importable `pyodbc` exposing constants, the
exception hierarchy, `version`, `drivers()`, `dataSources()`. CI job compiles
the crate and runs `import pyodbc`. C++ build still intact alongside.
*Exit: `maturin develop && python -c "import pyodbc"`.*

### Phase 1 — Async core (SQLite happy path)
Worker-thread executor, future bridge, `connect` / `close` / `commit` /
`rollback` / autocommit; cursor `execute` with basic parameter binding (None,
int, float, str, bytes, bool, datetime), `fetchone/many/all/val`,
`description`, `rowcount`, `Row` with name access, async iteration and context
managers.
*Exit: a hand-picked ~half of `sqlite_test.py` (converted) passes.*

### Phase 2 — Convert the test suites
Apply the mechanical async transforms to all four test files + pytest-asyncio
wiring. SQLite suite runs (expected failures mark the remaining gap = the work
list for phases 3–5).
*Exit: converted suites collect cleanly; sqlite subset from Phase 1 still green.*

### Phase 3 — Types, encodings, fetch-side completeness
Port `textenc` (four TextEncs, `setencoding`/`setdecoding`, `SQL_WMETADATA`),
`decimal` (NUMERIC_STRUCT default + `fetch_decimal_as_string` legacy path,
decimal separator), date/time/TIME2/GUID/`native_uuid`, long-data chunked
reads (`readvar_initsize`), `SQLPutData` writes (`maxwrite`), output
converters, `rows_as_dicts`, `skip`, `nextset`, `messages`,
`compat_diagrec_byte_length`.
*Exit: `sqlite_test.py` fully green.*

### Phase 4 — Parameter-side completeness
`CnxnInfo` cache, `SQLDescribeParam` NULL handling, `setinputsizes`,
`executemany` (iterate) and `fast_executemany` (array binding), TVPs
(SQL Server), `BinaryNull`.
*Exit: `sqlserver_test.py` green against a real SQL Server (CI container).*

### Phase 5 — Metadata & remaining surface
All catalog methods, `getinfo` (typed per info-id), `set_attr`,
`attrs_before`, `driver_completion`, `timeout`, `searchescape`, `lowercase`,
`pooling`/`odbcversion` env controls, `hdbc`/`hstmt`/`henv` handle exposure.
*Exit: `postgresql_test.py` and `mysql_test.py` green.*

### Phase 6 — CI matrix & packaging
Rework `.github/workflows/ubuntu_build.yml` to maturin builds; keep the same
DB service containers and driver installs; wheels via `maturin` (abi3) on the
platforms cibuildwheel covered; sdist builds from source with only a Rust
toolchain + unixODBC headers.
*Exit: full CI matrix green; wheel smoke-tested.*

### Phase 7 — Cutover
Delete `src/*.cpp` (keep `pyodbc.pyi` history via the new `.pyi`), remove
`setup.py`, update README/HACKING/CLAUDE.md/docs, bump version to `6.0.0a1`.
*Exit: repo contains no C++; docs describe the async API.*

---

## Parity checklist (easy-to-lose behaviors)

- Error text format `('HYT00', '[HYT00] [unixODBC]... (0) (SQLDriverConnect)')`
  and SQLSTATE→exception-class mapping (`errors.cpp` table).
- `description` 7-tuples including the type-code Python classes and nullable
  flag; the shared name→index map semantics between Cursor and Row
  (`lowercase` handling included).
- Row: pickling, `__reduce__`, attribute *assignment*, slicing, comparison,
  `cursor_description` surviving cursor/connection close.
- The three read paths / fencepost sizes (small bound, `SQLGetData` chunked,
  long) — driven by `readvar_initsize`.
- ODBC length-argument rules (characters vs bytes) — port faithfully from
  `notes.txt`.
- Connection pooling default on; `pooling`/`odbcversion` must be set before
  first connect (same runtime error otherwise).
- `fast_executemany` limitations documented in the wiki must fail the same way.

## Risks

1. **Encoding subtleties** are the historical bug farm (SQLWCHAR width,
   metadata encoding, byte-vs-char lengths). Mitigation: port the C++ logic
   line-by-line rather than redesigning; the fencepost tests exist for this.
2. **Per-row `fetchone` latency** gains a thread hop. Mitigation: whole-loop
   dispatch for `fetchmany/fetchall`; optional small read-ahead for
   `async for` later if benchmarks demand it.
3. **Blocking property setters** technically stall the event loop for one ODBC
   call. Accepted and documented; async alternatives provided.
4. **Driver quirk regressions** only surface against real drivers — hence real
   DB containers in CI from Phase 4 onward, not just SQLite.
5. **GC of unclosed connections** must never block or touch the loop from the
   wrong thread; the detach-on-drop design needs careful review.

## Open questions (decide before Phase 0 completes)

1. **Distribution name.** Same import name `pyodbc` at major version 6, or a
   new name (`aiopyodbc`) since async-only is API-breaking for every existing
   user? Recommendation: keep `pyodbc` import name in this fork, decide the
   PyPI question at release time.
2. **iODBC / 32-bit SQLWCHAR builds** (mostly macOS iODBC users): support in
   the first release or document unixODBC-only?
3. **MSRV / Rust toolchain floor** for downstream packagers (suggest: latest
   stable, revisit at release).
