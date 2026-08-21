#!/usr/bin/python
"""
Unit tests for SQLite

Download the SQLite ODBC driver from http://www.ch-werner.de/sqliteodbc

To use, set the connection parameter in the PYODBC_SQLITE shell variable, e.g.:

Windows:
set PYODBC_SQLITE=driver={SQLite3 ODBC Driver};Database=:memory:;

Unix/Mac:
export PYODBC_SQLITE=driver={SQLite3};Database=/tmp/test.db;

Then run the unit tests with:
python -m pytest tests/sqlite_test.py
"""

import ctypes
import os
import pathlib
import pickle
import platform
import re
from collections.abc import Iterator
from datetime import datetime

import pyodbc
import pytest


# the typical name of the SQLite driver on different platforms
DEFAULT_DRIVER = 'SQLite3 ODBC Driver' if platform.system() == 'Windows' else 'SQLite3'

_TESTSTR = '0123456789-abcdefghijklmnopqrstuvwxyz-'


def _generate_test_string(length):
    """
    Returns a string of `length` characters, constructed by repeating _TESTSTR as necessary.

    To enhance performance, there are 3 ways data is read, based on the length of the value, so most data types are
    tested with 3 lengths.  This function helps us generate the test data.

    We use a recognizable data set instead of a single character to make it less likely that "overlap" errors will
    be hidden and to help us manually identify where a break occurs.
    """
    if length <= len(_TESTSTR):
        return _TESTSTR[:length]

    c = (length + len(_TESTSTR) - 1) // len(_TESTSTR)
    v = _TESTSTR * c
    return v[:length]


SMALL_FENCEPOST_SIZES = [0, 1, 255, 256, 510, 511, 512, 1023, 1024, 2047, 2048, 4000]
LARGE_FENCEPOST_SIZES = [4095, 4096, 4097, 10 * 1024, 20 * 1024]

STR_FENCEPOSTS = [_generate_test_string(size) for size in SMALL_FENCEPOST_SIZES]
BYTE_FENCEPOSTS = [bytes(s, 'ascii') for s in STR_FENCEPOSTS]
IMAGE_FENCEPOSTS = BYTE_FENCEPOSTS + [bytes(_generate_test_string(size), 'ascii') for size in LARGE_FENCEPOST_SIZES]


@pytest.fixture
def connection_string(tmp_path: pathlib.Path):
    return os.environ.get('PYODBC_SQLITE', f'driver={DEFAULT_DRIVER};database={tmp_path}/test.db')


@pytest.fixture
async def cnxn(connection_string: str):
    c = await pyodbc.connect(connection_string, autocommit=False, attrs_before=None)
    yield c
    if not c.closed:
        await c.close()


@pytest.fixture
async def cursor(cnxn: pyodbc.Connection) -> Iterator[pyodbc.Cursor]:
    cur = cnxn.cursor()

    await cur.execute("drop table if exists t0")
    await cur.execute("drop table if exists t1")
    await cur.execute("drop table if exists t2")
    await cnxn.commit()

    return cur


async def test_multiple_bindings(cursor: pyodbc.Cursor):
    "More than one bind and select on a cursor"
    await cursor.execute("create table t1(n int)")
    await cursor.execute("insert into t1 values (?)", 1)
    await cursor.execute("insert into t1 values (?)", 2)
    await cursor.execute("insert into t1 values (?)", 3)
    for _ in range(3):
        await cursor.execute("select n from t1 where n < ?", 10)
        await cursor.execute("select n from t1 where n < 3")


async def test_different_bindings(cursor: pyodbc.Cursor):
    await cursor.execute("create table t1(n int)")
    await cursor.execute("create table t2(d datetime)")
    await cursor.execute("insert into t1 values (?)", 1)
    await cursor.execute("insert into t2 values (?)", datetime.now())  # noqa: DTZ005


def test_drivers():
    p = pyodbc.drivers()
    assert isinstance(p, list)


def test_datasources():
    p = pyodbc.dataSources()
    assert isinstance(p, dict)


async def test_getinfo_string(cnxn: pyodbc.Connection):
    value = await cnxn.getinfo(pyodbc.SQL_CATALOG_NAME_SEPARATOR)
    assert isinstance(value, str)


async def test_getinfo_bool(cnxn: pyodbc.Connection):
    value = await cnxn.getinfo(pyodbc.SQL_ACCESSIBLE_TABLES)
    assert isinstance(value, bool)


async def test_getinfo_int(cnxn: pyodbc.Connection):
    value = await cnxn.getinfo(pyodbc.SQL_DEFAULT_TXN_ISOLATION)
    assert isinstance(value, int)


async def test_getinfo_smallint(cnxn: pyodbc.Connection):
    value = await cnxn.getinfo(pyodbc.SQL_CONCAT_NULL_BEHAVIOR)
    assert isinstance(value, int)


async def _test_strtype(cursor: pyodbc.Cursor, sqltype, value, colsize=None):
    """
    The implementation for string, Unicode, and binary tests.
    """
    assert colsize is None or (value is None or colsize >= len(value))
    assert cursor.connection.readvar_initsize == 4096

    for initsize in [None, 1024 * 1024, 0]:
        if initsize is not None:
            cursor.connection.readvar_initsize = initsize
        if colsize:
            sql = "create table t1(s {}({}))".format(sqltype, colsize)
        else:
            sql = "create table t1(s {})".format(sqltype)

        await cursor.execute(sql)
        await cursor.execute("insert into t1 values(?)", value)
        v = (await (await cursor.execute("select * from t1")).fetchone())[0]
        assert type(v) is type(value)

        if value is not None:
            assert len(v) == len(value)

        assert v == value

        # Reported by Andy Hochhaus in the pyodbc group: In 2.1.7 and earlier, a hardcoded length of 255 was used to
        # determine whether a parameter was bound as a SQL_VARCHAR or SQL_LONGVARCHAR.  Apparently SQL Server chokes if
        # we bind as a SQL_LONGVARCHAR and the target column size is 8000 or less, which is considers just SQL_VARCHAR.
        # This means binding a 256 character value would cause problems if compared with a VARCHAR column under
        # 8001. We now use SQLGetTypeInfo to determine the time to switch.
        #
        # [42000] [Microsoft][SQL Server Native Client 10.0][SQL Server]The data types varchar and text are incompatible in the equal to operator.

        await cursor.execute("select * from t1 where s=?", value)
        await cursor.execute("drop table t1")


async def _test_strliketype(cursor: pyodbc.Cursor, sqltype, value, colsize=None):
    """
    The implementation for text, image, ntext, and binary.

    These types do not support comparison operators.
    """
    assert colsize is None or (value is None or colsize >= len(value))

    if colsize:
        sql = "create table t1(s {}({}))".format(sqltype, colsize)
    else:
        sql = "create table t1(s {})".format(sqltype)

    await cursor.execute(sql)
    await cursor.execute("insert into t1 values(?)", value)
    v = (await (await cursor.execute("select * from t1")).fetchone())[0]
    assert type(v) is type(value)

    if value is not None:
        assert len(v) == len(value)

    assert v == value


#
# text
#

async def test_text_null(cursor: pyodbc.Cursor):
    await _test_strtype(cursor, 'text', None, 100)


# Generate a test for each fencepost size: test_text_0, etc.
def _maketest(value):
    async def t(cursor: pyodbc.Cursor):
        await _test_strtype(cursor, 'text', value, len(value))
    return t

for value in STR_FENCEPOSTS:
    locals()['test_text_{}'.format(len(value))] = _maketest(value)


async def test_text_upperlatin(cursor: pyodbc.Cursor):
    await _test_strtype(cursor, 'varchar', 'á')


#
# blob
#

async def test_null_blob(cursor: pyodbc.Cursor):
    await _test_strtype(cursor, 'blob', None, 100)


async def test_large_null_blob(cursor: pyodbc.Cursor):
    # Bug 1575064
    await _test_strtype(cursor, 'blob', None, 4000)


# Generate a test for each fencepost size: test_unicode_0, etc.
def _maketest(value):
    async def t(cursor: pyodbc.Cursor):
        await _test_strtype(cursor, 'blob', value, len(value))
    return t

for value in BYTE_FENCEPOSTS:
    locals()['test_blob_{}'.format(len(value))] = _maketest(value)


async def test_subquery_params(cursor: pyodbc.Cursor):
    """Ensure parameter markers work in a subquery"""
    await cursor.execute("create table t1(id integer, s varchar(20))")
    await cursor.execute("insert into t1 values (?,?)", 1, 'test')
    row = await (await cursor.execute("""
                              select x.id
                              from (
                                select id
                                from t1
                                where s = ?
                                  and id between ? and ?
                               ) x
                               """, 'test', 1, 10)).fetchone()
    assert row is not None
    assert row[0] == 1


async def test_close_cnxn(cursor: pyodbc.Cursor, cnxn):
    """Make sure using a Cursor after closing its connection doesn't crash."""

    await cursor.execute("create table t1(id integer, s varchar(20))")
    await cursor.execute("insert into t1 values (?,?)", 1, 'test')
    await cursor.execute("select * from t1")

    await cnxn.close()

    # Now that the connection is closed, we expect an exception.  (If the code attempts to use
    # the HSTMT, we'll get an access violation instead.)
    sql = "select * from t1"
    with pytest.raises(pyodbc.ProgrammingError):
        await cursor.execute(sql)


async def test_negative_row_index(cursor: pyodbc.Cursor):
    await cursor.execute("create table t1(s varchar(20))")
    await cursor.execute("insert into t1 values(?)", "1")
    row = await (await cursor.execute("select * from t1")).fetchone()
    assert row[0] == "1"
    assert row[-1] == "1"


def test_version():
    assert len(pyodbc.version.split('.')) == 3  # 1.3.1 etc.


#
# ints and floats
#

async def test_int(cursor: pyodbc.Cursor):
    value = 1234
    await cursor.execute("create table t1(n int)")
    await cursor.execute("insert into t1 values (?)", value)
    result = (await (await cursor.execute("select n from t1")).fetchone())[0]
    assert result == value


async def test_negative_int(cursor: pyodbc.Cursor):
    value = -1
    await cursor.execute("create table t1(n int)")
    await cursor.execute("insert into t1 values (?)", value)
    result = (await (await cursor.execute("select n from t1")).fetchone())[0]
    assert result == value


async def test_bigint(cursor: pyodbc.Cursor):
    value = 3000000000
    await cursor.execute("create table t1(d bigint)")
    await cursor.execute("insert into t1 values (?)", value)
    result = (await (await cursor.execute("select d from t1")).fetchone())[0]
    assert result == value


async def test_negative_bigint(cursor: pyodbc.Cursor):
    # Issue 186: BIGINT problem on 32-bit architecture
    value = -430000000
    await cursor.execute("create table t1(d bigint)")
    await cursor.execute("insert into t1 values (?)", value)
    result = (await (await cursor.execute("select d from t1")).fetchone())[0]
    assert result == value


async def test_float(cursor: pyodbc.Cursor):
    value = 1234.567
    await cursor.execute("create table t1(n float)")
    await cursor.execute("insert into t1 values (?)", value)
    result = (await (await cursor.execute("select n from t1")).fetchone())[0]
    assert result == value


async def test_negative_float(cursor: pyodbc.Cursor):
    value = -200
    await cursor.execute("create table t1(n float)")
    await cursor.execute("insert into t1 values (?)", value)
    result = (await (await cursor.execute("select n from t1")).fetchone())[0]
    assert value == result


#
# rowcount
#

# Note: SQLRowCount does not define what the driver must return after a select statement
# and says that its value should not be relied upon.  The sqliteodbc driver is hardcoded to
# return 0 so I've deleted the test.

async def test_rowcount_delete(cursor: pyodbc.Cursor):
    assert cursor.rowcount == 0
    await cursor.execute("create table t1(i int)")
    count = 4
    for i in range(count):
        await cursor.execute("insert into t1 values (?)", i)
    await cursor.execute("delete from t1")
    assert cursor.rowcount == count


async def test_rowcount_nodata(cursor: pyodbc.Cursor):
    """
    This represents a different code path than a delete that deleted something.

    The return value is SQL_NO_DATA and code after it was causing an error.  We could use SQL_NO_DATA to step over
    the code that errors out and drop down to the same SQLRowCount code.  On the other hand, we could hardcode a
    zero return value.
    """
    await cursor.execute("create table t1(i int)")
    # This is a different code path internally.
    await cursor.execute("delete from t1")
    assert cursor.rowcount == 0


# In the 2.0.x branch, Cursor.execute sometimes returned the cursor and sometimes the rowcount.  This proved very
# confusing when things went wrong and added very little value even when things went right since users could always
# use: cursor.execute("...").rowcount

async def test_retcursor_delete(cursor: pyodbc.Cursor):
    await cursor.execute("create table t1(i int)")
    await cursor.execute("insert into t1 values (1)")
    v = await cursor.execute("delete from t1")
    assert v == cursor


async def test_retcursor_nodata(cursor: pyodbc.Cursor):
    """
    This represents a different code path than a delete that deleted something.

    The return value is SQL_NO_DATA and code after it was causing an error.  We could use SQL_NO_DATA to step over
    the code that errors out and drop down to the same SQLRowCount code.
    """
    await cursor.execute("create table t1(i int)")
    # This is a different code path internally.
    v = await cursor.execute("delete from t1")
    assert v == cursor


async def test_retcursor_select(cursor: pyodbc.Cursor):
    await cursor.execute("create table t1(i int)")
    await cursor.execute("insert into t1 values (1)")
    v = await cursor.execute("select * from t1")
    assert v == cursor


#
# misc
#

async def test_lower_case(cnxn: pyodbc.Connection):
    "Ensure pyodbc.lowercase forces returned column names to lowercase."

    # Has to be set before creating the cursor, so we must recreate cursor.

    pyodbc.lowercase = True
    cursor = cnxn.cursor()

    await cursor.execute("create table t1(Abc int, dEf int)")
    await cursor.execute("select * from t1")

    names = [t[0] for t in cursor.description]
    names.sort()

    assert names == ["abc", "def"]

    # Put it back so other tests don't fail.
    pyodbc.lowercase = False


async def test_row_description(cnxn: pyodbc.Connection):
    """
    Ensure Cursor.description is accessible as Row.cursor_description.
    """
    cursor = cnxn.cursor()
    await cursor.execute("create table t1(a int, b char(3))")
    await cnxn.commit()
    await cursor.execute("insert into t1 values(1, 'abc')")

    row = await (await cursor.execute("select * from t1")).fetchone()

    assert cursor.description == row.cursor_description


async def test_executemany(cursor: pyodbc.Cursor):
    await cursor.execute("create table t1(a int, b varchar(10))")

    params = [(i, str(i)) for i in range(1, 6)]

    await cursor.executemany("insert into t1(a, b) values (?,?)", params)

    count = (await (await cursor.execute("select count(*) from t1")).fetchone())[0]
    assert count == len(params)

    await cursor.execute("select a, b from t1 order by a")
    rows = await cursor.fetchall()
    assert count == len(rows)

    for param, row in zip(params, rows):
        assert param[0] == row[0]
        assert param[1] == row[1]


async def test_executemany_one(cursor: pyodbc.Cursor):
    "Pass executemany a single sequence"
    await cursor.execute("create table t1(a int, b varchar(10))")

    params = [(1, "test")]

    await cursor.executemany("insert into t1(a, b) values (?,?)", params)

    count = (await (await cursor.execute("select count(*) from t1")).fetchone())[0]
    assert count == len(params)

    await cursor.execute("select a, b from t1 order by a")
    rows = await cursor.fetchall()
    assert count == len(rows)

    for param, row in zip(params, rows):
        assert param[0] == row[0]
        assert param[1] == row[1]


async def test_executemany_failure(cursor: pyodbc.Cursor):
    """
    Ensure that an exception is raised if one query in an executemany fails.
    """
    await cursor.execute("create table t1(a int, b varchar(10))")

    params = [
        (1, 'good'),
        ('error', 'not an int'),
        (3, 'good'),
    ]

    with pytest.raises(pyodbc.Error):
        await cursor.executemany("insert into t1(a, b) value (?, ?)", params)


async def test_row_slicing(cursor: pyodbc.Cursor):
    await cursor.execute("create table t1(a int, b int, c int, d int)")
    await cursor.execute("insert into t1 values(1,2,3,4)")

    row = await (await cursor.execute("select * from t1")).fetchone()

    result = row[:]
    assert result is row

    result = row[:-1]
    assert result == (1, 2, 3)

    result = row[0:4]
    assert result is row


async def test_row_repr(cursor: pyodbc.Cursor):
    await cursor.execute("create table t1(a int, b int, c int, d int)")
    await cursor.execute("insert into t1 values(1,2,3,4)")

    row = await (await cursor.execute("select * from t1")).fetchone()

    result = str(row)
    assert result == "(1, 2, 3, 4)"

    result = str(row[:-1])
    assert result == "(1, 2, 3)"

    result = str(row[:1])
    assert result == "(1,)"


async def test_view_select(cursor: pyodbc.Cursor):
    # Reported in forum: Can't select from a view?  I think I do this a lot, but another test never hurts.

    # Create a table (t1) with 3 rows and a view (t2) into it.
    await cursor.execute("create table t1(c1 int identity(1, 1), c2 varchar(50))")
    for i in range(3):
        await cursor.execute("insert into t1(c2) values (?)", f"string{i}")
    await cursor.execute("create view t2 as select * from t1")

    # Select from the view
    await cursor.execute("select * from t2")
    rows = await cursor.fetchall()
    assert rows is not None
    assert len(rows) == 3


async def test_autocommit(cnxn: pyodbc.Connection, connection_string: str):
    assert cnxn.autocommit is False  # PEP249, the default should be False

    othercnxn = await pyodbc.connect(connection_string, autocommit=True)
    assert othercnxn.autocommit is True

    othercnxn.autocommit = False
    assert othercnxn.autocommit is False


async def test_skip(cursor: pyodbc.Cursor):
    # Insert 1, 2, and 3.  Fetch 1, skip 2, fetch 3.

    await cursor.execute("create table t1(id int)")
    for i in range(1, 5):
        await cursor.execute("insert into t1 values(?)", i)
    await cursor.execute("select id from t1 order by id")
    assert (await cursor.fetchone())[0] == 1
    await cursor.skip(2)
    assert (await cursor.fetchone())[0] == 4


async def test_sets_execute(cursor: pyodbc.Cursor):
    # Only lists and tuples are allowed.
    await cursor.execute("create table t1 (word varchar (100))")
    words = {'a'}
    with pytest.raises(pyodbc.ProgrammingError):
        await cursor.execute("insert into t1 (word) VALUES (?)", [words])


async def test_sets_executemany(cursor: pyodbc.Cursor):
    # Only lists and tuples are allowed.
    await cursor.execute("create table t1 (word varchar (100))")
    words = {'a'}
    with pytest.raises(TypeError):
        await cursor.executemany("insert into t1 (word) values (?)", [words])


async def test_row_execute(cursor: pyodbc.Cursor):
    "Ensure we can use a Row object as a parameter to execute"
    await cursor.execute("create table t1(n int, s varchar(10))")
    await cursor.execute("insert into t1 values (1, 'a')")
    row = await (await cursor.execute("select n, s from t1")).fetchone()
    assert row is not None

    await cursor.execute("create table t2(n int, s varchar(10))")
    await cursor.execute("insert into t2 values (?, ?)", row)


async def test_row_executemany(cursor: pyodbc.Cursor):
    "Ensure we can use a Row object as a parameter to executemany"
    await cursor.execute("create table t1(n int, s varchar(10))")

    for i in range(3):
        await cursor.execute("insert into t1 values (?, ?)", i, chr(ord('a') + i))

    rows = await (await cursor.execute("select n, s from t1")).fetchall()
    assert len(rows) != 0

    await cursor.execute("create table t2(n int, s varchar(10))")
    await cursor.executemany("insert into t2 values (?, ?)", rows)


async def test_description(cursor: pyodbc.Cursor):
    "Ensure cursor.description is correct"

    await cursor.execute("create table t1(n int, s text)")
    await cursor.execute("insert into t1 values (1, 'abc')")
    await cursor.execute("select * from t1")

    # (I'm not sure the precision of an int is constant across different versions, bits, so I'm hand checking the
    # items I do know.

    # int
    t = cursor.description[0]
    assert t[0] == 'n'
    assert t[1] is int
    assert t[5] == 0       # scale
    assert t[6] is True    # nullable

    # text
    t = cursor.description[1]
    assert t[0] == 's'
    assert t[1] is str
    assert t[5] == 0       # scale
    assert t[6] is True    # nullable


async def test_row_equal(cursor: pyodbc.Cursor):
    await cursor.execute("create table t1(n int, s varchar(20))")
    await cursor.execute("insert into t1 values (1, 'test')")
    row1 = await (await cursor.execute("select n, s from t1")).fetchone()
    row2 = await (await cursor.execute("select n, s from t1")).fetchone()
    b = (row1 == row2)
    assert b is True


async def test_row_gtlt(cursor: pyodbc.Cursor):
    await cursor.execute("create table t1(n int, s varchar(20))")
    await cursor.execute("insert into t1 values (1, 'test1')")
    await cursor.execute("insert into t1 values (1, 'test2')")
    rows = await (await cursor.execute("select n, s from t1 order by s")).fetchall()
    assert rows[0] < rows[1]
    assert rows[0] <= rows[1]
    assert rows[1] > rows[0]
    assert rows[1] >= rows[0]
    assert rows[0] != rows[1]

    rows = list(rows)
    rows.sort()  # uses <


async def _test_context_manager(connection_string):
    # TODO: This is failing, but it may be due to the design of sqlite.  I've disabled it
    # for now until I can research it some more.

    # WARNING: This isn't working right now.  We've set the driver's autocommit to "off",
    # but that doesn't automatically start a transaction.  I'm not familiar enough with the
    # internals of the driver to tell what is going on, but it looks like there is support
    # for the autocommit flag.
    #
    # I thought it might be a timing issue, like it not actually starting a txn until you
    # try to do something, but that doesn't seem to work either.  I'll leave this in to
    # remind us that it isn't working yet but we need to contact the SQLite ODBC driver
    # author for some guidance.

    async with pyodbc.connect(connection_string) as cnxn:
        cursor = cnxn.cursor()
        await cursor.execute("begin")
        await cursor.execute("create table t1(i int)")
        await cursor.execute('rollback')

    # The connection should be closed now.
    with pytest.raises(pyodbc.Error):
        await cnxn.execute('rollback')


async def test_untyped_none(cursor: pyodbc.Cursor):
    # From issue 129
    value = (await (await cursor.execute("select ?", None)).fetchone())[0]
    assert value is None


async def test_large_update_nodata(cursor: pyodbc.Cursor):
    await cursor.execute('create table t1(a blob)')
    hundredkb = 'x' * 100 * 1024
    await cursor.execute('update t1 set a=? where 1=0', (hundredkb,))


async def test_no_fetch(cursor: pyodbc.Cursor):
    # Issue 89 with FreeTDS: Multiple selects (or catalog functions that issue selects) without fetches seem to
    # confuse the driver.
    await cursor.execute('select 1')
    await cursor.execute('select 1')
    await cursor.execute('select 1')


async def test_connect_dict_only():
    # get the name of the ODBC driver used in these tests
    conn_str = os.environ.get('PYODBC_SQLITE')
    if conn_str:
        match = re.search(r'(^|;)driver={?([A-Z0-9 ()_.-]+)}?(;|$)', conn_str, flags=re.IGNORECASE)
        driver = match.group(2)
    else:
        driver = DEFAULT_DRIVER

    c = await pyodbc.connect(driver=driver, database=':memory:')
    await c.close()


async def test_pickling(cnxn: pyodbc.Connection):
    crsr = cnxn.cursor()
    await crsr.execute("create table t1(n int, s varchar(20))")
    await crsr.execute("insert into t1 values (1, 'test1')")
    await crsr.execute("insert into t1 values (2, 'test2')")
    await cnxn.commit()
    original_rows = await (await crsr.execute("select n, s from t1")).fetchall()

    # connections cannot be pickled
    with pytest.raises(TypeError, match=r"cannot pickle"):
        pickle.dumps(cnxn)

    # cursors cannot be pickled
    with pytest.raises(TypeError, match=r"cannot pickle"):
        pickle.dumps(crsr)

    # rows can be pickled
    pickled_rows = pickle.dumps(original_rows)
    unpickled_rows = pickle.loads(pickled_rows)
    assert unpickled_rows == original_rows

    # pickling works for rows with duplicate column names
    original_rows = await (await crsr.execute("select n, s, s from t1")).fetchall()
    pickled_rows = pickle.dumps(original_rows)
    unpickled_rows = pickle.loads(pickled_rows)
    assert unpickled_rows == original_rows


async def test_handles(cursor: pyodbc.Cursor):
    """Test the exposed native ODBC handles"""

    conn = cursor.connection
    for handle in (pyodbc.henv, conn.hdbc, cursor.hstmt):
        assert isinstance(handle, ctypes.c_void_p)
        with pytest.raises(TypeError):
            if handle > 42:
                print("we should never get here")
    await cursor.close()
    assert not isinstance(cursor.hstmt, ctypes.c_void_p)
    assert cursor.hstmt is None
    assert isinstance(conn.hdbc, ctypes.c_void_p)
    await conn.close()
    assert not isinstance(conn.hdbc, ctypes.c_void_p)
    assert conn.hdbc is None


async def test_rows_as_dicts(cursor: pyodbc.Cursor):
    """Test enhancement for ticket #171"""

    # Create and populate a test table.
    await cursor.execute("create table t1 (id int, name varchar(20))")
    await cursor.execute("insert into t1 values (42, 'Kathleen')")

    # Verify the default behavior
    assert cursor.rows_as_dicts is False
    row = await (await cursor.execute("select * from t1")).fetchone()
    assert not isinstance(row, dict)
    assert isinstance(row, pyodbc.Row)
    assert isinstance(row[0], int)
    assert isinstance(row[1], str)
    assert len(row) == 2
    with pytest.raises(TypeError, match="row indices must be integers"):
        print(row["name"])

    # Test the dict option
    cursor.rows_as_dicts = True
    row = await (await cursor.execute("select * from t1")).fetchone()
    assert not isinstance(row, pyodbc.Row)
    assert isinstance(row, dict)
    assert row == {"id": 42, "name": "Kathleen"}
    assert isinstance(row["id"], int)
    assert isinstance(row["name"], str)
    assert len(row) == 2
    with pytest.raises(KeyError):
        print(row[1])

    # Test aliasing
    row = await (await cursor.execute("select name as n1, name as n2 from t1")).fetchone()
    assert len(row) == 2
    assert row == {"n1": "Kathleen", "n2": "Kathleen"}
    with pytest.raises(KeyError):
        print(row["name"])

    # Test with a duplicate name
    row = await (await cursor.execute("select name, name from t1")).fetchone()
    assert len(row) == 1
    assert row == {"name": "Kathleen"}
