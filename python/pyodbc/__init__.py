"""
pyodbc - an asyncio-native DB API 2.0 module for ODBC, implemented in Rust.

This package is being ported from the C++ implementation in src/ per
docs/rust-asyncio-rewrite-plan.md.  The compiled Rust core is pyodbc._core; this
module assembles the public API on top of it.
"""

import datetime as _datetime
import locale as _locale
from importlib.metadata import PackageNotFoundError as _PackageNotFoundError
from importlib.metadata import version as _dist_version

# ODBC constants, the exception hierarchy, Connection/Cursor/Row, drivers() and
# data_sources() all come from the Rust core.
from pyodbc._core import *  # noqa: F401,F403
from pyodbc import _core
from pyodbc._core import data_sources as dataSources  # noqa: F401,N812  (historical name)

# https://peps.python.org/pep-0249/#globals
apilevel = '2.0'
paramstyle = 'qmark'
threadsafety = 1

# The single source of truth for the version is pyproject.toml (see CLAUDE.md);
# maturin records it in the installed distribution's metadata.
try:
    version = _dist_version('pyodbc')
except _PackageNotFoundError:  # e.g. the module was imported straight from a build dir
    version = '0.0.0.dev0'

# Read-write module-level configuration.  Like the C++ implementation, pooling and
# odbcversion are read when the shared ODBC environment handle is allocated, so they
# must be set before the first connection (or drivers()/dataSources() call).
pooling = True
lowercase = False
native_uuid = False
odbcversion = '3.X'

# DB API type objects and constructors.
# https://peps.python.org/pep-0249/#type-objects-and-constructors
Date = _datetime.date
Time = _datetime.time
Timestamp = _datetime.datetime
DATETIME = _datetime.datetime
STRING = str
NUMBER = float
ROWID = int
BINARY = bytearray
Binary = bytearray


def DateFromTicks(ticks):  # noqa: N802
    """Return a date object, given the ticks value (number of seconds since the epoch)."""
    return _datetime.date.fromtimestamp(ticks)


def TimeFromTicks(ticks):  # noqa: N802
    """Return a time object, given the ticks value (number of seconds since the epoch)."""
    return _datetime.datetime.fromtimestamp(ticks).time()


def TimestampFromTicks(ticks):  # noqa: N802
    """Return a datetime object, given the ticks value (number of seconds since the epoch)."""
    return _datetime.datetime.fromtimestamp(ticks)


# The decimal separator used when parsing NUMERIC/DECIMAL text defaults to the
# locale's, like InitializeDecimal in decimal.cpp.  setDecimalSeparator overrides it.
try:
    _core.set_decimal_separator(_locale.localeconv().get('decimal_point', '.'))
except Exception:
    _core.set_decimal_separator('.')

# Historical camelCase names for the decimal separator functions.
setDecimalSeparator = _core.set_decimal_separator  # noqa: N816
getDecimalSeparator = _core.get_decimal_separator  # noqa: N816


def __getattr__(name):
    # pyodbc.henv: the shared ODBC environment handle, allocated on first use like
    # the C++ module's mod_getattr.
    if name == 'henv':
        import ctypes
        return ctypes.c_void_p(_core._henv())
    raise AttributeError(f"module 'pyodbc' has no attribute '{name}'")


# Map DB API recommended connect() keywords to ODBC connection string keywords,
# like mod_connect in pyodbcmodule.cpp.
_KEYWORD_ALIASES = {'user': 'uid', 'password': 'pwd', 'host': 'server'}

# driver_completion values accepted on non-Windows platforms (SQL_DRIVER_PROMPT
# requires a window handle).  0=NOPROMPT, 1=COMPLETE, 2=PROMPT, 3=COMPLETE_REQUIRED.
_SQL_DRIVER_PROMPT = 2
_VALID_DRIVER_COMPLETION = (0, 1, 2, 3)


class _PendingConnection:
    """What connect() returns: awaitable, and usable as an async context manager,
    so both ``cnxn = await pyodbc.connect(cs)`` and ``async with pyodbc.connect(cs)
    as cnxn:`` work.  The ODBC connection is not opened until awaited/entered."""

    __slots__ = ('_args', '_future', '_cnxn')

    def __init__(self, args):
        self._args = args
        self._future = None
        self._cnxn = None

    def _start(self):
        if self._future is None:
            connstring, kwargs = self._args
            self._future = _core.connect(connstring, **kwargs)
        return self._future

    def __await__(self):
        return self._start().__await__()

    async def __aenter__(self):
        self._cnxn = await self._start()
        return self._cnxn

    async def __aexit__(self, exc_type, exc_value, traceback):
        # Delegates to Connection.__aexit__: commit on clean exit, rollback on
        # error, and - like the C++ Connection.__exit__ - does NOT close.
        return await self._cnxn.__aexit__(exc_type, exc_value, traceback)


def connect(connstring=None, /, **kwargs):
    """Open an ODBC connection.  Returns an awaitable that resolves to a
    Connection, also usable directly as an async context manager.

    Keyword arguments understood by pyodbc itself: autocommit, readonly, timeout,
    encoding, attrs_before, driver_completion.  All other keyword arguments are
    appended to the connection string as "key=value;" pairs (with the DB API
    aliases user->uid, password->pwd, host->server applied).
    """
    core_kwargs = {}
    parts = []
    for key, value in kwargs.items():
        if key == 'autocommit':
            core_kwargs['autocommit'] = bool(value)
        elif key == 'readonly':
            core_kwargs['readonly'] = bool(value)
        elif key == 'timeout':
            core_kwargs['timeout'] = int(value)
        elif key == 'encoding':
            if not isinstance(value, str):
                raise TypeError('encoding must be a string')
            core_kwargs['encoding'] = value
        elif key == 'attrs_before':
            if value is not None:
                raise NotImplementedError(
                    'attrs_before is not implemented in the Rust port yet '
                    '(phase 5 of docs/rust-asyncio-rewrite-plan.md)')
        elif key == 'driver_completion':
            value = int(value)
            if value not in _VALID_DRIVER_COMPLETION:
                raise ProgrammingError('Invalid value for driver_completion')  # noqa: F405
            if value == _SQL_DRIVER_PROMPT:
                raise NotSupportedError(  # noqa: F405
                    'SQL_DRIVER_PROMPT not supported on this platform')
            core_kwargs['driver_completion'] = value
        else:
            key = _KEYWORD_ALIASES.get(key, key)
            parts.append(f'{key}={value}')

    if parts:
        base = connstring or ''
        if base and not base.rstrip().endswith(';'):
            base += ';'
        connstring = base + ';'.join(parts) + ';'

    if not connstring:
        raise TypeError('no connection information was passed')

    return _PendingConnection((connstring, core_kwargs))


class _BinaryNullType:
    """The type of BinaryNull, a singleton passed as a parameter value to distinguish
    a binary NULL from a char NULL when the driver cannot describe parameters."""

    def __repr__(self):
        return 'BinaryNull'


BinaryNull = _BinaryNullType()
