#!/usr/bin/env python3
"""Clone the selected Pagila table shapes and internal PK/FKs into staging."""

import argparse
import os
import re
import subprocess
from pathlib import Path


CONTAINER = "pagila"
GSQL = "gsql-pagila"
TABLES = ("customer", "rental", "payment")

# synth skips temporal columns (fit_marginal has no datetime model), so a
# NOT NULL temporal column without a DEFAULT would make every generated
# INSERT fail; P2 metrics never evaluate temporal columns.
TEMPORAL_TYPE = re.compile(r"(?i)^(timestamp|date|time)")

COLUMNS_SQL = """
SELECT c.table_name, c.column_name, format_type(a.atttypid, a.atttypmod),
       CASE WHEN c.is_nullable = 'NO' THEN 'NO' ELSE 'YES' END,
       COALESCE(c.column_default, '')
FROM information_schema.columns c
JOIN pg_catalog.pg_namespace n ON n.nspname = c.table_schema
JOIN pg_catalog.pg_class t ON t.relnamespace = n.oid AND t.relname = c.table_name
JOIN pg_catalog.pg_attribute a ON a.attrelid = t.oid
  AND a.attname = c.column_name AND a.attnum > 0 AND NOT a.attisdropped
WHERE c.table_schema = '{schema}'
  AND c.table_name IN ('customer', 'rental', 'payment')
ORDER BY CASE c.table_name WHEN 'customer' THEN 1 WHEN 'rental' THEN 2 ELSE 3 END,
         c.ordinal_position;
"""

CONSTRAINTS_SQL = """
SELECT child.relname, con.conname, con.contype, pg_get_constraintdef(con.oid, true)
FROM pg_catalog.pg_constraint con
JOIN pg_catalog.pg_class child ON child.oid = con.conrelid
JOIN pg_catalog.pg_namespace ns ON ns.oid = child.relnamespace
LEFT JOIN pg_catalog.pg_class parent ON parent.oid = con.confrelid
LEFT JOIN pg_catalog.pg_namespace pns ON pns.oid = parent.relnamespace
WHERE ns.nspname = '{schema}'
  AND child.relname IN ('customer', 'rental', 'payment')
  AND (con.contype = 'p' OR
       (con.contype = 'f' AND pns.nspname = '{schema}'
        AND parent.relname IN ('customer', 'rental', 'payment')))
ORDER BY CASE con.contype WHEN 'p' THEN 1 ELSE 2 END,
         CASE child.relname WHEN 'customer' THEN 1 WHEN 'rental' THEN 2 ELSE 3 END,
         con.conname;
"""


def quote_ident(value: str) -> str:
    return '"' + value.replace('"', '""') + '"'


def query_rows(sql: str):
    result = subprocess.run(
        ["docker", "exec", CONTAINER, GSQL, "-t", "-A", "-F", "\t", "-c", sql],
        check=True,
        capture_output=True,
        text=True,
    )
    return [line.split("\t") for line in result.stdout.splitlines() if line.strip()]


def staging_constraint(definition: str, source_schema: str) -> str:
    pattern = rf"(?i)REFERENCES\s+(?:(?:{re.escape(source_schema)}|{re.escape(quote_ident(source_schema))})\.)?"
    return re.sub(pattern, 'REFERENCES "staging".', definition)


def build_ddl(schema: str) -> str:
    columns = query_rows(COLUMNS_SQL.format(schema=schema.replace("'", "''")))
    by_table = {table: [] for table in TABLES}
    temporal_columns = {table: set() for table in TABLES}
    for table, column, type_name, nullable, default in columns:
        not_null = nullable == "NO"
        if not_null and not default and TEMPORAL_TYPE.match(type_name):
            not_null = False
        if TEMPORAL_TYPE.match(type_name):
            temporal_columns[table].add(column)
        default_sql = f" DEFAULT {default}" if default else ""
        by_table[table].append(
            f"  {quote_ident(column)} {type_name}{default_sql}"
            + (" NOT NULL" if not_null else "")
        )
    missing = [table for table, definitions in by_table.items() if not definitions]
    if missing:
        raise RuntimeError(f"source schema has no columns for: {', '.join(missing)}")

    statements = ['DROP SCHEMA IF EXISTS "staging" CASCADE;', 'CREATE SCHEMA "staging";']
    for table in TABLES:
        statements.append(
            f'CREATE TABLE "staging".{quote_ident(table)} (\n'
            + ",\n".join(by_table[table])
            + "\n);"
        )

    constraints = query_rows(CONSTRAINTS_SQL.format(schema=schema.replace("'", "''")))
    for table, name, kind, definition in constraints:
        if kind == "p" and temporal_columns[table] & set(re.findall(r"[A-Za-z_][A-Za-z0-9_]*", definition)):
            # Postgres PK columns are implicitly NOT NULL — a PK covering a
            # temporal column would silently undo the relaxation above.
            print(f"skipped PK covering temporal column(s): {table}.{name}")
            continue
        statements.append(
            f'ALTER TABLE "staging".{quote_ident(table)} ADD CONSTRAINT '
            f"{quote_ident(name)} {staging_constraint(definition, schema)};"
        )
    return "\n\n".join(statements) + "\n"


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--schema", default=os.environ.get("P2_SCHEMA", "public"))
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    ddl = build_ddl(args.schema)
    if args.output:
        args.output.write_text(ddl, encoding="utf-8")
    subprocess.run(
        ["docker", "exec", "-i", CONTAINER, GSQL],
        input=ddl,
        check=True,
        text=True,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
