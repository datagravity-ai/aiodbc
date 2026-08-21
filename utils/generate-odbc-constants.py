#!/usr/bin/env python3
"""Regenerate rust/constants.rs against the platform's ODBC headers.

rust/constants.rs is the source of truth for WHICH module-level integer constants
aiodbc exposes (the list originally came from the C++ implementation's MAKECONST
table).  This script re-reads the names from that file, re-evaluates each one
against the platform's ODBC headers (sql.h / sqlext.h, plus utils/dbspecific.h for
driver-specific values) by compiling a small C program, and rewrites the file - so
it verifies the checked-in values still match the headers (CI runs it and diffs).
To ADD a constant, append a ("NAME", 0) row to rust/constants.rs and rerun.

Requires a C compiler and the unixODBC development headers (unixodbc-dev).

Names that are not defined by the headers on this platform are skipped.  Run on
Linux with unixODBC, which is the set of values aiodbc has always shipped on
non-Windows platforms.
"""

import re
import subprocess
import sys
import tempfile
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
OUTPUT = REPO / 'rust' / 'constants.rs'

HEADER = '''\
// ODBC constants exposed as aiodbc module attributes.
//
// GENERATED FILE - do not edit by hand.  Regenerate with:
//     python utils/generate-odbc-constants.py
//
// The list of names mirrors the C++ implementation's MAKECONST table; the
// values come from the unixODBC headers (sql.h/sqlext.h) and utils/dbspecific.h.

pub const CONSTANTS: &[(&str, i64)] = &[
'''

FOOTER = '''];
'''


def extract_names() -> list[str]:
    text = OUTPUT.read_text()
    names = re.findall(r'\("(SQL_[A-Za-z0-9_]+)", -?\d+\)', text)
    if len(names) < 100:
        sys.exit(f'error: only found {len(names)} constants in {OUTPUT}')
    # preserve order, drop duplicates
    seen = set()
    return [n for n in names if not (n in seen or seen.add(n))]


def evaluate(names: list[str]) -> list[str]:
    lines = [
        '#include <stdio.h>',
        '#include <sql.h>',
        '#include <sqlext.h>',
        'typedef unsigned char byte;',
        '#define SQL_WMETADATA -888',  # pyodbc-specific, see rust/textenc.rs
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
        subprocess.run(['cc', '-I', str(REPO / 'utils'), '-o', str(exe), str(src)],
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
