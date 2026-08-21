#!/usr/bin/env python3
"""Regenerate rust/constants.rs from the C++ module's constants table.

During the C++ -> Rust transition, the authoritative list of module-level integer
constants is the MAKECONST table in src/pyodbcmodule.cpp.  This script extracts the
names from that table, evaluates each one against the platform's ODBC headers
(sql.h / sqlext.h, plus src/dbspecific.h for driver-specific values) by compiling a
small C program, and writes the resulting name/value pairs to rust/constants.rs.

Requires a C compiler and the unixODBC development headers (unixodbc-dev).

Names that are not defined by the headers on this platform are skipped, exactly as
the C++ preprocessor would skip them.  Run on Linux with unixODBC, which is the set
of values pyodbc has always shipped on non-Windows platforms.

Once the C++ sources are removed, rust/constants.rs becomes the source of truth
and this script can be retired.
"""

import re
import subprocess
import sys
import tempfile
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
CPP_MODULE = REPO / 'src' / 'pyodbcmodule.cpp'
OUTPUT = REPO / 'rust' / 'constants.rs'

HEADER = '''\
// ODBC constants exposed as pyodbc module attributes.
//
// GENERATED FILE - do not edit by hand.  Regenerate with:
//     python utils/generate-odbc-constants.py
//
// The list of names mirrors the MAKECONST table in src/pyodbcmodule.cpp; the
// values come from the unixODBC headers (sql.h/sqlext.h) and src/dbspecific.h.

pub const CONSTANTS: &[(&str, i64)] = &[
'''

FOOTER = '''];
'''


def extract_names() -> list[str]:
    text = CPP_MODULE.read_text()
    # \b keeps the "#define MAKECONST(v)" definition itself out of the results
    names = re.findall(r'\bMAKECONST\((SQL_[A-Za-z0-9_]+)\)', text)
    if len(names) < 100:
        sys.exit(f'error: only found {len(names)} MAKECONST entries in {CPP_MODULE}')
    # preserve order, drop duplicates
    seen = set()
    return [n for n in names if not (n in seen or seen.add(n))]


def evaluate(names: list[str]) -> list[str]:
    lines = [
        '#include <stdio.h>',
        '#include <sql.h>',
        '#include <sqlext.h>',
        'typedef unsigned char byte;',
        '#define SQL_WMETADATA -888',  # from src/pyodbcmodule.h
        '#include "dbspecific.h"',
        'int main(void) {',
    ]
    for n in names:
        # Guard each name so platform-specific gaps are skipped, not fatal.
        lines.append(f'#ifdef {n}')
        lines.append(f'    printf("    (\\"{n}\\", %ld),\\n", (long){n});')
        lines.append('#endif')
    lines += ['    return 0;', '}', '']

    with tempfile.TemporaryDirectory() as tmp:
        src = Path(tmp) / 'gen.c'
        exe = Path(tmp) / 'gen'
        src.write_text('\n'.join(lines))
        subprocess.run(['cc', '-I', str(REPO / 'src'), '-o', str(exe), str(src)],
                       check=True)
        out = subprocess.run([str(exe)], check=True, capture_output=True, text=True)
    return out.stdout.splitlines()


def main() -> None:
    names = extract_names()
    rows = evaluate(names)
    emitted = {re.match(r'\s*\("([A-Za-z0-9_]+)"', row).group(1) for row in rows}
    skipped = [n for n in names if n not in emitted]
    OUTPUT.write_text(HEADER + '\n'.join(rows) + '\n' + FOOTER)
    print(f'wrote {len(rows)} constants to {OUTPUT}')
    if skipped:
        print(f'skipped (not defined by headers): {", ".join(skipped)}')


if __name__ == '__main__':
    main()
