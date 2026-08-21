# Type stubs for aiodbc._core, the compiled Rust extension module.  The public
# package interface is assembled by aiodbc/__init__.py on top of these names.
#
# ignore line spacing (E303), mixed case names (N802/N803)
# ruff: noqa: E303, N802, N803
from __future__ import annotations
from collections.abc import AsyncIterator, Awaitable, Generator, Iterator, Sequence
from typing import Any, Callable, Final
import ctypes

# SQLSetConnectAttr attributes
# ref: https://docs.microsoft.com/en-us/sql/odbc/reference/syntax/sqlsetconnectattr-function
SQL_ATTR_ACCESS_MODE: int
SQL_ATTR_AUTOCOMMIT: int
SQL_ATTR_CURRENT_CATALOG: int
SQL_ATTR_LOGIN_TIMEOUT: int
SQL_ATTR_ODBC_CURSORS: int
SQL_ATTR_QUIET_MODE: int
SQL_ATTR_TRACE: int
SQL_ATTR_TRACEFILE: int
SQL_ATTR_TRANSLATE_LIB: int
SQL_ATTR_TRANSLATE_OPTION: int
SQL_ATTR_TXN_ISOLATION: int
# other (e.g. specific to certain RDBMSs)
SQL_ACCESS_MODE: int
SQL_AUTOCOMMIT: int
SQL_CURRENT_QUALIFIER: int
SQL_LOGIN_TIMEOUT: int
SQL_ODBC_CURSORS: int
SQL_OPT_TRACE: int
SQL_OPT_TRACEFILE: int
SQL_PACKET_SIZE: int
SQL_QUIET_MODE: int
SQL_TRANSLATE_DLL: int
SQL_TRANSLATE_OPTION: int
SQL_TXN_ISOLATION: int
# Unicode
SQL_ATTR_ANSI_APP: int

# ODBC column data types
# https://docs.microsoft.com/en-us/sql/odbc/reference/appendixes/appendix-d-data-types
SQL_UNKNOWN_TYPE: int
SQL_CHAR: int
SQL_VARCHAR: int
SQL_LONGVARCHAR: int
SQL_WCHAR: int
SQL_WVARCHAR: int
SQL_WLONGVARCHAR: int
SQL_DECIMAL: int
SQL_NUMERIC: int
SQL_SMALLINT: int
SQL_INTEGER: int
SQL_REAL: int
SQL_FLOAT: int
SQL_DOUBLE: int
SQL_BIT: int
SQL_TINYINT: int
SQL_BIGINT: int
SQL_BINARY: int
SQL_VARBINARY: int
SQL_LONGVARBINARY: int
SQL_TYPE_DATE: int
SQL_TYPE_TIME: int
SQL_TYPE_TIMESTAMP: int
SQL_SS_TIME2: int
SQL_SS_VARIANT: int
SQL_SS_XML: int
SQL_INTERVAL_MONTH: int
SQL_INTERVAL_YEAR: int
SQL_INTERVAL_YEAR_TO_MONTH: int
SQL_INTERVAL_DAY: int
SQL_INTERVAL_HOUR: int
SQL_INTERVAL_MINUTE: int
SQL_INTERVAL_SECOND: int
SQL_INTERVAL_DAY_TO_HOUR: int
SQL_INTERVAL_DAY_TO_MINUTE: int
SQL_INTERVAL_DAY_TO_SECOND: int
SQL_INTERVAL_HOUR_TO_MINUTE: int
SQL_INTERVAL_HOUR_TO_SECOND: int
SQL_INTERVAL_MINUTE_TO_SECOND: int
SQL_GUID: int
# SQLDescribeCol
SQL_NO_NULLS: int
SQL_NULLABLE: int
SQL_NULLABLE_UNKNOWN: int
# specific to aiodbc
SQL_WMETADATA: int

# SQL_CONVERT_X
SQL_CONVERT_FUNCTIONS: int
SQL_CONVERT_BIGINT: int
SQL_CONVERT_BINARY: int
SQL_CONVERT_BIT: int
SQL_CONVERT_CHAR: int
SQL_CONVERT_DATE: int
SQL_CONVERT_DECIMAL: int
SQL_CONVERT_DOUBLE: int
SQL_CONVERT_FLOAT: int
SQL_CONVERT_GUID: int
SQL_CONVERT_INTEGER: int
SQL_CONVERT_INTERVAL_DAY_TIME: int
SQL_CONVERT_INTERVAL_YEAR_MONTH: int
SQL_CONVERT_LONGVARBINARY: int
SQL_CONVERT_LONGVARCHAR: int
SQL_CONVERT_NUMERIC: int
SQL_CONVERT_REAL: int
SQL_CONVERT_SMALLINT: int
SQL_CONVERT_TIME: int
SQL_CONVERT_TIMESTAMP: int
SQL_CONVERT_TINYINT: int
SQL_CONVERT_VARBINARY: int
SQL_CONVERT_VARCHAR: int
SQL_CONVERT_WCHAR: int
SQL_CONVERT_WLONGVARCHAR: int
SQL_CONVERT_WVARCHAR: int

# transaction isolation
# ref: https://docs.microsoft.com/en-us/sql/relational-databases/native-client-odbc-cursors/properties/cursor-transaction-isolation-level
SQL_TXN_READ_COMMITTED: int
SQL_TXN_READ_UNCOMMITTED: int
SQL_TXN_REPEATABLE_READ: int
SQL_TXN_SERIALIZABLE: int

# outer join capabilities
SQL_OJ_LEFT: int
SQL_OJ_RIGHT: int
SQL_OJ_FULL: int
SQL_OJ_NESTED: int
SQL_OJ_NOT_ORDERED: int
SQL_OJ_INNER: int
SQL_OJ_ALL_COMPARISON_OPS: int

# other ODBC database constants
SQL_SCOPE_CURROW: int
SQL_SCOPE_TRANSACTION: int
SQL_SCOPE_SESSION: int
SQL_PC_UNKNOWN: int
SQL_PC_NOT_PSEUDO: int
SQL_PC_PSEUDO: int
# SQL_INDEX_BTREE: int
# SQL_INDEX_CLUSTERED: int
# SQL_INDEX_CONTENT: int
# SQL_INDEX_HASHED: int
# SQL_INDEX_OTHER: int

# attributes for the ODBC SQLGetInfo function
# https://docs.microsoft.com/en-us/sql/odbc/reference/syntax/sqlgetinfo-function
SQL_ACCESSIBLE_PROCEDURES: int
SQL_ACCESSIBLE_TABLES: int
SQL_ACTIVE_ENVIRONMENTS: int
SQL_AGGREGATE_FUNCTIONS: int
SQL_ALTER_DOMAIN: int
SQL_ALTER_TABLE: int
SQL_ASYNC_MODE: int
SQL_BATCH_ROW_COUNT: int
SQL_BATCH_SUPPORT: int
SQL_BOOKMARK_PERSISTENCE: int
SQL_CATALOG_LOCATION: int
SQL_CATALOG_NAME: int
SQL_CATALOG_NAME_SEPARATOR: int
SQL_CATALOG_TERM: int
SQL_CATALOG_USAGE: int
SQL_COLLATION_SEQ: int
SQL_COLUMN_ALIAS: int
SQL_CONCAT_NULL_BEHAVIOR: int
SQL_CORRELATION_NAME: int
SQL_CREATE_ASSERTION: int
SQL_CREATE_CHARACTER_SET: int
SQL_CREATE_COLLATION: int
SQL_CREATE_DOMAIN: int
SQL_CREATE_SCHEMA: int
SQL_CREATE_TABLE: int
SQL_CREATE_TRANSLATION: int
SQL_CREATE_VIEW: int
SQL_CURSOR_COMMIT_BEHAVIOR: int
SQL_CURSOR_ROLLBACK_BEHAVIOR: int
# SQL_CURSOR_ROLLBACK_SQL_CURSOR_SENSITIVITY: int
SQL_DATABASE_NAME: int
SQL_DATA_SOURCE_NAME: int
SQL_DATA_SOURCE_READ_ONLY: int
SQL_DATETIME_LITERALS: int
SQL_DBMS_NAME: int
SQL_DBMS_VER: int
SQL_DDL_INDEX: int
SQL_DEFAULT_TXN_ISOLATION: int
SQL_DESCRIBE_PARAMETER: int
SQL_DM_VER: int
SQL_DRIVER_HDESC: int
SQL_DRIVER_HENV: int
SQL_DRIVER_HLIB: int
SQL_DRIVER_HSTMT: int
SQL_DRIVER_NAME: int
SQL_DRIVER_ODBC_VER: int
SQL_DRIVER_VER: int
SQL_DROP_ASSERTION: int
SQL_DROP_CHARACTER_SET: int
SQL_DROP_COLLATION: int
SQL_DROP_DOMAIN: int
SQL_DROP_SCHEMA: int
SQL_DROP_TABLE: int
SQL_DROP_TRANSLATION: int
SQL_DROP_VIEW: int
SQL_DYNAMIC_CURSOR_ATTRIBUTES1: int
SQL_DYNAMIC_CURSOR_ATTRIBUTES2: int
SQL_EXPRESSIONS_IN_ORDERBY: int
SQL_FILE_USAGE: int
SQL_FORWARD_ONLY_CURSOR_ATTRIBUTES1: int
SQL_FORWARD_ONLY_CURSOR_ATTRIBUTES2: int
SQL_GETDATA_EXTENSIONS: int
SQL_GROUP_BY: int
SQL_IDENTIFIER_CASE: int
SQL_IDENTIFIER_QUOTE_CHAR: int
SQL_INDEX_KEYWORDS: int
SQL_INFO_SCHEMA_VIEWS: int
SQL_INSERT_STATEMENT: int
SQL_INTEGRITY: int
SQL_KEYSET_CURSOR_ATTRIBUTES1: int
SQL_KEYSET_CURSOR_ATTRIBUTES2: int
SQL_KEYWORDS: int
SQL_LIKE_ESCAPE_CLAUSE: int
SQL_MAX_ASYNC_CONCURRENT_STATEMENTS: int
SQL_MAX_BINARY_LITERAL_LEN: int
SQL_MAX_CATALOG_NAME_LEN: int
SQL_MAX_CHAR_LITERAL_LEN: int
SQL_MAX_COLUMNS_IN_GROUP_BY: int
SQL_MAX_COLUMNS_IN_INDEX: int
SQL_MAX_COLUMNS_IN_ORDER_BY: int
SQL_MAX_COLUMNS_IN_SELECT: int
SQL_MAX_COLUMNS_IN_TABLE: int
SQL_MAX_COLUMN_NAME_LEN: int
SQL_MAX_CONCURRENT_ACTIVITIES: int
SQL_MAX_CURSOR_NAME_LEN: int
SQL_MAX_DRIVER_CONNECTIONS: int
SQL_MAX_IDENTIFIER_LEN: int
SQL_MAX_INDEX_SIZE: int
SQL_MAX_PROCEDURE_NAME_LEN: int
SQL_MAX_ROW_SIZE: int
SQL_MAX_ROW_SIZE_INCLUDES_LONG: int
SQL_MAX_SCHEMA_NAME_LEN: int
SQL_MAX_STATEMENT_LEN: int
SQL_MAX_TABLES_IN_SELECT: int
SQL_MAX_TABLE_NAME_LEN: int
SQL_MAX_USER_NAME_LEN: int
SQL_MULTIPLE_ACTIVE_TXN: int
SQL_MULT_RESULT_SETS: int
SQL_NEED_LONG_DATA_LEN: int
SQL_NON_NULLABLE_COLUMNS: int
SQL_NULL_COLLATION: int
SQL_NUMERIC_FUNCTIONS: int
SQL_ODBC_INTERFACE_CONFORMANCE: int
SQL_ODBC_VER: int
SQL_OJ_CAPABILITIES: int
SQL_ORDER_BY_COLUMNS_IN_SELECT: int
SQL_PARAM_ARRAY_ROW_COUNTS: int
SQL_PARAM_ARRAY_SELECTS: int
SQL_PARAM_TYPE_UNKNOWN: int
SQL_PARAM_INPUT: int
SQL_PARAM_INPUT_OUTPUT: int
SQL_PARAM_OUTPUT: int
SQL_RETURN_VALUE: int
SQL_RESULT_COL: int
SQL_PROCEDURES: int
SQL_PROCEDURE_TERM: int
SQL_QUOTED_IDENTIFIER_CASE: int
SQL_ROW_UPDATES: int
SQL_SCHEMA_TERM: int
SQL_SCHEMA_USAGE: int
SQL_SCROLL_OPTIONS: int
SQL_SEARCH_PATTERN_ESCAPE: int
SQL_SERVER_NAME: int
SQL_SPECIAL_CHARACTERS: int
SQL_SQL92_DATETIME_FUNCTIONS: int
SQL_SQL92_FOREIGN_KEY_DELETE_RULE: int
SQL_SQL92_FOREIGN_KEY_UPDATE_RULE: int
SQL_SQL92_GRANT: int
SQL_SQL92_NUMERIC_VALUE_FUNCTIONS: int
SQL_SQL92_PREDICATES: int
SQL_SQL92_RELATIONAL_JOIN_OPERATORS: int
SQL_SQL92_REVOKE: int
SQL_SQL92_ROW_VALUE_CONSTRUCTOR: int
SQL_SQL92_STRING_FUNCTIONS: int
SQL_SQL92_VALUE_EXPRESSIONS: int
SQL_SQL_CONFORMANCE: int
SQL_STANDARD_CLI_CONFORMANCE: int
SQL_STATIC_CURSOR_ATTRIBUTES1: int
SQL_STATIC_CURSOR_ATTRIBUTES2: int
SQL_STRING_FUNCTIONS: int
SQL_SUBQUERIES: int
SQL_SYSTEM_FUNCTIONS: int
SQL_TABLE_TERM: int
SQL_TIMEDATE_ADD_INTERVALS: int
SQL_TIMEDATE_DIFF_INTERVALS: int
SQL_TIMEDATE_FUNCTIONS: int
SQL_TXN_CAPABLE: int
SQL_TXN_ISOLATION_OPTION: int
SQL_UNION: int
SQL_USER_NAME: int
SQL_XOPEN_CLI_YEAR: int

# driver connect completion modes
SQL_DRIVER_COMPLETE: int
SQL_DRIVER_COMPLETE_REQUIRED: int
SQL_DRIVER_NOPROMPT: int
SQL_DRIVER_PROMPT: int

# aiodbc-specific constants
BinaryNull: Any  # to distinguish binary NULL values from char NULL values
SQLWCHAR_SIZE: int


# exceptions
# https://www.python.org/dev/peps/pep-0249/#exceptions
class Warning(Exception): ...
class Error(Exception): ...
class InterfaceError(Error): ...
class DatabaseError(Error): ...
class DataError(DatabaseError): ...
class OperationalError(DatabaseError): ...
class IntegrityError(DatabaseError): ...
class InternalError(DatabaseError): ...
class ProgrammingError(DatabaseError): ...
class NotSupportedError(DatabaseError): ...


class Connection:
    """An open ODBC connection.  Every ODBC call for the connection (and its
    cursors) runs on a dedicated worker thread; the async methods return asyncio
    futures completed from that thread.
    https://www.python.org/dev/peps/pep-0249/#connection-objects

    Not instantiated directly: call aiodbc.connect() and await the result (or use
    it as an async context manager).  Connection objects cannot be pickled.
    """

    @property
    def autocommit(self) -> bool:
        """Whether the database automatically commits after every successful
        statement.  Default is False.  The setter performs an ODBC call and blocks
        briefly."""
        ...

    @autocommit.setter
    def autocommit(self, value: bool) -> None: ...

    @property
    def closed(self) -> bool:
        """True once close() has completed."""
        ...

    @property
    def fetch_decimal_as_string(self) -> bool:
        """If True, DECIMAL and NUMERIC values are fetched as text using the legacy
        locale-aware path.  If False (the default), values are fetched using a
        binary representation that is not affected by the locale."""
        ...

    @fetch_decimal_as_string.setter
    def fetch_decimal_as_string(self, value: bool) -> None: ...

    @property
    def hdbc(self) -> ctypes.c_void_p | None:
        """The raw ODBC connection handle, or None once closed."""
        ...

    @property
    def compat_diagrec_byte_length(self) -> bool:
        """Set to True if the driver incorrectly reports byte length instead of
        character length for diagnostic messages.

        See https://github.com/mkleehammer/pyodbc/issues/489."""
        ...

    @compat_diagrec_byte_length.setter
    def compat_diagrec_byte_length(self, value: bool) -> None: ...

    @property
    def maxwrite(self) -> int:
        """The maximum bytes to write before using SQLPutData, default is zero for
        no maximum."""
        ...

    @maxwrite.setter
    def maxwrite(self, value: int) -> None: ...

    @property
    def readvar_initsize(self) -> int:
        """The initial buffer size in bytes for reading variable-length columns."""
        ...

    @readvar_initsize.setter
    def readvar_initsize(self, value: int) -> None: ...

    @property
    def searchescape(self) -> str:
        """The character for escaping search pattern characters like "%" and "_".
        Fetched from the driver once and cached."""
        ...

    @property
    def timeout(self) -> int:
        """The query timeout in seconds, zero (the default) for no timeout.  The
        setter performs an ODBC call and blocks briefly."""
        ...

    @timeout.setter
    def timeout(self, value: int) -> None: ...

    # async context manager: commits on clean exit, rolls back on error, and (like
    # the synchronous aiodbc Connection) does NOT close the connection
    async def __aenter__(self) -> Connection: ...
    async def __aexit__(self, exc_type, exc_value, traceback) -> bool: ...

    # text encodings

    def setencoding(self,
                    encoding: str | None = None,
                    ctype: int | None = None) -> None:
        """Set the text encoding for SQL statements and textual parameters sent to
        the database.

        Args:
            encoding: Text encoding codec, e.g. "utf-8".
            ctype: The C data type when passing data - either aiodbc.SQL_CHAR or
                aiodbc.SQL_WCHAR.
        """
        ...

    def setdecoding(self,
                    sqltype: int,
                    encoding: str | None = None,
                    ctype: int | None = None) -> None:
        """Set the text decoding used when reading SQL_CHAR or SQL_WCHAR data.

        Args:
            sqltype: aiodbc.SQL_CHAR, aiodbc.SQL_WCHAR, or aiodbc.SQL_WMETADATA.
            encoding: Text encoding codec, e.g. "utf-8".
            ctype: The C data type to request from SQLGetData - either
                aiodbc.SQL_CHAR or aiodbc.SQL_WCHAR.
        """
        ...

    # connection attributes

    def getinfo(self, infotype: int, /) -> Awaitable[Any]:
        """Retrieve general information about the driver and the data source, via
        SQLGetInfo.  The result type depends on the requested information."""
        ...

    def set_attr(self, attr_id: int, value: int | str, /) -> None:
        """Set an attribute on the connection, via SQLSetConnectAttr."""
        ...

    # non-standard database data types

    def add_output_converter(self, sqltype: int, func: Callable | None, /) -> None:
        """Register an output converter function called for every value with the
        given SQL type.  See:
        https://github.com/mkleehammer/pyodbc/wiki/Using-an-Output-Converter-function
        """
        ...

    def get_output_converter(self, sqltype: int, /) -> Callable | None:
        """The converter registered for the SQL type, or None."""
        ...

    def remove_output_converter(self, sqltype: int, /) -> None:
        """Delete a previously registered output converter function."""
        ...

    def clear_output_converters(self) -> None:
        """Delete all previously registered converter functions."""
        ...

    # transactions and cursors

    def cursor(self) -> Cursor:
        """Create a new cursor on the connection."""
        ...

    def execute(self, sql: str, *params: Any) -> Awaitable[Cursor]:
        """Convenience: create a new cursor, execute the SQL on it, and resolve to
        that cursor."""
        ...

    def commit(self) -> Awaitable[None]:
        """Commit all pending work on the connection (all cursors)."""
        ...

    def rollback(self) -> Awaitable[None]:
        """Roll back all pending work on the connection (all cursors)."""
        ...

    def close(self) -> Awaitable[None]:
        """Close the connection.  Uncommitted work is rolled back."""
        ...


class Cursor:
    """A database cursor: executes SQL and iterates results.
    https://www.python.org/dev/peps/pep-0249/#cursor-objects

    Not instantiated directly: call Connection.cursor().  Execute and fetch
    methods return awaitables; the cursor is also an async iterator, so
    "async for row in await cursor.execute(...)" streams the result set.
    """

    arraysize: int
    """Number of rows fetchmany() returns when no size is given.  Default 1."""

    rows_as_dicts: bool
    """If True, rows are returned as {column name: value} dicts instead of Row
    objects.  Default False."""

    fast_executemany: bool
    """If True, executemany() binds all parameter rows as column-wise arrays and
    executes once, instead of executing per row.  Default False."""

    @property
    def connection(self) -> Connection:
        """The parent connection (DB API extension)."""
        ...

    @property
    def description(self) -> tuple[tuple[str, Any, int, int, int, int, bool], ...] | None:
        """The current result set's columns, as 7-tuples of
        (name, type_code, display_size, internal_size, precision, scale, nullable).
        None when no result set is open."""
        ...

    @property
    def rowcount(self) -> int:
        """Rows affected by the last statement, or -1 when not in use / unknown."""
        ...

    @property
    def messages(self) -> list[tuple[str, str]] | None:
        """Diagnostic messages (e.g. PRINT output) from the last execute, as
        (exception class name, message) tuples (DB API extension)."""
        ...

    @property
    def hstmt(self) -> ctypes.c_void_p | None:
        """The raw ODBC statement handle, or None once closed."""
        ...

    @property
    def noscan(self) -> bool:
        """The SQL_ATTR_NOSCAN statement attribute (blocks briefly)."""
        ...

    @noscan.setter
    def noscan(self, value: bool) -> None: ...

    # executing

    def execute(self, sql: str, *params: Any) -> Awaitable[Cursor]:
        """Prepare and execute the SQL.  Parameters may be passed as one sequence
        or as individual arguments.  Resolves to this cursor."""
        ...

    def executemany(self, sql: str, params: Sequence | Iterator | Generator, /) -> Awaitable[None]:
        """Execute the SQL once per parameter row (or once with column-wise arrays
        when fast_executemany is on)."""
        ...

    def setinputsizes(self, sizes: Sequence | None, /) -> None:
        """Per-parameter binding overrides for the next execute: each element is
        None, a SQL type, or a (SQL type, size, scale) tuple."""
        ...

    # fetching

    def fetchone(self) -> Awaitable[Row | None]:
        """Resolve to the next row, or None when the result set is exhausted."""
        ...

    def fetchmany(self, size: int | None = None, /) -> Awaitable[list[Row]]:
        """Resolve to the next `size` rows (default: arraysize)."""
        ...

    def fetchall(self) -> Awaitable[list[Row]]:
        """Resolve to all remaining rows."""
        ...

    def fetchval(self) -> Awaitable[Any]:
        """Resolve to the first column of the first row, or None (aiodbc
        extension)."""
        ...

    def skip(self, count: int, /) -> Awaitable[None]:
        """Discard the next `count` rows."""
        ...

    def nextset(self) -> Awaitable[bool]:
        """Advance to the next result set; resolves to True if one is available."""
        ...

    # catalog functions
    # https://github.com/mkleehammer/pyodbc/wiki/Cursor#catalog-functions

    def tables(self,
               table: str | None = None,
               catalog: str | None = None,
               schema: str | None = None,
               tableType: str | None = None) -> Awaitable[Cursor]: ...

    def tablePrivileges(self,
                        table: str | None = None,
                        catalog: str | None = None,
                        schema: str | None = None) -> Awaitable[Cursor]: ...

    def columns(self,
                table: str | None = None,
                catalog: str | None = None,
                schema: str | None = None,
                column: str | None = None) -> Awaitable[Cursor]: ...

    def statistics(self,
                   table: str,
                   catalog: str | None = None,
                   schema: str | None = None,
                   unique: bool = False,
                   quick: bool = True) -> Awaitable[Cursor]: ...

    def rowIdColumns(self,
                     table: str,
                     catalog: str | None = None,
                     schema: str | None = None,
                     nullable: bool = True) -> Awaitable[Cursor]: ...

    def rowVerColumns(self,
                      table: str,
                      catalog: str | None = None,
                      schema: str | None = None,
                      nullable: bool = True) -> Awaitable[Cursor]: ...

    def primaryKeys(self,
                    table: str,
                    catalog: str | None = None,
                    schema: str | None = None) -> Awaitable[Cursor]: ...

    def foreignKeys(self,
                    table: str | None = None,
                    catalog: str | None = None,
                    schema: str | None = None,
                    foreignTable: str | None = None,
                    foreignCatalog: str | None = None,
                    foreignSchema: str | None = None) -> Awaitable[Cursor]: ...

    def procedures(self,
                   procedure: str | None = None,
                   catalog: str | None = None,
                   schema: str | None = None) -> Awaitable[Cursor]: ...

    def procedureColumns(self,
                         procedure: str | None = None,
                         catalog: str | None = None,
                         schema: str | None = None) -> Awaitable[Cursor]: ...

    def getTypeInfo(self, sqlType: int | None = None, /) -> Awaitable[Cursor]: ...

    # transactions and lifecycle

    def commit(self) -> Awaitable[None]:
        """Commit all pending work on the parent connection."""
        ...

    def rollback(self) -> Awaitable[None]:
        """Roll back all pending work on the parent connection."""
        ...

    def cancel(self) -> None:
        """Cancel the currently running statement via SQLCancel (callable from any
        task/thread)."""
        ...

    def close(self) -> Awaitable[None]:
        """Close the cursor.  Further operations raise ProgrammingError."""
        ...

    # async iteration and context management

    def __aiter__(self) -> AsyncIterator[Row]: ...
    def __anext__(self) -> Awaitable[Row]: ...
    async def __aenter__(self) -> Cursor: ...
    async def __aexit__(self, exc_type, exc_value, traceback) -> bool: ...


class Row:
    """A sequence of column values for one result row.  Values are accessible by
    index (like a tuple) and by column-name attribute.  Rows remain usable after
    the cursor and connection are closed, and can be pickled.
    """

    @property
    def cursor_description(self) -> tuple[tuple[str, Any, int, int, int, int, bool], ...]:
        """The description of the cursor that produced this row (see
        Cursor.description)."""
        ...

    def __len__(self) -> int: ...
    def __getattr__(self, name: str) -> Any: ...
    def __setattr__(self, name: str, value: Any) -> None: ...
    def __getitem__(self, index: int | slice) -> Any: ...
    def __setitem__(self, index: int, value: Any) -> None: ...
    def __contains__(self, value: Any) -> bool: ...
    def __iter__(self) -> Iterator[Any]: ...
    def __repr__(self) -> str: ...
    def __eq__(self, other: Any) -> bool: ...
    def __lt__(self, other: Any) -> bool: ...
    def __le__(self, other: Any) -> bool: ...
    def __gt__(self, other: Any) -> bool: ...
    def __ge__(self, other: Any) -> bool: ...


# module functions

def drivers() -> list[str]:
    """The names of the installed ODBC drivers."""
    ...

def data_sources(*, scope: str | None = None) -> dict[str, str]:
    """The defined data sources (DSNs) and their drivers.  scope may be "user",
    "system", or None for both."""
    ...

def connect(connstring: str,
            *,
            autocommit: bool = False,
            readonly: bool = False,
            timeout: int = 0,
            encoding: str | None = None,
            driver_completion: int = 0,
            attrs_before: dict | None = None) -> Awaitable[Connection]:
    """Open a connection (the connection string is already fully assembled).
    Application code should call aiodbc.connect(), which builds the connection
    string from keyword arguments and wraps this."""
    ...

def set_decimal_separator(sep: str, /) -> None:
    """Set the decimal separator used when parsing NUMERIC/DECIMAL text."""
    ...

def get_decimal_separator() -> str:
    """The decimal separator used when parsing NUMERIC/DECIMAL text."""
    ...

def _henv() -> int:
    """The shared ODBC environment handle, allocated on first use (internal;
    aiodbc.henv wraps it in ctypes.c_void_p)."""
    ...
