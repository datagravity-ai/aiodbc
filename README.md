# aiodbc

[![Ubuntu build](https://github.com/erickpeirson/pyodbc/actions/workflows/ubuntu_build.yml/badge.svg)](https://github.com/erickpeirson/pyodbc/actions/workflows/ubuntu_build.yml)
[![PyPI](https://img.shields.io/pypi/v/aiodbc?color=brightgreen)](https://pypi.org/project/aiodbc/)

aiodbc is an open source Python module that makes accessing ODBC databases simple.
It implements the [DB API 2.0](https://www.python.org/dev/peps/pep-0249)
specification with an **asyncio-native** API: connections, statement execution, and
fetching are awaitable, and each connection runs its ODBC calls on a dedicated
worker thread so the event loop is never blocked.

```python
import asyncio
import aiodbc

async def main():
    cnxn = await aiodbc.connect('DSN=mydsn')
    cursor = cnxn.cursor()
    await cursor.execute('select user_id, user_name from users')
    async for row in cursor:
        print(row.user_id, row.user_name)
    await cnxn.close()

asyncio.run(main())
```

aiodbc is the asyncio-native continuation of [pyodbc](https://github.com/mkleehammer/pyodbc),
implemented in Rust (pyodbc is a synchronous C++ extension; aiodbc keeps its DB API
surface, test suites, and ODBC behaviors, with awaitable methods).

The easiest way to install aiodbc is to use pip:

    python -m pip install aiodbc

On Macs, you should probably install unixODBC first if you don't already have an
ODBC driver manager installed.  For example, using the
[homebrew](https://brew.sh/) package manager:

    brew install unixodbc
    python -m pip install aiodbc

Similarly, on Unix you should make sure you have an ODBC driver manager installed
before installing aiodbc.  See the
[docs](https://github.com/mkleehammer/pyodbc/wiki/Install) for more information
about how to do this on different Unix flavors.  (On Windows, the ODBC driver
manager is built-in.)

Precompiled binary wheels are provided for multiple Python versions on most
Windows, macOS, and Linux platforms.  On other platforms aiodbc will be built from
the source code; you will need a Rust toolchain and the unixODBC headers when
building from source.  See HACKING.md for details.

[pyodbc Documentation](https://github.com/mkleehammer/pyodbc/wiki) (the DB API
surface is the same; aiodbc methods are awaited)

[Release Notes](https://github.com/erickpeirson/pyodbc/releases)
