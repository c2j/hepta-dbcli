//! Load plan building (issue #98): which files load into which tables, in
//! which order, with which columns.
//!
//! Everything here is pure and testable without a database: file discovery
//! reads the data directory, and the load order comes from mock FK rows that
//! mirror the `Dialect::foreign_keys_sql` row contract
//! `[schema_name, table_name, column_name, referenced_schema, referenced_table,
//! referenced_column, constraint_name]`.

use crate::backend::QueryResult;
use crate::tabular::GeneratedTable;
use std::path::{Path, PathBuf};

// ─── Plan data model (consumed by loader::execute in a later wave) ──────

/// One file that will be loaded into one existing table.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TableFile {
    pub table: String,
    pub columns: Vec<String>,
    pub row_count: usize,
    pub source_path: PathBuf,
}

/// One planned load step: a file loaded into a schema-qualified table.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PlanEntry {
    pub table: String,
    pub schema: Option<String>,
    pub path: PathBuf,
    pub columns: Vec<String>,
    pub row_count: usize,
}

/// The full load plan: entries in load order.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LoadPlan {
    pub entries: Vec<PlanEntry>,
}

// ─── File discovery ─────────────────────────────────────────────────────

/// Read one `<dir>/<table>.<ext>` file, dispatching on an explicit format.
/// The format string comes from `--format` (`auto`|`jsonl`|`json`|`csv`);
/// strict modes must find the file in exactly that format, `auto` falls back
/// to the shared discovery order (jsonl → json → csv).
pub(crate) fn read_table_file(
    dir: &Path,
    table: &str,
    format: &str,
) -> Result<Option<(PathBuf, GeneratedTable)>, String> {
    match format {
        "auto" => {
            // Discovery order jsonl -> json -> csv; the caller needs the path
            // that was actually read (the executor re-reads entry.path).
            let candidates = [
                (dir.join(format!("{table}.jsonl")), "jsonl"),
                (dir.join(format!("{table}.json")), "json"),
                (dir.join(format!("{table}.csv")), "csv"),
            ];
            for (path, ext) in candidates {
                if !path.is_file() {
                    continue;
                }
                let reader = match ext {
                    "jsonl" => crate::tabular::read_jsonl,
                    "json" => crate::tabular::read_json,
                    _ => crate::tabular::read_csv,
                };
                return reader(&path).map(|t| Some((path, t)));
            }
            Ok(None)
        }
        ext => {
            let path = dir.join(format!("{table}.{ext}"));
            if !path.is_file() {
                return Err(format!(
                    "missing data file {} (--format {ext} is strict: the file must exist in exactly that format)",
                    path.display()
                ));
            }
            let reader = match ext {
                "jsonl" => crate::tabular::read_jsonl,
                "json" => crate::tabular::read_json,
                "csv" => crate::tabular::read_csv,
                other => {
                    return Err(format!(
                        "unsupported --format '{other}' (expected auto|jsonl|json|csv)"
                    ))
                }
            };
            reader(&path).map(|t| Some((path, t)))
        }
    }
}

// ─── Plan building ──────────────────────────────────────────────────────

/// Discover the files that match `tables` (or all files found) and read their
/// columns and row counts. Pure apart from reading the data directory.
pub(crate) fn discover_table_files(
    dir: &Path,
    tables: Option<&[String]>,
    format: &str,
) -> Result<Vec<TableFile>, String> {
    let table_names: Vec<String> = match tables {
        Some(names) => names.to_vec(),
        None => scan_table_names(dir)?,
    };
    let mut files = Vec::new();
    for table in table_names {
        if let Some((path, (columns, rows))) = read_table_file(dir, &table, format)? {
            files.push(TableFile {
                row_count: rows.len(),
                source_path: path,
                table,
                columns,
            });
        }
    }
    Ok(files)
}

/// Database table names visible for the schema, from a `list_tables()` result
/// (`[schema_name, table_name, ...]`). Tables that have no data file are
/// skipped with a stderr note, not an error.
pub(crate) fn parse_db_tables(result: &QueryResult) -> Vec<String> {
    result
        .rows
        .iter()
        .filter_map(|row| row.get(1).and_then(|v| v.as_str()).map(str::to_string))
        .collect()
}

/// Table → schema map from a `list_tables()` result, used to qualify
/// per-table column lookups when `--schema` is not given.
pub(crate) fn parse_table_schemas(
    result: &QueryResult,
) -> std::collections::HashMap<String, Option<String>> {
    result
        .rows
        .iter()
        .filter_map(|row| {
            let table = row.get(1)?.as_str()?.to_string();
            let schema = row.first().and_then(|v| v.as_str()).map(str::to_string);
            Some((table, schema))
        })
        .collect()
}

/// Schema to run `foreign_keys_sql` against. Precedence: explicit `--schema`,
/// then the connection's configured default schema, then the schema the
/// planned tables were actually listed under. Only schemas that hold one of
/// the planned tables qualify for that last step — a server hosting many
/// schemas must not let an unrelated same-named table's schema win (the FK
/// query would return edges for foreign tables and the topological order
/// would degenerate to alphabetical). Ties resolve alphabetically, so the
/// choice is deterministic.
pub(crate) fn schema_for_fk_lookup(
    explicit: Option<String>,
    target_default: Option<String>,
    listed_schemas: &std::collections::HashMap<String, Option<String>>,
    planned_tables: &[String],
) -> Option<String> {
    if let Some(schema) = explicit.or(target_default) {
        return Some(schema);
    }
    let mut candidates: Vec<String> = planned_tables
        .iter()
        .filter_map(|t| listed_schemas.get(t).cloned().flatten())
        .filter(|s| !s.is_empty())
        .collect();
    candidates.sort();
    candidates.dedup();
    candidates.into_iter().next()
}

/// One column of an existing table, from a `table_columns()` result. Only the
/// facts load needs: the name, whether it can hold NULL, and whether it has a
/// default (issue #113 B).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DbColumn {
    pub name: String,
    pub nullable: bool,
    pub has_default: bool,
}

/// Columns from a `table_columns()` result. The `nullable` and
/// `default_value` positions are located by column name (MySQL and GaussDB
/// return them in different orders), never by a hardcoded index.
///
/// `nullable` parses from a JSON bool or the string flags `true`/`t`/`1`;
/// anything else is conservatively treated as NOT NULL. `default_value` is a
/// default unless it is a database NULL, the literal string `NULL`, or empty
/// (GaussDB returns `NULL`/`''` when a column has no default).
pub(crate) fn parse_db_columns(result: &QueryResult) -> Vec<DbColumn> {
    let index = |wanted: &str| -> Option<usize> {
        result
            .columns
            .iter()
            .position(|c| c.eq_ignore_ascii_case(wanted))
    };
    let name_col = index("column_name");
    let nullable_col = index("nullable");
    let default_col = index("default_value");
    result
        .rows
        .iter()
        .filter_map(|row| {
            let name = row.get(name_col?)?.as_str().map(str::to_string)?;
            let nullable = row.get(nullable_col?).map(parse_nullable).unwrap_or(false);
            let has_default = row
                .get(default_col?)
                .map(value_has_default)
                .unwrap_or(false);
            Some(DbColumn {
                name,
                nullable,
                has_default,
            })
        })
        .collect()
}

/// Parse a `nullable` cell: JSON bool, or the string flags `true`/`t`/`1`.
/// Anything else (including a missing/NULL cell) is conservatively `false`.
fn parse_nullable(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::String(s) => {
            matches!(s.trim().to_ascii_lowercase().as_str(), "true" | "t" | "1")
        }
        serde_json::Value::Number(n) => n.as_i64().is_some_and(|v| v != 0),
        _ => false,
    }
}

/// A `default_value` cell counts as a default when it is present and not a
/// stand-in for "none": database NULL, the literal `NULL`, or the empty string.
fn value_has_default(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => false,
        serde_json::Value::String(s) => {
            let trimmed = s.trim();
            !trimmed.is_empty() && !trimmed.eq_ignore_ascii_case("NULL")
        }
        _ => true,
    }
}

/// FK edges for the topological load order, from a
/// `Dialect::foreign_keys_sql(schema)` result. The parent (referenced table)
/// must load before the child, so the edge is `(referenced, child)`.
pub(crate) fn fk_edges(result: &QueryResult) -> Result<Vec<(String, String)>, String> {
    if result.rows.is_empty() {
        return Ok(Vec::new());
    }
    let col = |wanted: &str| -> Result<usize, String> {
        result
            .columns
            .iter()
            .position(|c| c.eq_ignore_ascii_case(wanted))
            .ok_or_else(|| format!("FK query result missing column '{}'", wanted))
    };
    let child = col("table_name")?;
    let parent = col("referenced_table")?;
    Ok(result
        .rows
        .iter()
        .filter_map(|row| {
            let parent = row.get(parent)?.as_str()?.to_string();
            let child = row.get(child)?.as_str()?.to_string();
            if parent == child {
                return None; // self-references load as one table
            }
            Some((parent, child))
        })
        .collect())
}

/// Errors when a discovered file has no matching DB table. Returns the DB
/// tables that have no file (reported, skipped) alongside the matched files.
pub(crate) fn match_files_to_db_tables(
    files: &[TableFile],
    db_tables: &[String],
) -> Result<(Vec<TableFile>, Vec<String>), String> {
    let unknown: Vec<String> = files
        .iter()
        .filter(|file| {
            !db_tables
                .iter()
                .any(|t| t.eq_ignore_ascii_case(&file.table))
        })
        .map(|file| file.table.clone())
        .collect();
    if !unknown.is_empty() {
        return Err(format!(
            "data file(s) with no matching table in the database: {} (load never creates tables)",
            unknown.join(", ")
        ));
    }
    let without_file: Vec<String> = db_tables
        .iter()
        .filter(|t| !files.iter().any(|file| t.eq_ignore_ascii_case(&file.table)))
        .cloned()
        .collect();
    let matched: Vec<TableFile> = files.to_vec();
    Ok((matched, without_file))
}

/// Issue #106: schema-aware variant of `match_files_to_db_tables`. When a
/// schema is in effect (`--schema`, else the connection default), only tables
/// listed under that schema count as existing, and a schema that holds none of
/// the listed tables fails with its own error instead of the misleading
/// table-level "no matching table". `schema == None` keeps the old
/// cross-schema behaviour (list everything, match by bare table name).
pub(crate) fn match_files_to_db_tables_in_schema(
    files: &[TableFile],
    listed: &QueryResult,
    schema: Option<&str>,
) -> Result<(Vec<TableFile>, Vec<String>), String> {
    let Some(schema) = schema.filter(|s| !s.is_empty()) else {
        let db_tables = parse_db_tables(listed);
        return match_files_to_db_tables(files, &db_tables);
    };

    let listed_schemas = parse_table_schemas(listed);
    let tables_in_schema: Vec<&str> = listed_schemas
        .iter()
        .filter(|(_, s)| s.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(schema)))
        .map(|(t, _)| t.as_str())
        .collect();
    if tables_in_schema.is_empty() {
        return Err(format!(
            "schema '{schema}' has no tables (does it exist? load never creates tables or schemas)"
        ));
    }

    let db_tables: Vec<String> = tables_in_schema.iter().map(|t| (*t).to_string()).collect();
    let (matched, skipped) = match_files_to_db_tables(files, &db_tables)?;
    Ok((matched, skipped))
}

/// Build the full plan: deterministic topo order of the matched tables with
/// filename order as tiebreak.
pub(crate) fn build_plan(
    files: Vec<TableFile>,
    schema: Option<String>,
    fk_result: &QueryResult,
    listed_schemas: &std::collections::HashMap<String, Option<String>>,
) -> Result<LoadPlan, String> {
    let edges = fk_edges(fk_result)?;
    let mut names: Vec<String> = files.iter().map(|f| f.table.clone()).collect();
    names.sort();
    let order = stable_topo_order(&names, &edges)?;
    let mut by_table: std::collections::HashMap<String, TableFile> =
        files.into_iter().map(|f| (f.table.clone(), f)).collect();
    let entries = order
        .into_iter()
        .filter_map(|table| {
            by_table.remove(&table).map(|file| PlanEntry {
                // Explicit --schema wins; otherwise the schema the table was
                // listed under flows into the entry so per-table column
                // lookups bind a real TABLE_SCHEMA even for URL connections
                // that carry no configured default schema.
                schema: schema
                    .clone()
                    .or_else(|| listed_schemas.get(&table).cloned().flatten()),
                table: file.table,
                columns: file.columns,
                row_count: file.row_count,
                path: file.source_path,
            })
        })
        .collect();
    Ok(LoadPlan { entries })
}

/// Deterministic Kahn's algorithm: among the tables whose parents all loaded,
/// the alphabetically first name goes next. `graph::topological_sort` is
/// order-valid but its ready queue follows HashMap iteration, so it cannot
/// give the filename tiebreak the plan contract requires. Edges are
/// `(parent, child)`: parent loads first.
fn stable_topo_order(nodes: &[String], edges: &[(String, String)]) -> Result<Vec<String>, String> {
    let mut in_degree: std::collections::HashMap<&str, usize> =
        nodes.iter().map(|n| (n.as_str(), 0)).collect();
    let mut children: std::collections::HashMap<&str, Vec<&str>> =
        nodes.iter().map(|n| (n.as_str(), Vec::new())).collect();
    for (from, to) in edges {
        // Edges between tables outside the plan (e.g. referencing a lookup
        // table with no file) do not constrain the order.
        if !in_degree.contains_key(to.as_str()) {
            continue;
        }
        *in_degree.entry(to.as_str()).or_insert(0) += 1;
        children.entry(from.as_str()).or_default().push(to.as_str());
    }
    let mut ready: std::collections::BTreeSet<&str> = in_degree
        .iter()
        .filter(|(_, &deg)| deg == 0)
        .map(|(name, _)| *name)
        .collect();
    let mut order = Vec::with_capacity(nodes.len());
    while let Some(name) = ready.pop_first() {
        order.push(name.to_string());
        for child in children.get(name).into_iter().flatten() {
            let deg = in_degree.get_mut(child).expect("child tracked");
            *deg -= 1;
            if *deg == 0 {
                ready.insert(child);
            }
        }
    }
    if order.len() != nodes.len() {
        let stuck: Vec<&str> = nodes
            .iter()
            .map(String::as_str)
            .filter(|n| !order.iter().any(|done| done == n))
            .collect();
        return Err(format!(
            "foreign key cycle between tables: {} (cannot determine load order)",
            stuck.join(" -> ")
        ));
    }
    Ok(order)
}

/// Column-set validation. In strict mode (`--strict-columns`) the file columns
/// must equal the DB columns (order-insensitive), matching the pre-#113
/// behaviour. Otherwise a column the file omits is allowed when the table can
/// absorb it (nullable, or it has a default) — the INSERT simply leaves it
/// out so the server default (or NULL) applies — and only a
/// NOT NULL column with no default rejects. A column the file carries that the
/// table does not is always rejected (load never creates columns).
///
/// On success returns, per table, the set of omitted columns the INSERT will
/// not mention, so the loader keeps them out of the column list instead of
/// binding explicit NULLs (issue #113 B PR review C5: binding NULL would
/// override a column DEFAULT; omission lets the server decide).
pub(crate) fn validate_column_sets(
    plan: &LoadPlan,
    db_columns: &std::collections::HashMap<String, Vec<DbColumn>>,
    strict: bool,
) -> Result<std::collections::HashMap<String, std::collections::HashSet<String>>, String> {
    let mut allowed_missing = std::collections::HashMap::new();
    for entry in &plan.entries {
        let Some(db_cols) = db_columns.get(&entry.table) else {
            return Err(format!(
                "table '{}': no column metadata available for validation",
                entry.table
            ));
        };
        let missing: Vec<&DbColumn> = db_cols
            .iter()
            .filter(|c| {
                !entry
                    .columns
                    .iter()
                    .any(|fc| fc.eq_ignore_ascii_case(&c.name))
            })
            .collect();
        let extra: Vec<&String> = entry
            .columns
            .iter()
            .filter(|fc| !db_cols.iter().any(|c| c.name.eq_ignore_ascii_case(fc)))
            .collect();

        // Strict mode keeps every miss a rejection; non-strict splits the
        // misses into "can insert NULL" and "cannot".
        let (allowed, rejected): (Vec<&DbColumn>, Vec<&DbColumn>) = if strict {
            (Vec::new(), missing.clone())
        } else {
            missing
                .iter()
                .copied()
                .partition(|c| c.nullable || c.has_default)
        };

        if !rejected.is_empty() || !extra.is_empty() {
            let mut message = format!(
                "table '{}': data file columns do not match the table columns",
                entry.table
            );
            if strict {
                if !missing.is_empty() {
                    message.push_str(&format!(
                        "\n  missing in file: {}",
                        join_names(missing.iter().map(|c| c.name.as_str()))
                    ));
                }
            } else {
                if !allowed.is_empty() {
                    message.push_str(&format!(
                        "\n  missing in file (omitted, server default or NULL applies): {}",
                        join_names(allowed.iter().map(|c| c.name.as_str()))
                    ));
                }
                if !rejected.is_empty() {
                    message.push_str(&format!(
                        "\n  missing in file (NOT NULL without default): {}",
                        join_names(rejected.iter().map(|c| c.name.as_str()))
                    ));
                }
            }
            if !extra.is_empty() {
                message.push_str(&format!(
                    "\n  extra in file: {}",
                    join_names(extra.iter().map(|s| s.as_str()))
                ));
            }
            return Err(message);
        }

        if !allowed.is_empty() {
            allowed_missing.insert(
                entry.table.clone(),
                allowed.iter().map(|c| c.name.clone()).collect(),
            );
        }
    }
    Ok(allowed_missing)
}

fn join_names<'a>(names: impl Iterator<Item = &'a str>) -> String {
    names.collect::<Vec<_>>().join(", ")
}

// ─── Rendering ──────────────────────────────────────────────────────────

/// One line per entry: `<table> (<rows> rows, <file>)`.
pub(crate) fn render_plan(
    plan: &LoadPlan,
    connection: &str,
    scheme: &str,
    schema: &str,
    skipped: &[String],
) -> String {
    let mut text = format!("load plan for {connection} ({scheme}), schema {schema}:\n");
    for entry in &plan.entries {
        text.push_str(&format!(
            "  {} ({} rows, {})\n",
            entry.table,
            entry.row_count,
            entry.path.display()
        ));
    }
    if !skipped.is_empty() {
        text.push_str(&format!(
            "  no data file, skipped: {}\n",
            skipped.join(", ")
        ));
    }
    let rows: usize = plan.entries.iter().map(|e| e.row_count).sum();
    text.push_str(&format!("  {} tables, {} rows", plan.entries.len(), rows));
    text
}

fn scan_table_names(dir: &Path) -> Result<Vec<String>, String> {
    let mut names: Vec<String> = Vec::new();
    let entries =
        std::fs::read_dir(dir).map_err(|e| format!("read data dir {}: {}", dir.display(), e))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("read dir entry: {e}"))?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        match path.extension().and_then(|s| s.to_str()) {
            Some("jsonl") | Some("json") | Some("csv") => names.push(stem.to_string()),
            _ => {}
        }
    }
    names.sort();
    names.dedup();
    Ok(names)
}

// ─── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn fk_result(columns: &[&str], rows: Vec<Vec<Value>>) -> QueryResult {
        QueryResult {
            columns: columns.iter().map(|s| s.to_string()).collect(),
            row_count: rows.len(),
            rows,
            rows_affected: None,
        }
    }

    fn empty_schemas() -> std::collections::HashMap<String, Option<String>> {
        std::collections::HashMap::new()
    }

    fn fk_row(child: &str, parent: &str) -> Vec<Value> {
        vec![
            Value::from("shop"),
            Value::from(child),
            Value::from("parent_id"),
            Value::from("shop"),
            Value::from(parent),
            Value::from("id"),
            Value::from("fk_1"),
        ]
    }

    fn file(table: &str, columns: &[&str], rows: usize) -> TableFile {
        TableFile {
            table: table.to_string(),
            columns: columns.iter().map(|s| s.to_string()).collect(),
            row_count: rows,
            source_path: PathBuf::from(format!("/data/{table}.jsonl")),
        }
    }

    fn empty_fk() -> QueryResult {
        fk_result(&["table_name", "referenced_table"], Vec::new())
    }

    /// `list_tables()` result rows as `(schema_name, table_name)` pairs.
    fn listed_result(rows: &[(&str, &str)]) -> QueryResult {
        QueryResult {
            columns: vec![
                "schema_name".to_string(),
                "table_name".to_string(),
                "table_type".to_string(),
                "engine".to_string(),
                "row_count".to_string(),
                "total_size".to_string(),
                "comment".to_string(),
            ],
            row_count: rows.len(),
            rows: rows
                .iter()
                .map(|(schema, table)| {
                    vec![
                        Value::from(*schema),
                        Value::from(*table),
                        Value::from("table"),
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        Value::Null,
                    ]
                })
                .collect(),
            rows_affected: None,
        }
    }

    #[test]
    fn should_plan_topological_order_from_fk_edges() {
        // orders references users; users must load first.
        let files = vec![
            file("orders", &["id", "user_id"], 5),
            file("users", &["id"], 3),
        ];
        let fk = fk_result(
            &[
                "schema_name",
                "table_name",
                "column_name",
                "referenced_schema",
                "referenced_table",
                "referenced_column",
                "constraint_name",
            ],
            vec![fk_row("orders", "users")],
        );
        let plan = build_plan(files, Some("shop".to_string()), &fk, &empty_schemas()).unwrap();
        let tables: Vec<&str> = plan.entries.iter().map(|e| e.table.as_str()).collect();
        assert_eq!(tables, vec!["users", "orders"]);
        assert_eq!(plan.entries[0].schema.as_deref(), Some("shop"));
        assert_eq!(plan.entries[0].row_count, 3);
    }

    #[test]
    fn should_inherit_listed_schema_into_plan_entries_without_explicit_schema() {
        // A named connection with no `schema` field (e.g. a URL connection):
        // the schema each table was listed under must flow into the plan so
        // per-table column lookups bind a real TABLE_SCHEMA.
        let files = vec![file("users", &["id"], 2)];
        let listed_schemas = parse_table_schemas(&listed_result(&[("testdb", "users")]));
        let plan = build_plan(files, None, &empty_fk(), &listed_schemas).unwrap();
        assert_eq!(
            plan.entries[0].schema.as_deref(),
            Some("testdb"),
            "plan entry must carry the schema the table was listed under"
        );
    }

    #[test]
    fn should_reject_file_without_matching_table() {
        let files = vec![file("orders", &["id"], 5), file("ghost", &["id"], 1)];
        let db_tables = vec!["users".to_string(), "orders".to_string()];
        let err = match_files_to_db_tables(&files, &db_tables).unwrap_err();
        assert!(err.contains("ghost"), "error must name the file: {err}");
        assert!(err.contains("never creates tables"), "{err}");
    }

    /// Issue #106: `--schema nonexistent` must fail with a schema-level
    /// error, not the misleading "no matching table" one.
    #[test]
    fn should_error_on_schema_with_no_tables() {
        let files = vec![file("customers", &["id"], 1)];
        let listed = listed_result(&[("testdb", "customers")]);
        let err = match_files_to_db_tables_in_schema(&files, &listed, Some("ghost")).unwrap_err();
        assert!(
            err.contains("schema 'ghost' has no tables"),
            "schema-level error must name the schema: {err}"
        );
        assert!(
            err.contains("does it exist"),
            "error must point at the schema qualifier: {err}"
        );
    }

    /// Issue #106: with an explicit --schema, tables listed under other
    /// schemas must not satisfy the match (the schema takes part in
    /// filtering), and a table only present in another schema is reported
    /// as unmatched.
    #[test]
    fn should_filter_db_tables_by_the_explicit_schema() {
        let files = vec![file("customers", &["id"], 1)];
        let listed = listed_result(&[
            ("shop", "customers"), // same name, different schema
            ("testdb", "unrelated"),
        ]);
        let err = match_files_to_db_tables_in_schema(&files, &listed, Some("testdb")).unwrap_err();
        assert!(
            err.contains("customers"),
            "customers lives in shop, not testdb: {err}"
        );

        // And a real match binds only within the requested schema.
        let listed2 = listed_result(&[
            ("shop", "customers"),
            ("testdb", "customers"),
            ("testdb", "unrelated"),
        ]);
        let (matched, skipped) =
            match_files_to_db_tables_in_schema(&files, &listed2, Some("testdb")).unwrap();
        assert_eq!(matched.len(), 1);
        assert_eq!(skipped, vec!["unrelated".to_string()]);
    }

    /// Issue #106: without --schema the old cross-schema behaviour holds
    /// (a URL connection without a default schema still loads what it sees).
    #[test]
    fn should_match_across_schemas_when_no_schema_is_given() {
        let files = vec![file("customers", &["id"], 1)];
        let listed = listed_result(&[("shop", "customers")]);
        let (matched, _skipped) =
            match_files_to_db_tables_in_schema(&files, &listed, None).unwrap();
        assert_eq!(matched.len(), 1);
    }

    #[test]
    fn should_pick_fk_schema_from_the_tables_being_loaded() {
        // A server with many schemas: only the schemas of the tables being
        // loaded are candidates. Any other listed schema must be ignored, so
        // the FK query cannot land on an unrelated schema's edges.
        let listed = listed_result(&[
            ("testdb", "orders"),
            ("rev81", "users"),
            ("synth_guard", "events"),
        ]);
        let listed_schemas = parse_table_schemas(&listed);
        assert_eq!(listed_schemas.len(), 3);

        let planned_tables = ["orders".to_string(), "users".to_string()];

        let explicit = schema_for_fk_lookup(
            Some("shop".to_string()),
            None,
            &listed_schemas,
            &planned_tables,
        );
        assert_eq!(explicit.as_deref(), Some("shop"));

        let from_target = schema_for_fk_lookup(
            None,
            Some("testdb".to_string()),
            &listed_schemas,
            &planned_tables,
        );
        assert_eq!(from_target.as_deref(), Some("testdb"));

        // No explicit schema and no configured default: the candidate set is
        // exactly the schemas of the planned tables. orders lives in testdb,
        // users in rev81 — the alphabetically first candidate wins, which is
        // deterministic and provably holds one of the planned tables.
        let inferred = schema_for_fk_lookup(None, None, &listed_schemas, &planned_tables)
            .expect("planned tables identify their schema");
        assert_eq!(inferred, "rev81", "sorted candidates pick the first");
    }

    #[test]
    fn should_return_none_for_fk_schema_when_planned_tables_have_no_schema() {
        let listed_schemas: std::collections::HashMap<String, Option<String>> =
            std::collections::HashMap::new();
        let planned_tables = vec!["users".to_string()];
        assert_eq!(
            schema_for_fk_lookup(None, None, &listed_schemas, &planned_tables),
            None
        );
    }

    #[test]
    fn should_filter_tables_by_flag() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.jsonl"), "{\"x\":1}\n").unwrap();
        std::fs::write(dir.path().join("b.jsonl"), "{\"x\":1}\n").unwrap();
        let tables = vec!["b".to_string()];
        let files = discover_table_files(dir.path(), Some(&tables), "auto").unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].table, "b");
        assert_eq!(files[0].row_count, 1);
        assert_eq!(files[0].columns, vec!["x".to_string()]);
    }

    #[test]
    fn should_count_rows_and_columns() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("t.jsonl"),
            "{\"a\":1,\"b\":\"x\"}\n{\"a\":2,\"b\":\"y\"}\n{\"a\":3,\"b\":\"z\"}\n",
        )
        .unwrap();
        let files = discover_table_files(dir.path(), None, "auto").unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].table, "t");
        assert_eq!(files[0].row_count, 3);
        assert_eq!(files[0].columns, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn should_record_the_discovered_file_path_in_auto_mode() {
        // `auto` discovers via the shared jsonl -> json -> csv order; the
        // recorded source_path must name the file actually read, not a
        // jsonl guess. The executor reads entry.path, so a wrong path here
        // fails the load with "No such file or directory" (found on a live
        // csv-only directory: dry-run counted rows fine, the run did not).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("t.json"), "[{\"a\":1}]").unwrap();
        std::fs::write(dir.path().join("u.csv"), "a\n7\n").unwrap();
        let files = discover_table_files(dir.path(), None, "auto").unwrap();
        assert_eq!(files.len(), 2);
        let by_table: std::collections::HashMap<&str, &TableFile> =
            files.iter().map(|f| (f.table.as_str(), f)).collect();
        assert_eq!(
            by_table["t"].source_path,
            dir.path().join("t.json"),
            "auto mode must record the discovered json path"
        );
        assert_eq!(
            by_table["u"].source_path,
            dir.path().join("u.csv"),
            "auto mode must record the discovered csv path"
        );
        // Strict modes keep naming the exact file in that format.
        let strict = discover_table_files(dir.path(), Some(&["u".to_string()]), "csv").unwrap();
        assert_eq!(strict[0].source_path, dir.path().join("u.csv"));
    }

    #[test]
    fn should_keep_filename_order_without_fk() {
        let files = vec![
            file("zeta", &["id"], 1),
            file("alpha", &["id"], 2),
            file("mid", &["id"], 3),
        ];
        let plan = build_plan(files, None, &empty_fk(), &empty_schemas()).unwrap();
        let tables: Vec<&str> = plan.entries.iter().map(|e| e.table.as_str()).collect();
        assert_eq!(tables, vec!["alpha", "mid", "zeta"]);
    }

    #[test]
    fn should_error_on_fk_cycle() {
        let files = vec![file("a", &["id"], 1), file("b", &["id"], 1)];
        let fk = fk_result(
            &["table_name", "referenced_table"],
            vec![
                vec![Value::from("a"), Value::from("b")],
                vec![Value::from("b"), Value::from("a")],
            ],
        );
        let err = build_plan(files, None, &fk, &empty_schemas()).unwrap_err();
        assert!(err.contains("cycle"), "{err}");
    }

    #[test]
    fn should_reject_strict_csv_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("t.jsonl"), "{\"x\":1}\n").unwrap();
        let err = read_table_file(dir.path(), "t", "csv").unwrap_err();
        assert!(err.contains("--format csv is strict"), "{err}");
        assert!(err.contains("t.csv"), "{err}");
        // auto falls back to the discovery order instead.
        assert!(read_table_file(dir.path(), "t", "auto").unwrap().is_some());
    }

    /// A single-entry plan whose file exposes exactly `columns`.
    fn plan_with_columns(columns: &[&str]) -> LoadPlan {
        LoadPlan {
            entries: vec![PlanEntry {
                table: "t".to_string(),
                schema: None,
                path: PathBuf::from("/data/t.jsonl"),
                columns: columns.iter().map(|s| s.to_string()).collect(),
                row_count: 2,
            }],
        }
    }

    fn db_col(name: &str, nullable: bool, has_default: bool) -> DbColumn {
        DbColumn {
            name: name.to_string(),
            nullable,
            has_default,
        }
    }

    fn db_map(cols: Vec<DbColumn>) -> std::collections::HashMap<String, Vec<DbColumn>> {
        let mut db = std::collections::HashMap::new();
        db.insert("t".to_string(), cols);
        db
    }

    /// Columns from a `table_columns()` result: the nullable/default_value
    /// positions are located by name, not index.
    #[test]
    fn should_parse_db_columns_nullable_and_defaults() {
        let result = fk_result(
            &[
                "column_name",
                "data_type",
                "nullable",
                "default_value",
                "ordinal_position",
                "comment",
                "column_key",
            ],
            vec![
                vec![
                    Value::from("id"),
                    Value::from("bigint"),
                    Value::Bool(false),
                    Value::Null,
                    Value::from(1),
                    Value::Null,
                    Value::from("PRI"),
                ],
                vec![
                    Value::from("name"),
                    Value::from("varchar(100)"),
                    Value::Bool(true),
                    Value::Null,
                    Value::from(2),
                    Value::Null,
                    Value::Null,
                ],
                vec![
                    Value::from("status"),
                    Value::from("varchar(20)"),
                    Value::Bool(false),
                    Value::from("active"),
                    Value::from(3),
                    Value::Null,
                    Value::Null,
                ],
                vec![
                    Value::from("created"),
                    Value::from("timestamp"),
                    Value::Bool(false),
                    Value::from("CURRENT_TIMESTAMP"),
                    Value::from(4),
                    Value::Null,
                    Value::Null,
                ],
            ],
        );
        let cols = parse_db_columns(&result);
        assert_eq!(cols.len(), 4);
        assert_eq!(cols[0], db_col("id", false, false));
        assert_eq!(cols[1], db_col("name", true, false));
        assert_eq!(cols[2], db_col("status", false, true));
        assert_eq!(cols[3], db_col("created", false, true));
    }

    /// A database NULL, the literal string "NULL", and the empty string all
    /// mean "no default"; nullable parses from bools and from string flags.
    #[test]
    fn should_treat_null_literal_empty_and_db_null_defaults_as_absent() {
        let result = fk_result(
            &["column_name", "nullable", "default_value"],
            vec![
                vec![Value::from("a"), Value::from("true"), Value::Null],
                vec![Value::from("b"), Value::from("false"), Value::from("NULL")],
                vec![Value::from("c"), Value::from("t"), Value::from("")],
                vec![Value::from("d"), Value::from("1"), Value::from("0")],
            ],
        );
        let cols = parse_db_columns(&result);
        assert_eq!(cols[0], db_col("a", true, false));
        assert_eq!(cols[1], db_col("b", false, false));
        assert_eq!(cols[2], db_col("c", true, false));
        assert_eq!(cols[3], db_col("d", true, true));
    }

    #[test]
    fn should_validate_column_sets_order_insensitively() {
        let plan = plan_with_columns(&["b", "a"]);
        let db = db_map(vec![
            db_col("a", true, false),
            db_col("b", true, false),
            db_col("c", false, false),
        ]);
        let err = validate_column_sets(&plan, &db, true).unwrap_err();
        assert!(err.contains("missing in file: c"), "{err}");
        assert!(!err.contains("extra in file"), "{err}");
    }

    #[test]
    fn should_name_extra_columns_in_validation() {
        let plan = plan_with_columns(&["a", "ghost"]);
        let db = db_map(vec![db_col("a", true, false)]);
        let err = validate_column_sets(&plan, &db, true).unwrap_err();
        assert!(err.contains("extra in file: ghost"), "{err}");
        assert!(err.contains("table 't'"), "{err}");
    }

    /// Issue #113 B: a missing nullable column (or one with a default) is
    /// allowed in non-strict mode and reported as insert-NULL in the allow set.
    #[test]
    fn should_allow_missing_nullable_or_defaulted_columns() {
        let plan = plan_with_columns(&["id"]);
        let db = db_map(vec![
            db_col("id", false, false),
            db_col("note", true, false),
            db_col("status", false, true),
        ]);
        let allowed = validate_column_sets(&plan, &db, false).unwrap();
        let set = allowed.get("t").expect("allow set for t");
        assert!(set.contains("note"), "{set:?}");
        assert!(set.contains("status"), "{set:?}");
        assert!(!set.contains("id"), "{set:?}");
    }

    /// Issue #113 B: a NOT NULL column with no default cannot be filled, so it
    /// still rejects; the message groups the allowed misses separately.
    #[test]
    fn should_reject_missing_not_null_column_without_default() {
        let plan = plan_with_columns(&["id"]);
        let db = db_map(vec![
            db_col("id", false, false),
            db_col("a", true, false),
            db_col("b", true, false),
            db_col("c", false, false),
        ]);
        let err = validate_column_sets(&plan, &db, false).unwrap_err();
        assert!(
            err.contains("missing in file (NOT NULL without default): c"),
            "{err}"
        );
        assert!(
            err.contains("missing in file (omitted, server default or NULL applies): a, b"),
            "{err}"
        );
    }

    /// `--strict-columns` restores the old set-equality behaviour: any missing
    /// column rejects, even nullable ones.
    #[test]
    fn should_reject_any_missing_column_in_strict_mode() {
        let plan = plan_with_columns(&["id"]);
        let db = db_map(vec![
            db_col("id", false, false),
            db_col("note", true, false),
            db_col("status", false, true),
        ]);
        let err = validate_column_sets(&plan, &db, true).unwrap_err();
        assert!(err.contains("missing in file: note, status"), "{err}");
    }

    #[test]
    fn should_render_plan_with_summary_and_skipped() {
        let plan = LoadPlan {
            entries: vec![
                PlanEntry {
                    table: "users".to_string(),
                    schema: Some("shop".to_string()),
                    path: PathBuf::from("/data/users.jsonl"),
                    columns: vec!["id".to_string()],
                    row_count: 3,
                },
                PlanEntry {
                    table: "orders".to_string(),
                    schema: Some("shop".to_string()),
                    path: PathBuf::from("/data/orders.jsonl"),
                    columns: vec!["id".to_string(), "user_id".to_string()],
                    row_count: 5,
                },
            ],
        };
        let text = render_plan(&plan, "dev", "mysql", "shop", &["logs".to_string()]);
        assert!(
            text.contains("load plan for dev (mysql), schema shop:"),
            "{text}"
        );
        assert!(text.contains("users (3 rows, /data/users.jsonl)"), "{text}");
        assert!(
            text.contains("orders (5 rows, /data/orders.jsonl)"),
            "{text}"
        );
        assert!(text.contains("no data file, skipped: logs"), "{text}");
        assert!(text.ends_with("2 tables, 8 rows"), "{text}");
    }

    #[test]
    fn should_parse_fk_edges_with_parent_before_child() {
        let fk = fk_result(
            &["table_name", "referenced_table"],
            vec![
                vec![Value::from("orders"), Value::from("users")],
                vec![Value::from("users"), Value::from("users")], // self-ref ignored
            ],
        );
        let edges = fk_edges(&fk).unwrap();
        assert_eq!(edges, vec![("users".to_string(), "orders".to_string())]);
    }

    #[test]
    fn should_error_when_fk_result_misses_columns() {
        let fk = fk_result(
            &["child", "parent"],
            vec![vec![Value::from("a"), Value::from("b")]],
        );
        assert!(fk_edges(&fk).is_err());
    }

    #[test]
    fn should_list_db_tables_from_list_tables_result() {
        let result = fk_result(
            &["schema_name", "table_name"],
            vec![
                vec![Value::from("shop"), Value::from("users")],
                vec![Value::from("shop"), Value::from("orders")],
            ],
        );
        assert_eq!(
            parse_db_tables(&result),
            vec!["users".to_string(), "orders".to_string()]
        );
    }
}
