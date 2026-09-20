//! Load execution (issue #98). Implemented in a later wave: this skeleton
//! owns the plan, the gate, and the audit trail; the executor turns each
//! `PlanEntry` into parameterized INSERTs.

use super::plan::LoadPlan;
use crate::backend::DbConn;

// ─── INSERT rendering ───────────────────────────────────────────────────

/// Render the parameterized INSERT for one table. Placeholder style follows
/// the URL scheme: mysql/duckdb use `?`, oracle uses numbered `:N` binds,
/// gaussdb uses `$N` binds. Identifier quoting (and oracle's uppercase
/// folding) is applied per scheme. Pure so it is testable without a backend.
pub(crate) fn insert_sql(
    scheme: &str,
    quote: char,
    schema: Option<&str>,
    table: &str,
    columns: &[String],
) -> String {
    let table_ref = crate::backend::quote_table_scheme(scheme, quote, schema, table);
    let cols: Vec<String> = columns
        .iter()
        .map(|c| crate::backend::quote_ident_scheme(scheme, quote, c))
        .collect();
    let placeholders: Vec<String> = match scheme {
        "oracle" => (1..=columns.len()).map(|i| format!(":{i}")).collect(),
        "gaussdb" => (1..=columns.len()).map(|i| format!("${i}")).collect(),
        _ => vec!["?".to_string(); columns.len()],
    };
    format!(
        "INSERT INTO {table_ref} ({}) VALUES ({})",
        cols.join(", "),
        placeholders.join(", ")
    )
}

// ─── Column type alignment ──────────────────────────────────────────────

/// Map each plan column to its DB `data_type` from a `table_columns()` result
/// (`[column_name, data_type, ...]`). Matching is case-insensitive (Oracle
/// folds to uppercase); a plan column missing from the DB result is a named
/// error — plan validation should have caught it, this re-checks defensively.
pub(crate) fn align_types(
    plan_columns: &[String],
    db_rows: &[Vec<serde_json::Value>],
) -> Result<Vec<String>, String> {
    let lookup: std::collections::HashMap<String, String> = db_rows
        .iter()
        .filter_map(|row| {
            let name = row.first()?.as_str()?;
            let data_type = row.get(1)?.as_str()?;
            Some((name.to_ascii_lowercase(), data_type.to_string()))
        })
        .collect();
    plan_columns
        .iter()
        .map(|col| {
            lookup
                .get(&col.to_ascii_lowercase())
                .cloned()
                .ok_or_else(|| {
                    format!(
                    "table column lookup failed: '{col}' is not in the table's columns ({} known)",
                    lookup.len()
                )
                })
        })
        .collect()
}

// ─── Executor ───────────────────────────────────────────────────────────

use crate::audit::event::{ActionClass, Channel, ConnectionInfo, Decision};

/// Execute the plan against `conn`, returning the cumulative rows inserted.
/// Fail-fast: each entry runs in its own transaction; the first error rolls
/// back that table's transaction and aborts the whole load. Per-table
/// intent/outcome events are recorded here (row counts only, never row data).
pub(crate) async fn execute(
    conn: &mut dyn DbConn,
    plan: &LoadPlan,
    audit: &crate::audit::AuditSession,
    conn_info: &ConnectionInfo,
) -> Result<u64, String> {
    let scheme = conn.dialect().url_scheme().to_string();
    let quote = conn.dialect().identifier_quote();
    let columns_sql = conn.dialect().table_columns().to_string();
    let mut completed: Vec<String> = Vec::new();
    let mut total: u64 = 0;

    for entry in &plan.entries {
        let table_started = std::time::Instant::now();
        audit.record_best_effort(table_event(
            conn_info,
            Decision::Allow,
            ActionClass::Dml,
            table_intent_detail(&scheme, entry, entry.row_count as u64),
        ));

        match load_table(conn, &scheme, quote, &columns_sql, entry).await {
            Ok(loaded) => {
                completed.push(entry.table.clone());
                total += loaded as u64;
                audit.record_best_effort(table_event(
                    conn_info,
                    Decision::Allow,
                    ActionClass::Dml,
                    table_outcome_detail(
                        &entry.table,
                        loaded,
                        table_started.elapsed().as_millis() as u64,
                    ),
                ));
            }
            Err((loaded_before_failure, message)) => {
                audit.record_best_effort(table_event(
                    conn_info,
                    Decision::Error,
                    ActionClass::Dml,
                    serde_json::json!({
                        "subcommand": "load",
                        "phase": "outcome",
                        "table": entry.table,
                        "rows_loaded": loaded_before_failure,
                        "error_kind": "insert_failed",
                    }),
                ));
                return Err(format!(
                    "table '{}' failed: {message} (completed: {})",
                    entry.table,
                    format_completed(&completed)
                ));
            }
        }
    }
    Ok(total)
}

/// Load one table inside a transaction: fetch column types, read + coerce the
/// file rows, then execute parameterized INSERTs. On success returns the
/// committed row count; on failure the transaction is rolled back and the
/// error carries the rows inserted before the failure (those rows are rolled
/// back too, so they are never durable).
async fn load_table(
    conn: &mut dyn DbConn,
    scheme: &str,
    quote: char,
    columns_sql: &str,
    entry: &super::plan::PlanEntry,
) -> Result<usize, (usize, String)> {
    // Oracle: oracle-rs DML hardcodes auto_commit=false and the only commit()
    // lives on its own Connection type, unreachable through the DbConn trait
    // (no commit/rollback method exists). Rather than guess, refuse: a load
    // that cannot be committed must not start.
    if scheme == "oracle" {
        return Err((
            0,
            "oracle backend cannot commit through the current connection trait (no commit/rollback); load support pending driver surface"
                .to_string(),
        ));
    }

    let schema_param = entry
        .schema
        .clone()
        .unwrap_or_else(|| default_schema_for_scheme(scheme));
    let columns_result = conn
        .exec(
            columns_sql,
            &[
                serde_json::Value::from(schema_param),
                serde_json::Value::from(entry.table.clone()),
            ],
        )
        .await
        .map_err(|e| (0, format!("column types: {e}")))?;
    let types = align_types(&entry.columns, &columns_result.rows)
        .map_err(|e| (0, format!("column types: {e}")))?;

    let (file_columns, rows) = read_table_rows(&entry.path).map_err(|e| (0, e))?;

    conn.query_drop(begin_sql(scheme))
        .await
        .map_err(|e| (0, format!("begin transaction: {e}")))?;

    let insert = insert_sql(
        scheme,
        quote,
        entry.schema.as_deref(),
        &entry.table,
        &entry.columns,
    );
    for (i, row) in rows.iter().enumerate() {
        // Align the file row to the plan column order first (JSONL/JSON key
        // order can drift from the discovered column list), then coerce with
        // the shared per-column error formatting.
        let mut aligned = Vec::with_capacity(entry.columns.len());
        for col in &entry.columns {
            match file_columns
                .iter()
                .position(|fc| fc.eq_ignore_ascii_case(col))
            {
                Some(pos) => aligned.push(row[pos].clone()),
                None => {
                    let message = format!("row {}: data file is missing column '{col}'", i + 1);
                    return Err(rollback(conn, loaded_so_far(i), message).await);
                }
            }
        }
        let coerced = match crate::tabular::coerce_row(&aligned, &types, &entry.columns) {
            Ok(values) => values,
            Err(reason) => {
                let message = format!("row {}: {reason}", i + 1);
                return Err(rollback(conn, loaded_so_far(i), message).await);
            }
        };
        if let Err(e) = conn.exec(&insert, &coerced).await {
            let message =
                format!("insert failed after {i} row(s) inserted: {e} (statement: {insert})");
            return Err(rollback(conn, loaded_so_far(i), message).await);
        }
    }

    conn.query_drop("COMMIT").await.map_err(|e| {
        (
            rows.len(),
            format!("commit failed, transaction rolled back: {e}"),
        )
    })?;
    Ok(rows.len())
}

/// Best-effort ROLLBACK after a failure, pairing the count of rows inserted
/// before the failure with the original message (the rolled-back rows are
/// never durable).
async fn rollback(
    conn: &mut dyn DbConn,
    loaded_before_failure: usize,
    message: String,
) -> (usize, String) {
    let _ = conn.query_drop("ROLLBACK").await;
    (loaded_before_failure, message)
}

/// Rows inserted before row `i` failed (0-based index of the failing row).
fn loaded_so_far(failing_row_index: usize) -> usize {
    failing_row_index
}

/// BEGIN statement per scheme: MySQL family uses START TRANSACTION, DuckDB and
/// GaussDB use BEGIN.
fn begin_sql(scheme: &str) -> &'static str {
    match scheme {
        "mysql" => "START TRANSACTION",
        _ => "BEGIN",
    }
}

/// The schema value bound into `table_columns()` when the plan entry has none.
fn default_schema_for_scheme(scheme: &str) -> String {
    match scheme {
        "duckdb" => "main".to_string(),
        _ => String::new(),
    }
}

/// Read rows from the entry's file, inferring the format from the extension
/// (same dispatch as plan.rs discovery: .jsonl / .json / .csv).
fn read_table_rows(
    path: &std::path::Path,
) -> Result<(Vec<String>, Vec<Vec<serde_json::Value>>), String> {
    let reader = match path.extension().and_then(|e| e.to_str()) {
        Some("jsonl") => crate::tabular::read_jsonl,
        Some("json") => crate::tabular::read_json,
        Some("csv") => crate::tabular::read_csv,
        other => {
            return Err(format!(
                "unsupported data file extension {other:?} (expected .jsonl|.json|.csv)"
            ))
        }
    };
    reader(path)
}

/// `a, b` or `none` when the first table itself failed; rendered into the
/// final error so stderr names what completed before the failure.
fn format_completed(tables: &[String]) -> String {
    if tables.is_empty() {
        "none".to_string()
    } else {
        tables.join(", ")
    }
}

// ─── Per-table audit events ─────────────────────────────────────────────

fn table_event(
    conn_info: &ConnectionInfo,
    decision: Decision,
    class: ActionClass,
    detail: serde_json::Value,
) -> crate::audit::event::DraftEvent {
    crate::audit::event::DraftEvent::new(Channel::Load, conn_info.clone(), "load", class, decision)
        .with_detail(detail)
}

fn table_intent_detail(
    scheme: &str,
    entry: &super::plan::PlanEntry,
    rows_planned: u64,
) -> serde_json::Value {
    serde_json::json!({
        "subcommand": "load",
        "phase": "intent",
        "scheme": scheme,
        "table": entry.table,
        "rows_planned": rows_planned,
    })
}

fn table_outcome_detail(table: &str, rows_loaded: usize, duration_ms: u64) -> serde_json::Value {
    serde_json::json!({
        "subcommand": "load",
        "phase": "outcome",
        "table": table,
        "rows_loaded": rows_loaded,
        "duration_ms": duration_ms,
    })
}

// ─── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[cfg(feature = "duckdb")]
    mod duckdb_live {
        use super::*;
        use crate::audit::AuditSession;
        use crate::backend::BackendFactory;
        use crate::load::plan::PlanEntry;
        use serde_json::json;

        async fn memory_conn() -> Box<dyn DbConn + Send> {
            let factory = crate::backend::duckdb::DuckDbFactory;
            let pool = factory
                .connect("duckdb://:memory:", None)
                .await
                .expect("pool");
            pool.acquire().await.expect("acquire")
        }

        fn conn_info() -> ConnectionInfo {
            ConnectionInfo::from_url("test", "duckdb://:memory:", false)
        }

        fn plan_entry(
            table: &str,
            columns: &[&str],
            path: &std::path::Path,
            rows: usize,
        ) -> PlanEntry {
            PlanEntry {
                table: table.to_string(),
                schema: Some("main".to_string()),
                path: path.to_path_buf(),
                columns: columns.iter().map(|s| s.to_string()).collect(),
                row_count: rows,
            }
        }

        async fn create_fk_schema(conn: &mut (dyn DbConn + Send)) {
            conn.query_drop(
                "CREATE TABLE main.users (id BIGINT PRIMARY KEY, name VARCHAR, note VARCHAR)",
            )
            .await
            .expect("create users");
            conn.query_drop(
                "CREATE TABLE main.orders (id BIGINT PRIMARY KEY, user_id BIGINT, amount DECIMAL(10,2), \
                 CONSTRAINT fk_orders_users FOREIGN KEY (user_id) REFERENCES main.users(id))",
            )
            .await
            .expect("create orders");
        }

        fn write_file(dir: &tempfile::TempDir, name: &str, content: &str) -> PathBuf {
            let path = dir.path().join(name);
            std::fs::write(&path, content).expect("write file");
            path
        }

        #[tokio::test]
        async fn execute_loads_two_fk_tables_in_order_and_preserves_null_vs_empty_string() {
            let dir = tempfile::tempdir().unwrap();
            let users_path = write_file(
                &dir,
                "users.jsonl",
                "{\"id\":1,\"name\":\"alice\",\"note\":null}\n{\"id\":2,\"name\":\"bob\",\"note\":\"\"}\n",
            );
            let orders_path = write_file(
                &dir,
                "orders.jsonl",
                "{\"id\":10,\"user_id\":1,\"amount\":\"9.50\"}\n{\"id\":11,\"user_id\":2,\"amount\":\"0.01\"}\n",
            );
            let plan = LoadPlan {
                entries: vec![
                    plan_entry("users", &["id", "name", "note"], &users_path, 2),
                    plan_entry("orders", &["id", "user_id", "amount"], &orders_path, 2),
                ],
            };

            let mut conn = memory_conn().await;
            create_fk_schema(conn.as_mut()).await;

            let audit = AuditSession::disabled();
            let inserted = loader_execute(&mut *conn, &plan, &audit)
                .await
                .expect("execute");
            assert_eq!(inserted, 4);

            // Row counts.
            let users = conn.query("SELECT COUNT(*) FROM main.users").await.unwrap();
            assert_eq!(users.rows[0][0], json!(2));
            let orders = conn
                .query("SELECT COUNT(*) FROM main.orders")
                .await
                .unwrap();
            assert_eq!(orders.rows[0][0], json!(2));

            // NULL vs '' preservation (the csv/jsonl round-trip contract).
            let notes = conn
                .query("SELECT name, note FROM main.users ORDER BY id")
                .await
                .unwrap();
            assert_eq!(notes.rows[0][1], serde_json::Value::Null);
            assert_eq!(notes.rows[1][1], json!(""));

            // Content + decimal handling: BIGINT binds as a number; the
            // decimal string "9.50" binds as text into DECIMAL(10,2) and
            // reads back as the numeric 9.5 (duckdb's decimal → JSON form).
            let order = conn
                .query("SELECT user_id, amount FROM main.orders WHERE id = 10")
                .await
                .unwrap();
            assert_eq!(order.rows[0][0], json!(1));
            assert_eq!(order.rows[0][1], json!(9.5));
        }

        #[tokio::test]
        async fn execute_fail_fast_rolls_back_failed_table_and_names_completed_tables() {
            let dir = tempfile::tempdir().unwrap();
            let users_path = write_file(
                &dir,
                "users.jsonl",
                "{\"id\":1,\"name\":\"alice\",\"note\":null}\n",
            );
            let orders_path = write_file(
                &dir,
                "orders.jsonl",
                "{\"id\":10,\"user_id\":1,\"amount\":\"1.00\"}\n{\"id\":10,\"user_id\":1,\"amount\":\"2.00\"}\n",
            );
            let plan = LoadPlan {
                entries: vec![
                    plan_entry("users", &["id", "name", "note"], &users_path, 1),
                    plan_entry("orders", &["id", "user_id", "amount"], &orders_path, 2),
                ],
            };

            let mut conn = memory_conn().await;
            create_fk_schema(conn.as_mut()).await;

            let audit = AuditSession::disabled();
            let err = loader_execute(&mut *conn, &plan, &audit).await.unwrap_err();
            assert!(err.contains("table 'orders' failed"), "{err}");
            assert!(err.contains("completed: users"), "{err}");

            // users committed; the orders PK duplicate rolled the whole
            // orders transaction back (count still 0, users still 1).
            let users = conn.query("SELECT COUNT(*) FROM main.users").await.unwrap();
            assert_eq!(users.rows[0][0], json!(1));
            let orders = conn
                .query("SELECT COUNT(*) FROM main.orders")
                .await
                .unwrap();
            assert_eq!(orders.rows[0][0], json!(0));
        }

        #[tokio::test]
        async fn execute_errors_when_plan_file_is_missing() {
            let plan = LoadPlan {
                entries: vec![plan_entry(
                    "users",
                    &["id", "name", "note"],
                    &PathBuf::from("/nonexistent/load_test/users.jsonl"),
                    1,
                )],
            };
            let mut conn = memory_conn().await;
            create_fk_schema(conn.as_mut()).await;
            let audit = AuditSession::disabled();
            let err = loader_execute(&mut *conn, &plan, &audit).await.unwrap_err();
            assert!(err.contains("table 'users' failed"), "{err}");
            assert!(err.contains("completed: none"), "{err}");
        }

        /// Adapter so the gated tests call the executor the way load::run does.
        async fn loader_execute(
            conn: &mut dyn DbConn,
            plan: &LoadPlan,
            audit: &AuditSession,
        ) -> Result<u64, String> {
            crate::load::loader::execute(conn, plan, audit, &conn_info()).await
        }
    }

    // ─── Pure helper tests (no DB) ──────────────────────────────────────

    #[test]
    fn begin_sql_uses_start_transaction_for_mysql_and_begin_otherwise() {
        assert_eq!(begin_sql("mysql"), "START TRANSACTION");
        assert_eq!(begin_sql("duckdb"), "BEGIN");
        assert_eq!(begin_sql("gaussdb"), "BEGIN");
    }

    #[test]
    fn read_table_rows_dispatches_on_extension() {
        let dir = tempfile::tempdir().unwrap();
        let jsonl = dir.path().join("t.jsonl");
        std::fs::write(&jsonl, "{\"a\":1}\n").unwrap();
        let (columns, rows) = read_table_rows(&jsonl).unwrap();
        assert_eq!(columns, vec!["a".to_string()]);
        assert_eq!(rows.len(), 1);

        let json = dir.path().join("t2.json");
        std::fs::write(&json, "[{\"b\":\"x\"}]").unwrap();
        let (columns, rows) = read_table_rows(&json).unwrap();
        assert_eq!(columns, vec!["b".to_string()]);
        assert_eq!(rows[0][0], serde_json::json!("x"));

        let csv = dir.path().join("t3.csv");
        std::fs::write(&csv, "c\n7\n").unwrap();
        let (columns, rows) = read_table_rows(&csv).unwrap();
        assert_eq!(columns, vec!["c".to_string()]);
        assert_eq!(rows[0][0], serde_json::json!("7"));
    }

    #[test]
    fn read_table_rows_rejects_unknown_extension() {
        let err = read_table_rows(std::path::Path::new("/data/t.parquet")).unwrap_err();
        assert!(err.contains("parquet"), "{err}");
        assert!(err.contains("unsupported data file extension"), "{err}");
    }

    #[test]
    fn format_completed_renders_none_for_empty_list() {
        assert_eq!(format_completed(&[]), "none");
        assert_eq!(
            format_completed(&["a".to_string(), "b".to_string()]),
            "a, b"
        );
    }

    #[test]
    fn table_intent_event_is_dml_allow_with_table_and_planned_rows() {
        let entry = super::super::plan::PlanEntry {
            table: "users".to_string(),
            schema: Some("shop".to_string()),
            path: PathBuf::from("/data/users.jsonl"),
            columns: vec!["id".to_string()],
            row_count: 3,
        };
        let conn_info = ConnectionInfo::from_url("dev", "mysql://u:p@h:3306/db", false);
        let event = table_event(
            &conn_info,
            Decision::Allow,
            ActionClass::Dml,
            table_intent_detail("mysql", &entry, 3),
        );
        assert_eq!(event.channel, Channel::Load);
        assert_eq!(event.class, ActionClass::Dml);
        assert_eq!(event.decision, Decision::Allow);
        let detail = event.detail.expect("detail");
        assert_eq!(detail["phase"], "intent");
        assert_eq!(detail["table"], "users");
        assert_eq!(detail["rows_planned"], 3);
        assert_eq!(detail["subcommand"], "load");
    }

    #[test]
    fn table_outcome_event_carries_counts_without_row_data() {
        let detail = table_outcome_detail("users", 3, 12);
        assert_eq!(detail["phase"], "outcome");
        assert_eq!(detail["table"], "users");
        assert_eq!(detail["rows_loaded"], 3);
        assert_eq!(detail["duration_ms"], 12);
        // Counts only: the serialized detail must not grow row payloads.
        let text = detail.to_string();
        assert!(!text.contains("alice"), "{text}");
    }
}
