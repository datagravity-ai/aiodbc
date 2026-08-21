# pyodbc

[![Ubuntu build](https://github.com/mkleehammer/pyodbc/actions/workflows/ubuntu_build.yml/badge.svg)](https://github.com/mkleehammer/pyodbc/actions/workflows/ubuntu_build.yml)
[![PyPI](https://img.shields.io/pypi/v/pyodbc?color=brightgreen)](https://pypi.org/project/pyodbc/)

pyodbc is an open source Python module that makes accessing ODBC databases simple.
It implements the [DB API 2.0](https://www.python.org/dev/peps/pep-0249)
specification with an **asyncio-native** API: connections, statement execution, and
fetching are awaitable, and each connection runs its ODBC calls on a dedicated
worker thread so the event loop is never blocked.

```python
import asyncio
import pyodbc

async def main():
    cnxn = await pyodbc.connect('DSN=mydsn')
    cursor = cnxn.cursor()
    await cursor.execute('select user_id, user_name from users')
    async for row in cursor:
        print(row.user_id, row.user_name)
    await cnxn.close()

asyncio.run(main())
```

As of version 6.0, pyodbc is implemented in Rust (it was previously a C++
extension, and previous major versions exposed a synchronous API).

The easiest way to install pyodbc is to use pip:

    python -m pip install pyodbc

On Macs, you should probably install unixODBC first if you don't already have an
ODBC driver manager installed.  For example, using the
[homebrew](https://brew.sh/) package manager:

    brew install unixodbc
    python -m pip install pyodbc

Similarly, on Unix you should make sure you have an ODBC driver manager installed
before installing pyodbc.  See the
[docs](https://github.com/mkleehammer/pyodbc/wiki/Install) for more information
about how to do this on different Unix flavors.  (On Windows, the ODBC driver
manager is built-in.)

Precompiled binary wheels are provided for multiple Python versions on most
Windows, macOS, and Linux platforms.  On other platforms pyodbc will be built from
the source code; you will need a Rust toolchain and the unixODBC headers when
building from source.  See HACKING.md for details.

[Documentation](https://github.com/mkleehammer/pyodbc/wiki)

[Release Notes](https://github.com/mkleehammer/pyodbc/releases)
