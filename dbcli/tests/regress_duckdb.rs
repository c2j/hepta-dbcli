// DuckDB regression test suite (embedded — no external service required).
// Run: cargo test --features "duckdb,integration" --test regress_duckdb

#[cfg(all(feature = "integration", feature = "duckdb"))]
mod common;

#[cfg(all(feature = "integration", feature = "duckdb"))]
mod tests {
    use crate::common::assert_columns;
    use polar_mysql::backend::duckdb::DuckDbFactory;
    use polar_mysql::backend::{BackendFactory, DbPool};
    use serde_json::json;
    use std::sync::Arc;

    const TABLE: &str = "regress_duck";

    async fn file_pool(path: &std::path::Path, query: &str) -> Result<Arc<dyn DbPool>, String> {
        let url = format!("duckdb://{}{query}", path.to_string_lossy());
        DuckDbFactory
            .connect(&url, None)
            .await
            .map_err(|e| e.to_string())
    }

    #[tokio::test]
    async fn duckdb_file_roundtrip_and_metadata() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("regress.duckdb");

        // Pools never create files — bootstrap the database directly.
        {
            let boot = duckdb::Connection::open(&path).expect("bootstrap create");
            boot.execute_batch(&format!(
                "CREATE TABLE {TABLE} (id BIGINT PRIMARY KEY, name VARCHAR, amount DECIMAL(10,2), active BOOLEAN, created_at TIMESTAMP)"
            ))
            .expect("bootstrap table");
        }

        let pool = file_pool(&path, "")
            .await
            .unwrap_or_else(|e| panic!("connect failed: {e}"));

        // CLI mode is not read-only: data can be inserted.
        {
            let mut conn = pool.acquire().await.expect("acquire");
            conn.query_drop(&format!(
                "INSERT INTO {TABLE} VALUES (1, 'alice', 99.99, true, TIMESTAMP '2024-01-02 03:04:05'), \
                 (2, 'bob', 50.00, false, TIMESTAMP '2024-06-01 12:00:00')"
            ))
            .await
            .expect("insert");
        }

        {
            let mut conn = pool.acquire().await.expect("acquire 2");
            let r = conn
                .query(&format!(
                    "SELECT id, name, amount, active, created_at FROM {TABLE} ORDER BY id"
                ))
                .await
                .expect("select");
            assert_columns(
                &r.columns,
                &["id", "name", "amount", "active", "created_at"],
            );
            assert_eq!(r.row_count, 2);
            assert_eq!(r.rows[0][0], json!(1));
            assert_eq!(r.rows[0][1], json!("alice"));
            assert_eq!(r.rows[0][2], json!(99.99));
            assert_eq!(r.rows[0][3], json!(true));
            assert_eq!(r.rows[0][4], json!("2024-01-02 03:04:05"));

            let tables_sql = conn.dialect().list_tables().to_string();
            let tables = conn.query(&tables_sql).await.expect("list_tables");
            let name_pos = tables
                .columns
                .iter()
                .position(|c| c == "table_name")
                .expect("table_name column");
            assert!(
                tables.rows.iter().any(|row| row[name_pos] == json!(TABLE)),
                "{TABLE} must be listed: {:?}",
                tables.rows
            );

            let cols_sql = conn.dialect().table_columns().to_string();
            let cols = conn
                .exec(&cols_sql, &[json!("main"), json!(TABLE)])
                .await
                .expect("table_columns");
            assert_columns(
                &cols.columns,
                &[
                    "column_name",
                    "data_type",
                    "nullable",
                    "default_value",
                    "ordinal_position",
                    "comment",
                    "column_key",
                ],
            );
            assert_eq!(cols.row_count, 5, "5 columns expected");

            let idx_sql = conn.dialect().table_indexes().to_string();
            let idx = conn
                .exec(&idx_sql, &[json!("main"), json!(TABLE)])
                .await
                .expect("table_indexes");
            assert_columns(
                &idx.columns,
                &[
                    "index_name",
                    "is_unique",
                    "is_primary",
                    "columns",
                    "index_type",
                ],
            );
        }
    }

    #[tokio::test]
    async fn duckdb_read_only_mode_enforced() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ro.duckdb");

        // Pools never create files — bootstrap the database directly.
        {
            let boot = duckdb::Connection::open(&path).expect("bootstrap create");
            boot.execute_batch(&format!("CREATE TABLE {TABLE} (id INTEGER)"))
                .expect("bootstrap table");
        }

        let ro = file_pool(&path, "?mode=ro").await.expect("ro pool");
        let mut conn = ro.acquire().await.expect("ro acquire");
        assert!(
            conn.query(&format!("SELECT COUNT(*) FROM {TABLE}"))
                .await
                .is_ok(),
            "reads must work in ro mode"
        );
        let write = conn
            .query_drop(&format!("INSERT INTO {TABLE} VALUES (1)"))
            .await;
        assert!(write.is_err(), "writes must fail in ro mode");
    }

    #[tokio::test]
    async fn duckdb_missing_file_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nope.duckdb");
        let result = file_pool(&path, "").await;
        match result {
            Ok(_) => panic!("missing file must fail to connect"),
            Err(e) => assert!(e.contains("not found"), "{e}"),
        }
        assert!(!path.exists(), "must not create the file");
    }

    #[tokio::test]
    async fn duckdb_memory_database() {
        let pool = DuckDbFactory
            .connect("duckdb://:memory:", None)
            .await
            .expect("memory pool");
        let mut conn = pool.acquire().await.expect("acquire");
        let r = conn
            .query("SELECT 1 + 1 AS two, 'duck' AS who")
            .await
            .expect("query");
        assert_eq!(r.rows[0][0], json!(2));
        assert_eq!(r.rows[0][1], json!("duck"));
    }

    #[tokio::test]
    async fn duckdb_execute_write_reports_rows_affected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("write.duckdb");
        {
            let boot = duckdb::Connection::open(&path).expect("bootstrap create");
            boot.execute_batch(&format!("CREATE TABLE {TABLE} (id BIGINT)"))
                .expect("bootstrap table");
        }

        let pool = file_pool(&path, "")
            .await
            .unwrap_or_else(|e| panic!("connect failed: {e}"));
        let mut conn = pool.acquire().await.expect("acquire");

        let inserted = conn
            .execute_write(&format!("INSERT INTO {TABLE} VALUES (1), (2), (3)"))
            .await
            .expect("insert");
        assert_eq!(
            inserted.rows_affected,
            Some(3),
            "a data change must report the affected rows (issue #58 D6)"
        );
        assert!(
            inserted.columns.is_empty() && inserted.rows.is_empty(),
            "a data change returns no result set"
        );

        let deleted = conn
            .execute_write(&format!("DELETE FROM {TABLE} WHERE id <= 2"))
            .await
            .expect("delete");
        assert_eq!(deleted.rows_affected, Some(2));
    }

    #[tokio::test]
    async fn duckdb_execute_write_is_refused_in_read_only_mode() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ro_write.duckdb");
        {
            let boot = duckdb::Connection::open(&path).expect("bootstrap create");
            boot.execute_batch(&format!("CREATE TABLE {TABLE} (id BIGINT)"))
                .expect("bootstrap table");
        }

        let ro = file_pool(&path, "?mode=ro").await.expect("ro pool");
        let mut conn = ro.acquire().await.expect("ro acquire");
        let write = conn
            .execute_write(&format!("INSERT INTO {TABLE} VALUES (1)"))
            .await;
        assert!(
            write.is_err(),
            "the driver-level ?mode=ro guard must also cover execute_write"
        );
        assert_eq!(
            conn.query(&format!("SELECT COUNT(*) FROM {TABLE}"))
                .await
                .expect("read still works")
                .rows[0][0],
            json!(0)
        );
    }
}
