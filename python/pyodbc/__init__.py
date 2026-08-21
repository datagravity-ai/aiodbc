"""
pyodbc - an asyncio-native DB API 2.0 module for ODBC, implemented in Rust.

This package is being ported from the C++ implementation in src/ per
docs/rust-asyncio-rewrite-plan.md.  The compiled Rust core is pyodbc._core; this
module assembles the public API on top of it.
"""

import datetime as _datetime
from importlib.metadata import PackageNotFoundError as _PackageNotFoundError
from importlib.metadata import version as _dist_version

# ODBC constants, the exception hierarchy, drivers() and data_sources() all come
# from the Rust core.
from pyodbc._core import *  # noqa: F401,F403
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


class _BinaryNullType:
    """The type of BinaryNull, a singleton passed as a parameter value to distinguish
    a binary NULL from a char NULL when the driver cannot describe parameters."""

    def __repr__(self):
        return 'BinaryNull'


BinaryNull = _BinaryNullType()
