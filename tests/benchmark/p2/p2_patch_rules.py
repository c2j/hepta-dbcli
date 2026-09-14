#!/usr/bin/env python3
"""Add Pagila row counts and the optional D4 Zipf retry to drafted rules."""

import argparse
from pathlib import Path

import yaml


ROW_COUNTS = {"customer": 599, "rental": 16044, "payment": 16049}
CHILD_TABLES = {"rental", "payment"}
IN_SCOPE_TABLES = frozenset(ROW_COUNTS)


def relationship_target(relationship) -> str:
    references = relationship.get("references") or [""]
    return str(references[0]).split(".", 1)[0]


def drop_out_of_scope_relationships(document) -> list:
    """Drop FK edges whose parent table is outside the 3-table benchmark graph
    (e.g. pagila customer.address_id -> address); the generator rejects
    relationships pointing at tables with no model, and staging DDL already
    skips those constraints (see p2_make_staging.CONSTRAINTS_SQL)."""
    dropped = []
    for table in document.get("tables", []):
        kept = []
        for relationship in table.get("relationships", []):
            target = relationship_target(relationship)
            if target in IN_SCOPE_TABLES:
                kept.append(relationship)
            else:
                dropped.append(f"{table.get('name')}.{relationship.get('pk')} -> {target}")
        table["relationships"] = kept
    return dropped


class TaggedMapping(dict):
    """A YAML mapping that retains serde_yaml's enum tag."""

    def __init__(self, tag, value):
        super().__init__(value)
        self.tag = tag


class RulesLoader(yaml.SafeLoader):
    pass


class RulesDumper(yaml.SafeDumper):
    pass


def _load_tagged(loader, tag_suffix, node):
    return TaggedMapping("!" + tag_suffix, loader.construct_mapping(node, deep=True))


def _dump_tagged(dumper, value):
    return dumper.represent_mapping(value.tag, value)


RulesLoader.add_multi_constructor("!", _load_tagged)
RulesDumper.add_representer(TaggedMapping, _dump_tagged)


def parse_row_overrides(pairs):
    counts = dict(ROW_COUNTS)
    for pair in pairs or []:
        name, separator, value = pair.partition("=")
        if name not in IN_SCOPE_TABLES or not separator or not value.isdigit():
            raise ValueError(f"bad --rows override (want table=count): {pair!r}")
        counts[name] = int(value)
    return counts


def patch_rules(path: Path, zipf: bool = False, row_counts=None) -> None:
    document = yaml.load(path.read_text(encoding="utf-8"), Loader=RulesLoader)
    tables = document.get("tables", [])
    counts = dict(row_counts) if row_counts else dict(ROW_COUNTS)
    names = {table.get("name") for table in tables}
    missing = set(counts) - names
    if missing:
        raise ValueError(f"drafted rules missing tables: {', '.join(sorted(missing))}")

    dropped = drop_out_of_scope_relationships(document)
    for edge in dropped:
        print(f"dropped out-of-scope relationship: {edge}")

    for table in tables:
        name = table["name"]
        if name in counts:
            table["rows"] = counts[name]
            table["strategy"] = "zipf" if zipf and name in CHILD_TABLES else "uniform"

    path.write_text(
        yaml.dump(document, Dumper=RulesDumper, sort_keys=False), encoding="utf-8"
    )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("rules", type=Path)
    parser.add_argument("--zipf", action="store_true")
    parser.add_argument(
        "--rows",
        action="append",
        metavar="TABLE=COUNT",
        help="override the per-table row count (repeatable); "
        "defaults to the pagila fixture constants",
    )
    args = parser.parse_args()
    patch_rules(args.rules, zipf=args.zipf, row_counts=parse_row_overrides(args.rows))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
